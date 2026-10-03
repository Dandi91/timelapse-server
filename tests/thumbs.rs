//! Keyframe thumbnail sprites: made, kept, cleaned up with their segments.

mod common;

use std::path::Path;
use std::time::Duration;

use common::*;
use timelapse_server::db::{self, NewSegment};
use timelapse_server::{reconcile, retention, server, supervisor, thumbs};
use tokio_util::sync::CancellationToken;

async fn dimensions(path: &Path) -> (u32, u32) {
    let out = tokio::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=width,height", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let (w, h) = text.trim().split_once(',').unwrap();
    (w.parse().unwrap(), h.parse().unwrap())
}

#[tokio::test]
async fn sprites_have_a_tile_per_keyframe_and_follow_their_segments() {
    // 54 s of stream in 18 s segments: 3 segments of 3 s of video, a keyframe each second.
    let f = fixture(&pattern_source(54, false)).await;
    let mut stream = add_stream(&f.ctx, "cam").await;
    stream.settings.0.segment_minutes = 0.3;
    let report = supervisor::attempt(&f.ctx, &stream, &CancellationToken::new())
        .await
        .unwrap();
    assert!(report.segments >= 3, "{report:?}");

    // A segment ffmpeg can't read is marked and not tried again.
    let junk_path = f.ctx.session_dir(stream.id, report.session_id).join("000099.ts");
    std::fs::write(&junk_path, b"not video").unwrap();
    db::insert_segment(
        &f.ctx.pool,
        &NewSegment {
            session_id: report.session_id,
            stream_id: stream.id,
            seq: 99,
            path: f.ctx.relative(&junk_path),
            wall_start: 0,
            wall_end: 1,
            media_start: None,
            media_end: None,
            media_dur: 3.0,
            bytes: 9,
        },
    )
    .await
    .unwrap();

    let made = thumbs::run_pending(&f.ctx, &CancellationToken::new()).await.unwrap();
    assert_eq!(made, report.segments + 1);
    assert_eq!(
        thumbs::run_pending(&f.ctx, &CancellationToken::new()).await.unwrap(),
        0,
        "nothing left to do"
    );

    let segments = db::list_segments(&f.ctx.pool, stream.id).await.unwrap();
    for seg in segments.iter().filter(|s| s.seq != 99) {
        let tiles = thumbs::tile_count(seg.media_dur, 1.0);
        assert_eq!(
            (seg.thumbs, seg.thumb_interval),
            (Some(tiles as i64), Some(1.0)),
            "{}",
            seg.path
        );
        let sprite = thumbs::sprite_path(&f.ctx.absolute(&seg.path));
        assert_eq!(dimensions(&sprite).await, (tiles * thumbs::WIDTH, 90));
    }
    // Full segments hold 3 s of video: a keyframe, so a tile, per second.
    assert_eq!(segments[1].thumbs, Some(3));
    let junk = segments.iter().find(|s| s.seq == 99).unwrap();
    assert_eq!(junk.thumbs, Some(0));
    assert!(!thumbs::sprite_path(&junk_path).exists());
    assert!(
        !junk_path.with_extension("jpg.part").exists(),
        "no half-written sprite left"
    );
    db::delete_segment_rows(&f.ctx.pool, &[junk.id]).await.unwrap();
    std::fs::remove_file(&junk_path).unwrap();

    // Reconcile keeps sprites of existing segments and removes the rest.
    let dir = f.ctx.session_dir(stream.id, report.session_id);
    std::fs::write(dir.join("000050.jpg"), b"orphan").unwrap();
    std::fs::write(dir.join("000000.jpg.part"), b"half").unwrap();
    reconcile::run(&f.ctx).await.unwrap();
    assert!(!dir.join("000050.jpg").exists() && !dir.join("000000.jpg.part").exists());
    assert!(dir.join("000000.jpg").exists() && dir.join("000002.jpg").exists());

    // Retention takes a segment's sprite with it.
    let mut config = stream.config();
    config.max_bytes = Some(1);
    db::update_stream(&f.ctx.pool, &stream, &config).await.unwrap();
    retention::run_once(&f.ctx).await.unwrap();
    assert!(db::list_segments(&f.ctx.pool, stream.id).await.unwrap().is_empty());
    let left: Vec<_> = std::fs::read_dir(&dir)
        .map(|d| d.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();
    assert!(
        left.is_empty(),
        "files left: {:?}",
        left.iter().map(|e| e.file_name()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn the_server_makes_sprites_as_segments_finish() {
    let f = fixture(&pattern_source(600, true)).await;
    let stream = add_stream(&f.ctx, "cam").await;
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(server::serve(f.ctx.clone(), shutdown.clone(), None));

    let pool = f.ctx.pool.clone();
    let made = wait_for(Duration::from_secs(30), || async {
        db::list_segments(&pool, stream.id)
            .await
            .unwrap()
            .iter()
            .any(|s| s.thumbs.is_some_and(|n| n > 0))
    })
    .await;
    assert!(made, "no sprite within 30 s");

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
