//! The desktop UI. The audio thread owns DSP; this module only mirrors controls and sends
//! complete commands through [`Shared`].

mod widgets;

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Pos2, Rect, Sense, Ui, Vec2};

use crate::audio_io::Audio;
use crate::dsp::cab::Ir;
use crate::engine::{Cmd, Engine, Flag, ReopenReq, Shared, Slot};
use crate::params::{EffectKind, AMP_SPECS, MAX_PARAMS, MAX_SLOTS};
use crate::preset::{Preset, SlotPreset};

#[derive(Clone)]
struct SlotUi {
    id: u64,
    kind: EffectKind,
    enabled: bool,
    norms: [f32; MAX_PARAMS],
}

enum RackEdit {
    SetEnabled { id: u64, on: bool },
    SetParam { id: u64, idx: usize, norm: f32 },
    Remove { id: u64 },
    Move { id: u64, to: usize },
    Replace { id: u64, kind: EffectKind },
    Insert { kind: EffectKind },
}

struct Preview {
    path: PathBuf,
    started: Instant,
    result: Arc<Mutex<Option<Result<(), String>>>>,
}

pub struct App {
    shared: Arc<Shared>,
    /// Held so the cpal streams live exactly as long as the window.
    _audio: Option<Audio>,
    slots: Vec<SlotUi>,
    next_slot_id: u64,
    amp: Vec<f32>,
    cab: bool,
    hpf: bool,
    muted: bool,

    /// Requested settings survive redraws; only the audio owner records `Shared::applied`.
    draft: ReopenReq,
    last_requested: ReopenReq,
    inputs: Vec<String>,
    outputs: Vec<String>,

    freeze: bool,
    scope_ms: f32,
    tone_hz: f32,
    snap: Option<(Vec<f32>, Vec<f32>, f32)>,
    seq: u64,

    add_kind: EffectKind,
    drag: Option<u64>,
    ir_path: String,
    custom_ir: Option<String>,
    preset_name: String,
    modified: bool,
    notice: String,
    error: Option<String>,
    preview: Option<Preview>,
}

fn norms_of(kind: EffectKind) -> [f32; MAX_PARAMS] {
    kind.default_norms().v
}

fn mirror(preset: &Preset) -> Vec<SlotUi> {
    preset
        .slots
        .iter()
        .enumerate()
        .map(|(index, slot)| {
            let mut norms = [0.0; MAX_PARAMS];
            for (i, value) in slot.norms().iter().enumerate() {
                norms[i] = *value;
            }
            SlotUi {
                id: index as u64 + 1,
                kind: slot.kind,
                enabled: slot.enabled,
                norms,
            }
        })
        .collect()
}

fn rate(shared: &Shared) -> f32 {
    match shared.stats.snapshot().rate {
        0 => 44_100.0,
        rate => rate as f32,
    }
}

impl App {
    /// Open the native window. No input is armed implicitly.
    pub fn launch(mut preset: Preset, req: ReopenReq) -> Result<(), String> {
        preset.sanitize();
        let shared = Shared::new();
        let mut engine = Engine::new(44_100.0, 128);
        engine.load_rack(preset.to_slots(44_100.0));
        for (i, norm) in preset.amp_norms().iter().enumerate() {
            engine.set_amp_param(i, *norm);
        }
        shared.send(Cmd::Flag(Flag::Cab, preset.cab));
        shared.send(Cmd::Flag(Flag::InputHpf, preset.hpf));

        let (audio, error) =
            match Audio::spawn(shared.clone(), Arc::new(Mutex::new(engine)), req.clone()) {
                Ok(audio) => (Some(audio), None),
                Err(error) => (None, Some(error)),
            };
        let (inputs, outputs) = crate::audio_io::devices();
        let mut app = Self::with_parts(shared, preset, req, audio, inputs, outputs);
        app.error = error;

        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1180.0, 780.0])
                .with_min_inner_size([880.0, 560.0])
                .with_title("Triode"),
            ..Default::default()
        };
        eframe::run_native(
            "Triode",
            options,
            Box::new(move |cc| {
                cc.egui_ctx.set_visuals(widgets::visuals());
                Ok(Box::new(app))
            }),
        )
        .map_err(|error| format!("the window could not be opened: {error}"))
    }

    fn with_parts(
        shared: Arc<Shared>,
        mut preset: Preset,
        draft: ReopenReq,
        audio: Option<Audio>,
        inputs: Vec<String>,
        outputs: Vec<String>,
    ) -> Self {
        preset.sanitize();
        let slots = mirror(&preset);
        let next_slot_id = slots.len() as u64 + 1;
        Self {
            shared,
            _audio: audio,
            slots,
            next_slot_id,
            amp: preset.amp_norms(),
            cab: preset.cab,
            hpf: preset.hpf,
            muted: false,
            last_requested: draft.clone(),
            draft,
            inputs,
            outputs,
            freeze: false,
            scope_ms: 20.0,
            tone_hz: 0.0,
            snap: None,
            seq: 0,
            add_kind: EffectKind::ALL[0],
            drag: None,
            ir_path: String::new(),
            custom_ir: None,
            preset_name: preset.name,
            modified: false,
            notice: String::new(),
            error: None,
            preview: None,
        }
    }

    /// Render the native UI without opening audio devices, then write one PPM screenshot.
    pub fn preview(preset: Preset, path: PathBuf, size: [f32; 2]) -> Result<(), String> {
        let result = Arc::new(Mutex::new(None));
        let mut app = Self::with_parts(
            Shared::new(),
            preset,
            ReopenReq {
                input: None,
                output: None,
                buffer_ms: 5,
                input_on: false,
            },
            None,
            Vec::new(),
            Vec::new(),
        );
        app.preview = Some(Preview {
            path,
            started: Instant::now(),
            result: result.clone(),
        });
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size(size)
                .with_title("Triode preview"),
            ..Default::default()
        };
        let run = eframe::run_native(
            "Triode preview",
            options,
            Box::new(move |cc| {
                cc.egui_ctx.set_visuals(widgets::visuals());
                Ok(Box::new(app))
            }),
        );
        let outcome = result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        outcome.unwrap_or_else(|| {
            Err(run
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "preview closed without a screenshot".into()))
        })
    }

    #[cfg(test)]
    fn for_test(preset: Preset) -> Self {
        Self::with_parts(
            Shared::new(),
            preset,
            ReopenReq {
                input: None,
                output: None,
                buffer_ms: 5,
                input_on: false,
            },
            None,
            Vec::new(),
            Vec::new(),
        )
    }

    fn send(&self, cmd: Cmd) {
        self.shared.send(cmd);
    }

    fn applied_request(&self) -> Option<ReopenReq> {
        self.shared
            .applied
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn queue_reopen(&self, request: ReopenReq) {
        *self
            .shared
            .reopen
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(request);
    }

    fn apply_draft(&mut self) {
        self.last_requested = self.draft.clone();
        self.queue_reopen(self.last_requested.clone());
        self.notice = "audio setup requested".into();
    }

    /// Arming deliberately retains the last successful device/buffer choice instead of
    /// smuggling unrelated, unapplied device edits into a safety-critical request.
    fn arm_input(&mut self, on: bool) {
        let mut request = match self.applied_request() {
            Some(request) => request,
            None if on => {
                self.notice =
                    "audio setup has not applied yet; apply a working setup before arming".into();
                return;
            }
            None => self.last_requested.clone(),
        };
        self.draft.input_on = on;
        request.input_on = on;
        self.queue_reopen(request);
    }

    fn build_preset(&self) -> Preset {
        let mut preset = Preset::empty();
        preset.name = self.preset_name.clone();
        preset.amp = self.amp.clone();
        preset.cab = self.cab;
        preset.hpf = self.hpf;
        preset.slots = self
            .slots
            .iter()
            .map(|slot| SlotPreset {
                kind: slot.kind,
                enabled: slot.enabled,
                // SlotPreset is normalized. Denormalizing here corrupts every saved patch.
                params: slot.norms[..slot.kind.params().len()].to_vec(),
            })
            .collect();
        preset
    }

    fn apply_preset(&mut self, preset: &Preset) {
        let mut preset = preset.clone();
        preset.sanitize();
        self.send(Cmd::LoadRack(preset.to_slots(rate(&self.shared))));
        for (idx, norm) in preset.amp_norms().iter().enumerate() {
            self.send(Cmd::AmpParam { idx, norm: *norm });
        }
        self.send(Cmd::Flag(Flag::Cab, preset.cab));
        self.send(Cmd::Flag(Flag::InputHpf, preset.hpf));
        self.shared.clear_ir();
        self.slots = mirror(&preset);
        self.next_slot_id = self.slots.len() as u64 + 1;
        self.amp = preset.amp_norms();
        self.cab = preset.cab;
        self.hpf = preset.hpf;
        self.preset_name = preset.name;
        self.custom_ir = None;
        self.modified = false;
        self.error = None;
    }

    fn load_named_preset(&mut self, name: &str) {
        let directory = crate::preset::preset_dir();
        let saved = directory.join(format!("{name}.json"));
        let preset = if saved.exists() {
            Preset::load(&directory, name)
        } else {
            match name {
                "blues" => Ok(Preset::blues()),
                "empty" => Ok(Preset::empty()),
                _ => Preset::load(&directory, name),
            }
        };
        match preset {
            Ok(preset) => {
                self.apply_preset(&preset);
                self.notice = format!("loaded {name}");
            }
            Err(error) => self.error = Some(format!("load failed: {error}")),
        }
    }

    fn apply_rack_edits(&mut self, edits: Vec<RackEdit>) {
        for edit in edits {
            match edit {
                RackEdit::SetEnabled { id, on } => {
                    if let Some(index) = self.slots.iter().position(|slot| slot.id == id) {
                        self.slots[index].enabled = on;
                        self.send(Cmd::SetEnabled { slot: index, on });
                        self.modified = true;
                    }
                }
                RackEdit::SetParam { id, idx, norm } => {
                    if let Some(index) = self.slots.iter().position(|slot| slot.id == id) {
                        self.slots[index].norms[idx] = norm;
                        self.send(Cmd::SetParam {
                            slot: index,
                            idx,
                            norm,
                        });
                        self.modified = true;
                    }
                }
                RackEdit::Remove { id } => {
                    if let Some(index) = self.slots.iter().position(|slot| slot.id == id) {
                        self.send(Cmd::RemoveSlot { slot: index });
                        self.slots.remove(index);
                        self.drag = None;
                        self.modified = true;
                    }
                }
                RackEdit::Move { id, to } => {
                    if let Some(from) = self.slots.iter().position(|slot| slot.id == id) {
                        let to = to.min(self.slots.len().saturating_sub(1));
                        if from != to {
                            self.send(Cmd::MoveSlot { from, to });
                            let slot = self.slots.remove(from);
                            self.slots.insert(to, slot);
                            self.modified = true;
                        }
                    }
                }
                RackEdit::Replace { id, kind } => {
                    if let Some(index) = self.slots.iter().position(|slot| slot.id == id) {
                        let enabled = self.slots[index].enabled;
                        self.send(Cmd::ReplaceSlot {
                            slot: index,
                            with: Slot::build(kind, enabled, rate(&self.shared)),
                        });
                        self.slots[index].kind = kind;
                        self.slots[index].norms = norms_of(kind);
                        self.modified = true;
                    }
                }
                RackEdit::Insert { kind } if self.slots.len() < MAX_SLOTS => {
                    let index = self.slots.len();
                    self.send(Cmd::InsertSlot {
                        at: index,
                        slot: Slot::build(kind, true, rate(&self.shared)),
                    });
                    self.slots.push(SlotUi {
                        id: self.next_slot_id,
                        kind,
                        enabled: true,
                        norms: norms_of(kind),
                    });
                    self.next_slot_id += 1;
                    self.modified = true;
                }
                RackEdit::Insert { .. } => {}
            }
        }
    }

    fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        self.send(Cmd::Flag(Flag::Muted, self.muted));
    }
}

fn write_ppm(path: &PathBuf, image: &egui::ColorImage) -> Result<(), String> {
    let mut file =
        File::create(path).map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    write!(file, "P6\n{} {}\n255\n", image.size[0], image.size[1])
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    for pixel in &image.pixels {
        let [red, green, blue, _] = pixel.to_array();
        file.write_all(&[red, green, blue])
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    Ok(())
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut Ui, _frame: &mut eframe::Frame) {
        self.draw(root);
        self.finish_preview(root.ctx());
    }
}

impl App {
    fn finish_preview(&mut self, ctx: &egui::Context) {
        let screenshot = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(preview) = self.preview.as_mut() else {
            return;
        };
        if let Some(image) = screenshot {
            let result = write_ppm(&preview.path, &image);
            *preview
                .result
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(result);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if preview.started.elapsed() > Duration::from_secs(5) {
            *preview
                .result
                .lock()
                .unwrap_or_else(|error| error.into_inner()) =
                Some(Err("preview screenshot timed out".into()));
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else {
            // egui may discard a sizing pass (including its viewport commands). Request
            // again on settled frames until the backend returns the screenshot event.
            if ctx.cumulative_frame_nr() >= 3 && !ctx.will_discard() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            }
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }

    fn draw(&mut self, root: &mut Ui) {
        self.shared.collect_retired();
        root.ctx().request_repaint_after(Duration::from_millis(33));
        if root.ctx().input(|input| input.key_pressed(egui::Key::M))
            && root.ctx().memory(|memory| memory.focused().is_none())
        {
            self.toggle_mute();
        }
        self.top_strip(root);
        self.monitoring(root);
        self.amp_region(root);
        self.pedalboard(root);
    }

    fn top_strip(&mut self, root: &mut Ui) {
        let status = self.shared.status();
        let local_state = self.error.as_deref().unwrap_or(&self.notice).to_owned();
        let local_color = if self.error.is_some() {
            widgets::ERROR
        } else {
            widgets::MUTED
        };
        let mut load = None;
        egui::Panel::top("safety_preset_strip").show(root, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.heading("TRIODE");
                ui.separator();
                ui.label(
                    egui::RichText::new("Input > Pedals > Amp > Cab > Output")
                        .color(widgets::AMBER),
                );
                ui.separator();
                let input_label = match self.applied_request() {
                    Some(applied) if applied.input_on == self.draft.input_on => {
                        if applied.input_on {
                            "Input armed"
                        } else {
                            "Input disarmed"
                        }
                    }
                    Some(_) => "Input change pending",
                    None if self.last_requested.input_on == self.draft.input_on
                        && self.draft.input_on =>
                    {
                        "Input requested"
                    }
                    None => "Input disarmed",
                };
                if ui
                    .button(input_label)
                    .on_hover_text("Arming keeps the last successfully applied device and buffer.")
                    .clicked()
                {
                    self.arm_input(!self.draft.input_on);
                }
                let mute = if self.muted { "UNMUTE (M)" } else { "MUTE (M)" };
                if ui.button(mute).clicked() {
                    self.toggle_mute();
                }
                ui.separator();
                ui.label("Preset");
                if ui
                    .add(egui::TextEdit::singleline(&mut self.preset_name).desired_width(150.0))
                    .changed()
                {
                    self.modified = true;
                }
                if self.modified {
                    ui.label(egui::RichText::new("modified").color(widgets::AMBER));
                }
                if ui.button("save").clicked() {
                    match self.build_preset().save(&crate::preset::preset_dir()) {
                        Ok(path) => {
                            self.modified = false;
                            self.notice = format!("saved {}", path.display());
                            self.error = None;
                        }
                        Err(error) => self.error = Some(format!("save failed: {error}")),
                    }
                }
                egui::ComboBox::from_id_salt("preset_load")
                    .selected_text("load…")
                    .show_ui(ui, |ui| {
                        for name in ["blues", "empty"] {
                            if ui.selectable_label(false, name).clicked() {
                                load = Some(name.to_string());
                            }
                        }
                        for name in Preset::list(&crate::preset::preset_dir()) {
                            if ui.selectable_label(false, &name).clicked() {
                                load = Some(name);
                            }
                        }
                    });
                if !local_state.is_empty() {
                    ui.separator();
                    ui.label(egui::RichText::new(&local_state).color(local_color));
                }
                if !status.is_empty() {
                    ui.separator();
                    ui.label(
                        egui::RichText::new(&status).color(if status.contains("error") {
                            widgets::ERROR
                        } else {
                            widgets::MUTED
                        }),
                    );
                }
            });
        });
        if let Some(name) = load {
            self.load_named_preset(&name);
        }
    }

    fn amp_region(&mut self, root: &mut Ui) {
        egui::Panel::left("amp_controls")
            .resizable(true)
            .default_size(280.0)
            .size_range(egui::Rangef::new(260.0, 400.0))
            .show(root, |ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                egui::ScrollArea::vertical().show(ui, |ui| {
                    widgets::section(ui, "Amp");
                    ui.label("Gain");
                    self.amp_knobs(ui, &[0]);
                    ui.separator();
                    ui.label("Tone");
                    self.amp_knobs(ui, &[1, 2, 3, 4]);
                    ui.separator();
                    ui.label("Output");
                    self.amp_knobs(ui, &[5, 6]);
                    ui.separator();
                    if ui.checkbox(&mut self.cab, "Cabinet").changed() {
                        self.send(Cmd::Flag(Flag::Cab, self.cab));
                        self.modified = true;
                    }
                    if ui.checkbox(&mut self.hpf, "Input high-pass").changed() {
                        self.send(Cmd::Flag(Flag::InputHpf, self.hpf));
                        self.modified = true;
                    }
                    ui.separator();
                    self.ir_controls(ui);
                    ui.separator();
                    self.setup(ui);
                });
            });
    }

    fn amp_knobs(&mut self, ui: &mut Ui, indices: &[usize]) {
        let mut changed = Vec::new();
        ui.horizontal_wrapped(|ui| {
            for &index in indices {
                let before = self.amp[index];
                let response = widgets::knob(
                    ui,
                    AMP_SPECS[index].name,
                    &AMP_SPECS[index],
                    &mut self.amp[index],
                );
                if response.changed() && self.amp[index] != before {
                    changed.push((index, self.amp[index]));
                }
            }
        });
        for (idx, norm) in changed {
            self.send(Cmd::AmpParam { idx, norm });
            self.modified = true;
        }
    }

    fn ir_controls(&mut self, ui: &mut Ui) {
        ui.label("Cabinet IR");
        ui.add(egui::TextEdit::singleline(&mut self.ir_path).hint_text("impulse.wav"));
        ui.horizontal(|ui| {
            if ui.button("load session IR").clicked() {
                match Ir::from_wav(&self.ir_path) {
                    Ok(ir) => {
                        let name = ir.name().to_string();
                        self.shared.load_ir(ir);
                        self.custom_ir = Some(name.clone());
                        self.notice = format!("session-only IR: {name}");
                        self.error = None;
                    }
                    Err(error) => self.error = Some(format!("IR load failed: {error}")),
                }
            }
            if ui.button("built-in cab").clicked() {
                self.shared.clear_ir();
                self.custom_ir = None;
                self.notice = "built-in cabinet model".into();
            }
        });
        ui.label(
            egui::RichText::new(match &self.custom_ir {
                Some(name) => format!("Custom IR: {name} (session-only)"),
                None => "Preset load restores the built-in cabinet model".to_string(),
            })
            .size(11.0)
            .color(widgets::MUTED),
        );
    }

    fn setup(&mut self, ui: &mut Ui) {
        egui::CollapsingHeader::new("Audio setup / diagnostics").show(ui, |ui| {
            egui::ComboBox::from_id_salt("input_device")
                .selected_text(self.draft.input.as_deref().unwrap_or("default"))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.draft.input, None, "default");
                    for device in &self.inputs {
                        ui.selectable_value(&mut self.draft.input, Some(device.clone()), device);
                    }
                });
            egui::ComboBox::from_id_salt("output_device")
                .selected_text(self.draft.output.as_deref().unwrap_or("default"))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.draft.output, None, "default");
                    for device in &self.outputs {
                        ui.selectable_value(&mut self.draft.output, Some(device.clone()), device);
                    }
                });
            egui::ComboBox::from_id_salt("requested_buffer")
                .selected_text(format!("{} ms requested", self.draft.buffer_ms))
                .show_ui(ui, |ui| {
                    for ms in [1, 2, 5, 10, 20, 50] {
                        ui.selectable_value(&mut self.draft.buffer_ms, ms, format!("{ms} ms"));
                    }
                });
            ui.horizontal(|ui| {
                if ui.button("refresh devices").clicked() {
                    (self.inputs, self.outputs) = crate::audio_io::devices();
                    self.notice = "device list refreshed".into();
                }
                if ui.button("apply audio setup").clicked() {
                    self.apply_draft();
                }
            });
            let mut tone_on = self.tone_hz > 0.0;
            if ui.checkbox(&mut tone_on, "test tone").changed() {
                self.tone_hz = if tone_on { 110.0 } else { 0.0 };
                self.shared.set_tone(self.tone_hz);
            }
            if tone_on
                && ui
                    .add(
                        egui::Slider::new(&mut self.tone_hz, 20.0..=2_000.0)
                            .suffix(" Hz")
                            .logarithmic(true),
                    )
                    .changed()
            {
                self.shared.set_tone(self.tone_hz);
            }
            let snap = self.shared.stats.snapshot();
            ui.label(format!("requested: {}", request_label(&self.draft)));
            match self.applied_request() {
                Some(request) => ui.label(format!("applied: {}", request_label(&request))),
                None if self.shared.ready.load(Ordering::Relaxed) => {
                    ui.label("applied: awaiting owner report")
                }
                None => ui.label("audio: connecting or retrying"),
            };
            ui.label(format!(
                "callback {} frames · DSP {} Hz · clock {:+.0} ppm",
                snap.buffer_frames,
                snap.rate,
                (snap.ratio - 1.0) * 1_000_000.0
            ));
        });
    }

    fn monitoring(&mut self, root: &mut Ui) {
        if !self.freeze {
            if let Some((input, output, rate, sequence)) = self.shared.scope_snap() {
                if sequence != self.seq {
                    self.seq = sequence;
                    self.snap = Some((input, output, rate));
                }
            }
        }
        egui::Panel::bottom("monitoring")
            .resizable(true)
            .size_range(egui::Rangef::new(180.0, 320.0))
            .show(root, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui.checkbox(&mut self.freeze, "freeze").changed() {
                        self.shared.set_scope(!self.freeze);
                    }
                    ui.add(
                        egui::Slider::new(&mut self.scope_ms, 2.0..=200.0)
                            .text("window")
                            .suffix(" ms"),
                    );
                    let snap = self.shared.stats.snapshot();
                    ui.label("IN");
                    widgets::meter(ui, snap.in_rms, snap.in_peak, snap.clip_in > 0);
                    ui.label("OUT");
                    widgets::meter(ui, snap.out_rms, snap.out_peak, snap.clip_out > 0);
                    ui.label(format!(
                        "CPU {:.0}% · xruns {} · dropped {} · clips {}/{}",
                        snap.cpu_pct,
                        snap.overruns + snap.underruns,
                        snap.dropped_in,
                        snap.clip_in,
                        snap.clip_out
                    ));
                    if ui.button("reset meters").clicked() {
                        self.shared.stats.reset_clips();
                    }
                    if !self.draft.input_on {
                        ui.label(egui::RichText::new("input disarmed").color(widgets::AMBER));
                    } else if self.muted {
                        ui.label(egui::RichText::new("output muted").color(widgets::AMBER));
                    }
                });
                match self.snap.as_ref() {
                    Some((input, output, sample_rate)) => {
                        let width = ui.available_width().max(400.0);
                        ui.horizontal_wrapped(|ui| {
                            widgets::scope(
                                ui,
                                input,
                                output,
                                *sample_rate,
                                self.scope_ms,
                                Vec2::new((width * 0.58).clamp(220.0, 620.0), 145.0),
                            );
                            widgets::transfer(
                                ui,
                                input,
                                output,
                                Vec2::new((width * 0.34).clamp(180.0, 360.0), 145.0),
                            );
                        });
                    }
                    None => {
                        ui.label(if self.draft.input_on {
                            "Waiting for signal."
                        } else {
                            "Input is disarmed; arm it in the safety strip to monitor a guitar."
                        });
                    }
                }
            });
    }

    fn pedalboard(&mut self, root: &mut Ui) {
        let mut edits = Vec::new();
        let mut rects = Vec::new();
        let mut released = None;
        egui::CentralPanel::default().show(root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                widgets::section(ui, "Pedals");
                if self.slots.is_empty() {
                    ui.label("No pedals. Add one below; the amp and cabinet remain active.");
                }
                ui.with_layout(
                    egui::Layout::left_to_right(egui::Align::Min).with_main_wrap(true),
                    |ui| {
                        for (position, slot) in self.slots.iter().enumerate() {
                            let (card_edits, rect, stopped) =
                                Self::slot_card(ui, slot, position, &mut self.drag);
                            edits.extend(card_edits);
                            rects.push((slot.id, rect));
                            released = released.or(stopped);
                        }
                        self.add_card(ui, &mut edits);
                    },
                );
            });
        });
        if let Some((id, point)) = released {
            if let Some(to) = rects
                .iter()
                .position(|(target, rect)| *target != id && rect.contains(point))
            {
                edits.push(RackEdit::Move { id, to });
            }
        }
        self.apply_rack_edits(edits);
    }

    fn slot_card(
        ui: &mut Ui,
        slot: &SlotUi,
        position: usize,
        drag: &mut Option<u64>,
    ) -> (Vec<RackEdit>, Rect, Option<(u64, Pos2)>) {
        let mut edits = Vec::new();
        let mut stopped = None;
        let response = ui.allocate_ui_with_layout(
            Vec2::new(278.0, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.push_id(slot.id, |ui| {
                    egui::Frame::group(ui.style())
                        .fill(widgets::PANEL)
                        .show(ui, |ui| {
                            ui.vertical(|ui| {
                                ui.set_width(264.0);
                                ui.horizontal_wrapped(|ui| {
                                    let handle = ui
                                        .add(egui::Label::new("::").sense(Sense::drag()))
                                        .on_hover_text("Drag to reorder");
                                    if handle.drag_started() {
                                        *drag = Some(slot.id);
                                    }
                                    if handle.drag_stopped() {
                                        if let (Some(id), Some(point)) = (
                                            drag.take(),
                                            handle.ctx.input(|input| input.pointer.interact_pos()),
                                        ) {
                                            stopped = Some((id, point));
                                        }
                                    }
                                    let enabled = if slot.enabled { "Active" } else { "Bypassed" };
                                    if widgets::led(ui, slot.enabled, enabled).clicked() {
                                        edits.push(RackEdit::SetEnabled {
                                            id: slot.id,
                                            on: !slot.enabled,
                                        });
                                    }
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{:02} {}",
                                            position + 1,
                                            slot.kind.short()
                                        ))
                                        .strong(),
                                    );
                                    if ui.button("<").on_hover_text("move earlier").clicked()
                                        && position > 0
                                    {
                                        edits.push(RackEdit::Move {
                                            id: slot.id,
                                            to: position - 1,
                                        });
                                    }
                                    if ui.button(">").on_hover_text("move later").clicked() {
                                        edits.push(RackEdit::Move {
                                            id: slot.id,
                                            to: position + 1,
                                        });
                                    }
                                    if ui.button("x").on_hover_text("remove pedal").clicked() {
                                        edits.push(RackEdit::Remove { id: slot.id });
                                    }
                                });
                                let mut kind = slot.kind;
                                egui::ComboBox::from_id_salt("pedal_kind")
                                    .selected_text(kind.name())
                                    .show_ui(ui, |ui| {
                                        for candidate in EffectKind::ALL {
                                            ui.selectable_value(
                                                &mut kind,
                                                candidate,
                                                candidate.name(),
                                            );
                                        }
                                    });
                                if kind != slot.kind {
                                    edits.push(RackEdit::Replace { id: slot.id, kind });
                                }
                                let mut norms = slot.norms;
                                widgets::knob_row(
                                    ui,
                                    slot.kind.params(),
                                    &mut norms[..slot.kind.params().len()],
                                    |idx, norm| {
                                        edits.push(RackEdit::SetParam {
                                            id: slot.id,
                                            idx,
                                            norm,
                                        });
                                    },
                                );
                            });
                        })
                })
            },
        );
        (edits, response.response.rect, stopped)
    }

    fn add_card(&mut self, ui: &mut Ui, edits: &mut Vec<RackEdit>) {
        ui.allocate_ui_with_layout(
            Vec2::new(278.0, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.group(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(264.0);
                        ui.label("Add pedal");
                        egui::ComboBox::from_id_salt("add_kind")
                            .selected_text(self.add_kind.name())
                            .show_ui(ui, |ui| {
                                for kind in EffectKind::ALL {
                                    ui.selectable_value(&mut self.add_kind, kind, kind.name());
                                }
                            });
                        if ui
                            .add_enabled(
                                self.slots.len() < MAX_SLOTS,
                                egui::Button::new("add to end"),
                            )
                            .clicked()
                        {
                            edits.push(RackEdit::Insert {
                                kind: self.add_kind,
                            });
                        }
                        if self.slots.len() >= MAX_SLOTS {
                            ui.label(
                                egui::RichText::new(format!("{MAX_SLOTS} pedals maximum"))
                                    .color(widgets::AMBER),
                            );
                        }
                    });
                });
            },
        );
    }
}

fn request_label(request: &ReopenReq) -> String {
    format!(
        "in {} · out {} · {} ms · {}",
        request.input.as_deref().unwrap_or("default"),
        request.output.as_deref().unwrap_or("default"),
        request.buffer_ms,
        if request.input_on {
            "input armed"
        } else {
            "input disarmed"
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rack(kinds: &[EffectKind]) -> Preset {
        let mut preset = Preset::empty();
        preset.slots = kinds.iter().map(|kind| SlotPreset::new(*kind)).collect();
        preset
    }

    #[test]
    fn saving_keeps_each_pedal_parameter_normalized() {
        let mut preset = rack(&EffectKind::ALL);
        for (slot_index, slot) in preset.slots.iter_mut().enumerate() {
            slot.params = slot
                .kind
                .params()
                .iter()
                .enumerate()
                .map(|(param_index, _)| ((slot_index + param_index + 1) as f32 / 12.0).min(0.97))
                .collect();
        }
        let app = App::for_test(preset.clone());
        let saved = app.build_preset();

        for (expected, actual) in preset.slots.iter().zip(&saved.slots) {
            assert_eq!(actual.params, expected.params);
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
                rack(&[EffectKind::Boost; MAX_SLOTS]),
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
}
