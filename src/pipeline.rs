//! Command lines for the `yt-dlp -o - | ffmpeg` pipeline, plus parsing of what ffmpeg reports back.

use std::path::{Path, PathBuf};

use crate::settings::EncodeSettings;

/// Paths of the external programs. yt-dlp often lives outside PATH, so all three are configurable.
#[derive(Debug, Clone)]
pub struct Tools {
    pub yt_dlp: PathBuf,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

/// yt-dlp `--match-filters` expression that lets live and upcoming streams through and refuses
/// finished ones. `?` keeps extractors that don't report `live_status` at all.
pub const LIVE_FILTER: &str = "live_status!=?was_live & live_status!=?not_live & live_status!=?post_live";

/// yt-dlp stderr fragments meaning "nothing live here right now" rather than a failure.
pub const OFFLINE_MARKERS: &[&str] = &["does not pass filter", "not currently live", "is not live"];

pub fn fetch_args(url: &str, settings: &EncodeSettings, live_only: bool) -> Vec<String> {
    let h = settings.height;
    let mut args: Vec<String> = vec![
        "--no-update".into(),
        "--no-progress".into(),
        "--no-playlist".into(),
        "--wait-for-video".into(),
        "5-60".into(),
        // yt-dlp hands live HLS to its own ffmpeg, which otherwise logs a progress line and an
        // "Opening https://..." line per chunk: tens of megabytes per stream per day.
        "--downloader-args".into(),
        "ffmpeg:-nostats -loglevel warning".into(),
        "-f".into(),
        // Fall back to the smallest stream if nothing fits under the height; ffmpeg scales anyway.
        format!("bv*[height<={h}]/b[height<={h}]/wv*/w"),
    ];
    if live_only {
        args.extend(["--match-filters".into(), LIVE_FILTER.into()]);
    }
    args.extend(["-o".into(), "-".into(), "--".into(), url.into()]);
    args
}

/// ffmpeg writes one `.ts` per segment into `dir` and prints a CSV line on stdout as each one is
/// finalized. Timestamps stay continuous across a session's segments (no `-reset_timestamps`), so
/// a player only needs a discontinuity between sessions.
pub fn encode_args(settings: &EncodeSettings, dir: &Path) -> Vec<String> {
    let s = settings;
    vec![
        "-hide_banner".into(),
        "-nostats".into(),
        "-loglevel".into(),
        "warning".into(),
        "-i".into(),
        "pipe:0".into(),
        "-vf".into(),
        s.video_filter(),
        "-r".into(),
        s.out_fps.to_string(),
        "-an".into(),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        s.preset.clone(),
        "-crf".into(),
        s.crf.to_string(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        // -g alone leaves the first GOP to x264's discretion, which makes the opening segment run
        // long; force_key_frames pins every cut point.
        "-g".into(),
        (s.out_fps * s.keyframe_seconds).to_string(),
        "-force_key_frames".into(),
        format!("expr:gte(t,n_forced*{})", s.keyframe_seconds),
        "-f".into(),
        "segment".into(),
        "-segment_format".into(),
        "mpegts".into(),
        // Aim half a second early: the stream's first frame rarely sits exactly at t=0, and a
        // forced keyframe landing a hair past the target gets skipped, doubling the first segment.
        "-segment_time".into(),
        format!("{}", s.segment_seconds() as f64 - 0.5),
        "-segment_list".into(),
        "pipe:1".into(),
        "-segment_list_type".into(),
        "csv".into(),
        dir.join("%06d.ts").to_string_lossy().into_owned(),
    ]
}

/// A segment ffmpeg has finished writing, as reported on its segment list.
#[derive(Debug, Clone, PartialEq)]
pub struct FinishedSegment {
    pub file: String,
    pub media_start: f64,
    pub media_end: f64,
}

/// Parse one `filename,start,end` line from the CSV segment list.
pub fn parse_segment_line(line: &str) -> Option<FinishedSegment> {
    let mut parts = line.trim().rsplitn(3, ',');
    let media_end = parts.next()?.parse().ok()?;
    let media_start = parts.next()?.parse().ok()?;
    let file = parts.next()?.trim_matches('"').to_string();
    (!file.is_empty()).then_some(FinishedSegment {
        file,
        media_start,
        media_end,
    })
}

/// Sequence number from a `%06d.ts` filename.
pub fn seq_from_file(file: &str) -> Option<i64> {
    file.strip_suffix(".ts")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_args_end_with_url_after_separator() {
        let args = fetch_args("https://example.com/-x", &EncodeSettings::default(), true);
        assert_eq!(&args[args.len() - 2..], ["--", "https://example.com/-x"]);
        assert!(args.contains(&LIVE_FILTER.to_string()));
        assert!(args.contains(&"bv*[height<=1080]/b[height<=1080]/wv*/w".to_string()));

        let args = fetch_args("https://example.com", &EncodeSettings::default(), false);
        assert!(!args.contains(&"--match-filters".to_string()));
    }

    #[test]
    fn encode_args_shape() {
        let args = encode_args(&EncodeSettings::default(), Path::new("/data/streams/1/2"));
        let joined = args.join(" ");
        assert!(joined.contains("-segment_time 99.5"));
        assert!(joined.contains("-g 150"));
        assert!(joined.contains("-segment_list pipe:1 -segment_list_type csv"));
        assert!(!joined.contains("reset_timestamps"));
        assert_eq!(args.last().unwrap(), "/data/streams/1/2/%06d.ts");
    }

    #[test]
    fn parses_segment_lines() {
        assert_eq!(
            parse_segment_line("000003.ts,301.400000,401.400000\n"),
            Some(FinishedSegment {
                file: "000003.ts".into(),
                media_start: 301.4,
                media_end: 401.4
            })
        );
        assert_eq!(parse_segment_line("garbage"), None);
        assert_eq!(parse_segment_line(",1,2"), None);
        assert_eq!(seq_from_file("000003.ts"), Some(3));
        assert_eq!(seq_from_file("capture.log"), None);
    }
}
