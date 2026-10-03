mod common;

use std::time::{Duration, Instant};

use common::*;
use timelapse_server::db;
use timelapse_server::server;
use timelapse_server::supervisor::{self, Outcome};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn indexes_each_segment_as_ffmpeg_finishes_it() {
    // 36 s of stream at 6x is 6 s of timelapse: six 1 s segments.
    let f = fixture(&pattern_source(36, false)).await;
    let stream = add_stream(&f.ctx, "cam").await;

    let report = supervisor::attempt(&f.ctx, &stream, &CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(report.outcome, Outcome::Ended(_)), "{:?}", report.outcome);
    assert_eq!(report.segments, 6);

    let segments = db::list_segments(&f.ctx.pool, stream.id).await.unwrap();
    assert_eq!(segments.len(), 6);
    for (i, seg) in segments.iter().enumerate() {
        assert_eq!(seg.seq, i as i64);
        assert_eq!(seg.session_id, report.session_id);
        assert!(
            (seg.media_dur - 1.0).abs() < 0.05,
            "segment {i} lasts {}",
            seg.media_dur
        );
        let path = f.ctx.absolute(&seg.path);
        assert_eq!(std::fs::metadata(&path).unwrap().len() as i64, seg.bytes);
        assert!((probe(&path).await - 1.0).abs() < 0.1);
        assert!(seg.wall_start <= seg.wall_end);
    }
    // Wall-clock spans of one session never overlap.
    for pair in segments.windows(2) {
        assert!(pair[0].wall_end <= pair[1].wall_start);
    }
    // Timestamps run on across the session instead of restarting in every file.
    assert!(segments[5].media_start.unwrap() > segments[0].media_start.unwrap() + 4.0);
}

#[tokio::test]
async fn cancelling_finalizes_the_segment_in_flight() {
    // Real time and long segments, so nothing is finished when we cancel.
    let f = fixture(&pattern_source(120, true)).await;
    let mut stream = add_stream(&f.ctx, "cam").await;
    stream.settings.0.segment_minutes = 1.0;

    let cancel = CancellationToken::new();
    let stopper = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(4)).await;
            cancel.cancel();
            Instant::now()
        })
    };
    let report = supervisor::attempt(&f.ctx, &stream, &cancel).await.unwrap();
    let wind_down = stopper.await.unwrap().elapsed();

    assert!(wind_down < Duration::from_secs(5), "shutdown took {wind_down:?}");
    assert_eq!(report.outcome, Outcome::Ended("cancelled".into()));
    assert_eq!(report.segments, 1, "the partial segment is written out and indexed");
    let seg = &db::list_segments(&f.ctx.pool, stream.id).await.unwrap()[0];
    let duration = probe(&f.ctx.absolute(&seg.path)).await;
    assert!(duration > 0.3, "partial segment plays for {duration}s");
}

#[tokio::test]
async fn ended_stream_is_reported_offline_and_leaves_nothing_behind() {
    let f =
        fixture("echo '[download] Some stream does not pass filter (live_status!=?was_live), skipping ..' >&2\nexit 0")
            .await;
    let stream = add_stream(&f.ctx, "cam").await;

    let report = supervisor::attempt(&f.ctx, &stream, &CancellationToken::new())
        .await
        .unwrap();
    match report.outcome {
        Outcome::Offline(detail) => assert!(detail.contains("does not pass filter"), "{detail}"),
        other => panic!("expected offline, got {other:?}"),
    }
    assert_eq!(report.segments, 0);
    assert!(
        db::list_sessions(&f.ctx.pool).await.unwrap().is_empty(),
        "empty session is dropped"
    );
    assert!(!f.ctx.session_dir(stream.id, report.session_id).exists());

    let log = std::fs::read_to_string(f.ctx.stream_dir(stream.id).join("capture.log")).unwrap();
    assert!(
        log.contains("does not pass filter"),
        "yt-dlp stderr lands in the stream log"
    );
}

#[tokio::test]
async fn failing_fetcher_is_retried_with_status() {
    let f = fixture("echo 'ERROR: HTTP Error 403: Forbidden' >&2\nexit 1").await;
    let stream = add_stream(&f.ctx, "cam").await;

    let cancel = CancellationToken::new();
    let task = tokio::spawn(supervisor::run(f.ctx.clone(), stream.clone(), cancel.clone()));
    let pool = f.ctx.pool.clone();
    let retrying = wait_for(Duration::from_secs(10), || async {
        let s = db::find_stream(&pool, "cam").await.unwrap().unwrap();
        s.status == "retrying" && s.status_detail.as_deref() == Some("ERROR: HTTP Error 403: Forbidden")
    })
    .await;
    assert!(retrying);
    cancel.cancel();
    task.await.unwrap();
    assert_eq!(
        db::find_stream(&f.ctx.pool, "cam").await.unwrap().unwrap().status,
        "stopped"
    );
}

#[tokio::test]
async fn server_follows_the_stream_table() {
    let f = fixture(&pattern_source(600, true)).await;
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(server::serve(f.ctx.clone(), shutdown.clone(), None));

    // Added while running: picked up and recording within a few seconds.
    let stream = add_stream(&f.ctx, "cam").await;
    let pool = f.ctx.pool.clone();
    let id = stream.id;
    let recorded = wait_for(Duration::from_secs(20), || async {
        !db::list_segments(&pool, id).await.unwrap().is_empty()
    })
    .await;
    assert!(recorded, "no segment within 20 s");

    // Removed while running: recorder stops and the files go.
    db::delete_stream(&f.ctx.pool, stream.id).await.unwrap();
    let dir = f.ctx.stream_dir(stream.id);
    let cleaned = wait_for(Duration::from_secs(10), || async { !dir.exists() }).await;
    assert!(cleaned, "{} still exists", dir.display());

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn chatty_fetcher_log_is_rotated_mid_run() {
    // 6 MB of stderr from one attempt, past the 5 MB rotation size.
    let f = fixture("head -c 6000000 /dev/zero | tr '\\0' x | fold -w 99 >&2\nexit 1").await;
    let stream = add_stream(&f.ctx, "cam").await;

    supervisor::attempt(&f.ctx, &stream, &CancellationToken::new())
        .await
        .unwrap();
    let dir = f.ctx.stream_dir(stream.id);
    let current = std::fs::metadata(dir.join("capture.log")).unwrap().len();
    let rotated = std::fs::metadata(dir.join("capture.log.1")).unwrap().len();
    assert!(rotated > 5 << 20, "rotated log holds {rotated} bytes");
    assert!(current < 2 << 20, "fresh log holds {current} bytes");
}
