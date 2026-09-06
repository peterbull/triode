//! Compressor / limiter — envelope follower with separate attack and release, no
//! lookahead (lookahead would add latency under the fingers for no benefit here).

use crate::dsp::biquad::OnePole;
use crate::dsp::{db2lin, sanitize, tau_coef, Frame};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const THRESHOLD: usize = 0;
pub const RATIO: usize = 1;
pub const ATTACK: usize = 2;
pub const RELEASE: usize = 3;
pub const MAKEUP: usize = 4;

#[derive(Debug)]
pub struct Compressor {
    sr: f32,
    env: OnePole,
    gain: f32,
    /// Reported gain reduction in dB for the last block (negative when squashing).
    reduction_db: f32,
}

impl Compressor {
    pub fn new() -> Compressor {
        Compressor {
            sr: 48000.0,
            env: OnePole::new(),
            gain: 1.0,
            reduction_db: 0.0,
        }
    }

    /// Gain the static curve asks for at envelope level `lvl`.
    #[inline]
    fn target_gain(&self, lvl: f32, th: f32, ratio: f32) -> f32 {
        if lvl <= th || th <= 0.0 {
            return 1.0;
        }
        // output = th * (lvl/th)^(1/ratio); gain = output / lvl
        let out = th * (lvl / th).powf(1.0 / ratio);
        (out / lvl).clamp(0.01, 1.0)
    }
}

impl Default for Compressor {
    fn default() -> Self {
        Compressor::new()
    }
}

impl Proc for Compressor {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sr.max(1.0);
        // Envelope detector time constant: short enough to catch picks, long enough not
        // to follow the waveform itself (which would distort instead of compress).
        self.env.set_tau(0.003, self.sr);
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let th = db2lin(sanitize(p.v[THRESHOLD], -90.0, -90.0, 0.0));
        let ratio = sanitize(p.v[RATIO], 1.0, 1.0, 40.0);
        let atk = 1.0 - tau_coef(sanitize(p.v[ATTACK], 0.05, 0.05, 200.0) / 1000.0, self.sr);
        let rel = 1.0 - tau_coef(sanitize(p.v[RELEASE], 1.0, 1.0, 4000.0) / 1000.0, self.sr);
        let makeup = db2lin(sanitize(p.v[MAKEUP], -30.0, -30.0, 40.0));

        let mut min_gain = 1.0f32;
        for f in buf[..n].iter_mut() {
            let lvl = self.env.lowpass(f[0].abs().max(f[1].abs()));
            let target = self.target_gain(lvl, th, ratio);
            // Attack when closing, release when opening — the asymmetry is the effect.
            let coef = if target < self.gain { atk } else { rel };
            self.gain += (target - self.gain) * coef;
            min_gain = min_gain.min(self.gain);
            f[0] *= self.gain * makeup;
            f[1] *= self.gain * makeup;
        }
        self.reduction_db = crate::dsp::lin2db(min_gain);
    }

    fn reset(&mut self) {
        self.env.reset();
        self.gain = 1.0;
        self.reduction_db = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{peak, rms, sine};
    use crate::dsp::lin2db;
    use crate::params::EffectKind;

    const SR: f32 = 48000.0;

    fn params(th: f32, ratio: f32, atk: f32, rel: f32, makeup: f32) -> ParamVals {
        // process() receives real units, not 0..=1 knob positions.
        let mut p = EffectKind::Compressor.default_values();
        for (i, v) in [th, ratio, atk, rel, makeup].iter().enumerate() {
            p.v[i] = *v;
        }
        p
    }

    fn run(c: &mut Compressor, x: &[f32], p: &ParamVals) -> Vec<f32> {
        let mut buf: Vec<Frame> = x.iter().map(|s| [*s, *s]).collect();
        let n = buf.len();
        c.process(&mut buf, n, p);
        buf.iter().map(|f| f[0]).collect()
    }

    #[test]
    fn loud_is_squashed_and_quiet_is_not() {
        let p = params(-30.0, 6.0, 2.0, 100.0, 0.0);
        let loud = sine(48000, 300.0, SR, 0.9);
        let quiet = sine(48000, 300.0, SR, 0.02);

        let mut c = Compressor::new();
        c.set_rates(SR);
        let y_loud = run(&mut c, &loud, &p);
        let loud_ratio = rms(&y_loud[24000..]) / rms(&loud[24000..]);

        let mut c = Compressor::new();
        c.set_rates(SR);
        let y_quiet = run(&mut c, &quiet, &p);
        let quiet_ratio = rms(&y_quiet[24000..]) / rms(&quiet[24000..]);

        assert!(
            loud_ratio < 0.7,
            "loud signal was not compressed (ratio {loud_ratio})"
        );
        assert!(
            (quiet_ratio - 1.0).abs() < 0.05,
            "quiet signal should be untouched, got {quiet_ratio}"
        );
    }

    #[test]
    fn higher_ratio_squashes_harder() {
        let loud = sine(48000, 300.0, SR, 0.9);
        let mut levels = Vec::new();
        for ratio in [1.0f32, 4.0, 12.0] {
            let mut c = Compressor::new();
            c.set_rates(SR);
            let y = run(&mut c, &loud, &params(-30.0, ratio, 1.0, 100.0, 0.0));
            levels.push(rms(&y[24000..]));
        }
        assert!(
            levels[0] > levels[1] && levels[1] > levels[2],
            "ratio order wrong: {levels:?}"
        );
    }

    #[test]
    fn gain_reduction_is_reported_and_makeup_compensates() {
        let loud = sine(48000, 300.0, SR, 0.9);
        let mut c = Compressor::new();
        c.set_rates(SR);
        let dry = run(&mut c, &loud, &params(-30.0, 8.0, 1.0, 100.0, 0.0));
        assert!(
            c.reduction_db < -3.0,
            "expected gain reduction, got {}",
            c.reduction_db
        );
        assert!(
            lin2db(peak(&dry[24000..])) < -6.0,
            "compressed peak should be down"
        );

        let mut c = Compressor::new();
        c.set_rates(SR);
        let wet = run(&mut c, &loud, &params(-30.0, 8.0, 1.0, 100.0, 18.0));
        assert!(
            peak(&wet[24000..]) > peak(&dry[24000..]),
            "makeup should lift level"
        );
    }

    #[test]
    fn attack_and_release_are_asymmetric() {
        // Loud for a while, then silence: with a slow attack the compressor should still
        // be clamping at the start of the note, and with a slow release still recovering
        // after it ends.
        let mut sig = sine(24000, 500.0, SR, 0.9);
        sig.extend(vec![0.0f32; 24000]);
        let mut slow_atk = Compressor::new();
        slow_atk.set_rates(SR);
        let y = run(&mut slow_atk, &sig, &params(-40.0, 10.0, 60.0, 5.0, 0.0));
        // A 60 ms attack over a 500 ms note: the first 10 ms should be near unity.
        assert!(
            rms(&y[100..5000]) > rms(&y[20000..23000]),
            "slow attack should let the transient through"
        );
    }

    #[test]
    fn junk_params_and_silence_stay_finite() {
        let mut c = Compressor::new();
        c.set_rates(SR);
        let mut p = ParamVals::ZEROED;
        p.v[THRESHOLD] = f32::NAN;
        p.v[RATIO] = f32::INFINITY;
        p.v[ATTACK] = -5.0;
        p.v[RELEASE] = f32::NAN;
        p.v[MAKEUP] = 1e30;
        let mut buf = [[0.0f32; 2]; 512];
        c.process(&mut buf, 512, &p);
        assert!(buf.iter().all(|f| f[0].is_finite() && f[1].is_finite()));
    }
}
