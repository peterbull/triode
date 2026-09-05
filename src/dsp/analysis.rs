//! Signal-measurement helpers, used by `--selftest` and the unit tests.
//!
//! These are the assertions that let the DSP be verified without ears or hardware:
//! a Goertzel detector is enough to check "the tone knob moved the shelf", "the
//! delay echoed at exactly N samples" and "oversampling reduced aliasing".

/// A sine, phase starting at 0.
pub fn sine(n: usize, freq: f32, sr: f32, amp: f32) -> Vec<f32> {
    (0..n)
        .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / sr).sin())
        .collect()
}

/// A one-pole-ish "guitar-ish" pluck: decaying saw with harmonics, for transient tests.
pub fn pluck(n: usize, freq: f32, sr: f32, decay_ms: f32) -> Vec<f32> {
    let k = (-1000.0 / (decay_ms.max(1.0) * sr)).exp();
    let mut env = 1.0f32;
    (0..n)
        .map(|i| {
            env *= k;
            let ph = 2.0 * std::f32::consts::PI * freq * i as f32 / sr;
            let mut s = ph.sin() + 0.5 * (2.0 * ph).sin() + 0.25 * (3.0 * ph).sin();
            s *= env;
            s * 0.5
        })
        .collect()
}

/// White-ish noise from a deterministic LCG (reproducible across runs).
pub fn noise(n: usize, seed: u32) -> Vec<f32> {
    let mut s = seed ^ 0x9e37_79b9;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s as f32 / u32::MAX as f32) * 2.0 - 1.0
        })
        .collect()
}

/// Magnitude at `freq` via the Goertzel algorithm (one bin of DFT, no FFT needed).
/// The window is trimmed to a whole number of cycles first. A window that ends
/// mid-cycle leaks into every other bin, which would make a clean sine look distorted and
/// a filtered signal look like it had vanished.
pub fn goertzel_mag(x: &[f32], freq: f32, sr: f32) -> f32 {
    if x.is_empty() || !freq.is_finite() || freq <= 0.0 || !sr.is_finite() || sr <= 0.0 {
        return 0.0;
    }
    let cycles = ((x.len() as f64 * freq as f64 / sr as f64).floor() as usize).max(1);
    let n_samp = ((cycles as f64 * sr as f64 / freq as f64).round() as usize).clamp(1, x.len());
    let x = &x[..n_samp];
    let n = x.len() as f32;
    let w = 2.0 * std::f32::consts::PI * freq / sr;
    let d = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &v in x {
        let s0 = v + d * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let mag2 = s1 * s1 + s2 * s2 - d * s1 * s2;
    2.0 * mag2.max(0.0).sqrt() / n
}

/// Total harmonic distortion as the ratio of harmonic (2..=5) power to fundamental.
pub fn thd(x: &[f32], fund: f32, sr: f32) -> f32 {
    // Trim once to a whole number of fundamental cycles. Every harmonic then also lands on
    // a whole cycle, so none of them see leakage from the fundamental.
    let cycles = ((x.len() as f64 * fund as f64 / sr as f64).floor() as usize).max(1);
    let n = ((cycles as f64 * sr as f64 / fund as f64).round() as usize).clamp(1, x.len());
    let x = &x[..n];
    let a1 = goertzel_exact(x, fund, sr);
    if a1 < 1e-9 {
        return 0.0;
    }
    let mut sum = 0.0f32;
    for h in 2..=5 {
        let f = fund * h as f32;
        if f < sr * 0.45 {
            let a = goertzel_exact(x, f, sr);
            sum += a * a;
        }
    }
    sum.sqrt() / a1
}

/// Goertzel over a window the caller has already made cycle-aligned.
pub fn goertzel_exact(x: &[f32], freq: f32, sr: f32) -> f32 {
    if x.is_empty() || !freq.is_finite() || freq <= 0.0 || !sr.is_finite() || sr <= 0.0 {
        return 0.0;
    }
    // Accumulated in f64 on purpose. The resonator's `d = 2*cos(w)` differs from 2.0 by
    // only w^2/2, and at low frequencies that gap is below f32 resolution: at 0.5 Hz on a
    // 48 kHz rate it is 4e-9, while f32's step at 2.0 is 2.4e-7, so `d` rounds to exactly
    // 2.0 and the resonator degenerates into a double integrator -- a 0.5 Hz sine of
    // amplitude 1.0 then measures 0.0. Same for anything under roughly 1 Hz.
    let n = x.len() as f64;
    let w = 2.0 * std::f64::consts::PI * freq as f64 / sr as f64;
    let d = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &v in x {
        let s0 = v as f64 + d * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let mag2 = s1 * s1 + s2 * s2 - d * s1 * s2;
    (2.0 * mag2.max(0.0).sqrt() / n) as f32
}

pub fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

pub fn mean(x: &[f32]) -> f32 {
    if x.is_empty() {
        0.0
    } else {
        x.iter().sum::<f32>() / x.len() as f32
    }
}

/// RMS of the tail half — used to check a reverb tail decays rather than rings.
pub fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
}

/// Any non-finite sample? (One NaN anywhere in the output is a hard failure.)
pub fn any_non_finite(x: &[f32]) -> bool {
    x.iter().any(|s| !s.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    #[test]
    fn goertzel_finds_amplitude_and_rejects_other_bins() {
        let x = sine(8192, 1000.0, SR, 0.4);
        assert!((goertzel_mag(&x, 1000.0, SR) - 0.4).abs() < 2e-3);
        assert!(goertzel_mag(&x, 3000.0, SR) < 1e-3);
    }

    #[test]
    fn thd_rises_with_distortion() {
        let clean = sine(16384, 200.0, SR, 0.2);
        let clipped: Vec<f32> = clean.iter().map(|s| s.max(-0.02).min(0.02)).collect();
        assert!(thd(&clean, 200.0, SR) < 1e-3);
        // thd() sums harmonics 2..=5, and a *perfect* square only reaches 0.43 there
        // (1/3, 1/5, 1/7, 1/9). Demanding more was demanding a wrong measurement.
        assert!(
            thd(&clipped, 200.0, SR) > 0.35,
            "hard clip should be very distorted"
        );
    }

    #[test]
    fn pluck_decays_and_noise_is_bounded() {
        let p = pluck(8192, 110.0, SR, 50.0);
        assert!(peak(&p[..512]) > peak(&p[6000..]), "pluck must decay");
        let n = noise(4096, 7);
        assert_eq!(n, noise(4096, 7), "noise has to be reproducible");
        assert!(peak(&n) <= 1.0);
        assert!(!any_non_finite(&n));
    }
}
