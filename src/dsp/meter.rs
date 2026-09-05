//! Running peak / RMS / clip detectors, published to the UI as atomics.
//!
//! These exist so the signal path is *visible* before it is trustworthy: an input
//! meter that moves is how you distinguish "mic permission denied" from "the amp is
//! broken" without a multimeter.

#[derive(Clone, Debug, Default)]
pub struct Meter {
    peak: f32,
    sumsq: f64,
    n: usize,
    clipped: bool,
}

impl Meter {
    pub fn new() -> Meter {
        Meter::default()
    }

    #[inline]
    pub fn push(&mut self, x: f32) {
        if x.is_finite() {
            let a = x.abs();
            if a > self.peak {
                self.peak = a;
            }
            self.sumsq += (x as f64) * (x as f64);
            if a >= 0.999 {
                self.clipped = true;
            }
        }
        self.n += 1;
    }

    #[inline]
    pub fn push_frame(&mut self, l: f32, r: f32) {
        self.push(l);
        self.push(r);
    }

    /// (peak, rms, clipped) for the window, then restart it.
    pub fn take(&mut self) -> (f32, f32, bool) {
        let rms = if self.n > 0 {
            ((self.sumsq / self.n as f64) as f32).sqrt()
        } else {
            0.0
        };
        let out = (self.peak, rms, self.clipped);
        self.peak = 0.0;
        self.sumsq = 0.0;
        self.n = 0;
        self.clipped = false;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::sine;

    const SR: f32 = 48000.0;

    #[test]
    fn reports_peak_rms_and_latches_clip() {
        let mut m = Meter::new();
        for s in sine(4800, 1000.0, SR, 0.5) {
            m.push(s);
        }
        let (p, r, clip) = m.take();
        assert!((p - 0.5).abs() < 1e-3, "peak {p}");
        assert!((r - 0.3536).abs() < 0.01, "sine rms {r}");
        assert!(!clip);

        m.push(1.2);
        let (_, _, clipped) = m.take();
        assert!(clipped, "a >0.999 sample must latch clip");

        // an empty window reports silence, never NaN
        let (p0, r0, c0) = m.take();
        assert_eq!((p0, r0, c0), (0.0, 0.0, false));
    }

    #[test]
    fn ignores_non_finite_samples() {
        let mut m = Meter::new();
        m.push(f32::NAN);
        m.push(f32::INFINITY);
        m.push_frame(f32::NEG_INFINITY, 0.25);
        let (p, r, _) = m.take();
        assert!((p - 0.25).abs() < 1e-6, "peak {p}");
        assert!(r.is_finite());
    }

    #[test]
    fn frame_and_scalar_paths_agree() {
        let mut a = Meter::new();
        let mut b = Meter::new();
        for i in 0..1000 {
            let l = ((i as f32) * 0.01).sin() * 0.4;
            let r = ((i as f32) * 0.013).cos() * 0.3;
            a.push_frame(l, r);
            b.push(l);
            b.push(r);
        }
        assert_eq!(a.take().0.to_bits(), b.take().0.to_bits());
    }
}
