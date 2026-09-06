//! DSP building blocks.
//!
//! Everything here is allocation-free after construction and free of locks,
//! syscalls and panics on normal input, because it runs inside the audio callback.

pub mod amp;
pub mod analysis;
pub mod biquad;
pub mod cab;
pub mod delayline;
pub mod fx;
pub mod lfo;
pub mod limiter;
pub mod meter;
pub mod resampler;
pub mod ring;
mod svf;

/// One stereo sample.
pub type Frame = [f32; 2];

/// Largest block the engine processes at a time. Fixed so the scratch buffer is
/// stack/embedded and a device of any buffer size can be served by chunking.
pub const MAX_CHUNK: usize = 512;

/// Highest device rate supported by preallocated DSP state.
pub(crate) const MAX_SAMPLE_RATE_HZ: f32 = 192_000.0;

#[inline]
pub fn db2lin(db: f32) -> f32 {
    10f32.powf(db * 0.05)
}

#[inline]
pub fn lin2db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

/// Sanitising clamp: a non-finite input becomes `fallback` rather than poisoning
/// filter state permanently.
#[inline]
pub fn sanitize(x: f32, fallback: f32, min: f32, max: f32) -> f32 {
    if x.is_finite() {
        x.clamp(min, max)
    } else {
        fallback
    }
}

/// Flush finite subnormal state before it can make a real-time recurrence expensive.
#[inline]
pub(crate) fn flush_denormal(x: f32) -> f32 {
    if x != 0.0 && x.abs() < f32::MIN_POSITIVE {
        0.0
    } else {
        x
    }
}

/// One-pole coefficient for a time constant in seconds (`tau` = 63 % point).
#[inline]
pub fn tau_coef(tau_s: f32, sr: f32) -> f32 {
    let tau = if tau_s.is_finite() {
        tau_s.max(1e-6)
    } else {
        1e-3
    };
    (-1.0 / (tau * sr.max(1.0))).exp()
}

/// One-pole coefficient from a -3 dB corner in Hz.
#[inline]
pub fn hz_coef(hz: f32, sr: f32) -> f32 {
    let h = hz.clamp(0.01, sr * 0.45);
    (-2.0 * std::f32::consts::PI * h / sr.max(1.0)).exp()
}

/// Smallest power of two >= `n`, at least 4 (delay lines and rings index by mask).
pub fn next_pow2(n: usize) -> usize {
    n.max(4).next_power_of_two()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_round_trip() {
        for db in [-60.0, -12.0, -0.0, 3.0, 20.0] {
            assert!((lin2db(db2lin(db)) - db).abs() < 1e-3, "{db}");
        }
        assert!((db2lin(0.0) - 1.0).abs() < 1e-6);
        assert!((db2lin(-6.0206) - 0.5).abs() < 1e-3);
    }

    #[test]
    fn sanitize_kills_non_finite() {
        assert_eq!(sanitize(f32::NAN, 0.5, -1.0, 1.0), 0.5);
        assert_eq!(sanitize(f32::INFINITY, 0.5, -1.0, 1.0), 0.5);
        assert_eq!(sanitize(9.0, 0.5, -1.0, 1.0), 1.0);
        assert_eq!(sanitize(-9.0, 0.5, -1.0, 1.0), -1.0);
    }

    #[test]
    fn pow2() {
        assert_eq!(next_pow2(0), 4);
        assert_eq!(next_pow2(3), 4);
        assert_eq!(next_pow2(1000), 1024);
        assert_eq!(next_pow2(1024), 1024);
    }
}
