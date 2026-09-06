use egui::{Pos2, Rect};

use super::*;

fn rack(kinds: &[EffectKind]) -> Preset {
    let mut preset = Preset::empty();
    preset.slots = kinds.iter().map(|kind| SlotPreset::new(*kind)).collect();
    preset
}

#[test]
fn saving_keeps_each_pedal_parameter_normalized() {
    for kind in EffectKind::ALL {
        let mut preset = rack(&[kind]);
        preset.slots[0].params = kind
            .params()
            .iter()
            .enumerate()
            .map(|(param_index, _)| ((param_index + 1) as f32 / 12.0).min(0.97))
            .collect();

        let saved = App::for_test(preset.clone()).build_preset();

        assert_eq!(saved.slots.len(), 1, "{kind:?}");
        assert_eq!(saved.slots[0].params, preset.slots[0].params, "{kind:?}");
    }
}

#[test]
fn bypass_updates_mirror_and_reaches_the_offline_engine() {
    struct Constant;
    impl crate::dsp::resampler::Source for Constant {
        fn read(&mut self, dst: &mut [f32]) -> usize {
            dst.fill(0.1);
            dst.len()
        }
        fn level(&self) -> usize {
            0
        }
    }

    let mut app = App::for_test(rack(&[EffectKind::Boost]));
    let id = app.slots[0].id;
    let mut engine = Engine::new(48_000.0, 128);
    engine.load_rack(app.build_preset().to_slots(48_000.0));
    app.apply_rack_edits(vec![RackEdit::SetEnabled { id, on: false }]);
    let mut source = Constant;
    let mut output = [[0.0; 2]; 128];
    engine.process(Some(&app.shared), &mut source, &mut output);

    assert!(!app.slots[0].enabled);
    assert!(
        !engine.slots[0].enabled,
        "queued bypass did not reach the engine"
    );
}

#[test]
fn replacement_reaches_engine_and_resets_params_without_rearming_bypass() {
    struct Silent;
    impl crate::dsp::resampler::Source for Silent {
        fn read(&mut self, dst: &mut [f32]) -> usize {
            dst.fill(0.0);
            dst.len()
        }
        fn level(&self) -> usize {
            0
        }
    }

    for kind in [
        EffectKind::StepFilter,
        EffectKind::RingModulator,
        EffectKind::AnalogDelay,
    ] {
        let mut preset = rack(&[EffectKind::Gate]);
        preset.slots[0].enabled = false;
        preset.slots[0].params = vec![0.1, 0.9];
        let mut app = App::for_test(preset);
        let mut engine = Engine::new(48_000.0, 128);
        engine.load_rack(app.build_preset().to_slots(48_000.0));
        let id = app.slots[0].id;

        app.apply_rack_edits(vec![RackEdit::Replace { id, kind }]);
        engine.process(Some(&app.shared), &mut Silent, &mut [[0.0; 2]; 128]);

        let defaults = kind.default_norms();
        let saved = app.build_preset();
        assert_eq!(app.slots[0].id, id);
        assert_eq!(app.slots[0].kind, kind);
        assert!(!app.slots[0].enabled);
        assert_eq!(engine.slots[0].kind, kind);
        assert!(!engine.slots[0].enabled);
        assert_eq!(saved.slots[0].params, defaults.v[..kind.params().len()]);
    }
}

#[test]
fn deferred_structural_edits_keep_stable_ids_and_remove_insert_order() {
    let mut app = App::for_test(rack(&[
        EffectKind::Gate,
        EffectKind::Boost,
        EffectKind::Delay,
    ]));
    let ids: Vec<u64> = app.slots.iter().map(|slot| slot.id).collect();
    app.apply_rack_edits(vec![
        RackEdit::Move { id: ids[0], to: 2 },
        RackEdit::Move { id: ids[2], to: 0 },
        RackEdit::Remove { id: ids[1] },
    ]);

    assert_eq!(
        app.slots.iter().map(|slot| slot.id).collect::<Vec<_>>(),
        vec![ids[2], ids[0]]
    );
    assert_eq!(
        app.slots.iter().map(|slot| slot.kind).collect::<Vec<_>>(),
        vec![EffectKind::Delay, EffectKind::Gate]
    );
}

#[test]
fn failed_setup_cannot_be_armed_but_can_always_be_disarmed() {
    let mut app = App::for_test(Preset::empty());
    app.draft.input = Some("unavailable interface".into());
    app.apply_draft();
    app.shared.reopen.lock().unwrap().take(); // Owner consumed a setup that failed.
    app.arm_input(true);
    assert!(!app.draft.input_on);
    assert!(app.shared.reopen.lock().unwrap().is_none());
    assert!(app.notice.contains("has not applied"));

    // An explicitly armed CLI startup must still be cancellable before a successful open.
    app.last_requested.input_on = true;
    app.draft.input_on = true;
    app.arm_input(false);
    assert!(!app.draft.input_on);
    assert!(!app.shared.reopen.lock().unwrap().as_ref().unwrap().input_on);
}

#[test]
fn arming_uses_applied_devices_not_unapplied_draft() {
    let mut app = App::for_test(Preset::empty());
    app.draft.input = Some("draft input".into());
    app.draft.output = Some("draft output".into());
    *app.shared.applied.lock().unwrap() = Some(ReopenReq {
        input: Some("applied input".into()),
        output: Some("applied output".into()),
        buffer_ms: 10,
        input_on: false,
    });

    app.arm_input(true);

    let request = app.shared.reopen.lock().unwrap().clone().unwrap();
    assert_eq!(request.input.as_deref(), Some("applied input"));
    assert_eq!(request.output.as_deref(), Some("applied output"));
    assert!(request.input_on);
}

#[test]
fn pedal_cards_keep_their_width_and_wrap_into_rows() {
    let context = egui::Context::default();
    let slots = mirror(&Preset::blues());
    let mut rects = Vec::new();
    for _ in 0..3 {
        rects.clear();
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 600.0))),
                ..Default::default()
            },
            |ui| {
                ui.with_layout(
                    egui::Layout::left_to_right(egui::Align::Min).with_main_wrap(true),
                    |ui| {
                        for (position, slot) in slots.iter().enumerate() {
                            let (_, rect, _) = App::slot_card(ui, slot, position, &mut None);
                            rects.push(rect);
                        }
                    },
                );
            },
        );
        output.textures_delta.clear();
    }
    assert_eq!(rects.len(), 4);
    assert!(
        rects.iter().all(|r| r.width() <= 280.0 && r.max.x <= 600.0),
        "cards overflow: {rects:?}"
    );
    assert!(
        rects[2].min.y >= rects[0].max.y,
        "third card must wrap: {rects:?}"
    );
}

#[test]
fn headless_egui_draw_handles_empty_and_full_racks() {
    for size in [[880.0, 560.0], [1180.0, 780.0], [1600.0, 1000.0]] {
        for preset in [
            Preset::empty(),
            Preset::blues(),
            rack(&EffectKind::ALL[..MAX_SLOTS]),
            rack(&EffectKind::ALL[MAX_SLOTS..]),
        ] {
            let mut app = App::for_test(preset);
            app.preset_name =
                "a deliberately long preset name for clipping regression coverage".into();
            let context = egui::Context::default();
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::Vec2::from(size),
                    )),
                    ..Default::default()
                },
                |root| app.draw(root),
            );
            output.textures_delta.clear();
        }
    }
}

#[test]
fn ppm_writer_emits_rgb_header_and_pixels() {
    let path = std::env::temp_dir().join(format!("triode-ui-{}.ppm", std::process::id()));
    let image = egui::ColorImage::new([1, 1], vec![egui::Color32::from_rgb(1, 2, 3)]);
    write_ppm(&path, &image).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"P6\n1 1\n255\n\x01\x02\x03");
    let _ = std::fs::remove_file(path);
}
