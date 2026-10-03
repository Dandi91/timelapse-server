//! The external tools: their versions, and keeping yt-dlp current.
//!
//! YouTube changes often and a stale yt-dlp gets 403s, so yt-dlp updates itself (`yt-dlp -U`) on a
//! schedule as well as on request. Nothing needs restarting afterwards: a running pipeline keeps
//! the stream it has, and every new attempt starts the updated binary.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{Ctx, ToolVersions, db};

/// The first scheduled update waits this long after startup, so it doesn't compete with it.
const FIRST_UPDATE_DELAY: Duration = Duration::from_secs(60);

/// How an update went, for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateOutcome {
    pub at: i64,
    /// Started by the schedule rather than a person.
    pub automatic: bool,
    pub ok: bool,
    pub before: Option<String>,
    pub after: Option<String>,
    pub output: String,
}

/// One update at a time, whoever asks.
static UPDATING: Mutex<()> = Mutex::const_new(());

/// Run `yt-dlp -U`. Works when yt-dlp is the standalone build in a writable place, as in the
/// Docker image. `None` when an update is already running.
pub async fn update_yt_dlp(ctx: &Ctx, automatic: bool) -> Option<UpdateOutcome> {
    let _guard = UPDATING.try_lock().ok()?;
    let before = ctx.versions.read().unwrap_or_else(|e| e.into_inner()).yt_dlp.clone();
    let run = tokio::process::Command::new(&ctx.tools.yt_dlp)
        .args(["-U", "--no-colors"])
        .kill_on_drop(true)
        .output();
    let (ok, output) = match tokio::time::timeout(Duration::from_secs(180), run).await {
        Ok(Ok(out)) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            (out.status.success(), text.trim().to_string())
        }
        Ok(Err(e)) => (false, format!("could not run {}: {e}", ctx.tools.yt_dlp.display())),
        Err(_) => (false, "yt-dlp -U did not finish within 3 minutes".into()),
    };
    refresh_versions(ctx).await;
    let after = ctx.versions.read().unwrap_or_else(|e| e.into_inner()).yt_dlp.clone();
    match (&before, &after) {
        _ if !ok => warn!("updating yt-dlp failed: {output}"),
        (Some(b), Some(a)) if b != a => info!("updated yt-dlp from {b} to {a}"),
        _ => info!("yt-dlp is up to date ({})", after.as_deref().unwrap_or("?")),
    }
    let outcome = UpdateOutcome {
        at: db::now_ms(),
        automatic,
        ok,
        before,
        after,
        output,
    };
    *ctx.last_update.write().unwrap_or_else(|e| e.into_inner()) = Some(outcome.clone());
    Some(outcome)
}

/// Update yt-dlp on the schedule in `tuning.yt_dlp_update` until `shutdown`; off when that is None.
pub async fn auto_update(ctx: Arc<Ctx>, shutdown: CancellationToken) {
    let Some(every) = ctx.tuning.yt_dlp_update else {
        return;
    };
    let mut wait = every.min(FIRST_UPDATE_DELAY);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        if update_yt_dlp(&ctx, true).await.is_none() {
            info!("skipping the scheduled yt-dlp update: one is already running");
        }
        wait = every;
    }
}

/// A stale yt-dlp is the usual cause of 403s on YouTube, so make its version visible, in the log and
/// the UI. Bounded by a timeout: a hung tool must not hold anything up.
pub async fn refresh_versions(ctx: &Ctx) {
    let yt_dlp = version(&ctx.tools.yt_dlp, "--version").await;
    let ffmpeg = version(&ctx.tools.ffmpeg, "-version").await;
    *ctx.versions.write().unwrap_or_else(|e| e.into_inner()) = ToolVersions { yt_dlp, ffmpeg };
}

async fn version(tool: &Path, flag: &str) -> Option<String> {
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
