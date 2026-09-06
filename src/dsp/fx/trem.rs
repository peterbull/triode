//! Tremolo — amplitude modulation. The LFO value is computed once per frame and applied
//! to both channels, so the effect never introduces a stereo phase artefact.

use crate::dsp::lfo::Lfo;
use crate::dsp::{db2lin, sanitize, Frame};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const RATE: usize = 0;
pub const DEPTH: usize = 1;
pub const LEVEL: usize = 2;

pub struct Tremolo {
    sr: f32,
    lfo: Lfo,
}

impl Tremolo {
    pub fn new() -> Tremolo {
        Tremolo {
            sr: 48000.0,
            lfo: Lfo::new(),
        }
    }
}

impl Default for Tremolo {
    fn default() -> Self {
        Tremolo::new()
    }
}

impl Proc for Tremolo {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sr.max(1.0);
        self.lfo.reset();
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let rate = p.v[RATE];
        let depth = sanitize(p.v[DEPTH], 0.0, 0.0, 1.0);
        let level = db2lin(sanitize(p.v[LEVEL], -60.0, -60.0, 30.0));
        self.lfo.set_rate(rate, self.sr);
        for f in buf[..n].iter_mut() {
            // sine in -1..=1 mapped to gain in (1-depth)..=1, never negative.
            let g = (1.0 - depth * 0.5 * (1.0 - self.lfo.sine())) * level;
            f[0] *= g;
            f[1] *= g;
        }
    }

    fn reset(&mut self) {
        self.lfo.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{goertzel_exact, peak, rms, sine};
    use crate::engine::Proc;
    use crate::params::EffectKind;

    const SR: f32 = 48000.0;

    fn params(rate: f32, depth: f32, level: f32) -> ParamVals {
        // process() receives real units, not 0..=1 knob positions.
        let mut p = EffectKind::Tremolo.default_values();
        for (i, v) in [rate, depth, level].iter().enumerate() {
            p.v[i] = *v;
        }
        p
    }

    fn run(t: &mut Tremolo, x: &[f32], p: &ParamVals) -> Vec<f32> {
        let mut buf: Vec<Frame> = x.iter().map(|s| [*s, *s]).collect();
        let n = buf.len();
        t.process(&mut buf, n, p);
        buf.iter().map(|f| f[0]).collect()
    }

    /// The modulation envelope, sampled at `sr / block`.
    ///
    /// Rectifying alone is not enough: |y| still wiggles at the carrier (300 Hz here),
    /// and a fixed-threshold or per-sample crossing count ends up measuring that. Block
    /// averaging throws the carrier away while keeping any tremolo rate we expose (<= 22
    /// Hz is well under the 125 Hz Nyquist of a 4 ms envelope).
    fn envelope(y: &[f32], sr: f32) -> (Vec<f32>, f32) {
        let block = (0.004 * sr) as usize;
        let mut env: Vec<f32> = Vec::with_capacity(y.len() / block + 1);
        for b in y.chunks(block) {
            env.push(b.iter().map(|s| s.abs()).sum::<f32>() / b.len() as f32);
        }
        (env, sr / block as f32)
    }

    #[test]
    fn zero_depth_is_just_a_level_control() {
        let x = sine(24000, 300.0, SR, 0.4);
        let mut t = Tremolo::new();
        t.set_rates(SR);
        let y = run(&mut t, &x, &params(6.0, 0.0, 0.0));
        for i in 4096..y.len() {
            assert!(
                (y[i] - x[i]).abs() < 1e-5,
                "depth 0 must be transparent at {i}"
            );
        }
    }

    #[test]
    fn depth_chops_the_signal() {
        let x = sine(48000, 300.0, SR, 0.5);
        let mut t = Tremolo::new();
        t.set_rates(SR);
        let y = run(&mut t, &x, &params(8.0, 1.0, 0.0));
        assert!(
            rms(&y[24000..]) < rms(&x[24000..]) * 0.8,
            "depth 1 should cut average level"
        );
        assert!(
            peak(&y[24000..]) > 0.4,
            "depth 1 should still pass the peaks"
        );
    }

    #[test]
    fn rate_matches_the_requested_frequency() {
        let x = sine(48000, 300.0, SR, 0.6);
        for rate in [4.0f32, 12.0] {
            let mut t = Tremolo::new();
            t.set_rates(SR);
            let y = run(&mut t, &x, &params(rate, 1.0, 0.0));
            // Ask the envelope directly how fast it pulses, rather than counting
            // crossings: the test's whole job is "the tremolo pulses at the knob rate",
            // and a full-wave (|sin|) stage would pulse at exactly twice that, which a
            // crossing count and a spectral test both see -- but only the spectral test
            // says at which rate.
            let (env, env_sr) = envelope(&y[4096..], SR);
            let at = |f: f32| goertzel_exact(&env, f, env_sr);
            let (a1, a2) = (at(rate), at(rate * 2.0));
            let mean = env.iter().sum::<f32>() / env.len() as f32;
            assert!(
                a1 > a2 * 2.0 && a1 > mean * 0.15,
                "{rate} Hz tremolo: envelope amplitude {a1:.5} at {rate} Hz, {a2:.5} at {} Hz (mean {mean:.4})",
                rate * 2.0
            );
        }
    }

    #[test]
    fn gain_is_never_negative_and_junk_is_finite() {
        let mut t = Tremolo::new();
        t.set_rates(SR);
        let mut p = ParamVals::ZEROED;
        p.v[RATE] = f32::NAN;
        p.v[DEPTH] = 4.0; // over-range depth
        p.v[LEVEL] = f32::NAN;
        let mut buf = [[0.5f32, -0.5]; 512];
        t.process(&mut buf, 512, &p);
        assert!(buf.iter().all(|f| f[0].is_finite() && f[1].is_finite()));
        // depth clamped to 1 means gain stays in 0..=1+level, never inverted
        let mut p2 = params(5.0, 1.0, 0.0);
        p2.v[DEPTH] = 1.0;
        let mut buf2 = [[1.0f32, 1.0]; 4096];
        t.process(&mut buf2, 4096, &p2);
        assert!(
            buf2.iter().all(|f| f[0] >= -1e-6),
            "tremolo inverted the signal"
        );
    }
}
