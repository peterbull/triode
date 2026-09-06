//! Input resampling with a drift-correcting ratio.
//!
//! A microphone and a pair of headphones are two CoreAudio devices on two clocks that
//! are not phase-locked. Without correction the input ring slowly fills (input faster)
//! or empties (output faster) and the amp either grows latently laggy or clicks. The
//! engine nudges this resampler's ratio by up to ±0.1 % to hold the ring centred, which
//! is far below the threshold of audible pitch change on a guitar but enough to stop the
//! drift.

/// Where mono input frames come from. Implemented by the SPSC ring in the live app and
/// by a slice in the offline renderer, so the engine is identical either way.
pub trait Source: Send {
    /// Read up to `dst.len()` frames; return how many were read (0 = starved).
    fn read(&mut self, dst: &mut [f32]) -> usize;
    /// Frames buffered and waiting (0 when the source has no queue).
    fn level(&self) -> usize;
    /// Live asynchronous FIFO occupancy, not the remaining length of a file.
    fn clock_level(&self) -> Option<usize> {
        None
    }
}

/// A source that just hands back a fixed slice, for `--render`.
pub struct SliceSource<'a> {
    data: &'a [f32],
    pos: usize,
}

impl<'a> SliceSource<'a> {
    pub fn new(data: &'a [f32]) -> SliceSource<'a> {
        SliceSource { data, pos: 0 }
    }

    /// Input samples handed out so far — how an offline render knows when the file is
    /// fully consumed.
    pub fn consumed(&self) -> usize {
        self.pos
    }
}

impl Source for SliceSource<'_> {
    fn read(&mut self, dst: &mut [f32]) -> usize {
        let n = (self.data.len() - self.pos).min(dst.len());
        dst[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        n
    }
    fn level(&self) -> usize {
        // The engine treats a zero level as "the input died" (dead microphone warning,
        // starvation stats). Reporting a constant 0 here used to make every offline render
        // claim its entire length was starved, because this is the source it reads from.
        self.data.len() - self.pos
    }
}

/// Linear-interpolating step resampler.
///
/// `ponytail:` linear interpolation, deliberately. At the ratios this app sees (44.1/48
/// and a ±0.1 % drift correction) the imaging cost is a few dB of HF roll-off on the way
/// *into* a distortion stage, which is inaudible under gain. A proper windowed-sinc
/// polyphase resampler becomes the right answer if a device ever forces a ratio beyond
/// about 1.5× (a Bluetooth headset at 8/16 kHz) — that case is surfaced as a UI warning
/// instead of being silently mangled here.
pub struct Resampler {
    buf: [f32; 512],
    len: usize,
    pos: usize,
    x0: f32,
    x1: f32,
    frac: f32,
    /// Input frames consumed per output frame.
    step: f32,
    underruns: u64,
    primed: bool,
}

impl Resampler {
    pub fn new() -> Resampler {
        Resampler {
            buf: [0.0; 512],
            len: 0,
            pos: 0,
            x0: 0.0,
            x1: 0.0,
            frac: 0.0,
            step: 1.0,
            underruns: 0,
            primed: false,
        }
    }

    /// Consume `step` input frames per output frame (`in_rate / out_rate`).
    pub fn set_step(&mut self, step: f32) {
        self.step = if step.is_finite() {
            step.clamp(0.05, 20.0)
        } else {
            1.0
        };
    }

    pub fn step(&self) -> f32 {
        self.step
    }

    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    /// Pull one input frame, refilling from `src`. `None` means starved.
    #[inline]
    fn pull(&mut self, src: &mut dyn Source) -> Option<f32> {
        if self.pos >= self.len {
            self.len = src.read(&mut self.buf);
            self.pos = 0;
            if self.len == 0 {
                return None;
            }
        }
        let s = self.buf[self.pos];
        self.pos += 1;
        Some(if s.is_finite() { s } else { 0.0 })
    }

    /// Next output frame.
    pub fn next(&mut self, src: &mut dyn Source) -> f32 {
        if !self.primed {
            // Prime both ends of the interpolation window so the first output is real
            // audio rather than a ramp out of silence.
            self.x0 = self.pull(src).unwrap_or(0.0);
            self.x1 = self.pull(src).unwrap_or(self.x0);
            self.primed = true;
        }
        while self.frac >= 1.0 {
            self.frac -= 1.0;
            self.x0 = self.x1;
            match self.pull(src) {
                Some(s) => self.x1 = s,
                None => {
                    self.x1 = 0.0;
                    self.underruns += 1;
                }
            }
        }
        let y = self.x0 + (self.x1 - self.x0) * self.frac;
        self.frac += self.step;
        if y.is_finite() {
            y
        } else {
            0.0
        }
    }

    pub fn reset(&mut self) {
        self.len = 0;
        self.pos = 0;
        self.x0 = 0.0;
        self.x1 = 0.0;
        self.frac = 0.0;
        self.primed = false;
    }
}

impl Default for Resampler {
    fn default() -> Self {
        Resampler::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{goertzel_mag, sine};

    const SR: f32 = 48000.0;

    #[test]
    fn unity_step_is_transparent() {
        let x: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.1).sin()).collect();
        let mut src = SliceSource::new(&x);
        let mut r = Resampler::new();
        r.set_step(1.0);
        // Two frames are consumed priming the window, so output == input from index 2.
        let y: Vec<f32> = (0..x.len()).map(|_| r.next(&mut src)).collect();
        for i in 3..y.len() - 1 {
            assert!(
                (y[i] - x[i]).abs() < 1e-6,
                "frame {i}: {} vs {}",
                y[i],
                x[i]
            );
        }
        // Asking for as many outputs as inputs, when two inputs went into priming the
        // window, legitimately runs the reader off the end exactly once.
        assert!(r.underruns() <= 1, "underruns {}", r.underruns());
    }

    #[test]
    fn upsampling_preserves_tone_and_frame_count() {
        // 44.1 kHz input, 48 kHz output: step = 0.9187.
        let x = sine(4410, 440.0, 44100.0, 0.5);
        let mut src = SliceSource::new(&x);
        let mut r = Resampler::new();
        r.set_step(44100.0 / 48000.0);
        let y: Vec<f32> = (0..4800).map(|_| r.next(&mut src)).collect();
        let mag = goertzel_mag(&y[512..], 440.0, SR);
        assert!(
            mag > 0.45 && mag < 0.55,
            "440 Hz should survive intact, measured {mag}"
        );
        // no spurious image at the difference frequency
        assert!(goertzel_mag(&y[512..], 3900.0, SR) < 0.01);
    }

    #[test]
    fn downsampling_does_not_run_out_of_input() {
        let x = sine(4800, 220.0, SR, 0.5);
        let mut src = SliceSource::new(&x);
        let mut r = Resampler::new();
        r.set_step(48000.0 / 44100.0);
        let y: Vec<f32> = (0..4000).map(|_| r.next(&mut src)).collect();
        assert!(goertzel_mag(&y[256..], 220.0, 44100.0) > 0.45);
    }

    #[test]
    fn starvation_counts_underruns_and_keeps_producing_finite_output() {
        let x = sine(100, 440.0, SR, 0.5);
        let mut src = SliceSource::new(&x);
        let mut r = Resampler::new();
        r.set_step(1.0);
        let y: Vec<f32> = (0..1000).map(|_| r.next(&mut src)).collect();
        assert!(y.iter().all(|s| s.is_finite()));
        assert!(
            r.underruns() > 800,
            "expected the source to run dry, got {}",
            r.underruns()
        );
    }

    #[test]
    fn short_reads_are_handled() {
        /// A source that hands back one frame at a time, like a tiny device block.
        struct Dribble(Vec<f32>, usize);
        impl Source for Dribble {
            fn read(&mut self, dst: &mut [f32]) -> usize {
                let n = 1usize.min(dst.len()).min(self.0.len() - self.1);
                dst[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
                self.1 += n;
                n
            }
            fn level(&self) -> usize {
                self.0.len() - self.1
            }
        }
        let x = sine(2000, 300.0, SR, 0.4);
        let mut src = Dribble(x.clone(), 0);
        let mut r = Resampler::new();
        r.set_step(1.0);
        let y: Vec<f32> = (0..2000).map(|_| r.next(&mut src)).collect();
        for i in 3..1990 {
            assert!((y[i] - x[i]).abs() < 1e-6, "dribbled frame {i} mismatch");
        }
        assert_eq!(src.level(), 0);
    }

    #[test]
    fn junk_step_and_junk_samples_are_survivable() {
        let mut r = Resampler::new();
        r.set_step(f32::NAN);
        assert_eq!(r.step(), 1.0);
        r.set_step(1e9);
        assert!(r.step() <= 20.0);
        struct Junk;
        impl Source for Junk {
            fn read(&mut self, dst: &mut [f32]) -> usize {
                dst.iter_mut().for_each(|s| *s = f32::NAN);
                dst.len()
            }
            fn level(&self) -> usize {
                0
            }
        }
        let mut src = Junk;
        for _ in 0..100 {
            assert_eq!(r.next(&mut src), 0.0, "NaN input must become silence");
        }
        r.reset();
        assert_eq!(r.next(&mut src), 0.0);
    }
}
