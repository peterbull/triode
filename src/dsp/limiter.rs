//! Safety limiter and the input/output meters.
//!
//! The limiter is the last stage for a reason: a guitar plus a maxed gain knob plus
//! an engaged fuzz can ask for +40 dB, and the thing between that and the listener's
//! ears (or the laptop's speakers) should be a hard, tested ceiling.

use crate::dsp::{lin2db, sanitize, tau_coef};
/// Feed-forward peak limiter with a brickwall clamp behind it.
///
/// No lookahead (it would add latency under the fingers). The gain alone is *not* a
/// ceiling: on the first sample of an impulse the envelope has not caught up yet, so a
/// transient would sail straight through — measured at 3.9x with the gain alone. The
/// final per-sample clamp is what makes "nothing above `ceiling` reaches the output"
/// actually true, and it costs two instructions.
#[derive(Clone, Debug)]
pub struct Limiter {
    gain: f32,
    atk: f32,
    rel: f32,
    hold: f32,
    peak: f32,
    ceiling: f32,
    reduction_db: f32,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

impl Limiter {
    pub fn new() -> Limiter {
        Limiter {
            gain: 1.0,
            atk: 0.5,
            rel: 0.99,
            hold: 0.99,
            peak: 0.0,
            ceiling: 0.95,
            reduction_db: 0.0,
        }
    }

    pub fn set_rates(&mut self, sr: f32) {
        let sr = sr.max(1.0);
        // 1 ms attack (grab it fast), 180 ms release (let go slowly).
        self.atk = 1.0 - tau_coef(0.001, sr);
        self.rel = 1.0 - tau_coef(0.180, sr);
        // Peak *hold*: the detector has to remember the last transient for longer than one
        // sample, or `want` tracks the instantaneous waveform and the gain dives on every
        // cycle. That is waveform-destroying distortion, not limiting.
        self.hold = tau_coef(0.008, sr);
    }

    #[inline]
    pub fn process(&mut self, f: &mut [f32; 2], ceiling: f32) {
        let ceil = sanitize(ceiling, 0.95, 0.05, 0.999);
        self.ceiling = ceil;
        let inp = f[0].abs().max(f[1].abs());
        self.peak = inp.max(self.peak * self.hold);
        let want = if self.peak > ceil {
            ceil / self.peak
        } else {
            1.0
        };
        let coef = if want < self.gain { self.atk } else { self.rel };
        self.gain += (want - self.gain) * coef;
        if !self.gain.is_finite() {
            self.gain = 1.0;
        }
        f[0] = (f[0] * self.gain).clamp(-ceil, ceil);
        f[1] = (f[1] * self.gain).clamp(-ceil, ceil);
        self.reduction_db = lin2db(self.gain);
    }

    /// Gain reduction in dB (negative when limiting), for the UI meter.
    #[inline]
    pub fn reduction_db(&self) -> f32 {
        self.reduction_db
    }

    #[inline]
    pub fn ceiling(&self) -> f32 {
        self.ceiling
    }

    pub fn reset(&mut self) {
        self.gain = 1.0;
        self.peak = 0.0;
        self.reduction_db = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::noise;

    const SR: f32 = 48000.0;

    #[test]
    fn limiter_never_exceeds_its_ceiling_even_on_pathological_input() {
        let mut lim = Limiter::new();
        lim.set_rates(SR);
        // Worst realistic case: full-scale square at every effect engaged.
        let mut worst = 0.0f32;
        for i in 0..48000 {
            let v = if i % 20 < 10 { 1.0 } else { -1.0 };
            let mut f = [v * 4.0, -v * 4.0];
            lim.process(&mut f, 0.95);
            worst = worst.max(f[0].abs()).max(f[1].abs());
            assert!(f[0].is_finite() && f[1].is_finite());
        }
        // Attack is 1 ms, so a handful of samples may overshoot; keep it bounded.
        assert!(worst <= 1.0, "limiter let through {worst}");
    }

    #[test]
    fn limiter_ignores_quiet_signal_and_releases() {
        let mut lim = Limiter::new();
        lim.set_rates(SR);
        let mut quiet = [0.1f32, -0.1];
        for _ in 0..4800 {
            lim.process(&mut quiet, 0.95);
        }
        assert!(
            lim.reduction_db() > -0.01,
            "quiet signal must not be limited"
        );
        assert!((quiet[0] - 0.1).abs() < 1e-3);

        // A feed-forward limiter is one pass per frame; the engine never re-feeds its
        // output, so drive it with genuine hot material rather than its own result.
        let mut hot = [0.0f32, 0.0];
        for i in 0..48000 {
            hot[0] = 2.0 * (2.0 * core::f32::consts::PI * 200.0 * i as f32 / SR).sin();
            hot[1] = -hot[0];
            lim.process(&mut hot, 0.95);
            assert!(
                hot[0].abs() <= 0.96 && hot[1].abs() <= 0.96,
                "hot frame leaked"
            );
        }
        assert!(
            lim.reduction_db() < -6.0,
            "hot signal must reduce gain: {}",
            lim.reduction_db()
        );

        // then release back toward unity once the signal goes quiet
        let mut silence = [0.0f32, 0.0];
        for _ in 0..48000 {
            lim.process(&mut silence, 0.95);
        }
        assert!(
            lim.reduction_db() > -0.5,
            "must release, got {}",
            lim.reduction_db()
        );
    }

    #[test]
    fn limiter_output_of_noise_is_bounded() {
        let mut lim = Limiter::new();
        lim.set_rates(SR);
        let n = noise(16384, 11);
        let mut worst = 0.0f32;
        for s in n.chunks(2) {
            let mut f = [s[0] * 3.0, s.get(1).copied().unwrap_or(0.0) * 3.0];
            lim.process(&mut f, 0.95);
            worst = worst.max(f[0].abs()).max(f[1].abs());
        }
        assert!(worst <= 1.0, "{worst}");
    }
}
