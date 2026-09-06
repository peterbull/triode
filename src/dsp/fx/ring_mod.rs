use crate::dsp::biquad::{DcBlocker, OnePole};
use crate::dsp::{db2lin, sanitize, Frame, MAX_SAMPLE_RATE_HZ};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const CARRIER: usize = 0;
pub const TONE: usize = 1;
pub const MIX: usize = 2;
pub const LEVEL: usize = 3;

const DEFAULT_SR: f32 = 48_000.0;
const MAX_AUDIO: f32 = 8.0;
const MAX_STATE: f32 = 64.0;

#[derive(Debug)]
pub struct RingModulator {
    sr: f32,
    phase: f32,
    carrier_hz: f32,
    tone_hz: f32,
    pre: [OnePole; 2],
    dc: [DcBlocker; 2],
}

impl Default for RingModulator {
    fn default() -> Self {
        Self::new()
    }
}

impl RingModulator {
    pub fn new() -> Self {
        let mut ring = Self {
            sr: DEFAULT_SR,
            phase: 0.0,
            carrier_hz: 120.0,
            tone_hz: 8_000.0,
            pre: [OnePole::new(); 2],
            dc: [DcBlocker::new(); 2],
        };
        ring.update_filters();
        ring
    }

    fn update_filters(&mut self) {
        let cutoff = self
            .tone_hz
            .min((0.45 * self.sr - self.carrier_hz).max(20.0));
        for filter in &mut self.pre {
            filter.set_hz(cutoff, self.sr);
        }
        for blocker in &mut self.dc {
            blocker.set_hz(10.0, self.sr);
        }
    }
}

impl Proc for RingModulator {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sanitize(sr, DEFAULT_SR, 8_000.0, MAX_SAMPLE_RATE_HZ);
        self.update_filters();
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        self.carrier_hz = sanitize(p.v[CARRIER], 120.0, 20.0, 2_000.0);
        self.tone_hz = sanitize(p.v[TONE], 8_000.0, 500.0, 16_000.0);
        self.update_filters();
        let mix = sanitize(p.v[MIX], 0.5, 0.0, 1.0);
        let level = db2lin(sanitize(p.v[LEVEL], -3.0, -18.0, 6.0));
        let phase_step = self.carrier_hz / self.sr;

        for frame in buf.iter_mut().take(n) {
            let dry = [
                if frame[0].is_finite() { frame[0] } else { 0.0 },
                if frame[1].is_finite() { frame[1] } else { 0.0 },
            ];
            self.phase += phase_step;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            let carrier = (std::f32::consts::TAU * self.phase).sin();

            for channel in 0..2 {
                let input = dry[channel].clamp(-MAX_AUDIO, MAX_AUDIO);
                let wet =
                    self.dc[channel].process(self.pre[channel].lowpass(input) * carrier) * level;
                frame[channel] = if mix == 0.0 {
                    dry[channel]
                } else {
                    sanitize(
                        dry[channel] * (1.0 - mix) + wet * mix,
                        0.0,
                        -MAX_STATE,
                        MAX_STATE,
                    )
                };
            }
        }
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        for filter in &mut self.pre {
            filter.reset();
        }
        for blocker in &mut self.dc {
            blocker.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{goertzel_exact, sine};
    use crate::params::EffectKind;

    const SR: f32 = 48_000.0;

    fn params(carrier: f32, tone: f32, mix: f32, level: f32) -> ParamVals {
        let mut p = EffectKind::RingModulator.default_values();
        p.v[..4].copy_from_slice(&[carrier, tone, mix, level]);
        p
    }

    fn process(ring: &mut RingModulator, buf: &mut [Frame], p: &ParamVals) {
        ring.process(buf, buf.len(), p);
    }

    fn stereo(input: &[f32]) -> Vec<Frame> {
        input.iter().map(|sample| [*sample; 2]).collect()
    }

    fn rms(input: &[Frame]) -> f32 {
        (input.iter().map(|frame| frame[0] * frame[0]).sum::<f32>() / input.len() as f32).sqrt()
    }

    #[test]
    fn multiplication_creates_sum_and_difference_sidebands() {
        let input = sine(56_000, 440.0, SR, 1.0);
        let mut buf = stereo(&input);
        let mut ring = RingModulator::new();
        ring.set_rates(SR);
        process(&mut ring, &mut buf, &params(100.0, 16_000.0, 1.0, 0.0));
        let output: Vec<f32> = buf[8_000..].iter().map(|frame| frame[0]).collect();

        for frequency in [340.0, 540.0] {
            let magnitude = goertzel_exact(&output, frequency, SR);
            assert!(
                (0.45..0.52).contains(&magnitude),
                "{frequency} Hz sideband magnitude was {magnitude}"
            );
        }
        for frequency in [100.0, 440.0] {
            let magnitude = goertzel_exact(&output, frequency, SR);
            assert!(
                magnitude < 0.01,
                "{frequency} Hz leaked at magnitude {magnitude}"
            );
        }
    }

    #[test]
    fn silent_input_has_no_carrier_bleed() {
        let mut ring = RingModulator::new();
        ring.set_rates(SR);
        let mut buf = [[0.0; 2]; 4_096];
        process(&mut ring, &mut buf, &params(2_000.0, 16_000.0, 1.0, 6.0));
        assert!(buf.iter().flatten().all(|sample| *sample == 0.0));
    }

    #[test]
    fn tone_attenuates_high_input_before_modulation() {
        let input = stereo(&sine(12_000, 8_000.0, SR, 0.5));
        let mut bright = input.clone();
        let mut dark = input;
        let mut bright_ring = RingModulator::new();
        let mut dark_ring = RingModulator::new();
        bright_ring.set_rates(SR);
        dark_ring.set_rates(SR);
        process(
            &mut bright_ring,
            &mut bright,
            &params(100.0, 16_000.0, 1.0, 0.0),
        );
        process(&mut dark_ring, &mut dark, &params(100.0, 500.0, 1.0, 0.0));
        assert!(rms(&dark[4_096..]) < rms(&bright[4_096..]) * 0.1);
    }

    #[test]
    fn level_follows_decibel_gain() {
        let input = stereo(&sine(8_192, 440.0, SR, 0.2));
        let mut quiet = input.clone();
        let mut loud = input;
        let mut quiet_ring = RingModulator::new();
        let mut loud_ring = RingModulator::new();
        quiet_ring.set_rates(SR);
        loud_ring.set_rates(SR);
        process(
            &mut quiet_ring,
            &mut quiet,
            &params(120.0, 16_000.0, 1.0, -18.0),
        );
        process(
            &mut loud_ring,
            &mut loud,
            &params(120.0, 16_000.0, 1.0, 6.0),
        );
        let ratio = rms(&loud[2_048..]) / rms(&quiet[2_048..]);
        let expected = 10.0_f32.powf(24.0 / 20.0);
        assert!((ratio - expected).abs() < expected * 1e-4);
    }

    #[test]
    fn block_partitions_preserve_the_result() {
        let original: Vec<Frame> = (0..1_037)
            .map(|i| [(i as f32 * 0.13).sin() * 0.7, (i as f32 * 0.07).cos() * 0.4])
            .collect();
        let p = params(731.0, 4_200.0, 0.63, -2.5);
        let mut whole = original.clone();
        let mut split = original;
        let mut whole_ring = RingModulator::new();
        let mut split_ring = RingModulator::new();
        whole_ring.set_rates(SR);
        split_ring.set_rates(SR);
        process(&mut whole_ring, &mut whole, &p);
        for chunk in split.chunks_mut(37) {
            process(&mut split_ring, chunk, &p);
        }
        assert_eq!(whole, split);
    }

    #[test]
    fn stereo_channels_share_carrier_without_audio_leakage() {
        let p = params(317.0, 7_000.0, 1.0, 0.0);
        let mut left_only: Vec<Frame> = sine(2_048, 700.0, SR, 0.6)
            .into_iter()
            .map(|sample| [sample, 0.0])
            .collect();
        let mut ring = RingModulator::new();
        ring.set_rates(SR);
        process(&mut ring, &mut left_only, &p);
        assert!(left_only.iter().all(|frame| frame[1] == 0.0));

        let mut identical = stereo(&sine(2_048, 700.0, SR, 0.6));
        let mut ring = RingModulator::new();
        ring.set_rates(SR);
        process(&mut ring, &mut identical, &p);
        assert!(identical.iter().all(|frame| frame[0] == frame[1]));
    }

    #[test]
    fn zero_mix_is_exact_while_wet_state_advances() {
        let prefix = stereo(&sine(257, 430.0, SR, 1.7));
        let tail = stereo(&sine(263, 670.0, SR, 0.4));
        let mut bypassed_prefix = prefix.clone();
        let mut reference_prefix = prefix;
        let mut bypassed_tail = tail.clone();
        let mut reference_tail = tail;
        let mut bypassed = RingModulator::new();
        let mut reference = RingModulator::new();
        bypassed.set_rates(SR);
        reference.set_rates(SR);
        process(
            &mut bypassed,
            &mut bypassed_prefix,
            &params(211.0, 3_000.0, 0.0, 6.0),
        );
        process(
            &mut reference,
            &mut reference_prefix,
            &params(211.0, 3_000.0, 1.0, 6.0),
        );
        assert_eq!(bypassed_prefix, stereo(&sine(257, 430.0, SR, 1.7)));
        let wet = params(211.0, 3_000.0, 1.0, 6.0);
        process(&mut bypassed, &mut bypassed_tail, &wet);
        process(&mut reference, &mut reference_tail, &wet);
        assert_eq!(bypassed_tail, reference_tail);
    }

    #[test]
    fn reset_restores_phase_and_filter_history() {
        let p = params(407.0, 1_900.0, 0.8, -4.0);
        let mut ring = RingModulator::new();
        ring.set_rates(SR);
        let mut excited = [[0.7, -0.4]; 333];
        process(&mut ring, &mut excited, &p);
        ring.reset();

        let input: Vec<Frame> = (0..257)
            .map(|i| [(i as f32 * 0.17).sin(), (i as f32 * 0.11).cos()])
            .collect();
        let mut actual = input.clone();
        process(&mut ring, &mut actual, &p);
        let mut fresh = RingModulator::new();
        fresh.set_rates(SR);
        let mut expected = input;
        process(&mut fresh, &mut expected, &p);
        assert_eq!(actual, expected);
    }

    #[test]
    fn carrier_and_rate_changes_preserve_phase_and_invalid_rates_are_sanitized() {
        let mut transitioned = RingModulator::new();
        transitioned.set_rates(SR);
        let mut silence = [[0.0; 2]; 137];
        process(
            &mut transitioned,
            &mut silence,
            &params(211.0, 16_000.0, 1.0, 0.0),
        );
        let before_carrier_change = transitioned.phase;
        process(
            &mut transitioned,
            &mut [[0.0; 2]],
            &params(997.0, 16_000.0, 1.0, 0.0),
        );
        let expected = (before_carrier_change + 997.0 / SR).rem_euclid(1.0);
        assert!((transitioned.phase - expected).abs() < 1e-7);

        let before_rate_change = transitioned.phase;
        transitioned.set_rates(96_000.0);
        assert_eq!(transitioned.phase, before_rate_change);
        process(
            &mut transitioned,
            &mut [[0.0; 2]],
            &params(997.0, 16_000.0, 1.0, 0.0),
        );
        let expected = (before_rate_change + 997.0 / 96_000.0).rem_euclid(1.0);
        assert!((transitioned.phase - expected).abs() < 1e-7);

        let p = params(100.0, 16_000.0, 1.0, 0.0);
        let mut invalid = RingModulator::new();
        let mut minimum = RingModulator::new();
        invalid.set_rates(1.0);
        minimum.set_rates(8_000.0);
        let mut invalid_output = [[0.5; 2]; 32];
        let mut minimum_output = invalid_output;
        process(&mut invalid, &mut invalid_output, &p);
        process(&mut minimum, &mut minimum_output, &p);
        assert_eq!(invalid_output, minimum_output);
    }

    #[test]
    fn malformed_controls_and_oversized_frame_count_do_not_panic() {
        let mut p = ParamVals::ZEROED;
        p.v[..4].copy_from_slice(&[f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::NAN]);
        let mut ring = RingModulator::new();
        ring.set_rates(f32::NAN);
        let mut buf = [[0.25, -0.5]; 64];
        ring.process(&mut buf, usize::MAX, &p);
        assert!(buf.iter().flatten().all(|sample| sample.is_finite()));
    }

    #[test]
    fn hot_and_non_finite_input_cannot_poison_processing() {
        for sr in [8_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let mut ring = RingModulator::new();
            ring.set_rates(sr);
            let p = params(2_000.0, 16_000.0, 1.0, 6.0);
            let mut buf = [[0.25, -0.25]; 2_048];
            buf[0] = [f32::NAN, f32::INFINITY];
            buf[1] = [f32::MAX, -f32::MAX];
            process(&mut ring, &mut buf, &p);
            assert!(
                buf.iter()
                    .flatten()
                    .all(|sample| sample.is_finite() && sample.abs() <= 64.0),
                "failed at {sr} Hz"
            );
            assert!(buf[1_024..]
                .iter()
                .flatten()
                .any(|sample| sample.abs() > 1e-5));
        }
    }
}
