//! Per-stream capture settings and the arithmetic that turns them into ffmpeg parameters.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// x264 presets. Settings are typed and checked against this list so nothing user-supplied
/// ever reaches a command line as free text.
pub const X264_PRESETS: &[&str] = &[
    "ultrafast",
    "superfast",
    "veryfast",
    "faster",
    "fast",
    "medium",
    "slow",
    "slower",
    "veryslow",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EncodeSettings {
    /// Frames kept per second of *stream*.
    pub sample_fps: f64,
    /// Frame rate the stream is published at.
    pub source_fps: u32,
    /// Playback rate of the timelapse.
    pub out_fps: u32,
    pub height: u32,
    pub crf: u8,
    pub preset: String,
    /// Minutes *of stream* per segment file.
    pub segment_minutes: f64,
    /// Keyframe spacing in output seconds; segments can only split here.
    pub keyframe_seconds: u32,
}

impl Default for EncodeSettings {
    fn default() -> Self {
        Self {
            sample_fps: 5.0,
            source_fps: 30,
            out_fps: 30,
            height: 1080,
            crf: 21,
            preset: "veryfast".into(),
            segment_minutes: 10.0,
            keyframe_seconds: 5,
        }
    }
}

impl EncodeSettings {
    pub fn validate(&self) -> Result<()> {
        if !(self.sample_fps > 0.0 && self.sample_fps <= self.source_fps as f64) {
            bail!(
                "sample-fps must be above 0 and at most source-fps ({})",
                self.source_fps
            );
        }
        let step = self.source_fps as f64 / self.sample_fps;
        if (step - step.round()).abs() > 1e-6 {
            bail!(
                "source-fps ({}) must be a whole multiple of sample-fps ({}); got a step of {step:.3}",
                self.source_fps,
                self.sample_fps
            );
        }
        if !(1..=240).contains(&self.source_fps) {
            bail!("source-fps must be between 1 and 240");
        }
        if !(1..=120).contains(&self.out_fps) {
            bail!("out-fps must be between 1 and 120");
        }
        if !(144..=4320).contains(&self.height) || !self.height.is_multiple_of(2) {
            bail!("height must be an even number between 144 and 4320");
        }
        if self.crf > 51 {
            bail!("crf must be between 0 and 51");
        }
        if !X264_PRESETS.contains(&self.preset.as_str()) {
            bail!("preset must be one of {}", X264_PRESETS.join(", "));
        }
        if !(self.segment_minutes > 0.0 && self.segment_minutes <= 24.0 * 60.0) {
            bail!("segment-minutes must be above 0 and at most a day");
        }
        if !(1..=60).contains(&self.keyframe_seconds) {
            bail!("keyframe-seconds must be between 1 and 60");
        }
        Ok(())
    }

    /// Seconds of stream per second of timelapse.
    pub fn speedup(&self) -> f64 {
        self.out_fps as f64 / self.sample_fps
    }

    /// Keep every Nth decoded frame. Only meaningful once `validate` has passed.
    pub fn frame_step(&self) -> u32 {
        (self.source_fps as f64 / self.sample_fps).round() as u32
    }

    /// Fixed output size, so segments stay concat-compatible across a resolution change.
    pub fn canvas(&self) -> (u32, u32) {
        let width = (self.height as f64 * 16.0 / 9.0).round() as u32;
        (width - width % 2, self.height)
    }

    /// ffmpeg counts segment length in *output* seconds, which are sped up, and it can only cut
    /// on a keyframe, so round to a whole number of keyframe intervals or segments silently come
    /// out longer than asked for.
    pub fn segment_seconds(&self) -> u32 {
        let wanted = self.segment_minutes * 60.0 / self.speedup();
        let step = self.keyframe_seconds as f64;
        ((wanted / step).round() * step).max(step) as u32
    }

    pub fn video_filter(&self) -> String {
        let (width, height) = self.canvas();
        format!(
            // Sample by frame index, not timestamp: live HLS timestamps are jittery enough that
            // fps=N samples unevenly and the result stutters.
            "select='not(mod(n,{step}))',setpts=N/{out}/TB,\
             scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:-1:-1",
            step = self.frame_step(),
            out = self.out_fps,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let s = EncodeSettings::default();
        s.validate().unwrap();
        assert_eq!(s.speedup(), 6.0);
        assert_eq!(s.frame_step(), 6);
        assert_eq!(s.canvas(), (1920, 1080));
        // 10 min of stream at 6x = 100 s, already a multiple of 5.
        assert_eq!(s.segment_seconds(), 100);
    }

    #[test]
    fn segment_seconds_rounds_to_keyframes() {
        let s = EncodeSettings {
            segment_minutes: 7.0,
            ..Default::default()
        };
        // 70 s wanted, rounds to 70.
        assert_eq!(s.segment_seconds(), 70);
        let s = EncodeSettings {
            segment_minutes: 0.01,
            ..Default::default()
        };
        assert_eq!(s.segment_seconds(), 5, "never shorter than one keyframe interval");
    }

    #[test]
    fn rejects_bad_settings() {
        let bad = [
            EncodeSettings {
                sample_fps: 7.0,
                ..Default::default()
            },
            EncodeSettings {
                sample_fps: 0.0,
                ..Default::default()
            },
            EncodeSettings {
                height: 721,
                ..Default::default()
            },
            EncodeSettings {
                preset: "fast; rm -rf /".into(),
                ..Default::default()
            },
            EncodeSettings {
                crf: 60,
                ..Default::default()
            },
            EncodeSettings {
                segment_minutes: 0.0,
                ..Default::default()
            },
        ];
        for s in bad {
            assert!(s.validate().is_err(), "{s:?} should be rejected");
        }
    }

    #[test]
    fn canvas_is_even() {
        let s = EncodeSettings {
            height: 362,
            ..Default::default()
        };
        let (w, h) = s.canvas();
        assert_eq!((w % 2, h % 2), (0, 0));
    }

    #[test]
    fn partial_json_takes_defaults() {
        let s: EncodeSettings = serde_json::from_str(r#"{"sample_fps": 1}"#).unwrap();
        assert_eq!(s.sample_fps, 1.0);
        assert_eq!(s.out_fps, 30);
    }
}
