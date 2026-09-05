//! The desktop UI.
//!
//! One rule holds this whole file together: **the UI never touches DSP state directly.**
//! Every edit becomes a [`Cmd`] through [`Shared::send`], and every read comes from
//! [`Shared::stats`] or the scope window. That is not purity for its own sake -- it is what
//! makes it safe to rebuild the rack, swap devices and drag pedals while a note is ringing:
//! the audio thread owns its state outright and only ever *drains a queue*. If the UI
//! reached into the engine, every knob move would be a lock the audio thread might have to
//! wait for, which is the classic way a UI kills your audio.
//!
//! So the UI keeps its own small mirror of the rack (`slots`, `amp`). The mirror is the
//! truth about what the knobs *look* like; the engine is the truth about what they *do*.
//! They can only diverge if the engine rejects a command (a 13th pedal), and both sides
//! enforce the same `MAX_SLOTS`, so they cannot.

mod widgets;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui::{Color32, InnerResponse, Rect, Sense, Stroke, Ui};

use crate::audio_io::Audio;
use crate::dsp::cab::Ir;
use crate::engine::{Cmd, Engine, Flag, ReopenReq, Shared};
use crate::params::{EffectKind, AMP_SPECS, MAX_PARAMS, MAX_SLOTS};
use crate::preset::{Preset, SlotPreset};

/// One pedal as the UI believes it to be.
struct SlotUi {
    kind: EffectKind,
    enabled: bool,
    norms: [f32; MAX_PARAMS],
}

pub struct App {
    shared: Arc<Shared>,
    /// Held so the cpal streams live exactly as long as the window.
    _audio: Option<Audio>,
    slots: Vec<SlotUi>,
    amp: Vec<f32>,
    cab: bool,
    hpf: bool,
    muted: bool,

    input: String,
    output: String,
    input_on: bool,
    inputs: Vec<String>,
    outputs: Vec<String>,
    buffer_ms: u32,
    req: ReopenReq,

    // wave view
    freeze: bool,
    scope_ms: f32,
    tone_hz: f32,
    snap: Option<(Vec<f32>, Vec<f32>, f32)>,
    seq: u64,

    // rack + preset editing
    add_kind: EffectKind,
    drag: Option<usize>,
    ir_path: String,
    preset_name: String,
    note: String,
}

fn norms_of(kind: EffectKind) -> [f32; MAX_PARAMS] {
    kind.default_norms().v
}

fn mirror(preset: &Preset) -> Vec<SlotUi> {
    preset
        .slots
        .iter()
        .map(|s| {
            let mut norms = [0.0f32; MAX_PARAMS];
            for (i, v) in s.norms().iter().enumerate() {
                if i < MAX_PARAMS {
                    norms[i] = *v;
                }
            }
            SlotUi {
                kind: s.kind,
                enabled: s.enabled,
                norms,
            }
        })
        .collect()
}

impl App {
    /// Open the window. Blocks until it closes.
    pub fn launch(mut preset: Preset, req: ReopenReq) -> Result<(), String> {
        preset.sanitize();
        let shared = Shared::new();
        // Placeholder rates and chunk: `audio_io::open` re-configures the engine for the
        // real devices before its first callback can fire, so these only need to be sane.
        let mut engine = Engine::new(44100.0, 128);
        engine.load_rack(preset.to_slots(44100.0));
        for (i, n) in preset.amp_norms().iter().enumerate() {
            engine.set_amp_param(i, *n);
        }
        shared.send(Cmd::Flag(Flag::Cab, preset.cab));
        shared.send(Cmd::Flag(Flag::InputHpf, preset.hpf));

        // No audio is not a reason to have no window: you can still edit and save presets,
        // and read what went wrong, so report it instead of exiting.
        let (audio, note) =
            match Audio::spawn(shared.clone(), Arc::new(Mutex::new(engine)), req.clone()) {
                Ok(a) => (Some(a), String::new()),
                Err(e) => (None, e),
            };

        let (inputs, outputs) = crate::audio_io::devices();
        let app = App {
            shared,
            _audio: audio,
            slots: mirror(&preset),
            amp: preset.amp_norms(),
            cab: preset.cab,
            hpf: preset.hpf,
            muted: false,
            input: req.input.clone().unwrap_or_default(),
            input_on: req.input_on,
            output: req.output.clone().unwrap_or_default(),
            inputs,
            outputs,
            buffer_ms: req.buffer_ms,
            req,
            freeze: false,
            scope_ms: 20.0,
            tone_hz: 0.0,
            snap: None,
            seq: 0,
            add_kind: EffectKind::ALL[0],
            drag: None,
            ir_path: String::new(),
            preset_name: preset.name.clone(),
            note,
        };
        let title = format!("Triode — {}", preset.name);
        let opts = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1180.0, 780.0])
                .with_min_inner_size([880.0, 560.0])
                .with_title(title),
            ..Default::default()
        };
        eframe::run_native(
            "Triode",
            opts,
            Box::new(move |cc| {
                cc.egui_ctx.set_visuals(egui::Visuals::dark());
                Ok(Box::new(app))
            }),
        )
        .map_err(|e| format!("the window could not be opened: {e}"))
    }

    fn send(&self, cmd: Cmd) {
        self.shared.send(cmd);
    }

    /// Ask the audio thread to rebuild its streams for the current device/buffer choice.
    fn request_reopen(&mut self) {
        self.req.input = (!self.input.is_empty()).then(|| self.input.clone());
        self.req.output = (!self.output.is_empty()).then(|| self.output.clone());
        self.req.buffer_ms = self.buffer_ms;
        self.req.input_on = self.input_on;
        if let Ok(mut r) = self.shared.reopen.lock() {
            *r = Some(self.req.clone());
        }
    }

    /// Snapshot the UI mirror back into a [`Preset`].
    fn build_preset(&self) -> Preset {
        let mut p = Preset::empty();
        p.name = self.preset_name.clone();
        p.amp = self.amp.clone();
        p.cab = self.cab;
        p.hpf = self.hpf;
        p.slots = self
            .slots
            .iter()
            .map(|s| {
                let values: Vec<f32> = s
                    .kind
                    .params()
                    .iter()
                    .enumerate()
                    .map(|(i, sp)| sp.denorm(s.norms[i]))
                    .collect();
                SlotPreset {
                    kind: s.kind,
                    enabled: s.enabled,
                    params: values,
                }
            })
            .collect();
        p
    }

    /// Push a preset to the engine and mirror it locally. The slots are built here, on the
    /// UI thread, because a reverb's comb buffers are a few hundred KB and the audio thread
    /// must not allocate.
    fn apply_preset(&mut self, p: &Preset) {
        let mut p2 = p.clone();
        p2.sanitize();
        // Before the audio thread has opened a device the published rate is 0, which would
        // build every delay line at 1 Hz. Fall back to the rate the engine starts on.
        let rate = self.shared.stats.snapshot().rate;
        let sr = if rate == 0 { 44100.0 } else { rate as f32 };
        self.send(Cmd::LoadRack(p2.to_slots(sr)));
        for (i, n) in p2.amp_norms().iter().enumerate() {
            self.send(Cmd::AmpParam { idx: i, norm: *n });
        }
        self.send(Cmd::Flag(Flag::Cab, p2.cab));
        self.send(Cmd::Flag(Flag::InputHpf, p2.hpf));
        self.amp = p2.amp_norms();
        self.cab = p2.cab;
        self.hpf = p2.hpf;
        self.slots = mirror(&p2);
    }
}

impl eframe::App for App {
    /// eframe 0.36 hands the app the root `Ui` rather than a `Context`: the two panels
    /// claim their edges, and whatever is left over is the rack.
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Continuous repaint: the meters and scope are live, and an immediate-mode UI only
        // redraws when asked. 30 Hz is past smooth for a level bar and costs nothing.
        root.ctx().request_repaint_after(Duration::from_millis(33));
        self.top_bar(root);
        self.wave_bar(root);
        self.devices_row(root);
        self.rack(root);
    }
}

impl App {
    fn top_bar(&mut self, root: &mut egui::Ui) {
        let snap = self.shared.stats.snapshot();
        let status = self.shared.status();
        let mut reset = false;
        egui::containers::Panel::top("bar").show(root, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Triode");
                ui.separator();
                let msg = if !self.note.is_empty() {
                    self.note.clone()
                } else if !status.is_empty() {
                    status
                } else {
                    "ready".to_string()
                };
                ui.label(egui::RichText::new(msg).color(Color32::from_gray(180)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("reset meters").clicked() {
                        reset = true;
                    }
                    ui.label(
                        egui::RichText::new(format!(
                            "cpu {:.0}%  {} Hz  drift {:+.0} ppm  clip in/out {}/{}  xruns {}  dropped {}",
                            snap.cpu_pct,
                            snap.rate,
                            (snap.ratio - 1.0) * 1e6,
                            snap.clip_in,
                            snap.clip_out,
                            snap.overruns,
                            snap.dropped_in,
                        ))
                        .monospace()
                        .size(11.0),
                    );
                });
            });
        });
        if reset {
            self.shared.stats.reset_clips();
        }
    }

    fn devices_row(&mut self, ui: &mut Ui) {
        let mut reopen = false;
        ui.horizontal(|ui| {
            let mut input = self.input.clone();
            let mut output = self.output.clone();
            let mut ms = self.buffer_ms;
            ui.label(
                egui::RichText::new("in")
                    .size(11.0)
                    .color(widgets::MUTED),
            );
            egui::ComboBox::from_id_salt("dev_in")
                .selected_text(if input.is_empty() { "default" } else { &input })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut input, String::new(), "default");
                    for d in &self.inputs {
                        ui.selectable_value(&mut input, d.clone(), d);
                    }
                });
            ui.label(
                egui::RichText::new("out")
                    .size(11.0)
                    .color(widgets::MUTED),
            );
            egui::ComboBox::from_id_salt("dev_out")
                .selected_text(if output.is_empty() {
                    "default"
                } else {
                    &output
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut output, String::new(), "default");
                    for d in &self.outputs {
                        ui.selectable_value(&mut output, d.clone(), d);
                    }
                });
            ui.label(
                egui::RichText::new("buffer")
                    .size(11.0)
                    .color(widgets::MUTED),
            );
            egui::ComboBox::from_id_salt("bufms")
                .selected_text(format!("{ms} ms"))
                .show_ui(ui, |ui| {
                    for m in [1u32, 2, 5, 10, 20, 50] {
                        ui.selectable_value(&mut ms, m, format!("{m} ms"));
                    }
                });
            // Arm/disarm is a switch, not a device change, so it takes effect at once.
            let mut on = self.input_on;
            ui.checkbox(&mut on, "input on").on_hover_text(
                "capture from the input device. Off means output only, so these speakers \
                 cannot howl through the laptop mic; the test tone and wave view still work",
            );
            if on != self.input_on {
                self.input_on = on;
                reopen = true;
            }
            let changed = input != self.input || output != self.output || ms != self.buffer_ms;
            if changed
                && ui
                    .button("apply")
                    .on_hover_text("rebuild the audio streams")
                    .clicked()
            {
                self.input = input;
                self.output = output;
                self.buffer_ms = ms;
                reopen = true;
            }
            if changed {
                ui.label(
                    egui::RichText::new("unapplied")
                        .size(11.0)
                        .color(Color32::from_rgb(255, 200, 90)),
                );
            }
        });
        if reopen {
            self.request_reopen();
        }
    }

    fn wave_bar(&mut self, root: &mut egui::Ui) {
        // Fresh window from the engine unless frozen; frozen keeps the last snapshot, which
        // is the whole point -- stop a nasty transient and look at it.
        if !self.freeze {
            if let Some((din, dout, sr, seq)) = self.shared.scope_snap() {
                if seq != self.seq {
                    self.seq = seq;
                    self.snap = Some((din, dout, sr));
                }
            }
        }
        let tone = self.tone_hz;
        let mut freeze = self.freeze;
        let mut ms = self.scope_ms;
        let mut new_tone: Option<f32> = None;
        egui::containers::Panel::bottom("wave").show(root, |ui| {
            ui.horizontal(|ui| {
                if ui.checkbox(&mut freeze, "freeze").changed() {
                    self.shared.set_scope(!freeze);
                }
                ui.add(
                    egui::Slider::new(&mut ms, 2.0..=200.0)
                        .text("window")
                        .suffix(" ms"),
                );
                let mut t = tone;
                let mut on = tone > 0.0;
                if ui.checkbox(&mut on, "test tone").changed() {
                    new_tone = Some(if on { 110.0 } else { 0.0 });
                }
                if on
                    && ui
                        .add(
                            egui::Slider::new(&mut t, 20.0..=2000.0)
                                .suffix(" Hz")
                                .logarithmic(true),
                        )
                        .changed()
                {
                    new_tone = Some(t);
                }
                // Explanation belongs on hover, not as a sentence of 11px gray wedged into
                // the middle of the controls: it pushed the real controls around and read as
                // noise once you knew what it said.
                ui.label("ⓘ").on_hover_text(
                    "The transfer plot is the honest one: a clipped sine still looks like a \
                     sine on the scope, but here any stage that limits or squares off peels \
                     off the 1:1 diagonal and goes flat.",
                );
                if !self.input_on {
                    // State has to be visible where the silence is, not only on a checkbox
                    // three widgets away.
                    ui.label(
                        egui::RichText::new("input off")
                            .color(Color32::from_rgb(200, 160, 90))
                            .strong(),
                    );
                }
            });
            if let Some(v) = new_tone {
                self.shared.set_tone(v);
                self.tone_hz = v;
            }
            self.scope_ms = ms;
            self.freeze = freeze;
            ui.separator();
            let snap = self.snap.clone();
            ui.horizontal(|ui| match snap {
                None => {
                    // Say what is actually true. This used to tell everyone to "play into the
                    // input", which since input-starts-disarmed is advice that cannot work.
                    let why = if self.input_on {
                        "no audio yet — play into the input, or switch on the test tone"
                    } else {
                        "input is off — tick `input on` in the header to arm it, or switch on \
                         the test tone to see the amp working"
                    };
                    ui.centered_and_justified(|ui| ui.label(why));
                }
                Some((din, dout, sr)) => {
                    widgets::scope(ui, &din, &dout, sr, self.scope_ms);
                    ui.separator();
                    widgets::transfer(ui, &din, &dout);
                    ui.separator();
                    self.levels(ui);
                    // Presets are a different job from watching the signal, and they were
                    // taking width from the two plots this view exists to show. Collapsed by
                    // default: the bar belongs to the scope and the transfer plot.
                    ui.separator();
                    egui::CollapsingHeader::new("presets").show(ui, |ui| {
                        self.preset_col(ui);
                    });
                }
            });
        });
    }

    fn levels(&mut self, ui: &mut Ui) {
        let snap = self.shared.stats.snapshot();
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new("input")
                    .size(11.0)
                    .color(widgets::MUTED),
            );
            widgets::meter(ui, snap.in_rms, snap.in_peak, snap.clip_in > 0);
            ui.label(
                egui::RichText::new("output")
                    .size(11.0)
                    .color(widgets::MUTED),
            );
            widgets::meter(ui, snap.out_rms, snap.out_peak, snap.clip_out > 0);
            ui.label(
                egui::RichText::new(format!("gate/comp GR {:+.1} dB", snap.gr_db))
                    .size(11.0)
                    .color(widgets::MUTED),
            );
        });
    }

    fn preset_col(&mut self, ui: &mut Ui) {
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new("preset")
                    .size(11.0)
                    .color(widgets::MUTED),
            );
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.preset_name).desired_width(120.0));
                if ui.button("save").clicked() {
                    let dir = crate::preset::preset_dir();
                    match self.build_preset().save(&dir) {
                        Ok(path) => self.note = format!("saved {}", path.display()),
                        Err(e) => self.note = format!("save failed: {e}"),
                    }
                }
            });
            let dir = crate::preset::preset_dir();
            let names = Preset::list(&dir);
            let mut load: Option<String> = None;
            egui::ComboBox::from_id_salt("preset_load")
                .selected_text("load…")
                .show_ui(ui, |ui| {
                    if names.is_empty() {
                        ui.label("(none saved yet)");
                    }
                    for n in &names {
                        if ui.selectable_label(false, n).clicked() {
                            load = Some(n.clone());
                        }
                    }
                });
            if let Some(n) = load {
                match Preset::load(&dir, &n) {
                    Ok(p) => {
                        self.apply_preset(&p);
                        self.preset_name = p.name;
                        self.note = format!("loaded {n}");
                    }
                    Err(e) => self.note = format!("load failed: {e}"),
                }
            }
        });
    }

    fn rack(&mut self, ui: &mut Ui) {
        let full = self.slots.len() >= MAX_SLOTS;
        widgets::section(ui, "amp + rack");
        egui::ScrollArea::horizontal().show(ui, |ui| {
            ui.horizontal_top(|ui| {
                self.amp_card(ui);
                let mut moved: Option<(usize, usize)> = None;
                let mut rects: Vec<(usize, Rect)> = Vec::new();
                // Each card records its rect as it is drawn; the drop is then resolved from
                // the pointer position at the moment the drag is released. (egui captures a
                // drag by its source widget, so the target has to be found by hit-test.)
                for i in 0..self.slots.len() {
                    self.slot_card(ui, i, &mut rects, &mut moved);
                }
                if let Some((from, to)) = moved {
                    self.send(Cmd::MoveSlot { from, to });
                    self.slots.swap(from, to);
                }
                self.add_card(ui, full);
            });
        });
    }

    fn amp_card(&mut self, ui: &mut Ui) {
        let mut sent: Vec<(usize, f32)> = Vec::new();
        let mut load_ir = false;
        let mut clear_ir = false;
        let mut cab = self.cab;
        let mut hpf = self.hpf;
        let mut muted = self.muted;
        let mut amp = self.amp.clone();
        let mut ir_path = self.ir_path.clone();

        let _: InnerResponse<()> = ui.group(|ui| {
            ui.set_width(430.0);
            ui.horizontal(|ui| {
                ui.heading("Amp");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.checkbox(&mut muted, "mute");
                    ui.checkbox(&mut cab, "cab");
                    ui.checkbox(&mut hpf, "hp filter");
                });
            });
            widgets::knob_row(ui, &AMP_SPECS, &mut amp, |i, v| sent.push((i, v)));
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("cab IR")
                        .size(11.0)
                        .color(widgets::MUTED),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut ir_path)
                        .hint_text("impulse.wav")
                        .desired_width(190.0),
                );
                if ui.button("load").clicked() {
                    load_ir = true;
                }
                if ui.button("built-in").clicked() {
                    clear_ir = true;
                }
            });
        });

        for (i, v) in sent {
            self.send(Cmd::AmpParam { idx: i, norm: v });
        }
        self.amp = amp;
        for (on, flag) in [
            (cab, Flag::Cab),
            (hpf, Flag::InputHpf),
            (muted, Flag::Muted),
        ] {
            let was = match flag {
                Flag::Cab => self.cab,
                Flag::InputHpf => self.hpf,
                Flag::Muted => self.muted,
            };
            if on != was {
                self.send(Cmd::Flag(flag, on));
                match flag {
                    Flag::Cab => self.cab = on,
                    Flag::InputHpf => self.hpf = on,
                    Flag::Muted => self.muted = on,
                }
            }
        }
        if load_ir {
            let path = self.ir_path.clone();
            match Ir::from_wav(&path) {
                Ok(ir) => {
                    let n = ir.name().to_string();
                    self.send(Cmd::LoadIr(Box::new(ir)));
                    self.note = format!("loaded IR {n}");
                }
                Err(e) => self.note = format!("IR: {e}"),
            }
        }
        if clear_ir {
            self.send(Cmd::ClearIr);
            self.note = "cab IR: built-in model".into();
        }
    }

    fn slot_card(
        &mut self,
        ui: &mut Ui,
        i: usize,
        rects: &mut Vec<(usize, Rect)>,
        moved: &mut Option<(usize, usize)>,
    ) {
        let specs = self.slots[i].kind.params();
        let dragging = self.drag.is_some();
        let mut rect = Rect::NOTHING;
        let mut remove = false;
        let mut new_kind: Option<EffectKind> = None;
        let mut left = false;
        let mut right = false;

        let mut slot = SlotUi {
            kind: self.slots[i].kind,
            enabled: self.slots[i].enabled,
            norms: self.slots[i].norms,
        };
        let mut sent: Vec<(usize, f32)> = Vec::new();

        ui.group(|ui| {
            ui.set_width(320.0);
            ui.horizontal(|ui| {
                if widgets::led(ui, slot.enabled, "").clicked() {
                    slot.enabled = !slot.enabled;
                }
                ui.heading(slot.kind.short());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("✕").on_hover_text("remove").clicked() {
                        remove = true;
                    }
                    if ui.button("▶").on_hover_text("move right").clicked() {
                        right = true;
                    }
                    if ui.button("◀").on_hover_text("move left").clicked() {
                        left = true;
                    }
                    egui::ComboBox::from_id_salt(("kind", i))
                        .selected_text(slot.kind.name())
                        .show_ui(ui, |ui| {
                            for k in EffectKind::ALL {
                                if ui.selectable_value(&mut slot.kind, k, k.name()).changed() {
                                    new_kind = Some(k);
                                }
                            }
                        });
                });
            });
            ui.separator();
            // Switching kind changes the knob set, so only draw the knobs the current kind
            // actually has -- an 8-slot array is storage, not a promise to show 8 knobs.
            let n = specs.len().min(MAX_PARAMS);
            let mut norms = slot.norms;
            widgets::knob_row(ui, &specs[..n], &mut norms[..n], |idx, v| {
                sent.push((idx, v))
            });
            slot.norms = norms;
            rect = ui.min_rect();
        });

        self.send_pending(i, &mut slot, sent);

        // The drag handle is an explicit overlay: the group itself has no drag sense, and a
        // card is only a drag *source* -- the target is whichever card the pointer is over
        // when the button comes up.
        if rect.is_positive() {
            let id = ui.id().with(("drag", i));
            let resp = ui.interact(rect, id, Sense::drag());
            if resp.drag_started() {
                self.drag = Some(i);
            }
            if dragging && self.drag == Some(i) {
                ui.painter().rect_stroke(
                    rect,
                    4.0,
                    Stroke::new(2.0, Color32::from_rgb(120, 200, 255)),
                    egui::StrokeKind::Inside,
                );
            }
            if resp.drag_stopped() {
                if let Some(from) = self.drag.take() {
                    let pos = resp.ctx.input(|r| r.pointer.interact_pos());
                    if let Some(pos) = pos {
                        if let Some(to) =
                            rects.iter().find(|(_, r)| r.contains(pos)).map(|(t, _)| *t)
                        {
                            if to != from {
                                *moved = Some((from, to));
                            }
                        }
                    }
                }
            }
            rects.push((i, rect));
        }

        if left && i > 0 {
            self.send(Cmd::MoveSlot { from: i, to: i - 1 });
            self.slots.swap(i, i - 1);
        }
        if right && i + 1 < self.slots.len() {
            self.send(Cmd::MoveSlot { from: i, to: i + 1 });
            self.slots.swap(i, i + 1);
        }
        if let Some(kind) = new_kind {
            // Fresh DSP state on purpose: a delay line from the old pedal has nothing to do
            // with the one that replaces it.
            self.send(Cmd::ReplaceSlot {
                slot: i,
                with: Box::new(crate::engine::Slot::new(kind, slot.enabled)),
            });
            slot.kind = kind;
            slot.norms = norms_of(kind);
        }
        self.slots[i] = slot;
        if remove {
            self.send(Cmd::RemoveSlot { slot: i });
            self.slots.remove(i);
            self.drag = None;
        }
    }

    /// Forward this card's knob and bypass edits, after the draw pass so nothing is sent for
    /// a widget that only moved visually.
    fn send_pending(&mut self, i: usize, slot: &mut SlotUi, sent: Vec<(usize, f32)>) {
        for (idx, v) in sent.into_iter() {
            slot.norms[idx] = v;
            self.send(Cmd::SetParam {
                slot: i,
                idx,
                norm: v,
            });
        }
    }

    fn add_card(&mut self, ui: &mut Ui, full: bool) {
        let mut add = false;
        ui.group(|ui| {
            ui.set_width(230.0);
            ui.heading("Add");
            egui::ComboBox::from_id_salt("add_kind")
                .selected_text(self.add_kind.name())
                .show_ui(ui, |ui| {
                    for k in EffectKind::ALL {
                        ui.selectable_value(&mut self.add_kind, k, k.name());
                    }
                });
            // Disabled-state is a getter in this egui, so gate the action rather than the
            // widget; the limit label below explains a refused click.
            if ui.button("add to end").clicked() && !full {
                add = true;
            }
            if full {
                ui.label(egui::RichText::new(format!("{MAX_SLOTS} slots is the limit")).size(11.0));
            }
            ui.separator();
            ui.label(
                egui::RichText::new(
                    "drag a card onto another to reorder · click its LED to bypass",
                )
                .size(11.0)
                .color(widgets::MUTED),
            );
        });
        if add {
            let kind = self.add_kind;
            self.send(Cmd::InsertSlot {
                at: self.slots.len(),
                slot: Box::new(crate::engine::Slot::new(kind, true)),
            });
            self.slots.push(SlotUi {
                kind,
                enabled: true,
                norms: norms_of(kind),
            });
        }
    }
}
