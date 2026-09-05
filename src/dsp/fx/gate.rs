//! Noise gate — hysteretic, with a release ramp rather than a switch.
//!
//! Two details matter. A hard on/off gate clicks on every syllable, so the gain is
//! ramped. And a single threshold chatters forever on a level that sits exactly at the
//! threshold (very common with a single-coil pickup and high gain), so the gate opens at
//! `threshold` and only closes below 70 % of it.

use crate::dsp::biquad::OnePole;
use crate::dsp::{db2lin, sanitize, tau_coef, Frame};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const THRESHOLD: usize = 0;
pub const RELEASE: usize = 1;

/// dB of attenuation when shut.
const CLOSE_DB: f32 = -80.0;

#[derive(Debug)]
pub struct Gate {
    sr: f32,
    env: OnePole,
    gain: f32,
    atk_coef: f32,
    open: bool,
}

impl Gate {
    pub fn new() -> Gate {
        Gate {
            sr: 48000.0,
            env: OnePole::new(),
            gain: 0.0,
            atk_coef: 0.5,
            open: false,
        }
    }
}

impl Default for Gate {
    fn default() -> Self {
        Gate::new()
    }
}

impl Proc for Gate {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sr.max(1.0);
        self.atk_coef = 1.0 - tau_coef(0.002, self.sr);
        self.env.set_tau(0.004, self.sr);
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let th = db2lin(sanitize(p.v[THRESHOLD], -90.0, -90.0, 0.0));
        let rel = 1.0 - tau_coef(sanitize(p.v[RELEASE], 1.0, 1.0, 4000.0) / 1000.0, self.sr);
        let floor = db2lin(CLOSE_DB);
        for f in buf[..n].iter_mut() {
            let lvl = self.env.lowpass(f[0].abs().max(f[1].abs()));
            if !self.open {
                if lvl > th {
                    self.open = true;
                }
            } else if lvl < th * 0.7 {
                self.open = false;
            }
            let target = if self.open { 1.0 } else { floor };
            let coef = if target > self.gain {
                self.atk_coef
            } else {
                rel
            };
            self.gain += (target - self.gain) * coef;
            f[0] *= self.gain;
            f[1] *= self.gain;
        }
    }

    fn reset(&mut self) {
        self.env.reset();
        self.gain = 0.0;
        self.open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{noise, peak, rms, sine};
    use crate::params::EffectKind;

    const SR: f32 = 48000.0;

    fn params(threshold_db: f32, release_ms: f32) -> ParamVals {
        // process() receives real units, not 0..=1 knob positions.
        let mut p = EffectKind::Gate.default_values();
        for (i, v) in [threshold_db, release_ms].iter().enumerate() {
            p.v[i] = *v;
        }
        p
    }

    fn run(g: &mut Gate, x: &[f32], p: &ParamVals) -> Vec<f32> {
        let mut buf: Vec<Frame> = x.iter().map(|s| [*s, *s]).collect();
        let n = buf.len();
        g.process(&mut buf, n, p);
        buf.iter().map(|f| f[0]).collect()
    }

    #[test]
    fn noise_floor_is_gated_away() {
        let mut g = Gate::new();
        g.set_rates(SR);
        let n = noise(48000, 3);
        let quiet: Vec<f32> = n.iter().map(|s| s * 0.002).collect(); // ~ -54 dBFS
        let y = run(&mut g, &quiet, &params(-40.0, 50.0));
        assert!(
            rms(&y[..2000]) > 0.0,
            "should start open-ish with real input"
        );
        assert!(
            peak(&y[24000..]) < 1e-3,
            "noise must be shut out, got {}",
            peak(&y[24000..])
        );
    }

    #[test]
    fn playing_signal_passes() {
        let mut g = Gate::new();
        g.set_rates(SR);
        let x = sine(24000, 200.0, SR, 0.5);
        let y = run(&mut g, &x, &params(-40.0, 50.0));
        assert!(
            rms(&y[12000..]) > 0.3,
            "a loud note must pass, rms {}",
            rms(&y[12000..])
        );
    }

    #[test]
    fn hysteresis_does_not_chatter_just_below_threshold() {
        let mut g = Gate::new();
        g.set_rates(SR);
        let p = params(-40.0, 5.0);
        let th = db2lin(-40.0);
        let mut open_seen = 0usize;
        let mut closed_seen = 0usize;
        // Level that lands between the close threshold (0.7*th) and th.
        let mid = sine(48000, 1000.0, SR, th * 0.85);
        let y = run(&mut g, &mid, &p);
        for w in y[12000..].chunks(500) {
            if rms(w) > th * 0.5 {
                open_seen += 1;
            } else {
                closed_seen += 1;
            }
        }
        assert!(
            closed_seen == 0 || open_seen == 0,
            "gate chattered: {open_seen} open / {closed_seen} closed windows"
        );
    }

    #[test]
    fn release_time_controls_how_long_it_takes_to_shut() {
        let th = db2lin(-40.0);
        let mut tone = sine(24000, 440.0, SR, th * 4.0);
        // Note, then a bed of "amp hiss" sitting *below* the hang threshold. Release is
        // only audible on something: against digital silence both gates read exactly zero
        // however long they take, which says nothing about release time.
        let bed: Vec<f32> = noise(24000, 3).iter().map(|s| s * th * 0.15).collect();
        tone.extend(bed);

        let mut fast = Gate::new();
        fast.set_rates(SR);
        let y_fast = run(&mut fast, &tone, &params(-40.0, 10.0));

        let mut slow = Gate::new();
        slow.set_rates(SR);
        let y_slow = run(&mut slow, &tone, &params(-40.0, 900.0));

        // After the note stops, the slow release must still be passing audio where the
        // fast one has already closed.
        let at = 24000 + 4800; // 100 ms into the silence
        assert!(
            peak(&y_fast[24100..24100 + 2000]) < 0.05,
            "fast gate stayed open"
        );
        assert!(
            rms(&y_slow[at..at + 2000]) > rms(&y_fast[at..at + 2000]) * 3.0,
            "slow release not slower"
        );
    }

    #[test]
    fn extreme_parameters_stay_finite() {
        let mut g = Gate::new();
        g.set_rates(SR);
        let mut p = ParamVals::ZEROED;
        p.v[THRESHOLD] = f32::NAN;
        p.v[RELEASE] = 1e9;
        let mut buf = [[0.3f32, -0.3]; 256];
        g.process(&mut buf, 256, &p);
        assert!(buf.iter().all(|f| f[0].is_finite() && f[1].is_finite()));
    }
}
