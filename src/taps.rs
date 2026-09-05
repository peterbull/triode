//! Tap points along the signal chain, for measurement and for "where did my signal go?".
//!
//! A guitar signal that comes out silent can die at any of a dozen stages, and the output
//! alone cannot tell you which. This records a mono summary at every stage boundary so each
//! one can be measured (and written to its own WAV and listened to) independently.
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
    pub data: Vec<f32>,
}

pub struct TapLog {
    pub stages: Vec<TapStage>,
    /// Frames recorded per stage, shared by all of them.
    limit: usize,
    written: usize,
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
                })
                .collect(),
            limit,
            written: 0,
        }
    }

    /// How many frames each stage holds.
    pub fn len(&self) -> usize {
        self.written
    }

    pub fn is_empty(&self) -> bool {
        self.written == 0
    }

    pub fn stage_count(&self) -> usize {
        self.stages.len()
    }

    /// Append one frame's worth of a stage's output, summed to mono.
    ///
    /// Mono because the question this answers is "is there signal here and what did it do to
    /// the level", and a stereo stage costs twice the memory to answer it. Stages past the
    /// limit are dropped, which is the whole point of a bounded recorder.
    pub fn push(&mut self, stage: usize, frame: &Frame) {
        if self.written >= self.limit {
            return;
        }
        if let Some(s) = self.stages.get_mut(stage) {
            s.data.push(0.5 * (frame[0] + frame[1]));
        }
    }

    /// Append a single mono sample for a stage (the cab/limiter stages are summed already).
    pub fn push_mono(&mut self, stage: usize, v: f32) {
        if self.written >= self.limit {
            return;
        }
        if let Some(s) = self.stages.get_mut(stage) {
            s.data.push(v);
        }
    }

    /// Call once a frame, after all stages for that frame have been pushed.
    ///
    /// Saturates at the limit: `written` means "frames recorded", and it is also the gate
    /// `push` consults. Letting it run past the limit made `len()` disagree with the stage
    /// buffers (5 frames seen vs 3 stored), which is exactly the kind of off-by-N that would
    /// then misalign the per-stage WAVs against each other.
    pub fn advance(&mut self) {
        if self.written < self.limit {
            self.written += 1;
        }
    }

    /// Write each stage as a stereo WAV (both channels equal) so any player or `afinfo` can
    /// open it. Returns the paths written.
    pub fn write_wavs(&self, dir: &Path, sr: u32) -> std::io::Result<Vec<std::path::PathBuf>> {
        std::fs::create_dir_all(dir)?;
        let mut out = Vec::with_capacity(self.stages.len());
        for (i, s) in self.stages.iter().enumerate() {
            let frames: Vec<Frame> = s.data.iter().map(|v| [*v, *v]).collect();
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
        for i in 0..5 {
            let f = [0.25 * (i as f32 + 1.0), 0.25 * (i as f32 + 1.0)];
            for s in 0..3 {
                t.push(s, &f);
            }
            t.advance();
        }
        // Five frames were pushed but the limit is three: a runaway run cannot eat RAM.
        assert_eq!(t.len(), 3);
        assert_eq!(t.stage_count(), 3);
        assert_eq!(t.stages[1].data.len(), 3);
        // Mono sum of two equal halves is the signal itself, not double it.
        assert!((t.stages[2].data[0] - 0.25).abs() < 1e-6);
        // A stage index past the end is ignored rather than a panic in the audio path.
        t.push(99, &[1.0, 1.0]);
    }

    #[test]
    fn stage_names_are_made_safe_for_filenames() {
        assert_eq!(TapLog::slug("2.Overdrive"), "2_overdrive");
        assert_eq!(TapLog::slug("cab sim"), "cab_sim");
    }
}
