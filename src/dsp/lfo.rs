//! Low-frequency oscillators for tremolo and chorus.

/// Phase-accumulating LFO. Output shapes are computed from one phase so several
/// effects can share the same clock and stay sample-accurate.
#[derive(Clone, Copy, Debug, Default)]
pub struct Lfo {
    phase: f32,
    inc: f32,
}

impl Lfo {
    pub fn new() -> Lfo {
        Lfo {
            phase: 0.0,
            inc: 0.0,
        }
    }

    /// `hz` is clamped to something sane for audio-rate stability (a 20 kHz "tremolo"
    /// would be an oscillator, not a modulation source).
    pub fn set_rate(&mut self, hz: f32, sr: f32) {
        let h = if hz.is_finite() {
            hz.clamp(0.01, 40.0)
        } else {
            1.0
        };
        self.inc = h / sr.max(1.0);
    }

    #[inline]
    fn tick_phase(&mut self) {
        self.phase += self.inc;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
    }

    /// Sine, -1..=1, advancing phase.
    #[inline]
    pub fn sine(&mut self) -> f32 {
        self.tick_phase();
        (2.0 * std::f32::consts::PI * self.phase).sin()
    }

    /// Bipolar triangle, -1..=1.
    #[inline]
    pub fn triangle(&mut self) -> f32 {
        self.tick_phase();
        if self.phase < 0.5 {
            -1.0 + 4.0 * self.phase
        } else {
            3.0 - 4.0 * self.phase
        }
    }

    /// Unipolar 0..=1 sine (centred up), handy for tremolo depth.
    #[inline]
    pub fn sine_unipolar(&mut self) -> f32 {
        (self.sine() + 1.0) * 0.5
    }

    pub fn phase(&self) -> f32 {
        self.phase
    }

    /// Set the phase (0..1). Used to offset a second LFO for stereo width.
    pub fn set_phase(&mut self, phase: f32) {
        self.phase = if phase.is_finite() {
            phase.rem_euclid(1.0)
        } else {
            0.0
        };
    }

    /// Number of samples for one full cycle at the current rate.
    pub fn period_samples(&self, sr: f32) -> f32 {
        if self.inc > 0.0 {
            sr / (self.inc * sr)
        } else {
            f32::INFINITY
        }
    }

    pub fn reset(&mut self) {
        self.phase = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{goertzel_exact, peak};

    const SR: f32 = 48000.0;

    #[test]
    fn rate_is_accurate() {
        for hz in [0.5f32, 5.0, 21.9] {
            let mut lfo = Lfo::new();
            lfo.set_rate(hz, SR);
            // Four whole periods, measured with the Goertzel that trims to whole cycles.
            // Probing 0.5 Hz inside a one-second window means analysing half a cycle,
            // which reads as low-level mush at every frequency rather than at the rate
            // under test.
            let n = ((4.0 * SR / hz).round() as usize).max(1024);
            let y: Vec<f32> = (0..n).map(|_| lfo.sine()).collect();
            // The LFO's own fundamental should measure at the requested rate.
            let mag = goertzel_exact(&y, hz, SR);
            assert!(mag > 0.3, "{hz} Hz fundamental measured {mag}");
            // and nothing a octave up
            assert!(
                goertzel_exact(&y, hz * 2.0, SR) < 0.02,
                "{hz} Hz LFO has a second harmonic"
            );
        }
    }

    #[test]
    fn outputs_stay_in_range_and_phase_wraps() {
        let mut lfo = Lfo::new();
        lfo.set_rate(7.0, SR);
        let mut mx = 0.0f32;
        let mut tr = 0.0f32;
        for _ in 0..200000 {
            mx = mx.max(lfo.sine().abs());
            tr = tr.max(lfo.triangle().abs());
            assert!(lfo.phase() < 1.0 && lfo.phase() >= 0.0);
        }
        assert!(mx > 0.999 && mx <= 1.0001, "sine peak {mx}");
        assert!(tr > 0.99 && tr <= 1.0001, "triangle peak {tr}");
    }

    #[test]
    fn unipolar_stays_non_negative() {
        let mut lfo = Lfo::new();
        lfo.set_rate(3.0, SR);
        let y: Vec<f32> = (0..9600).map(|_| lfo.sine_unipolar()).collect();
        assert!(y.iter().all(|s| *s >= -1e-6 && *s <= 1.0 + 1e-6));
        assert!(peak(&y) > 0.99);
    }

    #[test]
    fn junk_rate_does_not_stop_the_clock() {
        let mut lfo = Lfo::new();
        lfo.set_rate(f32::NAN, SR);
        let y: Vec<f32> = (0..1000).map(|_| lfo.sine()).collect();
        assert!(y.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
    }
}
