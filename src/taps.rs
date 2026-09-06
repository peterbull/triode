//! Tap points along the signal chain, for measurement and for "where did my signal go?".
//!
//! A guitar signal that comes out silent can die at any of a dozen stages, and the output
//! alone cannot tell you which. This records every stage boundary so each one can be measured
//! (and written to its own WAV and listened to) independently.
//!
//! ## Real-time shape
//!
//! All buffers are reserved in [`TapLog::new`], i.e. on whichever thread switched taps on,
//! so `push` never allocates: it writes into space that already exists and stops writing
//! once `limit` frames are in. Taps are therefore an offline / bring-up tool rather than
//! something left running in a live set -- an hour of a 9-stage chain at 96 kHz is 250 MB of
//! RAM, which is not a thing the audio thread should be quietly deciding to do.

use std::path::Path;

use crate::dsp::Frame;

/// The stage order. Kept in one place so the engine's hook indices and the names people see
/// in a trace report cannot drift apart.
#[derive(Debug)]
pub struct TapStage {
    pub name: String,
    /// Left channel, retained under its original name for callers that read mono taps.
    pub data: Vec<f32>,
    pub right: Vec<f32>,
}

pub struct TapLog {
    pub stages: Vec<TapStage>,
    /// Frames recorded per stage.
    limit: usize,
}

impl TapLog {
    /// Reserve `limit` frames for each of `names`. Allocation happens here and only here.
    pub fn new(names: &[&str], limit: usize) -> TapLog {
        TapLog {
            stages: names
                .iter()
                .map(|n| TapStage {
                    name: (*n).to_string(),
                    data: Vec::with_capacity(limit),
                    right: Vec::with_capacity(limit),
                })
                .collect(),
            limit,
        }
    }

    /// How many frames each stage holds.
    pub fn len(&self) -> usize {
        self.stages.first().map_or(0, |stage| stage.data.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn stage_count(&self) -> usize {
        self.stages.len()
    }

    /// Append one frame's worth of a stage's output without collapsing stereo.
    ///
    /// Stages past the limit are dropped, which is the whole point of a bounded recorder.
    pub fn push(&mut self, stage: usize, frame: &Frame) {
        if let Some(s) = self.stages.get_mut(stage) {
            if s.data.len() < self.limit {
                s.data.push(frame[0]);
                s.right.push(frame[1]);
            }
        }
    }

    /// Append a single mono sample for a stage (the cab/limiter stages are summed already).
    pub fn push_mono(&mut self, stage: usize, v: f32) {
        if let Some(s) = self.stages.get_mut(stage) {
            if s.data.len() < self.limit {
                s.data.push(v);
                s.right.push(v);
            }
        }
    }

    /// Write each stage as its recorded stereo WAV. Returns the paths written.
    pub fn write_wavs(&self, dir: &Path, sr: u32) -> std::io::Result<Vec<std::path::PathBuf>> {
        std::fs::create_dir_all(dir)?;
        let mut out = Vec::with_capacity(self.stages.len());
        for (i, s) in self.stages.iter().enumerate() {
            let frames: Vec<Frame> = s
                .data
                .iter()
                .zip(&s.right)
                .map(|(&left, &right)| [left, right])
                .collect();
            let path = dir.join(format!("{i:02}-{}.wav", Self::slug(&s.name)));
            crate::render::write_wav_stereo(&path, &frames, sr)
                .map_err(|e| std::io::Error::other(format!("{}: {e}", path.display())))?;
            out.push(path);
        }
        Ok(out)
    }

    /// Stage names carry `/` and spaces; WAV filenames should not.
    fn slug(name: &str) -> String {
        name.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tap_records_every_stage_and_stops_at_its_limit() {
        let mut t = TapLog::new(&["in", "amp", "out"], 3);
        // The engine records one whole block per stage, not one stage per frame.
        for s in 0..3 {
            for i in 0..5 {
                let f = [0.25 * (i as f32 + 1.0), 0.25 * (i as f32 + 1.0)];
                t.push(s, &f);
            }
        }
        // Five frames were pushed but the limit is three: a runaway run cannot eat RAM.
        assert_eq!(t.len(), 3);
        assert_eq!(t.stage_count(), 3);
        assert_eq!(t.stages[1].data.len(), 3);
        assert_eq!(t.stages[1].right.len(), 3);
        assert!((t.stages[2].data[0] - 0.25).abs() < 1e-6);
        assert!((t.stages[2].right[0] - 0.25).abs() < 1e-6);
        // A stage index past the end is ignored rather than a panic in the audio path.
        t.push(99, &[1.0, 1.0]);
    }

    #[test]
    fn anti_phase_stereo_is_not_recorded_as_silence() {
        let mut t = TapLog::new(&["stereo"], 1);
        t.push(0, &[0.5, -0.5]);

        assert_eq!(t.stages[0].data, vec![0.5]);
        assert_eq!(t.stages[0].right, vec![-0.5]);
    }

    #[test]
    fn stage_wavs_keep_left_and_right_distinct() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("triode-taps-{}", std::process::id()));
        let mut t = TapLog::new(&["stereo"], 1);
        t.push(0, &[0.5, -0.5]);
        let paths = t.write_wavs(&dir, 48_000)?;
        let samples: Vec<f32> = hound::WavReader::open(&paths[0])
            .and_then(|mut reader| reader.samples::<f32>().collect())?;
        assert_eq!(samples, vec![0.5, -0.5]);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn stage_names_are_made_safe_for_filenames() {
        assert_eq!(TapLog::slug("2.Overdrive"), "2_overdrive");
        assert_eq!(TapLog::slug("cab sim"), "cab_sim");
    }
}
