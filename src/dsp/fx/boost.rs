//! Clean boost with a tilt control — a volume pedal / line driver with a bit of EQ
//! character, useful both in front of the amp (to push it harder) and in an effects loop.

use crate::dsp::biquad::OnePole;
use crate::dsp::{db2lin, sanitize, Frame};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const LEVEL: usize = 0;
pub const TILT: usize = 1;

/// Tilt crossover: below it the `tilt` knob cuts, above it the knob boosts, so the two
/// ends of the knob are "bass up / treble down" and "treble up / bass down" rather than
/// two independent controls that fight.
const TILT_HZ: f32 = 250.0;
const TILT_RANGE_DB: f32 = 8.0;

#[derive(Debug, Default)]
pub struct Boost {
    sr: f32,
    lp: [OnePole; 2],
}

impl Boost {
    pub fn new() -> Boost {
        Boost {
            sr: 48000.0,
            lp: [OnePole::new(); 2],
        }
    }
}

impl Proc for Boost {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sr.max(1.0);
        for lp in self.lp.iter_mut() {
            lp.set_hz(TILT_HZ, self.sr);
        }
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let level = db2lin(sanitize(p.v[LEVEL], -60.0, -60.0, 40.0));
        let tilt = sanitize(p.v[TILT], 0.0, 0.0, 1.0);
        let lo = db2lin((0.5 - tilt) * TILT_RANGE_DB);
        let hi = db2lin((tilt - 0.5) * TILT_RANGE_DB);

        for f in buf[..n].iter_mut() {
            for (channel, sample) in f.iter_mut().enumerate() {
                let low = self.lp[channel].lowpass(*sample);
                *sample = (low * lo + (*sample - low) * hi) * level;
            }
        }
    }

    fn reset(&mut self) {
        for lp in self.lp.iter_mut() {
            lp.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{goertzel_mag, sine};
    use crate::params::EffectKind;

    const SR: f32 = 48000.0;

    fn params(level_db: f32, tilt: f32) -> ParamVals {
        // process() receives real units, not 0..=1 knob positions.
        let mut p = EffectKind::Boost.default_values();
        for (i, v) in [level_db, tilt].iter().enumerate() {
            p.v[i] = *v;
        }
        p
    }

    fn run(b: &mut Boost, x: &[f32], p: &ParamVals) -> Vec<f32> {
        let mut buf: Vec<Frame> = x.iter().map(|s| [*s, *s]).collect();
        let n = buf.len();
        b.process(&mut buf, n, p);
        buf.iter().map(|f| f[0]).collect()
    }

    #[test]
    fn channel_histories_are_independent() {
        let x = sine(4096, 700.0, SR, 0.4);
        let p = params(0.0, 0.0);

        let mut matching = Boost::new();
        matching.set_rates(SR);
        let mut stereo: Vec<Frame> = x.iter().map(|s| [*s, *s]).collect();
        let n = stereo.len();
        matching.process(&mut stereo, n, &p);
        assert!(
            stereo.iter().all(|f| (f[0] - f[1]).abs() < 1e-7),
            "identical inputs must remain identical"
        );

        let mut isolated = Boost::new();
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
    fn level_knob_acts_in_db() {
        let x = sine(16384, 500.0, SR, 0.2);
        for (db, expect) in [(-12.0f32, 0.25f32), (0.0, 1.0), (12.0, 4.0)] {
            let mut b = Boost::new();
            b.set_rates(SR);
            let y = run(&mut b, &x, &params(db, 0.5));
            let g = goertzel_mag(&y[4096..], 500.0, SR) / goertzel_mag(&x[4096..], 500.0, SR);
            assert!(
                (g - expect).abs() / expect < 0.05,
                "{db} dB gave gain {g}, wanted {expect}"
            );
        }
    }

    #[test]
    fn centred_tilt_is_flat_and_extremes_tilt_opposite_ways() {
        let lo_sig = sine(16384, 80.0, SR, 0.3);
        let hi_sig = sine(16384, 6000.0, SR, 0.3);
        let measure = |tilt: f32, sig: &[f32], f: f32| -> f32 {
            let mut b = Boost::new();
            b.set_rates(SR);
            let y = run(&mut b, sig, &params(0.0, tilt));
            let src: Vec<f32> = sig.iter().skip(4096).copied().collect();
            goertzel_mag(&y[4096..], f, SR) / goertzel_mag(&src, f, SR).max(1e-9)
        };
        // centred tilt with unity level is transparent
        assert!((measure(0.5, &hi_sig, 6000.0) - 1.0).abs() < 0.1);
        let dark_low = measure(0.0, &lo_sig, 80.0);
        let dark_hi = measure(0.0, &hi_sig, 6000.0);
        let bright_low = measure(1.0, &lo_sig, 80.0);
        let bright_hi = measure(1.0, &hi_sig, 6000.0);
        assert!(dark_low > bright_low, "tilt=0 should favour bass");
        assert!(bright_hi > dark_hi, "tilt=1 should favour treble");
    }

    #[test]
    fn junk_stays_finite() {
        let mut b = Boost::new();
        b.set_rates(SR);
        let mut p = ParamVals::ZEROED;
        p.v[LEVEL] = f32::NAN;
        p.v[TILT] = 99.0;
        let mut buf = [[0.4f32, -0.4]; 128];
        b.process(&mut buf, 128, &p);
        assert!(buf.iter().all(|f| f[0].is_finite() && f[1].is_finite()));
    }
}
