//! Biquad filters with RBJ Audio EQ Cookbook coefficients, plus the one-pole and
//! DC-blocker helpers the rest of the engine uses.
//!
//! Coefficients are computed once per `set`/`apply` call, never per sample; the
//! engine calls them at control rate (once per chunk).

use crate::dsp::sanitize;

/// Unnormalised biquad coefficients (a0 is divided out on `apply`).
#[derive(Clone, Copy, Debug)]
pub struct Coef {
    pub a0: f32,
    pub a1: f32,
    pub a2: f32,
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
}

/// Q for a maximally-flat (Butterworth) 2-pole section.
const BUTTERWORTH_Q: f32 = core::f32::consts::FRAC_1_SQRT_2;

impl Coef {
    #[inline]
    fn terms(a0: f32, a1: f32, a2: f32, b0: f32, b1: f32, b2: f32) -> Coef {
        Coef {
            a0,
            a1,
            a2,
            b0,
            b1,
            b2,
        }
    }

    /// `(f0, Q)` into `(sin(w0), cos(w0), alpha)`. `f0` is clamped to a range where
    /// the coefficients stay well-conditioned.
    #[inline]
    fn warp(f0: f32, q: f32, sr: f32) -> (f32, f32, f32) {
        let f = f0.clamp(1.0, sr * 0.45);
        let q = q.max(0.1);
        let w0 = 2.0 * core::f32::consts::PI * f / sr;
        let (sin, cos) = w0.sin_cos();
        (sin, cos, sin / (2.0 * q))
    }

    pub fn lowpass(f0: f32, q: f32, sr: f32) -> Coef {
        let (_sin, cos, alpha) = Self::warp(f0, q, sr);
        let a0 = 1.0 + alpha;
        Self::terms(
            a0,
            -2.0 * cos,
            1.0 - alpha,
            (1.0 - cos) / 2.0,
            1.0 - cos,
            (1.0 - cos) / 2.0,
        )
    }

    pub fn highpass(f0: f32, q: f32, sr: f32) -> Coef {
        let (_sin, cos, alpha) = Self::warp(f0, q, sr);
        let a0 = 1.0 + alpha;
        Self::terms(
            a0,
            -2.0 * cos,
            1.0 - alpha,
            (1.0 + cos) / 2.0,
            -(1.0 + cos),
            (1.0 + cos) / 2.0,
        )
    }

    /// `gain_db` boost/cut centred on `f0`.
    pub fn peaking(f0: f32, gain_db: f32, q: f32, sr: f32) -> Coef {
        let (_sin, cos, alpha) = Self::warp(f0, q, sr);
        let a = 10f32.powf(gain_db / 40.0);
        let a0 = 1.0 + alpha / a;
        Self::terms(
            a0,
            -2.0 * cos,
            1.0 - alpha / a,
            1.0 + alpha * a,
            -2.0 * cos,
            1.0 - alpha * a,
        )
    }

    /// RBJ low/high shelf, transcribed from the W3C cookbook note.
    ///
    /// Two things are easy to get wrong here and both are silent:
    ///
    /// * the **a-side `cos` term flips sign between the two shelves**, so the pair cannot
    ///   share one set of denominator coefficients. Sharing them (as this file once did)
    ///   leaves a *0 dB* high shelf cutting ~25 dB at 200 Hz, because a shelf that should
    ///   be flat is left with the other shelf's denominator.
    /// * `b1` pairs `(A-1)` with `(A+1)cos`, not the other way round.
    ///
    /// Verified against the asymptotes: 0 dB is unity at both ends, and the shelf end hits
    /// the requested gain exactly at DC / Nyquist.
    fn shelf(f0: f32, gain_db: f32, sr: f32, high: bool) -> Coef {
        let f = f0.clamp(1.0, sr * 0.45);
        let w0 = 2.0 * core::f32::consts::PI * f / sr;
        let (sin, cos) = w0.sin_cos();
        let a = 10f32.powf(gain_db / 40.0);
        // Shelf slope S = 1, which reduces alpha to sin/2 * sqrt(2A).
        let alpha = sin / 2.0 * (2.0 * a).sqrt();
        let ap = a + 1.0;
        let am = a - 1.0;
        let t = 2.0 * a.sqrt() * alpha;
        if high {
            Coef::terms(
                ap - am * cos + t,
                2.0 * (am - ap * cos),
                ap - am * cos - t,
                a * (ap + am * cos + t),
                -2.0 * a * (am + ap * cos),
                a * (ap + am * cos - t),
            )
        } else {
            Coef::terms(
                ap + am * cos + t,
                -2.0 * (am + ap * cos),
                ap + am * cos - t,
                a * (ap - am * cos + t),
                2.0 * a * (am - ap * cos),
                a * (ap - am * cos - t),
            )
        }
    }

    pub fn lowshelf(f0: f32, gain_db: f32, sr: f32) -> Coef {
        Self::shelf(f0, gain_db, sr, false)
    }

    pub fn highshelf(f0: f32, gain_db: f32, sr: f32) -> Coef {
        Self::shelf(f0, gain_db, sr, true)
    }

    /// Butterworth 2-pole lowpass.
    pub fn lowpass_bw(f0: f32, sr: f32) -> Coef {
        Self::lowpass(f0, BUTTERWORTH_Q, sr)
    }
}

/// Transposed-direct-form II biquad. State is four floats; `set` is cheap so it
/// can be called at control rate.
#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Default for Biquad {
    fn default() -> Self {
        Self::passthrough()
    }
}

impl Biquad {
    pub const fn passthrough() -> Self {
        Biquad {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    pub fn new(c: Coef) -> Biquad {
        let mut b = Biquad::passthrough();
        b.set(c);
        b
    }

    pub fn set(&mut self, c: Coef) {
        let a0 = if c.a0.abs() < 1e-20 { 1.0 } else { c.a0 };
        self.b0 = c.b0 / a0;
        self.b1 = c.b1 / a0;
        self.b2 = c.b2 / a0;
        self.a1 = c.a1 / a0;
        self.a2 = c.a2 / a0;
    }

    pub fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        // TDF-II: fewer round-trips of quantised state than direct form I.
        let y = crate::dsp::flush_denormal(self.b0 * x + self.x1);
        if !y.is_finite() {
            self.reset();
            return 0.0;
        }
        self.x1 = crate::dsp::flush_denormal(self.b1 * x - self.a1 * y + self.x2);
        self.x2 = crate::dsp::flush_denormal(self.b2 * x - self.a2 * y);
        self.y1 = y;
        y
    }

    /// |H(e^jw)| at `f` — used by tests to check coefficients analytically.
    pub fn gain_at(&self, f: f32, sr: f32) -> f32 {
        let w = 2.0 * core::f32::consts::PI * f / sr;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        // numerator: b0 + b1 e^-jw + b2 e^-2jw
        let nr = self.b0 + self.b1 * c1 + self.b2 * c2;
        let ni = -(self.b1 * s1 + self.b2 * s2);
        let dr = 1.0 + self.a1 * c1 + self.a2 * c2;
        let di = -(self.a1 * s1 + self.a2 * s2);
        let n = (nr * nr + ni * ni).sqrt();
        let d = (dr * dr + di * di).sqrt();
        if d < 1e-20 {
            0.0
        } else {
            n / d
        }
    }
}

/// One-pole lowpass/highpass — the cheap filter for tone controls, envelope
/// followers and gate/release ramps.
#[derive(Clone, Copy, Debug)]
pub struct OnePole {
    /// 0..1 pole (a = e^(-1/(tau*sr))).
    a: f32,
    z: f32,
}

impl Default for OnePole {
    fn default() -> Self {
        Self::new()
    }
}

impl OnePole {
    pub const fn new() -> OnePole {
        OnePole { a: 0.0, z: 0.0 }
    }

    pub fn set_coef(&mut self, a: f32) {
        self.a = sanitize(a, 0.0, 0.0, 0.999_999);
    }

    /// -3 dB corner at `hz`.
    pub fn set_hz(&mut self, hz: f32, sr: f32) {
        self.set_coef(crate::dsp::hz_coef(hz, sr));
    }

    /// 63 % time constant at `tau_s` seconds.
    pub fn set_tau(&mut self, tau_s: f32, sr: f32) {
        self.set_coef(crate::dsp::tau_coef(tau_s, sr));
    }

    #[inline]
    pub fn lowpass(&mut self, x: f32) -> f32 {
        self.z = crate::dsp::flush_denormal(x * (1.0 - self.a) + self.z * self.a);
        self.z
    }

    /// Lowpass, but read the *previous* state (fast attack path for envelopes).
    #[inline]
    pub fn lowpass_prev(&mut self, x: f32) -> f32 {
        let prev = self.z;
        self.z = crate::dsp::flush_denormal(x * (1.0 - self.a) + self.z * self.a);
        prev
    }

    /// Highpass = input minus the lowpassed signal (`lp` must be fed the same x).
    #[inline]
    pub fn highpass(&mut self, x: f32) -> f32 {
        let lp = self.lowpass(x);
        x - lp
    }

    #[inline]
    pub fn state(&self) -> f32 {
        self.z
    }

    pub fn reset(&mut self) {
        self.z = 0.0;
    }
}

/// DC blocker: `y[n] = x[n] - x[n-1] + R*y[n-1]`. Asymmetric clippers generate DC,
/// and the amp stage needs it gone before it hits the cab sim.
#[derive(Clone, Copy, Debug, Default)]
pub struct DcBlocker {
    r: f32,
    x1: f32,
    y1: f32,
}

impl DcBlocker {
    pub fn new() -> DcBlocker {
        DcBlocker {
            r: 0.0,
            x1: 0.0,
            y1: 0.0,
        }
    }

    /// Corner in Hz; R follows from the standard first-order relation.
    pub fn set_hz(&mut self, hz: f32, sr: f32) {
        let r = 1.0 - 2.0 * core::f32::consts::PI * hz.max(0.01) / sr.max(1.0);
        self.r = sanitize(r, 0.0, 0.0, 0.999_999);
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = x - self.x1 + self.r * self.y1;
        self.x1 = crate::dsp::flush_denormal(x);
        self.y1 = if y.is_finite() {
            crate::dsp::flush_denormal(y)
        } else {
            self.reset();
            0.0
        };
        self.y1
    }

    pub fn reset(&mut self) {
        self.x1 = 0.0;
        self.y1 = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{goertzel_mag, sine};
    use crate::dsp::lin2db;

    const SR: f32 = 48000.0;

    fn db_near(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol
    }

    /// Run `x` through a biquad and report the steady-state magnitude at `f`.
    fn measured(c: Coef, f: f32) -> f32 {
        let mut bq = Biquad::new(c);
        let x = sine(8192, f, SR, 0.5);
        let y: Vec<f32> = x.iter().map(|s| bq.process(*s)).collect();
        let mag = goertzel_mag(&y[1024..], f, SR);
        mag / goertzel_mag(&x[1024..], f, SR)
    }

    #[test]
    fn lowpass_passes_low_and_stops_high() {
        let c = Coef::lowpass(1000.0, 0.707, SR);
        assert!(db_near(lin2db(measured(c, 100.0)), 0.0, 0.3));
        assert!(
            measured(c, 8000.0) < 0.03,
            "8 kHz should be down -36 dB, got {}",
            measured(c, 8000.0)
        );
    }

    #[test]
    fn highpass_passes_high_and_stops_low() {
        let c = Coef::highpass(200.0, 0.707, SR);
        assert!(db_near(lin2db(measured(c, 4000.0)), 0.0, 0.3));
        assert!(measured(c, 20.0) < 0.02);
    }

    #[test]
    fn peaking_boosts_only_around_f0() {
        let c = Coef::peaking(1000.0, 12.0, 1.0, SR);
        assert!(db_near(lin2db(measured(c, 1000.0)), 12.0, 0.2), "peak gain");
        assert!(db_near(lin2db(measured(c, 60.0)), 0.0, 0.3));
        assert!(db_near(lin2db(measured(c, 12000.0)), 0.0, 0.4));
    }

    #[test]
    fn shelves_hold_their_gain_at_the_far_end() {
        // Assertions at DC / Nyquist, where a shelf's gain is exact. Measured one octave
        // from the corner it is still mid-transition and would read ~0.9 dB low.
        for (g, name) in [(12.0f32, "boost"), (-15.0, "cut")] {
            let lo = Biquad::new(Coef::lowshelf(150.0, g, SR));
            assert!(
                db_near(lin2db(lo.gain_at(1.0, SR)), g, 0.05),
                "low shelf {name} at DC"
            );
            assert!(
                db_near(lin2db(lo.gain_at(SR * 0.4999, SR)), 0.0, 0.05),
                "low shelf {name} at Nyquist"
            );

            let hi = Biquad::new(Coef::highshelf(4000.0, g, SR));
            assert!(
                db_near(lin2db(hi.gain_at(SR * 0.4999, SR)), g, 0.05),
                "high shelf {name} at Nyquist"
            );
            assert!(
                db_near(lin2db(hi.gain_at(1.0, SR)), 0.0, 0.05),
                "high shelf {name} at DC"
            );
        }
        // A 0 dB shelf must be transparent at *both* ends. This is the regression that
        // mattered: sharing one denominator between the two shelves made a flat treble
        // knob suck 25 dB of bass, which no "does the boost work" test can see.
        for c in [
            Coef::lowshelf(150.0, 0.0, SR),
            Coef::highshelf(4000.0, 0.0, SR),
        ] {
            let b = Biquad::new(c);
            for f in [20.0, 200.0, 1000.0, 5000.0, 15000.0] {
                assert!(
                    db_near(lin2db(b.gain_at(f, SR)), 0.0, 0.02),
                    "0 dB shelf not flat at {f} Hz"
                );
            }
        }
        // and the gain really does land on the far side, in steady state
        let lo = Coef::lowshelf(150.0, 10.0, SR);
        assert!(
            db_near(lin2db(measured(lo, 30.0)), 10.0, 0.6),
            "low shelf must boost bass"
        );
        assert!(db_near(lin2db(measured(lo, 8000.0)), 0.0, 0.4));
        let hi = Coef::highshelf(4000.0, -6.0, SR);
        assert!(
            db_near(lin2db(measured(hi, 16000.0)), -6.0, 0.6),
            "high shelf must cut treble"
        );
        assert!(db_near(lin2db(measured(hi, 100.0)), 0.0, 0.4));
    }

    #[test]
    fn analytic_gain_matches_measured() {
        let c = Coef::peaking(1500.0, 9.0, 1.4, SR);
        let bq = Biquad::new(c);
        let analytic = lin2db(bq.gain_at(1500.0, SR));
        let measured_db = lin2db(measured(c, 1500.0));
        assert!(
            db_near(measured_db, analytic, 0.15),
            "{measured_db} vs {analytic}"
        );
    }

    #[test]
    fn one_pole_settles_and_highpass_removes_dc() {
        let mut op = OnePole::new();
        op.set_tau(0.001, SR); // 1 ms
        let mut y = 0.0;
        for _ in 0..100 {
            y = op.lowpass(1.0); // ~2 ms of 1 ms time constant
        }
        assert!(
            y > 0.85 && y <= 1.0,
            "one-pole should be mostly settled, got {y}"
        );

        let mut dc = OnePole::new();
        dc.set_tau(0.005, SR);
        let mut last = 1.0;
        for _ in 0..48000 {
            last = dc.highpass(1.0);
        }
        assert!(
            last.abs() < 1e-3,
            "highpass must bleed DC to zero, got {last}"
        );
    }

    #[test]
    fn dc_blocker_kills_a_dc_offset() {
        let mut dcb = DcBlocker::new();
        dcb.set_hz(20.0, SR);
        let mut y = 0.0;
        for _ in 0..48000 {
            y = dcb.process(0.5);
        }
        assert!(y.abs() < 5e-3, "DC in should give ~0 out, got {y}");
        // and still passes audio
        let mut bq_pass = 0.0f32;
        let x = sine(4096, 1000.0, SR, 0.5);
        for s in &x {
            bq_pass = bq_pass.max(dcb.process(*s).abs());
        }
        let in_peak = x.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            bq_pass > in_peak * 0.8,
            "1 kHz should survive a 20 Hz blocker"
        );
    }

    #[test]
    fn recursive_helpers_flush_subnormal_state() {
        let mut one_pole = OnePole::new();
        one_pole.set_hz(10.0, SR);
        one_pole.lowpass(1.0);
        for _ in 0..100_000 {
            one_pole.lowpass(0.0);
        }
        assert_eq!(one_pole.state(), 0.0);

        let mut blocker = DcBlocker::new();
        blocker.set_hz(10.0, SR);
        blocker.process(1.0);
        for _ in 0..100_000 {
            blocker.process(0.0);
        }
        assert_eq!(blocker.x1, 0.0);
        assert_eq!(blocker.y1, 0.0);

        blocker.process(f32::from_bits(1));
        assert_eq!(blocker.x1, 0.0);

        let mut biquad = Biquad::new(Coef::lowpass(1_000.0, 0.707, SR));
        biquad.process(1.0);
        for _ in 0..2_000 {
            biquad.process(0.0);
        }
        assert_eq!(biquad.x1, 0.0);
        assert_eq!(biquad.x2, 0.0);
        assert_eq!(biquad.y1, 0.0);
    }

    #[test]
    fn non_finite_input_cannot_permanently_break_a_biquad() {
        let mut bq = Biquad::new(Coef::lowpass(1000.0, 0.707, SR));
        assert_eq!(bq.process(f32::NAN), 0.0);
        let mut last = 0.0;
        for s in sine(2048, 1000.0, SR, 0.5) {
            last = bq.process(s);
        }
        assert!(last.is_finite(), "filter must recover after a NaN sample");
    }
}
