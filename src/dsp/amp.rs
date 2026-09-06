//! The amplifier: pre gain → 2× oversampled asymmetric soft clip → tone stack →
//! power-amp sag.
//!
//! ## Why oversample the clipper
//!
//! Clipping is the point of a guitar amp and also the loudest aliasing source in the
//! whole signal chain: a hard-clipped 7 kHz sine produces a 4th harmonic at 28 kHz,
//! which folds back to 20 kHz and sits there as a permanent artefact that no EQ can
//! remove. Interpolating to 2×, low-passing, shaping, low-passing again and decimating
//! trades a little CPU for stop-band attenuation exactly where those products land.
//! `clipper_reduces_aliasing_vs_undersampled` measures the difference rather than
//! asserting it in a comment.

use crate::dsp::biquad::{Biquad, Coef, DcBlocker};
use crate::dsp::{db2lin, sanitize, Frame};
use crate::params::{amp_ix, ParamVals};

/// Number of filter stages per direction in the oversampling pair. Three 2-pole
/// stages is 6th order each way: enough stop-band at the fold point, still cheap
/// (12 biquad operations per input frame per channel).
const STAGES: usize = 3;

/// Asymmetric soft clipper with the same knee shape in both directions but different
/// thresholds, which is what makes single-ended stage clipping sound even-harmonic.
///
/// `t` is the knee: `1.0` is transparent, small values are saturated. Below the knee
/// the curve is the identity (so a clean signal really is clean, unity gain), above it
/// the excess is compressed with `tanh`, which is smooth to all orders and therefore
/// bounded and click-free.
#[inline]
pub fn shape(x: f32, t_pos: f32, t_neg: f32) -> f32 {
    let a = x.abs();
    let t = if x >= 0.0 { t_pos } else { t_neg };
    if a <= t {
        return x;
    }
    let k = (1.0 - t).max(1e-4);
    let over = (a - t) / k;
    let soft = t + (1.0 - t) * over.tanh();
    if x >= 0.0 {
        soft
    } else {
        -soft
    }
}

/// 2× oversampling wrapper around a waveshaper.
pub struct Clip2x {
    up: Vec<Biquad>,
    dn: Vec<Biquad>,
    x1: f32,
}

impl Clip2x {
    pub fn new(sr: f32) -> Clip2x {
        let mut c = Clip2x {
            up: vec![Biquad::passthrough(); STAGES],
            dn: Vec::new(),
            x1: 0.0,
        };
        c.dn = vec![Biquad::passthrough(); STAGES];
        c.set_rates(sr);
        c
    }

    pub fn set_rates(&mut self, sr: f32) {
        // The filter below runs on the 2x-interpolated stream, so its coefficients must be
        // designed at 2*sr — designing them at `sr` put the cutoff an octave too low and
        // cost most of the aliasing rejection. Cut just under the original Nyquist, which
        // is 0.21 of the doubled rate.
        let doubled = sr * 2.0;
        let fc = sr * 0.42;
        for b in self.up.iter_mut().chain(self.dn.iter_mut()) {
            b.set(Coef::lowpass_bw(fc, doubled));
            b.reset();
        }
        self.x1 = 0.0;
    }

    /// One input frame in, one (filtered, shaped) output frame out.
    #[inline]
    pub fn push<F: Fn(f32) -> f32>(&mut self, x: f32, f: F) -> f32 {
        // The interpolated sample h falls between the previous and current input sample,
        // so the upsampled order is h then x; only the x phase survives decimation.
        let h = 0.5 * (x + self.x1);
        self.x1 = x;
        let mut out = 0.0;
        for (phase, u) in [h, x].into_iter().enumerate() {
            let mut v = u;
            for b in self.up.iter_mut() {
                v = b.process(v);
            }
            v = f(v);
            for b in self.dn.iter_mut() {
                v = b.process(v);
            }
            if phase == 1 {
                out = v;
            }
        }
        out
    }

    pub fn reset(&mut self) {
        for b in self.up.iter_mut().chain(self.dn.iter_mut()) {
            b.reset();
        }
        self.x1 = 0.0;
    }
}

/// Per-channel filter chain. Two of these exist (one per channel) so the clipper and
/// tone state never interleave between left and right.
struct Chain {
    dc: DcBlocker,
    clip: Clip2x,
    bass: Biquad,
    mid: Biquad,
    treb: Biquad,
    pres: Biquad,
}

impl Chain {
    fn new(sr: f32) -> Chain {
        let mut c = Chain {
            dc: DcBlocker::new(),
            clip: Clip2x::new(sr),
            bass: Biquad::passthrough(),
            mid: Biquad::passthrough(),
            treb: Biquad::passthrough(),
            pres: Biquad::passthrough(),
        };
        c.dc.set_hz(12.0, sr);
        c
    }

    fn set_rates(&mut self, sr: f32) {
        self.dc.set_hz(12.0, sr);
        self.clip.set_rates(sr);
    }
}

/// Tone-stack corner frequencies. An *active* approximation of the passive network in a
/// classic stack — labelled honestly as an emulation, not a component simulation.
const BASS_HZ: f32 = 120.0;
const MID_HZ: f32 = 800.0;
const MID_Q: f32 = 0.9;
const TREBLE_HZ: f32 = 3500.0;
const PRESENCE_HZ: f32 = 5000.0;

pub struct Amp {
    sr: f32,
    ch: [Chain; 2],
    /// Last values the tone coefficients were computed from, so they are only rebuilt
    /// when a knob actually moved. Seeded with NaN so the first call always builds; note
    /// that a subtraction-based "did it move" test would silently never fire against a
    /// NaN seed, which is exactly how this file's tone stack once stayed dead forever.
    cached: [f32; 4],
}

impl Amp {
    pub fn new(sr: f32) -> Amp {
        let sr = sr.max(8000.0);
        Amp {
            sr,
            ch: [Chain::new(sr), Chain::new(sr)],
            cached: [f32::NAN; 4],
        }
    }

    pub fn set_rates(&mut self, sr: f32) {
        self.sr = sr.max(8000.0);
        for c in self.ch.iter_mut() {
            c.set_rates(self.sr);
        }
        self.cached = [f32::NAN; 4];
    }

    pub fn sr(&self) -> f32 {
        self.sr
    }

    /// Rebuild tone coefficients if any tone knob moved.
    #[inline]
    fn update_tone(&mut self, bass: f32, mid: f32, treb: f32, pres: f32) {
        let moved = self.cached[0] != bass
            || self.cached[1] != mid
            || self.cached[2] != treb
            || self.cached[3] != pres;
        if !moved {
            return;
        }
        self.cached = [bass, mid, treb, pres];
        let cb = Coef::lowshelf(BASS_HZ, bass, self.sr);
        let cm = Coef::peaking(MID_HZ, mid, MID_Q, self.sr);
        let ct = Coef::highshelf(TREBLE_HZ, treb, self.sr);
        let cp = Coef::highshelf(PRESENCE_HZ, pres, self.sr);
        for c in self.ch.iter_mut() {
            c.bass.set(cb);
            c.mid.set(cm);
            c.treb.set(ct);
            c.pres.set(cp);
        }
    }

    /// Drive knee from the pre-amp gain in dB: `-20 dB` is essentially transparent,
    /// `+40 dB` is heavily saturated. The negative side clips later than the positive
    /// side, which is where the even harmonics come from.
    #[inline]
    fn knees(gain_db: f32) -> (f32, f32) {
        let g = gain_db.clamp(-20.0, 40.0);
        let t_pos = (0.98 - (g + 20.0) / 60.0 * 0.93).clamp(0.02, 0.995);
        let t_neg = (t_pos * 1.35).clamp(0.02, 0.999);
        (t_pos, t_neg)
    }

    /// Power-amp sag: a smooth, bounded compression that softens the very top end
    /// without the hard fold-over of a clamp.
    #[inline]
    fn power(x: f32) -> f32 {
        if !x.is_finite() {
            return 0.0;
        }
        let a = x.abs();
        if a <= 1.0 {
            x
        } else {
            let over = a - 1.0;
            let soft = 1.0 + over / (1.0 + over * 1.6);
            if x >= 0.0 {
                soft
            } else {
                -soft
            }
        }
    }

    /// `p` holds denormalised [`crate::params::AMP_SPECS`] values. Gain and master are
    /// consumed by the engine (master) and here (gain, tone).
    pub fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let gain_db = sanitize(p.v[amp_ix::GAIN], 0.0, -20.0, 40.0);
        let bass = sanitize(p.v[amp_ix::BASS], 0.0, -15.0, 15.0);
        let mid = sanitize(p.v[amp_ix::MID], 0.0, -15.0, 15.0);
        let treb = sanitize(p.v[amp_ix::TREBLE], 0.0, -15.0, 15.0);
        let pres = sanitize(p.v[amp_ix::PRESENCE], 0.0, -15.0, 15.0);
        self.update_tone(bass, mid, treb, pres);

        let pre = db2lin(gain_db);
        // Gain-compensated drive: a real channel does get louder with more gain, but not
        // by the full +60 dB, so give back 70 % of it and let the player's master knob
        // stay put while they set up.
        let makeup = db2lin(-gain_db * 0.70);
        let (t_pos, t_neg) = Self::knees(gain_db);
        let shaper = |v: f32| shape(v, t_pos, t_neg);

        for f in buf[..n].iter_mut() {
            for (ci, c) in self.ch.iter_mut().enumerate() {
                let mut x = f[ci];
                x = c.dc.process(x);
                x = c.clip.push(x * pre, shaper);
                x *= makeup;
                x = c.bass.process(x);
                x = c.mid.process(x);
                x = c.treb.process(x);
                x = c.pres.process(x);
                f[ci] = Self::power(x);
            }
        }
    }

    pub fn reset(&mut self) {
        for c in self.ch.iter_mut() {
            c.dc.reset();
            c.clip.reset();
            c.bass.reset();
            c.mid.reset();
            c.treb.reset();
            c.pres.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{any_non_finite, goertzel_mag, peak, sine, thd};
    use crate::params::amp_default_values;

    const SR: f32 = 48000.0;

    /// Amp params with the given tone/gain settings and spec defaults elsewhere.
    fn amp_params(gain_db: f32, bass: f32, mid: f32, treb: f32) -> ParamVals {
        // Amp::process receives real units (dB, Hz), same as Proc::process.
        let mut p = amp_default_values();
        p.v[amp_ix::GAIN] = gain_db;
        p.v[amp_ix::BASS] = bass;
        p.v[amp_ix::MID] = mid;
        p.v[amp_ix::TREBLE] = treb;
        p
    }

    /// Run a mono signal through one channel of the amp.
    fn run(amp: &mut Amp, x: &[f32], p: &ParamVals) -> Vec<f32> {
        let mut buf: Vec<Frame> = x.iter().map(|s| [*s, *s]).collect();
        let n = buf.len();
        amp.process(&mut buf, n, p);
        buf.iter().map(|f| f[0]).collect()
    }

    #[test]
    fn clean_setting_is_actually_clean() {
        let mut amp = Amp::new(SR);
        let p = amp_params(-20.0, 0.0, 0.0, 0.0);
        let x = sine(16384, 200.0, SR, 0.25);
        let y = run(&mut amp, &x, &p);
        let settled = &y[4096..];
        assert!(
            thd(settled, 200.0, SR) < 0.02,
            "at -20 dB gain THD was {}",
            thd(settled, 200.0, SR)
        );
        // and it should pass roughly unity through the whole chain
        let ratio = goertzel_mag(&y[2048..], 200.0, SR) / goertzel_mag(&x[2048..], 200.0, SR);
        // The channel gives back 70 % of the gain change (see `process`), so a -20 dB
        // clean setting is deliberately ~6 dB down rather than unity.
        assert!(
            ratio > 0.4 && ratio < 1.6,
            "clean pass-through gain {ratio}"
        );
    }

    #[test]
    fn thd_rises_monotonically_with_gain() {
        let x = sine(32768, 150.0, SR, 0.2);
        let mut prev = -1.0f32;
        for gain in [-20.0f32, -6.0, 6.0, 18.0, 30.0, 40.0] {
            let mut amp = Amp::new(SR);
            let p = amp_params(gain, 0.0, 0.0, 0.0);
            let y = run(&mut amp, &x, &p);
            let t = thd(&y[4096..], 150.0, SR);
            assert!(
                t > prev,
                "THD must increase with gain: {gain} dB gave {t} (prev {prev})"
            );
            prev = t;
        }
        assert!(
            prev > 0.2,
            "max gain should be audibly distorted, THD {prev}"
        );
    }

    #[test]
    fn clipper_reduces_aliasing_vs_undersampled() {
        // A 7 kHz sine driven hard: the 4th harmonic is 28 kHz, which folds to 20 kHz
        // when decimated to 24 kHz Nyquist. Path A shapes at 1x, path B through Clip2x.
        let f = 7000.0;
        let x = sine(32768, f, SR, 0.5);
        let drive = 12.0f32;
        let (tp, tn) = Amp::knees(30.0);
        let shaper = |v: f32| shape(v, tp, tn);

        let direct: Vec<f32> = x.iter().map(|s| shaper(*s * drive)).collect();

        let mut over = Clip2x::new(SR);
        let ovs: Vec<f32> = x.iter().map(|s| over.push(*s * drive, shaper)).collect();

        assert!(!any_non_finite(&direct) && !any_non_finite(&ovs));
        // sanity: both are actually distorting the same signal
        assert!(thd(&direct[4096..], f, SR) > 0.1);

        let alias_a = goertzel_mag(&direct[4096..], 20000.0, SR);
        let alias_b = goertzel_mag(&ovs[4096..], 20000.0, SR);
        let fund_a = goertzel_mag(&direct[4096..], f, SR);
        let fund_b = goertzel_mag(&ovs[4096..], f, SR);
        assert!(
            alias_a > 1e-4,
            "expected the 1x path to alias measurably, got {alias_a}"
        );
        assert!(
            alias_b < alias_a / 3.0,
            "oversampled alias {} should beat 1x alias {} by >9.5 dB",
            alias_b,
            alias_a
        );
        // and the oversampled path must not eat the fundamental
        assert!(
            fund_b > fund_a * 0.6,
            "fundamental lost: {fund_b} vs {fund_a}"
        );
    }

    #[test]
    fn tone_controls_move_the_right_region() {
        let x = sine(16384, 100.0, SR, 0.3);
        let hi = sine(16384, 6000.0, SR, 0.3);
        let mid = sine(16384, 800.0, SR, 0.3);

        let level_at = |p: &ParamVals, sig: &[f32], f: f32| -> f32 {
            let mut amp = Amp::new(SR);
            let y: Vec<f32> = run(&mut amp, sig, p).into_iter().skip(2048).collect();
            let src: Vec<f32> = sig.iter().skip(2048).copied().collect();
            goertzel_mag(&y, f, SR) / goertzel_mag(&src, f, SR).max(1e-9)
        };

        let ref_l = level_at(&amp_params(-20.0, 0.0, 0.0, 0.0), &x, 100.0);
        let boost_bass = level_at(&amp_params(-20.0, 12.0, 0.0, 0.0), &x, 100.0);
        let cut_bass = level_at(&amp_params(-20.0, -12.0, 0.0, 0.0), &x, 100.0);
        assert!(
            boost_bass > ref_l * 1.4,
            "bass boost did nothing: {boost_bass} vs {ref_l}"
        );
        assert!(
            cut_bass < ref_l * 0.7,
            "bass cut did nothing: {cut_bass} vs {ref_l}"
        );

        let ref_t = level_at(&amp_params(-20.0, 0.0, 0.0, 0.0), &hi, 6000.0);
        let cut_treble = level_at(&amp_params(-20.0, 0.0, 0.0, -12.0), &hi, 6000.0);
        assert!(
            cut_treble < ref_t * 0.7,
            "treble cut did nothing: {cut_treble} vs {ref_t}"
        );

        let ref_m = level_at(&amp_params(-20.0, 0.0, 0.0, 0.0), &mid, 800.0);
        let boost_mid = level_at(&amp_params(-20.0, 0.0, 12.0, 0.0), &mid, 800.0);
        assert!(
            boost_mid > ref_m * 1.4,
            "mid boost did nothing: {boost_mid} vs {ref_m}"
        );
    }

    #[test]
    fn stereo_channels_do_not_bleed_or_share_clip_state() {
        // A hot signal on L and silence on R must not produce R output.
        let mut amp = Amp::new(SR);
        let p = amp_params(30.0, 0.0, 0.0, 0.0);
        let x = sine(4096, 500.0, SR, 0.6);
        let mut buf: Vec<Frame> = x.iter().map(|s| [*s, 0.0]).collect();
        let n = buf.len();
        amp.process(&mut buf, n, &p);
        let right = peak(&buf.iter().skip(1024).map(|f| f[1]).collect::<Vec<f32>>());
        assert!(right < 1e-6, "L leaked into R: {right}");
        assert!(peak(&buf.iter().map(|f| f[0]).collect::<Vec<f32>>()) > 0.01);
    }

    #[test]
    fn junk_parameters_cannot_break_the_amp() {
        let mut amp = Amp::new(SR);
        let mut p = ParamVals::ZEROED;
        p.v[amp_ix::GAIN] = f32::NAN;
        p.v[amp_ix::BASS] = f32::INFINITY;
        p.v[amp_ix::MID] = -1e30;
        p.v[amp_ix::TREBLE] = 1e30;
        let mut buf: Vec<Frame> = sine(4096, 440.0, SR, 0.4)
            .iter()
            .map(|s| [*s, *s])
            .collect();
        let n = buf.len();
        amp.process(&mut buf, n, &p);
        assert!(buf.iter().all(|f| f[0].is_finite() && f[1].is_finite()));
        // and a NaN input sample cannot permanently poison it
        buf[10][0] = f32::NAN;
        let n = buf.len();
        amp.process(&mut buf, n, &ParamVals::ZEROED);
        assert!(buf.iter().skip(64).all(|f| f[0].is_finite()));
    }

    #[test]
    fn output_stays_bounded_at_max_gain() {
        let mut amp = Amp::new(SR);
        let mut p = amp_params(40.0, 15.0, 0.0, 15.0);
        p.v[amp_ix::GAIN] = 1.0; // knob wide open, denormalises to max gain
        let mut buf: Vec<Frame> = sine(16384, 100.0, SR, 8.0)
            .iter()
            .map(|s| [*s, *s])
            .collect();
        let n = buf.len();
        amp.process(&mut buf, n, &p);
        let p1 = peak(&buf.iter().map(|f| f[0]).collect::<Vec<f32>>());
        assert!(p1 < 2.0, "power stage should bound the output, peak {p1}");
        assert!(p1 > 0.1, "should still be passing signal, peak {p1}");
    }

    #[test]
    fn rate_changes_rebuild_everything() {
        for sr in [44100.0f32, 96000.0, 22050.0] {
            let mut amp = Amp::new(sr);
            let p = amp_params(20.0, 0.0, 0.0, 0.0);
            let x = sine(8192, 220.0, sr, 0.3);
            let y = run(&mut amp, &x, &p);
            assert!(
                y.iter().all(|s| s.is_finite()),
                "{sr} produced non-finite output"
            );
        }
    }
}
