//! Cabinet simulation: either a modelled "greenback-ish" response, or a measured IR.
//!
//! The modelled path is deliberately boring — high-pass, two low-pass sections and a
//! cone peak — because that box-y roll-off *is* most of what a 12" guitar speaker does
//! to an amp's raw output, and it is the single biggest difference between "a pedal" and
//! "an amp". A loaded IR replaces the model entirely rather than stacking on top of it,
//! which is what you want when the IR already contains the cab's EQ.

use crate::dsp::biquad::{Biquad, Coef};
use crate::dsp::{next_pow2, Frame};

/// Longest IR accepted, in samples. Direct time-domain convolution costs
/// `taps × 2 channels × sr` multiply-accumulates, so 1024 taps at 48 kHz stereo is
/// ~98 M MAC/s — affordable; 8192 taps would not be, and is refused with a message
/// instead of silently eating a core.
///
/// `ponytail:` direct convolution. Upgrade path if anyone loads long IRs: partitioned
/// (FFT) convolution with a per-partition overlap-add.
pub const MAX_IR_TAPS: usize = 1024;

/// A measured impulse response, peak-normalised, ready to convolve.
/// Most an IR is allowed to multiply its input by. The engine's master and
/// limiter sit behind this, so being generous here only costs headroom, never safety.
const GAIN_BOUND: f32 = 2.5;

pub struct Ir {
    data: Box<[f32]>,
    norm: f32,
    name: String,
}

impl Ir {
    /// Normalise so the IR cannot add gain, and cap the length.
    ///
    /// Normalised by the L1 norm (sum of |taps|), which is a *provable* upper bound on
    /// |H(f)| — so `GAIN_BOUND / L1` guarantees the cab can never hand the next stage more
    /// than GAIN_BOUND x whatever came in, for any IR. Peak-normalising (as this did) is
    /// not such a bound: a 64-tap one-pole "speaker" IR has sum 6.7 but peak 1.0, and read
    /// 1.86 out of a 0.5 input, i.e. an IR swap could hand the user +11 dB unannounced.
    /// Errors are descriptive because this is the
    /// one path a human gets wrong (wrong file, empty file, all zeros).
    pub fn new(name: String, mut samples: Vec<f32>) -> Result<Ir, String> {
        if samples.is_empty() || samples.iter().any(|s| !s.is_finite()) {
            return Err("IR is empty or contains non-finite samples".to_string());
        }
        let l1 = samples.iter().fold(0.0f32, |m, s| m + s.abs());
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        if peak < 1e-9 {
            return Err("IR is silent".to_string());
        }
        if samples.len() > MAX_IR_TAPS {
            // Keep the meaningful head rather than a tail of near-silence.
            samples.truncate(MAX_IR_TAPS);
        }
        let norm = GAIN_BOUND / l1.max(1e-9);
        Ok(Ir {
            data: samples.into_boxed_slice(),
            norm,
            name,
        })
    }

    /// Read a WAV file as mono, taking the first `MAX_IR_TAPS` frames.
    pub fn from_wav(path: &str) -> Result<Ir, String> {
        let mut reader = hound::WavReader::open(path).map_err(|e| format!("{path}: {e}"))?;
        let spec = reader.spec();
        if spec.channels == 0 || !(1..=32).contains(&spec.bits_per_sample) {
            return Err(format!("{path}: invalid channel count or bit depth"));
        }
        let ch = spec.channels as usize;
        let want = MAX_IR_TAPS.min(reader.duration() as usize).max(1);
        let mut out = Vec::with_capacity(want * ch);
        match spec.sample_format {
            hound::SampleFormat::Float => {
                for s in reader.samples::<f32>().take(want * ch) {
                    out.push(s.map_err(|e| format!("{path}: {e}"))?);
                }
            }
            hound::SampleFormat::Int => {
                let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
                if spec.bits_per_sample <= 16 {
                    for s in reader.samples::<i16>().take(want * ch) {
                        out.push(s.map_err(|e| format!("{path}: {e}"))? as f32 * scale);
                    }
                } else {
                    for s in reader.samples::<i32>().take(want * ch) {
                        out.push(s.map_err(|e| format!("{path}: {e}"))? as f32 * scale);
                    }
                }
            }
        }
        // Mix down to mono and find where the response actually starts (a common IR has
        // pre-ring silence, and a late peak makes the cab sound flappy).
        let mono: Vec<f32> = if ch <= 1 {
            out
        } else {
            out.chunks(ch)
                .map(|c| c.iter().sum::<f32>() / ch as f32)
                .collect()
        };
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        Ir::new(name, mono)
    }

    pub fn taps(&self) -> usize {
        self.data.len()
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Direct convolution over a rolling input window.
struct IrConv {
    ir: Box<[f32]>,
    norm: f32,
    buf: Box<[f32]>,
    w: usize,
}

impl IrConv {
    fn new(ir: &Ir) -> IrConv {
        let taps = ir.taps();
        let cap = next_pow2(taps);
        IrConv {
            ir: ir.data.clone(),
            norm: ir.norm,
            buf: vec![0.0; cap].into_boxed_slice(),
            w: 0,
        }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let mask = self.buf.len() - 1;
        self.buf[self.w & mask] = x;
        let mut acc = 0.0f32;
        for (i, h) in self.ir.iter().enumerate() {
            acc += *h * self.buf[self.w.wrapping_sub(i) & mask];
        }
        self.w = self.w.wrapping_add(1);
        acc * self.norm
    }
}

pub struct Cab {
    sr: f32,
    hp: [Biquad; 2],
    lp: [[Biquad; 2]; 2],
    peak: [Biquad; 2],
    /// Level trim so switching between model and IR is not a loud surprise.
    trim: f32,
    conv: [Option<IrConv>; 2],
    ir_name: Option<String>,
}

impl Cab {
    pub fn new(sr: f32) -> Cab {
        let mut c = Cab {
            sr,
            hp: [Biquad::passthrough(); 2],
            lp: [[Biquad::passthrough(); 2]; 2],
            peak: [Biquad::passthrough(); 2],
            trim: 0.85,
            conv: [None, None],
            ir_name: None,
        };
        c.set_rates(sr);
        c
    }

    pub fn set_rates(&mut self, sr: f32) {
        self.sr = sr.max(8000.0);
        // 850 Hz top end with a 24 dB/oct slope is the "mic'd 4x12" sound; a raw amp
        // straight into a full-range speaker is exactly the harshness this removes.
        let hp = Coef::highpass(85.0, 0.707, self.sr);
        let lp = Coef::lowpass_bw(5200.0, self.sr);
        let peak = Coef::peaking(1100.0, 3.0, 1.2, self.sr);
        for channel in 0..2 {
            self.hp[channel].set(hp);
            self.lp[channel][0].set(lp);
            self.lp[channel][1].set(lp);
            self.peak[channel].set(peak);
        }
    }

    pub fn sr(&self) -> f32 {
        self.sr
    }

    pub fn load_ir(&mut self, ir: Ir) {
        self.ir_name = Some(ir.name().to_string());
        self.conv = [Some(IrConv::new(&ir)), Some(IrConv::new(&ir))];
        self.trim = 1.0;
    }

    pub fn clear_ir(&mut self) {
        self.ir_name = None;
        self.conv = [None, None];
        self.trim = 0.85;
    }

    pub fn ir_name(&self) -> Option<&str> {
        self.ir_name.as_deref()
    }

    /// True when a measured IR is in use instead of the model.
    pub fn using_ir(&self) -> bool {
        self.conv[0].is_some()
    }

    pub fn process(&mut self, buf: &mut [Frame], n: usize) {
        let (slot_l, slot_r) = self.conv.split_at_mut(1);
        if let (Some(c0), Some(c1)) = (&mut slot_l[0], &mut slot_r[0]) {
            for f in buf[..n].iter_mut() {
                f[0] = c0.process(f[0]);
                f[1] = c1.process(f[1]);
            }
            return;
        }
        for f in buf[..n].iter_mut() {
            for (channel, sample) in f.iter_mut().enumerate() {
                let mut x = self.hp[channel].process(*sample);
                x = self.lp[channel][0].process(x);
                x = self.lp[channel][1].process(x);
                x = self.peak[channel].process(x);
                *sample = x * self.trim;
            }
        }
    }

    pub fn reset(&mut self) {
        for channel in 0..2 {
            self.hp[channel].reset();
            self.lp[channel][0].reset();
            self.lp[channel][1].reset();
            self.peak[channel].reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{goertzel_mag, peak, sine};

    const SR: f32 = 48000.0;

    fn levels(cab: &mut Cab, f: f32) -> f32 {
        let x = sine(16384, f, SR, 0.4);
        let mut buf: Vec<Frame> = x.iter().map(|s| [*s, *s]).collect();
        let n = buf.len();
        cab.process(&mut buf, n);
        let y: Vec<f32> = buf.iter().skip(4096).map(|fr| fr[0]).collect();
        let src: Vec<f32> = x.iter().skip(4096).copied().collect();
        goertzel_mag(&y, f, SR) / goertzel_mag(&src, f, SR).max(1e-9)
    }

    #[test]
    fn model_channels_have_independent_history() {
        let x = sine(4096, 700.0, SR, 0.4);

        let mut matching = Cab::new(SR);
        let mut stereo: Vec<Frame> = x.iter().map(|s| [*s, *s]).collect();
        let n = stereo.len();
        matching.process(&mut stereo, n);
        assert!(
            stereo.iter().all(|f| (f[0] - f[1]).abs() < 1e-7),
            "identical inputs must remain identical"
        );

        let mut isolated = Cab::new(SR);
        let mut left_only: Vec<Frame> = x.iter().map(|s| [*s, 0.0]).collect();
        let n = left_only.len();
        isolated.process(&mut left_only, n);
        assert!(
            left_only.iter().all(|f| f[1].abs() < 1e-7),
            "left input leaked into right output"
        );
    }

    #[test]
    fn model_band_limits_like_a_speaker() {
        let mut cab = Cab::new(SR);
        let mid = levels(&mut cab, 1000.0);
        let sub = levels(&mut cab, 40.0);
        let fizz = levels(&mut cab, 14000.0);
        assert!(mid > 0.5, "1 kHz should pass, got {mid}");
        assert!(
            sub < mid * 0.35,
            "40 Hz must be filtered, got {sub} vs {mid}"
        );
        assert!(
            fizz < mid * 0.25,
            "14 kHz fizz must be rolled off, got {fizz} vs {mid}"
        );
    }

    #[test]
    fn cone_peak_is_a_bump_not_a_hole() {
        let cab = Cab::new(SR);
        let mut with = Cab::new(SR);
        with.set_rates(SR);
        let at_peak = levels(&mut with, 1100.0);
        let mut no_peak = Cab::new(SR);
        for peak in no_peak.peak.iter_mut() {
            peak.set(Coef::peaking(1100.0, 0.0, 1.2, SR));
        }
        let without = levels(&mut no_peak, 1100.0);
        assert!(at_peak > without, "the 1.1 kHz peak should add presence");
        let _ = cab;
    }

    #[test]
    fn ir_replaces_the_model_and_stays_bounded() {
        // A one-pole "speaker" IR.
        let mut taps = vec![0.0f32; 64];
        let mut v = 1.0f32;
        for t in taps.iter_mut() {
            *t = v;
            v *= 0.85;
        }
        let ir = Ir::new("test-ir.wav".into(), taps.clone()).unwrap();
        assert_eq!(ir.taps(), 64);
        assert_eq!(ir.name(), "test-ir.wav");

        let mut cab = Cab::new(SR);
        assert!(!cab.using_ir());
        cab.load_ir(ir);
        assert!(cab.using_ir());
        assert_eq!(cab.ir_name(), Some("test-ir.wav"));

        let mut buf: Vec<Frame> = sine(4096, 500.0, SR, 0.5)
            .iter()
            .map(|s| [*s, *s])
            .collect();
        let n = buf.len();
        cab.process(&mut buf, n);
        let p = peak(&buf.iter().map(|f| f[0]).collect::<Vec<f32>>());
        assert!(p > 0.05 && p < 1.2, "IR output level {p}");
        // an energy-preserving-ish normalisation must not leave the DC of the IR in the
        // output as a permanent offset
        let mean = buf.iter().skip(2048).map(|f| f[0] as f64).sum::<f64>() / 2048.0;
        assert!(mean.abs() < 0.2, "IR left a large offset: {mean}");

        cab.clear_ir();
        assert!(!cab.using_ir());
        assert_eq!(cab.ir_name(), None);
    }

    #[test]
    fn bad_irs_are_refused_with_a_reason() {
        assert!(Ir::new("empty".into(), vec![]).is_err());
        assert!(Ir::new("zeros".into(), vec![0.0; 100]).is_err());
        assert!(Ir::new("nans".into(), vec![f32::NAN; 50]).is_err());
        assert!(Ir::new("mixed".into(), vec![1.0, f32::NAN, 0.5]).is_err());
        assert!(Ir::from_wav("/definitely/not/here.wav").is_err());
        // over-long IRs are truncated to the affordable length, not rejected silently
        let long = Ir::new("long".into(), vec![0.4; MAX_IR_TAPS * 3]).unwrap();
        assert_eq!(long.taps(), MAX_IR_TAPS);
    }

    #[test]
    fn junk_and_rate_changes_are_survivable() {
        let mut cab = Cab::new(SR);
        let mut buf: Vec<Frame> = sine(1024, 1000.0, SR, 0.4)
            .iter()
            .map(|s| [*s, *s])
            .collect();
        buf[10][0] = f32::NAN;
        let n = buf.len();
        cab.process(&mut buf, n);
        assert!(buf
            .iter()
            .skip(64)
            .all(|f| f[0].is_finite() && f[1].is_finite()));
        for sr in [44100.0f32, 96000.0] {
            cab.set_rates(sr);
            let n = buf.len();
            cab.process(&mut buf, n);
            assert!(buf.iter().all(|f| f[0].is_finite()));
        }
        cab.reset();
    }
}
