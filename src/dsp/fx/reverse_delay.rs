//! Bounded overlapping reverse-grain delay.

use crate::dsp::delayline::DelayLine;
use crate::dsp::{sanitize, Frame, MAX_SAMPLE_RATE_HZ};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const WINDOW: usize = 0;
pub const FEEDBACK: usize = 1;
pub const MIX: usize = 2;

const DEFAULT_SR: f32 = 48_000.0;
const MIN_WINDOW_MS: f32 = 40.0;
const MAX_WINDOW_MS: f32 = 2_000.0;
const MAX_FEEDBACK: f32 = 0.85;
const MAX_INPUT: f32 = 8.0;
const MAX_OUTPUT: f32 = 64.0;
const OVERLAP_SECONDS: f32 = 0.005;
// 2 * 2000 ms at 192 kHz. DelayLine rounds this to 1,048,576 samples.
const HISTORY_FRAMES: usize = 768_000;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Grain {
    n: usize,
    f: usize,
    start: u64,
    age: usize,
}

pub struct ReverseDelay {
    sr: f32,
    left: DelayLine,
    right: DelayLine,
    grains: [Option<Grain>; 2],
    valid_history: usize,
    sample: u64,
}

impl Default for ReverseDelay {
    fn default() -> Self {
        Self::new()
    }
}

impl ReverseDelay {
    pub fn new() -> Self {
        Self {
            sr: DEFAULT_SR,
            left: DelayLine::new(HISTORY_FRAMES),
            right: DelayLine::new(HISTORY_FRAMES),
            grains: [None; 2],
            valid_history: 0,
            sample: 0,
        }
    }

    #[inline]
    fn requested_window(&self, window_ms: f32) -> (usize, usize) {
        let n = (window_ms * self.sr / 1_000.0).round() as usize;
        let f = ((OVERLAP_SECONDS * self.sr).round() as usize).max(2);
        (n, f)
    }

    #[inline]
    fn launch(&mut self, slot: usize, n: usize, f: usize) {
        self.grains[slot] = Some(Grain {
            n,
            f,
            start: self.sample,
            age: 0,
        });
    }

    #[inline]
    fn schedule(&mut self, n: usize, f: usize) {
        match self.grains {
            [None, _] if self.valid_history >= n => self.launch(0, n, f),
            [Some(old), None] if old.age == old.n - old.f && self.valid_history >= n => {
                self.launch(1, n, f)
            }
            _ => {}
        }
    }

    #[inline]
    fn gains(&self) -> [f32; 2] {
        match self.grains {
            [Some(old), Some(new)]
                if old.age >= old.n - old.f && new.age < new.f && old.f == new.f =>
            {
                let q = edge(new.age, new.f);
                [1.0 - q, q]
            }
            [first, second] => [
                first.map_or(0.0, grain_gain),
                second.map_or(0.0, grain_gain),
            ],
        }
    }

    #[inline]
    fn wet(&self, delay: &DelayLine, gains: [f32; 2]) -> f32 {
        let mut wet = 0.0;
        for (grain, gain) in self.grains.into_iter().zip(gains) {
            if let Some(grain) = grain {
                wet += gain * delay.read((1 + 2 * grain.age) as f32);
            }
        }
        sanitize(wet, 0.0, -MAX_INPUT, MAX_INPUT)
    }

    #[inline]
    fn advance(&mut self) {
        self.valid_history = self.valid_history.saturating_add(1);
        self.sample = self.sample.wrapping_add(1);
        for grain in self.grains.iter_mut().flatten() {
            grain.age = self.sample.wrapping_sub(grain.start) as usize;
        }
        if self.grains[0].is_some_and(|grain| grain.age >= grain.n) {
            self.grains[0] = self.grains[1].take();
        }
        if self.grains[1].is_some_and(|grain| grain.age >= grain.n) {
            self.grains[1] = None;
        }
    }

    #[inline]
    fn invalidate(&mut self) {
        self.grains = [None; 2];
        self.valid_history = 0;
        self.sample = 0;
    }
}

#[inline]
fn edge(age: usize, frames: usize) -> f32 {
    let u = age as f32 / (frames - 1) as f32;
    (1.0 - (std::f32::consts::PI * u).cos()) * 0.5
}

#[inline]
fn grain_gain(grain: Grain) -> f32 {
    if grain.age < grain.f {
        edge(grain.age, grain.f)
    } else if grain.age < grain.n - grain.f {
        1.0
    } else {
        1.0 - edge(grain.age - (grain.n - grain.f), grain.f)
    }
}

impl Proc for ReverseDelay {
    fn set_rates(&mut self, sr: f32) {
        let sr = sanitize(sr, DEFAULT_SR, 8_000.0, MAX_SAMPLE_RATE_HZ);
        if self.sr != sr {
            self.sr = sr;
            self.invalidate();
        }
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let window_ms = sanitize(p.v[WINDOW], 500.0, MIN_WINDOW_MS, MAX_WINDOW_MS);
        let feedback = sanitize(p.v[FEEDBACK], 0.25, 0.0, MAX_FEEDBACK);
        let mix = sanitize(p.v[MIX], 0.5, 0.0, 1.0);
        let (window, overlap) = self.requested_window(window_ms);

        for frame in buf.iter_mut().take(n) {
            self.schedule(window, overlap);
            let gains = self.gains();
            // All readers run before either channel advances its write head.
            let wet_left = self.wet(&self.left, gains);
            let wet_right = self.wet(&self.right, gains);
            let dry_left = if frame[0].is_finite() { frame[0] } else { 0.0 };
            let dry_right = if frame[1].is_finite() { frame[1] } else { 0.0 };

            // Complementary gains sum to at most one, so feedback < 1 bounds the
            // unclamped recurrence by MAX_INPUT / (1 - feedback) before this clamp.
            self.left.write(
                (dry_left.clamp(-MAX_INPUT, MAX_INPUT) + feedback * wet_left)
                    .clamp(-MAX_INPUT, MAX_INPUT),
            );
            self.right.write(
                (dry_right.clamp(-MAX_INPUT, MAX_INPUT) + feedback * wet_right)
                    .clamp(-MAX_INPUT, MAX_INPUT),
            );

            if mix == 0.0 {
                *frame = [dry_left, dry_right];
            } else {
                frame[0] = (dry_left * (1.0 - mix) + wet_left * mix).clamp(-MAX_OUTPUT, MAX_OUTPUT);
                frame[1] =
                    (dry_right * (1.0 - mix) + wet_right * mix).clamp(-MAX_OUTPUT, MAX_OUTPUT);
            }
            self.advance();
        }
    }

    fn reset(&mut self) {
        self.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 8_000.0;

    fn params(window: f32, feedback: f32, mix: f32) -> ParamVals {
        let mut values = ParamVals::ZEROED;
        values.v[..3].copy_from_slice(&[window, feedback, mix]);
        values
    }

    fn run(delay: &mut ReverseDelay, input: &[[f32; 2]], values: &ParamVals) -> Vec<Frame> {
        let mut output = input.to_vec();
        let n = output.len();
        delay.process(&mut output, n, values);
        output
    }

    fn edge_oracle(age: usize, f: usize) -> f32 {
        (1.0 - (std::f32::consts::PI * age as f32 / (f - 1) as f32).cos()) * 0.5
    }

    fn gain_oracle(age: usize, n: usize, f: usize) -> f32 {
        if age < f {
            edge_oracle(age, f)
        } else if age < n - f {
            1.0
        } else {
            1.0 - edge_oracle(age - (n - f), f)
        }
    }

    fn peak(values: &[Frame]) -> f32 {
        values
            .iter()
            .flat_map(|frame| frame.iter())
            .fold(0.0f32, |peak, value| peak.max(value.abs()))
    }

    #[test]
    fn pre_overlap_and_overlap_follow_the_exact_reverse_oracle() {
        let (n, f) = (320, 40);
        let h = n - f;
        let values = params(40.0, 0.0, 1.0);
        let mut input: Vec<Frame> = (0..n + h + f)
            .map(|i| [i as f32 * 0.01 + 0.1, -(i as f32 * 0.01 + 0.2)])
            .collect();
        input[n - 1 - 120] = [0.731, -0.413];
        let marker_overlap = 17;
        input[n - 1 - (h + marker_overlap)] = [0.613, -0.271];
        input[n + h - 1 - marker_overlap] = [-0.457, 0.829];
        let mut delay = ReverseDelay::new();
        delay.set_rates(SR);
        let output = run(&mut delay, &input, &values);

        for j in 0..h {
            let gain = gain_oracle(j, n, f);
            for channel in 0..2 {
                assert!(
                    (output[n + j][channel] - gain * input[n - 1 - j][channel]).abs() < 2e-5,
                    "pre-overlap j={j}, channel={channel}"
                );
            }
        }
        for k in 0..f {
            let q = edge_oracle(k, f);
            for channel in 0..2 {
                let want =
                    (1.0 - q) * input[n - 1 - (h + k)][channel] + q * input[n + h - 1 - k][channel];
                assert!(
                    (output[n + h + k][channel] - want).abs() < 3e-5,
                    "overlap k={k}, channel={channel}"
                );
            }
        }
    }

    #[test]
    fn startup_is_silent_until_the_first_window_and_overlaps_are_constant() {
        let (n, f) = (320, 40);
        let h = n - f;
        let values = params(40.0, 0.0, 1.0);
        let input = vec![[0.5, -0.25]; n + h + f + 2];
        let mut delay = ReverseDelay::new();
        delay.set_rates(SR);
        let output = run(&mut delay, &input, &values);

        assert!(output[..=n].iter().all(|frame| *frame == [0.0, 0.0]));
        assert!(output[n + 1][0] > 0.0);
        for frame in &output[n + h..n + h + f] {
            assert!((frame[0] - 0.5).abs() < 2e-6);
            assert!((frame[1] + 0.25).abs() < 2e-6);
        }
    }

    #[test]
    fn maximum_window_at_192k_reads_a_real_marker_backward() {
        let sr = 192_000.0;
        let n = 384_000;
        let f = 960;
        let j = f / 2;
        let mut input = vec![[0.0; 2]; n + j + 1];
        input[n - 1 - j] = [0.75, -0.375];
        let mut delay = ReverseDelay::new();
        delay.set_rates(sr);
        let output = run(&mut delay, &input, &params(2_000.0, 0.0, 1.0));
        let gain = edge_oracle(j, f);
        assert!((output[n + j][0] - 0.75 * gain).abs() < 2e-5);
        assert!((output[n + j][1] + 0.375 * gain).abs() < 2e-5);
        assert_eq!(delay.left.capacity(), 1_048_576);
        assert_eq!(delay.right.capacity(), 1_048_576);
    }

    #[test]
    fn feedback_is_bounded_before_the_limiter_and_decays_after_input_stops() {
        let values = params(40.0, 0.85, 1.0);
        let n = 320;
        let mut delay = ReverseDelay::new();
        delay.set_rates(SR);
        let mut input = vec![[0.01; 2]; n * 5];
        input.extend(vec![[0.0; 2]; n * 5]);
        let output = run(&mut delay, &input, &values);
        let bound = 0.01 / (1.0 - MAX_FEEDBACK);
        let mut state_peak = 0.0f32;
        for frames in 1..=delay.valid_history {
            state_peak = state_peak.max(delay.left.read(frames as f32).abs());
            state_peak = state_peak.max(delay.right.read(frames as f32).abs());
        }
        assert!(state_peak < bound + 1e-4, "state peak {state_peak}");
        let stop = n * 5;
        let bands: Vec<f32> = (0..4)
            .map(|band| peak(&output[stop + band * n..stop + (band + 1) * n]))
            .collect();
        assert!(
            bands.windows(2).all(|pair| pair[1] <= pair[0] + 2e-5),
            "{bands:?}"
        );
    }

    #[test]
    fn constant_feedback_approaches_its_bound_then_decays_by_window() {
        let n = 320;
        let values = params(40.0, MAX_FEEDBACK, 1.0);
        let bound = 0.01 / (1.0 - MAX_FEEDBACK);
        let mut delay = ReverseDelay::new();
        delay.set_rates(SR);
        let _ = run(&mut delay, &vec![[0.01; 2]; 30 * n - 40], &values);

        let state_peak = (1..=delay.valid_history)
            .map(|frames| delay.left.read(frames as f32).abs())
            .fold(0.0f32, f32::max);
        assert!(state_peak < bound, "state peak {state_peak}, bound {bound}");
        assert!(
            state_peak > bound * 0.99,
            "state did not approach bound: {state_peak}, bound {bound}"
        );

        let tail = run(&mut delay, &vec![[0.0; 2]; 15 * n], &values);
        let peaks: Vec<f32> = (0..3)
            .map(|window| {
                let start = window * 7 * n;
                peak(&tail[start..start + n])
            })
            .collect();
        assert!(peaks[0] > 0.0, "{peaks:?}");
        assert!(peaks.windows(2).all(|pair| pair[1] < pair[0]), "{peaks:?}");
    }

    #[test]
    fn window_changes_latch_descriptors_without_stale_reads() {
        let short = params(40.0, 0.0, 1.0);
        let long = params(100.0, 0.0, 1.0);
        let source: Vec<Frame> = (0..2_200)
            .map(|i| {
                [
                    ((i % 37) as f32 - 18.0) / 19.0,
                    ((i % 29) as f32 - 14.0) / 17.0,
                ]
            })
            .collect();

        let mut reference = ReverseDelay::new();
        let mut changed = ReverseDelay::new();
        reference.set_rates(SR);
        changed.set_rates(SR);
        let baseline = run(&mut reference, &source[..400], &short);
        let first = run(&mut changed, &source[..321], &short);
        let switched = run(&mut changed, &source[321..400], &long);
        assert_eq!(&baseline[..321], &first);
        assert_eq!(&baseline[321..], &switched, "active short grain changed");
        assert_eq!(changed.grains[0].unwrap().n, 320);

        let mut overlap_reference = ReverseDelay::new();
        let mut overlap_changed = ReverseDelay::new();
        overlap_reference.set_rates(SR);
        overlap_changed.set_rates(SR);
        let _ = run(&mut overlap_reference, &source[..601], &short);
        let _ = run(&mut overlap_changed, &source[..601], &short);
        assert_eq!(overlap_changed.grains[1].unwrap().n, 320);
        assert_eq!(
            run(&mut overlap_reference, &source[601..641], &short),
            run(&mut overlap_changed, &source[601..641], &long),
            "a window change during overlap changed latched grains"
        );

        let mut before_launch = ReverseDelay::new();
        before_launch.set_rates(SR);
        let _ = run(&mut before_launch, &source[..600], &short);
        let _ = run(&mut before_launch, &source[600..601], &long);
        assert!(
            before_launch.grains[1].is_none(),
            "unavailable long request launched"
        );
        let recovered = run(&mut before_launch, &source[601..], &short);
        assert!(recovered
            .iter()
            .all(|frame| frame.iter().all(|sample| sample.is_finite())));

        let mut long_reference = ReverseDelay::new();
        let mut long_changed = ReverseDelay::new();
        long_reference.set_rates(SR);
        long_changed.set_rates(SR);
        let baseline = run(&mut long_reference, &source[..900], &long);
        let first = run(&mut long_changed, &source[..801], &long);
        let switched = run(&mut long_changed, &source[801..900], &short);
        assert_eq!(&baseline[..801], &first);
        assert_eq!(&baseline[801..], &switched, "active long grain changed");
        assert_eq!(long_changed.grains[0].unwrap().n, 800);
    }

    #[test]
    fn unavailable_long_window_at_short_overlap_restarts_on_completion() {
        let (n, f) = (320, 40);
        let short = params(40.0, 0.0, 1.0);
        let long = params(100.0, 0.0, 1.0);
        let mut input = vec![[0.0; 2]; 681];
        input[599] = [0.625, -0.3125];
        let mut delay = ReverseDelay::new();
        delay.set_rates(SR);

        let _ = run(&mut delay, &input[..600], &short);
        let _ = run(&mut delay, &input[600..601], &long);
        let _ = run(&mut delay, &input[601..640], &short);
        assert_eq!(delay.grains, [None; 2]);

        let _ = run(&mut delay, &input[640..641], &short);
        assert_eq!(
            delay.grains[0],
            Some(Grain {
                n,
                f,
                start: 640,
                age: 1,
            })
        );
        let recovered = run(&mut delay, &input[641..], &short);
        assert_eq!(recovered[39], input[599]);
    }

    #[test]
    fn ring_wrap_during_overlap_keeps_complementary_stereo_markers_exact() {
        let (n, f) = (323, 40);
        let h = n - f;
        let values = params(n as f32 * 1_000.0 / SR, 0.0, 1.0);
        let mut delay = ReverseDelay::new();
        delay.set_rates(SR);
        let capacity = delay.left.capacity();
        let first_overlap = 2 * n - f;
        let overlap = first_overlap + (capacity - first_overlap) / h * h;
        let k = capacity - overlap;
        assert!(k < f && overlap + k == capacity);

        let outgoing = [0.625, -0.3125];
        let incoming = [-0.4375, 0.78125];
        let mut input = vec![[0.0; 2]; overlap + f];
        input[overlap - 1 - 2 * h - k] = outgoing;
        input[overlap - 1 - k] = incoming;
        let output = run(&mut delay, &input, &values);

        let q = edge_oracle(k, f);
        for channel in 0..2 {
            let want = (1.0 - q) * outgoing[channel] + q * incoming[channel];
            assert!(
                (output[overlap + k][channel] - want).abs() < 2e-6,
                "channel={channel}"
            );
        }
    }

    #[test]
    fn ring_wrap_preserves_a_reverse_marker() {
        let n = 320;
        let f = 40;
        let h = n - f;
        let j = f;
        let mut delay = ReverseDelay::new();
        delay.set_rates(SR);
        let capacity = delay.left.capacity();
        let start = n + ((capacity - n) / h + 1) * h;
        let mut input = vec![[0.0; 2]; start + j + 1];
        input[start - 1 - j] = [0.625, -0.3125];
        let output = run(&mut delay, &input, &params(40.0, 0.0, 1.0));
        assert!(start > capacity);
        assert!((output[start + j][0] - 0.625).abs() < 2e-6);
        assert!((output[start + j][1] + 0.3125).abs() < 2e-6);
    }

    #[test]
    fn rates_reset_zero_frames_and_mix_zero_have_the_lifecycle_contract() {
        let values = params(40.0, 0.0, 1.0);
        let mut delay = ReverseDelay::new();
        delay.set_rates(f32::NAN);
        assert_eq!(delay.sr, DEFAULT_SR);
        delay.set_rates(-1.0);
        assert_eq!(delay.sr, 8_000.0);
        delay.set_rates(MAX_SAMPLE_RATE_HZ * 2.0);
        assert_eq!(delay.sr, MAX_SAMPLE_RATE_HZ);
        delay.set_rates(8_000.0);
        let mut empty = [];
        delay.process(&mut empty, 0, &values);
        assert_eq!(delay.valid_history, 0);
        let input = vec![[0.25, -0.125]; 321];
        let _ = run(&mut delay, &input, &values);
        let valid = delay.valid_history;
        delay.set_rates(8_000.0);
        assert_eq!(
            delay.valid_history, valid,
            "same effective rate must be a no-op"
        );
        delay.set_rates(44_100.0);
        assert_eq!(delay.valid_history, 0);
        assert_eq!(delay.grains, [None; 2]);

        let mut dry_delay = ReverseDelay::new();
        dry_delay.set_rates(SR);
        let dry_values = params(40.0, 0.0, 0.0);
        let dry = [[f32::from_bits(1), -3.0], [f32::NAN, f32::INFINITY]];
        let out = run(&mut dry_delay, &dry, &dry_values);
        assert_eq!(out[0], dry[0]);
        assert_eq!(out[1], [0.0, 0.0]);
        assert_eq!(dry_delay.valid_history, dry.len());
    }

    #[test]
    fn reset_partitioning_and_stereo_are_deterministic() {
        let values = params(40.0, 0.2, 1.0);
        let input: Vec<Frame> = (0..1_024)
            .map(|i| [if i % 31 == 0 { 0.4 } else { 0.0 }, 0.0])
            .collect();
        let mut whole = ReverseDelay::new();
        let mut split = ReverseDelay::new();
        whole.set_rates(SR);
        split.set_rates(SR);
        let full = run(&mut whole, &input, &values);
        let mut partitioned = run(&mut split, &input[..333], &values);
        partitioned.extend(run(&mut split, &input[333..711], &values));
        partitioned.extend(run(&mut split, &input[711..], &values));
        assert_eq!(full, partitioned);
        assert!(full.iter().all(|frame| frame[1] == 0.0));

        let mut used = ReverseDelay::new();
        let mut fresh = ReverseDelay::new();
        used.set_rates(SR);
        fresh.set_rates(SR);
        let _ = run(&mut used, &input, &values);
        used.reset();
        assert_eq!(
            run(&mut used, &input, &values),
            run(&mut fresh, &input, &values)
        );
    }

    #[test]
    fn partitioning_is_identical_for_denormalized_short_long_short_timeline() {
        let short = params(40.0, f32::from_bits(1), 1.0);
        let long = params(100.0, f32::from_bits(1), 1.0);
        let boundaries = [719, 1_837, 2_756];
        let input: Vec<Frame> = (0..boundaries[2])
            .map(|i| {
                [
                    if i % 19 == 0 {
                        f32::from_bits(1)
                    } else {
                        (i % 23) as f32 * 0.01
                    },
                    if i % 31 == 0 {
                        -f32::from_bits(1)
                    } else {
                        -((i % 29) as f32) * 0.01
                    },
                ]
            })
            .collect();

        let mut whole = ReverseDelay::new();
        let mut split = ReverseDelay::new();
        whole.set_rates(SR);
        split.set_rates(SR);
        let mut full = run(&mut whole, &input[..boundaries[0]], &short);
        full.extend(run(&mut whole, &input[boundaries[0]..boundaries[1]], &long));
        full.extend(run(&mut whole, &input[boundaries[1]..], &short));

        let mut partitioned = run(&mut split, &input[..73], &short);
        partitioned.extend(run(&mut split, &input[73..451], &short));
        partitioned.extend(run(&mut split, &input[451..boundaries[0]], &short));
        partitioned.extend(run(&mut split, &input[boundaries[0]..1_003], &long));
        partitioned.extend(run(&mut split, &input[1_003..1_511], &long));
        partitioned.extend(run(&mut split, &input[1_511..boundaries[1]], &long));
        partitioned.extend(run(&mut split, &input[boundaries[1]..2_041], &short));
        partitioned.extend(run(&mut split, &input[2_041..2_388], &short));
        partitioned.extend(run(&mut split, &input[2_388..], &short));
        assert_eq!(full, partitioned);
    }

    #[test]
    fn malformed_hot_and_subnormal_signals_recover_to_finite_audio() {
        let mut delay = ReverseDelay::new();
        delay.set_rates(f32::INFINITY);
        let mut malformed = ParamVals::ZEROED;
        malformed.v[..3].copy_from_slice(&[f32::NAN, f32::INFINITY, f32::NEG_INFINITY]);
        let mut input = vec![
            [f32::NAN, f32::INFINITY],
            [f32::NEG_INFINITY, f32::from_bits(1)],
        ];
        input.extend(vec![[1e30, -1e30]; 1_024]);
        let output = run(&mut delay, &input, &malformed);
        assert!(output.iter().all(|frame| frame
            .iter()
            .all(|sample| sample.is_finite() && sample.abs() <= MAX_OUTPUT)));
        delay.reset();
        let silence = run(&mut delay, &vec![[0.0; 2]; 640], &params(40.0, 0.0, 1.0));
        assert!(silence.iter().all(|frame| *frame == [0.0, 0.0]));
    }
}
