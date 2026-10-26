//! Vector tools: Shape tools (U), Pen (P), Path Selection (A), path overlays and the Paths panel.
//! Direct Selection (A) lives in `direct_select`.
//! All edits go through the engine's `shape.*` / `path.*` commands.

use egui::{Color32, Pos2, Rect, Sense, Stroke, pos2, vec2};
use photocraft_doc::vector::Path;
use photocraft_doc::{Document, LayerContent};
use photocraft_geom::Affine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::canvas::ViewXform;
use crate::shape_gradient_ui::GradientChoices;
use crate::shape_paint_ui::{PaintChange, paint_param, swatch};
use crate::state::{Tool, ToolOptions};
use crate::theme::Tokens;

/// Pen tool path under construction: knots as [anchor, in, out] (document px).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PenPath {
    pub knots: Vec<[[f64; 2]; 3]>,
    /// Indices of cusp knots whose incoming and outgoing handles are not linked.
    #[serde(default)]
    pub unlinked: Vec<usize>,
    #[serde(skip)]
    pub dragging: bool,
    /// Re-dragging the final anchor changes its outgoing control only; the incoming curve stays put.
    #[serde(skip)]
    pub adjusting_last: bool,
    /// ⌥/Alt-dragging the final anchor's incoming handle: it turns alone (#6077).
    #[serde(skip)]
    pub adjusting_in: bool,
    /// Drawn with the Curvature Pen: handles follow the anchors (`curvature_refresh`) and
    /// `unlinked` lists its corner points.
    #[serde(default)]
    pub curvature: bool,
    /// Curvature Pen: the anchor being dragged.
    #[serde(skip)]
    pub moving: Option<usize>,
    /// Drawing carries on from an endpoint of this open subpath of a document path (#6593): the
    /// commit replaces that subpath instead of adding one.
    #[serde(default)]
    pub resumes: Option<Resumed>,
}

/// The subpath a Pen path carries on: where it is and what it was when drawing resumed. The
/// commit replaces it only while it is still that subpath of that document; otherwise (another
/// document, or the path changed meanwhile) the drawing is added as a new subpath.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Resumed {
    pub doc: u64,
    pub path: String,
    pub index: usize,
    pub was: photocraft_doc::vector::Subpath,
}

impl PenPath {
    /// Does `tool` keep drawing this path? Another tool finishes it.
    pub fn drawn_with(&self, tool: Tool) -> bool {
        tool == if self.curvature { Tool::CurvaturePen } else { Tool::Pen }
    }
}

pub fn is_shape_tool(t: Tool) -> bool {
    matches!(t, Tool::Rectangle | Tool::EllipseShape | Tool::Triangle | Tool::Polygon | Tool::Line | Tool::CustomShape)
}

fn rgb32(c: [f32; 4]) -> Color32 {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgb(b(c[0]), b(c[1]), b(c[2]))
}

fn hex(c: [f32; 4]) -> String {
    let c = rgb32(c);
    format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())
}

fn stroke_param(app: &PhotocraftApp) -> Value {
    let o = &app.ui.tool_options;
    if o.stroke_width <= 0.0 {
        return Value::Null;
    }
    let mut p = o.shape_stroke.params();
    p["width"] = json!(o.stroke_width);
    p["color"] = tool_paint_param(app, true);
    p
}

/// The paint the next shape's fill (`stroke` false) or stroke takes: the options bar's gradient,
/// else the foreground / background colour.
fn tool_paint(app: &PhotocraftApp, stroke: bool) -> photocraft_doc::Fill {
    let (gradient, color) = tool_paint_source(app, stroke);
    gradient.clone().unwrap_or_else(|| solid_paint(color))
}

/// [`tool_paint`] as `shape.create` takes it.
fn tool_paint_param(app: &PhotocraftApp, stroke: bool) -> Value {
    let (gradient, color) = tool_paint_source(app, stroke);
    gradient.as_ref().map_or_else(|| json!(color), paint_param)
}

fn tool_paint_source(app: &PhotocraftApp, stroke: bool) -> (&Option<photocraft_doc::Fill>, [f32; 4]) {
    let o = &app.ui.tool_options;
    if stroke { (&o.shape_stroke_gradient, app.session.tools.background) } else { (&o.shape_fill_gradient, app.session.tools.foreground) }
}

/// Geometry of a Shape-tool drag as `shape.create` params, without fill and stroke (None when too
/// small): ⇧ constrains proportions, ⌥ draws from the centre. Custom shapes give their box as a `rect`.
fn shape_geometry(o: &ToolOptions, tool: Tool, start: [f64; 2], end: [f64; 2], mods: egui::Modifiers) -> Option<Value> {
    Some(if tool == Tool::Line {
        let (mut dx, mut dy) = (end[0] - start[0], end[1] - start[1]);
        if mods.shift {
            // Snap to 45°.
            let a = (dy.atan2(dx) / std::f64::consts::FRAC_PI_4).round() * std::f64::consts::FRAC_PI_4;
            let len = dx.hypot(dy);
            (dx, dy) = (len * a.cos(), len * a.sin());
        }
        if dx.hypot(dy) < 1.0 {
            return None;
        }
        json!({"kind": "line", "from": start, "to": [start[0] + dx, start[1] + dy], "weight": o.line_weight.max(1.0)})
    } else {
        let (mut w, mut h) = (end[0] - start[0], end[1] - start[1]);
        if mods.shift {
            let m = w.abs().max(h.abs());
            (w, h) = (m * w.signum(), m * h.signum());
        }
        let (x0, y0) = if mods.alt { (start[0] - w, start[1] - h) } else { (start[0], start[1]) };
        let (w, h) = if mods.alt { (w * 2.0, h * 2.0) } else { (w, h) };
        let rect = [x0.min(x0 + w).round(), y0.min(y0 + h).round(), w.abs().round(), h.abs().round()];
        if rect[2] < 1.0 || rect[3] < 1.0 {
            return None;
        }
        match tool {
            Tool::Rectangle if o.corner_radius > 0.0 => json!({"kind": "roundedRect", "rect": rect, "radii": vec![o.corner_radius; 4]}),
            Tool::Rectangle | Tool::CustomShape => json!({"kind": "rect", "rect": rect}),
            Tool::EllipseShape => json!({"kind": "ellipse", "rect": rect}),
            Tool::Triangle => json!({"kind": "polygon", "rect": rect, "sides": 3}),
            _ => json!({"kind": "polygon", "rect": rect, "sides": o.polygon_sides.max(3)}),
        }
    })
}

/// The box `[x0, y0, x1, y1]` a Shape-tool drag draws (a line's: its ends'), which snaps while
/// Space repositions it (`snap_ui`). None when too small to draw.
pub fn shape_bounds(o: &ToolOptions, tool: Tool, start: [f64; 2], end: [f64; 2], mods: egui::Modifiers) -> Option<[f64; 4]> {
    let g = shape_geometry(o, tool, start, end, mods)?;
    if tool == Tool::Line {
        let a: [f64; 2] = serde_json::from_value(g["from"].clone()).ok()?;
        let b: [f64; 2] = serde_json::from_value(g["to"].clone()).ok()?;
        return Some([a[0].min(b[0]), a[1].min(b[1]), a[0].max(b[0]), a[1].max(b[1])]);
    }
    let [x, y, w, h]: [f64; 4] = serde_json::from_value(g["rect"].clone()).ok()?;
    Some([x, y, x + w, y + h])
}

/// Finish a Shape-tool drag: ⇧ constrains proportions, ⌥ draws from the centre.
pub fn finish_shape(app: &mut PhotocraftApp, tool: Tool, start: [f64; 2], end: [f64; 2], mods: egui::Modifiers) {
    let Some(mut p) = shape_geometry(&app.ui.tool_options, tool, start, end, mods) else { return };
    if tool == Tool::CustomShape {
        // ⇧ keeps the shape's proportions (the rect is already squared).
        if let Ok(rect) = serde_json::from_value(p["rect"].take()) {
            let (fill, stroke) = (fill_param(app), stroke_param(app));
            crate::preset_panels::finish_custom_shape(app, rect, mods.shift, fill, stroke);
        }
        return;
    }
    if let Err(e) = create_shape(app, p) {
        app.ui.status = e;
        app.ui.status_error = true;
    }
}

fn fill_param(app: &PhotocraftApp) -> Value {
    if app.ui.tool_options.shape_fill { tool_paint_param(app, false) } else { Value::Null }
}

/// Create a shape layer from `shape.create` geometry with the options bar's fill and stroke: the
/// end of a drag, and the Create Rectangle / Ellipse / … dialogs (`shape_dialog`).
pub fn create_shape(app: &mut PhotocraftApp, mut geometry: Value) -> Result<Value, String> {
    geometry["fill"] = fill_param(app);
    geometry["stroke"] = stroke_param(app);
    app.run("shape.create", geometry)
}

/// The preview and command both use the full float tool colours, including transparent paint.
fn shape_preview(app: &PhotocraftApp, tool: Tool, start: [f64; 2], end: [f64; 2], mods: egui::Modifiers) -> Option<photocraft_doc::ShapeLayer> {
    let o = &app.ui.tool_options;
    let geometry = shape_geometry(o, tool, start, end, mods)?;
    let path = if tool == Tool::CustomShape {
        let rect = geometry.get("rect").and_then(|r| serde_json::from_value::<[f64; 4]>(r.clone()).ok())?;
        let groups = photocraft_engine::presets::shapes::all_groups(&app.session);
        let shape = groups.iter().flat_map(|g| &g.items).find(|s| s.name == app.ui.presets_ui.shape())?;
        photocraft_engine::presets::shapes::fit(&shape.path, rect, mods.shift)
    } else {
        photocraft_engine::vector_cmds::shape_path(&geometry).ok()?
    };
    Some(photocraft_doc::ShapeLayer {
        path,
        fill: o.shape_fill.then(|| tool_paint(app, false)),
        stroke: (o.stroke_width > 0.0).then(|| o.shape_stroke.stroke(o.stroke_width, tool_paint(app, true))),
        ..Default::default()
    })
}

/// Shape-tool drag preview using the same path and vector stroke geometry as the commit.
pub fn draw_shape_preview(app: &PhotocraftApp, painter: &egui::Painter, xf: &ViewXform, tool: Tool, start: [f64; 2], end: [f64; 2], mods: egui::Modifiers) {
    let Some(mut shape) = shape_preview(app, tool, start, end, mods) else { return };
    let path = shape.path.clone();
    let origin = xf.to_screen(0.0, 0.0);
    let x = xf.to_screen(1.0, 0.0) - origin;
    let y = xf.to_screen(0.0, 1.0) - origin;
    let transform = Affine { m: [x.x as f64, x.y as f64, y.x as f64, y.y as f64, origin.x as f64, origin.y as f64] };
    shape.path = shape.path.transform(&transform);
    if let Some(stroke) = &mut shape.stroke {
        stroke.width *= xf.zoom;
    }
    // This lightweight screen overlay retains the existing display-colour behaviour; document
    // rendering still goes through its format/profile path when the shape is committed.
    crate::shape_stroke_ui::paint_shape_preview(painter, &shape);
    let accent = Tokens::get(painter.ctx()).accent;
    for (pts, _) in path_lines(&path, &|q| xf.to_screen(q[0] as f32, q[1] as f32)) {
        // The accent path over a dark halo: visible on any pixels (#172).
        painter.extend(crate::tool_feedback::contrast_path(pts, true, accent));
    }
}

// ---------------------------------------------------------------------------------------------
// Pen

fn pen_to_json(pen: &PenPath, closed: bool) -> Value {
    let folded = fold_close(pen);
    let (pen, closed) = match &folded {
        Some(p) => (p, true),
        None => (pen, closed),
    };
    json!({"subpaths": [{"closed": closed, "knots": pen.knots.iter().enumerate().map(|(i, k)| json!({"anchor": k[0], "in": k[1], "out": k[2], "smooth": !pen.unlinked.contains(&i) && (k[1] != k[0] || k[2] != k[0])})).collect::<Vec<_>>()}]})
}

/// Is `(x, y)` on the first anchor of a Pen path that a click there would close (two anchors or
/// more)? `tol` is the anchors' hit radius in document pixels.
fn on_closing_anchor(pen: &PenPath, x: f64, y: f64, tol: f64) -> bool {
    pen.knots.len() >= 2 && pen.knots.first().is_some_and(|k| (k[0][0] - x).hypot(k[0][1] - y) < tol)
}

/// Would a click at `(x, y)` with `tool` close the path being drawn: the Pen over the first
/// anchor, or the Curvature Pen over the first of three anchors or more? The canvas shows the
/// close-path pointer there, as Photoshop does (#4596).
pub fn closes_path_at(app: &PhotocraftApp, tool: Tool, x: f64, y: f64) -> bool {
    let Some(pen) = app.ui.pen.as_ref().filter(|p| !p.dragging && p.moving.is_none()) else { return false };
    match tool {
        Tool::Pen => !pen.curvature && on_closing_anchor(pen, x, y, 6.0 / app.point_zoom().max(0.01) as f64),
        Tool::CurvaturePen => pen.curvature && pen.knots.len() >= 3 && curvature_hit(app, x, y) == Some(0),
        _ => false,
    }
}

/// Pen press: close on the first anchor, reshape the last anchor's outgoing handle,
/// or add a new anchor (dragging a new anchor pulls symmetrical handles). With ⇧ a new anchor
/// lands on the nearest 45° ray from the previous one, at the pointer's projection onto it.
pub fn pen_down(app: &mut PhotocraftApp, x: f64, y: f64, shift: bool) {
    if !cancel_stale(app) {
        return;
    }
    let tol = 6.0 / app.point_zoom().max(0.01) as f64;
    if app.ui.pen.is_none() {
        app.ui.pen_doc = app.session.active().map(|st| st.doc.id);
    }
    // A press on an endpoint of an open path carries on drawing it, as in Photoshop (#6593): the
    // press then acts on its last anchor, so a drag pulls out the next segment's handle.
    if app.ui.pen.as_ref().is_none_or(|p| p.knots.is_empty())
        && let Some(resumed) = resume_at(app, x, y, tol)
    {
        app.ui.pen = Some(resumed);
    }
    let pen = app.ui.pen.get_or_insert_with(PenPath::default);
    pen.adjusting_in = false;
    if let Some(first) = pen.knots.first().map(|k| k[0])
        && on_closing_anchor(pen, x, y, tol)
    {
        // Pressing the first anchor closes the path on release. Until then a stand-in for it
        // takes the drag, which pulls out that anchor's handles (close-drag, #3306).
        pen.knots.push([first; 3]);
        pen.dragging = true;
        pen.adjusting_last = false;
        return;
    }
    // Clicking the last anchor breaks its outgoing handle without throwing away the incoming
    // curve. Dragging from that anchor then sets the outgoing handle independently (#1482).
    // This must precede adding a point, or a re-click creates a zero-length segment.
    if let Some((i, last)) = pen.knots.len().checked_sub(1).zip(pen.knots.last_mut())
        && (last[0][0] - x).hypot(last[0][1] - y) < tol
    {
        last[2] = last[0];
        if !pen.unlinked.contains(&i) {
            pen.unlinked.push(i);
        }
        pen.dragging = true;
        pen.adjusting_last = true;
        return;
    }
    let p = match pen.knots.last() {
        Some(prev) => shift_45(prev[0], [x, y], shift),
        None => [x, y],
    };
    pen.knots.push([p; 3]);
    pen.dragging = true;
    pen.adjusting_last = false;
}

/// Pen drag: pull out the handles of the anchor just placed. ⇧ (read live, as it is held)
/// turns the handle to the nearest 45° direction from its anchor.
pub fn pen_move(app: &mut PhotocraftApp, x: f64, y: f64, shift: bool) {
    if let Some(pen) = app.ui.pen.as_mut()
        && pen.dragging
    {
        let (adjusting_last, adjusting_in) = (pen.adjusting_last, pen.adjusting_in);
        if let Some(k) = pen.knots.last_mut() {
            let a = k[0];
            let [x, y] = shift_45(a, [x, y], shift);
            if adjusting_in {
                k[1] = [x, y];
                return;
            }
            k[2] = [x, y];
            if !adjusting_last {
                k[1] = [2.0 * a[0] - x, 2.0 * a[1] - y];
            }
        }
    }
}

/// `p`, or with ⇧ `p` projected onto the nearest 45° ray from `origin` (Pen tool constraint).
fn shift_45(origin: [f64; 2], p: [f64; 2], shift: bool) -> [f64; 2] {
    if shift { crate::stroke_constraint::snap45(origin, p) } else { p }
}

/// Undo the most recently placed Pen anchor while its path is still in progress.
/// These knots are editor-only gesture state, not document history until Enter or
/// clicking the first anchor commits the path. Therefore Cmd/Ctrl+Z must consume one
/// knot rather than undoing an unrelated, already committed document operation.
pub fn pen_undo_last_point(app: &mut PhotocraftApp) -> bool {
    if !cancel_stale(app) {
        return false;
    }
    let Some(pen) = app.ui.pen.as_mut() else { return false };
    // A resumed path's own anchors are not the draft's to take back: undoing past the ones added
    // since it resumed drops the draft and leaves the path as it was (#6593).
    if pen.resumes.as_ref().is_some_and(|r| pen.knots.len() <= r.was.knots.len()) {
        // A ⌘/Ctrl selection on the draft goes with it.
        if app.ui.direct_selection.target == Some(crate::direct_select::PathRef::Pen) {
            app.ui.direct_selection = Default::default();
        }
        app.ui.pen = None;
        return true;
    }
    if pen.knots.pop().is_none() {
        return false;
    }
    let n = pen.knots.len();
    let ds = &mut app.ui.direct_selection;
    if ds.target == Some(crate::direct_select::PathRef::Pen) {
        // A ⌘/Ctrl selection on the path being drawn loses the removed anchor.
        ds.anchors.retain(|a| a[1] < n);
        ds.segment = ds.segment.filter(|s| s[1] + 1 < n);
    }
    pen.unlinked.retain(|&i| i < pen.knots.len());
    if pen.curvature {
        pen.moving = None;
        curvature_refresh(pen, false);
    }
    pen.dragging = false;
    if pen.knots.is_empty() {
        app.ui.pen = None;
    }
    true
}

pub fn pen_up(app: &mut PhotocraftApp) {
    let jitter = 2.0 / f64::from(app.point_zoom().max(0.01));
    let Some(pen) = app.ui.pen.as_mut() else { return };
    pen.dragging = false;
    pen.adjusting_last = false;
    pen.adjusting_in = false;
    if closing(pen) {
        // A click on the first anchor closes the path and leaves that anchor's handles alone.
        if let Some(k) = pen.knots.last_mut()
            && (k[2][0] - k[0][0]).hypot(k[2][1] - k[0][1]) < jitter
        {
            k[1] = k[0];
            k[2] = k[0];
        }
        pen_commit(app, true);
    }
}

/// ⌥/Alt during a Pen drag breaks the pair of handles being pulled out: from then on only the
/// outgoing handle follows the pointer and the incoming curve stays as it is (#3306).
pub fn pen_break_handle(app: &mut PhotocraftApp) {
    let Some(pen) = app.ui.pen.as_mut().filter(|p| p.dragging && !p.adjusting_last) else { return };
    let Some(i) = pen.knots.len().checked_sub(1) else { return };
    if !pen.unlinked.contains(&i) {
        pen.unlinked.push(i);
    }
    pen.adjusting_last = true;
}

/// ⌥/Alt-press on one of the last anchor's handles while drawing grabs that handle alone: the
/// drag turns it and the other handle stays put (#3306, #6077). False when the press misses both.
pub fn pen_grab_handle(app: &mut PhotocraftApp, x: f64, y: f64) -> bool {
    let tol = 6.0 / f64::from(app.point_zoom().max(0.01));
    let Some(pen) = app.ui.pen.as_mut() else { return false };
    let Some((i, k)) = pen.knots.len().checked_sub(1).zip(pen.knots.last()) else { return false };
    let dist = |p: [f64; 2]| (p[0] - x).hypot(p[1] - y);
    // Both handles show only while the outgoing one is pulled out (`draw_overlay`).
    if k[2] == k[0] {
        return false;
    }
    let grabbed = |h: [f64; 2]| h != k[0] && dist(h) < tol && dist(h) < dist(k[0]);
    let incoming = if grabbed(k[2]) && dist(k[2]) <= dist(k[1]) {
        false
    } else if grabbed(k[1]) {
        true
    } else {
        return false;
    };
    if !pen.unlinked.contains(&i) {
        pen.unlinked.push(i);
    }
    pen.dragging = true;
    pen.adjusting_last = true;
    pen.adjusting_in = incoming;
    true
}

/// Would a press at `at` close the Pen path: is the pointer on the first anchor of a path that
/// already has a segment, and no drag is under way? The canvas then shows the close cue (#4193).
pub(crate) fn over_first_anchor(pen: &PenPath, at: [f64; 2], tol: f64) -> bool {
    !pen.dragging && pen.moving.is_none() && pen.knots.len() >= 2 && pen.knots.first().is_some_and(|k| (k[0][0] - at[0]).hypot(k[0][1] - at[1]) < tol)
}

/// Is a close-drag in progress: does the path end in the stand-in for its first anchor?
fn closing(pen: &PenPath) -> bool {
    pen.knots.len() >= 3 && pen.knots.first().map(|k| k[0]) == pen.knots.last().map(|k| k[0])
}

/// The path with a close-drag's stand-in folded into the first knot, which takes the dragged
/// handles (smooth, or a cusp after ⌥/Alt); `None` when no close-drag is in progress.
fn fold_close(pen: &PenPath) -> Option<PenPath> {
    if !closing(pen) {
        return None;
    }
    let mut p = pen.clone();
    let stand_in = p.knots.pop()?;
    let broken = p.unlinked.contains(&p.knots.len());
    p.unlinked.retain(|&i| i < p.knots.len());
    if stand_in[1] != stand_in[0] || stand_in[2] != stand_in[0] {
        let first = p.knots.first_mut()?;
        first[1] = stand_in[1];
        first[2] = stand_in[2];
        p.unlinked.retain(|&i| i != 0);
        if broken {
            p.unlinked.push(0);
        }
    }
    Some(p)
}

/// The Pen path being drawn as a document path (one open subpath), so ⌘/Ctrl (Direct Selection)
/// can edit it in place while drawing (#4022).
pub(crate) fn pen_path(pen: &PenPath) -> Path {
    let pt = |q: [f64; 2]| photocraft_geom::Point::new(q[0], q[1]);
    let knots = pen
        .knots
        .iter()
        .enumerate()
        .map(|(i, k)| photocraft_doc::Knot {
            anchor: pt(k[0]),
            in_ctrl: pt(k[1]),
            out_ctrl: pt(k[2]),
            smooth: !pen.unlinked.contains(&i) && (k[1] != k[0] || k[2] != k[0]),
        })
        .collect();
    Path::new(vec![photocraft_doc::vector::Subpath { closed: false, knots, op: Default::default() }])
}

/// The Pen path being drawn becomes `path`'s first subpath, edited with ⌘/Ctrl (Direct Selection);
/// drawing carries on from its last anchor. A Curvature Pen path keeps following its anchors.
pub(crate) fn pen_replace(app: &mut PhotocraftApp, path: &Path) {
    let Some(pen) = app.ui.pen.as_mut() else { return };
    let knots = path.subpaths.first().map(|s| s.knots.as_slice()).unwrap_or_default();
    pen.knots = knots.iter().map(knot_points).collect();
    pen.unlinked = knots.iter().enumerate().filter(|(_, k)| !k.smooth).map(|(i, _)| i).collect();
    pen.dragging = false;
    pen.adjusting_last = false;
    pen.adjusting_in = false;
    pen.moving = None;
    if pen.curvature {
        curvature_refresh(pen, false);
    }
    if pen.knots.is_empty() {
        app.ui.pen = None;
    }
}

/// Preserve a foreign document's draft as its Work Path before another document can edit it.
/// A failed commit retains the draft and blocks the caller until it can be retried.
pub fn cancel_stale(app: &mut PhotocraftApp) -> bool {
    let active = app.session.active().map(|st| st.doc.id);
    if app.ui.pen.is_none() {
        app.ui.pen_doc = None;
        return true;
    }
    let Some(owner) = app.ui.pen_doc.filter(|d| Some(*d) != active) else { return true };
    if let Some(index) = app.session.documents().iter().position(|d| d.doc.id == owner) {
        let Some(mut pen) = app.ui.pen.clone() else { return true };
        if pen.curvature {
            curvature_refresh(&mut pen, false);
        }
        let active = app.session.active_index();
        app.session.set_active(index);
        // Bypass shell targets and sync_views: both belong to the newly active document.
        let result = app.session.execute("path.set", json!({"name": "work", "op": "combine", "path": pen_to_json(&pen, false)}));
        if let Some(index) = active {
            app.session.set_active(index);
        }
        if let Err(e) = result {
            app.ui.status = e.to_string();
            app.ui.status_error = true;
            return false;
        }
    }
    app.ui.pen = None;
    app.ui.pen_doc = None;
    if app.ui.direct_selection.target == Some(crate::direct_select::PathRef::Pen) {
        app.ui.direct_selection = Default::default();
    }
    true
}

/// Finish the pen path: a work path (Path mode) or a new shape layer (Shape mode).
pub fn pen_commit(app: &mut PhotocraftApp, closed: bool) {
    // The control channel is drained after `sync_views`, so a document switch in this frame has
    // not been seen yet: never commit the path into the document it was not drawn on.
    if !cancel_stale(app) {
        return;
    }
    let Some(mut pen) = app.ui.pen.take() else { return };
    app.ui.pen_doc = None;
    if app.ui.direct_selection.target == Some(crate::direct_select::PathRef::Pen) {
        // Its anchors are no longer the Pen's: the committed path is another path.
        app.ui.direct_selection = Default::default();
    }
    if pen.curvature {
        curvature_refresh(&mut pen, closed);
    }
    commit_path(app, &pen, closed);
}

/// Commit a finished Pen, Freeform Pen or Curvature Pen path, as one History step: a new shape
/// layer (Shape mode), else a subpath of the targeted vector mask, else the work path.
fn commit_path(app: &mut PhotocraftApp, pen: &PenPath, closed: bool) {
    if pen.knots.len() < 2 {
        return;
    }
    let path = pen_to_json(pen, closed);
    let r = if app.ui.tool_options.vector_mode == "shape" {
        let fill = if closed { fill_param(app) } else { Value::Null };
        let stroke = if closed {
            stroke_param(app)
        } else {
            let mut stroke = app.ui.tool_options.shape_stroke.params();
            stroke["width"] = json!(app.ui.tool_options.stroke_width.max(1.0));
            stroke["color"] = json!(app.session.tools.foreground);
            stroke
        };
        app.run("shape.create", json!({"kind": "path", "path": path, "fill": fill, "stroke": stroke}))
    } else if let Some((name, p)) =
        pen.resumes.as_ref().and_then(|r| app.session.active().and_then(|st| with_resumed_subpath(&st.doc, r, &path)).map(|p| (r.path.clone(), p)))
    {
        // The resumed subpath, carried on, takes its old place: one step, no second subpath.
        app.run("path.set", json!({"name": name, "path": p})).inspect(|_| app.ui.selected_path = Some(name))
    } else if let Some((id, existing)) = targeted_vector_mask(app) {
        // A targeted vector mask takes the new subpath (#196), as in Photoshop.
        let mut p = photocraft_engine::vector_cmds::path_json(&existing);
        if let (Some(subs), Some(new)) = (p.get_mut("subpaths").and_then(Value::as_array_mut), path.get("subpaths").and_then(Value::as_array)) {
            subs.extend(new.iter().cloned());
        }
        app.run("layer.vectorMask.edit", json!({"layer": id, "path": p}))
    } else {
        // Photoshop adds the new subpath to the path selected in the Paths panel, which is the
        // Work Path right after drawing one, so path after path builds up there (#4721). With no
        // path selected, a new Work Path replaces the old one. The drawn path ends up selected.
        // A resumed draft whose subpath changed meanwhile joins the path it came from, as a new
        // subpath, even with nothing selected in the Paths panel.
        let exists = |n: &String| app.session.active().is_some_and(|st| path_exists(&st.doc, n));
        let resumed_from = pen.resumes.as_ref().filter(|r| app.session.active().is_some_and(|st| st.doc.id.0 == r.doc)).map(|r| r.path.clone());
        let target = resumed_from.filter(exists).or_else(|| app.ui.selected_path.clone().filter(exists));
        let name = target.clone().unwrap_or_else(|| "work".into());
        let mut params = json!({"name": name, "path": path});
        if target.is_some() {
            params["op"] = json!("combine");
        }
        app.run("path.set", params).inspect(|_| app.ui.selected_path = Some(name))
    };
    if let Err(e) = r {
        app.ui.status = e;
        app.ui.status_error = true;
    }
}

/// Does `doc` have the path the Paths panel calls `name`: "work", or a saved path's name?
fn path_exists(doc: &Document, name: &str) -> bool {
    doc_path(doc, name).is_some()
}

/// The path the Paths panel calls `name`: "work", or a saved path's name.
fn doc_path<'a>(doc: &'a Document, name: &str) -> Option<&'a Path> {
    if name == "work" { doc.work_path.as_ref() } else { doc.paths.iter().find(|p| p.name == name).map(|p| &p.path) }
}

/// The open subpath of the path a new Pen path would join (the one selected in the Paths panel,
/// else the work path) that has its first or last anchor at `(x, y)`, as a Pen path to carry on
/// drawing from there (#6593). Its knots run towards that anchor: reversed, handles swapped, when
/// it is the first. Shape mode and a targeted vector mask take new paths, so they resume nothing.
fn resume_at(app: &PhotocraftApp, x: f64, y: f64, tol: f64) -> Option<PenPath> {
    if app.ui.tool_options.vector_mode == "shape" || targeted_vector_mask(app).is_some() {
        return None;
    }
    let st = app.session.active()?;
    let doc = st.doc.id.0;
    let name = app.ui.selected_path.clone().filter(|n| path_exists(&st.doc, n)).unwrap_or_else(|| "work".into());
    let near = |k: &photocraft_doc::Knot| (k.anchor.x - x).hypot(k.anchor.y - y) < tol;
    doc_path(&st.doc, &name)?.subpaths.iter().enumerate().filter(|(_, s)| !s.closed).find_map(|(i, s)| {
        let reversed = if s.knots.last().is_some_and(near) {
            false
        } else if s.knots.first().is_some_and(near) {
            true
        } else {
            return None;
        };
        let mut knots: Vec<_> = s.knots.iter().map(|k| (knot_points(k), k.smooth)).collect();
        if reversed {
            knots.reverse();
            for (k, _) in &mut knots {
                k.swap(1, 2);
            }
        }
        let unlinked = knots.iter().enumerate().filter(|(_, (_, smooth))| !smooth).map(|(i, _)| i).collect();
        let resumes = Resumed { doc, path: name.clone(), index: i, was: s.clone() };
        Some(PenPath { knots: knots.into_iter().map(|(k, _)| k).collect(), unlinked, resumes: Some(resumes), ..Default::default() })
    })
}

/// The resumed path with its subpath carried on: the one subpath of `drawn` takes the old one's
/// anchors and closing, which keeps its other settings (its path operation). `None` when the
/// subpath is no longer the one drawing resumed (another document, or the path changed).
fn with_resumed_subpath(doc: &Document, r: &Resumed, drawn: &Value) -> Option<Value> {
    let path = doc_path(doc, &r.path).filter(|_| doc.id.0 == r.doc)?;
    if path.subpaths.get(r.index) != Some(&r.was) {
        return None;
    }
    let mut p = photocraft_engine::vector_cmds::path_json(path);
    let slot = p.get_mut("subpaths")?.as_array_mut()?.get_mut(r.index)?.as_object_mut()?;
    let new = drawn.get("subpaths")?.get(0)?;
    for key in ["closed", "knots"] {
        slot.insert(key.into(), new.get(key)?.clone());
    }
    Some(p)
}

// ---------------------------------------------------------------------------------------------
// Freeform Pen and Curvature Pen

/// Freeform Pen release: fit the drag's trace within the options bar's Curve Fit and commit it
/// as the Pen does. Releasing on the start point closes the path; a click draws nothing. The
/// points at indices `straight` were ⌥-clicked: the segment into each one stays straight.
pub fn freeform_finish(app: &mut PhotocraftApp, points: &[[f64; 3]], straight: &[usize]) {
    let trace: Vec<[f64; 2]> = points.iter().map(|p| [p[0], p[1]]).collect();
    let (Some(first), Some(last)) = (trace.first(), trace.last()) else { return };
    let tol = 6.0 / app.point_zoom().max(0.01) as f64;
    let length: f64 = trace.windows(2).map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1])).sum();
    let closed = (first[0] - last[0]).hypot(first[1] - last[1]) < tol && length > 4.0 * tol;
    let fit = f64::from(app.ui.tool_options.curve_fit);
    let pen = if straight.is_empty() {
        let Some(sub) = photocraft_vector::trace::fit_trace(&trace, fit, closed) else { return };
        let knots = sub.knots.iter().map(knot_points).collect();
        let unlinked = sub.knots.iter().enumerate().filter(|(_, k)| !k.smooth).map(|(i, _)| i).collect();
        (PenPath { knots, unlinked, ..Default::default() }, sub.closed)
    } else {
        fit_with_straight_segments(&trace, straight, fit, closed.then_some(tol))
    };
    commit_path(app, &pen.0, pen.1);
}

fn knot_points(k: &photocraft_doc::Knot) -> [[f64; 2]; 3] {
    [[k.anchor.x, k.anchor.y], [k.in_ctrl.x, k.in_ctrl.y], [k.out_ctrl.x, k.out_ctrl.y]]
}

/// A Freeform Pen trace with ⌥-clicked straight segments (into the points at `straight`): each
/// freehand run between them is fitted on its own, and the runs are joined by straight lines
/// between corner anchors. `close` (a reach) closes the path when it ends on its start point.
fn fit_with_straight_segments(trace: &[[f64; 2]], straight: &[usize], fit: f64, close: Option<f64>) -> (PenPath, bool) {
    let mut cuts: Vec<usize> = straight.iter().copied().filter(|&i| i > 0 && i < trace.len()).collect();
    cuts.sort_unstable();
    cuts.dedup();
    cuts.push(trace.len());
    let (mut knots, mut corners): (Vec<[[f64; 2]; 3]>, Vec<usize>) = (Vec::new(), Vec::new());
    let mut start = 0;
    for end in cuts {
        let run = trace.get(start..end).unwrap_or_default();
        let first = knots.len();
        match photocraft_vector::trace::fit_trace(run, fit, false) {
            Some(sub) => {
                corners.extend(sub.knots.iter().enumerate().filter(|(_, k)| !k.smooth).map(|(i, _)| first + i));
                knots.extend(sub.knots.iter().map(knot_points));
            }
            // A single click (or a run that never moved): one corner anchor.
            None => knots.extend(run.first().map(|p| [*p; 3])),
        }
        // Where a straight segment meets the run: a corner with no handle on that side.
        if let Some(k) = knots.get_mut(first).filter(|_| first > 0) {
            k[1] = k[0];
            corners.push(first);
        }
        if end < trace.len()
            && let Some(k) = knots.last_mut()
        {
            k[2] = k[0];
            corners.push(knots.len() - 1);
        }
        start = end;
    }
    // Ended on the start point: the last anchor is the first one.
    let ends_on_start = |k: &[[[f64; 2]; 3]], reach: f64| match (k.first(), k.last()) {
        (Some(a), Some(b)) => k.len() > 3 && (a[0][0] - b[0][0]).hypot(a[0][1] - b[0][1]) < reach,
        _ => false,
    };
    let closed = close.is_some_and(|reach| ends_on_start(&knots, reach));
    if closed && let Some(last) = knots.pop() {
        if let Some(k) = knots.first_mut() {
            k[1] = [last[1][0] - last[0][0] + k[0][0], last[1][1] - last[0][1] + k[0][1]];
        }
        corners.retain(|&i| i < knots.len());
        corners.push(0);
    }
    corners.sort_unstable();
    corners.dedup();
    (PenPath { knots, unlinked: corners, ..Default::default() }, closed)
}

/// Recompute a Curvature Pen path's handles from its anchors and corner points.
fn curvature_refresh(pen: &mut PenPath, closed: bool) {
    let anchors: Vec<[f64; 2]> = pen.knots.iter().map(|k| k[0]).collect();
    let corners: Vec<bool> = (0..anchors.len()).map(|i| pen.unlinked.contains(&i)).collect();
    pen.knots = photocraft_vector::edit::curvature_knots(&anchors, &corners, closed);
}

/// The anchor of the Curvature Pen path being drawn under (x, y), for the cursor.
pub(crate) fn curvature_anchor_at(app: &PhotocraftApp, x: f64, y: f64) -> Option<usize> {
    curvature_hit(app, x, y)
}

/// The Curvature Pen's anchor within a few screen pixels of (x, y).
fn curvature_hit(app: &PhotocraftApp, x: f64, y: f64) -> Option<usize> {
    let tol = 6.0 / app.point_zoom().max(0.01) as f64;
    let pen = app.ui.pen.as_ref().filter(|p| p.curvature)?;
    pen.knots
        .iter()
        .enumerate()
        .map(|(i, k)| (i, (k[0][0] - x).hypot(k[0][1] - y)))
        .filter(|(_, d)| *d < tol)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// Curvature Pen press: clicking the first anchor closes the path, pressing another anchor
/// grabs it to move, anywhere else adds a smooth anchor (still draggable until the release).
pub fn curvature_down(app: &mut PhotocraftApp, x: f64, y: f64) {
    if !cancel_stale(app) {
        return;
    }
    if app.ui.pen.as_ref().is_some_and(|p| !p.curvature) {
        pen_commit(app, false);
    }
    let hit = curvature_hit(app, x, y);
    if hit == Some(0) && app.ui.pen.as_ref().is_some_and(|p| p.knots.len() >= 3) {
        pen_commit(app, true);
        return;
    }
    if app.ui.pen.is_none() {
        app.ui.pen_doc = app.session.active().map(|st| st.doc.id);
    }
    let pen = app.ui.pen.get_or_insert_with(|| PenPath { curvature: true, ..Default::default() });
    if hit.is_none() {
        pen.knots.push([[x, y]; 3]);
    }
    pen.moving = hit.or(pen.knots.len().checked_sub(1));
    curvature_refresh(pen, false);
}

pub fn curvature_move(app: &mut PhotocraftApp, x: f64, y: f64) {
    if let Some(pen) = app.ui.pen.as_mut().filter(|p| p.curvature)
        && let Some(k) = pen.moving.and_then(|i| pen.knots.get_mut(i))
    {
        k[0] = [x, y];
        curvature_refresh(pen, false);
    }
}

pub fn curvature_up(app: &mut PhotocraftApp) {
    if let Some(pen) = app.ui.pen.as_mut() {
        pen.moving = None;
    }
}

/// Curvature Pen double-click: toggle the anchor under the pointer between smooth and corner
/// (double-clicking empty canvas therefore places a corner point, as in Photoshop). With no path
/// being drawn, an anchor of the active path toggles the same way and its subpath reflows
/// (`path.convertPoint` with `curvature`), so a Pen-drawn corner rounds off.
pub fn curvature_toggle(app: &mut PhotocraftApp, x: f64, y: f64) {
    if app.ui.pen.as_ref().is_none_or(|p| p.knots.is_empty()) {
        crate::direct_select::curvature_toggle_active(app, [x, y]);
        return;
    }
    let Some(i) = curvature_hit(app, x, y) else { return };
    let Some(pen) = app.ui.pen.as_mut() else { return };
    if let Some(at) = pen.unlinked.iter().position(|&c| c == i) {
        pen.unlinked.remove(at);
    } else {
        pen.unlinked.push(i);
    }
    curvature_refresh(pen, false);
}

/// The path that path commands act on without a name: the one selected in the Paths panel, else
/// the work path, else the selected layer's shape path or vector mask; `None` when there's none.
pub fn active_path_name(app: &PhotocraftApp) -> Option<String> {
    let st = app.session.active()?;
    let rows = path_rows(&st.doc, st.active_layer);
    let key = |r: &PathEntry| match r.kind {
        PathRow::Work => "work".to_string(),
        PathRow::Layer => "layer".to_string(),
        PathRow::Saved => r.name.clone(),
    };
    let selected = app.ui.selected_path.as_deref().filter(|s| rows.iter().any(|r| key(r) == *s)).map(str::to_string);
    selected.or_else(|| rows.iter().find(|r| r.kind == PathRow::Work).map(key)).or_else(|| rows.iter().find(|r| r.kind == PathRow::Layer).map(key))
}

/// The commands that create a fill or adjustment layer and take a `"path"` as its vector mask.
pub fn takes_path_mask(id: &str) -> bool {
    id.starts_with("layer.newFillLayer.") || id.starts_with("layer.newAdjustmentLayer.")
}

/// The work or saved path explicitly selected in the Paths panel (not a layer's own path).
pub fn selected_document_path(app: &PhotocraftApp) -> Option<String> {
    let st = app.session.active()?;
    let sel = app.ui.selected_path.as_deref()?;
    let listed = path_rows(&st.doc, st.active_layer).iter().any(|r| match r.kind {
        PathRow::Work => sel == "work",
        PathRow::Saved => r.name == sel,
        PathRow::Layer => false,
    });
    listed.then(|| sel.to_string())
}

/// New fill and adjustment layers take the path selected in the Paths panel as their vector mask,
/// as in Photoshop (#1419): the command gets it as `"path"` unless the caller set one (`null`
/// opts out). Only an explicitly selected work or saved path counts; deselect it by clicking the
/// panel's empty area.
pub fn with_active_path(app: &PhotocraftApp, id: &str, params: Value) -> Value {
    if !takes_path_mask(id) || params.get("path").is_some() {
        return params;
    }
    let Some(sel) = selected_document_path(app) else { return params };
    match params {
        Value::Object(mut m) => {
            m.insert("path".into(), json!(sel));
            Value::Object(m)
        }
        _ => json!({ "path": sel }),
    }
}

/// ⌘↩ / Ctrl+Enter (#306): load a path as a selection, like Photoshop with a Pen or Path
/// Selection tool or a path selected in the Paths panel. A path being drawn with the Pen is
/// finished first and loads; otherwise [`active_path_name`]. `params` may set feather, mode…
pub fn path_to_selection(app: &mut PhotocraftApp, params: Value) -> Result<Value, String> {
    if app.ui.pen.as_ref().is_some_and(|p| p.knots.len() >= 2) {
        // The finished path becomes the work path, or a shape layer's path or a vector mask.
        let on_layer = app.ui.tool_options.vector_mode == "shape" || targeted_vector_mask(app).is_some();
        pen_commit(app, false);
        app.ui.selected_path = Some(if on_layer { "layer" } else { "work" }.into());
    }
    let name = active_path_name(app)
        .ok_or_else(|| tl!("No path to make a selection from: draw one with the Pen tool or select one in the Paths panel.").to_string())?;
    let mut p = params.as_object().cloned().unwrap_or_default();
    p.insert("name".into(), json!(name));
    app.run("path.toSelection", Value::Object(p))
}

// ---------------------------------------------------------------------------------------------
// Path Selection

/// What Path Selection edits.
enum PathTarget {
    Shape(u64),
    /// The active layer's vector mask, when its Layers thumbnail is targeted (#196).
    VectorMask(u64),
    Work,
}

/// The targeted vector mask of the active layer, if the Layers panel targets it.
pub(crate) fn targeted_vector_mask(app: &PhotocraftApp) -> Option<(u64, Path)> {
    let st = app.session.active()?;
    let l = st.active_layer.and_then(|id| st.doc.layer(id))?;
    let targeted = app.ui.vector_mask_target || st.channel_view.target == photocraft_engine::channel_cmds::ChannelTarget::VectorMask;
    (targeted && !matches!(l.content, LayerContent::Shape(_))).then_some(())?;
    l.vector_mask.as_ref().map(|m| (l.id.0, m.path.clone()))
}

/// The active shape layer's path, if the active layer is a shape layer.
pub(crate) fn active_shape_path(app: &PhotocraftApp) -> Option<(u64, Path)> {
    let st = app.session.active()?;
    let l = st.active_layer.and_then(|id| st.doc.layer(id))?;
    match &l.content {
        LayerContent::Shape(sh) => Some((l.id.0, sh.path.clone())),
        _ => None,
    }
}

/// Does ⌫ / Delete delete the work path? With the Pen, Freeform Pen or Path Selection tool, no
/// path being drawn and the work path the one these tools show and edit (no shape layer or
/// targeted vector mask in front of it), as in Photoshop (#5702). It was drawn whenever one of
/// these tools was active, but the key reached Edit › Clear, so it looked impossible to delete.
/// Direct Selection and the Curvature Pen delete selected anchors in Photoshop, not the path.
pub(crate) fn delete_key_deletes_work_path(app: &PhotocraftApp) -> bool {
    matches!(app.ui.tool, Tool::Pen | Tool::FreeformPen | Tool::PathSelection)
        && app.ui.pen.is_none()
        && app.ui.transform.is_none()
        && app.ui.view.shows(app.ui.view.show.target_path)
        && matches!(target_path(app), Some((PathTarget::Work, _)))
}

/// The path Path Selection edits: the targeted vector mask, the active shape layer's path, else
/// the work path.
fn target_path(app: &PhotocraftApp) -> Option<(PathTarget, Path)> {
    if let Some((id, p)) = targeted_vector_mask(app) {
        return Some((PathTarget::VectorMask(id), p));
    }
    if let Some((id, p)) = active_shape_path(app) {
        return Some((PathTarget::Shape(id), p));
    }
    app.session.active()?.doc.work_path.clone().map(|p| (PathTarget::Work, p))
}

/// Edit › Free Transform Path (⌘T with Path Selection or Direct Selection): the path the
/// box transforms (the one Path Selection edits) and `path.transform`'s params that name it.
pub(crate) fn free_transform_path(app: &PhotocraftApp) -> Option<(Value, Path)> {
    if !matches!(app.ui.tool, Tool::PathSelection | Tool::DirectSelection) {
        return None;
    }
    let (target, path) = target_path(app)?;
    path.control_bounds()?;
    let params = match target {
        PathTarget::Shape(id) | PathTarget::VectorMask(id) => json!({"name": "layer", "layer": id}),
        PathTarget::Work => json!({"name": "work"}),
    };
    Some((params, path))
}

fn point_in_subpath(sp: &photocraft_doc::vector::Subpath, pt: [f64; 2]) -> bool {
    if !sp.closed || sp.knots.len() < 3 {
        return false;
    }
    let mut inside = false;
    let n = sp.knots.len();
    for i in 0..n {
        let j = (i + 1) % n;
        let xi = sp.knots[i].anchor.x;
        let yi = sp.knots[i].anchor.y;
        let xj = sp.knots[j].anchor.x;
        let yj = sp.knots[j].anchor.y;
        let intersect = ((yi > pt[1]) != (yj > pt[1])) && (pt[0] < (xj - xi) * (pt[1] - yi) / (yj - yi) + xi);
        if intersect {
            inside = !inside;
        }
    }
    inside
}

/// The topmost visible shape layer under `at` (document coordinates):
/// hits the shape path or anchor within `tol`, or any non-transparent pixel of the shape's raster surface,
/// or point inside the shape's filled geometry.
pub fn pick_shape_at(doc: &Document, at: [f64; 2], tol: f64) -> Option<photocraft_doc::LayerId> {
    let (x, y) = (at[0].floor() as i32, at[1].floor() as i32);
    for (path, _, l) in doc.walk().iter().rev() {
        if !l.visible || !(1..=path.len()).all(|n| doc.layer_at(&path[..n]).is_some_and(|a| a.visible)) {
            continue;
        }
        let board = path.first().and_then(|i| doc.layers.get(*i)).and_then(|t| t.artboard());
        if board.is_some_and(|a| !a.rect.contains(x, y)) {
            continue;
        }
        if let LayerContent::Shape(sh) = &l.content {
            if photocraft_vector::edit::hit(&sh.path, photocraft_geom::Point::new(at[0], at[1]), tol, &[]).is_some() {
                return Some(l.id);
            }
            if sh.fill.is_some() {
                if let Some(surf) = l.surface() {
                    let mut px = [[0.0f32; 4]; 1];
                    surf.read_rgba_into(photocraft_doc::Rect::from_xywh(x, y, 1, 1), &mut px);
                    if px[0][3] > 0.0 {
                        return Some(l.id);
                    }
                }
                if let Some(photocraft_doc::LiveShape::Rect { rect, .. }) = &sh.live {
                    let x0 = rect[0].min(rect[0] + rect[2]);
                    let x1 = rect[0].max(rect[0] + rect[2]);
                    let y0 = rect[1].min(rect[1] + rect[3]);
                    let y1 = rect[1].max(rect[1] + rect[3]);
                    if at[0] >= x0 && at[0] <= x1 && at[1] >= y0 && at[1] <= y1 {
                        return Some(l.id);
                    }
                }
                if let Some(photocraft_doc::LiveShape::Ellipse { rect }) = &sh.live {
                    let rx = rect[2].abs() / 2.0;
                    let ry = rect[3].abs() / 2.0;
                    if rx > 1e-6 && ry > 1e-6 {
                        let cx = rect[0] + rect[2] / 2.0;
                        let cy = rect[1] + rect[3] / 2.0;
                        let dx = (at[0] - cx) / rx;
                        let dy = (at[1] - cy) / ry;
                        if dx * dx + dy * dy <= 1.0 {
                            return Some(l.id);
                        }
                    }
                }
                if sh.path.subpaths.iter().any(|sp| point_in_subpath(sp, at)) {
                    return Some(l.id);
                }
            }
        }
    }
    None
}

pub fn path_selection_finish(app: &mut PhotocraftApp, start: [f64; 2], end: [f64; 2]) {
    let (dx, dy) = (end[0] - start[0], end[1] - start[1]);
    let tol = 6.0 / f64::from(app.point_zoom().max(0.01));
    if dx.abs() + dy.abs() < 0.5 {
        let Some(st) = app.session.active() else { return };
        if let Some(id) = pick_shape_at(&st.doc, start, tol) {
            let _ = app.run("layer.select", json!({"layer": id.0}));
        }
        return;
    }
    if target_path(app).is_none() {
        let Some(st) = app.session.active() else { return };
        if let Some(id) = pick_shape_at(&st.doc, start, tol) {
            let _ = app.run("layer.select", json!({"layer": id.0}));
        }
    }
    let Some((target, path)) = target_path(app) else { return };
    let _ = match target {
        PathTarget::Shape(id) => app.run("shape.edit", json!({"layer": id, "move": [dx.round(), dy.round()]})),
        PathTarget::VectorMask(id) => {
            let moved = path.transform(&Affine::translate(dx.round(), dy.round()));
            app.run("layer.vectorMask.edit", json!({"layer": id, "path": photocraft_engine::vector_cmds::path_json(&moved)}))
        }
        PathTarget::Work => {
            let moved = path.transform(&Affine::translate(dx.round(), dy.round()));
            app.run("path.set", json!({"name": "work", "path": photocraft_engine::vector_cmds::path_json(&moved)}))
        }
    };
}

// ---------------------------------------------------------------------------------------------
// Drawing

fn bezier(p0: [f64; 2], c0: [f64; 2], c1: [f64; 2], p1: [f64; 2], t: f64) -> [f64; 2] {
    let u = 1.0 - t;
    let f = |a: f64, b: f64, c: f64, d: f64| u * u * u * a + 3.0 * u * u * t * b + 3.0 * u * t * t * c + t * t * t * d;
    [f(p0[0], c0[0], c1[0], p1[0]), f(p0[1], c0[1], c1[1], p1[1])]
}

/// `q` lies on the chord from `a` to `d` (within a thousandth of a pixel).
fn on_chord(a: [f64; 2], q: [f64; 2], d: [f64; 2]) -> bool {
    let (dx, dy, vx, vy) = (d[0] - a[0], d[1] - a[1], q[0] - a[0], q[1] - a[1]);
    let len = dx.hypot(dy);
    if len < 1e-9 {
        return vx.hypot(vy) < 1e-3;
    }
    let t = (vx * dx + vy * dy) / (len * len);
    (vx * dy - vy * dx).abs() / len < 1e-3 && (-1e-9..=1.0 + 1e-9).contains(&t)
}

/// How far (in screen pixels) a drawn chord may stray from the true curve.
const CURVE_TOLERANCE: f64 = 0.3;
/// Pieces per segment at most: 0.3 px holds for curves bending up to about 1.4 million px on screen.
const MAX_CURVE_PIECES: usize = 1024;

/// Screen points along one cubic segment (both ends included). The control points are mapped to
/// the screen first (`xf` is affine, so the curve maps exactly), and the number of chords follows
/// how much the curve bends there: a straight segment is one chord, and a curve zoomed in stays
/// smooth instead of showing a fixed 16 chords (#4084). Uses the bound
/// `max|B''| / 8n² <= tol` with `max|B''| <= 6·max(|p0−2c0+c1|, |c0−2c1+p1|)`.
pub(crate) fn curve_points(p0: [f64; 2], c0: [f64; 2], c1: [f64; 2], p1: [f64; 2], xf: &dyn Fn([f64; 2]) -> Pos2) -> Vec<Pos2> {
    let [a, b, c, d] = [p0, c0, c1, p1].map(|q| {
        let s = xf(q);
        [f64::from(s.x), f64::from(s.y)]
    });
    let second = |p: [f64; 2], q: [f64; 2], r: [f64; 2]| (p[0] - 2.0 * q[0] + r[0]).hypot(p[1] - 2.0 * q[1] + r[1]);
    let n = (0.75 * second(a, b, c).max(second(b, c, d)) / CURVE_TOLERANCE).sqrt().ceil();
    // A straight segment (controls on the chord, like the Pen's retracted handles) is one chord;
    // NaN (a non-finite point) falls back to one too.
    let n = if n.is_finite() && !(on_chord(a, b, d) && on_chord(a, c, d)) { (n as usize).clamp(1, MAX_CURVE_PIECES) } else { 1 };
    (0..=n)
        .map(|k| {
            let q = bezier(a, b, c, d, k as f64 / n as f64);
            pos2(q[0] as f32, q[1] as f32)
        })
        .collect()
}

/// Screen polylines for a path (one per subpath).
pub(crate) fn path_lines(path: &Path, xf: &dyn Fn([f64; 2]) -> Pos2) -> Vec<(Vec<Pos2>, bool)> {
    path.subpaths
        .iter()
        .map(|s| {
            let n = s.knots.len();
            let mut pts = Vec::new();
            let segs = if s.closed { n } else { n.saturating_sub(1) };
            for i in 0..segs {
                let (a, b) = (&s.knots[i], &s.knots[(i + 1) % n]);
                pts.extend(curve_points([a.anchor.x, a.anchor.y], [a.out_ctrl.x, a.out_ctrl.y], [b.in_ctrl.x, b.in_ctrl.y], [b.anchor.x, b.anchor.y], xf));
            }
            if pts.is_empty()
                && let Some(k) = s.knots.first()
            {
                pts.push(xf([k.anchor.x, k.anchor.y]));
            }
            (pts, s.closed)
        })
        .collect()
}

/// A path's outline in the accent colour.
pub(crate) fn draw_outline(painter: &egui::Painter, p: &Path, to_scr: &dyn Fn([f64; 2]) -> Pos2, accent: Color32) {
    for (pts, closed) in path_lines(p, to_scr) {
        if closed {
            painter.add(egui::Shape::closed_line(pts, Stroke::new(1.0, accent)));
        } else {
            painter.add(egui::Shape::line(pts, Stroke::new(1.0, accent)));
        }
    }
}

/// An anchor point's square: filled with the accent when selected, else hollow (white).
pub(crate) fn draw_anchor(painter: &egui::Painter, c: Pos2, selected: bool, accent: Color32) {
    let r = Rect::from_center_size(c, vec2(6.0, 6.0));
    painter.rect_filled(r, 0.0, if selected { accent } else { Color32::WHITE });
    painter.rect_stroke(r, 0.0, Stroke::new(1.0, accent), egui::StrokeKind::Inside);
}

/// A direction handle: the line from its anchor and the round handle end.
pub(crate) fn draw_handle(painter: &egui::Painter, anchor: Pos2, handle: Pos2, accent: Color32) {
    painter.line_segment([anchor, handle], Stroke::new(1.0, accent));
    painter.circle_filled(handle, 3.0, accent);
}

/// Does `tool` draw the anchor squares of the paths it can edit? Path Selection does, and so do
/// the Pen tools: as in Photoshop, a path finished with Enter or by closing it keeps its anchors
/// on screen, so it can still be edited (⌘/Ctrl-drag, Auto Add/Delete) (#6884, #4716).
pub(crate) fn shows_anchors(tool: Tool) -> bool {
    // Add/Delete Anchor Point and Convert Point draw Direct Selection's overlay instead.
    matches!(tool, Tool::PathSelection | Tool::Pen | Tool::FreeformPen | Tool::CurvaturePen)
}

/// The saved path selected in the Paths panel: Photoshop's target path, drawn on the canvas with
/// any tool (#6550). `None` when the panel selects the work path, a layer's path or nothing.
pub(crate) fn selected_saved_path(app: &PhotocraftApp) -> Option<&Path> {
    let sel = app.ui.selected_path.as_deref().filter(|s| !matches!(*s, "work" | "layer"))?;
    app.session.active()?.doc.paths.iter().find(|p| p.name == sel).map(|p| &p.path)
}

/// Work path / active shape path / selected saved path outlines, anchors, and the pen path in
/// progress.
pub fn draw_overlay(app: &PhotocraftApp, painter: &egui::Painter, xf: &ViewXform, doc: &Document) {
    let tool = app.ui.tool;
    let accent = Tokens::get(painter.ctx()).accent;
    let to_scr = |q: [f64; 2]| xf.to_screen(q[0] as f32, q[1] as f32);
    let draw_path = |p: &Path, anchors: bool| {
        draw_outline(painter, p, &to_scr, accent);
        if anchors {
            for k in p.subpaths.iter().flat_map(|s| &s.knots) {
                draw_anchor(painter, to_scr([k.anchor.x, k.anchor.y]), false, accent);
            }
        }
    };
    // View › Show › Target Path (under Extras) hides the paths; the Pen's path in progress stays.
    // Free Transform Path draws the path through its box instead (transform_tool).
    let paths = app.ui.view.shows(app.ui.view.show.target_path) && app.ui.transform.as_ref().is_none_or(|t| t.path.is_none());
    if app.session.active().is_some_and(|st| st.doc.id == doc.id) && crate::direct_select::shows(app, painter.ctx().input(|i| i.modifiers)) {
        // Direct Selection (or the Pen with ⌘/Ctrl held) draws the paths it edits (#790).
        crate::direct_select::draw_overlay(app, painter, &to_scr, accent, paths);
        if paths {
            // The selected saved path shows too, though Direct Selection doesn't edit it yet.
            if let Some(p) = selected_saved_path(app) {
                draw_path(p, true);
            }
            crate::anchor_tools::draw_hover(app, painter, &to_scr, accent, painter.ctx().input(|i| i.modifiers));
        }
    } else if paths {
        // The shape's or vector mask's path stays visible as an outline with any tool. Path
        // Selection and the Pen tools add anchor squares; Direct Selection has its own overlay
        // above. The Work Path is drawn only while it is the path selected in the Paths panel or
        // a path tool is active, as in Photoshop: a Work Path left unselected (one a PSD arrived
        // with, or after Ctrl-click Load as Selection) would otherwise cover the marching ants (#6551).
        let anchors = shows_anchors(tool);
        if let Some(wp) = &doc.work_path
            && (anchors || app.ui.selected_path.as_deref() == Some("work"))
        {
            draw_path(wp, anchors);
        }
        // The saved path selected in the Paths panel is the target path (#6550).
        if let Some(p) = selected_saved_path(app) {
            draw_path(p, anchors);
        }
        let st = app.session.active();
        if let Some(l) = st.and_then(|s| s.active_layer.and_then(|id| s.doc.layer(id)))
            && let LayerContent::Shape(sh) = &l.content
        {
            draw_path(&sh.path, anchors);
        }
        // A targeted vector mask shows its path with any tool (#196).
        if let Some((_, p)) = targeted_vector_mask(app) {
            draw_path(&p, anchors);
        }
        crate::corner_radius::draw(app, painter, &to_scr, accent);
    }
    // Pen path in progress, with handles of the last knot and a rubber band to the pointer. While
    // ⌘/Ctrl drags it, Direct Selection draws its preview instead.
    let edited = matches!(&app.ui.direct_selection.drag, Some(crate::direct_select::Drag::Edit { target: crate::direct_select::PathRef::Pen, .. }));
    // Only on the document it is drawn on: a view of another document never shows it (#5373).
    if let Some(pen) = app.ui.pen.as_ref().filter(|_| !edited).filter(|_| app.ui.pen_doc.is_none_or(|d| d == doc.id)) {
        let mut pts = Vec::new();
        // A close-drag shows the first segment bending with the handle being pulled out.
        let mut knots = pen.knots.clone();
        if closing(pen)
            && let Some(&[a, _, out]) = pen.knots.last()
            && out != a
            && let Some(first) = knots.first_mut()
        {
            first[2] = out;
        }
        for w in knots.windows(2) {
            pts.extend(curve_points(w[0][0], w[0][2], w[1][1], w[1][0], &to_scr));
        }
        painter.add(egui::Shape::line(pts, Stroke::new(1.5, accent)));
        // No rubber band where a click adds or deletes an anchor of this path instead (#6140).
        let auto = app.hover_doc.is_some_and(|h| crate::anchor_tools::pen_auto(app, tool, painter.ctx().input(|i| i.modifiers), h).is_some());
        if let (Some(last), Some(h)) = (pen.knots.last(), app.hover_doc)
            && !pen.dragging
            && pen.moving.is_none()
            && !auto
        {
            if pen.curvature && pen.knots.len() >= 2 {
                // The Curvature Pen previews the curve the next click makes through the pointer.
                let mut next = pen.clone();
                next.knots.push([h; 3]);
                curvature_refresh(&mut next, false);
                if let [.., a, b] = next.knots.as_slice() {
                    let pts = curve_points(a[0], a[2], b[1], b[0], &to_scr);
                    painter.add(egui::Shape::dashed_line(&pts, Stroke::new(1.0, accent), 4.0, 3.0));
                }
            } else {
                // The rubber band ends where a ⇧-click would put the next anchor.
                let shift = painter.ctx().input(|i| i.modifiers.shift);
                let h = if pen.curvature { h } else { shift_45(last[0], h, shift) };
                painter.add(egui::Shape::dashed_line(&[to_scr(last[0]), to_scr(h)], Stroke::new(1.0, accent), 4.0, 3.0));
            }
        }
        for (i, k) in pen.knots.iter().enumerate() {
            let c = to_scr(k[0]);
            let last = i + 1 == pen.knots.len();
            if last && k[2] != k[0] && !pen.curvature {
                for hnd in [k[1], k[2]] {
                    draw_handle(painter, c, to_scr(hnd), accent);
                }
            }
            draw_anchor(painter, c, last, accent);
        }
        // Hovering the first anchor: Photoshop's close cue, a small ring beside the pointer.
        let tol = 6.0 / f64::from(app.point_zoom().max(0.01));
        if let Some(h) = app.hover_doc
            && over_first_anchor(pen, h, tol)
            && let Some(first) = pen.knots.first()
        {
            let c = to_scr(first[0]);
            painter.rect_filled(Rect::from_center_size(c, vec2(8.0, 8.0)), 0.0, accent);
            painter.circle_stroke(c + vec2(11.0, 11.0), 3.5, Stroke::new(1.5, accent));
        }
    }
    // The segment the Pen's Auto Add/Delete would add an anchor to, or the anchor it would delete,
    // over the path being drawn. While drawing, the other paths can be hidden: it edits none of them.
    if paths || app.ui.pen.is_some() {
        crate::anchor_tools::draw_pen_hover(app, painter, &to_scr, accent, painter.ctx().input(|i| i.modifiers));
    }
}

// ---------------------------------------------------------------------------------------------
// Options bars

/// Options bar for vector tools; false for other tools.
pub fn options_bar(app: &mut PhotocraftApp, ui: &mut egui::Ui, tool: Tool) -> bool {
    if !(is_shape_tool(tool)
        || matches!(
            tool,
            Tool::Pen | Tool::FreeformPen | Tool::CurvaturePen | Tool::AddAnchorPoint | Tool::DeleteAnchorPoint | Tool::PathSelection | Tool::DirectSelection
        ))
    {
        return false;
    }
    let t = Tokens::get(ui.ctx());
    let lbl = |ui: &mut egui::Ui, s: &str| {
        let label = ui.label(egui::RichText::new(s).color(t.text_dim).size(12.0));
        // The caption scrubs and names the control drawn right after it.
        crate::widgets::scrub_option_label(ui, &label);
    };
    let o = &mut app.ui.tool_options;
    let target = selected_shape(&app.session);
    if tool == Tool::PathSelection && target.is_none() {
        lbl(ui, if app.ui.vector_mask_target { "Drag to move the targeted vector mask" } else { tl!("Drag to move the active shape's path or the Work Path") });
        return true;
    }
    if tool == Tool::DirectSelection && target.is_none() {
        let (shift, alt) = (crate::shortcuts::pretty("Shift"), crate::shortcuts::pretty("Alt"));
        lbl(
            ui,
            &crate::i18n::fmt(
                tl!("Click: select anchor · Drag: move anchor, handle or segment · {shift}-click: add · {alt}-click: whole subpath"),
                &[("shift", &shift), ("alt", &alt)],
            ),
        );
        return true;
    }
    if matches!(tool, Tool::AddAnchorPoint | Tool::DeleteAnchorPoint) {
        let hint = if tool == Tool::AddAnchorPoint {
            tl!("Click a segment to add an anchor point · {alt}-click an anchor to delete it")
        } else {
            tl!("Click an anchor point to delete it · {alt}-click a segment to add one")
        };
        lbl(ui, &crate::i18n::fmt(hint, &[("alt", &crate::shortcuts::pretty("Alt"))]));
        return true;
    }
    if matches!(tool, Tool::Pen | Tool::FreeformPen | Tool::CurvaturePen) {
        let opts = [("path".to_string(), tl!("Path")), ("shape".to_string(), tl!("Shape"))];
        crate::widgets::dropdown(ui, "pen-mode", tl!("Mode"), &mut o.vector_mode, &opts, 80.0);
        crate::widgets::vline(ui, 22.0);
        if o.vector_mode == "shape" {
            crate::widgets::value_field(ui, &mut o.stroke_width, 0.0..=288.0, "px", 58.0).accessible_name(tl!("Stroke:"));
            if let Some(change) =
                crate::shape_stroke_ui::button(ui, "pen-stroke", &o.shape_stroke, &mut app.ui.stroke_editor, false, true, None, o.stroke_width > 0.0)
            {
                o.shape_stroke = change.options;
                o.stroke_width = enabled_stroke_width(o.stroke_width);
            }
        }
        if tool == Tool::Pen {
            crate::widgets::vline(ui, 22.0);
            crate::widgets::checkbox(ui, &mut o.pen_auto_add_delete, tl!("Auto Add/Delete"));
        }
        let enter = crate::shortcuts::pretty("Enter");
        match tool {
            Tool::FreeformPen => {
                crate::widgets::vline(ui, 22.0);
                lbl(ui, tl!("Curve Fit:"));
                crate::widgets::value_field(ui, &mut o.curve_fit, 0.5..=10.0, "px", 58.0);
                let alt = crate::shortcuts::pretty("Alt");
                lbl(ui, &crate::i18n::fmt(tl!("Drag: draw freehand · {alt}-click: straight segment · Release on the start point: close"), &[("alt", &alt)]));
            }
            Tool::CurvaturePen => lbl(
                ui,
                &crate::i18n::fmt(
                    tl!("Click: smooth point · Double-click: corner · Drag a point: move · Click first point: close · {key} finish"),
                    &[("key", &enter)],
                ),
            ),
            _ => lbl(
                ui,
                &crate::i18n::fmt(
                    tl!("Click: corner · Drag: smooth · {alt}-drag: break handle · Click or drag first point: close · {key} finish · Esc cancel"),
                    &[("alt", &crate::shortcuts::pretty("Alt")), ("key", &enter)],
                ),
            ),
        }
        // Path mode: Make Selection from the path being drawn or the active path, as Photoshop's
        // options bar's "Make: Selection…" does (#5703).
        if app.ui.tool_options.vector_mode == "path" {
            crate::widgets::vline(ui, 22.0);
            let has_path = active_path_name(app).is_some() || app.ui.pen.as_ref().is_some_and(|p| p.knots.len() >= 2);
            if ui.add_enabled(has_path, egui::Button::new(tl!("Make Selection"))).clicked()
                && let Err(e) = crate::canvas_tool_menu::make_selection(app)
            {
                app.ui.status = e;
                app.ui.status_error = true;
            }
        }
        return true;
    }
    let mut mode = "shape".to_string();
    crate::widgets::dropdown(ui, "shape-mode", tl!("Mode"), &mut mode, &[("shape".to_string(), tl!("Shape"))], 80.0);
    crate::widgets::vline(ui, 22.0);
    crate::shape_options::options_bar(app, ui, tool);
    true
}

/// Whether the active layer supplies the selected-shape options.
fn selected_shape(session: &photocraft_engine::Session) -> Option<photocraft_doc::LayerId> {
    let st = session.active()?;
    let layer = st.active_layer?;
    matches!(st.doc.layer(layer)?.content, LayerContent::Shape(_)).then_some(layer)
}

// ---------------------------------------------------------------------------------------------
// Properties (shape layers)

fn solid_paint(c: [f32; 4]) -> photocraft_doc::Fill {
    photocraft_doc::Fill::Solid(photocraft_doc::Color::rgba(c[0], c[1], c[2], c[3]))
}

pub(crate) fn enabled_stroke_width(width: f32) -> f32 {
    if width.is_finite() && width > 0.0 { width } else { 3.0 }
}

/// One corner of the live rectangle, preserving the other three radii.
/// The engine takes the full [top-left, top-right, bottom-right, bottom-left] array.
fn corner_radii_patch(radii: [f64; 4], corner: usize, radius: f32) -> Value {
    let mut next = radii;
    if let Some(r) = next.get_mut(corner) {
        *r = f64::from(radius.max(0.0));
    }
    json!({"radii": next})
}

/// Follow the existing brush/mask continuous-edit convention: one history state per press or
/// keyboard edit, with every value tick of a held-pointer gesture sharing its key.
pub(crate) fn paint_gesture(ui: &egui::Ui, key: egui::Id) -> u64 {
    let pass = ui.ctx().cumulative_pass_nr();
    let pressed = ui.input(|i| i.pointer.any_pressed());
    ui.data_mut(|d| {
        let gesture = d.get_temp_mut_or_default::<(u64, u64)>(key);
        if pressed && gesture.1 != pass {
            gesture.0 = gesture.0.wrapping_add(1);
            gesture.1 = pass;
        }
        gesture.0
    })
}

/// Properties panel for a shape layer: Appearance (fill, stroke) and live shape geometry.
pub fn shape_properties(app: &mut PhotocraftApp, ui: &mut egui::Ui, id: photocraft_doc::LayerId) {
    if app.session.active().is_some_and(|st| {
        let locks = st.doc.effective_locks(id);
        locks.all || locks.pixels
    }) {
        ui.disable();
    }
    let Some(sh) = app.session.active().and_then(|s| s.doc.layer(id)).and_then(|l| match &l.content {
        LayerContent::Shape(sh) => Some(sh.clone()),
        _ => None,
    }) else {
        return;
    };
    let t = Tokens::get(ui.ctx());
    let mut edit: Option<Value> = None;
    let doc_id = app.session.active().map(|st| st.doc.id);
    let key = |k: &str| format!("shape-{doc_id:?}-{}-{k}", id.0);
    let gesture_key = ui.scope_id().with(key("paint-gesture"));
    let gesture = paint_gesture(ui, gesture_key);
    let coalesce = |field: &str| format!("{}:{}:{gesture}", key(field), gesture_key.value());
    // The shared collapsible section headers (#155).
    // Prefs "Appearance" is theme chrome (Giao diện); shape fill/stroke is Diện mạo.
    if crate::props_layout::section(ui, "appearance", crate::i18n::tr_ctx(crate::i18n::current(), "shape", "Appearance")) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(tl!("Fill")).color(t.text_dim).size(12.0));
            let (fg, bg) = (app.session.tools.foreground, app.session.tools.background);
            let mut gradients = GradientChoices {
                presets: &app.session.presets.gradients,
                current: &app.session.presets.gradient,
                recent: &mut app.ui.recent_gradients,
                fg,
                bg,
            };
            if let Some(change) = swatch(ui, sh.fill.as_ref(), fg, tl!("Set shape fill type"), &mut gradients) {
                let fill = match change {
                    PaintChange::None => Value::Null,
                    PaintChange::Color(color) => json!(color),
                    PaintChange::Gradient(g) => paint_param(&g),
                };
                edit = Some(json!({"fill": fill, "coalesce": coalesce("fill")}));
            }
            ui.add_space(12.0);
            ui.label(egui::RichText::new(tl!("Stroke")).color(t.text_dim).size(12.0));
            if let Some(change) = swatch(ui, sh.stroke.as_ref().map(|s| &s.paint), fg, tl!("Set shape stroke type"), &mut gradients) {
                let width = enabled_stroke_width(sh.stroke.as_ref().map_or(0.0, |s| s.width));
                let stroke = match change {
                    PaintChange::None => Value::Null,
                    PaintChange::Color(color) => json!({"color": color, "width": width}),
                    PaintChange::Gradient(g) => json!({"color": paint_param(&g), "width": width}),
                };
                edit = Some(json!({"stroke": stroke, "coalesce": coalesce("stroke")}));
            }
            let mut w = sh.stroke.as_ref().map_or(0.0, |s| s.width);
            if crate::widgets::value_field(ui, &mut w, 0.0..=288.0, "px", 60.0).accessible_name(tl!("Stroke:")).changed() && (sh.stroke.is_some() || w > 0.0) {
                edit = Some(json!({"stroke": {"width": w}, "coalesce": coalesce("stroke-w")}));
            }
        });
        let style = sh.stroke.as_ref().map(crate::shape_stroke_ui::StrokeOptions::from).unwrap_or_default();
        let stroke_enabled = sh.stroke.as_ref().is_some_and(|stroke| stroke.width > 0.0);
        if let Some(change) = crate::shape_stroke_ui::button(
            ui,
            &key("style"),
            &style,
            &mut app.ui.stroke_editor,
            sh.path.subpaths.iter().all(|s| s.closed),
            !matches!(sh.live, Some(photocraft_doc::LiveShape::Ellipse { .. })),
            app.session.active().map(|st| (st.doc.id, id)),
            stroke_enabled,
        ) {
            let mut stroke = change.options.params();
            if !stroke_enabled {
                stroke["width"] = json!(3.0);
            }
            edit = Some(json!({"stroke": stroke, "coalesce": change.coalesce}));
        }
    }
    if let Some(live) = &sh.live
        && crate::props_layout::section(ui, "liveShape", tl!("Shape"))
    {
        let num = |ui: &mut egui::Ui, label: &str, v: &mut f32, range: std::ops::RangeInclusive<f32>, unit: &str| -> bool {
            ui.label(egui::RichText::new(tl!(&label)).color(t.text_dim).size(12.0));
            crate::widgets::value_field(ui, v, range, unit, 64.0).accessible_name(tl!(&label)).changed()
        };
        match live {
            photocraft_doc::vector::LiveShape::Rect { rect, .. }
            | photocraft_doc::vector::LiveShape::Ellipse { rect }
            | photocraft_doc::vector::LiveShape::Polygon { rect, .. } => {
                ui.horizontal(|ui| {
                    let (mut w, mut h) = (rect[2] as f32, rect[3] as f32);
                    let cw = num(ui, "W", &mut w, 1.0..=300000.0, "px");
                    let ch = num(ui, "H", &mut h, 1.0..=300000.0, "px");
                    if cw || ch {
                        edit = Some(json!({"rect": [rect[0], rect[1], w.round(), h.round()], "coalesce": key("wh")}));
                    }
                });
            }
            photocraft_doc::vector::LiveShape::Line { .. } => {}
        }
        ui.vertical(|ui| match live {
            photocraft_doc::vector::LiveShape::Rect { radii, .. } => {
                // Keep the uniform workflow available after independent corner edits too.
                ui.horizontal(|ui| {
                    let mut all = radii[0] as f32;
                    if num(ui, tl!("Corner radius"), &mut all, 0.0..=100000.0, "px") {
                        edit = Some(json!({"radii": all, "coalesce": key("radius-all")}));
                    }
                });
                // Separate rows also fit the narrow Properties dock. The labels use the
                // localized corner names, rather than untranslated abbreviations.
                for (index, label) in [(0, tl!("Top Left")), (1, tl!("Top Right")), (3, tl!("Bottom Left")), (2, tl!("Bottom Right"))] {
                    ui.horizontal(|ui| {
                        let mut radius = radii[index] as f32;
                        if num(ui, label, &mut radius, 0.0..=100000.0, "px") {
                            let mut patch = corner_radii_patch(*radii, index, radius);
                            patch["coalesce"] = json!(key(&format!("radius-{index}")));
                            edit = Some(patch);
                        }
                    });
                }
            }
            photocraft_doc::vector::LiveShape::Polygon { sides, star_ratio, corner_radius, .. } => {
                // One radius rounds every vertex of a triangle, polygon or star, as in Photoshop.
                ui.horizontal(|ui| {
                    let mut r = *corner_radius as f32;
                    if num(ui, tl!("Corner radius"), &mut r, 0.0..=100000.0, "px") {
                        edit = Some(json!({"cornerRadius": r, "coalesce": key("corner-radius")}));
                    }
                });
                ui.horizontal(|ui| {
                    let mut n = *sides as f32;
                    if num(ui, tl!("Sides"), &mut n, 3.0..=100.0, "") {
                        edit = Some(json!({"sides": n.round() as u32, "coalesce": key("sides")}));
                    }
                    let mut sr = (*star_ratio * 100.0) as f32;
                    if num(ui, tl!("Star ratio"), &mut sr, 1.0..=100.0, "%") {
                        edit = Some(json!({"starRatio": sr as f64 / 100.0, "coalesce": key("star")}));
                    }
                })
                .inner
            }
            photocraft_doc::vector::LiveShape::Line { weight, .. } => {
                ui.horizontal(|ui| {
                    let mut w = *weight as f32;
                    if num(ui, tl!("Weight"), &mut w, 1.0..=10000.0, "px") {
                        edit = Some(json!({"weight": w, "coalesce": key("weight")}));
                    }
                })
                .inner
            }
            _ => {}
        });
    }
    if let Some(mut p) = edit {
        p["layer"] = json!(id.0);
        if let Err(e) = app.run("shape.edit", p) {
            app.ui.status = e;
            app.ui.status_error = true;
        }
        if !ui.input(|i| i.pointer.any_down()) {
            ui.data_mut(|d| {
                let gesture = d.get_temp_mut_or_default::<(u64, u64)>(gesture_key);
                gesture.0 = gesture.0.wrapping_add(1);
            });
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Paths panel

fn thumb(ui: &egui::Ui, r: Rect, path: &Path, doc: &Document) {
    let t = Tokens::get(ui.ctx());
    // At the document's aspect ratio, like the Channels thumbnails.
    let r = crate::channels_panel::fit_thumb(r, doc.size.width, doc.size.height).0;
    ui.painter().rect_filled(r, 0.0, Color32::from_gray(if t.pro { 222 } else { 240 }));
    let (w, h) = (doc.size.width.max(1) as f64, doc.size.height.max(1) as f64);
    let s = (r.width() as f64 / w).min(r.height() as f64 / h);
    let off = r.center() - vec2((w * s) as f32 / 2.0, (h * s) as f32 / 2.0);
    let map = |q: [f64; 2]| pos2(off.x + (q[0] * s) as f32, off.y + (q[1] * s) as f32);
    for (pts, closed) in path_lines(path, &map) {
        if closed && pts.len() >= 3 {
            ui.painter().add(egui::Shape::closed_line(pts, Stroke::new(1.0, Color32::from_gray(40))));
        } else {
            ui.painter().add(egui::Shape::line(pts, Stroke::new(1.0, Color32::from_gray(40))));
        }
    }
}

/// What a Paths panel row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathRow {
    Saved,
    Work,
    /// The selected layer's shape path or vector mask (temporary, italic).
    Layer,
}

/// A Paths panel row.
#[derive(Clone, Debug)]
pub struct PathEntry {
    pub name: String,
    pub path: Path,
    pub kind: PathRow,
}

/// The Paths panel's rows, top to bottom: saved paths, the work path, then the selected layer's
/// shape path ("<Layer> Shape Path") or vector mask ("<Layer> Vector Mask"), like Photoshop.
pub fn path_rows(doc: &Document, active: Option<photocraft_doc::LayerId>) -> Vec<PathEntry> {
    let mut rows: Vec<_> = doc.paths.iter().map(|p| PathEntry { name: p.name.clone(), path: p.path.clone(), kind: PathRow::Saved }).collect();
    if let Some(wp) = &doc.work_path {
        rows.push(PathEntry { name: "Work Path".into(), path: wp.clone(), kind: PathRow::Work });
    }
    if let Some(l) = active.and_then(|id| doc.layer(id)) {
        let layer_path = match &l.content {
            LayerContent::Shape(sh) => Some((format!("{} Shape Path", l.name), sh.path.clone())),
            _ => l.vector_mask.as_ref().map(|v| (format!("{} Vector Mask", l.name), v.path.clone())),
        };
        if let Some((name, path)) = layer_path {
            rows.push(PathEntry { name, path, kind: PathRow::Layer });
        }
    }
    rows
}

fn footer_id() -> egui::Id {
    egui::Id::unique("paths-footer")
}

/// A Paths panel row being dragged, for a drop on the trash (#6553): its name and the command
/// that deletes it.
#[derive(Clone, Debug)]
struct PathDrag {
    name: String,
    cmd: &'static str,
    params: Value,
}

fn path_drag_key() -> egui::Id {
    egui::Id::unique("paths-drag")
}

fn ctx_data_footer(ctx: &egui::Context, r: Rect) {
    ctx.data_mut(|d| d.insert_temp(footer_id(), r));
}

/// Where the Paths panel's footer buttons were drawn last frame (the rect around them all).
pub fn paths_footer(ctx: &egui::Context) -> Option<Rect> {
    ctx.data(|d| d.get_temp(footer_id()))
}

/// Photoshop's default name for a saved path: the first free "Path N", counting from the number of
/// saved paths.
fn free_path_name(doc: &Document) -> String {
    let mut n = doc.paths.len().saturating_add(1);
    while doc.paths.iter().any(|p| p.name == format!("Path {n}")) {
        n = n.saturating_add(1);
        if n == usize::MAX {
            break;
        }
    }
    format!("Path {n}")
}

/// A Paths panel name being typed in place: the row's key ("work" or the saved path's name) and
/// the text so far. It lives in egui's temp data, like the Channels panel's rename field.
#[derive(Clone)]
struct PathRename {
    key: String,
    text: String,
    focused: bool,
}

fn rename_id(doc: &Document) -> egui::Id {
    egui::Id::unique(("path-rename", doc.id.0))
}

/// Commands offered by a Paths row's context menu. The row's path is passed explicitly so a
/// right-click acts on that row even when another path is selected in the panel. `layer` is the
/// layer a `PathRow::Layer` row belongs to (the active layer).
fn path_context_actions(entry: &PathEntry, doc: &Document, layer: Option<photocraft_doc::LayerId>) -> Vec<(&'static str, &'static str, Value)> {
    let key = match entry.kind {
        PathRow::Work => "work".to_string(),
        PathRow::Layer => "layer".to_string(),
        PathRow::Saved => entry.name.clone(),
    };
    let mut actions = vec![
        ("Make Selection", "path.toSelection", json!({"name": key})),
        ("Fill Path", "path.fill", json!({"name": key})),
        ("Stroke Path…", "path.stroke", json!({"name": key})),
    ];
    if entry.kind == PathRow::Work {
        actions.push(("Save Path", "path.rename", json!({"name": "work", "to": free_path_name(doc)})));
    } else {
        let mut n = 1usize;
        while doc.paths.iter().any(|p| p.name == format!("{} copy {n}", entry.name)) {
            n = n.saturating_add(1);
            if n == usize::MAX {
                break;
            }
        }
        let copy_name = format!("{} copy {n}", entry.name);
        actions.push(("Duplicate Path", "path.set", json!({"name": copy_name, "path": photocraft_engine::vector_cmds::path_json(&entry.path)})));
    }
    if entry.kind == PathRow::Saved {
        // No `to`: the panel asks for the new name in place.
        actions.push(("Rename Path…", "path.rename", json!({"name": key})));
    }
    if entry.kind != PathRow::Layer {
        actions.push(("Delete Path", "path.delete", json!({"name": key})));
    } else if entry.name.ends_with(" Vector Mask") {
        actions.push(("Delete Vector Mask", "layer.vectorMask.delete", json!({})));
    } else if let Some(id) = layer {
        // A shape layer's path is the shape: Photoshop's Delete Path deletes the layer (#6552).
        actions.push(("Delete Path", "layer.delete", json!({"layer": id.0})));
    }
    actions
}

pub fn paths_panel(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    let Some(st) = app.session.active() else {
        ui.label(egui::RichText::new(tl!("No document")).color(t.text_faint));
        return;
    };
    let doc = st.doc.clone();
    let st_layer = st.active_layer;
    let rows = path_rows(&doc, st_layer);
    let has_layer_path = rows.iter().any(|r| r.kind == PathRow::Layer);
    let mut action: Option<(&str, Value)> = None;
    let mut stroke_dialog: Option<String> = None;
    // A row dragged onto the trash is deleted (#6553); a drag that never saw its release must not
    // outlive it.
    let ctx = ui.ctx().clone();
    if !ctx.input(|i| i.pointer.primary_down() || i.pointer.any_released()) {
        ctx.data_mut(|d| d.remove::<PathDrag>(path_drag_key()));
    }
    let drag = ctx.data(|d| d.get_temp::<PathDrag>(path_drag_key()));
    let rename_id = rename_id(&doc);
    // The buttons sit in a footer at the panel's bottom, like Photoshop's.
    let footer = crate::widgets::footer_height(ui) + ui.spacing().item_spacing.y;
    let fill = ui.available_height() > footer + 60.0;
    let rows_h = if fill { ui.available_height() - footer } else { f32::INFINITY };
    egui::ScrollArea::vertical().id_salt("path-rows").max_height(rows_h).min_scrolled_height(if fill { rows_h } else { 0.0 }).auto_shrink([false, !fill]).show(
        ui,
        |ui| {
            if rows.is_empty() {
                ui.label(egui::RichText::new(tl!("Draw with the Pen tool (P) or make a work path from a selection.")).color(t.text_faint).size(11.5));
            }
            for PathEntry { name, path, kind } in &rows {
                // What dropping the row on the trash runs; a shape layer's path is not dragged.
                let delete = match kind {
                    PathRow::Work => Some(("path.delete", json!({"name": "work"}))),
                    PathRow::Saved => Some(("path.delete", json!({"name": name}))),
                    PathRow::Layer if name.ends_with(" Vector Mask") => Some(("layer.vectorMask.delete", json!({}))),
                    PathRow::Layer => None,
                };
                let sense = if delete.is_some() { Sense::click_and_drag() } else { Sense::click() };
                let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), sense);
                let key = match kind {
                    PathRow::Work => "work".to_string(),
                    PathRow::Layer => "layer".to_string(),
                    PathRow::Saved => name.clone(),
                };
                if resp.drag_started()
                    && let Some((cmd, params)) = delete
                {
                    let name = if *kind == PathRow::Work { tl!(name).to_string() } else { name.clone() };
                    ui.ctx().data_mut(|d| d.insert_temp(path_drag_key(), PathDrag { name, cmd, params }));
                }
                // Rows are painted: name them for screen readers and UI tests.
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, name));
                let sel = app.ui.selected_path.as_deref() == Some(key.as_str());
                if sel {
                    ui.painter().rect_filled(r, 0.0, t.row_selected);
                } else if resp.hovered() {
                    ui.painter().rect_filled(r, 0.0, t.hover.gamma_multiply(0.5));
                }
                let thumbnail = Rect::from_min_size(pos2(r.left() + 8.0, r.top() + 4.0), vec2(28.0, 28.0));
                thumb(ui, thumbnail, path, &doc);
                let mut job = egui::text::LayoutJob::default();
                // Temporary paths (the work path, the selected layer's shape path or vector mask) are italic.
                let italics = *kind != PathRow::Saved;
                job.append(
                    if *kind == PathRow::Work { tl!(name) } else { name },
                    0.0,
                    egui::TextFormat { font_id: egui::FontId::proportional(12.0), color: t.text, italics, ..Default::default() },
                );
                let renaming = ui.ctx().data(|d| d.get_temp::<PathRename>(rename_id)).filter(|e| e.key == key);
                if let Some(mut edit) = renaming {
                    let field = Rect::from_min_max(pos2(r.left() + 43.0, r.center().y - 11.0), pos2(r.right() - 8.0, r.center().y + 11.0));
                    let id = rename_id.with("field");
                    let te =
                        ui.put(field, egui::TextEdit::singleline(&mut edit.text).id(id).font(egui::FontId::proportional(12.0))).accessible_name(tl!("Name"));
                    let (enter, esc) = ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape)));
                    if !edit.focused {
                        te.request_focus();
                        // Like Photoshop, the whole name starts selected so typing replaces it.
                        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
                            let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(edit.text.chars().count()));
                            state.cursor.set_char_range(Some(all));
                            state.store(ui.ctx(), id);
                        }
                        edit.focused = true;
                        ui.ctx().data_mut(|d| d.insert_temp(rename_id, edit));
                    } else if esc {
                        ui.ctx().data_mut(|d| d.remove::<PathRename>(rename_id));
                    } else if enter || te.lost_focus() || !te.has_focus() {
                        ui.ctx().data_mut(|d| d.remove::<PathRename>(rename_id));
                        let to = edit.text.trim();
                        // The Work Path is saved even under its proposed name; an unchanged saved name is left alone.
                        if !to.is_empty() && (*kind == PathRow::Work || to != name) {
                            action = Some(("path.rename", json!({"name": key, "to": to})));
                        }
                    } else {
                        ui.ctx().data_mut(|d| d.insert_temp(rename_id, edit));
                    }
                } else {
                    let g = ui.painter().layout_job(job);
                    ui.painter().galley(pos2(r.left() + 46.0, r.center().y - g.size().y / 2.0), g, t.text);
                }
                // Ctrl/Cmd-click loads only the thumbnail's path, without changing the panel target.
                let modifiers = ui.input(|i| i.modifiers);
                let load = resp.clicked() && modifiers.command && resp.interact_pointer_pos().is_some_and(|p| thumbnail.contains(p));
                if load {
                    let mode = match crate::channels_panel::load_operation(modifiers) {
                        "new" => "replace",
                        mode => mode,
                    };
                    action = Some(("path.toSelection", json!({"name": key, "mode": mode})));
                } else if resp.clicked() {
                    app.ui.selected_path = Some(key.clone());
                }
                // As in Photoshop, double-clicking a saved path's name renames it, and double-clicking
                // the Work Path asks for the name to save it under (#6238).
                let proposed = match kind {
                    PathRow::Work => Some(free_path_name(&doc)),
                    PathRow::Saved => Some(name.clone()),
                    PathRow::Layer => None,
                };
                if !load
                    && resp.double_clicked()
                    && let Some(text) = proposed
                {
                    ui.ctx().data_mut(|d| d.insert_temp(rename_id, PathRename { key: key.clone(), text, focused: false }));
                }
                resp.context_menu(|ui| {
                    crate::widgets::menu_scroll(ui, |ui| {
                        ui.set_min_width(190.0);
                        let entry = PathEntry { name: name.clone(), path: path.clone(), kind: *kind };
                        let can_paint = app.session.active().and_then(|s| s.active_layer.and_then(|id| s.doc.layer(id))).is_some_and(|l| l.surface().is_some());
                        for (label, cmd, params) in path_context_actions(&entry, &doc, st_layer) {
                            let enabled = !matches!(cmd, "path.fill" | "path.stroke") || can_paint;
                            if ui.add_enabled(enabled, egui::Button::new(tl!(label))).clicked() {
                                if cmd == "path.stroke" {
                                    // Stroke Path… asks for the tool first, as in Photoshop (#2978).
                                    stroke_dialog = Some(params.get("name").and_then(Value::as_str).unwrap_or("work").to_string());
                                } else if cmd == "path.rename" && params.get("to").is_none() {
                                    let edit = PathRename { key: key.clone(), text: name.clone(), focused: false };
                                    ui.ctx().data_mut(|d| d.insert_temp(rename_id, edit));
                                } else {
                                    action = Some((cmd, params));
                                }
                                ui.close();
                            }
                        }
                    });
                });
                ui.painter().line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0, t.separator));
            }
            // Clicking the empty area deselects the path, as in Photoshop: new fill and
            // adjustment layers then take the selection as their mask instead (#1419).
            let rest = ui.available_rect_before_wrap();
            let rest = rest.intersect(Rect::from_min_size(rest.min, vec2(rest.width(), rest.height().min(4000.0))));
            if rest.is_positive() && ui.interact(rest, ui.scope_id().with("paths-empty"), Sense::click()).clicked() {
                app.ui.selected_path = None;
            }
        },
    );
    // The dragged row follows the cursor.
    if let Some(drag) = &drag
        && let Some(p) = ctx.input(|i| i.pointer.interact_pos())
    {
        crate::layer_transfer::ghost(&ctx, p, &drag.name);
    }
    let sel = app.ui.selected_path.clone().filter(|s| s != "layer" || has_layer_path).unwrap_or_else(|| "work".into());
    let buttons = crate::widgets::panel_footer(ui, |ui| {
        let fg = hex(app.session.tools.foreground);
        let mut n = doc.paths.len() + 1;
        while doc.paths.iter().any(|p| p.name == format!("Path {n}")) {
            n += 1;
        }
        let items: [(&str, &str, &str, Value); 7] = [
            ("paint-bucket", "Fill path with foreground color", "path.fill", json!({"name": sel, "color": fg})),
            ("circle", "Stroke path with brush", "path.stroke", json!({"name": sel, "tool": "brush"})),
            ("square-dashed", "Load path as a selection", "path.toSelection", json!({"name": sel})),
            ("spline", "Make work path from selection", "select.toWorkPath", json!({"tolerance": 2.0})),
            ("square", "Add vector mask", "layer.vectorMask.fromPath", json!({"name": sel})),
            ("square-plus", "Create new path", "path.set", json!({"name": format!("Path {n}"), "path": {"subpaths": []}})),
            ("trash", "Delete path", "path.delete", json!({"name": sel})),
        ];
        // The layer's own path: deleting it deletes the vector mask (a shape keeps its path).
        let shape = rows.iter().any(|r| r.kind == PathRow::Layer && r.name.ends_with(" Shape Path"));
        let delete_layer_path = sel == "layer" && !shape;
        let items = items.map(|(icon, tip, cmd, p)| {
            if cmd == "path.delete" && delete_layer_path { (icon, "Delete vector mask", "layer.vectorMask.delete", json!({})) } else { (icon, tip, cmd, p) }
        });
        // Laid out from the right, so the last button goes first.
        let mut buttons = Rect::NOTHING;
        for (icon, tip, cmd, p) in items.into_iter().rev() {
            let icon = if crate::icons::exists(icon) { icon } else { "square" };
            let b = crate::icons::button(ui, icon, 26.0, false, tip);
            if b.clicked() {
                if cmd == "path.stroke" && ui.input(|i| i.modifiers.alt) {
                    // Alt-click opens the Stroke Path dialog, as in Photoshop (#2978).
                    stroke_dialog = Some(sel.clone());
                } else {
                    action = Some((cmd, p));
                }
            }
            // A row dropped on the trash deletes that row's path, selected or not (#6553).
            if matches!(cmd, "path.delete" | "layer.vectorMask.delete")
                && let Some(drag) = &drag
                && crate::panels::dropped_on(ui, &b)
            {
                action = Some((drag.cmd, drag.params.clone()));
            }
            buttons = buttons.union(b.rect);
        }
        buttons
    });
    ctx_data_footer(ui.ctx(), buttons);
    if ctx.input(|i| i.pointer.any_released()) {
        ctx.data_mut(|d| d.remove::<PathDrag>(path_drag_key()));
    }
    if let Some(name) = stroke_dialog {
        crate::canvas_tool_menu::stroke_path_dialog(app, &name);
    }
    if let Some((cmd, p)) = action {
        let renamed = if cmd == "path.rename" { p.get("to").and_then(Value::as_str).map(str::to_string) } else { None };
        match app.run(cmd, p) {
            // The renamed (or just saved) path stays the panel's target.
            Ok(_) => {
                if renamed.is_some() {
                    app.ui.selected_path = renamed;
                }
            }
            Err(e) => {
                app.ui.status = e;
                app.ui.status_error = true;
            }
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod path_selection_tests;

#[cfg(test)]
#[path = "pen_draw_tests.rs"]
mod pen_draw_tests;

#[cfg(test)]
#[path = "vector_paint_tests.rs"]
mod paint_tests;
