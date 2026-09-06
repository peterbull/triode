use crate::dsp::svf::Svf;
use crate::dsp::{db2lin, sanitize, Frame, MAX_SAMPLE_RATE_HZ};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const SENSITIVITY: usize = 0;
pub const BASE: usize = 1;
pub const SWEEP: usize = 2;
pub const Q: usize = 3;
pub const RELEASE: usize = 4;
pub const MIX: usize = 5;

const UPDATE_INTERVAL: usize = 16;
const MAX_AUDIO: f32 = 8.0;
const MAX_STATE: f32 = 64.0;

pub struct EnvelopeFilter {
    sr: f32,
    envelope: f32,
    filters: [Svf; 2],
    g: f32,
    g_step: f32,
    ramp_remaining: usize,
}

impl EnvelopeFilter {
    pub fn new() -> Self {
        Self {
            sr: 48_000.0,
            envelope: 0.0,
            filters: [Svf::default(); 2],
            g: 0.0,
            g_step: 0.0,
            ramp_remaining: 0,
        }
    }

    fn update_g(&mut self, sensitivity: f32, base: f32, sweep: f32) {
        if self.ramp_remaining == 0 {
            let e = (self.envelope * db2lin(sensitivity)).clamp(0.0, 1.0);
            let cutoff = (base * 2.0f32.powf(sweep * e)).min(0.45 * self.sr);
            let target = sanitize(
                (std::f32::consts::PI * cutoff / self.sr).tan(),
                0.0,
                0.0,
                8.0,
            );
            self.g_step = sanitize((target - self.g) / UPDATE_INTERVAL as f32, 0.0, -8.0, 8.0);
            self.ramp_remaining = UPDATE_INTERVAL;
        }
        self.g = sanitize(self.g + self.g_step, 0.0, 0.0, 8.0);
        self.ramp_remaining -= 1;
    }
}

impl Default for EnvelopeFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl Proc for EnvelopeFilter {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sanitize(sr, 48_000.0, 8_000.0, MAX_SAMPLE_RATE_HZ);
        self.reset();
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let sensitivity = sanitize(p.v[SENSITIVITY], 0.0, -24.0, 24.0);
        let base = sanitize(p.v[BASE], 300.0, 150.0, 1000.0);
        let sweep = sanitize(p.v[SWEEP], 2.0, 0.0, 3.0);
        let k = 1.0 / sanitize(p.v[Q], 1.5, 0.5, 4.0);
        let release = sanitize(p.v[RELEASE], 180.0, 30.0, 600.0) / 1000.0;
        let mix = sanitize(p.v[MIX], 1.0, 0.0, 1.0);

        for frame in buf.iter_mut().take(n) {
            let dry_l = if frame[0].is_finite() { frame[0] } else { 0.0 };
            let dry_r = if frame[1].is_finite() { frame[1] } else { 0.0 };
            let filter_l = dry_l.clamp(-MAX_AUDIO, MAX_AUDIO);
            let filter_r = dry_r.clamp(-MAX_AUDIO, MAX_AUDIO);
            let detector = filter_l.abs().max(filter_r.abs());
            let tau = if detector > self.envelope {
                0.003
            } else {
                release
            };
            let coefficient = (-1.0 / (tau * self.sr)).exp();
            self.envelope = sanitize(
                detector + coefficient * (self.envelope - detector),
                0.0,
                0.0,
                MAX_AUDIO,
            );
            self.update_g(sensitivity, base, sweep);
            let wet_l = self.filters[0].process(filter_l, self.g, k);
            let wet_r = self.filters[1].process(filter_r, self.g, k);
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
        self.envelope = 0.0;
        self.filters = [Svf::default(); 2];
        self.g = 0.0;
        self.g_step = 0.0;
        self.ramp_remaining = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::sine;
    use crate::engine::Proc;
    use crate::params::EffectKind;

    const SR: f32 = 48_000.0;

    fn params(
        sensitivity: f32,
        base: f32,
        sweep: f32,
        q: f32,
        release: f32,
        mix: f32,
    ) -> ParamVals {
        let mut p = EffectKind::EnvelopeFilter.default_values();
        for (i, value) in [sensitivity, base, sweep, q, release, mix]
            .iter()
            .enumerate()
        {
            p.v[i] = *value;
        }
        p
    }

    fn process(filter: &mut EnvelopeFilter, input: &[Frame], p: &ParamVals) -> Vec<Frame> {
        let mut output = input.to_vec();
        let n = output.len();
        filter.process(&mut output, n, p);
        output
    }

    fn mono(input: &[f32]) -> Vec<Frame> {
        input.iter().map(|&sample| [sample, sample]).collect()
    }

    fn channel_rms(input: &[Frame]) -> f32 {
        (input.iter().map(|frame| frame[0] * frame[0]).sum::<f32>() / input.len() as f32).sqrt()
    }

    #[test]
    fn tpt_svf_response_is_stable() {
        let mut filter = Svf::default();
        let actual = [
            filter.process(1.0, 0.125, 0.8).to_bits(),
            filter.process(-0.25, 0.125, 0.8).to_bits(),
            filter.process(0.5, 0.75, 0.4).to_bits(),
            filter.process(-1.0, 0.75, 0.4).to_bits(),
            filter.process(0.25, 2.0, 1.5).to_bits(),
            filter.process(0.0, 2.0, 1.5).to_bits(),
        ];

        assert_eq!(
            actual,
            [
                0x3db7_9301,
                0x3e0b_07ce,
                0x3dc0_6f1f,
                0xbe1c_b88b,
                0xbd73_cc27,
                0x3e9f_5d82,
            ]
        );
    }

    #[test]
    fn tpt_svf_keeps_coefficient_sanitization() {
        let mut sanitized = Svf::default();
        let mut explicit = Svf::default();

        assert_eq!(
            sanitized.process(1.0, f32::INFINITY, f32::NAN).to_bits(),
            explicit.process(1.0, 0.0, 1.0 / 1.5).to_bits()
        );

        let mut clamped = Svf::default();
        let mut boundary = Svf::default();
        assert_eq!(
            clamped.process(1.0, 9.0, 3.0).to_bits(),
            boundary.process(1.0, 8.0, 2.0).to_bits()
        );
    }

    #[test]
    fn louder_excitation_and_sensitivity_open_the_filter() {
        let tone = sine(16_384, 800.0, SR, 1.0);
        let p = params(0.0, 300.0, 2.0, 1.5, 180.0, 1.0);
        let mut quiet = EnvelopeFilter::new();
        quiet.set_rates(SR);
        let quiet = process(
            &mut quiet,
            &mono(&tone.iter().map(|x| x * 0.03).collect::<Vec<_>>()),
            &p,
        );
        let mut loud = EnvelopeFilter::new();
        loud.set_rates(SR);
        let loud = process(
            &mut loud,
            &mono(&tone.iter().map(|x| x * 0.6).collect::<Vec<_>>()),
            &p,
        );
        let quiet_level = channel_rms(&quiet[4096..]) / 0.03;
        let loud_level = channel_rms(&loud[4096..]) / 0.6;
        assert!(
            loud_level > quiet_level * 1.5,
            "louder excitation did not raise the spectral center"
        );

        let mut closed = EnvelopeFilter::new();
        closed.set_rates(SR);
        let closed = process(
            &mut closed,
            &mono(&tone.iter().map(|x| x * 0.2).collect::<Vec<_>>()),
            &params(-24.0, 300.0, 2.0, 1.5, 180.0, 1.0),
        );
        let mut open = EnvelopeFilter::new();
        open.set_rates(SR);
        let open = process(
            &mut open,
            &mono(&tone.iter().map(|x| x * 0.2).collect::<Vec<_>>()),
            &params(24.0, 300.0, 2.0, 1.5, 180.0, 1.0),
        );
        assert!(
            channel_rms(&open[4096..]) > channel_rms(&closed[4096..]) * 2.0,
            "sensitivity did not raise the spectral center"
        );
    }

    #[test]
    fn release_holds_the_open_filter_after_a_transient() {
        let mut input = vec![[0.8, 0.8]; 128];
        input.extend(std::iter::repeat_n([0.0, 0.0], 4000));
        input.extend(mono(&sine(1024, 800.0, SR, 0.02)));
        let mut fast = EnvelopeFilter::new();
        fast.set_rates(SR);
        let fast = process(&mut fast, &input, &params(0.0, 300.0, 2.0, 1.5, 30.0, 1.0));
        let mut slow = EnvelopeFilter::new();
        slow.set_rates(SR);
        let slow = process(&mut slow, &input, &params(0.0, 300.0, 2.0, 1.5, 600.0, 1.0));
        let probe = 4128..5152;
        assert!(
            channel_rms(&slow[probe.clone()]) > channel_rms(&fast[probe]) * 2.0,
            "long release did not preserve the opened filter"
        );
    }

    #[test]
    fn release_matches_the_documented_exponential_recurrence() {
        let release_ms = 180.0;
        let values = params(0.0, 300.0, 2.0, 1.5, release_ms, 0.0);
        let mut filter = EnvelopeFilter::new();
        filter.set_rates(SR);
        let mut excitation = [[0.8, 0.8]; 1_024];
        filter.process(&mut excitation, 1_024, &values);
        let initial = filter.envelope;
        let samples = (release_ms * 0.001 * SR) as usize;
        let mut silence = vec![[0.0; 2]; samples];
        filter.process(&mut silence, samples, &values);
        let expected = initial * (-1.0f32).exp();
        assert!(
            (filter.envelope - expected).abs() < expected * 5e-4,
            "release envelope {}, expected {expected}",
            filter.envelope
        );
    }

    #[test]
    fn dry_mix_is_transparent_even_above_the_wet_safety_range() {
        let mut input = mono(&sine(1024, 440.0, SR, 0.7));
        input[0] = [12.0, -12.0];
        let mut filter = EnvelopeFilter::new();
        filter.set_rates(SR);
        assert_eq!(
            process(
                &mut filter,
                &input,
                &params(24.0, 1000.0, 3.0, 4.0, 30.0, 0.0)
            ),
            input
        );
    }

    #[test]
    fn rapid_sweeps_stay_finite_and_bounded() {
        let mut filter = EnvelopeFilter::new();
        filter.set_rates(SR);
        for i in 0..128 {
            let x = mono(&sine(31, 100.0 + i as f32 * 30.0, SR, 0.9));
            let output = process(
                &mut filter,
                &x,
                &params(
                    -24.0 + (i % 49) as f32,
                    150.0 + (i % 6) as f32 * 170.0,
                    (i % 4) as f32,
                    0.5 + (i % 8) as f32 * 0.5,
                    30.0 + (i % 20) as f32 * 30.0,
                    1.0,
                ),
            );
            assert!(output
                .iter()
                .flatten()
                .all(|x| x.is_finite() && x.abs() < 64.0));
        }
    }

    #[test]
    fn coefficient_cadence_survives_callback_partitions() {
        let input = mono(&sine(1025, 730.0, SR, 0.5));
        let p = params(12.0, 450.0, 2.5, 1.2, 240.0, 0.8);
        let mut whole = EnvelopeFilter::new();
        whole.set_rates(SR);
        let expected = process(&mut whole, &input, &p);
        let mut split = EnvelopeFilter::new();
        split.set_rates(SR);
        let mut actual = input.clone();
        let mut start = 0;
        for size in [1, 7, 16, 31, 3, 64].into_iter().cycle() {
            if start == actual.len() {
                break;
            }
            let end = (start + size).min(actual.len());
            split.process(&mut actual[start..end], end - start, &p);
            start = end;
        }
        for (a, b) in actual.iter().zip(expected) {
            assert!((a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6);
        }
    }

    #[test]
    fn stereo_is_linked_without_audio_leakage() {
        let input = mono(&sine(2048, 650.0, SR, 0.4));
        let p = params(6.0, 400.0, 2.0, 1.5, 180.0, 1.0);
        let mut matched = EnvelopeFilter::new();
        matched.set_rates(SR);
        for frame in process(&mut matched, &input, &p) {
            assert!((frame[0] - frame[1]).abs() < 1e-6);
        }

        let left_only: Vec<Frame> = input.iter().map(|f| [f[0], 0.0]).collect();
        let mut separate = EnvelopeFilter::new();
        separate.set_rates(SR);
        assert!(process(&mut separate, &left_only, &p)
            .iter()
            .all(|f| f[1].abs() < 1e-7));
    }

    #[test]
    fn reset_restores_deterministic_state() {
        let input = mono(&sine(2048, 900.0, SR, 0.5));
        let p = params(12.0, 350.0, 2.0, 1.5, 180.0, 1.0);
        let mut dirty = EnvelopeFilter::new();
        dirty.set_rates(SR);
        let _ = process(&mut dirty, &input, &p);
        dirty.reset();
        let after_reset = process(&mut dirty, &input, &p);
        let mut fresh = EnvelopeFilter::new();
        fresh.set_rates(SR);
        assert_eq!(after_reset, process(&mut fresh, &input, &p));
    }

    #[test]
    fn junk_and_extreme_inputs_stay_finite_at_supported_rates() {
        for sr in [8000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let mut filter = EnvelopeFilter::new();
            filter.set_rates(sr);
            let mut p = params(f32::NAN, f32::INFINITY, -3.0, f32::NAN, -1.0, f32::INFINITY);
            p.v[0] = f32::INFINITY;
            let input = vec![[f32::NAN, f32::INFINITY]; 512];
            let junk = process(&mut filter, &input, &p);
            assert!(junk.iter().flatten().all(|x| x.is_finite()), "junk at {sr}");
            let sane = process(
                &mut filter,
                &mono(&sine(1024, 700.0, sr, 0.5)),
                &params(24.0, 1000.0, 3.0, 0.5, 600.0, 1.0),
            );
            assert!(
                sane.iter()
                    .flatten()
                    .all(|x| x.is_finite() && x.abs() < 64.0),
                "recovery at {sr}"
            );
        }
    }
}
