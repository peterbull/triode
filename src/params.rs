//! Parameter model: a static spec table per effect + fixed-size normalised blocks.
//!
//! The UI renders knobs purely from [`ParamSpec`] tables, so adding an effect adds
//! no UI code. Values travel between threads normalised to `0.0..=1.0`, which keeps
//! every cross-thread payload `Copy` and allocation-free, and makes clamping a
//! single obvious operation at the one place values cross into the audio thread.

use serde::{Deserialize, Serialize};

/// Params per effect (fixed so [`ParamVals`] is `Copy` and never allocates).
pub const MAX_PARAMS: usize = 8;
/// Rack slots. Reserved up front by the engine so `Vec` never reallocates on the
/// audio thread.
pub const MAX_SLOTS: usize = 12;

/// How a normalised `0..=1` knob position maps to a real value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Curve {
    Linear,
    /// Geometric — frequencies, times, anything perceived logarithmically.
    Log,
    /// Quadratic — levels and depths, so the useful range is not squashed at the
    /// bottom of the travel.
    Exp,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamSpec {
    pub name: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub curve: Curve,
    pub unit: &'static str,
    pub decimals: u8,
}

impl ParamSpec {
    pub const fn lin(
        name: &'static str,
        min: f32,
        max: f32,
        default: f32,
        unit: &'static str,
        decimals: u8,
    ) -> Self {
        Self {
            name,
            min,
            max,
            default,
            curve: Curve::Linear,
            unit,
            decimals,
        }
    }
    pub const fn log(
        name: &'static str,
        min: f32,
        max: f32,
        default: f32,
        unit: &'static str,
        decimals: u8,
    ) -> Self {
        Self {
            name,
            min,
            max,
            default,
            curve: Curve::Log,
            unit,
            decimals,
        }
    }
    pub const fn exp(
        name: &'static str,
        min: f32,
        max: f32,
        default: f32,
        unit: &'static str,
        decimals: u8,
    ) -> Self {
        Self {
            name,
            min,
            max,
            default,
            curve: Curve::Exp,
            unit,
            decimals,
        }
    }

    /// Normalised position -> real value. Never returns NaN/inf, whatever `u` is.
    pub fn denorm(&self, u: f32) -> f32 {
        let u = if u.is_finite() {
            u.clamp(0.0, 1.0)
        } else {
            return self.default;
        };
        let v = match self.curve {
            Curve::Linear => self.min + (self.max - self.min) * u,
            // min is always > 0 for Log specs (asserted by `spec_table_is_sane`).
            Curve::Log => self.min * (self.max / self.min).powf(u),
            Curve::Exp => self.min + (self.max - self.min) * u * u,
        };
        if v.is_finite() {
            v
        } else {
            self.default
        }
    }

    /// Real value -> normalised position, clamped into travel.
    pub fn norm(&self, v: f32) -> f32 {
        if !v.is_finite() {
            return self.norm(self.default);
        }
        let u = match self.curve {
            Curve::Linear => (v - self.min) / (self.max - self.min),
            Curve::Log => (v.max(f32::MIN_POSITIVE) / self.min).ln() / (self.max / self.min).ln(),
            Curve::Exp => (((v - self.min) / (self.max - self.min)).max(0.0)).sqrt(),
        };
        u.clamp(0.0, 1.0)
    }

    pub fn default_norm(&self) -> f32 {
        self.norm(self.default)
    }

    /// `"3.4 kHz"` style readout for knobs and tooltips.
    pub fn format(&self, v: f32) -> String {
        match self.unit {
            "Hz" if v >= 1000.0 => format!("{:.1} kHz", v / 1000.0),
            "ms" if v >= 1000.0 => format!("{:.2} s", v / 1000.0),
            unit => {
                let d = self.decimals as usize;
                match unit {
                    "" => format!("{v:.*}", d),
                    "%" => format!("{v:.*}%", d),
                    _ => format!("{v:.*} {unit}", d),
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectKind {
    Gate,
    Compressor,
    Boost,
    Overdrive,
    Fuzz,
    ParametricEq,
    EnvelopeFilter,
    StepFilter,
    Tremolo,
    Phaser,
    Flanger,
    Chorus,
    RingModulator,
    BitCrusher,
    Delay,
    AnalogDelay,
    Reverb,
}

#[derive(Clone, Copy)]
struct EffectMeta {
    name: &'static str,
    short: &'static str,
    params: &'static [ParamSpec],
}

impl EffectMeta {
    const fn new(name: &'static str, short: &'static str, params: &'static [ParamSpec]) -> Self {
        Self {
            name,
            short,
            params,
        }
    }
}

impl EffectKind {
    pub const ALL: [EffectKind; 17] = [
        EffectKind::Gate,
        EffectKind::Compressor,
        EffectKind::Boost,
        EffectKind::Overdrive,
        EffectKind::Fuzz,
        EffectKind::ParametricEq,
        EffectKind::EnvelopeFilter,
        EffectKind::StepFilter,
        EffectKind::Tremolo,
        EffectKind::Phaser,
        EffectKind::Flanger,
        EffectKind::Chorus,
        EffectKind::RingModulator,
        EffectKind::BitCrusher,
        EffectKind::Delay,
        EffectKind::AnalogDelay,
        EffectKind::Reverb,
    ];

    const fn meta(self) -> EffectMeta {
        match self {
            EffectKind::Gate => EffectMeta::new("Noise Gate", "GATE", &GATE),
            EffectKind::Compressor => EffectMeta::new("Compressor", "COMP", &COMPRESSOR),
            EffectKind::Boost => EffectMeta::new("Boost", "BOOST", &BOOST),
            EffectKind::Overdrive => EffectMeta::new("Overdrive", "OD", &OVERDRIVE),
            EffectKind::Fuzz => EffectMeta::new("Fuzz", "FUZZ", &FUZZ),
            EffectKind::ParametricEq => EffectMeta::new("Parametric EQ", "EQ", &PARAMETRIC_EQ),
            EffectKind::EnvelopeFilter => {
                EffectMeta::new("Envelope Filter", "ENV", &ENVELOPE_FILTER)
            }
            EffectKind::StepFilter => EffectMeta::new("Step Filter", "STEP", &STEP_FILTER),
            EffectKind::Tremolo => EffectMeta::new("Tremolo", "TREM", &TREMOLO),
            EffectKind::Phaser => EffectMeta::new("Phaser", "PHASE", &PHASER),
            EffectKind::Flanger => EffectMeta::new("Flanger", "FLANGE", &FLANGER),
            EffectKind::Chorus => EffectMeta::new("Chorus", "CHOR", &CHORUS),
            EffectKind::RingModulator => EffectMeta::new("Ring Modulator", "RING", &RING_MODULATOR),
            EffectKind::BitCrusher => EffectMeta::new("Bit Crusher", "BITS", &BIT_CRUSHER),
            EffectKind::Delay => EffectMeta::new("Delay", "DLY", &DELAY),
            EffectKind::AnalogDelay => EffectMeta::new("Analog Delay", "ANLG", &ANALOG_DELAY),
            EffectKind::Reverb => EffectMeta::new("Reverb", "RVB", &REVERB),
        }
    }

    pub const fn name(self) -> &'static str {
        self.meta().name
    }

    /// Short label for the pedal-card title bar.
    pub const fn short(self) -> &'static str {
        self.meta().short
    }

    pub const fn params(self) -> &'static [ParamSpec] {
        self.meta().params
    }

    /// Default normalised params, index-aligned with [`ParamSpec`] order.
    /// Real (denormalised) values at the effect's factory settings. This is the shape
    /// [`crate::engine::Proc::process`] receives, so tests and offline callers should use
    /// this rather than handing in normalised 0..=1 numbers.
    pub fn default_values(self) -> ParamVals {
        let norms = self.default_norms();
        let specs = self.params();
        let mut out = ParamVals::ZEROED;
        for (i, spec) in specs.iter().enumerate() {
            out.v[i] = spec.denorm(norms.v[i]);
        }
        out
    }

    pub fn default_norms(self) -> ParamVals {
        let mut v = ParamVals::ZEROED;
        for (i, spec) in self.params().iter().enumerate() {
            v.v[i] = spec.default_norm();
        }
        v
    }
}

const GATE: [ParamSpec; 2] = [
    ParamSpec::lin("threshold", -70.0, -6.0, -42.0, "dB", 1),
    ParamSpec::log("release", 20.0, 900.0, 160.0, "ms", 0),
];

const COMPRESSOR: [ParamSpec; 5] = [
    ParamSpec::lin("threshold", -50.0, 0.0, -24.0, "dB", 1),
    ParamSpec::lin("ratio", 1.0, 12.0, 4.0, ":1", 1),
    ParamSpec::log("attack", 0.3, 60.0, 12.0, "ms", 1),
    ParamSpec::log("release", 20.0, 900.0, 220.0, "ms", 0),
    ParamSpec::lin("makeup", 0.0, 18.0, 6.0, "dB", 1),
];

const BOOST: [ParamSpec; 2] = [
    ParamSpec::lin("level", -12.0, 18.0, 6.0, "dB", 1),
    ParamSpec::lin("tilt", 0.0, 1.0, 0.5, "", 2),
];

const OVERDRIVE: [ParamSpec; 3] = [
    ParamSpec::exp("drive", 0.0, 1.0, 0.55, "", 2),
    ParamSpec::log("tone", 220.0, 9000.0, 3200.0, "Hz", 0),
    ParamSpec::lin("level", -24.0, 12.0, -8.0, "dB", 1),
];

const FUZZ: [ParamSpec; 3] = [
    ParamSpec::exp("fuzz", 0.0, 1.0, 0.75, "", 2),
    ParamSpec::log("tone", 200.0, 8000.0, 2400.0, "Hz", 0),
    ParamSpec::lin("level", -30.0, 6.0, -14.0, "dB", 1),
];

const PARAMETRIC_EQ: [ParamSpec; 6] = [
    ParamSpec::lin("bass", -12.0, 12.0, 0.0, "dB", 1),
    ParamSpec::lin("mid", -12.0, 12.0, 0.0, "dB", 1),
    ParamSpec::log("frequency", 150.0, 3000.0, 800.0, "Hz", 0),
    ParamSpec::log("Q", 0.3, 4.0, 0.9, "", 2),
    ParamSpec::lin("treble", -12.0, 12.0, 0.0, "dB", 1),
    ParamSpec::lin("level", -12.0, 12.0, 0.0, "dB", 1),
];

const ENVELOPE_FILTER: [ParamSpec; 6] = [
    ParamSpec::lin("sensitivity", -24.0, 24.0, 0.0, "dB", 1),
    ParamSpec::log("base", 150.0, 1000.0, 300.0, "Hz", 0),
    ParamSpec::lin("sweep", 0.0, 3.0, 2.0, "oct", 1),
    ParamSpec::log("Q", 0.5, 4.0, 1.5, "", 2),
    ParamSpec::log("release", 30.0, 600.0, 180.0, "ms", 0),
    ParamSpec::lin("mix", 0.0, 1.0, 1.0, "", 2),
];

const STEP_FILTER: [ParamSpec; 6] = [
    ParamSpec::log("frequency", 150.0, 3000.0, 800.0, "Hz", 0),
    ParamSpec::log("Q", 0.5, 4.0, 1.5, "", 2),
    ParamSpec::log("speed", 0.5, 16.0, 4.0, "Hz", 2),
    ParamSpec::lin("steps", 2.0, 9.0, 6.0, "", 0),
    ParamSpec::lin("random", 0.0, 1.0, 0.0, "", 0),
    ParamSpec::lin("mix", 0.0, 1.0, 0.7, "", 2),
];

const TREMOLO: [ParamSpec; 3] = [
    ParamSpec::log("rate", 0.5, 22.0, 5.0, "Hz", 2),
    ParamSpec::exp("depth", 0.0, 1.0, 0.7, "", 2),
    ParamSpec::lin("level", -12.0, 12.0, 0.0, "dB", 1),
];

const PHASER: [ParamSpec; 4] = [
    ParamSpec::log("rate", 0.05, 8.0, 0.5, "Hz", 2),
    ParamSpec::exp("depth", 0.0, 1.0, 0.7, "", 2),
    ParamSpec::lin("feedback", 0.0, 0.7, 0.2, "", 2),
    ParamSpec::lin("mix", 0.0, 1.0, 0.5, "", 2),
];

const FLANGER: [ParamSpec; 5] = [
    ParamSpec::log("rate", 0.05, 5.0, 0.25, "Hz", 2),
    ParamSpec::exp("depth", 0.0, 1.0, 0.7, "", 2),
    ParamSpec::log("base", 0.5, 5.0, 2.0, "ms", 2),
    ParamSpec::lin("feedback", -0.85, 0.85, 0.35, "", 2),
    ParamSpec::lin("mix", 0.0, 1.0, 0.5, "", 2),
];

const CHORUS: [ParamSpec; 4] = [
    ParamSpec::log("rate", 0.05, 8.0, 0.8, "Hz", 2),
    ParamSpec::exp("depth", 0.0, 1.0, 0.5, "", 2),
    ParamSpec::lin("mix", 0.0, 1.0, 0.5, "", 2),
    ParamSpec::log("base", 2.0, 24.0, 8.0, "ms", 1),
];

const RING_MODULATOR: [ParamSpec; 4] = [
    ParamSpec::log("carrier", 20.0, 2000.0, 120.0, "Hz", 1),
    ParamSpec::log("tone", 500.0, 16000.0, 8000.0, "Hz", 0),
    ParamSpec::lin("mix", 0.0, 1.0, 0.5, "", 2),
    ParamSpec::lin("level", -18.0, 6.0, -3.0, "dB", 1),
];

const BIT_CRUSHER: [ParamSpec; 5] = [
    ParamSpec::lin("bits", 4.0, 16.0, 10.0, "", 0),
    ParamSpec::exp("downsample", 1.0, 32.0, 4.0, "×", 0),
    ParamSpec::lin("drive", -12.0, 24.0, 0.0, "dB", 1),
    ParamSpec::log("tone", 500.0, 16000.0, 8000.0, "Hz", 0),
    ParamSpec::lin("mix", 0.0, 1.0, 0.5, "", 2),
];

const DELAY: [ParamSpec; 4] = [
    ParamSpec::log("time", 20.0, 1500.0, 380.0, "ms", 0),
    ParamSpec::exp("feedback", 0.0, 1.0, 0.45, "", 2),
    ParamSpec::lin("mix", 0.0, 1.0, 0.35, "", 2),
    ParamSpec::log("tone", 300.0, 12000.0, 3400.0, "Hz", 0),
];

const ANALOG_DELAY: [ParamSpec; 6] = [
    ParamSpec::log("time", 20.0, 600.0, 350.0, "ms", 0),
    ParamSpec::exp("feedback", 0.0, 0.9, 0.45, "", 2),
    ParamSpec::log("tone", 500.0, 8000.0, 3500.0, "Hz", 0),
    ParamSpec::log("rate", 0.05, 8.0, 0.6, "Hz", 2),
    ParamSpec::exp("depth", 0.0, 1.0, 0.35, "", 2),
    ParamSpec::lin("mix", 0.0, 1.0, 0.35, "", 2),
];

const REVERB: [ParamSpec; 4] = [
    ParamSpec::lin("size", 0.0, 1.0, 0.6, "", 2),
    ParamSpec::lin("decay", 0.0, 1.0, 0.6, "", 2),
    ParamSpec::lin("mix", 0.0, 1.0, 0.3, "", 2),
    ParamSpec::lin("damp", 0.0, 1.0, 0.4, "", 2),
];

/// Amplifier + global controls. Not rack slots: an amp always exists.
pub const AMP_SPECS: [ParamSpec; 7] = [
    ParamSpec::exp("gain", -20.0, 40.0, 16.0, "dB", 1),
    ParamSpec::lin("bass", -15.0, 15.0, 0.0, "dB", 1),
    ParamSpec::lin("mid", -15.0, 15.0, 0.0, "dB", 1),
    ParamSpec::lin("treble", -15.0, 15.0, 0.0, "dB", 1),
    ParamSpec::lin("presence", -15.0, 15.0, 0.0, "dB", 1),
    ParamSpec::exp("master", 0.0, 1.0, 0.3, "", 2),
    ParamSpec::lin("input trim", -24.0, 42.0, 14.0, "dB", 1),
];

/// Real (denormalised) amp values at factory settings — the shape `Amp::process` receives.
pub fn amp_default_values() -> ParamVals {
    let mut out = ParamVals::ZEROED;
    for (i, spec) in AMP_SPECS.iter().enumerate() {
        out.v[i] = spec.denorm(spec.default_norm());
    }
    out
}

pub mod amp_ix {
    pub const GAIN: usize = 0;
    pub const BASS: usize = 1;
    pub const MID: usize = 2;
    pub const TREBLE: usize = 3;
    pub const PRESENCE: usize = 4;
    pub const MASTER: usize = 5;
    pub const TRIM: usize = 6;
}

/// Normalised (`0.0..=1.0`) parameter values, index-aligned with a spec table.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamVals {
    pub v: [f32; MAX_PARAMS],
}

impl ParamVals {
    pub const ZEROED: ParamVals = ParamVals {
        v: [0.0; MAX_PARAMS],
    };

    pub fn from_norms(norms: &[f32]) -> ParamVals {
        let mut out = ParamVals::ZEROED;
        for (i, n) in norms.iter().take(MAX_PARAMS).enumerate() {
            out.v[i] = if n.is_finite() {
                n.clamp(0.0, 1.0)
            } else {
                0.0
            };
        }
        out
    }

    /// Sanitised normalised value; out-of-range/NaN becomes the spec default so a
    /// corrupt preset cannot silently produce an extreme setting.
    pub fn get(&self, spec: &ParamSpec, i: usize) -> f32 {
        let n = self.v.get(i).copied().unwrap_or(0.0);
        if n.is_finite() {
            n.clamp(0.0, 1.0)
        } else {
            spec.default_norm()
        }
    }

    /// Real (denormalised) value for param `i`.
    pub fn value(&self, specs: &[ParamSpec], i: usize) -> f32 {
        match specs.get(i) {
            Some(spec) => spec.denorm(self.get(spec, i)),
            None => 0.0,
        }
    }
}

impl Default for ParamVals {
    fn default() -> Self {
        ParamVals::ZEROED
    }
}

/// Every spec table must be denormalisable in both directions. Runs from a test.
pub fn spec_problems() -> Vec<String> {
    let mut out = Vec::new();
    let mut tables: Vec<(String, &[ParamSpec])> = EffectKind::ALL
        .iter()
        .map(|k| (k.name().to_string(), k.params()))
        .collect();
    tables.push(("amp".to_string(), &AMP_SPECS));
    for (name, table) in tables {
        if table.len() > MAX_PARAMS {
            out.push(format!(
                "{name}: {} params > MAX_PARAMS {MAX_PARAMS}",
                table.len()
            ));
        }
        for spec in table {
            if spec.min >= spec.max {
                out.push(format!(
                    "{name}/{}: min {} >= max {}",
                    spec.name, spec.min, spec.max
                ));
            }
            if spec.curve == Curve::Log && spec.min <= 0.0 {
                out.push(format!("{name}/{}: Log curve needs min > 0", spec.name));
            }
            if !(spec.default >= spec.min && spec.default <= spec.max) {
                out.push(format!(
                    "{name}/{}: default {} outside {}..{}",
                    spec.name, spec.default, spec.min, spec.max
                ));
            }
            // Round-trip: norm(denorm(u)) must be stable across the whole travel.
            for step in 0..=20 {
                let u = step as f32 / 20.0;
                let back = spec.norm(spec.denorm(u));
                if (back - u).abs() > 2e-3 {
                    out.push(format!(
                        "{name}/{}: round-trip {u:.3} -> {:.4} off by more than 2e-3",
                        spec.name, back
                    ));
                    break;
                }
            }
            if !spec.denorm(f32::NAN).is_finite() || !spec.denorm(1e9).is_finite() {
                out.push(format!(
                    "{name}/{}: denorm not finite for bad input",
                    spec.name
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_tables_are_sane() {
        assert_eq!(spec_problems(), Vec::<String>::new());
    }

    #[test]
    fn effect_metadata_is_unique_and_stable() {
        let expected = [
            (EffectKind::Gate, "Noise Gate", "GATE", GATE.as_slice()),
            (
                EffectKind::Compressor,
                "Compressor",
                "COMP",
                COMPRESSOR.as_slice(),
            ),
            (EffectKind::Boost, "Boost", "BOOST", BOOST.as_slice()),
            (
                EffectKind::Overdrive,
                "Overdrive",
                "OD",
                OVERDRIVE.as_slice(),
            ),
            (EffectKind::Fuzz, "Fuzz", "FUZZ", FUZZ.as_slice()),
            (
                EffectKind::ParametricEq,
                "Parametric EQ",
                "EQ",
                PARAMETRIC_EQ.as_slice(),
            ),
            (
                EffectKind::EnvelopeFilter,
                "Envelope Filter",
                "ENV",
                ENVELOPE_FILTER.as_slice(),
            ),
            (
                EffectKind::StepFilter,
                "Step Filter",
                "STEP",
                STEP_FILTER.as_slice(),
            ),
            (EffectKind::Tremolo, "Tremolo", "TREM", TREMOLO.as_slice()),
            (EffectKind::Phaser, "Phaser", "PHASE", PHASER.as_slice()),
            (EffectKind::Flanger, "Flanger", "FLANGE", FLANGER.as_slice()),
            (EffectKind::Chorus, "Chorus", "CHOR", CHORUS.as_slice()),
            (
                EffectKind::RingModulator,
                "Ring Modulator",
                "RING",
                RING_MODULATOR.as_slice(),
            ),
            (
                EffectKind::BitCrusher,
                "Bit Crusher",
                "BITS",
                BIT_CRUSHER.as_slice(),
            ),
            (EffectKind::Delay, "Delay", "DLY", DELAY.as_slice()),
            (
                EffectKind::AnalogDelay,
                "Analog Delay",
                "ANLG",
                ANALOG_DELAY.as_slice(),
            ),
            (EffectKind::Reverb, "Reverb", "RVB", REVERB.as_slice()),
        ];
        let mut names = std::collections::HashSet::new();
        let mut shorts = std::collections::HashSet::new();

        assert_eq!(EffectKind::ALL.len(), expected.len());
        for (actual, (kind, name, short, params)) in EffectKind::ALL.iter().zip(expected) {
            assert_eq!(*actual, kind, "effect catalog order changed");
            assert_eq!(kind.name(), name);
            assert_eq!(kind.short(), short);
            assert_eq!(kind.params(), params);
            assert!(names.insert(kind.name()));
            assert!(shorts.insert(kind.short()));
        }
    }

    #[test]
    fn pre_omar_parameter_contracts_are_literal_and_stable() {
        let expected: [(EffectKind, &[ParamSpec]); 14] = [
            (
                EffectKind::Gate,
                &[
                    ParamSpec::lin("threshold", -70.0, -6.0, -42.0, "dB", 1),
                    ParamSpec::log("release", 20.0, 900.0, 160.0, "ms", 0),
                ],
            ),
            (
                EffectKind::Compressor,
                &[
                    ParamSpec::lin("threshold", -50.0, 0.0, -24.0, "dB", 1),
                    ParamSpec::lin("ratio", 1.0, 12.0, 4.0, ":1", 1),
                    ParamSpec::log("attack", 0.3, 60.0, 12.0, "ms", 1),
                    ParamSpec::log("release", 20.0, 900.0, 220.0, "ms", 0),
                    ParamSpec::lin("makeup", 0.0, 18.0, 6.0, "dB", 1),
                ],
            ),
            (
                EffectKind::Boost,
                &[
                    ParamSpec::lin("level", -12.0, 18.0, 6.0, "dB", 1),
                    ParamSpec::lin("tilt", 0.0, 1.0, 0.5, "", 2),
                ],
            ),
            (
                EffectKind::Overdrive,
                &[
                    ParamSpec::exp("drive", 0.0, 1.0, 0.55, "", 2),
                    ParamSpec::log("tone", 220.0, 9000.0, 3200.0, "Hz", 0),
                    ParamSpec::lin("level", -24.0, 12.0, -8.0, "dB", 1),
                ],
            ),
            (
                EffectKind::Fuzz,
                &[
                    ParamSpec::exp("fuzz", 0.0, 1.0, 0.75, "", 2),
                    ParamSpec::log("tone", 200.0, 8000.0, 2400.0, "Hz", 0),
                    ParamSpec::lin("level", -30.0, 6.0, -14.0, "dB", 1),
                ],
            ),
            (
                EffectKind::ParametricEq,
                &[
                    ParamSpec::lin("bass", -12.0, 12.0, 0.0, "dB", 1),
                    ParamSpec::lin("mid", -12.0, 12.0, 0.0, "dB", 1),
                    ParamSpec::log("frequency", 150.0, 3000.0, 800.0, "Hz", 0),
                    ParamSpec::log("Q", 0.3, 4.0, 0.9, "", 2),
                    ParamSpec::lin("treble", -12.0, 12.0, 0.0, "dB", 1),
                    ParamSpec::lin("level", -12.0, 12.0, 0.0, "dB", 1),
                ],
            ),
            (
                EffectKind::EnvelopeFilter,
                &[
                    ParamSpec::lin("sensitivity", -24.0, 24.0, 0.0, "dB", 1),
                    ParamSpec::log("base", 150.0, 1000.0, 300.0, "Hz", 0),
                    ParamSpec::lin("sweep", 0.0, 3.0, 2.0, "oct", 1),
                    ParamSpec::log("Q", 0.5, 4.0, 1.5, "", 2),
                    ParamSpec::log("release", 30.0, 600.0, 180.0, "ms", 0),
                    ParamSpec::lin("mix", 0.0, 1.0, 1.0, "", 2),
                ],
            ),
            (
                EffectKind::Tremolo,
                &[
                    ParamSpec::log("rate", 0.5, 22.0, 5.0, "Hz", 2),
                    ParamSpec::exp("depth", 0.0, 1.0, 0.7, "", 2),
                    ParamSpec::lin("level", -12.0, 12.0, 0.0, "dB", 1),
                ],
            ),
            (
                EffectKind::Phaser,
                &[
                    ParamSpec::log("rate", 0.05, 8.0, 0.5, "Hz", 2),
                    ParamSpec::exp("depth", 0.0, 1.0, 0.7, "", 2),
                    ParamSpec::lin("feedback", 0.0, 0.7, 0.2, "", 2),
                    ParamSpec::lin("mix", 0.0, 1.0, 0.5, "", 2),
                ],
            ),
            (
                EffectKind::Flanger,
                &[
                    ParamSpec::log("rate", 0.05, 5.0, 0.25, "Hz", 2),
                    ParamSpec::exp("depth", 0.0, 1.0, 0.7, "", 2),
                    ParamSpec::log("base", 0.5, 5.0, 2.0, "ms", 2),
                    ParamSpec::lin("feedback", -0.85, 0.85, 0.35, "", 2),
                    ParamSpec::lin("mix", 0.0, 1.0, 0.5, "", 2),
                ],
            ),
            (
                EffectKind::Chorus,
                &[
                    ParamSpec::log("rate", 0.05, 8.0, 0.8, "Hz", 2),
                    ParamSpec::exp("depth", 0.0, 1.0, 0.5, "", 2),
                    ParamSpec::lin("mix", 0.0, 1.0, 0.5, "", 2),
                    ParamSpec::log("base", 2.0, 24.0, 8.0, "ms", 1),
                ],
            ),
            (
                EffectKind::BitCrusher,
                &[
                    ParamSpec::lin("bits", 4.0, 16.0, 10.0, "", 0),
                    ParamSpec::exp("downsample", 1.0, 32.0, 4.0, "×", 0),
                    ParamSpec::lin("drive", -12.0, 24.0, 0.0, "dB", 1),
                    ParamSpec::log("tone", 500.0, 16000.0, 8000.0, "Hz", 0),
                    ParamSpec::lin("mix", 0.0, 1.0, 0.5, "", 2),
                ],
            ),
            (
                EffectKind::Delay,
                &[
                    ParamSpec::log("time", 20.0, 1500.0, 380.0, "ms", 0),
                    ParamSpec::exp("feedback", 0.0, 1.0, 0.45, "", 2),
                    ParamSpec::lin("mix", 0.0, 1.0, 0.35, "", 2),
                    ParamSpec::log("tone", 300.0, 12000.0, 3400.0, "Hz", 0),
                ],
            ),
            (
                EffectKind::Reverb,
                &[
                    ParamSpec::lin("size", 0.0, 1.0, 0.6, "", 2),
                    ParamSpec::lin("decay", 0.0, 1.0, 0.6, "", 2),
                    ParamSpec::lin("mix", 0.0, 1.0, 0.3, "", 2),
                    ParamSpec::lin("damp", 0.0, 1.0, 0.4, "", 2),
                ],
            ),
        ];

        for (kind, contract) in expected {
            assert_eq!(
                kind.params(),
                contract,
                "{kind:?} parameter contract changed"
            );
        }
    }

    #[test]
    fn denorm_endpoints_and_nan_are_safe() {
        let s = &DELAY[0]; // log 20..1500 ms
        assert!((s.denorm(0.0) - 20.0).abs() < 1e-3);
        assert!((s.denorm(1.0) - 1500.0).abs() < 1e-2);
        assert_eq!(s.denorm(f32::NAN), s.default);
        assert!((s.denorm(9.0) - 1500.0).abs() < 1e-2);
        assert!((s.denorm(-9.0)).abs() - 20.0 < 1e-3);
    }

    #[test]
    fn log_and_exp_curves_are_monotonic() {
        for table in [OVERDRIVE.as_slice(), DELAY.as_slice(), REVERB.as_slice()] {
            for spec in table {
                let mut last = f32::MIN;
                for step in 0..=100 {
                    let v = spec.denorm(step as f32 / 100.0);
                    assert!(v >= last - 1e-4, "{} not monotonic", spec.name);
                    last = v;
                }
            }
        }
    }

    #[test]
    fn amp_spec_count_matches_indices() {
        assert_eq!(AMP_SPECS.len(), 7);
        assert_eq!(AMP_SPECS[amp_ix::MASTER].name, "master");
        assert_eq!(AMP_SPECS[amp_ix::TRIM].name, "input trim");
    }

    #[test]
    fn from_norms_clamps_junk() {
        let p = ParamVals::from_norms(&[f32::NAN, 5.0, -3.0, 0.25]);
        assert_eq!(p.v[0], 0.0);
        assert_eq!(p.v[1], 1.0);
        assert_eq!(p.v[2], 0.0);
        assert!((p.v[3] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn format_reads_like_a_pedal() {
        assert_eq!(DELAY[0].format(380.0), "380 ms");
        assert_eq!(DELAY[0].format(1300.0), "1.30 s");
        assert_eq!(OVERDRIVE[1].format(3200.0), "3.2 kHz");
        assert_eq!(REVERB[0].format(0.6), "0.60");
    }
}
