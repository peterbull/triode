//! Dual-reader granular pitch shifter with a fixed 80 ms history window.

use crate::dsp::biquad::OnePole;
use crate::dsp::delayline::DelayLine;
use crate::dsp::{sanitize, Frame, MAX_SAMPLE_RATE_HZ};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const INTERVAL: usize = 0;
pub const BEND: usize = 1;
pub const MIX: usize = 2;

const DEFAULT_SR: f32 = 48_000.0;
const WINDOW_SECONDS: f64 = 0.080;
const SMOOTH_SECONDS: f64 = 0.005;
const MAX_INPUT: f32 = 8.0;
const MAX_OUTPUT: f32 = 64.0;
const BASE_DELAY: f64 = 2.0;

pub struct PitchShifter {
    sr: f64,
    left: DelayLine,
    right: DelayLine,
    filter: [OnePole; 4],
    phase: f64,
    ratio: f64,
    ratio_initialized: bool,
    valid_history: usize,
    startup_age: usize,
}

impl Default for PitchShifter {
    fn default() -> Self {
        Self::new()
    }
}

impl PitchShifter {
    pub fn new() -> Self {
        let mut shifter = Self {
            sr: f64::from(DEFAULT_SR),
            left: DelayLine::new(
                (WINDOW_SECONDS * f64::from(MAX_SAMPLE_RATE_HZ)).round() as usize + 2,
            ),
            right: DelayLine::new(
                (WINDOW_SECONDS * f64::from(MAX_SAMPLE_RATE_HZ)).round() as usize + 2,
            ),
            filter: [OnePole::new(); 4],
            phase: 0.0,
            ratio: 1.0,
            ratio_initialized: false,
            valid_history: 0,
            startup_age: 0,
        };
        shifter.set_filter_rates();
        shifter
    }

    #[inline]
    fn window(&self) -> f64 {
        (WINDOW_SECONDS * self.sr).round()
    }

    #[inline]
    fn priming_frames(&self) -> usize {
        (BASE_DELAY + self.window()).ceil() as usize
    }

    #[inline]
    fn startup_frames(&self) -> usize {
        (SMOOTH_SECONDS * self.sr).round().max(2.0) as usize
    }

    fn set_filter_rates(&mut self) {
        let cutoff = (0.2 * self.sr).min(8_000.0) as f32;
        for filter in &mut self.filter {
            filter.set_hz(cutoff, self.sr as f32);
        }
    }

    fn invalidate(&mut self) {
        self.phase = 0.0;
        self.ratio = 1.0;
        self.ratio_initialized = false;
        self.valid_history = 0;
        self.startup_age = 0;
        for filter in &mut self.filter {
            filter.reset();
        }
    }

    #[inline]
    fn wrap01(value: f64) -> f64 {
        value.rem_euclid(1.0)
    }

    #[inline]
    fn phase_step(ratio: f64, window: f64) -> f64 {
        (1.0 - ratio) / window
    }

    #[inline]
    fn reader_state(phase: f64, window: f64) -> (f64, f64, f64, f64, f64, f64) {
        let p0 = Self::wrap01(phase);
        let p1 = Self::wrap01(phase + 0.5);
        let d0 = BASE_DELAY + window * p0;
        let d1 = BASE_DELAY + window * p1;
        let e0 = (std::f64::consts::PI * p0).sin();
        let e1 = (std::f64::consts::PI * p1).sin();
        (p0, p1, d0, d1, e0, e1)
    }

    #[inline]
    fn dry(sample: f32) -> f32 {
        if sample.is_finite() {
            sample
        } else {
            0.0
        }
    }

    #[inline]
    fn filtered(&mut self, channel: usize, dry: f32) -> f32 {
        let input = dry.clamp(-MAX_INPUT, MAX_INPUT);
        let first = self.filter[channel * 2].lowpass(input);
        self.filter[channel * 2 + 1].lowpass(first)
    }
}

impl Proc for PitchShifter {
    fn set_rates(&mut self, sr: f32) {
        let effective = f64::from(sanitize(sr, DEFAULT_SR, 8_000.0, MAX_SAMPLE_RATE_HZ));
        if effective == self.sr {
            return;
        }
        self.sr = effective;
        self.set_filter_rates();
        self.invalidate();
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        if n == 0 {
            return;
        }
        let interval = f64::from(sanitize(p.v[INTERVAL], 12.0, -12.0, 12.0));
        let bend = f64::from(sanitize(p.v[BEND], 1.0, 0.0, 1.0));
        let mix = sanitize(p.v[MIX], 1.0, 0.0, 1.0);
        let target_unity = interval == 0.0 || bend == 0.0;
        let target_ratio = 2.0f64.powf(interval * bend / 12.0);
        let smoothing = (-1.0 / (SMOOTH_SECONDS * self.sr)).exp();
        let window = self.window();
        let priming = self.priming_frames();
        let fade_frames = self.startup_frames();

        for frame in buf.iter_mut().take(n) {
            let dry = [Self::dry(frame[0]), Self::dry(frame[1])];
            if self.ratio_initialized {
                self.ratio = target_ratio + smoothing * (self.ratio - target_ratio);
                if (self.ratio - target_ratio).abs() < 1e-9 {
                    self.ratio = target_ratio;
                }
            } else {
                self.ratio = target_ratio;
                self.ratio_initialized = true;
            }

            let (_, _, d0, d1, e0, e1) = Self::reader_state(self.phase, window);
            let shifted = [
                e0 * f64::from(self.left.read(d0 as f32))
                    + e1 * f64::from(self.left.read(d1 as f32)),
                e0 * f64::from(self.right.read(d0 as f32))
                    + e1 * f64::from(self.right.read(d1 as f32)),
            ];
            let filtered = [self.filtered(0, dry[0]), self.filtered(1, dry[1])];
            self.left.write(filtered[0]);
            self.right.write(filtered[1]);
            self.phase = Self::wrap01(self.phase + Self::phase_step(self.ratio, window));

            let delta = 2.0f64.powf(0.1 / 12.0) - 1.0;
            let u = ((self.ratio - 1.0).abs() / delta).clamp(0.0, 1.0);
            let h = u * u * (3.0 - 2.0 * u);
            let granular = [
                ((1.0 - h) * f64::from(dry[0]) + h * shifted[0]) as f32,
                ((1.0 - h) * f64::from(dry[1]) + h * shifted[1]) as f32,
            ];
            let startup_gain = if self.valid_history < priming {
                0.0
            } else if self.startup_age < fade_frames {
                let t = self.startup_age as f64 / (fade_frames - 1) as f64;
                self.startup_age += 1;
                (1.0 - (std::f64::consts::PI * t).cos()) * 0.5
            } else {
                1.0
            };
            let wet = if target_unity {
                dry
            } else {
                [
                    ((1.0 - startup_gain) * f64::from(dry[0])
                        + startup_gain * f64::from(granular[0])) as f32,
                    ((1.0 - startup_gain) * f64::from(dry[1])
                        + startup_gain * f64::from(granular[1])) as f32,
                ]
            };
            self.valid_history = self.valid_history.saturating_add(1).min(priming);

            if mix == 0.0 || target_unity {
                *frame = dry;
            } else {
                frame[0] = (dry[0] * (1.0 - mix) + wet[0] * mix).clamp(-MAX_OUTPUT, MAX_OUTPUT);
                frame[1] = (dry[1] * (1.0 - mix) + wet[1] * mix).clamp(-MAX_OUTPUT, MAX_OUTPUT);
            }
        }
    }

    fn reset(&mut self) {
        self.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{goertzel_exact, peak, sine};

    const SR: f32 = 48_000.0;

    fn params(interval: f32, bend: f32, mix: f32) -> ParamVals {
        let mut p = ParamVals::ZEROED;
        p.v[..3].copy_from_slice(&[interval, bend, mix]);
        p
    }

    fn process(shifter: &mut PitchShifter, input: &[Frame], p: &ParamVals) -> Vec<Frame> {
        let mut output = input.to_vec();
        let n = output.len();
        shifter.process(&mut output, n, p);
        output
    }

    #[test]
    fn reader_coordinates_wrap_with_the_correct_pitch_sign() {
        for (interval, ratio) in [
            (-12.0, 0.5),
            (-7.0, 2f64.powf(-7.0 / 12.0)),
            (-5.0, 2f64.powf(-5.0 / 12.0)),
            (5.0, 2f64.powf(5.0 / 12.0)),
            (7.0, 2f64.powf(7.0 / 12.0)),
            (12.0, 2.0),
        ] {
            let (p0, p1, d0, d1, e0, e1) = PitchShifter::reader_state(-0.25, 3_840.0);
            assert!((p0 - 0.75).abs() < 1e-12, "{interval}");
            assert!((p1 - 0.25).abs() < 1e-12, "{interval}");
            assert!((d0 - 2.0 - 3_840.0 * 0.75).abs() < 1e-12, "{interval}");
            assert!((d1 - 2.0 - 3_840.0 * 0.25).abs() < 1e-12, "{interval}");
            assert!((e0 * e0 + e1 * e1 - 1.0).abs() < 1e-12, "{interval}");
            assert_eq!(
                PitchShifter::phase_step(ratio, 3_840.0).is_sign_positive(),
                interval < 0.0
            );
            let bent_ratio = 2.0f64.powf(interval * 0.37 / 12.0);
            assert_eq!(
                PitchShifter::phase_step(bent_ratio, 3_840.0).is_sign_positive(),
                interval < 0.0
            );
        }
    }

    #[test]
    fn construction_preallocates_the_fixed_80ms_192khz_history() {
        let shifter = PitchShifter::new();
        assert_eq!(shifter.left.capacity(), 16_384);
        assert_eq!(shifter.right.capacity(), 16_384);
    }

    #[test]
    fn reader_envelopes_have_zero_endpoints_and_constant_power() {
        let (_, _, _, _, first, opposite) = PitchShifter::reader_state(0.0, 1.0);
        let (_, _, _, _, last, other) = PitchShifter::reader_state(1.0, 1.0);
        assert!(first.abs() < 1e-12 && last.abs() < 1e-12);
        assert!((opposite * opposite - 1.0).abs() < 1e-12);
        assert!((other * other - 1.0).abs() < 1e-12);
        for phase in [-1.5, -0.25, 0.0, 0.13, 0.5, 0.99, 2.75] {
            let (p0, p1, _, _, e0, e1) = PitchShifter::reader_state(phase, 1.0);
            assert!((p1 - (p0 + 0.5).rem_euclid(1.0)).abs() < 1e-12);
            assert!((e0 * e0 + e1 * e1 - 1.0).abs() < 1e-12);
        }
    }

    #[test]
    fn exact_unity_is_dry_cold_and_after_a_non_unity_target() {
        let input: Vec<Frame> = (0..12_000)
            .map(|i| [i as f32 * 1e-5, -i as f32 * 1e-5])
            .collect();
        for unity in [params(0.0, 1.0, 1.0), params(12.0, 0.0, 1.0)] {
            let mut cold = PitchShifter::new();
            cold.set_rates(SR);
            assert_eq!(process(&mut cold, &input, &unity), input);
            assert_eq!(cold.startup_age, cold.startup_frames());
        }

        for unity in [params(0.0, 1.0, 1.0), params(12.0, 0.0, 1.0)] {
            let mut shifter = PitchShifter::new();
            shifter.set_rates(SR);
            process(&mut shifter, &input, &params(12.0, 1.0, 1.0));
            let ratio = shifter.ratio;
            let phase = shifter.phase;
            assert_eq!(process(&mut shifter, &input[..1], &unity), input[..1]);
            assert!(shifter.ratio > 1.0 && shifter.ratio < ratio);
            assert_ne!(shifter.phase, phase);
            assert_eq!(process(&mut shifter, &input[1..], &unity), input[1..]);
            assert_eq!(shifter.valid_history, shifter.priming_frames());
            assert_eq!(shifter.startup_age, shifter.startup_frames());
        }
    }

    #[test]
    fn startup_uses_the_full_raised_cosine_after_exact_priming() {
        let p = params(12.0, 1.0, 1.0);
        let mut fading = PitchShifter::new();
        let mut full = PitchShifter::new();
        fading.set_rates(SR);
        full.set_rates(SR);
        let prime: Vec<Frame> = (0..fading.priming_frames())
            .map(|i| [0.2 + i as f32 * 1e-5, -0.1 - i as f32 * 1e-5])
            .collect();
        assert_eq!(process(&mut fading, &prime, &p), prime);
        assert_eq!(process(&mut full, &prime, &p), prime);

        let fade = fading.startup_frames();
        assert_eq!(fading.startup_age, 0);
        full.startup_age = fade;
        let input: Vec<Frame> = (0..fade)
            .map(|i| [0.275 + i as f32 * 1e-4, -0.125 + i as f32 * 1e-4])
            .collect();
        let faded = process(&mut fading, &input, &p);
        let granular = process(&mut full, &input, &p);
        for (k, ((faded, granular), dry)) in faded.iter().zip(&granular).zip(&input).enumerate() {
            let q = (1.0 - (std::f64::consts::PI * k as f64 / (fade - 1) as f64).cos()) * 0.5;
            for channel in 0..2 {
                let expected =
                    f64::from(dry[channel]) * (1.0 - q) + f64::from(granular[channel]) * q;
                assert!(
                    (f64::from(faded[channel]) - expected).abs() < 2e-7,
                    "sample {k}, channel {channel}"
                );
            }
        }
        assert_eq!(faded[0], input[0]);
        assert_eq!(faded[fade - 1], granular[fade - 1]);
        assert_eq!(fading.startup_age, fade);
    }

    #[test]
    fn process_advances_ratio_phase_and_wraps_for_bent_positive_and_negative_targets() {
        for (interval, bend, steps) in [(7.0, 1.0, 9_000), (-5.0, 1.0, 17_000), (7.0, 0.37, 25_000)]
        {
            let mut shifter = PitchShifter::new();
            shifter.set_rates(SR);
            process(&mut shifter, &[[0.0; 2]], &params(0.0, 1.0, 0.0));
            let p = params(interval, bend, 0.0);
            let target = 2.0f64.powf(f64::from(interval) * f64::from(bend) / 12.0);
            let smoothing = (-1.0 / (SMOOTH_SECONDS * shifter.sr)).exp();
            let mut expected_ratio = 1.0;
            let mut expected_phase = 0.0;
            let mut previous = 0.0;
            let mut wraps = 0;
            for frame in 0..steps {
                expected_ratio = target + smoothing * (expected_ratio - target);
                if (expected_ratio - target).abs() < 1e-9 {
                    expected_ratio = target;
                }
                let step = PitchShifter::phase_step(expected_ratio, shifter.window());
                expected_phase = PitchShifter::wrap01(expected_phase + step);
                process(&mut shifter, &[[0.0; 2]], &p);
                if (step < 0.0 && shifter.phase > previous)
                    || (step > 0.0 && shifter.phase < previous)
                {
                    wraps += 1;
                }
                assert!((shifter.ratio - expected_ratio).abs() < 1e-12);
                assert!((shifter.phase - expected_phase).abs() < 1e-12);
                if frame == 0 {
                    assert!(shifter.ratio > target.min(1.0) && shifter.ratio < target.max(1.0));
                }
                previous = shifter.phase;
            }
            assert!(wraps > 0, "{interval} semitones, bend {bend}");
        }
    }

    #[test]
    fn zero_mix_is_exact_dry_while_history_advances() {
        let mut shifter = PitchShifter::new();
        shifter.set_rates(SR);
        let input: Vec<Frame> = sine(4_000, 200.0, SR, 0.2)
            .into_iter()
            .map(|x| [x, x])
            .collect();
        assert_eq!(
            process(&mut shifter, &input, &params(12.0, 1.0, 0.0)),
            input
        );
        assert_eq!(shifter.valid_history, shifter.priming_frames());
        let wet = process(&mut shifter, &vec![[0.0; 2]; 500], &params(12.0, 1.0, 1.0));
        assert!(peak(&wet.iter().map(|x| x[0]).collect::<Vec<_>>()) > 0.001);
    }

    #[test]
    fn wet_granular_path_has_no_same_sample_impulse_leak_after_zero_priming() {
        let p = params(12.0, 1.0, 1.0);
        let mut shifter = PitchShifter::new();
        shifter.set_rates(SR);
        let silence = vec![[0.0; 2]; shifter.priming_frames() + shifter.startup_frames()];
        assert_eq!(process(&mut shifter, &silence, &p), silence);

        let mut input = vec![[0.0; 2]; shifter.priming_frames() + 8];
        input[0] = [0.75, -0.25];
        let output = process(&mut shifter, &input, &p);
        assert_eq!(output[0], [0.0; 2]);
        assert!(
            output[1..]
                .iter()
                .any(|frame| frame[0].abs() > 1e-5 || frame[1].abs() > 1e-5),
            "the delayed granular readers must eventually emit the impulse"
        );
    }

    fn shifted_tone(interval: f32, hz: f32) -> Vec<f32> {
        let mut shifter = PitchShifter::new();
        shifter.set_rates(SR);
        let count = shifter.priming_frames() + shifter.startup_frames() + (0.8 * SR) as usize;
        let input: Vec<Frame> = sine(count, hz, SR, 0.2)
            .into_iter()
            .map(|x| [x, x])
            .collect();
        let output = process(&mut shifter, &input, &params(interval, 1.0, 1.0));
        output[shifter.priming_frames() + shifter.startup_frames()..]
            .iter()
            .map(|x| x[0])
            .collect()
    }

    #[test]
    fn sustained_200hz_shifts_both_octaves_over_many_wraps() {
        let up = shifted_tone(12.0, 200.0);
        let down = shifted_tone(-12.0, 200.0);
        assert!(goertzel_exact(&up, 400.0, SR) >= 4.0 * goertzel_exact(&up, 200.0, SR));
        assert!(goertzel_exact(&down, 100.0, SR) >= 4.0 * goertzel_exact(&down, 200.0, SR));
    }

    #[test]
    fn documented_87_point_5hz_artifact_sidebands_remain_visible() {
        let output = shifted_tone(12.0, 87.5);
        let sideband = goertzel_exact(&output, 162.5, SR).max(goertzel_exact(&output, 187.5, SR));
        assert!(sideband >= 4.0 * goertzel_exact(&output, 175.0, SR));
    }

    #[test]
    fn process_filters_high_frequency_audio_before_it_reaches_history() {
        let mut shifter = PitchShifter::new();
        shifter.set_rates(SR);
        let source = sine(8_192, 20_000.0, SR, 0.2);
        let input: Vec<Frame> = source.iter().map(|&x| [x, 0.0]).collect();
        assert_eq!(
            process(&mut shifter, &input, &params(12.0, 1.0, 0.0)),
            input
        );

        let history: Vec<f32> = (2..2_050)
            .map(|delay| shifter.left.read(delay as f32))
            .collect();
        let raw: Vec<f32> = (2..2_050)
            .map(|delay| source[source.len() - delay])
            .collect();
        assert!(goertzel_exact(&history, 20_000.0, SR) < goertzel_exact(&raw, 20_000.0, SR) * 0.3);
    }

    #[test]
    fn reset_and_actual_rate_change_invalidate_wet_history_and_recursive_state() {
        let wet = params(12.0, 1.0, 1.0);
        let seed: Vec<Frame> = sine(5_000, 200.0, SR, 0.5)
            .into_iter()
            .map(|x| [x, -x])
            .collect();
        let mut reset = PitchShifter::new();
        reset.set_rates(SR);
        process(&mut reset, &seed, &wet);
        reset.reset();
        let silence = vec![[0.0; 2]; reset.priming_frames() + reset.startup_frames() + 64];
        assert!(process(&mut reset, &silence, &wet)
            .iter()
            .all(|frame| *frame == [0.0; 2]));

        let mut rate_changed = PitchShifter::new();
        rate_changed.set_rates(SR);
        process(&mut rate_changed, &seed, &wet);
        rate_changed.set_rates(44_100.0);
        assert_eq!(rate_changed.sr, 44_100.0);
        let silence =
            vec![[0.0; 2]; rate_changed.priming_frames() + rate_changed.startup_frames() + 64];
        assert!(process(&mut rate_changed, &silence, &wet)
            .iter()
            .all(|frame| *frame == [0.0; 2]));

        let mut shifter = PitchShifter::new();
        shifter.set_rates(SR);
        process(&mut shifter, &[[1.0, -0.5]; 64], &params(12.0, 1.0, 0.0));
        let silence =
            vec![
                [0.0; 2];
                shifter.left.capacity() + shifter.priming_frames() + shifter.startup_frames() + 64
            ];
        process(&mut shifter, &silence, &params(12.0, 1.0, 0.0));
        assert!(shifter.filter.iter().all(|filter| filter.state() == 0.0));
        assert!((0..shifter.left.capacity())
            .all(|delay| !shifter.left.read(delay as f32).is_subnormal()));
        assert!((0..shifter.right.capacity())
            .all(|delay| !shifter.right.read(delay as f32).is_subnormal()));
    }

    #[test]
    fn irregular_partitions_match_parameter_segments_including_unity() {
        let input: Vec<Frame> = (0..9_000)
            .map(|i| {
                let phase = std::f32::consts::TAU * 200.0 * i as f32 / SR;
                [phase.sin() * 0.2, phase.cos() * 0.1]
            })
            .collect();
        let segments = [
            (3_000, params(12.0, 1.0, 0.6)),
            (3_000, params(-7.0, 0.37, 0.6)),
            (3_000, params(0.0, 1.0, 0.6)),
        ];
        let mut whole = PitchShifter::new();
        let mut split = PitchShifter::new();
        whole.set_rates(SR);
        split.set_rates(SR);
        let mut expected = input.clone();
        let mut offset = 0;
        for (length, p) in &segments {
            whole.process(&mut expected[offset..offset + length], *length, p);
            offset += length;
        }

        let mut actual = input.clone();
        offset = 0;
        for (length, p) in &segments {
            let end = offset + length;
            for chunk in [17, 113, 29, 251, 7, 64].iter().cycle() {
                if offset == end {
                    break;
                }
                let next = (offset + chunk).min(end);
                split.process(&mut actual[offset..next], next - offset, p);
                offset = next;
            }
        }
        assert_eq!(actual, expected);
        assert_eq!(&actual[6_000..], &input[6_000..]);
    }

    #[test]
    fn malformed_hot_and_subnormal_inputs_stay_finite_and_recover() {
        let mut shifter = PitchShifter::new();
        shifter.set_rates(f32::INFINITY);
        let mut bad = ParamVals::ZEROED;
        bad.v[..3].copy_from_slice(&[f32::NAN, f32::INFINITY, f32::NAN]);
        let mut input = vec![[1e20, -1e20]; 10_000];
        input[0] = [f32::NAN, f32::INFINITY];
        input[1] = [f32::from_bits(1), -f32::from_bits(1)];
        let output = process(&mut shifter, &input, &bad);
        assert!(output
            .iter()
            .flatten()
            .all(|x| x.is_finite() && x.abs() <= MAX_OUTPUT));
        shifter.reset();
        let clean = process(&mut shifter, &[[0.125, -0.25]; 4], &params(0.0, 1.0, 1.0));
        assert_eq!(clean, [[0.125, -0.25]; 4]);
    }
}
