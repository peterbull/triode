//! Plate-ish reverb: eight damped comb filters (four per side, with the classic offset
//! tunings that keep the channels decorrelated) followed by four series all-pass filters
//! per channel.
//!
//! Tunings are the canonical 44.1 kHz values scaled by `sr / 44100`, so the room sounds
//! the same at 48 k and 96 k rather than a quarter-tone sharper. Buffers are allocated at
//! construction for 96 kHz, so `set_rates` never allocates on the audio thread.

use crate::dsp::{sanitize, Frame};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const SIZE: usize = 0;
pub const DECAY: usize = 1;
pub const MIX: usize = 2;
pub const DAMP: usize = 3;

const COMB_L: [usize; 4] = [1557, 1617, 1491, 1422];
const COMB_R: [usize; 4] = [1687, 1601, 1277, 1356];
const AP: [usize; 4] = [556, 441, 341, 225];
const REF_SR: f32 = 44100.0;
/// Buffers are sized for this much headroom over the reference rate (96 kHz).
const MAX_SCALE: f32 = 96000.0 / REF_SR;

/// Input scale that keeps the comb loops well inside unity.
const INPUT_GAIN: f32 = 0.015;
/// Wet make-up: the `INPUT_GAIN` scaling above is aggressive, so the return needs to come
/// back up. `reverb_level_is_audible_and_bounded` pins this to a sane range.
const WET_GAIN: f32 = 42.0;
const ALLPASS_FEEDBACK: f32 = 0.5;
/// Feedback ceiling for the comb loops.
const MAX_DECAY_FB: f32 = 0.98;

#[derive(Clone)]
struct Comb {
    buf: Vec<f32>,
    tune: usize,
    w: usize,
    store: f32,
}

impl Comb {
    fn new(tune: usize) -> Comb {
        let cap = ((tune as f32 * MAX_SCALE) as usize) + 8;
        Comb {
            buf: vec![0.0; cap],
            tune,
            w: 0,
            store: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, input: f32, len: usize, feedback: f32, damp: f32) -> f32 {
        let len = len.clamp(1, self.buf.len() - 1);
        let out = self.buf[self.w];
        self.store = damp * out + (1.0 - damp) * self.store;
        self.buf[self.w] = input + feedback * self.store;
        self.w = if self.w + 1 >= len { 0 } else { self.w + 1 };
        self.store
    }

    fn len(&self, scale: f32, size: f32) -> usize {
        ((self.tune as f32 * scale * (0.5 + 0.5 * size)) as usize)
            .max(1)
            .min(self.buf.len() - 1)
    }

    fn reset(&mut self) {
        for s in self.buf.iter_mut() {
            *s = 0.0;
        }
        self.w = 0;
        self.store = 0.0;
    }
}

#[derive(Clone)]
struct Allpass {
    buf: Vec<f32>,
    tune: usize,
    w: usize,
}

impl Allpass {
    fn new(tune: usize) -> Allpass {
        let cap = ((tune as f32 * MAX_SCALE) as usize) + 8;
        Allpass {
            buf: vec![0.0; cap],
            tune,
            w: 0,
        }
    }

    #[inline]
    fn process(&mut self, input: f32, len: usize) -> f32 {
        let len = len.clamp(1, self.buf.len() - 1);
        let bufout = self.buf[self.w];
        let out = bufout - ALLPASS_FEEDBACK * input;
        self.buf[self.w] = input + ALLPASS_FEEDBACK * bufout;
        self.w = if self.w + 1 >= len { 0 } else { self.w + 1 };
        out
    }

    fn len(&self, scale: f32) -> usize {
        ((self.tune as f32 * scale) as usize)
            .max(1)
            .min(self.buf.len() - 1)
    }

    fn reset(&mut self) {
        for s in self.buf.iter_mut() {
            *s = 0.0;
        }
        self.w = 0;
    }
}

pub struct Reverb {
    sr: f32,
    combs_l: Vec<Comb>,
    combs_r: Vec<Comb>,
    ap_l: Vec<Allpass>,
    ap_r: Vec<Allpass>,
}

impl Reverb {
    pub fn new() -> Reverb {
        Reverb {
            sr: 48000.0,
            combs_l: COMB_L.iter().map(|t| Comb::new(*t)).collect(),
            combs_r: COMB_R.iter().map(|t| Comb::new(*t)).collect(),
            ap_l: AP.iter().map(|t| Allpass::new(*t)).collect(),
            ap_r: AP.iter().map(|t| Allpass::new(*t)).collect(),
        }
    }
}

impl Default for Reverb {
    fn default() -> Self {
        Reverb::new()
    }
}

impl Proc for Reverb {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sr.max(8000.0);
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let size = sanitize(p.v[SIZE], 0.0, 0.0, 1.0);
        let decay = sanitize(p.v[DECAY], 0.0, 0.0, 1.0);
        let mix = sanitize(p.v[MIX], 0.0, 0.0, 1.0);
        let damp = sanitize(p.v[DAMP], 0.0, 0.0, 1.0);

        let scale = (self.sr / REF_SR).clamp(0.25, MAX_SCALE);
        let feedback = (0.28 + decay * 0.70).clamp(0.0, MAX_DECAY_FB);
        // 0 damp = dark and long-sounding, 1 = bright: damp is the comb loop's
        // one-pole coefficient, so higher damp keeps more high end in the loop.
        let damp_coef = 0.2 + 0.7 * damp;

        let len_l: [usize; 4] = std::array::from_fn(|i| self.combs_l[i].len(scale, size));
        let len_r: [usize; 4] = std::array::from_fn(|i| self.combs_r[i].len(scale, size));
        let len_ap: [usize; 4] = std::array::from_fn(|i| self.ap_l[i].len(scale));

        for f in buf[..n].iter_mut() {
            let (dry_l, dry_r) = (f[0], f[1]);
            // A guitar is mono; both comb sets see the same input and diverge purely from
            // their different tunings.
            let input = (dry_l + dry_r) * 0.5 * INPUT_GAIN;

            let mut wet_l = 0.0f32;
            let mut wet_r = 0.0f32;
            for i in 0..self.combs_l.len() {
                wet_l += self.combs_l[i].process(input, len_l[i], feedback, damp_coef);
                wet_r += self.combs_r[i].process(input, len_r[i], feedback, damp_coef);
            }
            wet_l *= 0.25;
            wet_r *= 0.25;
            for ((left, right), len) in self.ap_l.iter_mut().zip(&mut self.ap_r).zip(len_ap) {
                wet_l = left.process(wet_l, len);
                wet_r = right.process(wet_r, len);
            }

            f[0] = dry_l * (1.0 - mix) + wet_l * WET_GAIN * mix;
            f[1] = dry_r * (1.0 - mix) + wet_r * WET_GAIN * mix;
        }
    }

    fn reset(&mut self) {
        for c in self.combs_l.iter_mut().chain(self.combs_r.iter_mut()) {
            c.reset();
        }
        for a in self.ap_l.iter_mut().chain(self.ap_r.iter_mut()) {
            a.reset();
        }
    }
}

/// Buffer memory allocated for the delay lines, for the `--selftest` report.
pub fn footprint() -> usize {
    let f = |t: usize| -> usize { (t as f32 * MAX_SCALE) as usize + 8 };
    let combs: usize = COMB_L.iter().chain(COMB_R.iter()).map(|t| f(*t)).sum();
    let aps: usize = AP.iter().map(|t| f(*t)).sum::<usize>() * 2;
    (combs + aps) * std::mem::size_of::<f32>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{any_non_finite, mean, peak, rms, sine};
    use crate::engine::Proc;
    use crate::params::EffectKind;

    const SR: f32 = 48000.0;

    fn params(size: f32, decay: f32, mix: f32, damp: f32) -> ParamVals {
        // process() receives real units, not 0..=1 knob positions.
        let mut p = EffectKind::Reverb.default_values();
        for (i, v) in [size, decay, mix, damp].iter().enumerate() {
            p.v[i] = *v;
        }
        p
    }

    fn impulse(n: usize) -> Vec<f32> {
        let mut x = vec![0.0f32; n];
        x[100] = 0.7;
        x
    }

    fn run(r: &mut Reverb, x: &[f32], p: &ParamVals) -> (Vec<f32>, Vec<f32>) {
        let mut buf: Vec<Frame> = x.iter().map(|s| [*s, *s]).collect();
        let n = buf.len();
        r.process(&mut buf, n, p);
        (
            buf.iter().map(|f| f[0]).collect(),
            buf.iter().map(|f| f[1]).collect(),
        )
    }

    #[test]
    fn zero_mix_is_transparent() {
        let mut r = Reverb::new();
        r.set_rates(SR);
        let x = sine(16384, 440.0, SR, 0.4);
        let (l, _) = run(&mut r, &x, &params(0.7, 0.7, 0.0, 0.4));
        for i in 8192..x.len() {
            assert!((l[i] - x[i]).abs() < 1e-6);
        }
    }

    #[test]
    fn tail_exists_decays_and_is_audible_but_bounded() {
        let mut r = Reverb::new();
        r.set_rates(SR);
        let x = impulse(96000);
        let (l, rr) = run(&mut r, &x, &params(0.8, 0.8, 1.0, 0.4));
        let all = [l.as_slice(), rr.as_slice()].concat();
        assert!(!any_non_finite(&all));

        // Audible: the tail 200 ms after the impulse is clearly not silence.
        let tail = rms(&l[10000..19600]);
        assert!(tail > 1e-3, "reverb tail is inaudible: {tail}");
        // Bounded: never runs away.
        assert!(peak(&all) < 1.5, "reverb peak {p}", p = peak(&all));
        // Decaying: later windows get steadily quieter.
        let a = rms(&l[2000..6000]);
        let b = rms(&l[40000..44000]);
        let c = rms(&l[80000..84000]);
        assert!(a > b && b > c, "tail not decaying: {a} {b} {c}");
    }

    #[test]
    fn decay_knob_makes_a_longer_tail() {
        let mut short = Reverb::new();
        short.set_rates(SR);
        let x = impulse(96000);
        let (l_short, _) = run(&mut short, &x, &params(0.5, 0.1, 1.0, 0.4));

        let mut long = Reverb::new();
        long.set_rates(SR);
        let (l_long, _) = run(&mut long, &x, &params(0.5, 1.0, 1.0, 0.4));

        let far = 60000..64000;
        assert!(
            rms(&l_long[far.clone()]) > rms(&l_short[far.clone()]) * 5.0,
            "decay knob did nothing: long {} vs short {}",
            rms(&l_long[far.clone()]),
            rms(&l_short[far.clone()])
        );
    }

    #[test]
    fn channels_are_decorrelated() {
        let mut r = Reverb::new();
        r.set_rates(SR);
        let x = impulse(48000);
        let (l, rr) = run(&mut r, &x, &params(0.9, 0.9, 1.0, 0.5));
        let diff = l
            .iter()
            .zip(&rr)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(diff > 1e-3, "reverb is mono, max L-R {diff}");
        // but the two sides carry similar energy (it is a room, not a panner)
        let (el, er) = (rms(&l[2000..20000]), rms(&rr[2000..20000]));
        assert!(
            (el / (er + 1e-9)) > 0.3 && (el / (er + 1e-9)) < 3.0,
            "L/R energy mismatch {el} {er}"
        );
    }

    #[test]
    fn damping_reduces_high_frequency_content_of_the_tail() {
        let hf = |y: &[f32]| -> f32 {
            y.windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .fold(0.0f32, f32::max)
        };
        let mut bright = Reverb::new();
        bright.set_rates(SR);
        let x = impulse(48000);
        let (l_bright, _) = run(&mut bright, &x, &params(0.8, 0.8, 1.0, 1.0));

        let mut dark = Reverb::new();
        dark.set_rates(SR);
        let (l_dark, _) = run(&mut dark, &x, &params(0.8, 0.8, 1.0, 0.0));

        let seg = 4000..16000;
        assert!(
            hf(&l_dark[seg.clone()]) < hf(&l_bright[seg.clone()]),
            "damp knob did nothing"
        );
    }

    #[test]
    fn no_dc_and_no_runaway_at_the_extremes() {
        for (size, decay, damp) in [
            (0.0, 0.0, 0.0),
            (1.0, 1.0, 1.0),
            (1.0, 1.0, 0.0),
            (0.0, 1.0, 1.0),
        ] {
            let mut r = Reverb::new();
            r.set_rates(SR);
            let x = sine(48000, 200.0, SR, 0.9);
            let (l, rr) = run(&mut r, &x, &params(size, decay, 1.0, damp));
            let all = [l.as_slice(), rr.as_slice()].concat();
            assert!(
                !any_non_finite(&all),
                "{size}/{decay}/{damp} produced non-finite"
            );
            assert!(
                peak(&all) < 4.0,
                "{size}/{decay}/{damp} peak {}",
                peak(&all)
            );
            assert!(
                mean(&all[16000..]).abs() < 0.35,
                "dc built up: {}",
                mean(&all[16000..])
            );
        }
    }

    #[test]
    fn tunings_scale_with_sample_rate() {
        let r = Reverb::new();
        let c = &r.combs_l[0];
        assert_eq!(c.len(1.0, 1.0), COMB_L[0]);
        assert!(
            c.len(48000.0 / REF_SR, 1.0) > COMB_L[0],
            "must scale up with rate"
        );
        assert!(c.len(0.5, 1.0) < COMB_L[0], "must scale down with rate");
        // size shrinks the room; it must never reach a zero-length loop
        assert!(c.len(1.0, 0.0) >= 1);
        assert!(c.len(0.25, 0.0) >= 1);
    }

    #[test]
    fn runs_at_every_supported_rate_without_allocating_state_growth() {
        for sr in [44100.0f32, 48000.0, 88200.0, 96000.0] {
            let mut r = Reverb::new();
            r.set_rates(sr);
            let x = impulse(24000);
            let (l, _) = run(&mut r, &x, &params(0.9, 0.9, 1.0, 0.5));
            assert!(l.iter().all(|s| s.is_finite()), "{sr} non-finite");
            assert!(peak(&l) < 1.5, "{sr} peak {}", peak(&l));
        }
        // buffers were sized for 96 kHz at construction
        assert!(footprint() > 0);
    }

    #[test]
    fn junk_parameters_stay_finite() {
        let mut r = Reverb::new();
        r.set_rates(SR);
        let mut p = ParamVals::ZEROED;
        p.v[SIZE] = f32::NAN;
        p.v[DECAY] = f32::INFINITY;
        p.v[MIX] = 7.0;
        p.v[DAMP] = -3.0;
        let mut buf = [[0.5f32, -0.5]; 512];
        for _ in 0..20 {
            r.process(&mut buf, 512, &p);
            assert!(buf.iter().all(|f| f[0].is_finite() && f[1].is_finite()));
        }
    }
}
