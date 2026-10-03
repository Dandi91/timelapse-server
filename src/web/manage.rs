//! Changing streams, reading their logs, and the system panel.
//!
//! Every change goes to the database first and then wakes the recorder manager, which brings the
//! recorders in line and announces the change; the CLI's changes take the same path.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use super::AppError;
use crate::db::{self, StreamConfig};
use crate::settings::EncodeSettings;
use crate::{Ctx, retention, tools};

/// Fields to set on a stream; absent ones stay as they are. `settings` may name only some
/// settings. A limit given as `null` is removed.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamPatch {
    label: Option<String>,
    url: Option<String>,
    enabled: Option<bool>,
    live_only: Option<bool>,
    settings: Option<Map<String, Value>>,
    #[serde(default, deserialize_with = "present")]
    max_bytes: Option<Option<i64>>,
    #[serde(default, deserialize_with = "present")]
    max_duration_secs: Option<Option<i64>>,
}

/// Tells "absent" (outer `None`) from "null" (`Some(None)`).
fn present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

impl StreamPatch {
    fn apply(self, config: &mut StreamConfig) -> Result<(), AppError> {
        if let Some(v) = self.label {
            config.label = v.trim().to_string();
        }
        if let Some(v) = self.url {
            config.url = v.trim().to_string();
        }
        if let Some(v) = self.enabled {
            config.enabled = v;
        }
        if let Some(v) = self.live_only {
            config.live_only = v;
        }
        if let Some(changes) = self.settings {
            let Value::Object(mut merged) = serde_json::to_value(&config.settings).map_err(anyhow::Error::from)? else {
                unreachable!("settings serialize to an object");
            };
            // Checked here rather than on EncodeSettings itself, which must keep reading rows
            // written before a setting was dropped.
            if let Some(unknown) = changes.keys().find(|key| !merged.contains_key(*key)) {
                return Err(AppError::BadRequest(format!("unknown setting {unknown:?}")));
            }
            merged.extend(changes);
            config.settings = serde_json::from_value::<EncodeSettings>(Value::Object(merged))
                .map_err(|e| AppError::BadRequest(format!("settings: {e}")))?;
        }
        if let Some(v) = self.max_bytes {
            config.max_bytes = v;
        }
        if let Some(v) = self.max_duration_secs {
            config.max_duration_secs = v;
        }
        config.validate().map_err(|e| AppError::BadRequest(format!("{e:#}")))
    }
}

/// Insert or update failed: a taken label is the user's to fix, anything else is ours.
fn write_error(e: anyhow::Error) -> AppError {
    let unique = e
        .downcast_ref::<sqlx::Error>()
        .and_then(|e| e.as_database_error())
        .is_some_and(|e| e.is_unique_violation());
    if unique {
        AppError::Conflict("another stream already has that label".into())
    } else {
        e.into()
    }
}

async fn summary(ctx: &Ctx, id: i64) -> Result<Response, AppError> {
    let summary = db::stream_summary(&ctx.pool, id).await?.ok_or(AppError::NotFound)?;
    Ok(Json(summary).into_response())
}

pub async fn create(State(ctx): State<Arc<Ctx>>, Json(patch): Json<StreamPatch>) -> Result<Response, AppError> {
    if patch.label.is_none() || patch.url.is_none() {
        return Err(AppError::BadRequest("a new stream needs a label and a url".into()));
    }
    let mut config = StreamConfig {
        label: String::new(),
        url: String::new(),
        enabled: true,
        live_only: true,
        settings: EncodeSettings::default(),
        max_bytes: None,
        max_duration_secs: None,
    };
    patch.apply(&mut config)?;
    let id = db::insert_stream(&ctx.pool, &config).await.map_err(write_error)?;
    ctx.wake.notify_one();
    Ok((StatusCode::CREATED, summary(&ctx, id).await?).into_response())
}

pub async fn update(
    State(ctx): State<Arc<Ctx>>,
    Path(id): Path<i64>,
    Json(patch): Json<StreamPatch>,
) -> Result<Response, AppError> {
    let stream = db::get_stream(&ctx.pool, id).await?.ok_or(AppError::NotFound)?;
    let mut config = stream.config();
    patch.apply(&mut config)?;
    db::update_stream(&ctx.pool, &stream, &config)
        .await
        .map_err(write_error)?;
    ctx.wake.notify_one();
    summary(&ctx, id).await
}

/// The recorder stops and the stream's files are deleted once it has.
pub async fn remove(State(ctx): State<Arc<Ctx>>, Path(id): Path<i64>) -> Result<Response, AppError> {
    db::get_stream(&ctx.pool, id).await?.ok_or(AppError::NotFound)?;
    db::delete_stream(&ctx.pool, id).await?;
    ctx.wake.notify_one();
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Start the stream's pipeline afresh, e.g. after updating yt-dlp.
pub async fn restart(State(ctx): State<Arc<Ctx>>, Path(id): Path<i64>) -> Result<Response, AppError> {
    if !db::bump_revision(&ctx.pool, id).await? {
        return Err(AppError::NotFound);
    }
    ctx.wake.notify_one();
    Ok(StatusCode::ACCEPTED.into_response())
}

#[derive(Deserialize)]
pub struct LogQuery {
    lines: Option<usize>,
}

/// The end of the stream's `capture.log`.
pub async fn log_tail(
    State(ctx): State<Arc<Ctx>>,
    Path(id): Path<i64>,
    Query(query): Query<LogQuery>,
) -> Result<Response, AppError> {
    db::get_stream(&ctx.pool, id).await?.ok_or(AppError::NotFound)?;
    let lines = query.lines.unwrap_or(200).clamp(1, 5000);
    let path = ctx.stream_dir(id).join("capture.log");
    let text = tokio::task::spawn_blocking(move || tail(&path, lines))
        .await
        .map_err(anyhow::Error::from)?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

/// Last `lines` lines of a file, reading at most the final 256 KB of it.
fn tail(path: &std::path::Path, lines: usize) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(256 << 10);
    let mut bytes = Vec::new();
    if file
        .seek(SeekFrom::Start(start))
        .and_then(|_| file.read_to_end(&mut bytes))
        .is_err()
    {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = text.lines().collect();
    // Starting mid-file, the first line is probably cut off.
    let usable = if start > 0 { &all[1.min(all.len())..] } else { &all[..] };
    usable[usable.len().saturating_sub(lines)..].join("\n")
}

#[derive(Serialize)]
pub struct SystemInfo {
    /// Bytes of recordings, across all streams.
    recordings_bytes: i64,
    /// Bytes of finished exports.
    exports_bytes: i64,
    disk_free_bytes: u64,
    disk_total_bytes: u64,
    /// The free-space guard: below this, the oldest segments go.
    min_free_bytes: u64,
    versions: crate::ToolVersions,
    yt_dlp_update: UpdateSchedule,
}

#[derive(Serialize)]
struct UpdateSchedule {
    /// None: automatic updates are off.
    every_hours: Option<f64>,
    last: Option<tools::UpdateOutcome>,
}

pub async fn system(State(ctx): State<Arc<Ctx>>) -> Result<Response, AppError> {
    let (free, total) = retention::disk_space(&ctx.data_dir)?;
    let versions = ctx.versions.read().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(Json(SystemInfo {
        recordings_bytes: db::total_segment_bytes(&ctx.pool).await?,
        exports_bytes: db::total_export_bytes(&ctx.pool).await?,
        disk_free_bytes: free,
        disk_total_bytes: total,
        min_free_bytes: ctx.tuning.min_free_bytes,
        versions,
        yt_dlp_update: UpdateSchedule {
            every_hours: ctx.tuning.yt_dlp_update.map(|d| d.as_secs_f64() / 3600.0),
            last: ctx.last_update.read().unwrap_or_else(|e| e.into_inner()).clone(),
        },
    })
    .into_response())
}

/// Run `yt-dlp -U` now. Running pipelines keep their stream; new attempts use the new version.
pub async fn update_yt_dlp(State(ctx): State<Arc<Ctx>>) -> Result<Response, AppError> {
    let outcome = tools::update_yt_dlp(&ctx, false)
        .await
        .ok_or_else(|| AppError::Conflict("an update is already running".into()))?;
    Ok(Json(outcome).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> StreamConfig {
        StreamConfig {
            label: "cam".into(),
            url: "https://example.com".into(),
            enabled: true,
            live_only: true,
            settings: EncodeSettings::default(),
            max_bytes: Some(100),
            max_duration_secs: Some(60),
        }
    }

    fn patch(json: &str) -> StreamPatch {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn merges_partial_settings_and_clears_limits() {
        let mut c = config();
        patch(r#"{"settings": {"crf": 25}, "max_bytes": null}"#)
            .apply(&mut c)
            .ok()
            .unwrap();
        assert_eq!(c.settings.crf, 25);
        assert_eq!(c.settings.sample_fps, 5.0, "untouched settings stay");
        assert_eq!(c.max_bytes, None);
        assert_eq!(c.max_duration_secs, Some(60), "absent limit stays");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(
            serde_json::from_str::<StreamPatch>(r#"{"lable": "x"}"#).is_err(),
            "typo in a field name"
        );
        for bad in [
            r#"{"settings": {"sample_fps": 7}}"#,
            r#"{"settings": {"preset": "fast; rm -rf /"}}"#,
            r#"{"settings": {"crfx": 3}}"#,
            r#"{"url": "file:///etc/passwd"}"#,
            r#"{"label": "  "}"#,
            r#"{"max_bytes": -1}"#,
        ] {
            assert!(
                matches!(patch(bad).apply(&mut config()), Err(AppError::BadRequest(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn tails_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        assert_eq!(tail(&path, 2), "b\nc");
        assert_eq!(tail(&path, 10), "a\nb\nc");
        assert_eq!(tail(&dir.path().join("missing"), 5), "");
    }
}
