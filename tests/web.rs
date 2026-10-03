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
    let created = add_stream(&f.ctx, "cam").await;
    // 18 s of stream per segment: 3 s of video, a keyframe each second, so three parts each.
    let mut config = created.config();
    config.settings.segment_minutes = 0.3;
    db::update_stream(&f.ctx.pool, &created, &config).await.unwrap();
    let stream = db::find_stream(&f.ctx.pool, "cam").await.unwrap().unwrap();
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
    // Pin the range so recording that goes on meanwhile doesn't change what we compare, and let
    // post-processing index what is in it.
    let to = db::list_segments(&f.ctx.pool, stream.id)
        .await
        .unwrap()
        .last()
        .unwrap()
        .wall_end;
    let indexed = wait_for(Duration::from_secs(30), || async {
        db::segments_in_range(&pool, stream.id, None, Some(to))
            .await
            .unwrap()
            .iter()
            .all(|s| s.parts.is_some())
    })
    .await;
    assert!(indexed, "segments never got indexed");
    let segments = db::segments_in_range(&f.ctx.pool, stream.id, None, Some(to))
        .await
        .unwrap();
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
    // One entry per keyframe part.
    let parts: usize = segments.iter().map(|s| s.parts.as_ref().unwrap().0.len().max(1)).sum();
    assert!(
        parts >= 2 * segments.len(),
        "{parts} parts in {} segments",
        segments.len()
    );
    assert_eq!(playlist.matches("#EXTINF").count(), parts);
    assert_eq!(playlist.matches("#EXT-X-BYTERANGE").count(), parts);

    // Every range is served on its own and starts with the stream tables (SDT or PAT).
    let lines: Vec<&str> = playlist.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let Some(range) = line.strip_prefix("#EXT-X-BYTERANGE:") else {
            continue;
        };
        let (length, offset): (u64, u64) = {
            let (l, o) = range.split_once('@').unwrap();
            (l.parse().unwrap(), o.parse().unwrap())
        };
        let response = http
            .get(format!("{base}/streams/{}/{}", stream.id, lines[i + 1]))
            .header("range", format!("bytes={offset}-{}", offset + length - 1))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 206);
        let body = response.bytes().await.unwrap();
        assert_eq!(body.len() as u64, length);
        let pid = (u16::from(body[1] & 0x1f) << 8) | u16::from(body[2]);
        assert!(
            body[0] == 0x47 && (pid == 0x0 || pid == 0x11),
            "range {range} starts with pid {pid:#x}"
        );
    }

    // The files themselves are still there whole.
    let first_uri = playlist.lines().find(|l| l.ends_with(".ts")).unwrap();
    let body = get(format!("/streams/{}/{first_uri}", stream.id))
        .await
        .bytes()
        .await
        .unwrap();
    assert_eq!(body.len() as i64, segments[0].bytes);

    // A real HLS client plays the whole thing from byte ranges, discontinuities included.
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
