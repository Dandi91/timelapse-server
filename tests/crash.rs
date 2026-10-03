//! A server killed with SIGKILL leaves its pipeline running. The next startup must wind it down
//! gracefully and adopt the segment it was writing.

mod common;

use std::process::{Command, Stdio};
use std::time::Duration;

use common::*;
use nix::sys::signal::killpg;
use nix::unistd::Pid;
use timelapse_server::procs::Identity;
use timelapse_server::{db, reconcile};

#[tokio::test]
async fn restart_adopts_the_segment_a_crash_left_in_flight() {
    // Like real yt-dlp, the script runs ffmpeg as a child rather than exec'ing it, so a grandchild
    // feeds the encoder.
    let f = fixture(&format!(
        "[ \"$1\" = --version ] && exit 0\n{}",
        pattern_source(600, true).replace("exec ", "")
    ))
    .await;
    let bin = env!("CARGO_BIN_EXE_timelapse-server");
    let run = |args: &[&str]| {
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .env("TIMELAPSE_DATA_DIR", &f.ctx.data_dir)
            .env("TIMELAPSE_YT_DLP", &f.ctx.tools.yt_dlp)
            .env("TIMELAPSE_MIN_FREE", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    };
    // One-minute segments: nothing completes before the crash.
    let added = run(&[
        "add",
        "https://example.com/x",
        "--label",
        "x",
        "--height",
        "180",
        "--segment-minutes",
        "1",
    ])
    .status()
    .unwrap();
    assert!(added.success());
    let mut server = run(&["serve", "--bind", "127.0.0.1:0"]).spawn().unwrap();

    let pool = f.ctx.pool.clone();
    let started = wait_for(Duration::from_secs(15), || async {
        db::open_pipelines(&pool)
            .await
            .unwrap()
            .iter()
            .any(|p| p.encoder.is_some())
    })
    .await;
    assert!(started, "pipeline never started");
    tokio::time::sleep(Duration::from_secs(6)).await;
    server.kill().unwrap(); // SIGKILL: no cleanup at all
    server.wait().unwrap();

    let pipeline = db::open_pipelines(&f.ctx.pool).await.unwrap().remove(0);
    let encoder = Identity::parse(pipeline.encoder.as_deref().unwrap()).unwrap();
    let fetcher = Identity::parse(pipeline.fetcher.as_deref().unwrap()).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        encoder.is_running(),
        "the orphaned encoder keeps recording until the restart"
    );

    let report = reconcile::run(&f.ctx).await.unwrap();
    assert_eq!(report.adopted, 1, "{report:?}");
    assert!(!encoder.is_running());
    let group_gone = killpg(Pid::from_raw(fetcher.pid as i32), None).is_err();
    assert!(group_gone, "yt-dlp's process group is gone, grandchild included");

    let segment = &db::all_segments(&f.ctx.pool).await.unwrap()[0];
    // About 7 s of stream at 6x, so a bit over a second of video.
    assert!(segment.media_dur > 0.8, "adopted segment lasts {}s", segment.media_dur);
    assert!(db::open_pipelines(&f.ctx.pool).await.unwrap().is_empty());
    let log = std::fs::read_to_string(f.ctx.stream_dir(1).join("capture.log")).unwrap();
    assert!(
        !log.contains("Immediate exit requested"),
        "ffmpeg was aborted instead of finishing:\n{log}"
    );
}
