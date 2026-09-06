//! The desktop UI. The audio thread owns DSP; this module only mirrors controls and sends
//! complete commands through [`Shared`].

mod rack;
#[cfg(test)]
mod tests;
mod widgets;

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Ui, Vec2};

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
