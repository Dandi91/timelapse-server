//! HTTP: the JSON API, HLS playlists, segment files, and the embedded UI.

pub mod auth;
mod manage;
mod playlist;

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::sse::{self, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router, middleware};
use futures_util::{Stream, StreamExt};
use rust_embed::Embed;
use serde::Deserialize;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::Ctx;
use crate::db;
use crate::events::Event;

/// `shutdown` ends long-lived responses (the event stream), so a graceful shutdown doesn't wait
/// for browsers to disconnect.
pub fn router(ctx: Arc<Ctx>, shutdown: CancellationToken) -> Router {
    // Playlists and the segment files they list share one directory, so URIs inside a playlist
    // are relative. Anything that isn't a playlist is a file under the data dir's streams/.
    let streams = Router::new()
        .route("/{id}/playlist.m3u8", get(playlist))
        .fallback_service(ServeDir::new(ctx.streams_dir()));
    Router::new()
        .route("/api/streams", get(list_streams).post(manage::create))
        .route(
            "/api/streams/{id}",
            axum::routing::patch(manage::update).delete(manage::remove),
        )
        .route("/api/streams/{id}/segments", get(list_segments))
        .route("/api/streams/{id}/log", get(manage::log_tail))
        .route("/api/streams/{id}/restart", post(manage::restart))
        .route("/api/system", get(manage::system))
        .route("/api/system/update-yt-dlp", post(manage::update_yt_dlp))
        .route("/api/events", get(events))
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route("/api/auth", get(auth::status))
        .nest("/streams", streams)
        .fallback(ui)
        .layer(middleware::from_fn_with_state(ctx.clone(), auth::require))
        .layer(Extension(shutdown))
        .layer(TraceLayer::new_for_http())
        .with_state(ctx)
}

/// Server-sent events: one JSON object per event, `{"type": ...}`.
async fn events(
    State(ctx): State<Arc<Ctx>>,
    Extension(shutdown): Extension<CancellationToken>,
) -> Sse<impl Stream<Item = Result<sse::Event, Infallible>>> {
    let stream = BroadcastStream::new(ctx.events.subscribe())
        .map(|item| {
            // A lagging client missed events; it resynchronises from the API.
            let event = item.unwrap_or(Event::Resync);
            Ok(sse::Event::default().json_data(&event).unwrap_or_default())
        })
        .take_until(shutdown.cancelled_owned());
    Sse::new(stream).keep_alive(KeepAlive::default())
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
    Ok(Json(db::stream_summaries(&ctx.pool).await?).into_response())
}

async fn list_segments(
    State(ctx): State<Arc<Ctx>>,
    Path(id): Path<i64>,
    Query(range): Query<Range>,
) -> Result<Response, AppError> {
    db::get_stream(&ctx.pool, id).await?.ok_or(AppError::NotFound)?;
    let segments = db::segments_in_range(&ctx.pool, id, range.from, range.to).await?;
    Ok(Json(segments).into_response())
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

#[derive(Debug)]
enum AppError {
    NotFound,
    Unauthorized,
    /// Invalid input; the message is for the user.
    BadRequest(String),
    Conflict(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "log in first".to_string()),
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::Conflict(message) => (StatusCode::CONFLICT, message),
            Self::Internal(e) => {
                tracing::error!("request failed: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}
