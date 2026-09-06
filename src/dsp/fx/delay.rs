//! Ping-pong stereo delay.
//!
//! The feedback path is cross-coupled (L feeds R feeds L), so one round trip through the
//! loop gains `feedback²` rather than `feedback`. That is why the cap here is 0.92 and not
//! the 0.65 a single-line delay would need for the same stability: at 0.92 the round-trip
//! gain is 0.85, which rings out musically instead of self-oscillating forever.

use crate::dsp::biquad::OnePole;
use crate::dsp::delayline::DelayLine;
use crate::dsp::{sanitize, Frame, MAX_SAMPLE_RATE_HZ};
use crate::engine::Proc;
use crate::params::ParamVals;

pub const TIME: usize = 0;
pub const FEEDBACK: usize = 1;
pub const MIX: usize = 2;
pub const TONE: usize = 3;

/// Longest echo, and the tap for the right-hand line, which is 1.5× the left.
const MAX_MS: f32 = 1600.0;
/// Stability ceiling — see the module comment for why this is not 0.65.
const MAX_FEEDBACK: f32 = 0.92;

pub struct StereoDelay {
    sr: f32,
    l: DelayLine,
    r: DelayLine,
    tone_l: OnePole,
    tone_r: OnePole,
}

impl StereoDelay {
    pub fn new() -> StereoDelay {
        let cap = (MAX_MS * 0.001 * MAX_SAMPLE_RATE_HZ * 1.6) as usize;
        StereoDelay {
            sr: 48000.0,
            l: DelayLine::new(cap),
            r: DelayLine::new(cap),
            tone_l: OnePole::new(),
            tone_r: OnePole::new(),
        }
    }
}

impl Default for StereoDelay {
    fn default() -> Self {
        StereoDelay::new()
    }
}

impl Proc for StereoDelay {
    fn set_rates(&mut self, sr: f32) {
        self.sr = sanitize(sr, 48_000.0, 8_000.0, MAX_SAMPLE_RATE_HZ);
    }

    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals) {
        let time_ms = sanitize(p.v[TIME], 380.0, 1.0, MAX_MS);
        let fb = sanitize(p.v[FEEDBACK], 0.0, 0.0, 1.0) * MAX_FEEDBACK;
        let mix = sanitize(p.v[MIX], 0.0, 0.0, 1.0);
        let tone_hz = sanitize(p.v[TONE], 3000.0, 100.0, self.sr * 0.45);
        self.tone_l.set_hz(tone_hz, self.sr);
        self.tone_r.set_hz(tone_hz, self.sr);

        let dt_l = (time_ms * 0.001 * self.sr).min(self.l.capacity() as f32 - 2.0);
        let dt_r = (time_ms * 1.5 * 0.001 * self.sr).min(self.r.capacity() as f32 - 2.0);

        for f in buf[..n].iter_mut() {
            let (dry_l, dry_r) = (f[0], f[1]);
            let tap_l = self.l.read(dt_l);
            let tap_r = self.r.read(dt_r);
            // Cross-coupled: each line is excited by the other's filtered echo, plus its
            // own dry input, so a mono input alternates sides.
            self.l.write(dry_l + self.tone_l.lowpass(tap_r) * fb);
            self.r.write(dry_r + self.tone_r.lowpass(tap_l) * fb);
            f[0] = dry_l * (1.0 - mix) + tap_l * mix;
            f[1] = dry_r * (1.0 - mix) + tap_r * mix;
        }
    }

    fn reset(&mut self) {
        self.l.reset();
        self.r.reset();
        self.tone_l.reset();
        self.tone_r.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::peak;
    use crate::engine::Proc;
    use crate::params::EffectKind;

    const SR: f32 = 48000.0;

    fn params(time_ms: f32, fb: f32, mix: f32, tone: f32) -> ParamVals {
        // process() receives real units, not 0..=1 knob positions.
        let mut p = EffectKind::Delay.default_values();
        for (i, v) in [time_ms, fb, mix, tone].iter().enumerate() {
            p.v[i] = *v;
        }
        p
    }

    fn run(d: &mut StereoDelay, x: &[f32], p: &ParamVals) -> (Vec<f32>, Vec<f32>) {
        let mut buf: Vec<Frame> = x.iter().map(|s| [*s, *s]).collect();
        let n = buf.len();
        d.process(&mut buf, n, p);
        (
            buf.iter().map(|f| f[0]).collect(),
            buf.iter().map(|f| f[1]).collect(),
        )
    }

    #[test]
    fn echo_lands_at_exactly_the_requested_time() {
        let time_ms = 250.0;
        let want = (time_ms * 0.001 * SR) as usize; // 12000 frames
        let mut x = vec![0.0f32; 48000];
        x[100] = 0.8; // one impulse

        let mut d = StereoDelay::new();
        d.set_rates(SR);
        // mix 1.0 so the output is the wet path only, dry removed
        let (l, _r) = run(&mut d, &x, &params(time_ms, 0.0, 1.0, 12000.0));

        // With mix 1.0 the dry is gone; the first echo appears one delay after the input.
        let region = &l[100 + want - 60..100 + want + 60];
        let at = region
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap();
        assert!(
            at.1.abs() > 0.5,
            "no echo found near {want} frames, peak {} in region",
            at.1.abs()
        );
        assert!(
            (at.0 as i64 - 60).abs() <= 2,
            "echo at wrong offset: {} vs {want}",
            100 + want - 60 + at.0
        );
        // and nothing before it
        assert!(
            peak(&l[200..100 + want - 60]) < 0.05,
            "signal appeared before the echo time"
        );
    }

    #[test]
    fn max_time_echo_lands_at_192khz() {
        let want = (MAX_MS * 0.001 * MAX_SAMPLE_RATE_HZ) as usize;
        let mut buf = vec![[0.0f32; 2]; want + 4];
        buf[0] = [0.8, 0.8];

        let mut d = StereoDelay::new();
        d.set_rates(MAX_SAMPLE_RATE_HZ);
        let n = buf.len();
        d.process(&mut buf, n, &params(MAX_MS, 0.0, 1.0, 12000.0));

        assert!(
            buf[want - 2..want + 3].iter().any(|f| f[0].abs() > 0.5),
            "no 1.6 second echo at 192 kHz"
        );
    }

    #[test]
    fn feedback_repeats_and_the_right_side_is_later() {
        let time_ms = 100.0;
        let dt = (time_ms * 0.001 * SR) as usize;
        let mut x = vec![0.0f32; 48000];
        x[0] = 0.8;
        let mut d = StereoDelay::new();
        d.set_rates(SR);
        let (l, r) = run(&mut d, &x, &params(time_ms, 0.6, 1.0, 12000.0));

        // Left: echo at dt, cross-coupled echo at ~2.5*dt (through the 1.5x right line).
        assert!(
            l[dt - 3..dt + 4].iter().any(|s| s.abs() > 0.3),
            "missing first echo"
        );
        let second = (dt as f32 * 2.5) as usize;
        assert!(
            l[second - 4..second + 5].iter().any(|s| s.abs() > 0.05),
            "missing cross-coupled repeat at {second}"
        );
        // Right line is 1.5x longer, so its first echo is later than the left's.
        let dt_r = (dt as f32 * 1.5) as usize;
        assert!(peak(&r[dt..dt_r - 4]) < 0.05, "right echoed too early");
        assert!(
            r[dt_r - 3..dt_r + 4].iter().any(|s| s.abs() > 0.3),
            "missing right echo"
        );
    }

    #[test]
    fn feedback_at_max_rings_out_but_never_runs_away() {
        let mut x = vec![0.0f32; 96000];
        x[0] = 0.9;
        let mut d = StereoDelay::new();
        d.set_rates(SR);
        let (l, r) = run(&mut d, &x, &params(400.0, 1.0, 1.0, 12000.0));
        let all = [l.as_slice(), r.as_slice()].concat();
        assert!(all.iter().all(|s| s.is_finite()));
        assert!(peak(&all) < 4.0, "delay ran away: peak {}", peak(&all));
        // and it must actually decay: the tail is quieter than the head
        assert!(
            peak(&all[60000..]) < peak(&all[..20000]),
            "delay is not decaying"
        );
    }

    #[test]
    fn tone_knob_darkens_the_repeats() {
        let mut x = vec![0.0f32; 48000];
        x[0] = 0.8;
        let bright = {
            let mut d = StereoDelay::new();
            d.set_rates(SR);
            let (l, _) = run(&mut d, &x, &params(150.0, 0.5, 1.0, 12000.0));
            l
        };
        let dark = {
            let mut d = StereoDelay::new();
            d.set_rates(SR);
            let (l, _) = run(&mut d, &x, &params(150.0, 0.5, 1.0, 400.0));
            l
        };
        // Compare a later repeat: the dark one must have lost high-frequency energy.
        let hf = |y: &[f32]| -> f32 {
            y.windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .fold(0.0f32, f32::max)
        };
        let seg = 15000..24000;
        assert!(
            hf(&dark[seg.clone()]) < hf(&bright[seg.clone()]),
            "tone knob did nothing"
        );
    }

    #[test]
    fn extremes_and_junk_stay_bounded() {
        let mut d = StereoDelay::new();
        d.set_rates(SR);
        let mut p = ParamVals::ZEROED;
        p.v[TIME] = f32::NAN;
        p.v[FEEDBACK] = f32::INFINITY; // over-range feedback must clamp below 1
        p.v[MIX] = 4.0;
        p.v[TONE] = -50.0;
        let mut buf = [[0.5f32, -0.5]; 256];
        for _ in 0..40 {
            d.process(&mut buf, 256, &p);
            assert!(buf.iter().all(|f| f[0].is_finite() && f[1].is_finite()));
            assert!(peak(&buf.iter().map(|f| f[0]).collect::<Vec<f32>>()) < 8.0);
        }
        // extreme time at the lowest supported rate
        d.set_rates(8000.0);
        d.process(&mut buf, 256, &params(1500.0, 1.0, 1.0, 300.0));
        assert!(buf.iter().all(|f| f[0].is_finite()));
    }

    #[test]
    fn sample_rate_is_clamped_to_the_allocated_ceiling() {
        let mut delay = StereoDelay::new();
        delay.set_rates(MAX_SAMPLE_RATE_HZ * 2.0);
        assert_eq!(delay.sr, MAX_SAMPLE_RATE_HZ);
    }

    #[test]
    fn works_at_every_supported_rate() {
        for sr in [44100.0f32, 48000.0, 88200.0, 96000.0] {
            let mut d = StereoDelay::new();
            d.set_rates(sr);
            let mut x = vec![0.0f32; (0.5 * sr) as usize];
            x[0] = 0.7;
            let (l, _) = run(&mut d, &x, &params(200.0, 0.4, 1.0, 4000.0));
            let want = (0.2 * sr) as usize;
            assert!(
                l[want - 3..want + 4].iter().any(|s| s.abs() > 0.3),
                "{sr} Hz mis-timed"
            );
            assert!(l.iter().all(|s| s.is_finite()));
        }
    }
}
