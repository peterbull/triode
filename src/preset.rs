//! Presets: the serialisable description of a rack plus the amp knobs.
//!
//! Only normalised (`0..=1`) knob values are stored, for two reasons: a preset stays
//! meaningful if a parameter's range is retuned later, and a corrupt or hand-edited file
//! can only ever select a valid knob position — never an out-of-range value that would
//! need a second layer of validation at every use.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine::Slot;
use crate::params::{EffectKind, AMP_SPECS, MAX_SLOTS};

fn yes() -> bool {
    true
}

fn default_name() -> String {
    "untitled".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SlotPreset {
    pub kind: EffectKind,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Knob values in `EffectKind::params()` order. Shorter is fine: missing knobs keep
    /// their defaults, which lets a preset written for an older effect still load.
    #[serde(default)]
    pub params: Vec<f32>,
}

impl SlotPreset {
    /// A slot with the kind's factory settings.
    pub fn new(kind: EffectKind) -> SlotPreset {
        SlotPreset {
            kind,
            enabled: true,
            params: Vec::new(),
        }
    }

    /// A slot with real (denormalised) values, for writing presets by hand in tests.
    pub fn with_values(kind: EffectKind, values: &[f32]) -> SlotPreset {
        let norms: Vec<f32> = kind
            .params()
            .iter()
            .zip(values)
            .map(|(spec, v)| spec.norm(*v))
            .collect();
        SlotPreset {
            kind,
            enabled: true,
            params: norms,
        }
    }

    /// Normalised values with bad entries replaced by the knob's default.
    pub fn norms(&self) -> Vec<f32> {
        let specs = self.kind.params();
        (0..specs.len())
            .map(|i| match self.params.get(i) {
                Some(n) if n.is_finite() => n.clamp(0.0, 1.0),
                _ => specs[i].default_norm(),
            })
            .collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preset {
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default)]
    pub slots: Vec<SlotPreset>,
    /// Amp knobs in `AMP_SPECS` order (gain, bass, mid, treble, presence, master, trim).
    #[serde(default)]
    pub amp: Vec<f32>,
    /// Cabinet simulation on/off.
    #[serde(default = "yes")]
    pub cab: bool,
    /// Input high-pass filter on/off.
    #[serde(default)]
    pub hpf: bool,
}

impl Preset {
    /// The built-in starting sound: a light overdrive into a short plate, both audible
    /// enough to tell that audio is moving without being a mess to play through.
    pub fn blues() -> Preset {
        Preset {
            name: "blues".to_string(),
            slots: vec![
                SlotPreset::with_values(EffectKind::Gate, &[-54.0, 120.0]),
                SlotPreset::with_values(EffectKind::Overdrive, &[0.35, 2600.0, -4.0]),
                SlotPreset::with_values(EffectKind::Delay, &[340.0, 0.25, 0.2, 2400.0]),
                SlotPreset::with_values(EffectKind::Reverb, &[0.55, 0.5, 0.28, 0.4]),
            ],
            amp: AMP_SPECS.iter().map(|s| s.default_norm()).collect(),
            cab: true,
            hpf: true,
        }
    }

    /// An empty rack at unity: the safest thing to open a fresh session with.
    pub fn empty() -> Preset {
        Preset {
            name: "empty".to_string(),
            slots: Vec::new(),
            amp: AMP_SPECS.iter().map(|s| s.default_norm()).collect(),
            cab: true,
            hpf: true,
        }
    }

    /// Build rack slots for `sr`. Processors are allocated here, on the calling (UI)
    /// thread, so the audio thread only ever receives finished `Slot`s.
    pub fn to_slots(&self, sr: f32) -> Vec<Slot> {
        self.slots
            .iter()
            .take(MAX_SLOTS)
            .map(|sp| {
                let mut slot = Slot::build(sp.kind, sp.enabled, sr);
                for (i, n) in sp.norms().iter().enumerate() {
                    slot.target.v[i] = *n;
                    // Start the smoother at the target: a preset load should not sweep
                    // every knob from wherever the previous rack happened to sit.
                    slot.smooth.v[i] = *n;
                    slot.values.v[i] = slot.kind.params()[i].denorm(*n);
                }
                slot
            })
            .collect()
    }

    /// Amp knobs, normalised, defaults filled in for anything missing.
    pub fn amp_norms(&self) -> Vec<f32> {
        AMP_SPECS
            .iter()
            .enumerate()
            .map(|(i, spec)| match self.amp.get(i) {
                Some(n) if n.is_finite() => n.clamp(0.0, 1.0),
                _ => spec.default_norm(),
            })
            .collect()
    }

    /// Clamp and fill everything. Applied after loading a file from disk, which is
    /// untrusted input like any other.
    pub fn sanitize(&mut self) {
        if self.name.trim().is_empty() {
            self.name = default_name();
        }
        self.slots.truncate(MAX_SLOTS);
        for sp in self.slots.iter_mut() {
            let specs = sp.kind.params();
            sp.params.truncate(specs.len());
            for (i, spec) in specs.iter().enumerate() {
                let n = match sp.params.get(i) {
                    Some(v) if v.is_finite() => v.clamp(0.0, 1.0),
                    _ => spec.default_norm(),
                };
                if sp.params.len() <= i {
                    sp.params.push(n);
                } else {
                    sp.params[i] = n;
                }
            }
        }
        let specs = &AMP_SPECS[..];
        self.amp.truncate(specs.len());
        for (i, spec) in specs.iter().enumerate() {
            let n = match self.amp.get(i) {
                Some(v) if v.is_finite() => v.clamp(0.0, 1.0),
                _ => spec.default_norm(),
            };
            if self.amp.len() <= i {
                self.amp.push(n);
            } else {
                self.amp[i] = n;
            }
        }
    }

    /// Pretty JSON, which is the point: presets are meant to be hand-editable.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Parse and sanitize. Returns a readable message for the UI's status line.
    pub fn from_json(text: &str) -> Result<Preset, String> {
        let mut p: Preset = serde_json::from_str(text).map_err(|e| format!("bad preset: {e}"))?;
        p.sanitize();
        Ok(p)
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, self.to_json())
    }

    pub fn load_from(path: &Path) -> Result<Preset, String> {
        let text =
            fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Preset::from_json(&text)
    }
}

/// Presets live in `./presets` by default, overridable with `TRIODE_PRESETS` — a
/// directory next to the project beats a hidden one under `~` for files you are meant to
/// edit and check in.
pub fn preset_dir() -> PathBuf {
    match std::env::var_os("TRIODE_PRESETS") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from("presets"),
    }
}

fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "preset".to_string()
    } else {
        trimmed.chars().take(64).collect()
    }
}

impl Preset {
    /// Write into `dir` using the preset's own name. Returns the path written.
    pub fn save(&self, dir: &Path) -> io::Result<PathBuf> {
        let path = dir.join(format!("{}.json", safe_name(&self.name)));
        self.save_to(&path)?;
        Ok(path)
    }

    pub fn load(dir: &Path, name: &str) -> Result<Preset, String> {
        Preset::load_from(&dir.join(format!("{}.json", safe_name(name))))
    }

    /// Preset file names (without `.json`) in `dir`, sorted. A missing directory is not
    /// an error — it just means nothing saved yet.
    pub fn list(dir: &Path) -> Vec<String> {
        let mut names = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        names.push(stem.to_string());
                    }
                }
            }
        }
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::ParamVals;

    #[test]
    fn blues_survives_a_json_round_trip() {
        let p = Preset::blues();
        let text = p.to_json();
        let back = Preset::from_json(&text).expect("round trip");
        assert_eq!(back.name, p.name);
        assert_eq!(back.slots.len(), p.slots.len());
        assert_eq!(back.cab, p.cab);
        assert_eq!(back.hpf, p.hpf);
        for (a, b) in p.slots.iter().zip(&back.slots) {
            assert_eq!(a.kind, b.kind);
            assert_eq!(a.enabled, b.enabled);
            assert_eq!(a.params.len(), b.params.len());
            for (x, y) in a.params.iter().zip(&b.params) {
                assert!((x - y).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn every_effect_kind_round_trips_without_rack_truncation() {
        for kind in EffectKind::ALL {
            let params: Vec<f32> = kind
                .params()
                .iter()
                .enumerate()
                .map(|(index, _)| (index as f32 + 1.0) / 10.0)
                .collect();
            let preset = Preset {
                name: kind.name().into(),
                slots: vec![SlotPreset {
                    kind,
                    enabled: false,
                    params: params.clone(),
                }],
                ..Preset::empty()
            };
            let decoded = Preset::from_json(&preset.to_json()).expect("effect round trip");
            assert_eq!(decoded.slots.len(), 1, "{kind:?}");
            assert_eq!(decoded.slots[0].kind, kind);
            assert!(!decoded.slots[0].enabled);
            assert_eq!(decoded.slots[0].params, params);
        }
    }

    #[test]
    fn existing_effect_names_are_stable_json_contracts() {
        for (kind, spelling) in [
            (EffectKind::Gate, "\"Gate\""),
            (EffectKind::Compressor, "\"Compressor\""),
            (EffectKind::Boost, "\"Boost\""),
            (EffectKind::Overdrive, "\"Overdrive\""),
            (EffectKind::Fuzz, "\"Fuzz\""),
            (EffectKind::ParametricEq, "\"ParametricEq\""),
            (EffectKind::EnvelopeFilter, "\"EnvelopeFilter\""),
            (EffectKind::StepFilter, "\"StepFilter\""),
            (EffectKind::Tremolo, "\"Tremolo\""),
            (EffectKind::Phaser, "\"Phaser\""),
            (EffectKind::Flanger, "\"Flanger\""),
            (EffectKind::Chorus, "\"Chorus\""),
            (EffectKind::RingModulator, "\"RingModulator\""),
            (EffectKind::BitCrusher, "\"BitCrusher\""),
            (EffectKind::Delay, "\"Delay\""),
            (EffectKind::AnalogDelay, "\"AnalogDelay\""),
            (EffectKind::Reverb, "\"Reverb\""),
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, spelling);
            assert_eq!(serde_json::from_str::<EffectKind>(&json).unwrap(), kind);
        }
    }

    #[test]
    fn new_effect_names_are_stable_json_contracts() {
        for (kind, spelling) in [
            (EffectKind::ReverseDelay, "\"ReverseDelay\""),
            (EffectKind::PitchShifter, "\"PitchShifter\""),
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, spelling);
            assert_eq!(serde_json::from_str::<EffectKind>(&json).unwrap(), kind);
        }
    }

    #[test]
    fn legacy_effect_names_and_values_keep_their_meaning() {
        let json = r#"{"name":"legacy","slots":[{"kind":"Gate","enabled":false,"params":[0.25,0.75]},{"kind":"Overdrive","params":[0.1,0.2,0.3]},{"kind":"Delay","params":[0.4,0.5,0.6,0.7]},{"kind":"Reverb","params":[0.8,0.7,0.6,0.5]}],"amp":[0.1,0.2,0.3,0.4,0.5,0.6,0.7],"cab":true,"hpf":true}"#;
        let preset = Preset::from_json(json).expect("legacy preset");
        assert_eq!(
            preset
                .slots
                .iter()
                .map(|slot| slot.kind)
                .collect::<Vec<_>>(),
            [
                EffectKind::Gate,
                EffectKind::Overdrive,
                EffectKind::Delay,
                EffectKind::Reverb,
            ]
        );
        assert_eq!(preset.slots[0].params, [0.25, 0.75]);
        assert_eq!(preset.slots[2].params, [0.4, 0.5, 0.6, 0.7]);
        assert_eq!(preset.amp, [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7]);
        assert!(preset.cab && preset.hpf);
    }

    #[test]
    fn slots_are_built_with_their_knobs_not_defaults() {
        let p = Preset::blues();
        let slots = p.to_slots(48000.0);
        assert_eq!(slots.len(), 4);
        let od = slots
            .iter()
            .find(|s| s.kind == EffectKind::Overdrive)
            .unwrap();
        // `values` already hold denormalised real units (the Proc contract), so compare
        // them directly -- running spec.denorm() over them again measures nothing.
        assert!(
            (od.values.v[2] - (-4.0)).abs() < 0.2,
            "preset value not applied: {}",
            od.values.v[2]
        );
        // smoother starts at the target so loading does not sweep
        assert!((od.smooth.v[2] - od.target.v[2]).abs() < 1e-6);
    }

    #[test]
    fn junk_in_a_file_cannot_produce_an_out_of_range_knob() {
        let junk = r#"{
            "name": "",
            "cab": "not a bool",
            "slots": [
                {"kind": "Reverb", "params": [2.0, -3.0, 1e300, "x", 0.5]},
                {"kind": "NoSuchEffect", "params": []}
            ],
            "amp": [99.0, -99.0]
        }"#;
        // Unknown effect kinds must be rejected, not defaulted to something surprising.
        assert!(
            Preset::from_json(junk).is_err(),
            "unknown kind should fail to parse"
        );

        let mostly_junk = r#"{
            "name": "  ",
            "slots": [{"kind": "Reverb", "params": [2.0, -3.0, 1e300, 0.5]}],
            "amp": [99.0, -99.0]
        }"#;
        let mut p = Preset::from_json(mostly_junk).unwrap();
        p.sanitize();
        assert!(!p.name.trim().is_empty());
        for sp in &p.slots {
            assert_eq!(sp.params.len(), sp.kind.params().len());
            assert!(
                sp.params.iter().all(|n| (0.0..=1.0).contains(n)),
                "unclamped knob"
            );
        }
        assert!(p.amp.len() == AMP_SPECS.len());
        assert!(p.amp.iter().all(|n| (0.0..=1.0).contains(n)));
    }

    #[test]
    fn missing_knobs_and_slots_fall_back_to_defaults() {
        for kind in EffectKind::ALL {
            let kind_json = serde_json::to_string(&kind).unwrap();
            let text = format!(r#"{{"slots":[{{"kind":{kind_json},"params":[]}}]}}"#);
            let slots = Preset::from_json(&text).unwrap().to_slots(48_000.0);
            let defaults = kind.default_norms();
            assert_eq!(slots.len(), 1, "{kind:?}");
            assert_eq!(
                &slots[0].target.v[..kind.params().len()],
                &defaults.v[..kind.params().len()],
                "{kind:?} missing parameters did not use defaults"
            );
        }

        // Written against an older version of the effect: fewer knobs than it now has.
        let short = r#"{"name":"old","slots":[{"kind":"Delay","params":[0.4]}]}"#;
        let slots = Preset::from_json(short).unwrap().to_slots(48_000.0);
        for (i, spec) in EffectKind::Delay.params().iter().enumerate().skip(1) {
            assert!(
                (slots[0].target.v[i] - spec.default_norm()).abs() < 1e-6,
                "knob {i} not defaulted"
            );
        }

        // An empty rack is legal.
        assert!(Preset::from_json("{}").unwrap().slots.is_empty());
    }

    #[test]
    fn more_slots_than_the_engine_reserves_are_dropped_not_wrapped() {
        let mut p = Preset::empty();
        p.slots = (0..(MAX_SLOTS + 5))
            .map(|_| SlotPreset::new(EffectKind::Boost))
            .collect();
        let slots = p.to_slots(48000.0);
        assert_eq!(slots.len(), MAX_SLOTS);
        p.sanitize();
        assert_eq!(p.slots.len(), MAX_SLOTS);
    }

    #[test]
    fn names_are_never_a_path_escape() {
        assert_eq!(safe_name("../../etc/passwd"), "etc-passwd");
        assert_eq!(safe_name("  "), "preset");
        assert_eq!(safe_name("Blues Solo '81"), "Blues-Solo--81");
        assert!(safe_name(&"x".repeat(200)).len() <= 64);
    }

    #[test]
    fn save_list_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("triode-presets-{}", std::process::id()));
        let p = Preset::blues();
        let path = p.save(&dir).unwrap();
        assert!(path.exists());
        let names = Preset::list(&dir);
        assert!(names.iter().any(|n| n == "blues"), "{names:?}");
        let back = Preset::load(&dir, "blues").unwrap();
        assert_eq!(back.slots.len(), 4);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_a_message_not_a_panic() {
        let err = Preset::load(Path::new("/nonexistent-triode-dir"), "nope").unwrap_err();
        assert!(err.contains("cannot read"), "unhelpful error: {err}");
    }

    #[test]
    fn amp_norms_count_matches_the_spec_table() {
        let p = Preset::blues();
        assert_eq!(p.amp_norms().len(), AMP_SPECS.len());
        // An amp list that is too short still yields every knob.
        let mut partial = Preset::blues();
        partial.amp = vec![0.5];
        let norms = partial.amp_norms();
        assert_eq!(norms.len(), AMP_SPECS.len());
        assert!((norms[0] - 0.5).abs() < 1e-6);
        assert!(norms[1..].iter().all(|n| (0.0..=1.0).contains(n)));
    }

    #[test]
    fn norms_helper_replaces_nan_with_the_default() {
        let sp = SlotPreset {
            kind: EffectKind::Delay,
            enabled: true,
            params: vec![f32::NAN, 0.5],
        };
        let norms = sp.norms();
        let specs = EffectKind::Delay.params();
        assert!((norms[0] - specs[0].default_norm()).abs() < 1e-6);
        assert!((norms[1] - 0.5).abs() < 1e-6);
        assert!((norms[2] - specs[2].default_norm()).abs() < 1e-6);
        // and the resulting values are usable
        let vals = ParamVals::from_norms(&norms);
        assert!(vals.v.iter().all(|v| v.is_finite()));
    }
}
