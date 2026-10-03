//! The long-running service: reconcile, then keep one recorder per enabled stream in step with the
//! database, and run retention in the background.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::db::{self, Stream};
use crate::events::Event;
use crate::{Ctx, ToolVersions, exports, reconcile, retention, supervisor, thumbs, web};

struct Recorder {
    revision: i64,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

/// Runs until `shutdown`. With a `listener`, also serves the web UI and API on it.
pub async fn serve(ctx: Arc<Ctx>, shutdown: CancellationToken, listener: Option<TcpListener>) -> Result<()> {
    reconcile::run(&ctx).await?;
    exports::recover(&ctx).await?;
    {
        let ctx = ctx.clone();
        tokio::spawn(async move { refresh_tool_versions(&ctx).await });
    }

    let http = listener.map(|listener| {
        if let Ok(addr) = listener.local_addr() {
            info!("web UI on http://{addr}");
        }
        let app = web::router(ctx.clone(), shutdown.clone());
        let stop = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
            {
                error!("web server failed: {e}");
            }
        })
    });

    let retention = tokio::spawn(retention_loop(ctx.clone(), shutdown.clone()));
    let exporter = tokio::spawn(exports::worker(ctx.clone(), shutdown.clone()));
    let thumbnailer = tokio::spawn(thumbs::worker(ctx.clone(), shutdown.clone()));
    let mut recorders: HashMap<i64, Recorder> = HashMap::new();
    let mut seen = None;
    loop {
        match db::list_streams(&ctx.pool).await {
            Ok(streams) => {
                // Changes arrive from the web UI and from the CLI alike; announce both the same way.
                let configs = fingerprint(&streams);
                if seen.as_ref().is_some_and(|seen| *seen != configs) {
                    ctx.emit(Event::StreamsChanged);
                }
                seen = Some(configs);
                sync(&ctx, &mut recorders, &streams);
            }
            Err(e) => error!("reading streams: {e:#}"),
        }
        // The web UI wakes us right after a change; the poll catches CLI changes.
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ctx.wake.notified() => {}
            _ = sleep(ctx.tuning.poll_interval) => {}
        }
    }

    info!("shutting down {} recorder(s)", recorders.len());
    for recorder in recorders.values() {
        recorder.cancel.cancel();
    }
    for (_, recorder) in recorders {
        let _ = recorder.handle.await;
    }
    let _ = retention.await;
    let _ = exporter.await;
    let _ = thumbnailer.await;
    if let Some(http) = http {
        let _ = http.await;
    }
    Ok(())
}

/// Bring the running recorders in line with the stream table. A changed stream is stopped on one
/// tick and started again, with its new settings, on a later one once the old run has finished.
fn sync(ctx: &Arc<Ctx>, recorders: &mut HashMap<i64, Recorder>, streams: &[Stream]) {
    let by_id: HashMap<i64, &Stream> = streams.iter().map(|s| (s.id, s)).collect();

    recorders.retain(|id, recorder| {
        if !recorder.handle.is_finished() {
            return true;
        }
        if !by_id.contains_key(id) {
            let dir = ctx.stream_dir(*id);
            info!("stream {id} was removed, deleting {}", dir.display());
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                warn!("deleting {}: {e}", dir.display());
            }
        }
        false
    });

    for (id, recorder) in recorders.iter() {
        let current = by_id.get(id).filter(|s| s.enabled && s.revision == recorder.revision);
        if current.is_none() && !recorder.cancel.is_cancelled() {
            info!("stream {id} changed or was disabled, stopping its recorder");
            recorder.cancel.cancel();
        }
    }

    for stream in streams.iter().filter(|s| s.enabled) {
        if recorders.contains_key(&stream.id) {
            continue;
        }
        let (id, revision) = (stream.id, stream.revision);
        let cancel = CancellationToken::new();
        let (ctx, stream, token) = (ctx.clone(), stream.clone(), cancel.clone());
        let handle = tokio::spawn(async move {
            supervisor::run(ctx.clone(), stream, token).await;
            // A changed stream restarts only once its old recorder is done; don't wait for a poll.
            ctx.wake.notify_one();
        });
        recorders.insert(
            id,
            Recorder {
                revision,
                cancel,
                handle,
            },
        );
    }
}

async fn retention_loop(ctx: Arc<Ctx>, shutdown: CancellationToken) {
    loop {
        match retention::run_once(&ctx).await {
            Ok(report) if report.segments > 0 => {
                info!(
                    "retention removed {} segment(s), {} bytes",
                    report.segments, report.bytes
                )
            }
            Ok(_) => {}
            Err(e) => error!("retention pass failed: {e:#}"),
        }
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = sleep(ctx.tuning.retention_interval) => {}
        }
    }
}

/// The parts of the stream table the UI shows as configuration (statuses travel as their own
/// events).
fn fingerprint(streams: &[Stream]) -> Vec<(i64, i64, String)> {
    streams
        .iter()
        .map(|s| (s.id, s.revision, format!("{:?}", s.config())))
        .collect()
}

/// A stale yt-dlp is the usual cause of 403s on YouTube, so make its version visible, in the log and
/// the UI. Bounded by a timeout: a hung tool must not hold anything up.
pub async fn refresh_tool_versions(ctx: &Ctx) {
    let yt_dlp = tool_version(&ctx.tools.yt_dlp, "--version").await;
    let ffmpeg = tool_version(&ctx.tools.ffmpeg, "-version").await;
    *ctx.versions.write().unwrap_or_else(|e| e.into_inner()) = ToolVersions { yt_dlp, ffmpeg };
}

async fn tool_version(tool: &Path, flag: &str) -> Option<String> {
    let probe = tokio::process::Command::new(tool).arg(flag).kill_on_drop(true).output();
    match tokio::time::timeout(Duration::from_secs(30), probe).await {
        Ok(Ok(out)) => {
            let text = String::from_utf8_lossy(&out.stdout);
            let version = text.lines().next().unwrap_or("").trim().to_string();
            info!("{}: {version}", tool.display());
            Some(version)
        }
        Ok(Err(e)) => {
            warn!("{} is not runnable: {e}", tool.display());
            None
        }
        Err(_) => {
            warn!("{} {flag} did not answer within 30 s", tool.display());
            None
        }
    }
}
