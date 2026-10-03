//! Cutting clips out of a stream's recordings, one job at a time in the background.
//!
//! The overlapping segments are concatenated by ffmpeg's concat demuxer, read from their start, and
//! trimmed with output-side `-ss`/`-to`. Seeking is avoided on purpose: in MPEG-TS it lands on the
//! wrong keyframe. Fast mode copies the stream and so starts at the keyframe at or before the
//! requested time; exact mode re-encodes and cuts to the frame.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::db::{self, Export, ExportSegment};
use crate::events::Event;
use crate::settings::EncodeSettings;
use crate::{Ctx, reconcile};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Fast,
    Exact,
}

impl Mode {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "fast" => Some(Self::Fast),
            "exact" => Some(Self::Exact),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Exact => "exact",
        }
    }
}

/// How one export will run.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Input for ffmpeg's concat demuxer.
    pub concat: String,
    /// Output-side trim, in seconds on the concatenated timeline.
    pub ss: f64,
    pub to: f64,
    pub mode: Mode,
    /// x264 quality and preset for exact mode.
    pub encode: Option<(u8, String)>,
    /// Scale everything to this size, when the range spans different capture heights.
    pub canvas: Option<(u32, u32)>,
    /// Expected length of the clip in seconds.
    pub duration: f64,
    /// The wall-clock span the clip really covers.
    pub actual_from: i64,
    pub actual_to: i64,
}

/// Work out cuts for `[from, to)` over `segments` (all overlapping the range, in playback order).
pub fn plan(segments: &[ExportSegment], from: i64, to: i64, requested: Mode, data_dir: &Path) -> Result<Plan> {
    let (Some(first), Some(last)) = (segments.first(), segments.last()) else {
        bail!("no footage between those times");
    };
    let mut offsets = Vec::with_capacity(segments.len());
    let mut total = 0.0;
    for seg in segments {
        offsets.push(total);
        total += seg.media_dur;
    }
    let start = fraction(first, from) * first.media_dur;
    let end = offsets[segments.len() - 1] + fraction(last, to) * last.media_dur;
    if end - start < 0.1 {
        bail!("that range holds less than a tenth of a second of video");
    }

    let settings: &EncodeSettings = &first.settings.0;
    let uniform = segments.iter().all(|s| s.settings.0 == *settings);
    let mode = if requested == Mode::Fast && uniform {
        Mode::Fast
    } else {
        Mode::Exact
    };

    let (ss, clip_start) = match mode {
        // Segments begin on a keyframe and keyframes follow every `keyframe_seconds`, so the
        // keyframe at or before `start` is known exactly. The copy path compares decode
        // timestamps, which trail by a few frames, and drops leading non-keyframes anyway, so
        // aiming half an interval early lands precisely on it.
        Mode::Fast => {
            let interval = settings.keyframe_seconds as f64;
            let keyframe = (start / interval).floor() * interval;
            ((keyframe - interval / 2.0).max(0.0), keyframe)
        }
        Mode::Exact => (start, start),
    };

    let (encode, canvas) = match mode {
        Mode::Fast => (None, None),
        Mode::Exact => {
            let newest = &last.settings.0;
            let tallest = segments
                .iter()
                .map(|s| &s.settings.0)
                .max_by_key(|s| s.height)
                .unwrap_or(newest);
            let mixed = segments.iter().any(|s| s.settings.0.height != tallest.height);
            (
                Some((newest.crf, newest.preset.clone())),
                mixed.then(|| tallest.canvas()),
            )
        }
    };

    let mut concat = String::from("ffconcat version 1.0\n");
    for seg in segments {
        let path = data_dir.join(&seg.path);
        // Single quotes are the only special character inside a quoted concat path.
        let quoted = path.to_string_lossy().replace('\'', r"'\''");
        let _ = writeln!(concat, "file '{quoted}'\nduration {:.6}", seg.media_dur);
    }

    Ok(Plan {
        concat,
        ss,
        to: end,
        mode,
        encode,
        canvas,
        duration: end - clip_start,
        actual_from: wall_at(segments, &offsets, clip_start),
        actual_to: wall_at(segments, &offsets, end),
    })
}

/// Where `ms` falls within a segment, from 0 to 1.
fn fraction(seg: &ExportSegment, ms: i64) -> f64 {
    let span = (seg.wall_end - seg.wall_start) as f64;
    if span <= 0.0 {
        0.0
    } else {
        ((ms - seg.wall_start) as f64 / span).clamp(0.0, 1.0)
    }
}

/// Wall-clock time at `t` seconds into the concatenated timeline.
fn wall_at(segments: &[ExportSegment], offsets: &[f64], t: f64) -> i64 {
    let i = offsets.iter().rposition(|&o| o <= t).unwrap_or(0);
    let seg = &segments[i];
    let within = if seg.media_dur > 0.0 {
        ((t - offsets[i]) / seg.media_dur).clamp(0.0, 1.0)
    } else {
        0.0
    };
    seg.wall_start + (within * (seg.wall_end - seg.wall_start) as f64).round() as i64
}

pub fn ffmpeg_args(plan: &Plan, list: &Path, output: &Path) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-nostats", "-loglevel", "error", "-y"]
        .map(String::from)
        .into();
    args.extend(["-f", "concat", "-safe", "0", "-i"].map(String::from));
    args.push(list.to_string_lossy().into_owned());
    args.extend([
        "-ss".into(),
        format!("{:.6}", plan.ss),
        "-to".into(),
        format!("{:.6}", plan.to),
    ]);
    args.extend(["-map", "0:v:0", "-an"].map(String::from));
    match &plan.encode {
        None => args.extend(["-c", "copy"].map(String::from)),
        Some((crf, preset)) => {
            if let Some((w, h)) = plan.canvas {
                args.push("-vf".into());
                args.push(format!(
                    "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:-1:-1"
                ));
            }
            args.extend(["-c:v".into(), "libx264".into(), "-preset".into(), preset.clone()]);
            args.extend(["-crf".into(), crf.to_string(), "-pix_fmt".into(), "yuv420p".into()]);
        }
    }
    args.extend(["-movflags", "+faststart", "-progress", "pipe:1"].map(String::from));
    args.push(output.to_string_lossy().into_owned());
    args
}

/// Seconds of output written, from a `-progress` line.
pub fn parse_progress(line: &str) -> Option<f64> {
    let micros: f64 = line.strip_prefix("out_time_us=")?.trim().parse().ok()?;
    Some(micros / 1e6)
}

/// A download name like `bridges-cam-1_2026-10-03_104710Z.mp4`.
pub fn file_name(label: &str, from_ms: i64) -> String {
    let safe: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let when = crate::units::format_utc(from_ms).replace(' ', "_").replace(':', "");
    format!("{safe}_{when}.mp4")
}

/// Lets the API wake the worker and cancel the job it is running.
#[derive(Default)]
pub struct Control {
    pub wake: Notify,
    current: Mutex<Option<(i64, CancellationToken)>>,
}

impl Control {
    /// Cancel export `id` if it is the one running.
    pub fn cancel(&self, id: i64) -> bool {
        let current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        match &*current {
            Some((running, token)) if *running == id => {
                token.cancel();
                true
            }
            _ => false,
        }
    }

    fn set(&self, job: Option<(i64, CancellationToken)>) {
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = job;
    }
}

pub fn exports_dir(ctx: &Ctx) -> PathBuf {
    ctx.data_dir.join("exports")
}

fn final_path(ctx: &Ctx, id: i64) -> PathBuf {
    exports_dir(ctx).join(format!("{id}.mp4"))
}

/// After a restart: interrupted jobs go back in the queue, and leftovers of them are removed,
/// along with files no job owns.
pub async fn recover(ctx: &Ctx) -> Result<()> {
    let requeued = db::requeue_running_exports(&ctx.pool).await?;
    if requeued > 0 {
        info!("requeued {requeued} interrupted export(s)");
    }
    let dir = exports_dir(ctx);
    std::fs::create_dir_all(&dir)?;
    let owned: Vec<String> = db::list_exports(&ctx.pool)
        .await?
        .into_iter()
        .filter_map(|e| e.path)
        .collect();
    for entry in std::fs::read_dir(&dir)?.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !owned.contains(&ctx.relative(&path)) {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Runs queued exports one after another until `shutdown`.
pub async fn worker(ctx: Arc<Ctx>, shutdown: CancellationToken) {
    loop {
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            match db::start_next_export(&ctx.pool).await {
                Ok(Some(job)) => run(&ctx, job, &shutdown).await,
                Ok(None) => break,
                Err(e) => {
                    error!("picking the next export: {e:#}");
                    break;
                }
            }
        }
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ctx.exports.wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(60)) => {}
        }
    }
}

enum JobError {
    /// Deleted by the user while running; the row is already gone.
    Cancelled,
    /// The server is stopping; the job runs again after the restart.
    Interrupted,
    Failed(String),
}

async fn run(ctx: &Ctx, job: Export, shutdown: &CancellationToken) {
    let id = job.id;
    let token = shutdown.child_token();
    ctx.exports.set(Some((id, token.clone())));
    ctx.emit(Event::ExportUpdated {
        id,
        state: "running".into(),
        progress: 0.0,
    });
    info!(export = id, stream = job.stream_label, "export started");

    let holder = format!("export-{id}");
    let outcome = execute(ctx, &job, &holder, &token, shutdown).await;
    ctx.exports.set(None);
    if let Err(e) = db::release_leases(&ctx.pool, &holder).await {
        warn!(export = id, "releasing leases: {e:#}");
    }
    let dir = exports_dir(ctx);
    let _ = std::fs::remove_file(dir.join(format!("{id}.txt")));
    let _ = std::fs::remove_file(dir.join(format!("{id}.part.mp4")));

    let result = match outcome {
        Ok(()) => {
            // Deleted while finishing: don't leave the file behind.
            if db::get_export(&ctx.pool, id).await.ok().flatten().is_none() {
                let _ = std::fs::remove_file(final_path(ctx, id));
            }
            info!(export = id, "export done");
            Ok(())
        }
        Err(JobError::Cancelled) => {
            info!(export = id, "export cancelled");
            Ok(())
        }
        Err(JobError::Interrupted) => db::requeue_export(&ctx.pool, id).await,
        Err(JobError::Failed(message)) => {
            warn!(export = id, "export failed: {message}");
            db::fail_export(&ctx.pool, id, &message).await
        }
    };
    if let Err(e) = result {
        error!(export = id, "recording the export's outcome: {e:#}");
    }
    ctx.emit(Event::ExportsChanged);
}

async fn execute(
    ctx: &Ctx,
    job: &Export,
    holder: &str,
    token: &CancellationToken,
    shutdown: &CancellationToken,
) -> Result<(), JobError> {
    let failed = |e: anyhow::Error| JobError::Failed(format!("{e:#}"));
    let stream_id = job
        .stream_id
        .ok_or_else(|| JobError::Failed("the stream was removed".into()))?;
    let segments = db::lease_range(&ctx.pool, stream_id, job.from_ms, job.to_ms, holder)
        .await
        .map_err(failed)?;
    let mode = Mode::parse(&job.mode).unwrap_or(Mode::Fast);
    let plan = plan(&segments, job.from_ms, job.to_ms, mode, &ctx.data_dir).map_err(failed)?;
    db::set_export_plan(&ctx.pool, job.id, plan.mode.as_str(), plan.actual_from, plan.actual_to)
        .await
        .map_err(failed)?;

    let dir = exports_dir(ctx);
    std::fs::create_dir_all(&dir).map_err(|e| failed(e.into()))?;
    let list = dir.join(format!("{}.txt", job.id));
    let partial = dir.join(format!("{}.part.mp4", job.id));
    std::fs::write(&list, &plan.concat).map_err(|e| failed(e.into()))?;

    let mut child = Command::new(&ctx.tools.ffmpeg)
        .args(ffmpeg_args(&plan, &list, &partial))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| failed(anyhow::Error::from(e).context("starting ffmpeg")))?;
    let mut stderr = child.stderr.take().expect("piped");
    let errors = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    });
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();

    let mut reported = (0.0, Instant::now());
    loop {
        tokio::select! {
            _ = token.cancelled() => {
                let _ = child.kill().await;
                return Err(if shutdown.is_cancelled() { JobError::Interrupted } else { JobError::Cancelled });
            }
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    let Some(done) = parse_progress(&line) else { continue };
                    let progress = (done / plan.duration).clamp(0.0, 0.99);
                    // A row update and an event per percent or second is plenty.
                    if progress - reported.0 >= 0.01 || reported.1.elapsed() >= Duration::from_secs(1) {
                        reported = (progress, Instant::now());
                        let _ = db::set_export_progress(&ctx.pool, job.id, progress).await;
                        ctx.emit(Event::ExportUpdated { id: job.id, state: "running".into(), progress });
                    }
                }
                Ok(None) | Err(_) => break,
            },
        }
    }
    let status = child.wait().await.map_err(|e| failed(e.into()))?;
    let errors = errors.await.unwrap_or_default();
    if !status.success() {
        let tail: Vec<&str> = errors.lines().rev().take(3).collect();
        let detail = tail.into_iter().rev().collect::<Vec<_>>().join("; ");
        return Err(JobError::Failed(format!("ffmpeg failed ({status}): {detail}")));
    }

    let output = final_path(ctx, job.id);
    std::fs::rename(&partial, &output).map_err(|e| failed(e.into()))?;
    let bytes = std::fs::metadata(&output).map(|m| m.len() as i64).unwrap_or(0);
    let duration = reconcile::probe_duration(&ctx.tools.ffprobe, &output)
        .await
        .unwrap_or(plan.duration);
    db::finish_export(&ctx.pool, job.id, &ctx.relative(&output), bytes, duration)
        .await
        .map_err(failed)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::types::Json;

    fn settings(height: u32) -> EncodeSettings {
        EncodeSettings {
            height,
            keyframe_seconds: 5,
            ..Default::default()
        }
    }

    /// A 100 s segment covering 600 s of wall time from `start` (in seconds).
    fn seg(session_id: i64, start: i64, height: u32) -> ExportSegment {
        ExportSegment {
            id: start,
            session_id,
            path: format!("streams/1/{session_id}/{start}.ts"),
            wall_start: start * 1000,
            wall_end: (start + 600) * 1000,
            media_dur: 100.0,
            settings: Json(settings(height)),
        }
    }

    #[test]
    fn fast_snaps_back_to_a_keyframe() {
        let segs = [seg(1, 0, 1080), seg(1, 600, 1080)];
        // 200 s of wall time is 33.33 s of video: the keyframe before is at 30 s.
        let p = plan(&segs, 200_000, 900_000, Mode::Fast, Path::new("/d")).unwrap();
        assert_eq!(p.mode, Mode::Fast);
        assert_eq!(p.ss, 27.5);
        assert!((p.to - 150.0).abs() < 1e-9);
        assert!((p.duration - 120.0).abs() < 1e-9);
        assert_eq!(
            p.actual_from, 180_000,
            "keyframe at 30 s of video is 180 s of wall time"
        );
        assert_eq!(p.actual_to, 900_000);
        assert!(p.encode.is_none() && p.canvas.is_none());
        assert_eq!(
            p.concat,
            "ffconcat version 1.0\nfile '/d/streams/1/1/0.ts'\nduration 100.000000\n\
             file '/d/streams/1/1/600.ts'\nduration 100.000000\n"
        );
    }

    #[test]
    fn exact_cuts_where_asked_and_clamps_to_footage() {
        let segs = [seg(1, 0, 1080)];
        let p = plan(&segs, 200_000, 400_000, Mode::Exact, Path::new("/d")).unwrap();
        assert!((p.ss - 33.333333).abs() < 1e-5 && (p.to - 66.666666).abs() < 1e-5);
        assert_eq!((p.actual_from, p.actual_to), (200_000, 400_000));
        assert_eq!(p.encode, Some((21, "veryfast".into())));
        // Asking for more than exists gives what exists.
        let p = plan(&segs, -50_000, 9_000_000, Mode::Exact, Path::new("/d")).unwrap();
        assert_eq!((p.ss, p.to), (0.0, 100.0));
    }

    #[test]
    fn mixed_settings_force_exact_and_one_canvas() {
        let segs = [seg(1, 0, 720), seg(2, 1000, 1080)];
        let p = plan(&segs, 0, 1_600_000, Mode::Fast, Path::new("/d")).unwrap();
        assert_eq!(p.mode, Mode::Exact);
        assert_eq!(p.canvas, Some((1920, 1080)));
    }

    #[test]
    fn rejects_empty_and_tiny_ranges() {
        assert!(plan(&[], 0, 1000, Mode::Fast, Path::new("/d")).is_err());
        assert!(plan(&[seg(1, 0, 1080)], 1000, 1100, Mode::Exact, Path::new("/d")).is_err());
    }

    #[test]
    fn quotes_paths_for_concat() {
        let mut s = seg(1, 0, 1080);
        s.path = "streams/it's.ts".into();
        let p = plan(&[s], 0, 600_000, Mode::Fast, Path::new("/d")).unwrap();
        assert!(p.concat.contains(r"file '/d/streams/it'\''s.ts'"), "{}", p.concat);
    }

    #[test]
    fn builds_args_and_reads_progress() {
        let segs = [seg(1, 0, 720), seg(2, 1000, 1080)];
        let p = plan(&segs, 0, 1_600_000, Mode::Exact, Path::new("/d")).unwrap();
        let args = ffmpeg_args(&p, Path::new("/x/1.txt"), Path::new("/x/1.part.mp4")).join(" ");
        assert!(args.contains("-f concat -safe 0 -i /x/1.txt -ss "), "{args}");
        assert!(args.contains("scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:-1:-1"));
        assert!(args.contains("-c:v libx264 -preset veryfast -crf 21"));
        assert!(args.ends_with("-progress pipe:1 /x/1.part.mp4"));
        assert_eq!(parse_progress("out_time_us=1500000"), Some(1.5));
        assert_eq!(parse_progress("out_time_us=N/A"), None);
        assert_eq!(parse_progress("frame=3"), None);
    }

    #[test]
    fn download_names() {
        assert_eq!(
            file_name("bridges cam/1", 1_791_024_466_000),
            "bridges_cam_1_2026-10-03_104746Z.mp4"
        );
    }
}
