//! Retention and startup reconcile, against segment rows and files made by hand.

mod common;

use std::path::Path;

use common::*;
use timelapse_server::db::{self, NewSegment};
use timelapse_server::{Ctx, reconcile, retention};

/// Writes a file of `bytes` and its row: the n-th 10-minute segment of a session.
async fn fake_segment(ctx: &Ctx, stream_id: i64, session_id: i64, seq: i64, bytes: usize) -> i64 {
    let dir = ctx.session_dir(stream_id, session_id);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{seq:06}.ts"));
    std::fs::write(&path, vec![0u8; bytes]).unwrap();
    let start = 1_000_000_000_000 + seq * 600_000;
    db::insert_segment(
        &ctx.pool,
        &NewSegment {
            session_id,
            stream_id,
            seq,
            path: ctx.relative(&path),
            wall_start: start,
            wall_end: start + 600_000,
            media_start: None,
            media_end: None,
            media_dur: 100.0,
            bytes: bytes as i64,
        },
    )
    .await
    .unwrap()
}

async fn ended_session(ctx: &Ctx, stream: &db::Stream) -> i64 {
    let id = db::create_session(&ctx.pool, stream.id, &stream.settings.0)
        .await
        .unwrap();
    db::end_session(&ctx.pool, id).await.unwrap();
    id
}

async fn surviving_seqs(ctx: &Ctx, stream_id: i64) -> Vec<i64> {
    db::list_segments(&ctx.pool, stream_id)
        .await
        .unwrap()
        .iter()
        .map(|s| s.seq)
        .collect()
}

#[tokio::test]
async fn size_limit_prunes_oldest_and_spares_leased() {
    let f = fixture("exit 1").await;
    let stream = add_stream(&f.ctx, "cam").await;
    let session = ended_session(&f.ctx, &stream).await;
    let mut ids = Vec::new();
    for seq in 0..6 {
        ids.push(fake_segment(&f.ctx, stream.id, session, seq, 1000).await);
    }
    db::lease_segment(&f.ctx.pool, ids[1], "export-1").await.unwrap();

    let mut config = stream.config();
    config.max_bytes = Some(3500);
    db::update_stream(&f.ctx.pool, &stream, &config).await.unwrap();

    let report = retention::run_once(&f.ctx).await.unwrap();
    // Newest three fit; 2 and 0 go; 1 is leased and stays.
    assert_eq!(report.segments, 2);
    assert_eq!(surviving_seqs(&f.ctx, stream.id).await, vec![1, 3, 4, 5]);
    let dir = f.ctx.session_dir(stream.id, session);
    assert!(!dir.join("000000.ts").exists() && !dir.join("000002.ts").exists());
    assert!(dir.join("000001.ts").exists());

    // Once the export lets go, it goes too.
    db::release_leases(&f.ctx.pool, "export-1").await.unwrap();
    retention::run_once(&f.ctx).await.unwrap();
    assert_eq!(surviving_seqs(&f.ctx, stream.id).await, vec![3, 4, 5]);
}

#[tokio::test]
async fn duration_limit_prunes_and_removes_emptied_sessions() {
    let f = fixture("exit 1").await;
    let stream = add_stream(&f.ctx, "cam").await;
    let old = ended_session(&f.ctx, &stream).await;
    let new = ended_session(&f.ctx, &stream).await;
    fake_segment(&f.ctx, stream.id, old, 0, 10).await;
    fake_segment(&f.ctx, stream.id, old, 1, 10).await;
    fake_segment(&f.ctx, stream.id, new, 2, 10).await;
    fake_segment(&f.ctx, stream.id, new, 3, 10).await;

    let mut config = stream.config();
    config.max_duration_secs = Some(25 * 60); // two and a half segments
    db::update_stream(&f.ctx.pool, &stream, &config).await.unwrap();
    retention::run_once(&f.ctx).await.unwrap();

    assert_eq!(surviving_seqs(&f.ctx, stream.id).await, vec![2, 3]);
    assert!(
        !f.ctx.session_dir(stream.id, old).exists(),
        "emptied session directory removed"
    );
    let sessions: Vec<i64> = db::list_sessions(&f.ctx.pool)
        .await
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(sessions, vec![new]);
}

#[tokio::test]
async fn retention_change_does_not_restart_recorder_but_settings_do() {
    let f = fixture("exit 1").await;
    let stream = add_stream(&f.ctx, "cam").await;

    let mut config = stream.config();
    config.max_bytes = Some(1 << 30);
    db::update_stream(&f.ctx.pool, &stream, &config).await.unwrap();
    let stream2 = db::find_stream(&f.ctx.pool, "cam").await.unwrap().unwrap();
    assert_eq!(stream2.revision, stream.revision);

    config.settings.crf = 25;
    db::update_stream(&f.ctx.pool, &stream2, &config).await.unwrap();
    let stream3 = db::find_stream(&f.ctx.pool, "cam").await.unwrap().unwrap();
    assert_eq!(stream3.revision, stream.revision + 1);
}

#[tokio::test]
async fn reconcile_repairs_a_crashed_data_dir() {
    let f = fixture("exit 1").await;
    let ctx = &f.ctx;
    let stream = add_stream(ctx, "cam").await;

    // A session the crash left open, holding one indexed segment and the unindexed one in flight.
    let crashed = db::create_session(&ctx.pool, stream.id, &stream.settings.0)
        .await
        .unwrap();
    fake_segment(ctx, stream.id, crashed, 0, 100).await;
    let in_flight = ctx.session_dir(stream.id, crashed).join("000001.ts");
    make_ts(&in_flight, 2).await;
    // A truncated-to-nothing file next to it, and a stray non-segment file.
    std::fs::write(ctx.session_dir(stream.id, crashed).join("000002.ts"), b"").unwrap();
    std::fs::write(ctx.session_dir(stream.id, crashed).join("junk.tmp"), b"x").unwrap();

    // A row whose file vanished.
    let other = ended_session(ctx, &stream).await;
    let vanished = fake_segment(ctx, stream.id, other, 5, 100).await;
    std::fs::remove_file(ctx.session_dir(stream.id, other).join("000005.ts")).unwrap();

    // A delete interrupted between marking and unlinking.
    let doomed = fake_segment(ctx, stream.id, other, 6, 100).await;
    db::mark_deleting(&ctx.pool, &[doomed]).await.unwrap();

    // Directories belonging to nothing.
    std::fs::create_dir_all(ctx.stream_dir(999).join("1")).unwrap();
    std::fs::create_dir_all(ctx.session_dir(stream.id, 777)).unwrap();
    // A stale lease from an export that died with the process.
    db::lease_segment(&ctx.pool, vanished, "export-x").await.ok();

    let report = reconcile::run(ctx).await.unwrap();
    assert_eq!(report.missing_rows, 1);
    assert_eq!(report.adopted, 1);

    let segments = db::list_segments(&ctx.pool, stream.id).await.unwrap();
    assert_eq!(segments.iter().map(|s| s.seq).collect::<Vec<_>>(), vec![0, 1]);
    let adopted = &segments[1];
    assert!(
        (adopted.media_dur - 2.0).abs() < 0.2,
        "adopted duration {}",
        adopted.media_dur
    );
    // 2 s of video at 6x is 12 s of stream, though never reaching back before the previous
    // segment's end.
    assert!(adopted.wall_end - adopted.wall_start <= 13_500);
    assert!(adopted.wall_start >= segments[0].wall_end);

    assert!(!ctx.session_dir(stream.id, crashed).join("000002.ts").exists());
    assert!(!ctx.session_dir(stream.id, crashed).join("junk.tmp").exists());
    assert!(
        !ctx.session_dir(stream.id, other).exists(),
        "emptied session cleaned up"
    );
    assert!(!ctx.stream_dir(999).exists());
    assert!(!ctx.session_dir(stream.id, 777).exists());
    assert!(
        db::list_sessions(&ctx.pool)
            .await
            .unwrap()
            .iter()
            .all(|s| s.ended_at.is_some()),
        "open sessions are closed"
    );
    // Running it again finds nothing to do.
    assert_eq!(reconcile::run(ctx).await.unwrap(), Default::default());
}

async fn make_ts(path: &Path, seconds: u32) {
    let status = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size=320x180:rate=30:duration={seconds}"))
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-f", "mpegts", "-y"])
        .arg(path)
        .status()
        .await
        .unwrap();
    assert!(status.success());
}
