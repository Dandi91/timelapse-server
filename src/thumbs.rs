//! Keyframe thumbnails: one sprite per segment, a tile per keyframe, for previews on the timeline.
//!
//! Only keyframes are decoded, so a 100 s 1080p segment takes about a second and makes an 80 KB
//! image. One worker makes them, newest segment first, so a backlog after an upgrade fills in
//! from the present backwards without competing much with recording.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{Ctx, db};

/// Tile size; 16:9 at the height the timeline shows them.
pub const WIDTH: u32 = 160;

/// `000003.ts` -> `000003.jpg`.
pub fn sprite_path(segment: &Path) -> PathBuf {
    segment.with_extension("jpg")
}

/// Tiles for a segment of `media_dur` seconds with a keyframe every `interval` seconds. Segments
/// start on a keyframe, so the first tile is at 0.
pub fn tile_count(media_dur: f64, interval: f64) -> u32 {
    ((media_dur / interval) - 1e-6).ceil().max(1.0) as u32
}

pub fn ffmpeg_args(segment: &Path, tiles: u32, output: &Path) -> Vec<String> {
    let mut args: Vec<String> = [
        "-hide_banner",
        "-nostats",
        "-loglevel",
        "error",
        "-y",
        "-skip_frame",
        "nokey",
        "-i",
    ]
    .map(String::from)
    .into();
    args.push(segment.to_string_lossy().into_owned());
    // `tile` still emits an incomplete row at the end of input, so a short count is harmless.
    args.extend([
        "-vf".into(),
        format!("scale={WIDTH}:-2,tile={tiles}x1"),
        "-frames:v".into(),
        "1".into(),
        "-fps_mode".into(),
        "passthrough".into(),
        "-q:v".into(),
        "5".into(),
        "-f".into(),
        "mjpeg".into(),
    ]);
    args.push(output.to_string_lossy().into_owned());
    args
}

/// Make the sprite for one segment. Written under a temporary name and renamed into place, so a
/// half-written image is never served.
pub async fn make(ctx: &Ctx, segment: &Path, tiles: u32) -> Result<()> {
    let sprite = sprite_path(segment);
    let partial = segment.with_extension("jpg.part");
    let run = Command::new(&ctx.tools.ffmpeg)
        .args(ffmpeg_args(segment, tiles, &partial))
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(120), run).await??;
    if !output.status.success() {
        let _ = std::fs::remove_file(&partial);
        bail!("ffmpeg failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    std::fs::rename(&partial, &sprite)?;
    Ok(())
}

/// Make sprites for every segment without one, newest first. Returns how many were attempted.
pub async fn run_pending(ctx: &Ctx, stop: &CancellationToken) -> Result<usize> {
    let mut done = 0;
    while !stop.is_cancelled() {
        let Some(next) = db::next_unthumbed(&ctx.pool).await? else {
            break;
        };
        // While leased, retention leaves the segment alone, so its sprite can't be orphaned.
        db::lease_segment(&ctx.pool, next.id, "thumbnails").await?;
        let interval = next.settings.0.keyframe_seconds as f64;
        let tiles = tile_count(next.media_dur, interval);
        let made = make(ctx, &ctx.absolute(&next.path), tiles).await;
        db::release_lease(&ctx.pool, next.id, "thumbnails").await?;
        match made {
            Ok(()) => db::set_thumbs(&ctx.pool, next.id, tiles as i64, interval).await?,
            Err(e) => {
                // Marked, so a segment ffmpeg can't read isn't retried forever.
                warn!("thumbnails for {}: {e:#}", next.path);
                db::set_thumbs(&ctx.pool, next.id, 0, interval).await?;
            }
        }
        done += 1;
    }
    Ok(done)
}

/// Background worker: woken whenever a segment is finished.
pub async fn worker(ctx: std::sync::Arc<Ctx>, shutdown: CancellationToken) {
    loop {
        match run_pending(&ctx, &shutdown).await {
            Ok(n) if n > 1 => info!("made thumbnails for {n} segments"),
            Ok(_) => {}
            Err(e) => warn!("thumbnail worker: {e:#}"),
        }
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ctx.thumbs_wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(300)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_tiles() {
        assert_eq!(tile_count(100.0, 5.0), 20);
        assert_eq!(tile_count(100.07, 5.0), 21);
        assert_eq!(tile_count(4.2, 5.0), 1);
        assert_eq!(tile_count(0.0, 5.0), 1);
    }

    #[test]
    fn builds_args() {
        let args = ffmpeg_args(Path::new("/d/1/2/000003.ts"), 20, Path::new("/d/1/2/000003.jpg.part")).join(" ");
        assert!(args.contains("-skip_frame nokey -i /d/1/2/000003.ts"));
        assert!(args.contains("-vf scale=160:-2,tile=20x1 -frames:v 1"));
        assert!(args.ends_with("-f mjpeg /d/1/2/000003.jpg.part"));
        assert_eq!(sprite_path(Path::new("/d/000003.ts")), Path::new("/d/000003.jpg"));
    }
}
