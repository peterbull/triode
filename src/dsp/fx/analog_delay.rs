//! Same-time stereo delay with modulation and coloration inside each feedback loop.

use crate::dsp::biquad::OnePole;
use crate::dsp::delayline::DelayLine;
use crate::dsp::{sanitize, tau_coef, Frame, MAX_SAMPLE_RATE_HZ};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const TIME: usize = 0;
pub const FEEDBACK: usize = 1;
pub const TONE: usize = 2;
pub const RATE: usize = 3;
pub const DEPTH: usize = 4;
pub const MIX: usize = 5;

const DEFAULT_SR: f32 = 48_000.0;
const MAX_DELAY_MS: f32 = 608.0;
const MAX_INPUT: f32 = 8.0;
const MAX_OUTPUT: f32 = 64.0;

pub struct AnalogDelay {
    sr: f32,
    left: DelayLine,
    right: DelayLine,
    tone_left: OnePole,
    tone_right: OnePole,
    modulation_phase: f64,
    smoothed_frames: f32,
    smoothing_initialized: bool,
}

impl Default for AnalogDelay {
    fn default() -> Self {
        Self::new()
    }
}

impl AnalogDelay {
    pub fn new() -> Self {
        let capacity = (MAX_DELAY_MS * 0.001 * MAX_SAMPLE_RATE_HZ).ceil() as usize;
        Self {
            sr: DEFAULT_SR,
            left: DelayLine::new(capacity),
            right: DelayLine::new(capacity),
            tone_left: OnePole::new(),
            tone_right: OnePole::new(),
            modulation_phase: 0.0,
            smoothed_frames: 0.0,
            smoothing_initialized: false,
        }
    }
}

impl Proc for AnalogDelay {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sanitize(sr, DEFAULT_SR, 8_000.0, MAX_SAMPLE_RATE_HZ);
        self.smoothing_initialized = false;
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let time_ms = sanitize(p.v[TIME], 350.0, 20.0, 600.0);
        let feedback = sanitize(p.v[FEEDBACK], 0.45, 0.0, 0.9);
        let max_tone = (0.45 * self.sr).min(8_000.0);
        let tone_hz = sanitize(p.v[TONE], 3_500.0, 500.0, max_tone);
        let rate_hz = sanitize(p.v[RATE], 0.6, 0.05, 8.0);
        let depth = sanitize(p.v[DEPTH], 0.35, 0.0, 1.0);
        let mix_level = sanitize(p.v[MIX], 0.35, 0.0, 1.0);
        let target_frames = time_ms * 0.001 * self.sr;
        let smoothing = tau_coef(0.005, self.sr);
        let max_tap = (self.left.capacity() - 2) as f32;

        self.tone_left.set_hz(tone_hz, self.sr);
        self.tone_right.set_hz(tone_hz, self.sr);
        let phase_increment = rate_hz as f64 / self.sr as f64;

        for frame in buf.iter_mut().take(n) {
            let dry_left = if frame[0].is_finite() { frame[0] } else { 0.0 };
            let dry_right = if frame[1].is_finite() { frame[1] } else { 0.0 };
            if self.smoothing_initialized {
                self.smoothed_frames =
                    target_frames + smoothing * (self.smoothed_frames - target_frames);
            } else {
                self.smoothed_frames = target_frames;
                self.smoothing_initialized = true;
            }

            let excursion =
                (0.008 * self.sr * depth).min(0.8 * (self.smoothed_frames - 1.0).max(0.0));
            self.modulation_phase += phase_increment;
            if self.modulation_phase >= 1.0 {
                self.modulation_phase -= 1.0;
            }
            let lfo = (std::f64::consts::TAU * self.modulation_phase).sin() as f32;
            let tap_frames = (self.smoothed_frames + excursion * lfo).clamp(1.0, max_tap);
            let tap_left = sanitize(self.left.read(tap_frames), 0.0, -MAX_INPUT, MAX_INPUT);
            let tap_right = sanitize(self.right.read(tap_frames), 0.0, -MAX_INPUT, MAX_INPUT);
            let repeat_left = self.tone_left.lowpass(tap_left).tanh();
            let repeat_right = self.tone_right.lowpass(tap_right).tanh();
            let safe_left = dry_left.clamp(-MAX_INPUT, MAX_INPUT);
            let safe_right = dry_right.clamp(-MAX_INPUT, MAX_INPUT);

            self.left.write(sanitize(
                safe_left + repeat_left * feedback,
                0.0,
                -MAX_INPUT,
                MAX_INPUT,
            ));
            self.right.write(sanitize(
                safe_right + repeat_right * feedback,
                0.0,
                -MAX_INPUT,
                MAX_INPUT,
            ));

            if mix_level == 0.0 {
                *frame = [dry_left, dry_right];
            } else {
                frame[0] = sanitize(
                    dry_left * (1.0 - mix_level) + tap_left * mix_level,
                    0.0,
                    -MAX_OUTPUT,
                    MAX_OUTPUT,
                );
                frame[1] = sanitize(
                    dry_right * (1.0 - mix_level) + tap_right * mix_level,
                    0.0,
                    -MAX_OUTPUT,
                    MAX_OUTPUT,
                );
            }
        }
    }

    fn reset(&mut self) {
        self.left.reset();
        self.right.reset();
        self.tone_left.reset();
        self.tone_right.reset();
        self.modulation_phase = 0.0;
        self.smoothed_frames = 0.0;
        self.smoothing_initialized = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{goertzel_exact, peak, sine};

    const SR: f32 = 48_000.0;

    fn params(
        time_ms: f32,
        feedback: f32,
        tone_hz: f32,
        rate_hz: f32,
        depth: f32,
        mix: f32,
    ) -> ParamVals {
        let mut p = ParamVals::ZEROED;
        p.v[..6].copy_from_slice(&[time_ms, feedback, tone_hz, rate_hz, depth, mix]);
        p
    }

    fn process(delay: &mut AnalogDelay, input: &[Frame], p: &ParamVals) -> Vec<Frame> {
        let mut output = input.to_vec();
        let n = output.len();
        delay.process(&mut output, n, p);
        output
    }

    fn impulse_response(delay: &mut AnalogDelay, sr: f32, time_ms: f32) -> Vec<Frame> {
        delay.set_rates(sr);
        let effective_sr = if sr.is_finite() {
            sr.clamp(8_000.0, 192_000.0)
        } else {
            SR
        };
        let at = (time_ms * effective_sr / 1_000.0) as usize;
        let mut input = vec![[0.0; 2]; at + 4];
        input[0] = [0.8, -0.4];
        process(delay, &input, &params(time_ms, 0.0, 8_000.0, 0.6, 0.0, 1.0))
    }

    #[test]
    fn cold_and_post_reset_depth_zero_timing_is_exact() {
        let mut delay = AnalogDelay::new();
        for pass in 0..2 {
            let output = impulse_response(&mut delay, SR, 100.0);
            assert!(output[..4_800].iter().all(|frame| *frame == [0.0; 2]));
            assert_eq!(output[4_800], [0.8, -0.4], "pass {pass}");
            assert!(output[4_801..].iter().all(|frame| *frame == [0.0; 2]));
            delay.reset();
        }
    }

    #[test]
    fn delay_time_reaches_63_point_2_percent_after_five_ms() {
        const SCALE: f32 = 0.000_05;
        let mut delay = AnalogDelay::new();
        delay.set_rates(SR);

        let prime: Vec<Frame> = (0..12_000)
            .map(|i| [i as f32 * SCALE, i as f32 * SCALE])
            .collect();
        process(
            &mut delay,
            &prime,
            &params(100.0, 0.0, 8_000.0, 0.6, 0.0, 0.0),
        );

        let transition: Vec<Frame> = (12_000..12_240)
            .map(|i| [i as f32 * SCALE, i as f32 * SCALE])
            .collect();
        let output = process(
            &mut delay,
            &transition,
            &params(200.0, 0.0, 8_000.0, 0.6, 0.0, 1.0),
        );
        let global_frame = 12_239.0;
        let measured_delay = global_frame - output[239][0] / SCALE;
        let moved = (measured_delay - 4_800.0) / 4_800.0;

        assert!((moved - (1.0 - (-1.0f32).exp())).abs() < 0.002, "{moved}");
    }

    #[test]
    fn each_feedback_repeat_is_progressively_darker() {
        let burst_len = 1_920;
        let mut input = vec![[0.0; 2]; 12_000];
        for (i, frame) in input[..burst_len].iter_mut().enumerate() {
            let t = i as f32 / SR;
            let sample = 0.01 * (2.0 * std::f32::consts::PI * 250.0 * t).sin()
                + 0.01 * (2.0 * std::f32::consts::PI * 4_000.0 * t).sin();
            *frame = [sample, sample];
        }

        let mut delay = AnalogDelay::new();
        delay.set_rates(SR);
        let output = process(
            &mut delay,
            &input,
            &params(100.0, 0.8, 1_000.0, 0.6, 0.0, 1.0),
        );
        let left: Vec<_> = output.iter().map(|frame| frame[0]).collect();
        let first = &left[4_800..4_800 + burst_len];
        let second = &left[9_600..9_600 + burst_len];
        let first_ratio = goertzel_exact(first, 4_000.0, SR) / goertzel_exact(first, 250.0, SR);
        let second_ratio = goertzel_exact(second, 4_000.0, SR) / goertzel_exact(second, 250.0, SR);

        assert!(
            second_ratio < first_ratio * 0.5,
            "{first_ratio} -> {second_ratio}"
        );
    }

    #[test]
    fn modulation_creates_sidebands_that_depth_zero_does_not() {
        let input: Vec<Frame> = sine(96_000, 440.0, SR, 0.02)
            .into_iter()
            .map(|sample| [sample, sample])
            .collect();
        let render = |depth| {
            let mut delay = AnalogDelay::new();
            delay.set_rates(SR);
            let output = process(
                &mut delay,
                &input,
                &params(100.0, 0.8, 8_000.0, 8.0, depth, 1.0),
            );
            output[48_000..]
                .iter()
                .map(|frame| frame[0])
                .collect::<Vec<_>>()
        };
        let fixed = render(0.0);
        let modulated = render(1.0);
        let sidebands =
            |signal: &[f32]| goertzel_exact(signal, 432.0, SR) + goertzel_exact(signal, 448.0, SR);
        let fixed_sidebands = sidebands(&fixed);
        let modulated_sidebands = sidebands(&modulated);

        assert!(
            modulated_sidebands > 0.001 && modulated_sidebands > fixed_sidebands * 5.0,
            "fixed {fixed_sidebands}, modulated {modulated_sidebands}"
        );
    }

    #[test]
    fn modulation_is_stored_inside_the_feedback_loop() {
        let render = |depth| {
            let mut input = vec![[0.0; 2]; 12_000];
            for (i, frame) in input[..4_800].iter_mut().enumerate() {
                let sample = 0.02 * (std::f32::consts::TAU * 440.0 * i as f32 / SR).sin();
                *frame = [sample, sample];
            }
            let mut delay = AnalogDelay::new();
            delay.set_rates(SR);
            let _ = process(
                &mut delay,
                &input,
                &params(20.0, 0.9, 8_000.0, 8.0, depth, 0.0),
            );
            process(
                &mut delay,
                &vec![[0.0; 2]; 12_000],
                &params(20.0, 0.9, 8_000.0, 8.0, 0.0, 1.0),
            )
            .into_iter()
            .map(|frame| frame[0])
            .collect::<Vec<_>>()
        };

        let fixed_history = render(0.0);
        let modulated_history = render(1.0);
        let fixed_sidebands =
            goertzel_exact(&fixed_history, 432.0, SR) + goertzel_exact(&fixed_history, 448.0, SR);
        let modulated_sidebands = goertzel_exact(&modulated_history, 432.0, SR)
            + goertzel_exact(&modulated_history, 448.0, SR);
        let difference_rms = (fixed_history
            .iter()
            .zip(&modulated_history)
            .map(|(fixed, modulated)| (fixed - modulated).powi(2))
            .sum::<f32>()
            / fixed_history.len() as f32)
            .sqrt();

        assert!(
            modulated_sidebands > fixed_sidebands * 1.1,
            "feedback sidebands fixed={fixed_sidebands}, modulated={modulated_sidebands}"
        );
        assert!(
            difference_rms > 1e-4,
            "feedback histories were identical: rms={difference_rms}"
        );
    }

    #[test]
    fn low_level_max_feedback_decays_without_internal_runaway() {
        let delay_frames: usize = 960;
        let mut input = vec![[0.0; 2]; 30_000];
        input[0] = [0.01, 0.01];
        let mut delay = AnalogDelay::new();
        delay.set_rates(SR);
        let output = process(
            &mut delay,
            &input,
            &params(20.0, 0.9, 8_000.0, 0.6, 0.0, 1.0),
        );
        let left: Vec<_> = output.iter().map(|frame| frame[0]).collect();
        let repeats: Vec<_> = (1..=12)
            .map(|repeat| {
                let center = repeat * delay_frames;
                peak(&left[center.saturating_sub(32)..center + 64])
            })
            .collect();

        assert!(repeats[0] > 0.009);
        assert!(
            repeats.windows(2).all(|pair| pair[1] < pair[0]),
            "{repeats:?}"
        );
        assert!(repeats[11] < repeats[0] * 0.1, "{repeats:?}");
        assert!(left.iter().all(|sample| sample.is_finite()));
        assert!(peak(&left) < 0.011);
    }

    #[test]
    fn maximum_modulated_excursion_fits_at_192khz() {
        const HIGH_SR: f32 = 192_000.0;
        let want = (0.608 * HIGH_SR) as usize;
        let rate = 0.25 * HIGH_SR / (want + 1) as f32;
        let mut input = vec![[0.0; 2]; want + 4];
        input[0] = [0.8, 0.8];
        let mut delay = AnalogDelay::new();
        delay.set_rates(HIGH_SR);
        let output = process(
            &mut delay,
            &input,
            &params(600.0, 0.0, 8_000.0, rate, 1.0, 1.0),
        );

        assert!(output[want - 2..=want + 2]
            .iter()
            .any(|frame| frame[0] > 0.5));
        assert!(output[..want - 2].iter().all(|frame| frame[0].abs() < 1e-5));
    }

    #[test]
    fn zero_mix_is_exact_while_wet_state_keeps_advancing() {
        let p_dry = params(100.0, 0.5, 3_500.0, 0.6, 0.0, 0.0);
        let p_wet = params(100.0, 0.5, 3_500.0, 0.6, 0.0, 1.0);
        let mut first = vec![[0.0; 2]; 2_400];
        first[0] = [0.75, -0.25];
        let mut delay = AnalogDelay::new();
        delay.set_rates(SR);
        let dry = process(&mut delay, &first, &p_dry);
        assert_eq!(dry, first);

        let wet = process(&mut delay, &vec![[0.0; 2]; 2_401], &p_wet);
        assert!(wet[..2_400].iter().all(|frame| *frame == [0.0; 2]));
        assert_eq!(wet[2_400], [0.75, -0.25]);
    }

    #[test]
    fn processing_is_partition_invariant() {
        let input: Vec<Frame> = (0..12_345)
            .map(|i| {
                let t = i as f32 / SR;
                [
                    0.2 * (2.0 * std::f32::consts::PI * 330.0 * t).sin(),
                    0.15 * (2.0 * std::f32::consts::PI * 517.0 * t).sin(),
                ]
            })
            .collect();
        let p = params(37.0, 0.72, 2_100.0, 3.7, 0.8, 0.6);
        let mut whole = AnalogDelay::new();
        let mut split = AnalogDelay::new();
        whole.set_rates(SR);
        split.set_rates(SR);
        let expected = process(&mut whole, &input, &p);
        let mut actual = input.clone();
        for chunk in actual.chunks_mut(37) {
            split.process(chunk, chunk.len(), &p);
        }

        assert_eq!(actual, expected);
        assert_ne!(expected, input, "partition test must exercise the wet path");
    }

    #[test]
    fn stereo_lines_are_independent_and_same_time() {
        let mut input = vec![[0.0; 2]; 4_804];
        input[0] = [0.8, 0.0];
        let mut delay = AnalogDelay::new();
        let output = impulse_response(&mut delay, SR, 100.0);
        assert_eq!(output[4_800], [0.8, -0.4]);

        let mut left_only = AnalogDelay::new();
        left_only.set_rates(SR);
        let output = process(
            &mut left_only,
            &input,
            &params(100.0, 0.8, 3_500.0, 0.6, 0.0, 1.0),
        );
        assert!(output.iter().all(|frame| frame[1] == 0.0));
        assert_eq!(output[4_800][0], 0.8);
    }

    #[test]
    fn slowest_lfo_rate_is_accurate_at_the_highest_sample_rate() {
        let sr = MAX_SAMPLE_RATE_HZ;
        let rate = 0.05;
        let samples = (sr / rate) as usize;
        let values = params(20.0, 0.0, 8_000.0, rate, 1.0, 0.0);
        let mut delay = AnalogDelay::new();
        delay.set_rates(sr);
        let mut silence = [[0.0; 2]; 512];
        for _ in 0..samples / silence.len() {
            delay.process(&mut silence, 512, &values);
        }

        assert!(
            delay.modulation_phase < 1e-4,
            "slow LFO drifted to phase {} after one expected cycle",
            delay.modulation_phase
        );
    }

    #[test]
    fn rate_change_preserves_history_and_modulation_phase() {
        let mut delay = AnalogDelay::new();
        delay.set_rates(SR);
        let mut first = vec![[0.0; 2]; 2_400];
        first[0] = [0.7, 0.7];
        process(
            &mut delay,
            &first,
            &params(100.0, 0.0, 8_000.0, 1.0, 0.0, 1.0),
        );
        let phase_before_rate_change = delay.modulation_phase;
        delay.set_rates(96_000.0);
        assert_eq!(delay.modulation_phase, phase_before_rate_change);
        let after = process(
            &mut delay,
            &vec![[0.0; 2]; 7_201],
            &params(100.0, 0.0, 8_000.0, 1.0, 0.0, 1.0),
        );
        assert!(after[..7_200].iter().all(|frame| *frame == [0.0; 2]));
        assert_eq!(after[7_200], [0.7, 0.7]);

        let prime: Vec<Frame> = sine(6_000, 440.0, SR, 0.2)
            .into_iter()
            .map(|sample| [sample, sample])
            .collect();
        let continuation: Vec<Frame> = (6_000..10_000)
            .map(|i| {
                let sample = 0.2 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / SR).sin();
                [sample, sample]
            })
            .collect();
        let p = params(20.0, 0.0, 8_000.0, 8.0, 1.0, 1.0);
        let mut untouched = AnalogDelay::new();
        let mut retuned = AnalogDelay::new();
        untouched.set_rates(SR);
        retuned.set_rates(SR);
        process(&mut untouched, &prime, &p);
        process(&mut retuned, &prime, &p);
        retuned.set_rates(SR);
        assert_eq!(
            process(&mut untouched, &continuation, &p),
            process(&mut retuned, &continuation, &p)
        );
    }

    #[test]
    fn invalid_rates_are_sanitized_deterministically() {
        let mut low = AnalogDelay::new();
        let output = impulse_response(&mut low, 1.0, 100.0);
        assert_eq!(output[800], [0.8, -0.4]);

        let mut malformed = AnalogDelay::new();
        let output = impulse_response(&mut malformed, f32::NAN, 100.0);
        assert_eq!(output[4_800], [0.8, -0.4]);
    }

    #[test]
    fn malformed_controls_and_hot_input_recover_after_reset() {
        let mut delay = AnalogDelay::new();
        delay.set_rates(f32::INFINITY);
        let mut junk = ParamVals::ZEROED;
        junk.v[..6].copy_from_slice(&[
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            f32::INFINITY,
            f32::NAN,
        ]);
        let mut input = vec![[1.0e20, -1.0e20]; 8_192];
        input[0] = [f32::NAN, f32::INFINITY];
        input[1] = [f32::MAX, -f32::MAX];
        let output = process(&mut delay, &input, &junk);
        assert!(output
            .iter()
            .flatten()
            .all(|sample| sample.is_finite() && sample.abs() <= 64.0));

        delay.reset();
        let output = impulse_response(&mut delay, SR, 100.0);
        assert_eq!(output[4_800], [0.8, -0.4]);
        assert!(output[..4_800].iter().all(|frame| *frame == [0.0; 2]));
    }
}
