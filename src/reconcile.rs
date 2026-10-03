//! Startup pass that makes the database and the data directory agree again after a crash.
//! Must run before any recorder starts.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::{info, warn};

use crate::db::{self, NewSegment};
use crate::{Ctx, pipeline, procs, retention};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ReconcileReport {
    /// Rows whose file had vanished.
    pub missing_rows: usize,
    /// Files found on disk without a row (typically the in-flight segment of a crash) and indexed.
    pub adopted: usize,
    /// Files and directories that belonged to nothing and were removed.
    pub removed: usize,
}

pub async fn run(ctx: &Ctx) -> Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    db::clear_leases(&ctx.pool).await?;

    // A crashed server's pipelines may still be recording. Let them finish their segment first,
    // so it is complete when adopted below.
    let mut leftovers = tokio::task::JoinSet::new();
    for pipeline in db::open_pipelines(&ctx.pool).await? {
        let fetcher = pipeline.fetcher.as_deref().and_then(procs::Identity::parse);
        let encoder = pipeline.encoder.as_deref().and_then(procs::Identity::parse);
        leftovers.spawn(procs::wind_down(fetcher, encoder));
    }
    leftovers.join_all().await;
    db::end_open_sessions(&ctx.pool).await?;
    // Statuses are only ever set by running recorders, so a crash leaves them stale.
    db::reset_statuses(&ctx.pool).await?;

    // Finish deletes that were interrupted between marking and unlinking.
    let doomed: Vec<i64> = db::deleting_segments(&ctx.pool)
        .await?
        .into_iter()
        .map(|s| s.id)
        .collect();
    if !doomed.is_empty() {
        retention::delete_segments(ctx, &doomed).await?;
    }

    let segments = db::all_segments(&ctx.pool).await?;
    let gone: Vec<i64> = segments
        .iter()
        .filter(|s| !ctx.absolute(&s.path).exists())
        .map(|s| s.id)
        .collect();
    if !gone.is_empty() {
        db::delete_segment_rows(&ctx.pool, &gone).await?;
        report.missing_rows = gone.len();
    }
    let known: HashSet<String> = segments.into_iter().map(|s| s.path).collect();

    let streams: HashSet<i64> = db::list_streams(&ctx.pool).await?.into_iter().map(|s| s.id).collect();
    let sessions: HashMap<i64, db::Session> = db::list_sessions(&ctx.pool)
        .await?
        .into_iter()
        .map(|s| (s.id, s))
        .collect();

    for stream_dir in subdirs(&ctx.streams_dir())? {
        let Some(stream_id) = numeric_name(&stream_dir).filter(|id| streams.contains(id)) else {
            remove(&stream_dir, &mut report);
            continue;
        };
        for session_dir in subdirs(&stream_dir)? {
            let session = numeric_name(&session_dir)
                .and_then(|id| sessions.get(&id))
                .filter(|s| s.stream_id == stream_id);
            match session {
                Some(session) => adopt_untracked(ctx, session, &session_dir, &known, &mut report).await?,
                None => remove(&session_dir, &mut report),
            }
        }
    }

    for session in db::delete_empty_sessions(&ctx.pool).await? {
        let _ = std::fs::remove_dir_all(ctx.session_dir(session.stream_id, session.id));
    }
    if report != ReconcileReport::default() {
        info!(?report, "reconciled data directory with the database");
    }
    Ok(report)
}

/// Index segment files the database doesn't know. Unreadable ones are removed.
async fn adopt_untracked(
    ctx: &Ctx,
    session: &db::Session,
    dir: &Path,
    known: &HashSet<String>,
    report: &mut ReconcileReport,
) -> Result<()> {
    let (sprites, mut files): (Vec<_>, Vec<_>) = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && !known.contains(&ctx.relative(p)))
        .partition(|p| p.extension().is_some_and(|ext| ext == "jpg"));
    files.sort();
    // Like live indexing, never let an adopted segment overlap the one before it.
    let mut previous_end = db::session_last_wall_end(&ctx.pool, session.id).await?;
    for path in files {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let Some(seq) = pipeline::seq_from_file(&name) else {
            remove(&path, report);
            continue;
        };
        let duration = match probe_duration(&ctx.tools.ffprobe, &path).await {
            Ok(d) if d > 0.0 => d,
            Ok(_) | Err(_) => {
                remove(&path, report);
                continue;
            }
        };
        let meta = std::fs::metadata(&path)?;
        // The file stopped growing when the crash hit, so its mtime is the best end we have.
        let wall_end = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or_else(db::now_ms);
        let row = NewSegment {
            session_id: session.id,
            stream_id: session.stream_id,
            seq,
            path: ctx.relative(&path),
            wall_start: (wall_end - (duration * session.speedup * 1000.0) as i64)
                .max(previous_end.unwrap_or(i64::MIN))
                .min(wall_end),
            wall_end,
            media_start: None,
            media_end: None,
            media_dur: duration,
            bytes: meta.len() as i64,
        };
        db::insert_segment(&ctx.pool, &row).await?;
        previous_end = Some(wall_end);
        report.adopted += 1;
    }
    // Thumbnails stay with a segment that is still there (an adopted one gets them remade).
    for sprite in sprites {
        if !sprite.with_extension("ts").exists() {
            remove(&sprite, report);
        }
    }
    Ok(())
}

pub async fn probe_duration(ffprobe: &Path, file: &Path) -> Result<f64> {
    let output = Command::new(ffprobe)
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .await
        .with_context(|| format!("running {}", ffprobe.display()))?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .with_context(|| format!("no duration for {}", file.display()))
}

fn subdirs(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    match std::fs::read_dir(dir) {
        Ok(entries) => Ok(entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("listing {}", dir.display())),
    }
}

fn numeric_name(path: &Path) -> Option<i64> {
    path.file_name()?.to_str()?.parse().ok()
}

fn remove(path: &Path, report: &mut ReconcileReport) {
    let result = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Ok(()) => report.removed += 1,
        Err(e) => warn!("removing {}: {e}", path.display()),
    }
}
