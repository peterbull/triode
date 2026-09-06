//! Stereo flanger: short, independently modulated feedback delays.

use crate::dsp::delayline::DelayLine;
use crate::dsp::lfo::Lfo;
use crate::dsp::{sanitize, Frame, MAX_SAMPLE_RATE_HZ};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const RATE: usize = 0;
pub const DEPTH: usize = 1;
pub const BASE: usize = 2;
pub const FEEDBACK: usize = 3;
pub const MIX: usize = 4;

const DEFAULT_SR: f32 = 48_000.0;
const MAX_BASE_MS: f32 = 5.0;
const MAX_DELAY_MULTIPLIER: f32 = 1.9;
const MAX_AUDIO: f32 = 8.0;
const MAX_STATE: f32 = 64.0;

pub struct Flanger {
    sr: f32,
    lfo_l: Lfo,
    lfo_r: Lfo,
    dl_l: DelayLine,
    dl_r: DelayLine,
}

impl Default for Flanger {
    fn default() -> Self {
        Self::new()
    }
}

impl Flanger {
    pub fn new() -> Self {
        // DelayLine reserves its own two-frame interpolation guard.
        let max_delay =
            (MAX_BASE_MS * MAX_SAMPLE_RATE_HZ * MAX_DELAY_MULTIPLIER / 1_000.0).ceil() as usize;
        let mut lfo_r = Lfo::new();
        lfo_r.set_phase(0.25);
        Self {
            sr: DEFAULT_SR,
            lfo_l: Lfo::new(),
            lfo_r,
            dl_l: DelayLine::new(max_delay),
            dl_r: DelayLine::new(max_delay),
        }
    }

    #[inline]
    fn delay(base_frames: f32, depth: f32, lfo: f32) -> f32 {
        let delay = base_frames * (1.0 + 0.9 * depth * lfo);
        if delay.is_finite() {
            delay.max(1.0)
        } else {
            1.0
        }
    }
}

impl Proc for Flanger {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sanitize(sr, DEFAULT_SR, 8_000.0, MAX_SAMPLE_RATE_HZ);
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let rate = sanitize(p.v[RATE], 0.25, 0.05, 5.0);
        let depth = sanitize(p.v[DEPTH], 0.7, 0.0, 1.0);
        let base_ms = sanitize(p.v[BASE], 2.0, 0.5, MAX_BASE_MS);
        let feedback = sanitize(p.v[FEEDBACK], 0.35, -0.85, 0.85);
        let mix = sanitize(p.v[MIX], 0.5, 0.0, 1.0);
        let base_frames = base_ms * self.sr / 1_000.0;

        self.lfo_l.set_rate(rate, self.sr);
        self.lfo_r.set_rate(rate, self.sr);

        for frame in buf[..n].iter_mut() {
            let dry_l = if frame[0].is_finite() { frame[0] } else { 0.0 };
            let dry_r = if frame[1].is_finite() { frame[1] } else { 0.0 };
            let input_l = dry_l.clamp(-MAX_AUDIO, MAX_AUDIO);
            let input_r = dry_r.clamp(-MAX_AUDIO, MAX_AUDIO);
            let delayed_l = sanitize(
                self.dl_l
                    .read(Self::delay(base_frames, depth, self.lfo_l.sine())),
                0.0,
                -MAX_STATE,
                MAX_STATE,
            );
            let delayed_r = sanitize(
                self.dl_r
                    .read(Self::delay(base_frames, depth, self.lfo_r.sine())),
                0.0,
                -MAX_STATE,
                MAX_STATE,
            );
            self.dl_l.write(sanitize(
                input_l + delayed_l * feedback,
                0.0,
                -MAX_AUDIO,
                MAX_AUDIO,
            ));
            self.dl_r.write(sanitize(
                input_r + delayed_r * feedback,
                0.0,
                -MAX_AUDIO,
                MAX_AUDIO,
            ));
            if mix == 0.0 {
                frame[0] = dry_l;
                frame[1] = dry_r;
            } else {
                frame[0] = sanitize(
                    dry_l * (1.0 - mix) + delayed_l * mix,
                    0.0,
                    -MAX_STATE,
                    MAX_STATE,
                );
                frame[1] = sanitize(
                    dry_r * (1.0 - mix) + delayed_r * mix,
                    0.0,
                    -MAX_STATE,
                    MAX_STATE,
                );
            }
        }
    }

    fn reset(&mut self) {
        self.dl_l.reset();
        self.dl_r.reset();
        self.lfo_l.reset();
        self.lfo_r.set_phase(0.25);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{peak, sine};
    use crate::params::EffectKind;

    const SR: f32 = 48_000.0;

    fn params(rate: f32, depth: f32, base_ms: f32, feedback: f32, mix: f32) -> ParamVals {
        let mut p = EffectKind::Flanger.default_values();
        p.v[..5].copy_from_slice(&[rate, depth, base_ms, feedback, mix]);
        p
    }

    fn run(f: &mut Flanger, x: &[f32], p: &ParamVals) -> Vec<Frame> {
        let mut buf: Vec<Frame> = x.iter().map(|&sample| [sample, sample]).collect();
        let n = buf.len();
        f.process(&mut buf, n, p);
        buf
    }

    #[test]
    fn zero_mix_is_transparent() {
        let input = sine(512, 440.0, SR, 0.4);
        let mut flanger = Flanger::new();
        flanger.set_rates(SR);
        let out = run(&mut flanger, &input, &params(2.0, 1.0, 5.0, 0.8, 0.0));

        for (frame, &dry) in out.iter().zip(&input) {
            assert_eq!(*frame, [dry, dry]);
        }
    }

    #[test]
    fn fixed_depth_zero_impulse_lands_at_the_exact_delay() {
        let mut input = vec![0.0; 160];
        input[0] = 1.0;
        let mut flanger = Flanger::new();
        flanger.set_rates(SR);
        let out = run(&mut flanger, &input, &params(1.0, 0.0, 2.0, 0.0, 1.0));
        let delay = 96; // 2 ms at 48 kHz

        assert!(out[..delay].iter().all(|frame| frame[0] == 0.0));
        assert_eq!(out[delay], [1.0, 1.0]);
        assert!(out[delay + 1..].iter().all(|frame| frame[0] == 0.0));
    }

    #[test]
    fn modulation_delay_stays_within_its_bounds() {
        let base_frames = 5.0 * SR / 1_000.0;
        assert!((Flanger::delay(base_frames, 1.0, -1.0) - 24.0).abs() < 1e-4);
        assert!((Flanger::delay(base_frames, 1.0, 1.0) - 456.0).abs() < 1e-4);
        assert!(
            Flanger::new().dl_l.capacity() >= 456 + 2,
            "delay line lacks interpolation guard"
        );

        let mut input = vec![0.0; 600];
        input[0] = 1.0;
        let mut flanger = Flanger::new();
        flanger.set_rates(SR);
        let out = run(&mut flanger, &input, &params(0.05, 1.0, 5.0, 0.0, 1.0));
        let positions: Vec<_> = out
            .iter()
            .enumerate()
            .filter_map(|(i, frame)| (frame[0].abs().max(frame[1].abs()) > 1e-4).then_some(i))
            .collect();

        assert!(!positions.is_empty());
        assert!(
            positions.iter().all(|&i| (2..=456).contains(&i)),
            "{positions:?}"
        );
    }

    #[test]
    fn feedback_sign_controls_the_second_delayed_return() {
        let mut input = vec![0.0; 160];
        input[0] = 0.8;
        let p = params(1.0, 0.0, 1.0, 0.5, 1.0);
        let n = params(1.0, 0.0, 1.0, -0.5, 1.0);
        let mut positive = Flanger::new();
        let mut negative = Flanger::new();
        positive.set_rates(SR);
        negative.set_rates(SR);
        let positive = run(&mut positive, &input, &p);
        let negative = run(&mut negative, &input, &n);

        assert!(positive[48][0] > 0.7 && negative[48][0] > 0.7);
        assert!(positive[96][0] > 0.0 && negative[96][0] < 0.0);
        assert!((positive[96][0].abs() - negative[96][0].abs()).abs() < 1e-6);
    }

    #[test]
    fn feedback_decays_without_running_away() {
        let mut input = vec![0.0; 4_000];
        input[0] = 0.8;
        let mut flanger = Flanger::new();
        flanger.set_rates(SR);
        let out = run(&mut flanger, &input, &params(1.0, 0.0, 0.5, 0.85, 1.0));
        let left: Vec<_> = out.iter().map(|frame| frame[0]).collect();

        assert!(peak(&left[..256]) > 0.7);
        assert!(left.iter().all(|sample| sample.is_finite()));
        assert!(peak(&left) < 1.0);
        assert!(peak(&left[2_000..]) < peak(&left[..256]));
    }

    #[test]
    fn quarter_cycle_lfos_create_stereo_movement() {
        let input = sine(20_000, 440.0, SR, 0.4);
        let mut flanger = Flanger::new();
        flanger.set_rates(SR);
        let out = run(&mut flanger, &input, &params(5.0, 1.0, 5.0, 0.0, 1.0));
        let difference = out[1_024..]
            .iter()
            .map(|frame| (frame[0] - frame[1]).abs())
            .fold(0.0f32, f32::max);

        assert!(difference > 1e-3, "stereo movement difference {difference}");
    }

    #[test]
    fn processing_partitions_are_equivalent() {
        let input = sine(4_096, 330.0, SR, 0.3);
        let p = params(3.0, 0.75, 3.0, 0.4, 0.65);
        let mut whole = Flanger::new();
        let mut split = Flanger::new();
        whole.set_rates(SR);
        split.set_rates(SR);
        let whole = run(&mut whole, &input, &p);
        let mut split_out: Vec<Frame> = input.iter().map(|&sample| [sample, sample]).collect();
        for chunk in split_out.chunks_mut(37) {
            split.process(chunk, chunk.len(), &p);
        }

        for (a, b) in whole.iter().zip(&split_out) {
            assert!((a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6);
        }
    }

    #[test]
    fn reset_clears_the_delay_tail() {
        let mut flanger = Flanger::new();
        flanger.set_rates(SR);
        let p = params(1.0, 0.0, 1.0, 0.5, 1.0);
        let mut before = [[0.0; 2]; 49];
        before[0] = [1.0, 1.0];
        let before_n = before.len();
        flanger.process(&mut before, before_n, &p);
        assert_eq!(before[48], [1.0, 1.0]);

        flanger.reset();
        let mut after = [[0.0; 2]; 100];
        let after_n = after.len();
        flanger.process(&mut after, after_n, &p);
        assert!(after.iter().all(|frame| *frame == [0.0, 0.0]));
    }

    #[test]
    fn sample_rate_changes_keep_the_quarter_cycle_stereo_offset() {
        let mut flanger = Flanger::new();
        flanger.lfo_l.set_rate(5.0, SR);
        flanger.lfo_r.set_rate(5.0, SR);
        for _ in 0..1_000 {
            flanger.lfo_l.sine();
            flanger.lfo_r.sine();
        }

        flanger.set_rates(96_000.0);
        flanger.lfo_l.set_rate(5.0, 96_000.0);
        flanger.lfo_r.set_rate(5.0, 96_000.0);
        let left = flanger.lfo_l.sine();
        let right = flanger.lfo_r.sine();
        assert!((left * left + right * right - 1.0).abs() < 1e-4);
    }

    #[test]
    fn invalid_sample_rate_uses_the_supported_minimum() {
        let mut flanger = Flanger::new();
        flanger.set_rates(1.0);
        assert_eq!(flanger.sr, 8_000.0);
    }

    #[test]
    fn non_finite_and_hot_input_cannot_poison_the_feedback_state() {
        let mut flanger = Flanger::new();
        flanger.set_rates(SR);
        let mut input = vec![[0.0; 2]; 2_048];
        input[0] = [f32::NAN, f32::INFINITY];
        input[1] = [f32::MAX, -f32::MAX];
        let n = input.len();
        flanger.process(&mut input, n, &params(1.0, 0.0, 0.5, 0.85, 1.0));
        assert!(input
            .iter()
            .flatten()
            .all(|sample| sample.is_finite() && sample.abs() <= MAX_STATE));
    }

    #[test]
    fn junk_extremes_and_supported_rates_stay_finite() {
        let mut flanger = Flanger::new();
        let mut p = ParamVals::ZEROED;
        p.v[..5].copy_from_slice(&[
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::INFINITY,
            f32::NAN,
        ]);

        for sr in [8_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            flanger.set_rates(sr);
            let mut buf = [[4.0, -4.0]; 512];
            let n = buf.len();
            flanger.process(&mut buf, n, &p);
            assert!(
                buf.iter().flatten().all(|sample| sample.is_finite()),
                "{sr}"
            );
        }
    }
}
