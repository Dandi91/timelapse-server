//! HLS playlists built from segment rows.
//!
//! The playlist is served from `/streams/<id>/playlist.m3u8` and segments from
//! `/streams/<id>/<session>/<file>.ts`, so every URI in it is relative and the whole thing works
//! behind a reverse proxy under any prefix.

use std::fmt::Write as _;

use crate::db::Segment;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// A fixed range: the player can seek anywhere in it.
    Vod,
    /// Grows as segments finish; the player reloads it.
    Event,
}

/// `segments` must be in playback order (as `segments_in_range` returns them). A discontinuity
/// separates sessions, whose timestamps and encode settings are unrelated. `min_target` raises the
/// target duration for live playlists, whose next segment may be longer than any listed so far.
pub fn build(segments: &[Segment], kind: Kind, min_target: f64) -> String {
    let longest = segments.iter().map(|s| s.media_dur).fold(min_target, f64::max);
    let mut out = String::new();
    out.push_str("#EXTM3U\n#EXT-X-VERSION:3\n");
    let _ = writeln!(out, "#EXT-X-TARGETDURATION:{}", longest.ceil().max(1.0) as u64);
    out.push_str("#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    out.push_str(match kind {
        Kind::Vod => "#EXT-X-PLAYLIST-TYPE:VOD\n",
        Kind::Event => "#EXT-X-PLAYLIST-TYPE:EVENT\n",
    });

    let mut session = None;
    for seg in segments {
        if session.is_some_and(|s| s != seg.session_id) {
            out.push_str("#EXT-X-DISCONTINUITY\n");
        }
        session = Some(seg.session_id);
        // Microsecond precision: hls.js lays out sessions by these durations, and rounding to
        // milliseconds would drift by up to half a second of video over a week of segments.
        let _ = writeln!(out, "#EXTINF:{:.6},\n{}", seg.media_dur, uri(seg));
    }
    if kind == Kind::Vod {
        out.push_str("#EXT-X-ENDLIST\n");
    }
    out
}

/// `streams/<id>/<session>/<file>` relative to the playlist's `streams/<id>/` directory.
fn uri(seg: &Segment) -> &str {
    let prefix = format!("streams/{}/", seg.stream_id);
    seg.path.strip_prefix(&prefix).unwrap_or(&seg.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(session_id: i64, seq: i64, media_dur: f64) -> Segment {
        Segment {
            id: seq,
            session_id,
            stream_id: 3,
            seq,
            path: format!("streams/3/{session_id}/{seq:06}.ts"),
            wall_start: 0,
            wall_end: 0,
            media_start: None,
            media_end: None,
            media_dur,
            bytes: 0,
            state: "ready".into(),
            thumbs: None,
            thumb_interval: None,
        }
    }

    #[test]
    fn vod_with_session_break() {
        let playlist = build(&[seg(7, 0, 100.0), seg(7, 1, 99.96), seg(9, 0, 41.5)], Kind::Vod, 0.0);
        assert_eq!(
            playlist,
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:100\n#EXT-X-MEDIA-SEQUENCE:0\n\
             #EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-PLAYLIST-TYPE:VOD\n\
             #EXTINF:100.000000,\n7/000000.ts\n#EXTINF:99.960000,\n7/000001.ts\n\
             #EXT-X-DISCONTINUITY\n#EXTINF:41.500000,\n9/000000.ts\n#EXT-X-ENDLIST\n"
        );
    }

    #[test]
    fn event_stays_open_and_covers_the_next_segment() {
        let playlist = build(&[seg(1, 0, 4.2)], Kind::Event, 100.0);
        assert!(playlist.contains("#EXT-X-TARGETDURATION:100\n"));
        assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:EVENT\n"));
        assert!(!playlist.contains("ENDLIST"));
    }

    #[test]
    fn target_duration_rounds_up() {
        assert!(build(&[seg(1, 0, 100.4)], Kind::Vod, 0.0).contains("#EXT-X-TARGETDURATION:101\n"));
    }
}
