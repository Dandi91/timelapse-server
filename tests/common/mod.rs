//! Shared fixtures. The recorder runs real ffmpeg; yt-dlp is replaced by a script that pipes
//! ffmpeg's test pattern, so nothing touches the network.

#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use timelapse_server::db::{self, StreamConfig};
use timelapse_server::pipeline::Tools;
use timelapse_server::settings::EncodeSettings;
use timelapse_server::{Ctx, Tuning};

pub struct Fixture {
    pub ctx: Arc<Ctx>,
    pub dir: TempDir,
}

/// Small, fast settings: 6x speedup, 1 s keyframes, one segment per 6 s of stream.
pub fn fast_settings() -> EncodeSettings {
    EncodeSettings {
        sample_fps: 5.0,
        source_fps: 30,
        out_fps: 30,
        height: 180,
        crf: 30,
        preset: "ultrafast".into(),
        segment_minutes: 0.1,
        keyframe_seconds: 1,
    }
}

/// A stand-in for yt-dlp: emits `seconds` of 30 fps test pattern as MPEG-TS on stdout,
/// in real time if `realtime`.
pub fn pattern_source(seconds: u32, realtime: bool) -> String {
    format!(
        "exec ffmpeg -hide_banner -loglevel error {} -f lavfi -i testsrc2=size=320x180:rate=30 -t {seconds} \
         -c:v libx264 -preset ultrafast -f mpegts -",
        if realtime { "-re" } else { "" }
    )
}

pub async fn fixture(fetcher_script: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let yt_dlp = write_script(dir.path(), "fake-yt-dlp", fetcher_script);
    let pool = db::connect(&data_dir.join("timelapse.db")).await.unwrap();
    let tools = Tools {
        yt_dlp,
        ffmpeg: "ffmpeg".into(),
        ffprobe: "ffprobe".into(),
    };
    let tuning = Tuning {
        retry_min: Duration::from_millis(200),
        retry_max: Duration::from_secs(1),
        good_run: Duration::from_secs(60),
        offline_retry: Duration::from_secs(1),
        poll_interval: Duration::from_millis(200),
        retention_interval: Duration::from_millis(500),
        min_free_bytes: 0,
    };
    Fixture {
        ctx: Arc::new(Ctx {
            pool,
            data_dir,
            tools,
            tuning,
        }),
        dir,
    }
}

pub fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

pub async fn add_stream(ctx: &Ctx, label: &str) -> db::Stream {
    let config = StreamConfig {
        label: label.into(),
        url: format!("https://example.com/{label}"),
        enabled: true,
        live_only: true,
        settings: fast_settings(),
        max_bytes: None,
        max_duration_secs: None,
    };
    db::insert_stream(&ctx.pool, &config).await.unwrap();
    db::find_stream(&ctx.pool, label).await.unwrap().unwrap()
}

pub async fn probe(path: &Path) -> f64 {
    timelapse_server::reconcile::probe_duration(Path::new("ffprobe"), path)
        .await
        .unwrap()
}

/// Poll `check` until it returns true or `limit` passes.
pub async fn wait_for<F, Fut>(limit: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}
