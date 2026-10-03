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

/// One playlist entry: a whole segment, or a byte range of one.
struct Entry<'a> {
    session: i64,
    duration: f64,
    /// `(length, offset)` in bytes.
    range: Option<(u64, u64)>,
    uri: &'a str,
}

/// A segment's entries: one per keyframe part when indexed, else the whole file.
fn entries(seg: &Segment) -> Vec<Entry<'_>> {
    let whole = || {
        vec![Entry {
            session: seg.session_id,
            duration: seg.media_dur,
            range: None,
            uri: uri(seg),
        }]
    };
    let Some(parts) = seg.parts.as_ref().map(|p| &p.0).filter(|p| !p.is_empty()) else {
        return whole();
    };
    let size = seg.bytes as u64;
    let mut out = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        let (end_offset, end_time) = match parts.get(i + 1) {
            Some(next) => (next.offset, next.time),
            None => (size, seg.media_dur),
        };
        if end_offset <= part.offset || end_time <= part.time {
            // An index that doesn't fit the file: serve it whole rather than wrongly.
            return whole();
        }
        out.push(Entry {
            session: seg.session_id,
            duration: end_time - part.time,
            range: Some((end_offset - part.offset, part.offset)),
            uri: uri(seg),
        });
    }
    out
}

/// `segments` must be in playback order (as `segments_in_range` returns them). A discontinuity
/// separates sessions, whose timestamps and encode settings are unrelated. Indexed segments are
/// listed as byte ranges a keyframe interval long, so a player fetches only what it shows.
///
/// A live (EVENT) playlist may only ever be appended to, so it stops at the first segment not yet
/// indexed (one just finished): listing it whole now and as ranges later would rewrite it.
/// `min_target` raises the target duration for live playlists, whose next entry may be a whole
/// segment if indexing it fails.
pub fn build(segments: &[Segment], kind: Kind, min_target: f64) -> String {
    let listed = match kind {
        Kind::Vod => segments.len(),
        Kind::Event => segments.iter().take_while(|s| s.parts.is_some()).count(),
    };
    let entries: Vec<Entry> = segments[..listed].iter().flat_map(entries).collect();
    let longest = entries.iter().map(|e| e.duration).fold(min_target, f64::max);
    let ranged = entries.iter().any(|e| e.range.is_some());

    let mut out = String::new();
    // Byte ranges need version 4.
    let _ = writeln!(out, "#EXTM3U\n#EXT-X-VERSION:{}", if ranged { 4 } else { 3 });
    let _ = writeln!(out, "#EXT-X-TARGETDURATION:{}", longest.ceil().max(1.0) as u64);
    out.push_str("#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    out.push_str(match kind {
        Kind::Vod => "#EXT-X-PLAYLIST-TYPE:VOD\n",
        Kind::Event => "#EXT-X-PLAYLIST-TYPE:EVENT\n",
    });

    let mut session = None;
    for entry in &entries {
        if session.is_some_and(|s| s != entry.session) {
            out.push_str("#EXT-X-DISCONTINUITY\n");
        }
        session = Some(entry.session);
        // Microsecond precision: hls.js lays out sessions by these durations, and rounding to
        // milliseconds would drift by up to half a second of video over a week of segments.
        let _ = writeln!(out, "#EXTINF:{:.6},", entry.duration);
        if let Some((length, offset)) = entry.range {
            let _ = writeln!(out, "#EXT-X-BYTERANGE:{length}@{offset}");
        }
        let _ = writeln!(out, "{}", entry.uri);
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
            parts: None,
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

    fn indexed(mut seg: Segment, bytes: i64, parts: &[(u64, f64)]) -> Segment {
        seg.bytes = bytes;
        seg.parts = Some(sqlx::types::Json(
            parts
                .iter()
                .map(|&(offset, time)| crate::parts::Part { offset, time })
                .collect(),
        ));
        seg
    }

    #[test]
    fn indexed_segments_become_byte_ranges() {
        let a = indexed(seg(7, 0, 12.0), 3000, &[(0, 0.0), (1000, 5.0), (2200, 10.0)]);
        let b = seg(7, 1, 12.0); // not indexed yet: whole
        let playlist = build(&[a, b], Kind::Vod, 0.0);
        assert_eq!(
            playlist,
            "#EXTM3U\n#EXT-X-VERSION:4\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:0\n\
             #EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-PLAYLIST-TYPE:VOD\n\
             #EXTINF:5.000000,\n#EXT-X-BYTERANGE:1000@0\n7/000000.ts\n\
             #EXTINF:5.000000,\n#EXT-X-BYTERANGE:1200@1000\n7/000000.ts\n\
             #EXTINF:2.000000,\n#EXT-X-BYTERANGE:800@2200\n7/000000.ts\n\
             #EXTINF:12.000000,\n7/000001.ts\n#EXT-X-ENDLIST\n"
        );
    }

    #[test]
    fn live_playlists_stop_before_unindexed_segments() {
        let a = indexed(seg(1, 0, 10.0), 2000, &[(0, 0.0), (900, 5.0)]);
        let b = seg(1, 1, 10.0);
        let c = indexed(seg(1, 2, 10.0), 2000, &[(0, 0.0)]);
        let playlist = build(&[a, b, c], Kind::Event, 100.0);
        assert_eq!(playlist.matches("#EXTINF").count(), 2, "{playlist}");
        assert!(!playlist.contains("000001.ts") && !playlist.contains("000002.ts"));
    }

    #[test]
    fn a_failed_index_is_served_whole() {
        let failed = indexed(seg(1, 0, 10.0), 2000, &[]);
        let nonsense = indexed(seg(1, 1, 10.0), 2000, &[(0, 0.0), (5000, 5.0)]);
        let playlist = build(&[failed, nonsense], Kind::Event, 0.0);
        assert!(!playlist.contains("BYTERANGE"), "{playlist}");
        assert_eq!(playlist.matches("#EXTINF").count(), 2);
    }

    #[test]
    fn target_duration_rounds_up() {
        assert!(build(&[seg(1, 0, 100.4)], Kind::Vod, 0.0).contains("#EXT-X-TARGETDURATION:101\n"));
    }
}
