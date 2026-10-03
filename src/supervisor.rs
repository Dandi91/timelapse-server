//! One task per stream: runs `yt-dlp -o - | ffmpeg`, indexes each segment as ffmpeg finishes it,
//! and restarts the pipeline with backoff until cancelled.
//!
//! A drop costs the gap and nothing more: the segment in flight is finalized, and the next attempt
//! opens a new session.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::Ctx;
use crate::db::{self, NewSegment, Stream};
use crate::events::Event;
use crate::pipeline::{self, OFFLINE_MARKERS};

const LOG_ROTATE_BYTES: u64 = 5 << 20;

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Nothing live at the URL (ended, or a channel that isn't streaming).
    Offline(String),
    /// The pipeline stopped for any other reason: a drop, an error, or cancellation.
    Ended(String),
}

#[derive(Debug)]
pub struct AttemptReport {
    pub session_id: i64,
    pub segments: usize,
    pub outcome: Outcome,
}

pub async fn run(ctx: Arc<Ctx>, stream: Stream, cancel: CancellationToken) {
    let label = stream.label.as_str();
    let tuning = &ctx.tuning;
    let mut backoff = tuning.retry_min;
    let mut attempt_no = 0u32;

    while !cancel.is_cancelled() {
        attempt_no += 1;
        info!(stream = label, attempt = attempt_no, "starting pipeline");
        let started = Instant::now();
        let outcome = match attempt(&ctx, &stream, &cancel).await {
            Ok(report) => report.outcome,
            Err(e) => Outcome::Ended(format!("{e:#}")),
        };
        if cancel.is_cancelled() {
            break;
        }

        let (status, detail, delay) = match outcome {
            Outcome::Offline(detail) => ("offline", detail, tuning.offline_retry),
            Outcome::Ended(detail) => {
                backoff = if started.elapsed() >= tuning.good_run {
                    tuning.retry_min
                } else {
                    (backoff * 2).min(tuning.retry_max)
                };
                ("retrying", detail, backoff)
            }
        };
        info!(
            stream = label,
            status,
            detail,
            "pipeline ended after {:.0?}, next try in {delay:.0?}",
            started.elapsed()
        );
        ctx.set_status(stream.id, status, Some(&detail)).await;
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = sleep(delay) => {}
        }
    }
    ctx.set_status(stream.id, "stopped", None).await;
    info!(stream = label, "recorder stopped");
}

/// One run of the pipeline, start to finish, in its own session.
pub async fn attempt(ctx: &Ctx, stream: &Stream, cancel: &CancellationToken) -> Result<AttemptReport> {
    let settings = &stream.settings.0;
    let session_id = db::create_session(&ctx.pool, stream.id, settings).await?;
    let dir = ctx.session_dir(stream.id, session_id);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let log = open_log(&ctx.stream_dir(stream.id), session_id)?;
    let fetch_log = tokio::fs::File::from_std(log.try_clone()?);

    let mut fetcher = Command::new(&ctx.tools.yt_dlp)
        .args(pipeline::fetch_args(&stream.url, settings, stream.live_only))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group, so we can signal yt-dlp *and* the ffmpeg it spawns to pull HLS;
        // that grandchild holds the write end of the pipe.
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", ctx.tools.yt_dlp.display()))?;
    let fetcher_group = Pid::from_raw(fetcher.id().context("yt-dlp exited at once")? as i32);
    let video: Stdio = fetcher.stdout.take().context("no yt-dlp stdout")?.try_into()?;
    let tee = tokio::spawn(tee_fetcher_log(
        fetcher.stderr.take().context("no yt-dlp stderr")?,
        fetch_log,
        ctx.stream_dir(stream.id),
    ));

    let encoder = Command::new(&ctx.tools.ffmpeg)
        .args(pipeline::encode_args(settings, &dir))
        .stdin(video)
        .stdout(Stdio::piped())
        .stderr(log)
        .kill_on_drop(true)
        .spawn();
    let mut encoder = match encoder {
        Ok(child) => child,
        Err(e) => {
            reap_group(&mut fetcher, fetcher_group).await;
            return Err(e).with_context(|| format!("starting {}", ctx.tools.ffmpeg.display()));
        }
    };
    // Recorded so a restart after a crash can wind down a pipeline that outlived us.
    let pids = (fetcher.id(), encoder.id());
    if let (Some(fetcher_pid), Some(encoder_pid)) = pids
        && let Err(e) = db::set_session_pids(&ctx.pool, session_id, fetcher_pid, encoder_pid).await
    {
        warn!(stream = stream.label, "recording pipeline pids: {e:#}");
    }
    ctx.set_status(stream.id, "recording", None).await;

    let mut indexer = Indexer {
        ctx,
        stream,
        session_id,
        dir: &dir,
        count: 0,
        last_wall_end: None,
    };
    let mut list = BufReader::new(encoder.stdout.take().context("no ffmpeg stdout")?).lines();

    // Wind down so the segment in flight stays playable: stop the fetcher and let ffmpeg finish on
    // EOF; if it doesn't, SIGTERM makes ffmpeg write its trailer; SIGKILL is the last resort.
    let mut stage = Stage::Running;
    let mut deadline = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled(), if stage == Stage::Running => {
                let _ = killpg(fetcher_group, Signal::SIGTERM);
                (stage, deadline) = (Stage::FetcherTerm, Some(Instant::now() + Duration::from_secs(15)));
            }
            _ = sleep_until(deadline.unwrap_or_else(far_future)), if deadline.is_some() => {
                (stage, deadline) = match stage {
                    Stage::FetcherTerm => {
                        let _ = killpg(fetcher_group, Signal::SIGKILL);
                        (Stage::EncoderDrain, Some(Instant::now() + Duration::from_secs(10)))
                    }
                    Stage::EncoderDrain => {
                        signal_child(&encoder, Signal::SIGTERM);
                        (Stage::EncoderTerm, Some(Instant::now() + Duration::from_secs(30)))
                    }
                    _ => {
                        warn!(stream = stream.label, "ffmpeg ignored SIGTERM, killing it; last segment is truncated");
                        signal_child(&encoder, Signal::SIGKILL);
                        (Stage::Killed, None)
                    }
                };
            }
            line = list.next_line() => match line {
                Ok(Some(line)) => indexer.handle(&line).await,
                Ok(None) => break,
                Err(e) => {
                    warn!(stream = stream.label, "reading ffmpeg segment list: {e}");
                    break;
                }
            },
        }
    }

    // ffmpeg has closed its segment list, so it is exiting.
    if timeout(Duration::from_secs(30), encoder.wait()).await.is_err() {
        let _ = encoder.kill().await;
    }
    let encoder_status = encoder.wait().await.ok();
    reap_group(&mut fetcher, fetcher_group).await;
    let fetch = timeout(Duration::from_secs(5), tee)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();

    db::end_session(&ctx.pool, session_id).await?;
    if indexer.count == 0 {
        db::delete_session(&ctx.pool, session_id).await?;
        let _ = std::fs::remove_dir_all(&dir);
    }

    let outcome = if fetch.offline && indexer.count == 0 {
        Outcome::Offline(fetch.last_error.unwrap_or_else(|| "not live".into()))
    } else if cancel.is_cancelled() {
        Outcome::Ended("cancelled".into())
    } else {
        Outcome::Ended(fetch.last_error.unwrap_or_else(|| match encoder_status {
            Some(status) => format!("ffmpeg exited ({status})"),
            None => "pipeline exited".into(),
        }))
    };
    Ok(AttemptReport {
        session_id,
        segments: indexer.count,
        outcome,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    Running,
    FetcherTerm,
    EncoderDrain,
    EncoderTerm,
    Killed,
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(86400 * 365)
}

fn signal_child(child: &Child, signal: Signal) {
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), signal);
    }
}

/// Signal the whole group, so yt-dlp's own ffmpeg child goes too.
async fn reap_group(fetcher: &mut Child, group: Pid) {
    for signal in [Signal::SIGTERM, Signal::SIGKILL] {
        let _ = killpg(group, signal);
        if timeout(Duration::from_secs(15), fetcher.wait()).await.is_ok() {
            // The leader is gone; make sure no grandchild outlives it.
            let _ = killpg(group, Signal::SIGKILL);
            return;
        }
    }
}

/// Turns ffmpeg's "segment finished" lines into rows.
struct Indexer<'a> {
    ctx: &'a Ctx,
    stream: &'a Stream,
    session_id: i64,
    dir: &'a Path,
    count: usize,
    last_wall_end: Option<i64>,
}

impl Indexer<'_> {
    async fn handle(&mut self, line: &str) {
        let label = &self.stream.label;
        let Some(seg) = pipeline::parse_segment_line(line) else {
            warn!(stream = label, "unexpected segment list line {line:?}");
            return;
        };
        let path = self.dir.join(&seg.file);
        let bytes = match std::fs::metadata(&path) {
            Ok(meta) => meta.len() as i64,
            Err(e) => {
                warn!(stream = label, "finished segment {} is missing: {e}", path.display());
                return;
            }
        };
        let media_dur = (seg.media_end - seg.media_start).max(0.0);
        // The line arrives as the segment closes, so "now" is its wall-clock end; the start
        // follows from the speedup, clamped so segments of one session never overlap.
        let wall_end = db::now_ms();
        let span = (media_dur * self.stream.settings.0.speedup() * 1000.0) as i64;
        let wall_start = (wall_end - span)
            .max(self.last_wall_end.unwrap_or(i64::MIN))
            .min(wall_end);
        let row = NewSegment {
            session_id: self.session_id,
            stream_id: self.stream.id,
            seq: pipeline::seq_from_file(&seg.file).unwrap_or(self.count as i64),
            path: self.ctx.relative(&path),
            wall_start,
            wall_end,
            media_start: Some(seg.media_start),
            media_end: Some(seg.media_end),
            media_dur,
            bytes,
        };
        match db::insert_segment(&self.ctx.pool, &row).await {
            Ok(segment_id) => {
                self.ctx.emit(Event::SegmentAdded {
                    stream_id: row.stream_id,
                    segment_id,
                    wall_start: row.wall_start,
                    wall_end: row.wall_end,
                    bytes: row.bytes,
                });
                self.ctx.thumbs_wake.notify_one();
                self.count += 1;
                self.last_wall_end = Some(wall_end);
            }
            // Most likely the stream was deleted while recording; the files go with its directory.
            Err(e) => warn!(stream = label, "indexing {}: {e:#}", row.path),
        }
    }
}

#[derive(Debug, Default)]
struct FetchReport {
    offline: bool,
    last_error: Option<String>,
}

/// Copy yt-dlp's stderr into the stream log, noting errors and "not live" messages on the way.
/// A pipeline can run for days, so the log is rotated here too, not only between attempts.
async fn tee_fetcher_log(stderr: ChildStderr, mut log: tokio::fs::File, stream_dir: PathBuf) -> FetchReport {
    let mut report = FetchReport::default();
    let mut size = log.metadata().await.map(|m| m.len()).unwrap_or(0);
    let mut lines = BufReader::new(stderr).split(b'\n');
    while let Ok(Some(line)) = lines.next_segment().await {
        if size > LOG_ROTATE_BYTES
            && let Ok(file) = rotate_log(&stream_dir)
        {
            log = tokio::fs::File::from_std(file);
            size = 0;
        }
        let _ = log.write_all(&line).await;
        let _ = log.write_all(b"\n").await;
        size += line.len() as u64 + 1;
        let text = String::from_utf8_lossy(&line);
        if OFFLINE_MARKERS.iter().any(|m| text.contains(m)) {
            report.offline = true;
        }
        if text.starts_with("ERROR:") || OFFLINE_MARKERS.iter().any(|m| text.contains(m)) {
            report.last_error = Some(text.trim().chars().take(500).collect());
        }
    }
    let _ = log.flush().await;
    report
}

/// The per-stream `capture.log`, rotated once it grows past a few megabytes.
fn open_log(stream_dir: &Path, session_id: i64) -> Result<std::fs::File> {
    let path = stream_dir.join("capture.log");
    let mut file = if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        rotate_log(stream_dir)?
    } else {
        std::fs::OpenOptions::new().create(true).append(true).open(&path)?
    };
    writeln!(
        file,
        "\n=== session {session_id} at {} ===",
        crate::units::format_utc(db::now_ms())
    )?;
    Ok(file)
}

/// Move `capture.log` to `capture.log.1` and start a fresh one. ffmpeg keeps writing its (quiet)
/// warnings to the old file until the next attempt; that's fine.
fn rotate_log(stream_dir: &Path) -> Result<std::fs::File> {
    let path = stream_dir.join("capture.log");
    std::fs::rename(&path, stream_dir.join("capture.log.1"))?;
    Ok(std::fs::OpenOptions::new().create(true).append(true).open(&path)?)
}
