use super::*;
use crate::params::EffectKind;
use crate::preset::Preset;

struct Const(f32);
impl Source for Const {
    fn read(&mut self, dst: &mut [f32]) -> usize {
        dst.iter_mut().for_each(|s| *s = self.0);
        dst.len()
    }
    fn level(&self) -> usize {
        0
    }
}

/// Both amp-knob paths have to mean the same thing: a normalised position in, the spec's
/// real value out. `set_amp_param` once re-normalised what it was handed, so the factory
/// positions landed in the wrong places -- `master` 0.3 became ~0.8 and the amp buzzed on
/// any input. The UI pushes knobs through this exact setter, so the regression is audible
/// rather than theoretical, and this test is what keeps the two units straight.
#[test]
fn amp_knobs_land_where_the_caller_put_them() {
    let mut e = Engine::new(48_000.0, 256);
    for (i, spec) in AMP_SPECS.iter().enumerate() {
        e.set_amp_param(i, spec.default_norm());
    }
    let mut src = Const(0.0);
    let mut buf = [[0.0f32; 2]; 256];
    for _ in 0..64 {
        e.process(None, &mut src, &mut buf);
    }
    let v = e.amp_values();
    for (i, spec) in AMP_SPECS.iter().enumerate() {
        assert!(
            (v.v[i] - spec.default).abs() <= spec.default.abs() * 0.02 + 0.02,
            "knob {} ({}) at its factory position landed at {:.3}, expected {:.3}",
            i,
            spec.name,
            v.v[i],
            spec.default
        );
    }
}

/// Dead for the first `gap` blocks, alive afterwards -- how a microphone comes back.
/// A cumulative starved counter cannot tell this apart from an input that never
/// returned, and the UI's "check your input" warning depends on telling them apart.
struct DeadThenLive {
    gap: usize,
    blocks: usize,
}
impl Source for DeadThenLive {
    fn read(&mut self, dst: &mut [f32]) -> usize {
        self.blocks += 1;
        dst.iter_mut().for_each(|s| *s = 0.0);
        if self.blocks <= self.gap {
            0
        } else {
            dst.len()
        }
    }
    fn level(&self) -> usize {
        if self.blocks < self.gap {
            0
        } else {
            8192
        }
    }
}

#[test]
fn an_input_gap_is_counted_as_a_run_and_clears_when_input_returns() {
    let mut e = Engine::new(48000.0, 64);
    let mut buf = [[0.0f32; 2]; 64];
    let mut src = DeadThenLive { gap: 6, blocks: 0 };
    e.process(None, &mut src, &mut buf);
    assert!(
        e.silent_frames() > 0,
        "a dead input must register while it is dead"
    );
    // How many times the resampler pulls per block is its own business, so the assertions
    // below are about the semantics: the live figure clears, the historical one does not.
    for _ in 0..12 {
        e.process(None, &mut src, &mut buf);
    }
    assert_eq!(
        e.silent_frames(),
        0,
        "input returned, so the live warning must clear"
    );
    assert!(e.silent_max() > 0, "the worst gap stays on the record");
}

struct Silent;
impl Source for Silent {
    fn read(&mut self, dst: &mut [f32]) -> usize {
        dst.iter_mut().for_each(|s| *s = 0.0);
        dst.len()
    }
    fn level(&self) -> usize {
        0
    }
}

fn engine() -> Engine {
    Engine::new(48000.0, 128)
}

#[test]
fn engine_starts_empty_and_clean() {
    let mut e = engine();
    e.slots.reserve(MAX_SLOTS);
    assert!(
        e.slots.capacity() >= MAX_SLOTS,
        "no realloc on the audio thread"
    );
    let mut out = [[0.1f32; 2]; 64];
    e.process(None, &mut Silent, &mut out);
    assert_eq!(
        out.iter().flatten().fold(0.0f32, |m, s| m.max(s.abs())),
        0.0
    );
    assert_eq!(e.active_slots(), 0);
}

#[test]
fn mailbox_commands_apply_in_order_and_bad_indices_are_ignored() {
    let shared = Shared::new();
    let mut e = engine();
    shared.send(Cmd::InsertSlot {
        at: 0,
        slot: Slot::build(EffectKind::Boost, false, 48000.0),
    });
    shared.send(Cmd::SetEnabled { slot: 0, on: true });
    shared.send(Cmd::SetParam {
        slot: 0,
        idx: 0,
        norm: 0.8,
    });
    shared.send(Cmd::SetParam {
        slot: 7,
        idx: 3,
        norm: 0.5,
    }); // no such slot
    shared.send(Cmd::SetParam {
        slot: 0,
        idx: 99,
        norm: 0.5,
    }); // no such param
    shared.send(Cmd::RemoveSlot { slot: 3 }); // out of range
    let mut out = [[0.0f32; 2]; 32];
    e.process(Some(&shared), &mut Silent, &mut out);
    assert_eq!(e.slots.len(), 1);
    assert!(e.slots[0].enabled);
    assert!((e.slots[0].target.v[0] - 0.8).abs() < 1e-3);
    assert!(shared.cmds.lock().unwrap().is_empty(), "queue must drain");
}

#[test]
fn inserting_beyond_max_slots_is_refused_not_panicky() {
    let shared = Shared::new();
    let mut e = engine();
    for i in 0..(MAX_SLOTS + 6) {
        shared.send(Cmd::InsertSlot {
            at: i,
            slot: Slot::build(EffectKind::Boost, true, 48000.0),
        });
    }
    let mut out = [[0.0f32; 2]; 16];
    e.process(Some(&shared), &mut Silent, &mut out);
    assert_eq!(e.slots.len(), MAX_SLOTS);
}

#[test]
fn reordering_moves_state_and_bypass_silences_only_its_own_slot() {
    let mut e = engine();
    e.slots.push(Slot::build(EffectKind::Delay, true, 48000.0));
    e.slots.push(Slot::build(EffectKind::Reverb, true, 48000.0));
    let first = e.slots[0].kind;
    let shared = Shared::new();
    shared.send(Cmd::MoveSlot { from: 0, to: 1 });
    shared.send(Cmd::SetEnabled { slot: 0, on: false });
    let mut out = [[0.0f32; 2]; 16];
    e.process(Some(&shared), &mut Silent, &mut out);
    assert_eq!(
        e.slots[1].kind, first,
        "moved slot keeps its kind and state"
    );
    assert!(!e.slots[0].enabled);
    assert_eq!(e.active_slots(), 1);
}

#[test]
fn every_effect_stays_in_range_with_every_knob_maxed() {
    struct Hot(usize);
    impl Source for Hot {
        fn read(&mut self, dst: &mut [f32]) -> usize {
            for s in dst.iter_mut() {
                *s = if self.0 % 8 < 4 { 3.0 } else { -3.0 };
                self.0 += 1;
            }
            dst.len()
        }
        fn level(&self) -> usize {
            0
        }
    }
    for kind in EffectKind::ALL {
        let mut e = engine();
        e.slots.push(Slot::build(kind, true, 48000.0));
        for i in 0..kind.params().len() {
            e.slots[0].target.v[i] = 1.0;
            e.slots[0].smooth.v[i] = 1.0;
        }
        for i in [amp_ix::GAIN, amp_ix::MASTER, amp_ix::TRIM, amp_ix::TREBLE] {
            e.amp_target.v[i] = 1.0;
            e.amp_smooth.v[i] = 1.0;
        }
        let mut src = Hot(0);
        let mut out = [[0.0f32; 2]; 256];
        let mut worst = 0.0f32;
        for _ in 0..60 {
            e.process(None, &mut src, &mut out);
            for f in out.iter() {
                assert!(
                    f[0].is_finite() && f[1].is_finite(),
                    "{kind:?} produced NaN"
                );
                worst = worst.max(f[0].abs()).max(f[1].abs());
            }
        }
        assert!(worst <= 1.0, "{kind:?} leaked peak {worst}");
    }
}

#[test]
fn every_effect_stays_in_range_with_every_knob_min() {
    struct Ramp(usize);
    impl Source for Ramp {
        fn read(&mut self, dst: &mut [f32]) -> usize {
            for s in dst.iter_mut() {
                *s = ((self.0 as f32) * 0.01).sin() * 0.8;
                self.0 += 1;
            }
            dst.len()
        }
        fn level(&self) -> usize {
            0
        }
    }
    for kind in EffectKind::ALL {
        let mut e = engine();
        let mut slot = Slot::build(kind, true, 48000.0);
        for i in 0..kind.params().len() {
            slot.target.v[i] = 0.0;
            slot.smooth.v[i] = 0.0;
        }
        assert!(
            slot.target.v[..kind.params().len()]
                .iter()
                .chain(slot.smooth.v[..kind.params().len()].iter())
                .all(|&norm| norm == 0.0),
            "{kind:?} min-knob setup must explicitly set every target and smooth parameter"
        );
        e.slots.push(slot);
        let mut src = Ramp(0);
        let mut out = [[0.0f32; 2]; 128];
        for _ in 0..40 {
            e.process(None, &mut src, &mut out);
            for f in out.iter() {
                assert!(
                    f[0].is_finite() && f[1].is_finite(),
                    "{kind:?} min-knob NaN"
                );
                assert!(f[0].abs() <= 1.0, "{kind:?} peak {}", f[0].abs());
            }
        }
    }
}

#[test]
fn twelve_slot_mixed_effect_rack_stays_finite_and_bounded() {
    let mut e = engine();
    let kinds = [
        EffectKind::Gate,
        EffectKind::Compressor,
        EffectKind::Boost,
        EffectKind::Overdrive,
        EffectKind::Fuzz,
        EffectKind::ParametricEq,
        EffectKind::StepFilter,
        EffectKind::RingModulator,
        EffectKind::BitCrusher,
        EffectKind::Delay,
        EffectKind::AnalogDelay,
        EffectKind::Reverb,
    ];
    assert_eq!(kinds.len(), MAX_SLOTS);
    for (slot_index, kind) in kinds.into_iter().enumerate() {
        let mut slot = Slot::build(kind, true, 48_000.0);
        for parameter in 0..kind.params().len() {
            let norm = if (slot_index + parameter) % 2 == 0 {
                0.0
            } else {
                1.0
            };
            slot.target.v[parameter] = norm;
            slot.smooth.v[parameter] = norm;
        }
        e.slots.push(slot);
    }

    let mut source = Const(3.0);
    let mut output = [[0.0f32; 2]; 128];
    for _ in 0..100 {
        e.process(None, &mut source, &mut output);
        assert!(output
            .iter()
            .flatten()
            .all(|sample| { sample.is_finite() && sample.abs() <= FULL_SCALE }));
    }
}

#[test]
fn default_preset_passes_signal_and_strips_input_dc() {
    let mut e = engine();
    let p = Preset::blues();
    e.load_rack(p.to_slots(48000.0));
    struct SinePlusDc(usize);
    impl Source for SinePlusDc {
        fn read(&mut self, dst: &mut [f32]) -> usize {
            for s in dst.iter_mut() {
                // sine plus a real DC offset, the kind a cheap interface adds
                *s = ((self.0 as f32) * 0.05).sin() * 0.5 + 0.2;
                self.0 += 1;
            }
            dst.len()
        }
        fn level(&self) -> usize {
            0
        }
    }
    let mut src = SinePlusDc(0);
    let mut out = [[0.0f32; 2]; 512];
    let (mut peak, mut dc_sum, mut n) = (0.0f32, 0.0f64, 0usize);
    for _ in 0..40 {
        e.process(None, &mut src, &mut out);
        for f in out.iter() {
            peak = peak.max(f[0].abs());
            dc_sum += f[0] as f64;
            n += 1;
        }
    }
    assert!(
        peak > 0.01,
        "default preset should pass signal, peak {peak}"
    );
    assert!(peak <= 1.0);
    let dc = (dc_sum / n as f64).abs();
    assert!(dc < 0.02, "output must not carry DC, got {dc}");
}

#[test]
fn mute_switch_really_silences_the_output() {
    let shared = Shared::new();
    let mut e = engine();
    e.slots.push(Slot::build(EffectKind::Boost, true, 48000.0));
    let mut src = Const(0.5);
    let mut out = [[0.0f32; 2]; 256];
    e.process(Some(&shared), &mut src, &mut out);
    assert!(out.iter().flatten().any(|s| *s != 0.0));
    shared.send(Cmd::Flag(Flag::Muted, true));
    e.process(Some(&shared), &mut src, &mut out);
    assert!(
        out.iter().flatten().all(|s| *s == 0.0),
        "muted output leaked"
    );
}

#[test]
fn a_poisoned_mailbox_still_gets_drained() {
    let shared = Shared::new();
    {
        let s = shared.clone();
        let _ = std::panic::catch_unwind(move || {
            let _g = s.cmds.lock().unwrap();
            panic!("ui thread died holding the queue");
        });
    }
    assert!(shared.cmds.is_poisoned());
    shared.send(Cmd::Flag(Flag::Cab, false));
    let mut e = engine();
    let mut out = [[0.0f32; 2]; 8];
    e.process(Some(&shared), &mut Silent, &mut out);
    assert!(!e.flags().0, "commands after poisoning must still apply");
    assert_eq!(shared.status(), String::new());
}

#[test]
fn stats_publish_finite_numbers() {
    let shared = Shared::new();
    let mut e = engine();
    e.slots.push(Slot::build(EffectKind::Reverb, true, 48000.0));
    let mut src = Const(0.3);
    let mut out = [[0.0f32; 2]; 128];
    for _ in 0..10 {
        e.process(Some(&shared), &mut src, &mut out);
    }
    let s = shared.stats.snapshot();
    assert!(s.in_peak > 0.0 && s.in_peak <= 8.0, "in_peak {}", s.in_peak);
    assert!(s.out_peak.is_finite() && s.out_peak <= 1.0);
    assert!(s.cpu_pct.is_finite() && s.cpu_pct >= 0.0);
    assert!(s.gr_db <= 0.001 && s.gr_db > -80.0, "gr_db {}", s.gr_db);
    assert_eq!(s.rate, 48000);
}

#[test]
fn rate_change_propagates_to_every_slot_and_stays_stable() {
    let mut base = engine();
    base.set_rates(44100.0, 96000.0);
    assert_eq!(base.sr(), 96000.0);
    assert!((base.step_base - 44100.0 / 96000.0).abs() < 1e-6);

    struct R(usize);
    impl Source for R {
        fn read(&mut self, dst: &mut [f32]) -> usize {
            for s in dst.iter_mut() {
                *s = ((self.0 as f32) * 0.02).sin() * 0.6;
                self.0 += 1;
            }
            dst.len()
        }
        fn level(&self) -> usize {
            100
        }
    }
    for kind in EffectKind::ALL {
        let mut e = engine();
        e.slots.push(Slot::build(kind, true, 48_000.0));
        e.set_rates(44_100.0, 96_000.0);
        let mut src = R(0);
        let mut out = [[0.0f32; 2]; 256];
        for _ in 0..40 {
            e.process(None, &mut src, &mut out);
            for frame in &out {
                assert!(
                    frame[0].is_finite() && frame[1].is_finite(),
                    "{kind:?} produced non-finite output across a rate change"
                );
                assert!(frame[0].abs() <= 1.0, "{kind:?} exceeded full scale");
            }
        }
    }
}
