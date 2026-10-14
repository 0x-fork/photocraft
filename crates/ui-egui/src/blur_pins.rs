//! Filter › Blur Gallery › Field Blur… and Iris Blur… as Photoshop's workspace (#3373): pins on the
//! image, each with its own blur. While the dialog is open the image under it takes the pointer:
//! a click on the image adds a pin (and selects it), a drag moves a pin, and a drag on the selected
//! pin's ring turns its blur up or down (one turn = the full 0–500 px; the arc shows the amount).
//! Delete or Backspace removes the selected pin. The dialog shows the selected pin's Blur and the
//! Preview switch; OK runs the filter with the `pins`.
//!
//! Field Blur interpolates the pins' blur between them. An Iris Blur pin is a sharp superellipse,
//! blurred outside: the selected pin shows Photoshop's handles, four on the outline (drag to resize
//! that axis and turn the shape), a square at its corner (drag out to round it towards a rectangle)
//! and four dots inside (drag to set the sharp core, where the blur starts).
//!
//! Everything lives in the dialog's fields, so automation drives it with `ui.dialog.set`: `pins`
//! (as the command takes them; x and y as fractions of the filter's bounds, Iris radii as fractions
//! of their shorter side) and `__selected`. The bounds are the engine's: the selection's when there
//! is one, else the canvas.

use egui::{Color32, Pos2, Stroke, vec2};
use serde_json::{Map, Value, json};

use crate::PhotocraftApp;
use crate::canvas::ViewXform;

pub const COMMAND: &str = "filter.blurGallery.fieldBlur";
pub const IRIS: &str = "filter.blurGallery.irisBlur";
/// The Blur Gallery range, in pixels.
const MAX_BLUR: f64 = 500.0;
/// Photoshop's first pin.
const DEFAULT_BLUR: f64 = 15.0;
/// More pins than anyone places by hand; keeps the params and the per-pixel loop bounded.
pub const MAX_PINS: usize = 64;
/// Screen radii (points): the pin's dot, how near a press must be to grab it, and its ring.
const DOT: f32 = 5.0;
const GRAB: f32 = 9.0;
const RING: f32 = 24.0;
const RING_BAND: f32 = 5.0;
/// Screen size (points) of an Iris handle.
const HANDLE: f32 = 4.5;
/// Iris radii, as the command bounds them.
const MIN_RADIUS: f64 = 0.01;
const MAX_FEATHER: f64 = 0.99;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Field,
    Iris,
}

fn kind(fields: &Map<String, Value>) -> Option<Kind> {
    match fields.get("__command").and_then(Value::as_str) {
        Some(COMMAND) => Some(Kind::Field),
        Some(IRIS) => Some(Kind::Iris),
        _ => None,
    }
}

pub fn owns(fields: &Map<String, Value>) -> bool {
    kind(fields).is_some()
}

/// The Blur Gallery filters that open as this workspace.
pub fn handles(command: &str) -> bool {
    command == COMMAND || command == IRIS
}

/// An Iris Blur pin's shape, as the command takes it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Iris {
    /// Radii as fractions of the bounds' shorter side.
    pub rx: f64,
    pub ry: f64,
    /// Degrees, clockwise on screen.
    pub angle: f64,
    /// 0 = ellipse … 100 = rounded rectangle.
    pub roundness: f64,
    /// Where the blur starts, as a fraction of the shape (the sharp core).
    pub feather: f64,
}

impl Default for Iris {
    /// The command's defaults.
    fn default() -> Self {
        Iris { rx: 0.35, ry: 0.25, angle: 0.0, roundness: 0.0, feather: 0.5 }
    }
}

impl Iris {
    fn clean(self) -> Self {
        let f = |v: f64, d: f64| if v.is_finite() { v } else { d };
        Iris {
            rx: f(self.rx, 0.35).clamp(MIN_RADIUS, 1.0),
            ry: f(self.ry, 0.25).clamp(MIN_RADIUS, 1.0),
            angle: (f(self.angle, 0.0) + 180.0).rem_euclid(360.0) - 180.0,
            roundness: f(self.roundness, 0.0).clamp(0.0, 100.0),
            feather: f(self.feather, 0.5).clamp(0.0, MAX_FEATHER),
        }
    }

    /// The superellipse exponent the engine uses (2 = ellipse … 8).
    fn exponent(self) -> f64 {
        2.0 + self.roundness / 100.0 * 6.0
    }
}

/// Turn a freshly opened Field or Iris Blur dialog into the workspace: one pin in the middle of
/// the filter's bounds with the remembered settings, selected.
pub fn init(app: &mut PhotocraftApp, fields: &mut Map<String, Value>) {
    reset(fields);
    let bounds = app.session.active().map(|st| (st.doc.id, st.doc.bounds(), st.doc.selection.clone()));
    if let Some((doc, canvas, selection)) = bounds {
        let b = selection.map(|s| app.cached_bounds(u64::MAX - doc.0, &s)).filter(|b| !b.is_empty()).unwrap_or(canvas);
        fields.insert("__bounds".into(), json!([b.x0, b.y0, b.x1, b.y1]));
    }
}

/// One pin in the middle with the dialog's settings, selected. Alt+Cancel (Reset) runs this
/// alone, so the bounds captured on opening stay.
pub fn reset(fields: &mut Map<String, Value>) {
    let num = |k: &str, d: f64| fields.get(k).and_then(Value::as_f64).filter(|v| v.is_finite()).unwrap_or(d);
    let blur = num("blur", DEFAULT_BLUR).clamp(0.0, MAX_BLUR);
    let iris = (kind(fields) == Some(Kind::Iris)).then(|| {
        Iris { rx: num("radiusX", 0.35), ry: num("radiusY", 0.25), angle: num("angle", 0.0), roundness: num("roundness", 0.0), feather: num("feather", 0.5) }
            .clean()
    });
    // The pins replace the single shape's params.
    for k in ["centerX", "centerY", "radiusX", "radiusY", "angle", "roundness", "feather"] {
        fields.remove(k);
    }
    fields.insert("blur".into(), json!(blur));
    set_pins(fields, &[Pin { x: 0.5, y: 0.5, blur, iris }]);
    fields.insert("__selected".into(), json!(0));
}

/// Where the dialog first opens (offset from the window's top-left): at the right edge, beside the
/// image, as Photoshop docks its Blur Tools panel, so the pins under it stay in view. Moved, it
/// reopens where it was left, like any dialog.
pub fn first_place(fields: &Map<String, Value>, ctx: &egui::Context) -> Option<egui::Vec2> {
    owns(fields).then(|| vec2((ctx.content_rect().width() - 480.0).max(0.0), 96.0))
}

/// One pin: position as fractions of the bounds, blur in pixels, and an Iris pin's shape.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pin {
    pub x: f64,
    pub y: f64,
    pub blur: f64,
    pub iris: Option<Iris>,
}

/// The pins in `fields`, cleaned (finite, in range, at most [`MAX_PINS`]).
pub fn pins(fields: &Map<String, Value>) -> Vec<Pin> {
    let Some(list) = fields.get("pins").and_then(Value::as_array) else { return Vec::new() };
    let iris = kind(fields) == Some(Kind::Iris);
    let num = |v: &Value, k: &str, d: f64| v.get(k).and_then(Value::as_f64).filter(|x| x.is_finite()).unwrap_or(d);
    list.iter()
        .take(MAX_PINS)
        .map(|v| Pin {
            x: num(v, "x", 0.5).clamp(0.0, 1.0),
            y: num(v, "y", 0.5).clamp(0.0, 1.0),
            blur: num(v, "blur", DEFAULT_BLUR).clamp(0.0, MAX_BLUR),
            iris: iris.then(|| {
                let d = Iris::default();
                Iris {
                    rx: num(v, "radiusX", d.rx),
                    ry: num(v, "radiusY", d.ry),
                    angle: num(v, "angle", d.angle),
                    roundness: num(v, "roundness", d.roundness),
                    feather: num(v, "feather", d.feather),
                }
                .clean()
            }),
        })
        .collect()
}

fn set_pins(fields: &mut Map<String, Value>, pins: &[Pin]) {
    let list: Vec<Value> = pins
        .iter()
        .map(|p| match p.iris {
            None => json!({"x": p.x, "y": p.y, "blur": p.blur}),
            Some(s) => {
                json!({"x": p.x, "y": p.y, "blur": p.blur, "radiusX": s.rx, "radiusY": s.ry, "angle": s.angle, "roundness": s.roundness, "feather": s.feather})
            }
        })
        .collect();
    fields.insert("pins".into(), Value::Array(list));
}

/// The selected pin's index, if it exists.
pub fn selected(fields: &Map<String, Value>) -> Option<usize> {
    let n = pins(fields).len();
    fields.get("__selected").and_then(Value::as_u64).map(|i| i as usize).filter(|i| *i < n)
}

fn select(fields: &mut Map<String, Value>, i: Option<usize>) {
    fields.insert("__selected".into(), i.map_or(Value::Null, |i| json!(i)));
    // The dialog's Blur (remembered for the next time) follows the selected pin.
    if let Some(p) = i.and_then(|i| pins(fields).get(i).copied()) {
        fields.insert("blur".into(), json!(p.blur));
    }
}

/// The filter's bounds in document pixels `[x0, y0, x1, y1]`.
fn bounds(fields: &Map<String, Value>) -> Option<[f64; 4]> {
    let b = fields.get("__bounds")?.as_array()?;
    let v = |i: usize| b.get(i).and_then(Value::as_f64);
    let r = [v(0)?, v(1)?, v(2)?, v(3)?];
    (r[2] > r[0] && r[3] > r[1]).then_some(r)
}

/// The bounds' shorter side, which Iris radii are fractions of.
fn short_side(b: [f64; 4]) -> f64 {
    (b[2] - b[0]).min(b[3] - b[1]).max(1.0)
}

fn to_doc(b: [f64; 4], p: Pin) -> [f64; 2] {
    [b[0] + p.x * (b[2] - b[0]), b[1] + p.y * (b[3] - b[1])]
}

fn from_doc(b: [f64; 4], d: [f64; 2]) -> (f64, f64) {
    (((d[0] - b[0]) / (b[2] - b[0])).clamp(0.0, 1.0), ((d[1] - b[1]) / (b[3] - b[1])).clamp(0.0, 1.0))
}

/// An Iris pin's local point `(u, v)` (1 = on the shape along that axis) in document pixels.
fn iris_point(b: [f64; 4], p: Pin, s: Iris, u: f64, v: f64) -> [f64; 2] {
    let c = to_doc(b, p);
    let ss = short_side(b);
    let (x, y) = (u * s.rx * ss, v * s.ry * ss);
    let (sin, cos) = s.angle.to_radians().sin_cos();
    [c[0] + x * cos - y * sin, c[1] + x * sin + y * cos]
}

/// A document point in an Iris pin's local coordinates (the engine's `u`, `v`).
fn iris_local(b: [f64; 4], p: Pin, s: Iris, d: [f64; 2]) -> (f64, f64) {
    let c = to_doc(b, p);
    let ss = short_side(b);
    let (dx, dy) = (d[0] - c[0], d[1] - c[1]);
    let (sin, cos) = s.angle.to_radians().sin_cos();
    ((dx * cos + dy * sin) / (s.rx * ss), (-dx * sin + dy * cos) / (s.ry * ss))
}

/// The superellipse's diagonal point `t` (on the shape at (t, t)): 2^(−1/m).
fn diagonal(s: Iris) -> f64 {
    0.5f64.powf(1.0 / s.exponent())
}

/// The selected Iris pin's handles in local coordinates: the four on the outline (along the axes),
/// the roundness square (upper right) and the four feather dots inside (on the diagonals).
fn axis_handles() -> [(f64, f64); 4] {
    [(1.0, 0.0), (0.0, 1.0), (-1.0, 0.0), (0.0, -1.0)]
}

fn round_handle(s: Iris) -> (f64, f64) {
    let t = diagonal(s);
    (t, -t)
}

fn feather_handles(s: Iris) -> [(f64, f64); 4] {
    let k = s.feather * diagonal(s);
    [(k, k), (-k, k), (-k, -k), (k, -k)]
}

/// Remove the selected pin; the one before it (or the new first) is selected.
pub fn remove_selected(fields: &mut Map<String, Value>) -> bool {
    let Some(i) = selected(fields) else { return false };
    let mut list = pins(fields);
    list.remove(i);
    set_pins(fields, &list);
    select(fields, if list.is_empty() { None } else { Some(i.saturating_sub(1)) });
    true
}

/// The dialog body: the selected pin's Blur, how to place pins, and Preview.
pub fn body(ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let t = crate::theme::Tokens::get(ui.ctx());
    match selected(f) {
        Some(i) => {
            let mut list = pins(f);
            if let Some(pin) = list.get_mut(i) {
                let mut v = pin.blur as f32;
                if crate::widgets::slider_row(ui, tl!("Blur"), &mut v, 0.0..=MAX_BLUR as f32, "px", None).changed() {
                    pin.blur = f64::from(v.round()).clamp(0.0, MAX_BLUR);
                    set_pins(f, &list);
                    select(f, Some(i));
                }
            }
        }
        None => {
            ui.label(egui::RichText::new(tl!("No pin selected")).color(t.text_dim));
        }
    }
    ui.add_space(4.0);
    let hint = |ui: &mut egui::Ui, text: &str| ui.add(egui::Label::new(egui::RichText::new(text).color(t.text_dim).size(11.5)).wrap());
    hint(ui, tl!("Click the image to add a pin; drag a pin to move it, or its ring to change its blur. Delete removes the selected pin."));
    if kind(f) == Some(Kind::Iris) {
        hint(
            ui,
            tl!("Drag the handles on the outline to resize and turn it, the square to round its corners, and the dots inside to set where the blur starts."),
        );
    }
    // Delete removes the selected pin, unless a field is being typed in.
    let typing = ui.ctx().memory(|m| m.focused().is_some());
    if !typing && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Delete) || i.consume_key(egui::Modifiers::NONE, egui::Key::Backspace)) {
        remove_selected(f);
        ui.ctx().request_repaint();
    }
    if let Some(mut preview) = f.get("__preview").and_then(Value::as_bool) {
        ui.add_space(4.0);
        crate::widgets::checkbox(ui, &mut preview, tl!("Preview"));
        f.insert("__preview".into(), json!(preview));
    }
}

/// What a press at screen point `at` grabs: one of the selected Iris pin's handles (axis 0–3 =
/// +u, +v, −u, −v), the selected pin's ring, a pin, or nothing (a new pin).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Grab {
    Axis(usize, usize),
    Round(usize),
    Feather(usize),
    Ring(usize),
    Pin(usize),
    New,
}

fn grab(fields: &Map<String, Value>, xf: &ViewXform, at: Pos2) -> Grab {
    let Some(b) = bounds(fields) else { return Grab::New };
    let list = pins(fields);
    let screen = |d: [f64; 2]| xf.to_screen(d[0] as f32, d[1] as f32);
    let near = |d: [f64; 2]| screen(d).distance(at) <= GRAB;
    if let Some(i) = selected(fields)
        && let Some(p) = list.get(i).copied()
    {
        if let Some(s) = p.iris {
            if let Some(k) = axis_handles().iter().position(|&(u, v)| near(iris_point(b, p, s, u, v))) {
                return Grab::Axis(i, k);
            }
            let (u, v) = round_handle(s);
            if near(iris_point(b, p, s, u, v)) {
                return Grab::Round(i);
            }
            if feather_handles(s).iter().any(|&(u, v)| near(iris_point(b, p, s, u, v))) {
                return Grab::Feather(i);
            }
        }
        if (screen(to_doc(b, p)).distance(at) - RING).abs() <= RING_BAND {
            return Grab::Ring(i);
        }
    }
    list.iter()
        .enumerate()
        .map(|(i, p)| (i, screen(to_doc(b, *p)).distance(at)))
        .filter(|(_, d)| *d <= GRAB)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(Grab::New, |(i, _)| Grab::Pin(i))
}

/// The pointer on the free image under the dialog: `over` is where it is, `press` where the
/// primary button is held (`dialogs::free_press`), both in screen points. Returns whether the
/// workspace took it.
pub fn canvas(app: &mut PhotocraftApp, ctx: &egui::Context, xf: &ViewXform, over: Pos2, press: Option<Pos2>) -> bool {
    let Some(d) = app.ui.dialogs.last_mut().filter(|d| owns(&d.fields)) else { return false };
    let f = &mut d.fields;
    let Some(b) = bounds(f) else { return false };
    let (start, down) = ctx.input(|i| (i.pointer.primary_pressed(), i.pointer.primary_down()));
    ctx.set_cursor_icon(match grab(f, xf, over) {
        Grab::Ring(_) | Grab::Round(_) => egui::CursorIcon::Alias,
        Grab::Axis(..) | Grab::Feather(_) => egui::CursorIcon::Move,
        Grab::Pin(_) => egui::CursorIcon::Grab,
        Grab::New => egui::CursorIcon::Crosshair,
    });
    if !down && press.is_none() {
        f.remove("__drag");
        return true;
    }
    let Some(at) = press else { return true };
    let doc = xf.to_doc(at);
    let mut list = pins(f);
    if start {
        let drag = match grab(f, xf, at) {
            Grab::Axis(i, k) => json!({"axis": i, "k": k}),
            Grab::Round(i) => json!({"round": i}),
            Grab::Feather(i) => json!({"feather": i}),
            Grab::Ring(i) => {
                let c = list.get(i).map(|p| to_doc(b, *p)).unwrap_or(doc);
                json!({"ring": i, "angle": (doc[1] - c[1]).atan2(doc[0] - c[0])})
            }
            Grab::Pin(i) => {
                select(f, Some(i));
                let c = list.get(i).map(|p| to_doc(b, *p)).unwrap_or(doc);
                json!({"pin": i, "dx": c[0] - doc[0], "dy": c[1] - doc[1]})
            }
            Grab::New if list.len() < MAX_PINS && (b[0]..=b[2]).contains(&doc[0]) && (b[1]..=b[3]).contains(&doc[1]) => {
                // A new pin takes the selected pin's blur (as the dialog's Blur shows it) and an
                // Iris pin its shape too.
                let blur = f.get("blur").and_then(Value::as_f64).filter(|v| v.is_finite()).unwrap_or(DEFAULT_BLUR).clamp(0.0, MAX_BLUR);
                let iris = (kind(f) == Some(Kind::Iris)).then(|| selected(f).and_then(|i| list.get(i)).and_then(|p| p.iris).unwrap_or_default());
                let (x, y) = from_doc(b, doc);
                list.push(Pin { x, y, blur, iris });
                set_pins(f, &list);
                select(f, Some(list.len() - 1));
                json!({"pin": list.len() - 1, "dx": 0.0, "dy": 0.0})
            }
            Grab::New => return true,
        };
        f.insert("__drag".into(), drag);
        ctx.request_repaint();
        return true;
    }
    let Some(drag) = f.get("__drag").cloned() else { return true };
    let index = |k: &str| drag.get(k).and_then(Value::as_u64).map(|i| i as usize);
    if let Some(i) = index("pin")
        && let Some(p) = list.get_mut(i)
    {
        let num = |k: &str| drag.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        (p.x, p.y) = from_doc(b, [doc[0] + num("dx"), doc[1] + num("dy")]);
    } else if let Some(i) = index("ring")
        && let Some(p) = list.get_mut(i)
    {
        // The angle turned since the last frame, the short way round: clockwise raises the blur.
        let c = to_doc(b, *p);
        let a = (doc[1] - c[1]).atan2(doc[0] - c[0]);
        let last = drag.get("angle").and_then(Value::as_f64).unwrap_or(a);
        let turn = (a - last + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI;
        p.blur = (p.blur + turn / std::f64::consts::TAU * MAX_BLUR).clamp(0.0, MAX_BLUR);
        f.insert("__drag".into(), json!({"ring": i, "angle": a}));
    } else if let Some(i) = index("axis")
        && let Some(p) = list.get_mut(i)
        && let Some(mut s) = p.iris
    {
        // The handle follows the pointer: its distance is that axis's radius, its direction turns
        // the shape (the +v handle sits a quarter turn after +u).
        let c = to_doc(b, *p);
        let (dx, dy) = (doc[0] - c[0], doc[1] - c[1]);
        let r = (dx.hypot(dy) / short_side(b)).clamp(MIN_RADIUS, 1.0);
        let k = index("k").unwrap_or(0);
        if k % 2 == 0 {
            s.rx = r;
        } else {
            s.ry = r;
        }
        s.angle = dy.atan2(dx).to_degrees() - 90.0 * k as f64;
        p.iris = Some(s.clean());
    } else if let Some(i) = index("round")
        && let Some(p) = list.get_mut(i)
        && let Some(s) = p.iris
    {
        // Out along the diagonal rounds towards a rectangle: the handle is the shape's diagonal
        // point (t, t), t = 2^(−1/m), so t gives the exponent back.
        let (u, v) = iris_local(b, *p, s, doc);
        let t = ((u.abs() + v.abs()) / 2.0).clamp(0.5f64.sqrt(), 0.5f64.powf(1.0 / 8.0));
        let m = -std::f64::consts::LN_2 / t.ln();
        p.iris = Some(Iris { roundness: (m - 2.0) / 6.0 * 100.0, ..s }.clean());
    } else if let Some(i) = index("feather")
        && let Some(p) = list.get_mut(i)
        && let Some(s) = p.iris
    {
        // The dots sit where the blur starts: the pointer's distance in the shape's own measure.
        let (u, v) = iris_local(b, *p, s, doc);
        let m = s.exponent();
        let e = (u.abs().powf(m) + v.abs().powf(m)).powf(1.0 / m);
        p.iris = Some(Iris { feather: if e.is_finite() { e } else { s.feather }, ..s }.clean());
    }
    set_pins(f, &list);
    if let Some(i) = ["pin", "ring", "axis", "round", "feather"].into_iter().find_map(index) {
        select(f, Some(i));
    }
    ctx.request_repaint();
    true
}

/// Draw the pins over the image while the workspace is open: a dot per pin (filled when
/// selected) and, round the selected one, its ring with the blur as an arc from the top, and an
/// Iris pin's outline and handles.
pub fn draw(app: &PhotocraftApp, painter: &egui::Painter, xf: &ViewXform) {
    let Some(d) = app.ui.dialogs.last().filter(|d| owns(&d.fields)) else { return };
    let Some(b) = bounds(&d.fields) else { return };
    let sel = selected(&d.fields);
    let (dark, light) = (Stroke::new(3.0, Color32::from_black_alpha(150)), Stroke::new(1.5, Color32::from_white_alpha(235)));
    let screen = |p: [f64; 2]| xf.to_screen(p[0] as f32, p[1] as f32);
    for (i, p) in pins(&d.fields).into_iter().enumerate() {
        let c = screen(to_doc(b, p));
        if sel != Some(i) {
            painter.circle_stroke(c, DOT, dark);
            painter.circle_stroke(c, DOT, light);
            continue;
        }
        if let Some(s) = p.iris {
            // The outline: the superellipse the blur reaches its full amount at.
            let m = s.exponent();
            let outline: Vec<Pos2> = (0..=128)
                .map(|k| {
                    let a = std::f64::consts::TAU * f64::from(k) / 128.0;
                    let (cos, sin) = (a.cos(), a.sin());
                    let r = (cos.abs().powf(m) + sin.abs().powf(m)).powf(-1.0 / m);
                    screen(iris_point(b, p, s, r * cos, r * sin))
                })
                .collect();
            painter.add(egui::Shape::line(outline.clone(), dark));
            painter.add(egui::Shape::line(outline, light));
            for (u, v) in axis_handles() {
                painter.circle(screen(iris_point(b, p, s, u, v)), HANDLE, Color32::WHITE, Stroke::new(1.0, Color32::from_black_alpha(180)));
            }
            let (u, v) = round_handle(s);
            let r = egui::Rect::from_center_size(screen(iris_point(b, p, s, u, v)), vec2(HANDLE * 2.0, HANDLE * 2.0));
            painter.rect(r, 1.0, Color32::WHITE, Stroke::new(1.0, Color32::from_black_alpha(180)), egui::StrokeKind::Middle);
            for (u, v) in feather_handles(s) {
                painter.circle(
                    screen(iris_point(b, p, s, u, v)),
                    HANDLE + 1.0,
                    Color32::from_white_alpha(230),
                    Stroke::new(1.0, Color32::from_black_alpha(180)),
                );
            }
        }
        painter.circle_stroke(c, RING, dark);
        painter.circle_stroke(c, RING, Stroke::new(1.0, Color32::from_white_alpha(140)));
        let frac = (p.blur / MAX_BLUR) as f32;
        let steps = (frac * 64.0).ceil().max(1.0) as usize;
        let arc: Vec<Pos2> = (0..=steps)
            .map(|k| {
                let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * frac * k as f32 / steps as f32;
                c + vec2(a.cos(), a.sin()) * RING
            })
            .collect();
        if frac > 0.0 {
            painter.add(egui::Shape::line(arc.clone(), Stroke::new(4.5, Color32::from_black_alpha(150))));
            painter.add(egui::Shape::line(arc, Stroke::new(2.5, Color32::WHITE)));
        }
        painter.circle(c, DOT, Color32::WHITE, Stroke::new(1.0, Color32::from_black_alpha(180)));
    }
}

/// Preview proxies run at 1/`k` of the document: the pins' pixel blur scales with them.
pub fn scale_for_proxy(params: &mut Value, k: u32) {
    if k <= 1 {
        return;
    }
    let Some(list) = params.get_mut("pins").and_then(Value::as_array_mut) else { return };
    for pin in list {
        if let Some(b) = pin.get("blur").and_then(Value::as_f64) {
            pin["blur"] = json!(b / f64::from(k));
        }
    }
}
