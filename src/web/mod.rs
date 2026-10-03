//! HTTP: the JSON API, HLS playlists, segment files, and the embedded UI.

mod playlist;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rust_embed::Embed;
use serde::Deserialize;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::Ctx;
use crate::db;

pub fn router(ctx: Arc<Ctx>) -> Router {
    // Playlists and the segment files they list share one directory, so URIs inside a playlist
    // are relative. Anything that isn't a playlist is a file under the data dir's streams/.
    let streams = Router::new()
        .route("/{id}/playlist.m3u8", get(playlist))
        .fallback_service(ServeDir::new(ctx.streams_dir()));
    Router::new()
        .route("/api/streams", get(list_streams))
        .route("/api/streams/{id}/segments", get(list_segments))
        .nest("/streams", streams)
        .fallback(ui)
        .layer(TraceLayer::new_for_http())
        .with_state(ctx)
}

/// A wall-clock range in unix milliseconds; either end may be open.
#[derive(Debug, Deserialize)]
struct Range {
    from: Option<i64>,
    to: Option<i64>,
    /// Serve a growing EVENT playlist instead of a fixed VOD one (`live=1` or `live=true`).
    live: Option<String>,
}

impl Range {
    fn live(&self) -> bool {
        self.live.as_deref().is_some_and(|v| !matches!(v, "0" | "false" | ""))
    }
}

async fn list_streams(State(ctx): State<Arc<Ctx>>) -> Result<Response, AppError> {
    Ok(axum::Json(db::stream_summaries(&ctx.pool).await?).into_response())
}

async fn list_segments(
    State(ctx): State<Arc<Ctx>>,
    Path(id): Path<i64>,
    Query(range): Query<Range>,
) -> Result<Response, AppError> {
    db::get_stream(&ctx.pool, id).await?.ok_or(AppError::NotFound)?;
    let segments = db::segments_in_range(&ctx.pool, id, range.from, range.to).await?;
    Ok(axum::Json(segments).into_response())
}

async fn playlist(
    State(ctx): State<Arc<Ctx>>,
    Path(id): Path<i64>,
    Query(range): Query<Range>,
) -> Result<Response, AppError> {
    let stream = db::get_stream(&ctx.pool, id).await?.ok_or(AppError::NotFound)?;
    let segments = db::segments_in_range(&ctx.pool, id, range.from, range.to).await?;
    let (kind, min_target) = if range.live() {
        (playlist::Kind::Event, stream.settings.0.segment_seconds() as f64)
    } else {
        (playlist::Kind::Vod, 0.0)
    };
    if segments.is_empty() && kind == playlist::Kind::Vod {
        return Err(AppError::NotFound);
    }
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        playlist::build(&segments, kind, min_target),
    )
        .into_response())
}

#[derive(Embed)]
#[folder = "ui/"]
struct Ui;

async fn ui(uri: Uri) -> Response {
    let path = match uri.path().trim_start_matches('/') {
        "" => "index.html",
        path => path,
    };
    match Ui::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref())], file.data).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

enum AppError {
    NotFound,
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Internal(e) => {
                tracing::error!("request failed: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        }
    }
}
