//! Deleting the oldest segments once a stream exceeds its size or duration limit, or the disk
//! runs low.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use tracing::{info, warn};

use crate::Ctx;
use crate::db::{self, PruneCandidate};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PruneReport {
    pub segments: usize,
    pub bytes: i64,
}

/// Given one stream's segments newest first, pick the ones beyond its limits. A segment goes as
/// soon as it would push the running total past either limit. Leased segments are skipped but
/// still count toward the totals.
pub fn select_prunable(
    newest_first: &[PruneCandidate],
    max_bytes: Option<i64>,
    max_duration_ms: Option<i64>,
) -> Vec<i64> {
    let (mut bytes, mut duration) = (0i64, 0i64);
    newest_first
        .iter()
        .filter(|seg| {
            bytes += seg.bytes;
            duration += seg.wall_end - seg.wall_start;
            let over = max_bytes.is_some_and(|max| bytes > max) || max_duration_ms.is_some_and(|max| duration > max);
            over && !seg.leased
        })
        .map(|seg| seg.id)
        .collect()
}

/// One pass over every stream, then the free-space guard.
pub async fn run_once(ctx: &Ctx) -> Result<PruneReport> {
    let mut report = PruneReport::default();
    for stream in db::list_streams(&ctx.pool).await? {
        if stream.max_bytes.is_none() && stream.max_duration_secs.is_none() {
            continue;
        }
        let candidates = db::prune_candidates(&ctx.pool, stream.id).await?;
        let doomed = select_prunable(
            &candidates,
            stream.max_bytes,
            stream.max_duration_secs.map(|s| s * 1000),
        );
        if !doomed.is_empty() {
            let freed: i64 = candidates
                .iter()
                .filter(|c| doomed.contains(&c.id))
                .map(|c| c.bytes)
                .sum();
            info!(
                stream = stream.label,
                "pruning {} segment(s) over the stream's limit",
                doomed.len()
            );
            delete_segments(ctx, &doomed).await?;
            report.segments += doomed.len();
            report.bytes += freed;
        }
    }

    let free = free_bytes(&ctx.data_dir)?;
    if free < ctx.tuning.min_free_bytes {
        let deficit = (ctx.tuning.min_free_bytes - free) as i64;
        let (mut doomed, mut freed) = (Vec::new(), 0i64);
        for seg in db::oldest_unleased(&ctx.pool, 1000).await? {
            if freed >= deficit {
                break;
            }
            freed += seg.bytes;
            doomed.push(seg.id);
        }
        if doomed.is_empty() {
            warn!("only {free} bytes free and nothing left to prune");
        } else {
            warn!(
                "only {free} bytes free, pruning the {} oldest segment(s) across all streams",
                doomed.len()
            );
            delete_segments(ctx, &doomed).await?;
            report.segments += doomed.len();
            report.bytes += freed;
        }
    }

    for session in db::delete_empty_sessions(&ctx.pool).await? {
        let _ = std::fs::remove_dir(ctx.session_dir(session.stream_id, session.id));
    }
    Ok(report)
}

/// Mark, unlink, then drop the rows. A crash at any point leaves state reconcile can finish.
pub async fn delete_segments(ctx: &Ctx, ids: &[i64]) -> Result<()> {
    let doomed = db::mark_deleting(&ctx.pool, ids).await?;
    let mut dirs = HashSet::new();
    for seg in &doomed {
        let path = ctx.absolute(&seg.path);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("deleting {}: {e}", path.display()),
        }
        // Never remove a live session's directory: ffmpeg is still writing into it.
        if seg.session_ended
            && let Some(dir) = path.parent()
        {
            dirs.insert(dir.to_path_buf());
        }
    }
    db::delete_segment_rows(&ctx.pool, &doomed.iter().map(|s| s.id).collect::<Vec<_>>()).await?;
    for dir in dirs {
        // Fails harmlessly while the directory still holds segments.
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

pub fn free_bytes(path: &Path) -> Result<u64> {
    let stat = nix::sys::statvfs::statvfs(path)?;
    Ok(stat.blocks_available() as u64 * stat.fragment_size() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(id: i64, bytes: i64, minutes: i64, leased: bool) -> PruneCandidate {
        PruneCandidate {
            id,
            stream_id: 1,
            bytes,
            wall_start: 0,
            wall_end: minutes * 60_000,
            leased,
        }
    }

    #[test]
    fn prunes_by_bytes_keeping_total_under_limit() {
        // Newest first: 4, 3, 2, 1.
        let segs = [
            seg(4, 100, 10, false),
            seg(3, 100, 10, false),
            seg(2, 100, 10, false),
            seg(1, 100, 10, false),
        ];
        assert_eq!(select_prunable(&segs, Some(250), None), vec![2, 1]);
        assert_eq!(select_prunable(&segs, Some(400), None), Vec::<i64>::new());
        assert_eq!(select_prunable(&segs, None, None), Vec::<i64>::new());
    }

    #[test]
    fn prunes_by_duration() {
        let segs = [seg(3, 1, 10, false), seg(2, 1, 10, false), seg(1, 1, 10, false)];
        assert_eq!(select_prunable(&segs, None, Some(20 * 60_000)), vec![1]);
        // Either limit is enough.
        assert_eq!(select_prunable(&segs, Some(1), Some(60 * 60_000)), vec![2, 1]);
    }

    #[test]
    fn leased_segments_survive_but_still_count() {
        let segs = [seg(3, 100, 10, false), seg(2, 100, 10, true), seg(1, 100, 10, false)];
        assert_eq!(select_prunable(&segs, Some(150), None), vec![1]);
    }
}
