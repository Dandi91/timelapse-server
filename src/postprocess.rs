//! Work on each finished segment, done by one background worker: the keyframe index for
//! byte-range playlists, then the thumbnail sprite. Newest segments go first, so a backlog after
//! an upgrade fills in from the present backwards without competing much with recording.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{Ctx, db, parts, thumbs};

const LEASE: &str = "postprocess";

/// Process every segment that still lacks its index or thumbnails. Returns how many were handled.
pub async fn run_pending(ctx: &Ctx, stop: &CancellationToken) -> Result<usize> {
    let mut done = 0;
    while !stop.is_cancelled() {
        let Some(next) = db::next_unprocessed(&ctx.pool).await? else {
            break;
        };
        // While leased, retention leaves the segment alone, so nothing made here is orphaned.
        db::lease_segment(&ctx.pool, next.id, LEASE).await?;
        let result = process(ctx, &next).await;
        db::release_lease(&ctx.pool, next.id, LEASE).await?;
        result?;
        done += 1;
    }
    Ok(done)
}

async fn process(ctx: &Ctx, segment: &db::Unprocessed) -> Result<()> {
    let path = ctx.absolute(&segment.path);
    if segment.needs_parts {
        // Marked empty on failure, so the segment is served whole rather than retried forever.
        let found = parts::index(&ctx.tools.ffprobe, &path).await.unwrap_or_else(|e| {
            warn!("indexing {}: {e:#}", segment.path);
            Vec::new()
        });
        db::set_parts(&ctx.pool, segment.id, &found).await?;
    }
    if segment.needs_thumbs {
        let interval = segment.settings.0.keyframe_seconds as f64;
        let tiles = thumbs::tile_count(segment.media_dur, interval);
        match thumbs::make(ctx, &path, tiles).await {
            Ok(()) => db::set_thumbs(&ctx.pool, segment.id, tiles as i64, interval).await?,
            Err(e) => {
                warn!("thumbnails for {}: {e:#}", segment.path);
                db::set_thumbs(&ctx.pool, segment.id, 0, interval).await?;
            }
        }
    }
    Ok(())
}

/// Background worker: woken whenever a segment is finished.
pub async fn worker(ctx: Arc<Ctx>, shutdown: CancellationToken) {
    loop {
        match run_pending(&ctx, &shutdown).await {
            Ok(n) if n > 1 => info!("post-processed {n} segments"),
            Ok(_) => {}
            Err(e) => warn!("post-processing: {e:#}"),
        }
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ctx.segment_finished.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(300)) => {}
        }
    }
}
