use crate::dsp::lfo::Lfo;
use crate::dsp::{sanitize, Frame, MAX_SAMPLE_RATE_HZ};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const RATE: usize = 0;
pub const DEPTH: usize = 1;
pub const FEEDBACK: usize = 2;
pub const MIX: usize = 3;

const STAGES: usize = 4;
const INTERPOLATION_SAMPLES: usize = 16;
const STAGE_RATIOS: [f32; STAGES] = [0.5, 0.8, 1.25, 2.0];

#[derive(Clone, Copy, Default)]
struct Allpass {
    x1: f32,
    y1: f32,
}

impl Allpass {
    #[inline]
    fn process(&mut self, a: f32, x: f32) -> f32 {
        let y = a * x + self.x1 - a * self.y1;
        self.x1 = x;
        self.y1 = y;
        y
    }
}

pub struct Phaser {
    sr: f32,
    lfo: Lfo,
    stages: [[Allpass; STAGES]; 2],
    previous_wet: [f32; 2],
    coefficients: [f32; STAGES],
    coefficient_steps: [f32; STAGES],
    interpolation_sample: usize,
}

impl Default for Phaser {
    fn default() -> Self {
        Self::new()
    }
}

impl Phaser {
    pub fn new() -> Self {
        Self {
            sr: 48_000.0,
            lfo: Lfo::new(),
            stages: [[Allpass::default(); STAGES]; 2],
            previous_wet: [0.0; 2],
            coefficients: [0.0; STAGES],
            coefficient_steps: [0.0; STAGES],
            interpolation_sample: 0,
        }
    }

    #[inline]
    fn coefficient(&self, frequency: f32) -> f32 {
        let maximum = self.sr * 0.45;
        let frequency = frequency.clamp(20.0_f32.min(maximum), maximum);
        let tangent = (std::f32::consts::PI * frequency / self.sr).tan();
        (tangent - 1.0) / (tangent + 1.0)
    }

    #[inline]
    fn set_targets(&mut self, lfo: f32, depth: f32) {
        let center = 700.0 * 2.0f32.powf(1.5 * depth * lfo);
        for (i, ratio) in STAGE_RATIOS.iter().enumerate() {
            let target = self.coefficient(center * ratio);
            self.coefficient_steps[i] =
                (target - self.coefficients[i]) / INTERPOLATION_SAMPLES as f32;
        }
    }
}

impl Proc for Phaser {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sanitize(sr, 48_000.0, 8_000.0, MAX_SAMPLE_RATE_HZ);
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let rate = sanitize(p.v[RATE], 0.5, 0.05, 8.0);
        let depth = sanitize(p.v[DEPTH], 0.7, 0.0, 1.0);
        let feedback = sanitize(p.v[FEEDBACK], 0.2, 0.0, 0.7);
        let mix = sanitize(p.v[MIX], 0.5, 0.0, 1.0);
        self.lfo.set_rate(rate, self.sr);

        for frame in &mut buf[..n] {
            let lfo = self.lfo.sine();
            if self.interpolation_sample == 0 {
                self.set_targets(lfo, depth);
            }
            for (coefficient, step) in self.coefficients.iter_mut().zip(self.coefficient_steps) {
                *coefficient += step;
            }

            for (channel, sample) in frame.iter_mut().enumerate() {
                let dry = if sample.is_finite() { *sample } else { 0.0 };
                let mut wet = dry + feedback * self.previous_wet[channel];
                for (stage, coefficient) in self.stages[channel].iter_mut().zip(self.coefficients) {
                    wet = stage.process(coefficient, wet);
                }
                self.previous_wet[channel] = wet;
                *sample = dry * (1.0 - mix) + wet * mix;
            }
            self.interpolation_sample = (self.interpolation_sample + 1) % INTERPOLATION_SAMPLES;
        }
    }

    fn reset(&mut self) {
        self.lfo.reset();
        self.stages = [[Allpass::default(); STAGES]; 2];
        self.previous_wet = [0.0; 2];
        self.coefficients = [0.0; STAGES];
        self.coefficient_steps = [0.0; STAGES];
        self.interpolation_sample = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{goertzel_mag, rms, sine};
    use crate::params::EffectKind;

    const SR: f32 = 48_000.0;

    fn params(rate: f32, depth: f32, feedback: f32, mix: f32) -> ParamVals {
        let mut p = EffectKind::Phaser.default_values();
        for (i, value) in [rate, depth, feedback, mix].into_iter().enumerate() {
            p.v[i] = value;
        }
        p
    }

    fn run(p: &mut Phaser, x: &[Frame], values: &ParamVals) -> Vec<Frame> {
        let mut out = x.to_vec();
        let n = out.len();
        p.process(&mut out, n, values);
        out
    }

    #[test]
    fn static_allpass_cascade_keeps_wet_magnitude_near_unity() {
        for frequency in [100.0, 500.0, 1_000.0, 4_000.0] {
            let input: Vec<Frame> = sine(32_768, frequency, SR, 0.4)
                .into_iter()
                .map(|x| [x, x])
                .collect();
            let mut phaser = Phaser::new();
            phaser.set_rates(SR);
            let out = run(&mut phaser, &input, &params(0.5, 0.0, 0.0, 1.0));
            let left: Vec<f32> = out[4_096..].iter().map(|frame| frame[0]).collect();
            assert!(
                (goertzel_mag(&left, frequency, SR) - 0.4).abs() < 0.02,
                "{frequency} Hz wet magnitude changed"
            );
        }
    }

    #[test]
    fn dry_wet_mix_creates_allpass_notches() {
        let mut smallest_ratio = 1.0f32;
        for frequency in [100.0, 250.0, 500.0, 700.0, 1_000.0, 2_000.0, 4_000.0] {
            let input: Vec<Frame> = sine(32_768, frequency, SR, 0.4)
                .into_iter()
                .map(|x| [x, x])
                .collect();
            let mut phaser = Phaser::new();
            phaser.set_rates(SR);
            let out = run(&mut phaser, &input, &params(0.5, 0.0, 0.0, 0.5));
            let left: Vec<f32> = out[4_096..].iter().map(|frame| frame[0]).collect();
            smallest_ratio = smallest_ratio.min(goertzel_mag(&left, frequency, SR) / 0.4);
        }
        assert!(smallest_ratio < 0.7, "no dry/wet notch: {smallest_ratio}");
    }

    #[test]
    fn modulation_creates_sidebands_at_the_requested_rate() {
        let input: Vec<Frame> = sine(96_000, 1_000.0, SR, 0.4)
            .into_iter()
            .map(|x| [x, x])
            .collect();
        let mut phaser = Phaser::new();
        phaser.set_rates(SR);
        let out = run(&mut phaser, &input, &params(4.0, 1.0, 0.0, 1.0));
        let left: Vec<f32> = out[12_000..].iter().map(|frame| frame[0]).collect();
        for sideband in [996.0, 1_004.0] {
            assert!(
                goertzel_mag(&left, sideband, SR) > 1e-3,
                "no sideband at {sideband} Hz"
            );
        }
    }

    #[test]
    fn feedback_tail_decays() {
        let mut input = vec![[0.0; 2]; 16_384];
        input[0] = [0.4, 0.4];
        let mut without_feedback = Phaser::new();
        without_feedback.set_rates(SR);
        let dry_tail = run(&mut without_feedback, &input, &params(0.5, 0.0, 0.0, 1.0));
        let mut with_feedback = Phaser::new();
        with_feedback.set_rates(SR);
        let feedback_tail = run(&mut with_feedback, &input, &params(0.5, 0.0, 0.7, 1.0));
        let dry_tail: Vec<f32> = dry_tail.iter().map(|frame| frame[0]).collect();
        let feedback_tail: Vec<f32> = feedback_tail.iter().map(|frame| frame[0]).collect();
        assert!(
            rms(&feedback_tail[64..512]) > rms(&dry_tail[64..512]) * 10.0,
            "feedback did not extend the all-pass tail"
        );
        assert!(
            rms(&feedback_tail[8_192..]) < rms(&feedback_tail[..1_024]),
            "feedback tail did not decay"
        );
    }

    #[test]
    fn process_is_block_partition_invariant() {
        let input: Vec<Frame> = sine(1_003, 440.0, SR, 0.4)
            .into_iter()
            .map(|x| [x, -x])
            .collect();
        let values = params(3.0, 0.8, 0.5, 0.65);
        let mut whole = Phaser::new();
        whole.set_rates(SR);
        let expected = run(&mut whole, &input, &values);

        let mut split = Phaser::new();
        split.set_rates(SR);
        let mut actual = input.clone();
        for range in [0..17, 17..256, 256..513, 513..1_003] {
            split.process(&mut actual[range.clone()], range.len(), &values);
        }
        for (a, b) in actual.iter().zip(expected) {
            assert!((a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6);
        }
    }

    #[test]
    fn stereo_is_shared_but_histories_are_isolated() {
        let identical: Vec<Frame> = sine(4_096, 440.0, SR, 0.4)
            .into_iter()
            .map(|x| [x, x])
            .collect();
        let values = params(2.0, 0.9, 0.5, 0.8);
        let mut phaser = Phaser::new();
        phaser.set_rates(SR);
        let out = run(&mut phaser, &identical, &values);
        assert!(out.iter().all(|frame| (frame[0] - frame[1]).abs() < 1e-6));

        let left_only: Vec<Frame> = sine(4_096, 440.0, SR, 0.4)
            .into_iter()
            .map(|x| [x, 0.0])
            .collect();
        let mut phaser = Phaser::new();
        phaser.set_rates(SR);
        let out = run(&mut phaser, &left_only, &values);
        assert!(out.iter().all(|frame| frame[1].abs() < 1e-6));
    }

    #[test]
    fn reset_restores_initial_processor_state() {
        let input: Vec<Frame> = sine(4_096, 440.0, SR, 0.4)
            .into_iter()
            .map(|x| [x, x])
            .collect();
        let values = params(2.0, 0.9, 0.5, 0.8);
        let mut used = Phaser::new();
        used.set_rates(SR);
        let _ = run(&mut used, &input, &values);
        used.reset();
        let after_reset = run(&mut used, &input, &values);

        let mut fresh = Phaser::new();
        fresh.set_rates(SR);
        let expected = run(&mut fresh, &input, &values);
        assert_eq!(after_reset, expected);
    }

    #[test]
    fn invalid_sample_rate_uses_the_supported_minimum() {
        let mut phaser = Phaser::new();
        phaser.set_rates(1.0);
        assert_eq!(phaser.sr, 8_000.0);
    }

    #[test]
    fn junk_and_extreme_rates_stay_finite() {
        for rate in [8_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let mut phaser = Phaser::new();
            phaser.set_rates(rate);
            let mut input = vec![[100.0, -100.0]; 512];
            let junk = params(f32::NAN, f32::INFINITY, -2.0, 5.0);
            let n = input.len();
            phaser.process(&mut input, n, &junk);
            assert!(
                input
                    .iter()
                    .all(|frame| frame[0].is_finite() && frame[1].is_finite()),
                "non-finite output at {rate} Hz"
            );
        }
    }
}
