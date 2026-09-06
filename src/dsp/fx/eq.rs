use crate::dsp::biquad::{Biquad, Coef};
use crate::dsp::{db2lin, sanitize, Frame, MAX_SAMPLE_RATE_HZ};
use crate::engine::Proc;
use crate::params::ParamVals;

const BASS: usize = 0;
const MID: usize = 1;
const FREQUENCY: usize = 2;
const Q: usize = 3;
const TREBLE: usize = 4;
const LEVEL: usize = 5;
const BASS_HZ: f32 = 120.0;
const TREBLE_HZ: f32 = 3500.0;

#[derive(Debug)]
struct Chain {
    bass: Biquad,
    mid: Biquad,
    treble: Biquad,
}

impl Chain {
    fn new() -> Self {
        Self {
            bass: Biquad::passthrough(),
            mid: Biquad::passthrough(),
            treble: Biquad::passthrough(),
        }
    }

    fn reset(&mut self) {
        self.bass.reset();
        self.mid.reset();
        self.treble.reset();
    }
}

#[derive(Debug)]
pub struct ParametricEq {
    sr: f32,
    ch: [Chain; 2],
    cached: [f32; 5],
}

impl ParametricEq {
    pub fn new() -> Self {
        Self {
            sr: 48_000.0,
            ch: [Chain::new(), Chain::new()],
            cached: [f32::NAN; 5],
        }
    }

    #[inline]
    fn update_coefficients(&mut self, bass: f32, mid: f32, frequency: f32, q: f32, treble: f32) {
        let values = [bass, mid, frequency, q, treble];
        if self.cached == values {
            return;
        }
        self.cached = values;
        let bass = Coef::lowshelf(BASS_HZ, bass, self.sr);
        let mid = Coef::peaking(frequency, mid, q, self.sr);
        let treble = Coef::highshelf(TREBLE_HZ, treble, self.sr);
        for chain in &mut self.ch {
            chain.bass.set(bass);
            chain.mid.set(mid);
            chain.treble.set(treble);
        }
    }
}

impl Default for ParametricEq {
    fn default() -> Self {
        Self::new()
    }
}

impl Proc for ParametricEq {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sanitize(sr, 48_000.0, 8000.0, MAX_SAMPLE_RATE_HZ);
        self.cached = [f32::NAN; 5];
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let bass = sanitize(p.v[BASS], 0.0, -12.0, 12.0);
        let mid = sanitize(p.v[MID], 0.0, -12.0, 12.0);
        let frequency = sanitize(
            p.v[FREQUENCY],
            800.0,
            150.0,
            3000.0_f32.min(self.sr * 0.45).max(150.0),
        );
        let q = sanitize(p.v[Q], 0.9, 0.3, 4.0);
        let treble = sanitize(p.v[TREBLE], 0.0, -12.0, 12.0);
        let level = db2lin(sanitize(p.v[LEVEL], 0.0, -12.0, 12.0));
        self.update_coefficients(bass, mid, frequency, q, treble);

        for frame in &mut buf[..n] {
            for (channel, chain) in self.ch.iter_mut().enumerate() {
                let sample = chain.bass.process(frame[channel]);
                let sample = chain.mid.process(sample);
                frame[channel] = chain.treble.process(sample) * level;
            }
        }
    }

    fn reset(&mut self) {
        for chain in &mut self.ch {
            chain.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{goertzel_mag, sine};
    use crate::dsp::db2lin;
    use crate::engine::Proc;
    use crate::params::EffectKind;

    const SR: f32 = 48_000.0;

    fn params(bass: f32, mid: f32, frequency: f32, q: f32, treble: f32, level: f32) -> ParamVals {
        let mut p = EffectKind::ParametricEq.default_values();
        p.v[..6].copy_from_slice(&[bass, mid, frequency, q, treble, level]);
        p
    }

    fn run(eq: &mut ParametricEq, x: &[f32], p: &ParamVals) -> Vec<f32> {
        let mut buf: Vec<Frame> = x.iter().map(|&sample| [sample, sample]).collect();
        let n = buf.len();
        eq.process(&mut buf, n, p);
        buf.into_iter().map(|frame| frame[0]).collect()
    }

    fn gain(frequency: f32, p: &ParamVals) -> f32 {
        let x = sine(16_384, frequency, SR, 0.25);
        let mut eq = ParametricEq::new();
        eq.set_rates(SR);
        let y = run(&mut eq, &x, p);
        goertzel_mag(&y[4096..], frequency, SR) / goertzel_mag(&x[4096..], frequency, SR)
    }

    #[test]
    fn flat_response_is_transparent() {
        let x = sine(4096, 997.0, SR, 0.4);
        let mut eq = ParametricEq::new();
        eq.set_rates(SR);
        let y = run(&mut eq, &x, &EffectKind::ParametricEq.default_values());
        assert!(
            x.iter()
                .zip(y)
                .all(|(input, output)| (input - output).abs() < 2e-6),
            "zero-gain EQ must be transparent"
        );
    }

    #[test]
    fn mid_gain_matches_the_requested_center_frequency() {
        let p = params(0.0, 6.0, 800.0, 0.9, 0.0, 0.0);
        let measured = gain(800.0, &p);
        let expected = db2lin(6.0);
        assert!(
            (measured - expected).abs() / expected < 0.05,
            "6 dB at the center measured {measured}, wanted {expected}"
        );
    }

    #[test]
    fn frequency_moves_the_mid_peak_and_q_controls_its_width() {
        let centered = gain(400.0, &params(0.0, 9.0, 400.0, 2.0, 0.0, 0.0));
        let moved = gain(400.0, &params(0.0, 9.0, 1_600.0, 2.0, 0.0, 0.0));
        assert!(centered > moved * 1.5, "frequency did not move the peak");

        let broad = gain(400.0, &params(0.0, 9.0, 800.0, 0.3, 0.0, 0.0));
        let narrow = gain(400.0, &params(0.0, 9.0, 800.0, 4.0, 0.0, 0.0));
        assert!(broad > narrow * 1.25, "Q did not change bandwidth");
    }

    #[test]
    fn shelves_move_their_respective_ends() {
        let low_boost = gain(80.0, &params(9.0, 0.0, 800.0, 0.9, 0.0, 0.0));
        let low_cut = gain(80.0, &params(-9.0, 0.0, 800.0, 0.9, 0.0, 0.0));
        let high_boost = gain(8000.0, &params(0.0, 0.0, 800.0, 0.9, 9.0, 0.0));
        let high_cut = gain(8000.0, &params(0.0, 0.0, 800.0, 0.9, -9.0, 0.0));
        assert!(
            low_boost > 1.5 && low_cut < 0.7,
            "bass shelf went the wrong way"
        );
        assert!(
            high_boost > 1.5 && high_cut < 0.7,
            "treble shelf went the wrong way"
        );
    }

    #[test]
    fn level_is_a_db_gain_after_the_filters() {
        let measured = gain(1000.0, &params(0.0, 0.0, 800.0, 0.9, 0.0, 6.0));
        let expected = db2lin(6.0);
        assert!(
            (measured - expected).abs() / expected < 0.02,
            "6 dB level measured {measured}, wanted {expected}"
        );
    }

    #[test]
    fn stereo_histories_match_without_crossfeed() {
        let x = sine(4096, 800.0, SR, 0.4);
        let p = params(6.0, -4.0, 800.0, 1.2, 5.0, 0.0);
        let mut matching = ParametricEq::new();
        matching.set_rates(SR);
        let mut stereo: Vec<Frame> = x.iter().map(|&sample| [sample, sample]).collect();
        let n = stereo.len();
        matching.process(&mut stereo, n, &p);
        assert!(stereo
            .iter()
            .all(|frame| (frame[0] - frame[1]).abs() < 1e-7));

        let mut isolated = ParametricEq::new();
        isolated.set_rates(SR);
        let mut left_only: Vec<Frame> = x.iter().map(|&sample| [sample, 0.0]).collect();
        let n = left_only.len();
        isolated.process(&mut left_only, n, &p);
        assert!(left_only.iter().all(|frame| frame[1].abs() < 1e-7));
    }

    #[test]
    fn reset_reproduces_the_same_output() {
        let x = sine(4096, 900.0, SR, 0.4);
        let p = params(4.0, -6.0, 900.0, 1.5, 3.0, -2.0);
        let mut eq = ParametricEq::new();
        eq.set_rates(SR);
        let first = run(&mut eq, &x, &p);
        eq.reset();
        let second = run(&mut eq, &x, &p);
        assert!(first.iter().zip(second).all(|(a, b)| (a - b).abs() < 1e-7));
    }

    #[test]
    fn junk_and_extreme_values_stay_finite_at_supported_rates() {
        for sr in [8000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            for p in [
                params(
                    f32::NAN,
                    f32::INFINITY,
                    f32::NEG_INFINITY,
                    f32::NAN,
                    f32::INFINITY,
                    f32::NEG_INFINITY,
                ),
                params(-999.0, 999.0, 999_999.0, -999.0, -999.0, 999.0),
            ] {
                let mut eq = ParametricEq::new();
                eq.set_rates(sr);
                let mut buf = [[0.4f32, -0.4]; 512];
                eq.process(&mut buf, 512, &p);
                assert!(
                    buf.iter().flatten().all(|sample| sample.is_finite()),
                    "non-finite output at {sr} Hz"
                );
            }
        }
    }
}
