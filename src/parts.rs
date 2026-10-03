//! Keyframe index of a segment, so a playlist can list it as byte ranges a few seconds long
//! instead of one 50 MB file: a player then fetches only the part it needs to show a frame.
//!
//! ffmpeg's MPEG-TS muxer writes the stream tables (SDT, PAT, PMT) right before every video
//! keyframe, so a range starting at those tables is decodable on its own.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

const PACKET: u64 = 188;
const PID_PAT: u16 = 0x0000;
const PID_SDT: u16 = 0x0011;
/// How far back from a keyframe to look for its tables.
const LOOK_BACK: u64 = 8;

/// A playable piece of a segment: where it starts in the file and in the segment's video.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Part {
    pub offset: u64,
    pub time: f64,
}

/// A keyframe as ffprobe reports it: byte position of its first TS packet and its timestamp.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Keyframe {
    pub pos: u64,
    pub pts: f64,
}

/// Parts from keyframes, given the PID of the TS packet at a byte offset. Each part starts at the
/// PAT before its keyframe (or the SDT right before that PAT). Errors if any keyframe lacks its
/// tables, in which case the segment is better served whole.
pub fn parts_from(keyframes: &[Keyframe], mut pid_at: impl FnMut(u64) -> Option<u16>) -> Result<Vec<Part>> {
    let Some(first) = keyframes.first() else {
        bail!("no keyframes");
    };
    let mut parts = Vec::with_capacity(keyframes.len());
    for kf in keyframes {
        if kf.pos % PACKET != 0 {
            bail!("keyframe at byte {} is not on a packet boundary", kf.pos);
        }
        let pat = (1..=LOOK_BACK)
            .filter_map(|k| kf.pos.checked_sub(k * PACKET))
            .find(|&at| pid_at(at) == Some(PID_PAT))
            .with_context(|| format!("no PAT before the keyframe at byte {}", kf.pos))?;
        let start = match pat.checked_sub(PACKET) {
            Some(sdt) if pid_at(sdt) == Some(PID_SDT) => sdt,
            _ => pat,
        };
        parts.push(Part {
            offset: start,
            time: kf.pts - first.pts,
        });
    }
    // Whatever precedes the first keyframe belongs to the first part.
    parts[0].offset = 0;
    Ok(parts)
}

/// Video keyframes of a TS file, from ffprobe.
pub async fn keyframes(ffprobe: &Path, file: &Path) -> Result<Vec<Keyframe>> {
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pos,pts_time,flags",
        ])
        .args(["-of", "csv=p=0"])
        .arg(file)
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("running {}", ffprobe.display()))?;
    if !output.status.success() {
        bail!("ffprobe failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            // pts_time,pos,flags
            let mut fields = line.split(',');
            let pts = fields.next()?.parse().ok()?;
            let pos = fields.next()?.parse().ok()?;
            fields.next()?.starts_with('K').then_some(Keyframe { pos, pts })
        })
        .collect())
}

/// Index a segment file.
pub async fn index(ffprobe: &Path, file: &Path) -> Result<Vec<Part>> {
    let keyframes = keyframes(ffprobe, file).await?;
    let path = file.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut handle = std::fs::File::open(&path)?;
        parts_from(&keyframes, |offset| pid_in(&mut handle, offset))
    })
    .await?
}

/// PID of the TS packet at `offset`, if there is a packet there.
fn pid_in(file: &mut std::fs::File, offset: u64) -> Option<u16> {
    let mut header = [0u8; 3];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut header).ok()?;
    (header[0] == 0x47).then(|| (u16::from(header[1] & 0x1f) << 8) | u16::from(header[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file layout: SDT, PAT, PMT, then video, repeated before each keyframe.
    fn layout(keyframe_packets: &[u64]) -> impl FnMut(u64) -> Option<u16> {
        let keyframe_packets = keyframe_packets.to_vec();
        move |offset| {
            let packet = offset / PACKET;
            keyframe_packets
                .iter()
                .find_map(|&k| match k.checked_sub(packet) {
                    Some(3) => Some(PID_SDT),
                    Some(2) => Some(PID_PAT),
                    Some(1) => Some(0x1000),
                    _ => None,
                })
                .or(Some(0x100))
        }
    }

    #[test]
    fn parts_start_at_the_tables_before_each_keyframe() {
        let kfs = [
            Keyframe {
                pos: 3 * PACKET,
                pts: 101.4,
            },
            Keyframe {
                pos: 1000 * PACKET,
                pts: 106.4,
            },
            Keyframe {
                pos: 2500 * PACKET,
                pts: 111.4,
            },
        ];
        let parts = parts_from(&kfs, layout(&[3, 1000, 2500])).unwrap();
        assert_eq!(
            parts.iter().map(|p| p.offset).collect::<Vec<_>>(),
            [0, 997 * PACKET, 2497 * PACKET]
        );
        let times: Vec<f64> = parts.iter().map(|p| (p.time * 1000.0).round() / 1000.0).collect();
        assert_eq!(times, [0.0, 5.0, 10.0]);
    }

    #[test]
    fn a_keyframe_without_tables_means_no_parts() {
        let kfs = [
            Keyframe {
                pos: 3 * PACKET,
                pts: 0.0,
            },
            Keyframe {
                pos: 900 * PACKET,
                pts: 5.0,
            },
        ];
        assert!(parts_from(&kfs, layout(&[3])).is_err());
        assert!(parts_from(&[], layout(&[])).is_err());
        let unaligned = [Keyframe {
            pos: 3 * PACKET + 4,
            pts: 0.0,
        }];
        assert!(parts_from(&unaligned, layout(&[3])).is_err());
    }
}
