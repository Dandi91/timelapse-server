//! The HTTP side against a live server: API, playlists, segment files, UI.

mod common;

use std::time::Duration;

use common::*;
use timelapse_server::{db, server};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn serves_playable_playlists_across_sessions() {
    // The source ends after 36 s of stream and the recorder restarts it, so footage piles up in
    // several sessions with a discontinuity between each.
    let f = fixture(&pattern_source(36, false)).await;
    let stream = add_stream(&f.ctx, "cam").await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(server::serve(f.ctx.clone(), shutdown.clone(), Some(listener)));

    let pool = f.ctx.pool.clone();
    let two_sessions = wait_for(Duration::from_secs(30), || async {
        let segments = db::list_segments(&pool, stream.id).await.unwrap();
        segments
            .iter()
            .map(|s| s.session_id)
            .collect::<std::collections::HashSet<_>>()
            .len()
            >= 2
    })
    .await;
    assert!(two_sessions, "no second session within 30 s");
    // Pin the range so recording that goes on meanwhile doesn't change what we compare.
    let segments = db::list_segments(&f.ctx.pool, stream.id).await.unwrap();
    let to = segments.last().unwrap().wall_end;
    let range = format!("from=0&to={to}");

    let http = reqwest::Client::new();
    let get = |path: String| {
        let http = http.clone();
        let base = base.clone();
        async move { http.get(format!("{base}{path}")).send().await.unwrap() }
    };

    let streams: serde_json::Value = get("/api/streams".into()).await.json().await.unwrap();
    assert_eq!(streams[0]["label"], "cam");
    assert!(streams[0]["segments"].as_i64().unwrap() >= segments.len() as i64);
    let spans: i64 = segments.iter().map(|s| s.wall_end - s.wall_start).sum();
    assert!(
        streams[0]["recorded_ms"].as_i64().unwrap() >= spans,
        "recorded time is the sum of segment spans"
    );

    let listed: Vec<serde_json::Value> = get(format!("/api/streams/{}/segments?{range}", stream.id))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(listed.len(), segments.len());

    let response = get(format!("/streams/{}/playlist.m3u8?{range}", stream.id)).await;
    assert_eq!(response.headers()["content-type"], "application/vnd.apple.mpegurl");
    let playlist = response.text().await.unwrap();
    assert!(playlist.contains("#EXT-X-DISCONTINUITY"), "{playlist}");
    assert!(playlist.ends_with("#EXT-X-ENDLIST\n"));
    assert_eq!(playlist.matches("#EXTINF").count(), segments.len());

    // Every URI resolves next to the playlist and serves the file as recorded.
    let first_uri = playlist.lines().find(|l| l.ends_with(".ts")).unwrap();
    let body = get(format!("/streams/{}/{first_uri}", stream.id))
        .await
        .bytes()
        .await
        .unwrap();
    assert_eq!(body.len() as i64, segments[0].bytes);

    // A real HLS client plays the whole thing, discontinuities included.
    let url = format!("{base}/streams/{}/playlist.m3u8?{range}", stream.id);
    let played = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i", &url, "-f", "null", "-"])
        .output()
        .await
        .unwrap();
    assert!(played.status.success(), "{}", String::from_utf8_lossy(&played.stderr));
    let expected: f64 = segments.iter().map(|s| s.media_dur).sum();
    let probed = probe_url(&url).await;
    assert!(
        (probed - expected).abs() < 1.0,
        "playlist plays {probed}s, segments add up to {expected}s"
    );

    let live = get(format!("/streams/{}/playlist.m3u8?live=1", stream.id))
        .await
        .text()
        .await
        .unwrap();
    assert!(live.contains("#EXT-X-PLAYLIST-TYPE:EVENT") && !live.contains("ENDLIST"));

    assert_eq!(get("/streams/999/playlist.m3u8".into()).await.status(), 404);
    assert_eq!(
        get(format!("/streams/{}/playlist.m3u8?from=0&to=1", stream.id))
            .await
            .status(),
        404
    );
    // Only the streams directory is served; the database next to it is not reachable.
    assert_eq!(get("/streams/%2e%2e/timelapse.db".into()).await.status(), 404);
    assert_eq!(get("/streams/..%2ftimelapse.db".into()).await.status(), 404);

    let index = get("/".into()).await;
    assert!(
        index.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(index.text().await.unwrap().contains("hls.min.js"));
    assert_eq!(get("/vendor/hls.min.js".into()).await.status(), 200);
    assert_eq!(get("/nope.js".into()).await.status(), 404);

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(15), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

async fn probe_url(url: &str) -> f64 {
    let output = tokio::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", url])
        .output()
        .await
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().parse().unwrap()
}
