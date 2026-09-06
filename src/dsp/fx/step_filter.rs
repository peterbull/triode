use crate::dsp::svf::Svf;
use crate::dsp::{sanitize, Frame, MAX_SAMPLE_RATE_HZ};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const FREQUENCY: usize = 0;
pub const Q: usize = 1;
pub const SPEED: usize = 2;
pub const STEPS: usize = 3;
pub const RANDOM: usize = 4;
pub const MIX: usize = 5;

const PATTERN: [f32; 9] = [0.0, 1.0, 0.25, 0.75, 0.125, 0.625, 0.375, 0.875, 0.5];
const PRNG_SEED: u32 = 0x4D59_5DF4;
const MAX_AUDIO: f32 = 8.0;
const MAX_STATE: f32 = 64.0;

pub struct StepFilter {
    sr: f32,
    phase: f64,
    phase_error: f64,
    index: usize,
    prng: u32,
    filters: [Svf; 2],
    g: f32,
    g_step: f32,
    ramp_remaining: usize,
    initialized: bool,
}

impl Default for StepFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl StepFilter {
    pub fn new() -> Self {
        Self {
            sr: 48_000.0,
            phase: 0.0,
            phase_error: 0.0,
            index: 0,
            prng: PRNG_SEED,
            filters: [Svf::default(); 2],
            g: 0.0,
            g_step: 0.0,
            ramp_remaining: 0,
            initialized: false,
        }
    }

    fn destination_g(&self, frequency: f32, index: usize) -> f32 {
        let cutoff =
            (frequency * 2.0f32.powf(3.0 * (PATTERN[index] - 0.5))).clamp(20.0, 0.45 * self.sr);
        sanitize(
            (std::f32::consts::PI * cutoff / self.sr).tan(),
            0.0,
            0.0,
            8.0,
        )
    }

    fn advance_step(&mut self, frequency: f32, steps: usize, random: bool) {
        self.index %= steps;
        self.prng = self
            .prng
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        self.index = if random {
            self.prng as usize % steps
        } else {
            (self.index + 1) % steps
        };

        let target = self.destination_g(frequency, self.index);
        let ramp_samples = (0.003 * self.sr).round().max(1.0) as usize;
        self.g_step = sanitize((target - self.g) / ramp_samples as f32, 0.0, -8.0, 8.0);
        self.ramp_remaining = ramp_samples;
    }
}

impl Proc for StepFilter {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sanitize(sr, 48_000.0, 8_000.0, MAX_SAMPLE_RATE_HZ);
        self.reset();
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let frequency = sanitize(p.v[FREQUENCY], 800.0, 150.0, 3_000.0);
        let q = sanitize(p.v[Q], 1.5, 0.5, 4.0);
        let speed = sanitize(p.v[SPEED], 4.0, 0.5, 16.0);
        let steps = sanitize(p.v[STEPS], 6.0, 2.0, 9.0).round() as usize;
        let random = sanitize(p.v[RANDOM], 0.0, 0.0, 1.0) >= 0.5;
        let mix = sanitize(p.v[MIX], 0.7, 0.0, 1.0);
        let phase_increment = speed as f64 / self.sr as f64;
        let k = 1.0 / q;

        for frame in buf.iter_mut().take(n) {
            if !self.initialized {
                self.g = self.destination_g(frequency, self.index);
                self.initialized = true;
            }

            let corrected_increment = phase_increment - self.phase_error;
            let next_phase = self.phase + corrected_increment;
            self.phase_error = (next_phase - self.phase) - corrected_increment;
            self.phase = next_phase;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
                self.advance_step(frequency, steps, random);
            }

            if self.ramp_remaining > 0 {
                self.g = sanitize(self.g + self.g_step, 0.0, 0.0, 8.0);
                self.ramp_remaining -= 1;
            }

            let dry_l = if frame[0].is_finite() { frame[0] } else { 0.0 };
            let dry_r = if frame[1].is_finite() { frame[1] } else { 0.0 };
            let wet_l = self.filters[0].process(dry_l.clamp(-MAX_AUDIO, MAX_AUDIO), self.g, k);
            let wet_r = self.filters[1].process(dry_r.clamp(-MAX_AUDIO, MAX_AUDIO), self.g, k);
            if mix == 0.0 {
                frame[0] = dry_l;
                frame[1] = dry_r;
            } else {
                frame[0] = sanitize(
                    dry_l * (1.0 - mix) + wet_l * mix,
                    0.0,
                    -MAX_STATE,
                    MAX_STATE,
                );
                frame[1] = sanitize(
                    dry_r * (1.0 - mix) + wet_r * mix,
                    0.0,
                    -MAX_STATE,
                    MAX_STATE,
                );
            }
        }
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        self.phase_error = 0.0;
        self.index = 0;
        self.prng = PRNG_SEED;
        self.filters = [Svf::default(); 2];
        self.g = 0.0;
        self.g_step = 0.0;
        self.ramp_remaining = 0;
        self.initialized = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{goertzel_mag, sine};
    use crate::params::EffectKind;

    const SR: f32 = 48_000.0;
    const PATTERN: [f32; 9] = [0.0, 1.0, 0.25, 0.75, 0.125, 0.625, 0.375, 0.875, 0.5];

    fn params(frequency: f32, q: f32, speed: f32, steps: f32, random: f32, mix: f32) -> ParamVals {
        let mut p = EffectKind::StepFilter.default_values();
        for (i, value) in [frequency, q, speed, steps, random, mix]
            .into_iter()
            .enumerate()
        {
            p.v[i] = value;
        }
        p
    }

    fn run(filter: &mut StepFilter, input: &[Frame], values: &ParamVals) -> Vec<Frame> {
        let mut output = input.to_vec();
        let n = output.len();
        filter.process(&mut output, n, values);
        output
    }

    fn mono(input: &[f32]) -> Vec<Frame> {
        input.iter().map(|&sample| [sample, sample]).collect()
    }

    fn cutoff(base: f32, destination: usize, sr: f32) -> f32 {
        (base * 2.0f32.powf(3.0 * (PATTERN[destination] - 0.5))).clamp(20.0, 0.45 * sr)
    }

    fn target_g(base: f32, destination: usize, sr: f32) -> f32 {
        (std::f32::consts::PI * cutoff(base, destination, sr) / sr).tan()
    }

    fn impulse_band(g: f32, q: f32) -> f32 {
        let k = 1.0 / q;
        let a1 = 1.0 / (1.0 + g * (g + k));
        g * a1 * k
    }

    fn boundary_probe(steps: f32, random: f32, boundary: usize) -> f32 {
        let speed = 16.0;
        let sr = 8_000.0;
        let period = (sr / speed) as usize;
        let mut filter = StepFilter::new();
        filter.set_rates(sr);
        let dry = params(800.0, 1.5, speed, steps, random, 0.0);
        let mut silence = vec![[0.0; 2]; boundary * period - 1];
        let n = silence.len();
        filter.process(&mut silence, n, &dry);
        let mut pulse = [[1.0, 1.0]];
        filter.process(
            &mut pulse,
            1,
            &params(800.0, 1.5, speed, steps, random, 1.0),
        );
        pulse[0][0]
    }

    fn expected_boundary_probe(previous: usize, destination: usize) -> f32 {
        let sr = 8_000.0;
        let ramp = (0.003_f32 * sr).round() as usize;
        let previous = target_g(800.0, previous, sr);
        let target = target_g(800.0, destination, sr);
        impulse_band(previous + (target - previous) / ramp as f32, 1.5)
    }

    #[test]
    fn initial_sample_uses_destination_zero_without_a_silent_wait() {
        let mut filter = StepFilter::new();
        filter.set_rates(SR);
        let output = run(
            &mut filter,
            &[[1.0, 1.0]],
            &params(800.0, 1.5, 0.5, 6.0, 0.0, 1.0),
        );

        assert!(output[0][0] > 0.0, "first step started as a zero-Hz mute");
        assert_eq!(output[0][0], output[0][1]);
    }

    #[test]
    fn empty_process_does_not_capture_stale_initial_frequency() {
        let mut filter = StepFilter::new();
        filter.set_rates(SR);
        let stale = params(150.0, 1.5, 0.5, 6.0, 0.0, 1.0);
        filter.process(&mut [], 0, &stale);

        let current = params(3_000.0, 1.5, 0.5, 6.0, 0.0, 1.0);
        let output = run(&mut filter, &[[1.0, 1.0]], &current);
        let expected = impulse_band(target_g(3_000.0, 0, SR), 1.5);
        assert!((output[0][0] - expected).abs() < 1e-6);
    }

    #[test]
    fn step_boundaries_land_on_the_exact_sample_at_supported_rates() {
        for (sr, speed) in [
            (8_000.0, 16.0),
            (44_100.0, 3.0),
            (48_000.0, 4.0),
            (192_000.0, 16.0),
        ] {
            let period = (sr / speed) as usize;
            let mut filter = StepFilter::new();
            filter.set_rates(sr);
            let values = params(800.0, 1.5, speed, 6.0, 0.0, 1.0);
            let mut before = vec![[0.0, 0.0]; period - 1];
            filter.process(&mut before, period - 1, &values);
            assert_eq!(filter.index, 0, "early boundary at {sr} Hz");
            filter.process(&mut [[0.0, 0.0]], 1, &values);
            assert_eq!(filter.index, 1, "late boundary at {sr} Hz");
        }
    }

    #[test]
    fn non_integral_step_period_does_not_accumulate_integer_truncation_drift() {
        let sr = 44_100.0;
        let speed = 7.3;
        let values = params(800.0, 1.5, speed, 9.0, 0.0, 0.0);
        let mut filter = StepFilter::new();
        filter.set_rates(sr);
        let mut boundaries = Vec::new();
        let mut previous = filter.index;
        for sample in 1..=31_000 {
            filter.process(&mut [[0.0, 0.0]], 1, &values);
            if filter.index != previous {
                boundaries.push(sample);
                previous = filter.index;
                if boundaries.len() == 5 {
                    break;
                }
            }
        }
        let expected: Vec<_> = (1..=5)
            .map(|step| (step as f64 * sr as f64 / speed as f64).ceil() as usize)
            .collect();
        assert_eq!(boundaries, expected);
    }

    #[test]
    fn repeat_length_rounds_to_two_and_nine_steps_and_wraps() {
        let two = boundary_probe(2.49, 0.0, 2);
        let expected_two = expected_boundary_probe(1, 0);
        assert!(
            (two - expected_two).abs() < 1e-6,
            "two-step wrap: {two} != {expected_two}"
        );

        let nine = boundary_probe(8.5, 0.0, 9);
        let expected_nine = expected_boundary_probe(8, 0);
        assert!(
            (nine - expected_nine).abs() < 1e-6,
            "nine-step wrap: {nine} != {expected_nine}"
        );
    }

    #[test]
    fn random_is_binary_and_uses_the_fixed_lcg_sequence() {
        let repeat = boundary_probe(6.0, 0.499, 3);
        let expected_repeat = expected_boundary_probe(2, 3);
        assert!((repeat - expected_repeat).abs() < 1e-6);

        // The specified seed and LCG constants produce destinations 1, 0, 5 modulo six.
        let random = boundary_probe(6.0, 0.5, 3);
        let expected_random = expected_boundary_probe(0, 5);
        assert!((random - expected_random).abs() < 1e-6);
        assert!((random - repeat).abs() > 1e-3);
    }

    #[test]
    fn lcg_advances_once_per_boundary_in_repeat_and_random_modes() {
        let speed = 16.0;
        let sr = 8_000.0;
        let period = (sr / speed) as usize;
        let ramp = (0.003_f32 * sr).round() as usize;
        let mut switched = StepFilter::new();
        switched.set_rates(sr);
        let mut silence = vec![[0.0; 2]; 2 * period];
        let n = silence.len();
        switched.process(&mut silence, n, &params(800.0, 1.5, speed, 6.0, 0.0, 0.0));
        let mut silence = vec![[0.0; 2]; period + ramp - 1];
        let n = silence.len();
        switched.process(&mut silence, n, &params(800.0, 1.5, speed, 6.0, 1.0, 0.0));
        let switched = run(
            &mut switched,
            &[[1.0, 1.0]],
            &params(800.0, 1.5, speed, 6.0, 1.0, 1.0),
        );

        let mut random = StepFilter::new();
        random.set_rates(sr);
        let mut silence = vec![[0.0; 2]; 3 * period + ramp - 1];
        let n = silence.len();
        random.process(&mut silence, n, &params(800.0, 1.5, speed, 6.0, 1.0, 0.0));
        let random = run(
            &mut random,
            &[[1.0, 1.0]],
            &params(800.0, 1.5, speed, 6.0, 1.0, 1.0),
        );

        assert!((switched[0][0] - random[0][0]).abs() < 1e-6);
        assert!((switched[0][0] - impulse_band(target_g(800.0, 5, sr), 1.5)).abs() < 1e-6);
    }

    fn response(input_hz: f32, q: f32) -> f32 {
        let speed = 0.5;
        let period = (SR / speed) as usize;
        let values = params(800.0, q, speed, 6.0, 0.0, 1.0);
        let mut filter = StepFilter::new();
        filter.set_rates(SR);
        let mut silence = vec![[0.0; 2]; period];
        filter.process(&mut silence, period, &values);
        let output = run(
            &mut filter,
            &mono(&sine(32_768, input_hz, SR, 0.4)),
            &values,
        );
        let left: Vec<f32> = output[4_096..].iter().map(|frame| frame[0]).collect();
        goertzel_mag(&left, input_hz, SR)
    }

    #[test]
    fn held_destination_has_the_expected_band_pass_center_and_q_bandwidth() {
        let center = cutoff(800.0, 1, SR);
        let centered = response(center, 1.5);
        assert!(centered > response(center * 0.5, 1.5) * 2.0);
        assert!(centered > response(center * 2.0, 1.5) * 2.0);

        let shoulder = center * 0.75;
        assert!(response(shoulder, 0.5) > response(shoulder, 4.0) * 2.0);
    }

    fn first_ramp_probe(offset: usize, partitioned: bool) -> f32 {
        let speed = 16.0;
        let sr = 8_000.0;
        let period = (sr / speed) as usize;
        let mut filter = StepFilter::new();
        filter.set_rates(sr);
        let values = params(800.0, 1.5, speed, 6.0, 0.0, 1.0);
        let mut silence = vec![[0.0; 2]; period - 1 + offset];
        if partitioned {
            for chunk in silence.chunks_mut(7) {
                filter.process(chunk, chunk.len(), &values);
            }
        } else {
            let n = silence.len();
            filter.process(&mut silence, n, &values);
        }
        run(&mut filter, &[[1.0, 1.0]], &values)[0][0]
    }

    #[test]
    fn coefficient_ramp_is_exactly_three_ms_and_persists_across_callbacks() {
        let ramp = (0.003_f32 * 8_000.0).round() as usize;
        let previous = target_g(800.0, 0, 8_000.0);
        let target = target_g(800.0, 1, 8_000.0);
        let first = previous + (target - previous) / ramp as f32;
        assert!((first_ramp_probe(0, false) - impulse_band(first, 1.5)).abs() < 1e-6);
        assert!((first_ramp_probe(ramp - 1, false) - impulse_band(target, 1.5)).abs() < 1e-6);
        assert_eq!(
            first_ramp_probe(ramp - 1, false),
            first_ramp_probe(ramp - 1, true)
        );
    }

    #[test]
    fn processing_is_callback_partition_invariant() {
        let input = mono(&sine(25_003, 730.0, SR, 0.5));
        let values = params(900.0, 2.0, 4.0, 7.0, 1.0, 0.65);
        let mut whole = StepFilter::new();
        whole.set_rates(SR);
        let expected = run(&mut whole, &input, &values);

        let mut split = StepFilter::new();
        split.set_rates(SR);
        let mut actual = input.clone();
        let mut start = 0;
        for size in [1, 7, 31, 256, 3, 1024].into_iter().cycle() {
            if start == actual.len() {
                break;
            }
            let end = (start + size).min(actual.len());
            split.process(&mut actual[start..end], end - start, &values);
            start = end;
        }
        assert_eq!(actual, expected);
    }

    #[test]
    fn modulation_is_linked_but_channel_audio_never_leaks() {
        let input = mono(&sine(16_384, 700.0, SR, 0.4));
        let values = params(800.0, 1.5, 8.0, 6.0, 1.0, 1.0);
        let mut linked = StepFilter::new();
        linked.set_rates(SR);
        assert!(run(&mut linked, &input, &values)
            .iter()
            .all(|frame| frame[0] == frame[1]));

        let left_only: Vec<Frame> = input.iter().map(|frame| [frame[0], 0.0]).collect();
        let mut isolated = StepFilter::new();
        isolated.set_rates(SR);
        assert!(run(&mut isolated, &left_only, &values)
            .iter()
            .all(|frame| frame[1] == 0.0));
    }

    #[test]
    fn zero_mix_is_bit_exact_while_filter_and_sequence_state_advance() {
        let input = mono(&sine(16_384, 500.0, SR, 0.7));
        let mut dry = input.clone();
        dry[0] = [12.0, -12.0];
        let mut zero_mix = StepFilter::new();
        zero_mix.set_rates(SR);
        assert_eq!(
            run(&mut zero_mix, &dry, &params(800.0, 1.5, 8.0, 6.0, 0.0, 0.0)),
            dry
        );

        let mut wet_while_advancing = StepFilter::new();
        wet_while_advancing.set_rates(SR);
        let _ = run(
            &mut wet_while_advancing,
            &dry,
            &params(800.0, 1.5, 8.0, 6.0, 0.0, 1.0),
        );
        let probe = mono(&sine(1024, 900.0, SR, 0.4));
        let after_dry = run(
            &mut zero_mix,
            &probe,
            &params(800.0, 1.5, 8.0, 6.0, 0.0, 1.0),
        );
        let after_wet = run(
            &mut wet_while_advancing,
            &probe,
            &params(800.0, 1.5, 8.0, 6.0, 0.0, 1.0),
        );
        assert_eq!(after_dry, after_wet);
        assert_ne!(after_dry, probe);
    }

    #[test]
    fn reset_and_rate_changes_restore_deterministic_initial_state() {
        let input = mono(&sine(16_384, 900.0, SR, 0.5));
        let values = params(800.0, 1.5, 8.0, 6.0, 1.0, 1.0);
        let mut used = StepFilter::new();
        used.set_rates(SR);
        let _ = run(&mut used, &input, &values);
        used.reset();
        let after_reset = run(&mut used, &input, &values);
        let mut fresh = StepFilter::new();
        fresh.set_rates(SR);
        assert_eq!(after_reset, run(&mut fresh, &input, &values));

        used.set_rates(8_000.0);
        fresh.set_rates(8_000.0);
        let short = &input[..2_000];
        assert_eq!(
            run(&mut used, short, &values),
            run(&mut fresh, short, &values)
        );
        used.set_rates(1.0);
        fresh.set_rates(8_000.0);
        assert_eq!(
            run(&mut used, short, &values),
            run(&mut fresh, short, &values)
        );
        assert_ne!(after_reset, input);
    }

    #[test]
    fn malformed_controls_and_non_finite_audio_recover_without_poisoning_state() {
        let mut filter = StepFilter::new();
        filter.set_rates(f32::NAN);
        let mut malformed = params(
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            f32::INFINITY,
            f32::NAN,
        );
        malformed.v[3] = -100.0;
        let junk = run(
            &mut filter,
            &vec![[f32::NAN, f32::INFINITY]; 2_048],
            &malformed,
        );
        assert!(junk.iter().flatten().all(|sample| sample.is_finite()));

        let sane = run(
            &mut filter,
            &mono(&sine(16_384, 700.0, SR, 0.5)),
            &params(800.0, 1.5, 8.0, 9.0, 1.0, 1.0),
        );
        assert!(sane
            .iter()
            .flatten()
            .all(|sample| sample.is_finite() && sample.abs() < 64.0));
        assert!(sane.iter().flatten().any(|sample| sample.abs() > 1e-5));
    }
}
