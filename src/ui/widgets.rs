//! Small custom widgets: knobs, LEDs, meters, and the two waveform views.
//!
//! Deliberately built out of `allocate_exact_size` + `painter` rather than egui's `Widget`
//! trait or `Plot` -- those APIs churn between egui releases, while these two have been
//! stable for years and give pixel-exact control, which a scope needs.

use egui::{Align2, Color32, FontId, Layout, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, Vec2};

use crate::params::ParamSpec;

/// Knob footprint including its label and value readout.
pub const KNOB: Vec2 = Vec2::new(58.0, 78.0);

const TRACK: Color32 = Color32::from_gray(70);
const TEXT: Color32 = Color32::from_gray(190);
const DIM: Color32 = Color32::from_gray(130);
const IN_COLOUR: Color32 = Color32::from_rgb(110, 190, 255);
const OUT_COLOUR: Color32 = Color32::from_rgb(120, 230, 140);

fn label_font() -> FontId {
    FontId::proportional(11.0)
}

fn value_font() -> FontId {
    FontId::monospace(11.0)
}

/// A rotary knob bound to a *normalised* 0..=1 value.
///
/// Everything in this app stores knobs normalised (that is what [`Cmd::SetParam`] carries
/// and what the smoothers interpolate), so the widget is normalised too and only consults
/// the [`ParamSpec`] to print a real unit. Drag up/right to raise, hold shift for fine,
/// scroll to nudge, double-click to reset to the preset default.
pub fn knob(ui: &mut Ui, label: &str, spec: &ParamSpec, u: &mut f32) -> Response {
    // `click_and_drag` rather than `drag`: the click half is what makes this widget able to
    // hold keyboard focus, which the arrow-key path below depends on.
    let (rect, resp) = ui.allocate_exact_size(KNOB, Sense::click_and_drag());
    let mut changed = false;

    let drag = resp.drag_delta();
    let step = -(drag.y - drag.x * 0.4) / 180.0;
    if resp.dragged() && step != 0.0 {
        let fine = if ui.input(|i| i.modifiers.shift) {
            0.2
        } else {
            1.0
        };
        *u = (*u + step * fine).clamp(0.0, 1.0);
        changed = true;
    }
    if resp.contains_pointer() {
        let wheel = ui.input(|i| i.smooth_scroll_delta.y);
        if wheel != 0.0 {
            *u = (*u + wheel / 600.0).clamp(0.0, 1.0);
            changed = true;
        }
    }
    // Keyboard: a focused knob takes the arrow keys. A mouse-only parameter control is not
    // acceptable on an instrument you are supposed to adjust while playing, and it is also
    // the only way to set a knob precisely enough to A/B two sounds.
    if resp.clicked() {
        resp.request_focus();
    }
    if resp.has_focus() {
        let (up, down, fine) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowUp) || i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::ArrowLeft),
                i.modifiers.shift,
            )
        });
        let step = if fine { 0.002 } else { 0.02 };
        if up {
            *u = (*u + step).clamp(0.0, 1.0);
            changed = true;
        }
        if down {
            *u = (*u - step).clamp(0.0, 1.0);
            changed = true;
        }
    }
    if resp.double_clicked() {
        *u = spec.default_norm();
        changed = true;
    }

    paint_knob(ui, rect, label, spec, *u);
    let mut resp = resp.on_hover_text(format!(
        "{}: {}\ndrag or arrows (shift = fine), scroll, double-click to reset",
        label,
        spec.format(spec.denorm(*u))
    ));
    // Tab/click focus ring, so the keyboard path above is at least discoverable.
    if resp.has_focus() {
        ui.painter().rect_stroke(
            rect.shrink(1.0),
            4.0,
            Stroke::new(1.5, Color32::from_rgb(120, 200, 255)),
            egui::StrokeKind::Inside,
        );
    }
    if changed {
        resp.mark_changed();
    }
    resp
}

fn paint_knob(ui: &Ui, rect: Rect, label: &str, spec: &ParamSpec, u: f32) {
    let p = ui.painter();
    // Reserve the bottom 24px for the two text lines.
    let body = Rect::from_min_size(rect.min, Vec2::new(rect.width(), rect.height() - 24.0));
    let c = body.center();
    let r = (body.width().min(body.height()) * 0.5 - 3.0).max(4.0);

    // 270° sweep, gap at the bottom: -135° .. +135° measured from 12 o'clock.
    let a0 = -225.0f32.to_radians();
    let span = 270.0f32.to_radians();
    let at = |t: f32| -> Pos2 {
        let a = a0 + span * t;
        Pos2::new(c.x + r * a.cos(), c.y + r * a.sin())
    };
    let mut track = Vec::with_capacity(25);
    for i in 0..=24 {
        track.push(at(i as f32 / 24.0));
    }
    p.add(Shape::line(track, Stroke::new(2.5, TRACK)));
    let mut val = Vec::with_capacity(13);
    for i in 0..=12 {
        let t = i as f32 / 12.0;
        if t > u {
            break;
        }
        val.push(at(t.min(u)));
    }
    if val.len() < 2 {
        val.push(at(0.0));
        val.push(at(u));
    }
    p.add(Shape::line(val, Stroke::new(2.5, OUT_COLOUR)));
    p.circle_filled(at(u), 3.0, Color32::WHITE);
    p.circle_filled(c, 2.0, DIM);

    p.text(
        Pos2::new(rect.center().x, rect.max.y - 16.0),
        Align2::CENTER_CENTER,
        label,
        label_font(),
        TEXT,
    );
    p.text(
        Pos2::new(rect.center().x, rect.max.y - 4.0),
        Align2::CENTER_CENTER,
        spec.format(spec.denorm(u)),
        value_font(),
        DIM,
    );
}

/// Stompbox LED + bypass. Clicking the light toggles the effect.
pub fn led(ui: &mut Ui, on: bool, label: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(14.0, 14.0), Sense::click());
    let c = rect.center();
    let fill = if on {
        Color32::from_rgb(255, 90, 60)
    } else {
        Color32::from_rgb(70, 40, 38)
    };
    ui.painter().circle_filled(c, 5.5, fill);
    ui.painter().circle_stroke(c, 6.5, Stroke::new(1.0, TRACK));
    if !label.is_empty() {
        ui.painter().text(
            Pos2::new(rect.max.x + 4.0, c.y),
            Align2::LEFT_CENTER,
            label,
            label_font(),
            DIM,
        );
    }
    resp.on_hover_text(if on { "bypass" } else { "engage" })
}

/// Map a linear level to 0..=1 on a log scale, floor at -60 dB.
fn db_unit(x: f32) -> f32 {
    ((x.max(1e-5)).ln() / 1e-5f32.ln()).clamp(0.0, 1.0)
}

/// Horizontal level bar: filled = RMS, bright cap = peak hold.
pub fn meter(ui: &mut Ui, rms: f32, peak: f32, clipped: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(110.0, 9.0), Sense::hover());
    let p = ui.painter();
    p.add(Shape::rect_filled(rect, 2.0, Color32::from_gray(28)));
    let w = rect.width();
    let fill = w * db_unit(rms);
    let cap = w * db_unit(peak);
    let colour = if clipped {
        Color32::from_rgb(255, 80, 80)
    } else if peak > 0.98 {
        Color32::from_rgb(255, 200, 90)
    } else {
        Color32::from_rgb(90, 200, 120)
    };
    let bar = Rect::from_min_size(rect.min, Vec2::new(fill, rect.height()));
    p.add(Shape::rect_filled(bar, 2.0, colour.linear_multiply(0.7)));
    let capr = Rect::from_min_size(
        Pos2::new(rect.min.x + cap.clamp(0.0, w - 2.0), rect.min.y),
        Vec2::new(2.0, rect.height()),
    );
    p.add(Shape::rect_filled(capr, 0.0, colour));
    resp
}

/// Draw a polyline of samples into `area`, one point per pixel at most.
fn trace(
    p: &egui::Painter,
    area: Rect,
    xs: &[f32],
    ys: &[f32],
    mid: f32,
    scale: f32,
    colour: Color32,
) {
    if ys.len() < 2 {
        return;
    }
    let h = area.height() * 0.5;
    let n = ys.len();
    let step = (n / (area.width() as usize).max(2)).max(1);
    let mut pts = Vec::with_capacity(n / step + 2);
    let mut i = 0;
    while i < n {
        // Downsample to one vertex per pixel, keeping the worst |value| in each bucket --
        // picking the first sample instead would alias away the peaks, which on a scope is
        // exactly the thing you are looking for.
        let mut worst = ys[i];
        let mut j = i;
        let end = (i + step).min(n);
        while j < end {
            if ys[j].abs() > worst.abs() {
                worst = ys[j];
            }
            j += 1;
        }
        let t = i as f32 / (n - 1) as f32;
        pts.push(Pos2::new(
            area.min.x + t * area.width(),
            mid - (worst / scale).clamp(-1.0, 1.0) * h,
        ));
        i = end;
    }
    if xs.len() == ys.len() {
        let _ = xs; // time axis is implicit here; kept in the signature for callers' sake
    }
    p.add(Shape::line(pts, Stroke::new(1.2, colour)));
}

fn frame(ui: &Ui, title: &str, area: Rect) {
    let p = ui.painter();
    p.add(Shape::rect_filled(area, 3.0, Color32::from_gray(18)));
    p.add(Shape::rect_stroke(
        area,
        3.0,
        Stroke::new(1.0, Color32::from_gray(60)),
        egui::StrokeKind::Inside,
    ));
    p.text(
        area.min + Vec2::new(6.0, 4.0),
        Align2::LEFT_TOP,
        title,
        label_font(),
        DIM,
    );
}

/// Oscilloscope: input over output, sharing a time axis.
///
/// The window is the engine's last few thousand frames, so this is what actually came out
/// of the DAC -- not a prediction from the shaper's math.
pub fn scope(ui: &mut Ui, din: &[f32], dout: &[f32], sr: f32, ms: f32) -> Response {
    let (area, resp) = ui.allocate_exact_size(Vec2::new(340.0, 150.0), Sense::hover());
    frame(
        ui,
        &format!("scope — last {ms:.0} ms ({} Hz)", sr as u32),
        area,
    );
    let shown = ((ms * 1e-3 * sr) as usize).min(din.len());
    if shown < 2 {
        return resp;
    }
    let (i, o) = (&din[din.len() - shown..], &dout[dout.len() - shown..]);
    // Auto-range so a quiet signal is still readable, with a floor so silence does not
    // explode into noise on a full-scale axis.
    let scale = peak(i).max(peak(o)).max(0.05);
    let mid = area.center().y;
    let p = ui.painter();
    p.line_segment(
        [Pos2::new(area.min.x, mid), Pos2::new(area.max.x, mid)],
        Stroke::new(1.0, Color32::from_gray(45)),
    );
    trace(p, area, i, i, mid, scale, IN_COLOUR);
    trace(p, area, o, o, mid, scale, OUT_COLOUR);
    let key = |x: f32, c: Color32, t: &str| {
        p.text(
            Pos2::new(area.max.x - x, area.min.y + 6.0),
            Align2::RIGHT_TOP,
            t,
            label_font(),
            c,
        );
    };
    key(60.0, OUT_COLOUR, "out");
    key(90.0, IN_COLOUR, "in");
    resp.on_hover_text(format!(
        "blue = input, green = output\nfull scale: {:.2}\n{} samples",
        scale, shown
    ))
}

/// Input→output transfer plot: x is the sample going in, y the same moment coming out.
///
/// A scope trace cannot prove a signal was clipped -- a saturated sine looks like a sine
/// until you zoom in. This cannot lie: unity is the diagonal, so any pedal that limits,
/// squares off, or pumps shows up as the cloud peeling off that line and going flat.
/// Memory-based effects (delay, reverb, chorus) legitimately make a thick cloud rather than
/// a curve, because their output does not depend on the current input alone.
pub fn transfer(ui: &mut Ui, din: &[f32], dout: &[f32]) -> Response {
    let (area, resp) = ui.allocate_exact_size(Vec2::new(220.0, 150.0), Sense::hover());
    frame(ui, "input → output", area);
    if din.len() != dout.len() || din.len() < 2 {
        return resp;
    }
    let xr = peak(din).max(0.05);
    let yr = peak(dout).max(0.05);
    let to = |x: f32, y: f32| -> Pos2 {
        Pos2::new(
            area.min.x + (0.5 + 0.5 * (x / xr)).clamp(0.0, 1.0) * area.width(),
            area.max.y - (0.5 + 0.5 * (y / yr)).clamp(0.0, 1.0) * area.height(),
        )
    };
    let p = ui.painter();
    // The unity reference. Note it is only a straight line when x and y share a range.
    p.line_segment(
        [to(-xr.min(yr), -xr.min(yr)), to(xr.min(yr), xr.min(yr))],
        Stroke::new(1.0, Color32::from_gray(70)),
    );
    p.line_segment(
        [to(0.0, -yr), to(0.0, yr)],
        Stroke::new(1.0, Color32::from_gray(40)),
    );
    p.line_segment(
        [to(-xr, 0.0), to(xr, 0.0)],
        Stroke::new(1.0, Color32::from_gray(40)),
    );
    // One vertex per input sample, drawn in input order so the trace reads as a path
    // through the transfer characteristic rather than a scatter of points.
    let step = (din.len() / 1200).max(1);
    let mut pts = Vec::with_capacity(din.len() / step + 2);
    let mut i = 0;
    while i < din.len() {
        pts.push(to(din[i], dout[i]));
        i += step;
    }
    p.add(Shape::line(pts, Stroke::new(1.0, OUT_COLOUR)));
    resp.on_hover_text(format!(
        "x ±{xr:.2} · y ±{yr:.2}\ndashed = 1:1. Flat tops = clipping.\nDelay/reverb spread by design."
    ))
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |a, s| a.max(s.abs()))
}

/// A labelled row of knobs that wraps without turning into a wall of code at the call site.
pub fn knob_row(
    ui: &mut Ui,
    specs: &[ParamSpec],
    norms: &mut [f32],
    mut send: impl FnMut(usize, f32),
) {
    ui.with_layout(Layout::left_to_right(egui::Align::Center), |ui| {
        for (i, spec) in specs.iter().enumerate() {
            if i >= norms.len() {
                break;
            }
            let before = norms[i];
            let mut v = before;
            let r = knob(ui, spec.name, spec, &mut v);
            if r.changed() && v != before {
                send(i, v);
            }
            norms[i] = v;
        }
    });
}

/// Muted microcopy, defined once. Before this, two ad-hoc greys (150 and 120) were
/// hand-applied at a dozen call sites, and `from_gray(120)` at 11px sits under the WCAG
/// 4.5:1 line on this background -- so the dimmest labels were also the ones you most
/// need to read (device names, buffer size, what a control does).
pub const MUTED: egui::Color32 = egui::Color32::from_gray(170);

/// A section heading: one size, one weight, one colour. AMP / RACK / WAVE were three
/// anonymous clusters of identical cards, which gives the eye nothing to scan against --
/// and the amp, the block you touch least, looked exactly like the pedals you tune constantly.
pub fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(title.to_uppercase())
            .strong()
            .size(11.0)
            .color(MUTED),
    );
    ui.separator();
}
