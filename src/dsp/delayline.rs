//! Power-of-two ring-buffer delay line with fractional (linear-interpolated) reads.
//!
//! Fractional reads are what let a delay-time knob move without stepping: the read
//! position is continuous, so changing `time` sweeps the pitch instead of clicking.

use crate::dsp::next_pow2;

#[derive(Clone)]
pub struct DelayLine {
    buf: Vec<f32>,
    mask: usize,
    w: usize,
}

impl DelayLine {
    /// Allocates for a maximum delay of `max_frames` (rounded up to a power of two).
    /// Called at construction time only — never on the audio thread's hot path.
    pub fn new(max_frames: usize) -> DelayLine {
        let cap = next_pow2(max_frames + 2);
        DelayLine {
            buf: vec![0.0; cap],
            mask: cap - 1,
            w: 0,
        }
    }

    pub const fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn reset(&mut self) {
        for s in self.buf.iter_mut() {
            *s = 0.0;
        }
        self.w = 0;
    }

    #[inline]
    pub fn write(&mut self, x: f32) {
        self.buf[self.w & self.mask] = x;
        self.w = self.w.wrapping_add(1);
    }

    /// Read `frames` samples in the past, linearly interpolated. Values beyond the
    /// buffer (or negative) are clamped, so a bad parameter cannot read out of bounds.
    ///
    /// Call it *before* this frame's [`write`](Self::write) for `frames` to mean exactly
    /// that many frames of delay — the ordering [`process`](Self::process) uses, and the
    /// ordering every effect in this crate follows.
    #[inline]
    pub fn read(&self, frames: f32) -> f32 {
        let max = (self.buf.len() - 2) as f32;
        let d = if frames.is_finite() {
            frames.clamp(0.0, max)
        } else {
            0.0
        };
        let base = self.w as f64 - d as f64;
        let i0 = base.floor();
        let frac = (base - i0) as f32;
        let len = self.buf.len() as i64;
        let a = (i0 as i64).rem_euclid(len) as usize;
        let b = (i0 as i64 + 1).rem_euclid(len) as usize;
        let (sa, sb) = (self.buf[a], self.buf[b]);
        sa + (sb - sa) * frac
    }

    /// Write and then read, which is the feedback-loop shape delay/reverb use.
    #[inline]
    pub fn process(&mut self, x: f32, frames: f32) -> f32 {
        let y = self.read(frames);
        self.write(x);
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{peak, sine};

    #[test]
    fn integer_delay_is_exact_and_lands_on_the_right_sample() {
        let mut dl = DelayLine::new(1024);
        let mut out = vec![0.0f32; 64];
        for i in 0..64 {
            let x = if i == 4 { 1.0 } else { 0.0 };
            out[i] = dl.read(8.0);
            dl.write(x);
        }
        assert_eq!(peak(&out[..12]), 0.0, "nothing may appear before 8 samples");
        assert!(
            (out[12] - 1.0).abs() < 1e-6,
            "impulse must land 8 samples later"
        );
        assert_eq!(peak(&out[13..]), 0.0);
    }

    #[test]
    fn fractional_delay_interpolates_between_samples() {
        let mut dl = DelayLine::new(64);
        // Ramp 0,1,2,3... read at 1.5 samples back -> midway between n-1 and n.
        let mut y = [0.0f32; 8];
        for i in 0..8 {
            y[i] = dl.read(1.5);
            dl.write(i as f32);
        }
        assert!((y[3] - 1.5).abs() < 1e-5, "got {}", y[3]);
        assert!((y[6] - 4.5).abs() < 1e-5);
        // whole and half steps differ, i.e. the read really is continuous
        assert!((dl.read(2.0) - dl.read(2.5)).abs() > 0.1);
    }

    #[test]
    fn wraps_correctly_across_the_buffer_end() {
        let mut dl = DelayLine::new(16); // 32 frames after pow2 rounding
        let cap = dl.capacity();
        let x = sine(4096, 100.0, 48000.0, 0.5);
        let mut worst = 0.0f32;
        for (i, s) in x.iter().enumerate() {
            let want = x[i.saturating_sub(cap / 2)];
            let got = dl.read(cap as f32 / 2.0);
            dl.write(*s);
            worst = worst.max((got - want).abs());
        }
        assert!(worst < 1e-6, "wrap-around error {worst}");
    }

    #[test]
    fn out_of_range_reads_are_clamped_not_panicky() {
        let mut dl = DelayLine::new(32);
        dl.write(0.5);
        assert!(dl.read(-100.0).is_finite());
        assert!(dl.read(1e9).is_finite());
        assert!(dl.read(f32::NAN).is_finite());
    }
}
