//! The long-running service: reconcile, then keep one recorder per enabled stream in step with the
//! database, and run retention in the background.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::db::{self, Stream};
use crate::{Ctx, reconcile, retention, supervisor};

struct Recorder {
    revision: i64,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

pub async fn serve(ctx: Arc<Ctx>, shutdown: CancellationToken) -> Result<()> {
    reconcile::run(&ctx).await?;
    tokio::spawn(log_tool_versions(ctx.clone()));

    let retention = tokio::spawn(retention_loop(ctx.clone(), shutdown.clone()));
    let mut recorders: HashMap<i64, Recorder> = HashMap::new();
    loop {
        match db::list_streams(&ctx.pool).await {
            Ok(streams) => sync(&ctx, &mut recorders, &streams),
            Err(e) => error!("reading streams: {e:#}"),
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
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
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(supervisor::run(ctx.clone(), stream.clone(), cancel.clone()));
        recorders.insert(
            stream.id,
            Recorder {
                revision: stream.revision,
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

/// A stale yt-dlp is the usual cause of 403s on YouTube, so make its version visible. Runs in the
/// background with a timeout: a hung tool must not hold up recording.
async fn log_tool_versions(ctx: Arc<Ctx>) {
    let tools = [(&ctx.tools.yt_dlp, "--version"), (&ctx.tools.ffmpeg, "-version")];
    for (tool, flag) in tools {
        let probe = tokio::process::Command::new(tool).arg(flag).kill_on_drop(true).output();
        match tokio::time::timeout(Duration::from_secs(30), probe).await {
            Ok(Ok(out)) => {
                let text = String::from_utf8_lossy(&out.stdout);
                info!("{}: {}", tool.display(), text.lines().next().unwrap_or("").trim());
            }
            Ok(Err(e)) => warn!("{} is not runnable: {e}", tool.display()),
            Err(_) => warn!("{} {flag} did not answer within 30 s", tool.display()),
        }
    }
}
