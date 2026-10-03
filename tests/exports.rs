//! Export jobs end to end, on real recorded segments with known wall-clock times.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use timelapse_server::db::{self, Stream};
use timelapse_server::{Ctx, exports, retention, server, supervisor};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const BASE: i64 = 1_800_000_000_000;

/// Record one session of `source` and pin its segments' wall-clock times: segment k spans
/// `start_s + 6k .. start_s + 6(k + 1)` seconds after BASE (1 s of video at 6x).
async fn record_session(ctx: &Ctx, stream: &Stream, start_s: i64) -> i64 {
    let report = supervisor::attempt(ctx, stream, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.segments, 6, "{:?}", report.outcome);
    for seg in db::list_segments(&ctx.pool, stream.id).await.unwrap() {
        if seg.session_id != report.session_id {
            continue;
        }
        let start = BASE + (start_s + 6 * seg.seq) * 1000;
        sqlx::query("UPDATE segments SET wall_start = ?, wall_end = ?, media_dur = 1.0 WHERE id = ?")
            .bind(start)
            .bind(start + 6000)
            .bind(seg.id)
            .execute(&ctx.pool)
            .await
            .unwrap();
    }
    report.session_id
}

struct Setup {
    ctx: Arc<Ctx>,
    _dir: tempfile::TempDir,
    stream: Stream,
    http: Client,
    base: String,
    shutdown: CancellationToken,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
}

/// Three sessions: A at 0-36 s, B at 60-96 s (same settings), C at 120-156 s at a larger height.
/// With `slow_exports`, ffmpeg reads export input in real time so a job runs for seconds.
async fn setup(slow_exports: bool) -> Setup {
    let (mut ctx, dir) = fixture_ctx(&pattern_source(36, false)).await;
    if slow_exports {
        let wrapper = write_script(
            dir.path(),
            "slow-ffmpeg",
            "case \"$*\" in *-progress*) exec ffmpeg -re \"$@\" ;; *) exec ffmpeg \"$@\" ;; esac",
        );
        ctx.tools.ffmpeg = wrapper;
    }
    let ctx = Arc::new(ctx);
    let mut stream = add_stream(&ctx, "cam").await;
    record_session(&ctx, &stream, 0).await;
    record_session(&ctx, &stream, 60).await;
    stream.settings.0.height = 240;
    record_session(&ctx, &stream, 120).await;
    // Done recording: keep the server from adding more.
    let mut config = stream.config();
    config.enabled = false;
    db::update_stream(&ctx.pool, &stream, &config).await.unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(server::serve(ctx.clone(), shutdown.clone(), Some(listener)));
    // Post-processing leases each segment briefly; let it finish so leases are the exports'.
    let pool = ctx.pool.clone();
    let thumbnailed = wait_for(Duration::from_secs(30), || async {
        db::list_segments(&pool, stream.id)
            .await
            .unwrap()
            .iter()
            .all(|s| s.thumbs.is_some() && s.parts.is_some())
    })
    .await;
    assert!(thumbnailed, "thumbnails never finished");
    Setup {
        ctx,
        _dir: dir,
        stream,
        http: Client::new(),
        base,
        shutdown,
        server,
    }
}

impl Setup {
    async fn export(&self, from_s: i64, to_s: i64, mode: &str) -> (StatusCode, Value) {
        let body =
            json!({"stream_id": self.stream.id, "from": BASE + from_s * 1000, "to": BASE + to_s * 1000, "mode": mode});
        let response = self
            .http
            .post(format!("{}/api/exports", self.base))
            .json(&body)
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }

    async fn job(&self, id: i64) -> Option<db::Export> {
        db::get_export(&self.ctx.pool, id).await.unwrap()
    }

    async fn finished(&self, id: i64) -> db::Export {
        let ok = wait_for(Duration::from_secs(60), || async {
            self.job(id)
                .await
                .is_some_and(|j| j.state == "done" || j.state == "failed")
        })
        .await;
        assert!(ok, "export {id} did not finish");
        let job = self.job(id).await.unwrap();
        assert_eq!(job.state, "done", "{:?}", job.error);
        job
    }

    async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(20), self.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

async fn first_frame(path: &std::path::Path) -> String {
    let out = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v",
            "-show_entries",
            "frame=key_frame,pict_type",
        ])
        .args(["-read_intervals", "%+#1", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .await
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim_end_matches(',')
        .to_string()
}

async fn decode_warnings(path: &std::path::Path) -> String {
    let out = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "warning", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .await
        .unwrap();
    String::from_utf8_lossy(&out.stderr).into_owned()
}

async fn height(path: &std::path::Path) -> u32 {
    let out = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v",
            "-show_entries",
            "stream=height",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .await
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

#[tokio::test]
async fn fast_export_snaps_to_a_keyframe_and_crosses_sessions() {
    let s = setup(false).await;
    // From 14 s (2.33 s into the video; the keyframe before is at 2 s = 12 s of wall time) to
    // 80 s (3.33 s into session B, whose video follows A's 6 s): 9.33 - 2 = 7.33 s of video.
    let (status, job) = s.export(14, 80, "fast").await;
    assert_eq!(status, 201, "{job}");
    // The worker may already have picked it up.
    assert!(
        ["queued", "running", "done"].contains(&job["state"].as_str().unwrap()),
        "{job}"
    );
    let job = s.finished(job["id"].as_i64().unwrap()).await;
    assert_eq!(job.used_mode.as_deref(), Some("fast"));
    assert_eq!(job.actual_from_ms, Some(BASE + 12_000));
    assert_eq!(job.actual_to_ms, Some(BASE + 80_000));
    let duration = job.duration.unwrap();
    assert!((duration - 7.333).abs() < 0.15, "clip lasts {duration}s");

    let path = s.ctx.absolute(job.path.as_ref().unwrap());
    assert_eq!(first_frame(&path).await, "1,I");
    assert_eq!(decode_warnings(&path).await, "");

    let response = s
        .http
        .get(format!("{}/api/exports/{}/file", s.base, job.id))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-disposition"],
        "attachment; filename=\"cam_2027-01-15_080012Z.mp4\""
    );
    assert_eq!(response.bytes().await.unwrap().len() as i64, job.bytes.unwrap());
    // Range requests work, so browsers can resume and players can seek.
    let partial = s
        .http
        .get(format!("{}/api/exports/{}/file", s.base, job.id))
        .header("range", "bytes=0-99")
        .send()
        .await
        .unwrap();
    assert_eq!(partial.status(), 206);
    s.stop().await;
}

#[tokio::test]
async fn mixed_settings_are_re_encoded_onto_one_canvas() {
    let s = setup(false).await;
    // From 63 s (0.5 s into B) to 129 s (1.5 s into C, captured at a larger height): 5.5 + 1.5 s.
    let (status, job) = s.export(63, 129, "fast").await;
    assert_eq!(status, 201, "{job}");
    let job = s.finished(job["id"].as_i64().unwrap()).await;
    assert_eq!(
        job.used_mode.as_deref(),
        Some("exact"),
        "fast is impossible across settings"
    );
    let duration = job.duration.unwrap();
    assert!((duration - 7.0).abs() < 0.15, "clip lasts {duration}s");
    let path = s.ctx.absolute(job.path.as_ref().unwrap());
    assert_eq!(height(&path).await, 240, "everything scaled to the larger canvas");
    assert_eq!(decode_warnings(&path).await, "");
    s.stop().await;
}

#[tokio::test]
async fn exact_export_cuts_where_asked() {
    let s = setup(false).await;
    let (_, job) = s.export(15, 27, "exact").await;
    let job = s.finished(job["id"].as_i64().unwrap()).await;
    assert_eq!(job.actual_from_ms, Some(BASE + 15_000));
    let duration = job.duration.unwrap();
    assert!(
        (duration - 2.0).abs() < 0.1,
        "12 s of wall time is 2 s of video, got {duration}s"
    );
    s.stop().await;
}

#[tokio::test]
async fn running_exports_hold_their_segments_and_can_be_cancelled() {
    let s = setup(true).await;
    // Sessions A and B, 12 s of video, read in real time.
    let (_, job) = s.export(0, 96, "exact").await;
    let id = job["id"].as_i64().unwrap();
    let running = wait_for(Duration::from_secs(10), || async {
        s.job(id)
            .await
            .is_some_and(|j| j.state == "running" && j.progress > 0.0)
    })
    .await;
    assert!(running, "export never got going");

    // Squeeze the stream to nothing: only the export's segments may survive.
    let stream = db::get_stream(&s.ctx.pool, s.stream.id).await.unwrap().unwrap();
    let mut config = stream.config();
    config.max_bytes = Some(1);
    db::update_stream(&s.ctx.pool, &stream, &config).await.unwrap();
    retention::run_once(&s.ctx).await.unwrap();
    let left = db::list_segments(&s.ctx.pool, s.stream.id).await.unwrap();
    assert_eq!(left.len(), 12, "A and B are leased, C is pruned");
    assert!(left.iter().all(|seg| seg.wall_start < BASE + 120_000));

    // Cancelling removes the job and everything it wrote.
    let response = s
        .http
        .delete(format!("{}/api/exports/{id}", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    let dir = exports::exports_dir(&s.ctx);
    let cleaned = wait_for(Duration::from_secs(5), || async {
        std::fs::read_dir(&dir).map(|d| d.count() == 0).unwrap_or(false)
    })
    .await;
    assert!(cleaned, "export files left behind");
    assert!(s.job(id).await.is_none());

    // Leases released: now retention takes the rest.
    let released = wait_for(Duration::from_secs(5), || async {
        retention::run_once(&s.ctx).await.unwrap();
        db::list_segments(&s.ctx.pool, s.stream.id).await.unwrap().is_empty()
    })
    .await;
    assert!(released, "segments still leased after cancelling");
    s.stop().await;
}

#[tokio::test]
async fn bad_requests_are_explained() {
    let s = setup(false).await;
    let (status, error) = s.export(50, 40, "fast").await;
    assert_eq!(
        (status, error["error"].as_str().unwrap()),
        (StatusCode::BAD_REQUEST, "the end must come after the start")
    );
    let (status, error) = s.export(40, 55, "fast").await;
    assert_eq!(
        (status, error["error"].as_str().unwrap()),
        (StatusCode::BAD_REQUEST, "no footage between those times")
    );
    let (status, _) = s.export(0, 10, "slow").await;
    assert_eq!(status, 400);
    let response = s
        .http
        .post(format!("{}/api/exports", s.base))
        .json(&json!({"stream_id": 99, "from": 0, "to": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(
        s.http
            .get(format!("{}/api/exports/99/file", s.base))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    s.stop().await;
}

#[tokio::test]
async fn interrupted_exports_resume_after_a_restart() {
    let (ctx, _dir) = fixture_ctx("exit 1").await;
    let stream = add_stream(&ctx, "cam").await;
    let id = db::create_export(&ctx.pool, &stream, 0, 1, "fast").await.unwrap();
    db::start_next_export(&ctx.pool).await.unwrap().unwrap();
    let dir = exports::exports_dir(&ctx);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{id}.part.mp4")), b"half").unwrap();
    std::fs::write(dir.join("stray.mp4"), b"?").unwrap();

    exports::recover(&ctx).await.unwrap();
    assert_eq!(db::get_export(&ctx.pool, id).await.unwrap().unwrap().state, "queued");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "partial and stray files removed"
    );
}
