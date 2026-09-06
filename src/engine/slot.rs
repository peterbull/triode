use crate::dsp::Frame;
use crate::params::{EffectKind, ParamVals};

/// A processing element. Implementations must not allocate, lock or block inside
/// `process` or `set_rates`; only construction may allocate.
pub trait Proc: Send {
    /// Recompute sample-rate-dependent state (coefficient caches, delay lengths).
    fn set_rates(&mut self, _sr: f32) {}
    /// Process `n` frames of `buf` in place. `p.v[i]` are real (denormalised) values.
    fn process(&mut self, buf: &mut [Frame], n: usize, p: &ParamVals);
    /// Clear internal state.
    fn reset(&mut self) {}
}

/// One rack position: a kind, its bypass state, its params, and its live state.
pub struct Slot {
    pub kind: EffectKind,
    pub enabled: bool,
    /// Target normalised values, as set by the UI.
    pub target: ParamVals,
    /// Smoothed normalised values, advanced toward `target` once per chunk.
    pub smooth: ParamVals,
    /// Real (denormalised) values handed to `Proc::process`, recomputed per chunk.
    pub values: ParamVals,
    pub proc: Box<dyn Proc>,
}

impl Slot {
    pub fn new(kind: EffectKind, enabled: bool) -> Slot {
        let norms = kind.default_norms();
        Slot {
            kind,
            enabled,
            target: norms,
            smooth: norms,
            values: norms,
            proc: make_proc(kind),
        }
    }

    /// Build a slot on a non-audio thread, ready to be moved into the mailbox.
    pub fn build(kind: EffectKind, enabled: bool, sr: f32) -> Slot {
        let mut s = Slot::new(kind, enabled);
        s.proc.set_rates(sr);
        s
    }

    /// Advance smoothed params toward the targets and denormalise them.
    ///
    /// Smoothing at chunk rate (rather than per sample) is the deliberate trade: a knob
    /// sweep steps at roughly 90 Hz with a ~15 ms time constant, which is inaudible, and
    /// it keeps filter coefficients constant inside a chunk so a biquad never sees a
    /// coefficient discontinuity mid-block.
    ///
    /// `ponytail:` if a fast-swept resonant filter ever clicks, upgrade this to
    /// per-sample coefficient interpolation inside the affected effect only.
    #[inline]
    pub(crate) fn advance(&mut self, k: f32) {
        let specs = self.kind.params();
        for (i, spec) in specs.iter().enumerate() {
            let t = self.target.get(spec, i);
            let cur = self.smooth.v[i].clamp(0.0, 1.0);
            self.smooth.v[i] = cur + (t - cur) * k;
            self.values.v[i] = spec.denorm(self.smooth.v[i]);
        }
    }
}

/// Build a processor off the audio callback when preparing a slot. Existing processors
/// are retuned in place through [`Proc::set_rates`].
pub fn make_proc(kind: EffectKind) -> Box<dyn Proc> {
    use crate::dsp::fx;
    match kind {
        EffectKind::Gate => Box::new(fx::gate::Gate::new()),
        EffectKind::Compressor => Box::new(fx::comp::Compressor::new()),
        EffectKind::Boost => Box::new(fx::boost::Boost::new()),
        EffectKind::Overdrive => Box::new(fx::drive::Overdrive::new()),
        EffectKind::Fuzz => Box::new(fx::drive::Fuzz::new()),
        EffectKind::ParametricEq => Box::new(fx::eq::ParametricEq::new()),
        EffectKind::EnvelopeFilter => Box::new(fx::envelope_filter::EnvelopeFilter::new()),
        EffectKind::StepFilter => Box::new(fx::step_filter::StepFilter::new()),
        EffectKind::Tremolo => Box::new(fx::trem::Tremolo::new()),
        EffectKind::Phaser => Box::new(fx::phaser::Phaser::new()),
        EffectKind::Flanger => Box::new(fx::flanger::Flanger::new()),
        EffectKind::Chorus => Box::new(fx::chorus::Chorus::new()),
        EffectKind::RingModulator => Box::new(fx::ring_mod::RingModulator::new()),
        EffectKind::BitCrusher => Box::new(fx::bitcrusher::BitCrusher::new()),
        EffectKind::Delay => Box::new(fx::delay::StereoDelay::new()),
        EffectKind::AnalogDelay => Box::new(fx::analog_delay::AnalogDelay::new()),
        EffectKind::Reverb => Box::new(fx::reverb::Reverb::new()),
    }
}
