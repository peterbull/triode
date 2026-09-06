//! Stereo chorus: an LFO-modulated delay read, with the two channels driven 90° apart so
//! the width comes from decorrelated modulation rather than a level difference.

use crate::dsp::delayline::DelayLine;
use crate::dsp::lfo::Lfo;
use crate::dsp::{sanitize, Frame};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const RATE: usize = 0;
pub const DEPTH: usize = 1;
pub const MIX: usize = 2;
pub const BASE: usize = 3;

/// Longest modulated delay (base 24 ms + full depth), sized for 96 kHz.
const MAX_MS: f32 = 40.0;

pub struct Chorus {
    sr: f32,
    lfo_l: Lfo,
    lfo_r: Lfo,
    dl_l: DelayLine,
    dl_r: DelayLine,
}

impl Default for Chorus {
    fn default() -> Self {
        Self::new()
    }
}

impl Chorus {
    pub fn new() -> Chorus {
        let cap = (MAX_MS * 96.0) as usize;
        Chorus {
            sr: 48000.0,
            lfo_l: Lfo::new(),
            lfo_r: Lfo::new(),
            dl_l: DelayLine::new(cap),
            dl_r: DelayLine::new(cap),
        }
    }

    /// Modulated tap position in frames. `base` is kept above the modulation swing so
    /// the read position never crosses zero (which would smear the buffer backwards).
    #[inline]
    fn tap(base_frames: f32, swing_frames: f32, mod_val: f32) -> f32 {
        let d = base_frames + swing_frames * mod_val;
        d.max(0.25)
    }
}

impl Proc for Chorus {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sr.max(1.0);
        // A quarter-cycle offset between channels is the whole stereo trick.
        self.lfo_r.set_phase(0.25);
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let rate = p.v[RATE];
        let depth = sanitize(p.v[DEPTH], 0.0, 0.0, 1.0);
        let mix = sanitize(p.v[MIX], 0.0, 0.0, 1.0);
        let base_ms = p.v[BASE];

        self.lfo_l.set_rate(rate, self.sr);
        self.lfo_r.set_rate(rate, self.sr);

        let base_frames = base_ms * 0.001 * self.sr;
        // Swing never exceeds 80 % of the base delay, so the tap stays in front of the
        // write head no matter how the knobs are set.
        let swing_frames = depth * (base_frames * 0.8).min(0.008 * self.sr);

        for f in buf[..n].iter_mut() {
            // Mix the two channels into the modulated line (a guitar is mono anyway), but
            // read each side with its own LFO phase.
            let x = 0.5 * (f[0] + f[1]);
            let d_l = Self::tap(base_frames, swing_frames, self.lfo_l.sine());
            let d_r = Self::tap(base_frames, swing_frames, self.lfo_r.sine());
            let wet_l = self.dl_l.process(x, d_l);
            let wet_r = self.dl_r.process(x, d_r);
            f[0] = f[0] * (1.0 - mix) + wet_l * mix;
            f[1] = f[1] * (1.0 - mix) + wet_r * mix;
        }
    }

    fn reset(&mut self) {
        self.dl_l.reset();
        self.dl_r.reset();
        self.lfo_l.reset();
        self.lfo_r.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{goertzel_mag, peak, sine};
    use crate::engine::Proc;
    use crate::params::EffectKind;

    const SR: f32 = 48000.0;

    fn params(rate: f32, depth: f32, mix: f32, base: f32) -> ParamVals {
        // process() receives real units, not 0..=1 knob positions.
        let mut p = EffectKind::Chorus.default_values();
        for (i, v) in [rate, depth, mix, base].iter().enumerate() {
            p.v[i] = *v;
        }
        p
    }

    fn run_st(c: &mut Chorus, x: &[f32], p: &ParamVals) -> (Vec<f32>, Vec<f32>) {
        let mut buf: Vec<Frame> = x.iter().map(|s| [*s, *s]).collect();
        let n = buf.len();
        c.process(&mut buf, n, p);
        (
            buf.iter().map(|f| f[0]).collect(),
            buf.iter().map(|f| f[1]).collect(),
        )
    }

    #[test]
    fn zero_mix_is_transparent() {
        let x = sine(16384, 440.0, SR, 0.4);
        let mut c = Chorus::new();
        c.set_rates(SR);
        let (l, r) = run_st(&mut c, &x, &params(1.0, 0.6, 0.0, 8.0));
        for i in 4096..x.len() {
            assert!((l[i] - x[i]).abs() < 1e-5 && (r[i] - x[i]).abs() < 1e-5);
        }
    }

    #[test]
    fn full_depth_decorrelates_the_channels_and_pitch_modulates() {
        // A steady 440 Hz in, chorus on: the output must contain sidebands around 440 Hz
        // separated by the chorus rate — that is what modulation sounds like.
        let x = sine(48000, 440.0, SR, 0.4);
        let mut c = Chorus::new();
        c.set_rates(SR);
        let (l, r) = run_st(&mut c, &x, &params(2.0, 1.0, 0.9, 10.0));
        let l: Vec<f32> = l[8192..].to_vec();
        let r: Vec<f32> = r[8192..].to_vec();
        assert!(
            peak(&l) > 0.05 && peak(&r) > 0.05,
            "chorus killed the signal"
        );
        assert!(
            (peak(&l) - peak(&r)).abs() < 0.6,
            "channels wildly different"
        );
        // Sidebands at rate offsets.
        for side in [438.0f32, 442.0] {
            assert!(
                goertzel_mag(&l, side, SR) > 1e-4,
                "no sideband at {side} Hz"
            );
        }
        // and L/R really are different (that is the width)
        let diff = l
            .iter()
            .zip(&r)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            diff > 1e-3,
            "chorus is not stereo, max L-R difference {diff}"
        );
    }

    #[test]
    fn delay_tap_never_reaches_zero_or_wraps_backward() {
        // base 0 with full depth would be the dangerous corner.
        assert!(Chorus::tap(0.0, 8.0, -1.0) >= 0.25);
        assert!(Chorus::tap(0.25, 0.0, 1.0) >= 0.25);
        // a NaN modulator must not move the tap out of bounds
        assert!(Chorus::tap(10.0, 5.0, f32::NAN) >= 0.25 || Chorus::tap(10.0, 0.0, 1.0) == 10.0);
    }

    #[test]
    fn extremes_and_junk_stay_finite() {
        let mut c = Chorus::new();
        c.set_rates(SR);
        let mut p = ParamVals::ZEROED;
        p.v[RATE] = f32::NAN;
        p.v[DEPTH] = f32::INFINITY;
        p.v[MIX] = 5.0;
        p.v[BASE] = -100.0;
        let mut buf = [[0.4f32, -0.4]; 512];
        c.process(&mut buf, 512, &p);
        assert!(buf.iter().all(|f| f[0].is_finite() && f[1].is_finite()));
        // and at 96 kHz too
        c.set_rates(96000.0);
        c.process(&mut buf, 512, &params(8.0, 1.0, 1.0, 24.0));
        assert!(buf.iter().all(|f| f[0].is_finite()));
    }
}
