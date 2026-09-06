use egui::{Pos2, Rect, Sense, Ui, Vec2};

use crate::params::{EffectKind, MAX_SLOTS};

use super::{widgets, App, RackEdit, SlotUi};

impl App {
    pub(super) fn pedalboard(&mut self, root: &mut Ui) {
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

    pub(super) fn slot_card(
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

    pub(super) fn add_card(&mut self, ui: &mut Ui, edits: &mut Vec<RackEdit>) {
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
