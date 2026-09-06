//! Overdrive and fuzz. Both are waveshapers run through the 2× oversampler in
//! [`crate::dsp::amp`], because a pedal into a gain-y amp is exactly where aliasing is
//! most audible.

use crate::dsp::amp::{shape, Clip2x};
use crate::dsp::biquad::{DcBlocker, OnePole};
use crate::dsp::{db2lin, sanitize, Frame};
use crate::engine::Proc;
use crate::params::ParamVals;

pub mod od_ix {
    pub const DRIVE: usize = 0;
    pub const TONE: usize = 1;
    pub const LEVEL: usize = 2;
}

pub mod fuzz_ix {
    pub const FUZZ: usize = 0;
    pub const TONE: usize = 1;
    pub const LEVEL: usize = 2;
}

/// A transparent-ish drive: more gain into a soft asymmetric knee, then a passive-
/// looking one-pole tone cut. Keeps the low end (the thing a cheap overdrive ruins).
pub struct Overdrive {
    sr: f32,
    clip: [Clip2x; 2],
    tone: [OnePole; 2],
}

impl Overdrive {
    pub fn new() -> Overdrive {
        Overdrive {
            sr: 48000.0,
            clip: [Clip2x::new(48000.0), Clip2x::new(48000.0)],
            tone: [OnePole::new(); 2],
        }
    }
}

impl Default for Overdrive {
    fn default() -> Self {
        Overdrive::new()
    }
}

impl Proc for Overdrive {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sr.max(1.0);
        for clip in self.clip.iter_mut() {
            clip.set_rates(self.sr);
        }
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let drive = sanitize(p.v[od_ix::DRIVE], 0.0, 0.0, 1.0);
        let tone_hz = sanitize(p.v[od_ix::TONE], 3000.0, 80.0, self.sr * 0.45);
        let level = db2lin(sanitize(p.v[od_ix::LEVEL], -60.0, -60.0, 30.0));
        for tone in self.tone.iter_mut() {
            tone.set_hz(tone_hz, self.sr);
        }

        let pre = 1.0 + drive * 36.0;
        let t_pos = (0.95 - drive * 0.92).clamp(0.015, 0.99);
        let t_neg = (t_pos * 1.45).clamp(0.02, 0.995);
        // Compensate roughly for the knee so the level knob still means something
        // as drive climbs.
        let trim = 0.35 + 0.65 * t_pos;
        let shaper = |v: f32| shape(v, t_pos, t_neg);

        for f in buf[..n].iter_mut() {
            for (channel, sample) in f.iter_mut().enumerate() {
                let x = self.clip[channel].push(*sample * pre, shaper);
                *sample = self.tone[channel].lowpass(x) * trim * level;
            }
        }
    }

    fn reset(&mut self) {
        for clip in self.clip.iter_mut() {
            clip.reset();
        }
        for tone in self.tone.iter_mut() {
            tone.reset();
        }
    }
}

/// Germanium-style fuzz: a lot more gain, an intentional DC bias ahead of the shaper
/// (which is where the sputtery even-harmonic character comes from) and a DC blocker
/// afterwards so the bias never reaches the amp's cab sim.
pub struct Fuzz {
    sr: f32,
    clip: [Clip2x; 2],
    tone: [OnePole; 2],
    dc: [DcBlocker; 2],
}

impl Fuzz {
    pub fn new() -> Fuzz {
        Fuzz {
            sr: 48000.0,
            clip: [Clip2x::new(48000.0), Clip2x::new(48000.0)],
            tone: [OnePole::new(); 2],
            dc: [DcBlocker::new(); 2],
        }
    }
}

impl Default for Fuzz {
    fn default() -> Self {
        Fuzz::new()
    }
}

impl Proc for Fuzz {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sr.max(1.0);
        for clip in self.clip.iter_mut() {
            clip.set_rates(self.sr);
        }
        for dc in self.dc.iter_mut() {
            dc.set_hz(90.0, self.sr); // fuzz wants its top end, not its bias
        }
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let fuzz = sanitize(p.v[fuzz_ix::FUZZ], 0.0, 0.0, 1.0);
        let tone_hz = sanitize(p.v[fuzz_ix::TONE], 2400.0, 80.0, self.sr * 0.45);
        let level = db2lin(sanitize(p.v[fuzz_ix::LEVEL], -60.0, -60.0, 30.0));
        for tone in self.tone.iter_mut() {
            tone.set_hz(tone_hz, self.sr);
        }

        let pre = 3.0 + fuzz * 220.0;
        let t_pos = (0.6 - fuzz * 0.57).clamp(0.008, 0.9);
        let t_neg = t_pos * 0.62; // deliberately lopsided
                                  // The bias has to be expressed in the shaper's own units. As an absolute offset it
                                  // stayed near 0.41 while the threshold fell to 0.03 with the knob, so at full fuzz
                                  // the bias alone was 13x the clip threshold: the shaper saturated hard on the DC,
                                  // passed no signal at all, and the 90 Hz DC blocker behind it then removed the
                                  // lot -- measured output was 1.5e-43 at fuzz 1.0, i.e. a pedal that goes silent
                                  // exactly where it should be nastiest. Scaling by t_pos keeps bias/threshold ratio
                                  // constant, which is what makes the knob keep working across its whole travel.
        let bias = (0.06 + fuzz * 0.35) * t_pos;
        let trim = 0.25 + 0.5 * t_pos;
        let shaper = |v: f32| shape(v, t_pos, t_neg);

        for f in buf[..n].iter_mut() {
            for (channel, sample) in f.iter_mut().enumerate() {
                let x = self.clip[channel].push((*sample + bias) * pre, shaper);
                let x = self.dc[channel].process(x);
                *sample = self.tone[channel].lowpass(x) * trim * level;
            }
        }
    }

    fn reset(&mut self) {
        for clip in self.clip.iter_mut() {
            clip.reset();
        }
        for tone in self.tone.iter_mut() {
            tone.reset();
        }
        for dc in self.dc.iter_mut() {
            dc.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{goertzel_mag, mean, peak, sine, thd};
    use crate::engine::Proc;
    use crate::params::EffectKind;

    const SR: f32 = 48000.0;

    fn p(kind: EffectKind, vals: [f32; 3]) -> ParamVals {
        // process() receives real units, not 0..=1 knob positions.
        let mut pv = kind.default_values();
        for (i, v) in vals.iter().enumerate() {
            pv.v[i] = *v;
        }
        pv
    }

    fn run<E: Proc>(e: &mut E, x: &[f32], p: &ParamVals) -> Vec<f32> {
        let mut buf: Vec<Frame> = x.iter().map(|s| [*s, *s]).collect();
        let n = buf.len();
        e.process(&mut buf, n, p);
        buf.iter().map(|f| f[0]).collect()
    }

    #[test]
    fn overdrive_channel_histories_are_independent() {
        let x = sine(4096, 700.0, SR, 0.4);
        let p = p(EffectKind::Overdrive, [0.8, 5000.0, 0.0]);

        let mut matching = Overdrive::new();
        matching.set_rates(SR);
        let mut stereo: Vec<Frame> = x.iter().map(|s| [*s, *s]).collect();
        let n = stereo.len();
        matching.process(&mut stereo, n, &p);
        assert!(
            stereo.iter().all(|f| (f[0] - f[1]).abs() < 1e-7),
            "identical inputs must remain identical"
        );

        let mut isolated = Overdrive::new();
        isolated.set_rates(SR);
        let mut left_only: Vec<Frame> = x.iter().map(|s| [*s, 0.0]).collect();
        let n = left_only.len();
        isolated.process(&mut left_only, n, &p);
        assert!(
            left_only.iter().all(|f| f[1].abs() < 1e-7),
            "left input leaked into right output"
        );
    }

    #[test]
    fn fuzz_channel_histories_are_independent_of_left_excitation() {
        let x = sine(4096, 700.0, SR, 0.4);
        let p = p(EffectKind::Fuzz, [0.8, 5000.0, 0.0]);

        let mut matching = Fuzz::new();
        matching.set_rates(SR);
        let mut stereo: Vec<Frame> = x.iter().map(|s| [*s, *s]).collect();
        let n = stereo.len();
        matching.process(&mut stereo, n, &p);
        assert!(
            stereo.iter().all(|f| (f[0] - f[1]).abs() < 1e-7),
            "identical inputs must remain identical"
        );

        let mut excited = Fuzz::new();
        excited.set_rates(SR);
        let mut left_only: Vec<Frame> = x.iter().map(|s| [*s, 0.0]).collect();
        let n = left_only.len();
        excited.process(&mut left_only, n, &p);

        let mut baseline = Fuzz::new();
        baseline.set_rates(SR);
        let mut zero = vec![[0.0f32; 2]; x.len()];
        baseline.process(&mut zero, n, &p);
        assert!(
            left_only
                .iter()
                .zip(&zero)
                .all(|(excited, baseline)| (excited[1] - baseline[1]).abs() < 1e-7),
            "left excitation changed the right-channel bias response"
        );
    }

    #[test]
    fn overdrive_adds_harmonics_with_drive_and_stays_clean_without() {
        let x = sine(32768, 200.0, SR, 0.25);
        let mut low = Overdrive::new();
        low.set_rates(SR);
        let y_low = run(&mut low, &x, &p(EffectKind::Overdrive, [0.0, 9000.0, 0.0]));

        let mut high = Overdrive::new();
        high.set_rates(SR);
        let y_high = run(&mut high, &x, &p(EffectKind::Overdrive, [1.0, 9000.0, 0.0]));

        let thd_low = thd(&y_low[8192..], 200.0, SR);
        let thd_high = thd(&y_high[8192..], 200.0, SR);
        assert!(
            thd_low < 0.05,
            "drive at zero should be nearly clean, got {thd_low}"
        );
        assert!(thd_high > 0.3, "full drive should distort, got {thd_high}");
    }

    #[test]
    fn overdrive_tone_knob_rolls_off_treble() {
        let x = sine(16384, 4000.0, SR, 0.2);
        let measure = |tone: f32| {
            let mut o = Overdrive::new();
            o.set_rates(SR);
            let y = run(&mut o, &x, &p(EffectKind::Overdrive, [0.0, tone, 0.0]));
            goertzel_mag(&y[4096..], 4000.0, SR)
        };
        let bright = measure(9000.0);
        let dark = measure(300.0);
        assert!(bright > 0.05, "tone at max should pass 4 kHz, got {bright}");
        assert!(
            dark < bright * 0.5,
            "tone knob did nothing: dark {dark} vs bright {bright}"
        );
    }

    #[test]
    fn fuzz_is_hotter_than_overdrive_and_neither_leaves_dc() {
        let x = sine(32768, 150.0, SR, 0.3);

        let mut f = Fuzz::new();
        f.set_rates(SR);
        let y_fuzz = run(&mut f, &x, &p(EffectKind::Fuzz, [1.0, 8000.0, 0.0]));

        let mut o = Overdrive::new();
        o.set_rates(SR);
        let y_od = run(&mut o, &x, &p(EffectKind::Overdrive, [1.0, 8000.0, 0.0]));

        // 0.43 is the ceiling for a fully squared wave summed over harmonics 2..=5.
        assert!(
            thd(&y_fuzz[8192..], 150.0, SR) > 0.35,
            "fuzz should be very saturated"
        );
        assert!(thd(&y_fuzz[8192..], 150.0, SR) > thd(&y_od[8192..], 150.0, SR));
        // The bias must not survive into the output.
        assert!(
            mean(&y_fuzz[8192..]).abs() < 0.02,
            "fuzz leaked DC: {}",
            mean(&y_fuzz[8192..])
        );
        assert!(mean(&y_od[8192..]).abs() < 0.02, "overdrive leaked DC");
    }

    #[test]
    fn both_are_bounded_for_a_screaming_input() {
        for level in [-30.0f32, 0.0, 30.0] {
            let x = sine(16384, 100.0, SR, 6.0);
            let mut f = Fuzz::new();
            f.set_rates(SR);
            let yf = run(&mut f, &x, &p(EffectKind::Fuzz, [1.0, 8000.0, level]));
            let mut o = Overdrive::new();
            o.set_rates(SR);
            let yo = run(&mut o, &x, &p(EffectKind::Overdrive, [1.0, 8000.0, level]));
            assert!(yf.iter().all(|s| s.is_finite()) && yo.iter().all(|s| s.is_finite()));
            // shaper-bounded * trim, before the engine's master and limiter
            // The shaper is bounded; the level knob is *meant* to be loud -- the engine's
            // master and limiter are the ceiling. A fixed bound here would just assert the
            // knob does nothing, so the bound has to track the knob.
            let bound = 3.0 * 10f32.powf(level / 20.0);
            assert!(
                peak(&yf) < bound && peak(&yo) < bound,
                "shaper output unbounded at {level} dB"
            );
        }
    }

    #[test]
    fn junk_parameters_stay_finite() {
        let mut o = Overdrive::new();
        o.set_rates(SR);
        let mut po = ParamVals::ZEROED;
        po.v[od_ix::DRIVE] = f32::NAN;
        po.v[od_ix::TONE] = f32::INFINITY;
        po.v[od_ix::LEVEL] = -1e30;
        let mut buf = [[0.3f32, -0.3]; 256];
        o.process(&mut buf, 256, &po);
        assert!(buf.iter().all(|f| f[0].is_finite()));

        let mut f = Fuzz::new();
        f.set_rates(SR);
        let mut pf = ParamVals::ZEROED;
        pf.v[fuzz_ix::FUZZ] = f32::NAN;
        pf.v[fuzz_ix::TONE] = -5.0;
        pf.v[fuzz_ix::LEVEL] = f32::NAN;
        f.process(&mut buf, 256, &pf);
        assert!(buf.iter().all(|f| f[0].is_finite()));
    }

    #[test]
    fn oversampled_shaper_does_not_emit_folds_at_high_input() {
        // A 12 kHz input driven hard: 3rd harmonic is 36 kHz, above Nyquist, and must
        // not arrive audible at 12 kHz as a fold-over of itself.
        let x = sine(32768, 12000.0, SR, 0.4);
        let mut o = Overdrive::new();
        o.set_rates(SR);
        let y = run(&mut o, &x, &p(EffectKind::Overdrive, [0.8, 9000.0, 0.0]));
        let fund = goertzel_mag(&y[8192..], 12000.0, SR);
        let alias20 = goertzel_mag(&y[8192..], 20000.0, SR);
        assert!(fund > 1e-4, "12 kHz fundamental vanished: {fund}");
        assert!(
            alias20 < fund * 0.5,
            "20 kHz fold {alias20} vs fundamental {fund}"
        );
    }
}
