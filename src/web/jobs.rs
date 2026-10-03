//! Export jobs over HTTP: queue, list, download, cancel or delete.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tower_http::services::ServeFile;

use super::AppError;
use crate::events::Event;
use crate::exports::{self, Mode};
use crate::{Ctx, db};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NewExport {
    stream_id: i64,
    /// Wall-clock range in unix milliseconds.
    from: i64,
    to: i64,
    /// "fast" (default) or "exact".
    mode: Option<String>,
}

pub(super) async fn list(State(ctx): State<Arc<Ctx>>) -> Result<Response, AppError> {
    Ok(Json(db::list_exports(&ctx.pool).await?).into_response())
}

pub(super) async fn create(State(ctx): State<Arc<Ctx>>, Json(new): Json<NewExport>) -> Result<Response, AppError> {
    let mode = Mode::parse(new.mode.as_deref().unwrap_or("fast"))
        .ok_or_else(|| AppError::BadRequest("mode must be \"fast\" or \"exact\"".into()))?;
    if new.to <= new.from {
        return Err(AppError::BadRequest("the end must come after the start".into()));
    }
    let stream = db::get_stream(&ctx.pool, new.stream_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if db::count_segments_in_range(&ctx.pool, stream.id, new.from, new.to).await? == 0 {
        return Err(AppError::BadRequest("no footage between those times".into()));
    }
    let id = db::create_export(&ctx.pool, &stream, new.from, new.to, mode.as_str()).await?;
    ctx.exports.wake.notify_one();
    ctx.emit(Event::ExportsChanged);
    let job = db::get_export(&ctx.pool, id).await?.ok_or(AppError::NotFound)?;
    Ok((StatusCode::CREATED, Json(job)).into_response())
}

/// Cancels the job if it is queued or running, and removes it and its file.
pub(super) async fn remove(State(ctx): State<Arc<Ctx>>, Path(id): Path<i64>) -> Result<Response, AppError> {
    let job = db::get_export(&ctx.pool, id).await?.ok_or(AppError::NotFound)?;
    db::delete_export(&ctx.pool, id).await?;
    ctx.exports.cancel(id);
    if let Some(path) = job.path {
        let _ = std::fs::remove_file(ctx.absolute(&path));
    }
    ctx.emit(Event::ExportsChanged);
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The finished clip, as an attachment with a readable name. Supports range requests.
pub(super) async fn download(
    State(ctx): State<Arc<Ctx>>,
    Path(id): Path<i64>,
    request: Request,
) -> Result<Response, AppError> {
    let job = db::get_export(&ctx.pool, id).await?.ok_or(AppError::NotFound)?;
    let path = job.path.filter(|_| job.state == "done").ok_or(AppError::NotFound)?;
    let mut response = ServeFile::new(ctx.absolute(&path))
        .try_call(request)
        .await
        .map_err(anyhow::Error::from)?
        .map(Body::new);
    let name = exports::file_name(&job.stream_label, job.actual_from_ms.unwrap_or(job.from_ms));
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        response.headers_mut().insert(header::CONTENT_DISPOSITION, value);
    }
    Ok(response)
}
