use crate::dsp::biquad::OnePole;
use crate::dsp::{db2lin, sanitize, Frame, MAX_SAMPLE_RATE_HZ};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const BITS: usize = 0;
pub const DOWNSAMPLE: usize = 1;
pub const DRIVE: usize = 2;
pub const TONE: usize = 3;
pub const MIX: usize = 4;

#[derive(Debug)]
pub struct BitCrusher {
    sr: f32,
    tone: [OnePole; 2],
    tone_hz: f32,
    held: Frame,
    remaining: usize,
}

impl BitCrusher {
    pub fn new() -> Self {
        let sr = 48_000.0;
        let tone_hz = 8_000.0;
        let mut tone = [OnePole::new(); 2];
        for filter in &mut tone {
            filter.set_hz(tone_hz, sr);
        }
        Self {
            sr,
            tone,
            tone_hz,
            held: [0.0; 2],
            remaining: 0,
        }
    }

    fn set_tone(&mut self, hz: f32) {
        self.tone_hz = sanitize(hz, 8_000.0, 500.0, 16_000.0);
        let hz = self.tone_hz.min(self.sr * 0.45);
        for filter in &mut self.tone {
            filter.set_hz(hz, self.sr);
        }
    }
}

impl Default for BitCrusher {
    fn default() -> Self {
        Self::new()
    }
}

fn quantize(x: f32, bits: u32) -> f32 {
    let bits = bits.clamp(4, 16);
    let steps = (1_u32 << (bits - 1)) - 1;
    (x.clamp(-1.0, 1.0) * steps as f32).round() / steps as f32
}

fn controls(bits: f32, downsample: f32) -> (u32, usize) {
    (
        sanitize(bits, 10.0, 4.0, 16.0).round() as u32,
        sanitize(downsample, 4.0, 1.0, 32.0).round() as usize,
    )
}

impl Proc for BitCrusher {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sanitize(sr, 48_000.0, 8_000.0, MAX_SAMPLE_RATE_HZ);
        self.set_tone(self.tone_hz);
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let (bits, factor) = controls(p.v[BITS], p.v[DOWNSAMPLE]);
        let drive = db2lin(sanitize(p.v[DRIVE], 0.0, -12.0, 24.0));
        self.set_tone(p.v[TONE]);
        let mix = sanitize(p.v[MIX], 0.5, 0.0, 1.0);

        for frame in &mut buf[..n] {
            let dry = [
                if frame[0].is_finite() { frame[0] } else { 0.0 },
                if frame[1].is_finite() { frame[1] } else { 0.0 },
            ];
            if self.remaining == 0 {
                self.held[0] = quantize((dry[0] * drive).clamp(-1.0, 1.0), bits);
                self.held[1] = quantize((dry[1] * drive).clamp(-1.0, 1.0), bits);
                self.remaining = factor - 1;
            } else {
                self.remaining -= 1;
            }
            for channel in 0..2 {
                let wet = self.tone[channel].lowpass(self.held[channel]);
                frame[channel] = dry[channel] * (1.0 - mix) + wet * mix;
            }
        }
    }

    fn reset(&mut self) {
        self.held = [0.0; 2];
        self.remaining = 0;
        for filter in &mut self.tone {
            filter.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::dsp::{hz_coef, Frame};
    use crate::params::EffectKind;

    const SR: f32 = 48_000.0;

    fn params(bits: f32, downsample: f32, drive: f32, tone: f32, mix: f32) -> ParamVals {
        let mut p = EffectKind::BitCrusher.default_values();
        for (i, value) in [bits, downsample, drive, tone, mix].iter().enumerate() {
            p.v[i] = *value;
        }
        p
    }

    fn process(crusher: &mut BitCrusher, buf: &mut [Frame], p: &ParamVals) {
        crusher.process(buf, buf.len(), p);
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn quantizer_has_exact_symmetric_levels_zero_and_endpoints() {
        for bits in 4..=16 {
            let steps = (1_u32 << (bits - 1)) - 1;
            let mut levels = HashSet::new();
            for code in -(2 * steps as i32)..=(2 * steps as i32) {
                let input = code as f32 / (2 * steps) as f32;
                let value = quantize(input, bits);
                levels.insert(value.to_bits());
                assert!((value + quantize(-input, bits)).abs() < 1e-7);
            }
            assert_eq!(levels.len(), (1_usize << bits) - 1);
            assert_eq!(quantize(0.0, bits), 0.0);
            assert_eq!(quantize(-1.0, bits), -1.0);
            assert_eq!(quantize(1.0, bits), 1.0);
        }
    }

    #[test]
    fn controls_round_and_clamp_to_integer_bounds() {
        assert_eq!(controls(4.49, 2.49), (4, 2));
        assert_eq!(controls(4.5, 2.5), (5, 3));
        assert_eq!(controls(-9.0, 99.0), (4, 32));
        assert_eq!(controls(f32::NAN, f32::INFINITY), (10, 4));
    }

    #[test]
    fn holds_exact_rounded_runs_before_tone() {
        let p = params(16.0, 3.6, 0.0, 16_000.0, 1.0);
        let input = [0.11, -0.23, 0.37, -0.49, 0.61, -0.73, 0.85, -0.97];
        let mut crusher = BitCrusher::new();
        crusher.set_rates(SR);
        let mut expected_held = 0.0;
        let mut expected_tone = 0.0;
        let a = hz_coef(16_000.0, SR);

        for (i, sample) in input.iter().enumerate() {
            let mut frame = [[*sample, -*sample]];
            process(&mut crusher, &mut frame, &p);
            if i % 4 == 0 {
                expected_held = quantize(*sample, 16);
            }
            expected_tone = expected_held * (1.0 - a) + expected_tone * a;
            assert!((crusher.held[0] - expected_held).abs() < 1e-7, "frame {i}");
            assert!((frame[0][0] - expected_tone).abs() < 1e-6, "frame {i}");
        }
    }

    #[test]
    fn downsample_factor_is_rate_independent() {
        let p = params(12.0, 3.6, 0.0, 16_000.0, 1.0);
        let input = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9];
        let mut held_at_rates = Vec::new();

        for sr in [8_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let mut crusher = BitCrusher::new();
            crusher.set_rates(sr);
            let mut held = Vec::new();
            for sample in input {
                let mut frame = [[sample, 0.0]];
                process(&mut crusher, &mut frame, &p);
                held.push(crusher.held[0]);
            }
            held_at_rates.push(held);
        }

        assert!(held_at_rates.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn zero_input_stays_exactly_silent() {
        let mut crusher = BitCrusher::new();
        crusher.set_rates(SR);
        let p = params(4.0, 32.0, 24.0, 500.0, 1.0);
        let mut buf = [[0.0; 2]; 128];
        process(&mut crusher, &mut buf, &p);
        assert!(buf.iter().flatten().all(|sample| *sample == 0.0));
    }

    #[test]
    fn zero_mix_is_transparent_even_above_the_wet_quantizer_range() {
        let mut crusher = BitCrusher::new();
        let p = params(4.0, 32.0, 24.0, 500.0, 0.0);
        let original = [[-2.5, 1.5], [-0.25, 0.125], [0.0, 0.9]];
        let mut buf = original;
        process(&mut crusher, &mut buf, &p);
        assert_eq!(buf, original);
    }

    #[test]
    fn drive_increases_then_bounds_the_wet_signal() {
        let mut clean = BitCrusher::new();
        let mut driven = BitCrusher::new();
        clean.set_rates(SR);
        driven.set_rates(SR);
        let mut clean_frame = [[0.2, 0.0]];
        let mut driven_frame = clean_frame;
        process(
            &mut clean,
            &mut clean_frame,
            &params(16.0, 1.0, 0.0, 16_000.0, 1.0),
        );
        process(
            &mut driven,
            &mut driven_frame,
            &params(16.0, 1.0, 24.0, 16_000.0, 1.0),
        );
        assert!(driven_frame[0][0] > clean_frame[0][0]);
        assert!(driven_frame[0][0].abs() <= 1.0);
    }

    #[test]
    fn lower_tone_attenuates_high_frequency_content() {
        let input: Vec<Frame> = (0..8192)
            .map(|i| {
                let sample = (2.0 * core::f32::consts::PI * 6_000.0 * i as f32 / SR).sin() * 0.5;
                [sample; 2]
            })
            .collect();
        let mut bright = BitCrusher::new();
        let mut dark = BitCrusher::new();
        bright.set_rates(SR);
        dark.set_rates(SR);
        let mut bright_buf = input.clone();
        let mut dark_buf = input;
        process(
            &mut bright,
            &mut bright_buf,
            &params(16.0, 1.0, 0.0, 16_000.0, 1.0),
        );
        process(
            &mut dark,
            &mut dark_buf,
            &params(16.0, 1.0, 0.0, 500.0, 1.0),
        );
        let bright_samples: Vec<f32> = bright_buf[1024..].iter().map(|frame| frame[0]).collect();
        let dark_samples: Vec<f32> = dark_buf[1024..].iter().map(|frame| frame[0]).collect();
        assert!(rms(&dark_samples) < rms(&bright_samples) * 0.25);
    }

    #[test]
    fn block_partitions_preserve_the_result() {
        let p = params(9.0, 5.0, 6.0, 3_000.0, 0.7);
        let original: Vec<Frame> = (0..173)
            .map(|i| {
                [
                    ((i as f32 * 0.13).sin()) * 0.8,
                    ((i as f32 * 0.07).cos()) * 0.6,
                ]
            })
            .collect();
        let mut whole = BitCrusher::new();
        let mut split = BitCrusher::new();
        whole.set_rates(SR);
        split.set_rates(SR);
        let mut whole_buf = original.clone();
        let mut split_buf = original;
        process(&mut whole, &mut whole_buf, &p);
        for chunk in split_buf.chunks_mut(37) {
            process(&mut split, chunk, &p);
        }
        for (expected, actual) in whole_buf.iter().zip(split_buf) {
            assert!((expected[0] - actual[0]).abs() < 1e-7);
            assert!((expected[1] - actual[1]).abs() < 1e-7);
        }
    }

    #[test]
    fn channels_do_not_leak() {
        let mut crusher = BitCrusher::new();
        crusher.set_rates(SR);
        let p = params(8.0, 4.0, 0.0, 5_000.0, 1.0);
        let mut buf = [[0.8, 0.0]; 128];
        process(&mut crusher, &mut buf, &p);
        assert!(buf.iter().all(|frame| frame[1] == 0.0));
    }

    #[test]
    fn reset_clears_hold_counter_and_tone_history() {
        let p = params(10.0, 7.0, 12.0, 800.0, 1.0);
        let mut crusher = BitCrusher::new();
        crusher.set_rates(SR);
        let mut excited = [[0.7, -0.3]; 11];
        process(&mut crusher, &mut excited, &p);
        crusher.reset();
        let mut after_reset = [[0.2, -0.4]];
        process(&mut crusher, &mut after_reset, &p);

        let mut fresh = BitCrusher::new();
        fresh.set_rates(SR);
        let mut expected = [[0.2, -0.4]];
        process(&mut fresh, &mut expected, &p);
        assert_eq!(after_reset, expected);
    }

    #[test]
    fn invalid_sample_rate_uses_the_supported_minimum() {
        let mut crusher = BitCrusher::new();
        crusher.set_rates(1.0);
        assert_eq!(crusher.sr, 8_000.0);
    }

    #[test]
    fn junk_and_extremes_stay_finite_at_supported_rates() {
        let mut p = ParamVals::ZEROED;
        p.v[BITS] = f32::NAN;
        p.v[DOWNSAMPLE] = f32::INFINITY;
        p.v[DRIVE] = f32::NEG_INFINITY;
        p.v[TONE] = f32::NAN;
        p.v[MIX] = f32::INFINITY;
        for sr in [8_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let mut crusher = BitCrusher::new();
            crusher.set_rates(sr);
            let mut buf = [[f32::INFINITY, f32::NEG_INFINITY]; 64];
            process(&mut crusher, &mut buf, &p);
            assert!(buf
                .iter()
                .flatten()
                .all(|sample| sample.is_finite() && sample.abs() <= 1.0));
        }
    }
}
