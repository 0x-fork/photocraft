//! Free Transform (⌘T): bounding box with handles over the canvas, live preview, commit via the
//! engine's `edit.transform` (one quad for scale/rotate/skew/distort/perspective).
//!
//! Gestures follow Photoshop CC: corner and edge drags scale proportionally (⇧ for free: an edge
//! then stretches its one axis), ⌥ scales about the reference point, ⌘-drag a corner distorts
//! (⌘⌥⇧: perspective), ⌘-drag an edge skews (⇧ along the edge), drag outside rotates (⇧ snaps to
//! 15°), drag inside moves (⇧ locks to 8 directions), the reference point can be dragged and
//! ⌥-click puts it under the pointer. Arrow keys nudge the box (move_mods.rs). ↩ commits, Esc
//! cancels. Undo and Redo step through the session's own changes (`Steps`), not the document's
//! history.

use std::sync::Arc;

use egui::{Color32, CursorIcon, Pos2, Stroke, pos2, vec2};
use photocraft_algo::transform::Homography;
use photocraft_doc::{DocId, Document, LayerContent, LayerId};
use photocraft_engine::jobs::{JobEvent, JobId, JobOutcome};
use photocraft_geom::warp::{BezierMesh, Warp, WarpStyle};
use serde_json::json;

use crate::PhotocraftApp;
use crate::canvas::{ToolEvent, ViewXform};
use crate::state::{MadeLayer, Tool, TransformMode, TransformSession};

/// Which Split button is armed. The guide follows the pointer and the split is added on release.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SplitTool {
    Cross,
    Vertical,
    Horizontal,
}

impl SplitTool {
    fn command(self) -> &'static str {
        match self {
            SplitTool::Cross => "edit.transform.splitWarpCrosswise",
            SplitTool::Vertical => "edit.transform.splitWarpVertically",
            SplitTool::Horizontal => "edit.transform.splitWarpHorizontally",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            SplitTool::Cross => "cross",
            SplitTool::Vertical => "vertical",
            SplitTool::Horizontal => "horizontal",
        }
    }
}

/// What a Warp-mode press grabbed. `pts0` is the mesh's control grid at the press.
enum WarpDrag {
    /// An anchor or handle: it follows the pointer (an anchor takes its handles along).
    Point { index: usize, start: [f64; 2], pts0: Vec<[f64; 2]> },
    /// Anywhere else on the grid: the surface under the pointer follows it.
    Surface { s: f64, t: f64, start: [f64; 2], pts0: Vec<[f64; 2]> },
}

/// Preview state that isn't serialisable: the document without the transformed pixels, and
/// full-resolution textures of those pixels (transform_tex.rs).
pub struct TransformPreview {
    pub session: u64,
    pub doc: Arc<Document>,
    pub texture: crate::transform_tex::PreviewTextures,
    pub opacity: f32,
    /// The layers stacked over the moving one, drawn over its pixels ([`Above`]).
    above: Option<Above>,
    /// A layer with a style: its pixels composited with the style as the box has them now,
    /// drawn instead of the bare pixels (transform_fx.rs, #3653).
    styled: Option<crate::transform_fx::StyledPreview>,
    gesture: Option<Gesture>,
    /// Warp-mode drag in progress.
    warp_drag: Option<WarpDrag>,
    /// Armed split tool. None while the buttons are off and control points drag as usual.
    split_tool: Option<SplitTool>,
    /// Document point the split guide is following (a drag, or the hover while placing).
    split_pointer: Option<[f64; 2]>,
    split_placing: bool,
    /// Option-click places a split without a button armed.
    split_quick: bool,
    steps: Steps,
    /// The tool the session began with: picking another one applies the transform.
    tool: Tool,
    /// Free Transform Path: the path as it was, drawn through the box instead of pixels.
    path: Option<photocraft_doc::vector::Path>,
    /// A layer's session keeps `doc` and `opacity` up to date with edits made while the box is
    /// open ([`follow_edits`]); `None` for a lone mask or channel and Transform Selection.
    follow: Option<Follow>,
    /// The commit running as a background job (#1015): the moving pixels stay on screen, without
    /// the box, until it lands; Esc cancels it. The session ends with the job.
    committing: Option<JobId>,
    /// Content-Aware Scale's live preview (cas_transform.rs).
    pub(crate) content_aware: Option<crate::cas_transform::Live>,
}

/// What [`follow_edits`] needs to rebuild the preview document.
#[derive(Clone, Copy, Debug)]
struct Follow {
    /// The document revision `doc` was built from.
    revision: u64,
    /// The moving layer is hidden in `doc` (nothing was lifted out of it).
    hidden: bool,
}

/// Photoshop's alert when Free Transform (⌘T, Edit › Transform, the Move tool's transform controls)
/// starts on a layer locked in place, by its own lock or a group's.
pub const LOCKED: &str = "Could not complete the Free Transform command because the layer is locked.";

/// Why Edit › Transform's modes and Warp refuse while Content-Aware Scale's box is open.
pub const CAS_ONLY_SCALES: &str = "Content-Aware Scale only scales: commit or cancel it first";

/// Grab radius of the box's handles, in screen points. Generous, so a corner is easy to catch;
/// just beyond it, outside the box, a drag rotates.
pub const HANDLE_PX: f64 = 12.0;

/// [`HANDLE_PX`] in document pixels at the current zoom.
pub fn handle_tolerance(app: &PhotocraftApp) -> f64 {
    HANDLE_PX / app.point_zoom().max(0.01) as f64
}

/// The box as one undo step restores it.
#[derive(Clone, Debug, PartialEq)]
struct Step {
    rect: [f64; 4],
    quad: [[f64; 2]; 4],
    pivot: [f64; 2],
    warp: Option<Warp>,
}

impl Step {
    fn of(t: &TransformSession) -> Self {
        Step { rect: t.rect, quad: t.quad, pivot: t.pivot, warp: t.warp.clone() }
    }

    fn restore(self, t: &mut TransformSession) {
        (t.rect, t.quad, t.pivot, t.warp) = (self.rect, self.quad, self.pivot, self.warp);
    }
}

/// The session's own history: inside Free Transform, Undo steps back through the
/// handle drags, nudges and option edits rather than the document's history.
#[derive(Default)]
struct Steps {
    /// The box when it last came to rest (set when the session starts).
    settled: Option<Step>,
    undo: Vec<Step>,
    redo: Vec<Step>,
    /// When the latest arrow-key nudge came (egui time) and the box it left.
    nudged: Option<(f64, Step)>,
    /// The nudge that moved the box since it last came to rest follows another one closely: it
    /// joins that one's step instead of adding its own (#2259).
    fold: bool,
}

impl Steps {
    /// An arrow-key nudge at `now` moved the box from `before` to `after`. With `pause` (seconds,
    /// `None`: nudges aren't bundled) it folds into the previous nudge's step when it came within
    /// that time of it and found the box as that nudge left it, so a drag, an option edit, Undo
    /// or Redo in between start a step of their own.
    fn nudged(&mut self, before: &Step, after: Step, now: f64, pause: Option<f64>) {
        self.fold = pause.is_some_and(|w| self.nudged.as_ref().is_some_and(|(at, left)| left == before && now >= *at && now - at <= w));
        self.nudged = pause.map(|_| (now, after));
    }

    /// Record a step if the box changed since it last came to rest.
    fn settle(&mut self, now: Step) {
        if self.settled.as_ref() == Some(&now) {
            return;
        }
        if std::mem::take(&mut self.fold) && self.settled.is_some() {
            self.settled = Some(now);
            return;
        }
        if let Some(prev) = self.settled.replace(now) {
            self.undo.push(prev);
            self.redo.clear();
        }
    }

    /// Something to undo, counting a change not yet recorded (a nudge this frame).
    fn can_undo(&self, now: &Step) -> bool {
        !self.undo.is_empty() || self.settled.as_ref().is_some_and(|p| p != now)
    }
}

fn corners(r: [f64; 4]) -> [[f64; 2]; 4] {
    [[r[0], r[1]], [r[2], r[1]], [r[2], r[3]], [r[0], r[3]]]
}

/// Start Free Transform on the active layer (or its selected pixels), or on the targeted unlinked
/// layer mask, alpha channel or Quick Mask.
pub fn begin(app: &mut PhotocraftApp, ctx: &egui::Context) -> Result<(), String> {
    if let Some((params, path)) = crate::vector_ui::free_transform_path(app) {
        return begin_path(app, ctx, params, path);
    }
    if app.session.active().is_some_and(|st| photocraft_engine::float_cmds::floating(st).is_some()) {
        app.run("select.drop", json!({}))?;
    }
    let target = crate::canvas::paint_target(app);
    let st = app.session.active().ok_or("no document")?;
    let (doc, revision) = (st.doc.clone(), st.revision);
    let lone = photocraft_engine::transform_cmds::lone_target(&doc, st.active_layer, &json!({ "target": target })).map_err(|e| e.to_string())?.cloned();
    if let Some(surf) = lone {
        return begin_lone(app, ctx, doc, surf, target);
    }
    let id = st.active_layer.ok_or("no active layer")?;
    let layer = doc.layer(id).ok_or("no layer")?;
    if photocraft_engine::transform_cmds::locked_for_transform(&doc, id) {
        // Photoshop refuses before the box opens rather than when it is committed (#2585).
        app.ui.open_dialog(crate::state::DialogKind::Error, json!({"message": tl!(LOCKED)}).as_object().cloned().unwrap_or_default());
        return Err(LOCKED.into());
    }
    if matches!(layer.content, LayerContent::Adjustment(_)) && layer.mask.is_none() {
        return Err("Adjustment layers have nothing to transform".into());
    }
    // With a selection the box is the selection's (#5297).
    let b = photocraft_engine::transform_cmds::transform_frame(&doc, layer);
    if b.is_empty() {
        return Err("Could not transform: the layer is empty".into());
    }
    crate::type_tool::commit(app);
    let rect = [b.x0 as f64, b.y0 as f64, b.x1 as f64, b.y1 as f64];
    let session = app.ui.alloc_id();
    // Preview document: the moving pixels removed (layer hidden, or the selection lifted out).
    let mut pd = (*doc).clone();
    let mut lifted = None;
    // Type, shapes and smart objects move whole, selection or not (#2630): only pixels are lifted.
    if let Some(sel) = doc.selection.clone().filter(|_| !photocraft_engine::transform_cmds::moves_whole(layer) && layer.surface().is_some()) {
        if let Some(surf) = pd.layer_mut(id).and_then(|l| l.surface_mut()) {
            let (l, rest) = photocraft_engine::transform_cmds::split_selected(surf, &sel).map_err(|e| e.to_string())?;
            *surf = rest;
            lifted = Some(l);
        }
    } else if let Some(l) = pd.layer_mut(id) {
        l.visible = false;
    }
    // egui reports the renderer's limit in the app; offscreen harnesses only through the device.
    let max_side = ctx.input(|i| i.max_texture_side).max(app.gpu.as_ref().map_or(1, |g| g.max_texture_side()));
    // The layers covering the moving pixels draw over them instead of under them.
    let above = Above::new(ctx, format!("transform-{session}-above"), &doc, id, max_side);
    above.hide_in(&mut pd.layers);
    // A style is drawn again from the pixels as the box moves them, so they are read without it.
    // Not for the selected part of a layer: its style belongs to that part and the rest together.
    let styled = lifted.is_none() && photocraft_compose::effects::has_effects(layer);
    let (image, uv) = preview_image(&doc, id, lifted.as_ref(), b, max_side, styled);
    let texture = crate::transform_tex::PreviewTextures::new(ctx, format!("transform-{session}"), image, uv);
    app.transform_preview = Some(TransformPreview {
        session,
        doc: Arc::new(pd),
        texture,
        opacity: layer.opacity * layer.fill_opacity,
        above: Some(above),
        styled: styled.then(|| crate::transform_fx::StyledPreview::new(format!("transform-{session}-styled"))),
        follow: Some(Follow { revision, hidden: lifted.is_none() }),
        committing: None,
        gesture: None,
        warp_drag: None,
        split_tool: None,
        split_pointer: None,
        split_placing: false,
        split_quick: false,
        steps: Steps::default(),
        tool: app.ui.tool,
        path: None,
        content_aware: None,
    });
    app.ui.transform = Some(TransformSession {
        session,
        layer: id.0,
        rect,
        quad: corners(rect),
        pivot: [(rect[0] + rect[2]) / 2.0, (rect[1] + rect[3]) / 2.0],
        interpolation: "bicubic".into(),
        warp: None,
        selection: false,
        target: None,
        path: None,
        made: None,
        mode: Default::default(),
        content_aware: None,
        skew_v: 0.0,
    });
    start_steps(app);
    Ok(())
}

/// Free Transform on a copy (⌥⌘T, no menu item): duplicates the active layer (with a selection,
/// its selected pixels, as Layer via Copy) and transforms the copy (#352).
pub fn begin_copy(app: &mut PhotocraftApp, ctx: &egui::Context) -> Result<(), String> {
    let selection = app.session.active().is_some_and(|d| d.doc.selection.is_some());
    let params = if selection { json!({}) } else { json!({"inPlace": true}) };
    app.run(if selection { "layer.new.layerViaCopy" } else { "layer.duplicate" }, params)?;
    let made = app.session.active().and_then(|st| st.active_layer.map(|layer| (st.doc.id, layer)));
    if let Err(e) = begin(app, ctx) {
        if let Some((document, layer)) = made {
            take_back_made(app, document, layer);
        }
        return Err(e);
    }
    if let Some(t) = app.ui.transform.as_mut() {
        t.made = Some(MadeLayer::Copy);
    }
    Ok(())
}

/// Free Transform on the layer a file dropped on the canvas just placed, like the reference app's
/// Place: Esc takes the place back, ↩ makes the place and the transform one Place Embedded step.
pub fn begin_placed(app: &mut PhotocraftApp, ctx: &egui::Context) -> Result<(), String> {
    begin(app, ctx)?;
    if let Some(t) = app.ui.transform.as_mut() {
        t.made = Some(MadeLayer::Place);
    }
    Ok(())
}

/// Edit › Content-Aware Scale (#3096): a scale-only box over `region` of the active pixel layer
/// (the selection's bounds, else the layer's content), cleared under the box as the command
/// clears it. OK runs `edit.contentAwareScale` (cas_transform.rs) instead of `edit.transform`.
pub(crate) fn begin_content_aware(
    app: &mut PhotocraftApp,
    ctx: &egui::Context,
    region: photocraft_geom::Rect,
    live: crate::cas_transform::Live,
) -> Result<(), String> {
    let st = app.session.active().ok_or("no document")?;
    let (doc, revision) = (st.doc.clone(), st.revision);
    let id = st.active_layer.ok_or("no active layer")?;
    let layer = doc.layer(id).ok_or("no layer")?;
    let surf = layer.surface().ok_or("Content-Aware Scale needs a pixel layer")?;
    if region.is_empty() {
        return Err("Could not scale: the layer is empty".into());
    }
    crate::type_tool::commit(app);
    let rect = [region.x0 as f64, region.y0 as f64, region.x1 as f64, region.y1 as f64];
    let session = app.ui.alloc_id();
    // The command clears the area before writing the result (a layer without transparency keeps it).
    let mut pd = (*doc).clone();
    if let Some(s) = pd.layer_mut(id).and_then(|l| l.surface_mut()).filter(|s| s.format().alpha) {
        let clear = vec![0.0; s.format().channels()];
        s.fill_rect(region, &clear);
        s.prune();
    }
    let max_side = ctx.input(|i| i.max_texture_side).max(app.gpu.as_ref().map_or(1, |g| g.max_texture_side()));
    let above = Above::new(ctx, format!("transform-{session}-above"), &doc, id, max_side);
    above.hide_in(&mut pd.layers);
    let (image, uv) = crate::transform_tex::read_surface(surf, None, region, max_side);
    let texture = crate::transform_tex::PreviewTextures::new(ctx, format!("transform-{session}"), image, uv);
    app.transform_preview = Some(TransformPreview {
        session,
        doc: Arc::new(pd),
        texture,
        opacity: layer.opacity * layer.fill_opacity,
        above: Some(above),
        follow: Some(Follow { revision, hidden: false }),
        committing: None,
        gesture: None,
        warp_drag: None,
        split_tool: None,
        split_pointer: None,
        split_placing: false,
        split_quick: false,
        steps: Steps::default(),
        tool: app.ui.tool,
        path: None,
        content_aware: Some(live),
        styled: None,
    });
    app.ui.transform = Some(TransformSession {
        session,
        layer: id.0,
        rect,
        quad: corners(rect),
        pivot: [(rect[0] + rect[2]) / 2.0, (rect[1] + rect[3]) / 2.0],
        interpolation: "bicubic".into(),
        warp: None,
        selection: false,
        target: None,
        path: None,
        made: None,
        mode: Default::default(),
        content_aware: Some(Default::default()),
        skew_v: 0.0,
    });
    start_steps(app);
    Ok(())
}

/// Undoes the layer a cancelled or failed session made (⌥⌘T's copy, a placed file), leaving
/// nothing to redo.
fn take_back_made(app: &mut PhotocraftApp, document: DocId, layer: LayerId) {
    // Tabs can move or close while transforming. Only undo the creation in its own document,
    // and only while it is still the latest step (it may already have been undone).
    let Some(index) = app.session.documents().iter().position(|st| {
        st.doc.id == document
            && st.doc.layer(layer).is_some()
            && st.history.past_len().checked_sub(1).and_then(|i| st.history.state(i)).is_some_and(|before| before.layer(layer).is_none())
    }) else {
        return;
    };
    let active = app.session.active_index();
    app.session.set_active(index);
    if app.session.undo()
        && let Some(st) = app.session.active_mut()
    {
        st.history.clear_redo();
    }
    if let Some(active) = active {
        app.session.set_active(active);
    }
    app.sync_views();
}

/// After the transform: the step that made the layer and the transform become one history step,
/// the transform's for a copy and Place Embedded for a placed file.
fn fold_made(app: &mut PhotocraftApp, document: DocId, layer: LayerId, made: MadeLayer) {
    // A background commit lands in its own document, which need not be the active one by then.
    let Some(index) = app.session.documents().iter().position(|st| st.doc.id == document) else { return };
    let active = app.session.active_index();
    app.session.set_active(index);
    // Only when the step before the transform made the layer: a step taken while the box was
    // open (New Layer, which also ends the box, #5667) stays its own, as does the made layer's.
    if let Some(st) = app.session.active_mut()
        && st.history.past_len().checked_sub(2).and_then(|i| st.history.state(i)).is_some_and(|before| before.layer(layer).is_none())
    {
        st.history.purge_last();
        if made == MadeLayer::Place {
            st.history.set_current_label(photocraft_engine::file_cmds::PLACE_EMBEDDED);
        }
    }
    if let Some(active) = active {
        app.session.set_active(active);
    }
}

/// Free Transform of a targeted unlinked layer mask, alpha channel or Quick Mask by itself: the
/// box frames its content (or the selection) and previews the moving values in grey over the
/// document with them vacated.
fn begin_lone(
    app: &mut PhotocraftApp,
    ctx: &egui::Context,
    doc: Arc<Document>,
    surf: photocraft_raster::Surface,
    target: serde_json::Value,
) -> Result<(), String> {
    use photocraft_engine::transform_cmds as tc;
    let b = tc::target_frame(&doc, &surf);
    if b.is_empty() {
        return Err("Could not transform: nothing is selected".into());
    }
    let whole;
    let sel = match &doc.selection {
        Some(s) => s,
        None => {
            let mut s = photocraft_raster::Surface::new(photocraft_color::PixelFormat::GRAY8);
            s.fill_rect(b, &[1.0]);
            whole = s;
            &whole
        }
    };
    let (lifted, rest) = tc::split_gray_selected(&surf, sel).map_err(|e| e.to_string())?.ok_or("this channel can't be transformed")?;
    crate::type_tool::commit(app);
    let layer = app.session.active().and_then(|st| st.active_layer).map_or(0, |l| l.0);
    let mut pd = (*doc).clone();
    let tp = json!({ "target": target });
    if let Ok(Some(s)) = tc::lone_target_mut(&mut pd, Some(LayerId(layer)), &tp) {
        *s = rest;
    }
    let rect = [b.x0 as f64, b.y0 as f64, b.x1 as f64, b.y1 as f64];
    let session = app.ui.alloc_id();
    let max_side = ctx.input(|i| i.max_texture_side).max(app.gpu.as_ref().map_or(1, |g| g.max_texture_side()));
    let (image, uv) = crate::transform_tex::read_surface(&lifted, None, b, max_side);
    let texture = crate::transform_tex::PreviewTextures::new(ctx, format!("transform-{session}"), image, uv);
    app.transform_preview = Some(TransformPreview {
        session,
        doc: Arc::new(pd),
        texture,
        opacity: 0.6,
        above: None,
        styled: None,
        follow: None,
        committing: None,
        gesture: None,
        warp_drag: None,
        split_tool: None,
        split_pointer: None,
        split_placing: false,
        split_quick: false,
        steps: Steps::default(),
        tool: app.ui.tool,
        path: None,
        content_aware: None,
    });
    app.ui.transform = Some(TransformSession {
        session,
        layer,
        rect,
        quad: corners(rect),
        pivot: [(rect[0] + rect[2]) / 2.0, (rect[1] + rect[3]) / 2.0],
        interpolation: "bicubic".into(),
        warp: None,
        selection: false,
        target: Some(target),
        path: None,
        made: None,
        mode: Default::default(),
        content_aware: None,
        skew_v: 0.0,
    });
    start_steps(app);
    Ok(())
}

/// Edit › Free Transform Path: the box frames the path's anchors and handles and draws the
/// path through it; OK commits one `path.transform` step.
fn begin_path(app: &mut PhotocraftApp, ctx: &egui::Context, params: serde_json::Value, path: photocraft_doc::vector::Path) -> Result<(), String> {
    let (x0, y0, x1, y1) = path.control_bounds().ok_or("the path has no points to transform")?;
    let st = app.session.active().ok_or("no document")?;
    let doc = st.doc.clone();
    let layer = st.active_layer.or_else(|| doc.layers.first().map(|l| l.id)).ok_or("no layer")?;
    crate::type_tool::commit(app);
    // A straight horizontal or vertical path still gets a box to turn.
    let rect = [x0, y0, x1.max(x0 + 1.0), y1.max(y0 + 1.0)];
    let session = app.ui.alloc_id();
    let texture =
        crate::transform_tex::PreviewTextures::new(ctx, format!("transform-{session}"), egui::ColorImage::new([1, 1], vec![Color32::TRANSPARENT]), [1.0, 1.0]);
    app.transform_preview = Some(TransformPreview {
        session,
        doc,
        texture,
        opacity: 1.0,
        styled: None,
        gesture: None,
        warp_drag: None,
        split_tool: None,
        split_pointer: None,
        split_placing: false,
        split_quick: false,
        steps: Steps::default(),
        tool: app.ui.tool,
        path: Some(path),
        content_aware: None,
        above: None,
        follow: None,
        committing: None,
    });
    app.ui.transform = Some(TransformSession {
        session,
        layer: layer.0,
        rect,
        quad: corners(rect),
        pivot: [(rect[0] + rect[2]) / 2.0, (rect[1] + rect[3]) / 2.0],
        interpolation: "bicubic".into(),
        warp: None,
        selection: false,
        target: None,
        path: Some(params),
        made: None,
        mode: Default::default(),
        content_aware: None,
        skew_v: 0.0,
    });
    start_steps(app);
    Ok(())
}

/// The affine map taking `rect`'s corners to `quad`'s; `None` when the box was distorted out of a
/// parallelogram (Distort, Perspective): the path then goes as a `quad`.
fn rect_to_quad_affine(r: [f64; 4], q: [[f64; 2]; 4]) -> Option<photocraft_geom::Affine> {
    let (w, h) = (r[2] - r[0], r[3] - r[1]);
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let tol = 1e-3 * (1.0 + w.max(h));
    if (q[0][0] + q[2][0] - q[1][0] - q[3][0]).abs() > tol || (q[0][1] + q[2][1] - q[1][1] - q[3][1]).abs() > tol {
        return None;
    }
    let (a, b) = ((q[1][0] - q[0][0]) / w, (q[1][1] - q[0][1]) / w);
    let (c, d) = ((q[3][0] - q[0][0]) / h, (q[3][1] - q[0][1]) / h);
    let m = [a, b, c, d, q[0][0] - a * r[0] - c * r[1], q[0][1] - b * r[0] - d * r[1]];
    m.iter().all(|v| v.is_finite()).then_some(photocraft_geom::Affine { m })
}

/// Start Select › Transform Selection: the same box over the selection's bounds, previewing the
/// selection mask (the marching ants are hidden meanwhile); commits `select.transformSelection`.
pub fn begin_selection(app: &mut PhotocraftApp, ctx: &egui::Context) -> Result<(), String> {
    let st = app.session.active().ok_or("no document")?;
    let doc = st.doc.clone();
    let sel = doc.selection.clone().ok_or("no selection")?;
    let b = sel.content_bounds();
    if b.is_empty() {
        return Err("no selection".into());
    }
    let layer = st.active_layer.or_else(|| doc.layers.first().map(|l| l.id)).ok_or("no layer")?;
    crate::type_tool::commit(app);
    let rect = [b.x0 as f64, b.y0 as f64, b.x1 as f64, b.y1 as f64];
    let session = app.ui.alloc_id();
    let (w, h) = (b.width() as usize, b.height() as usize);
    let k = w.max(h).div_ceil(2048).max(1);
    let (tw, th) = (w.div_ceil(k), h.div_ceil(k));
    let mut px = vec![Color32::TRANSPARENT; tw * th];
    for ty in 0..th {
        for tx in 0..tw {
            let m = sel.sample_channel(b.x0 + (tx * k) as i32, b.y0 + (ty * k) as i32, 0).clamp(0.0, 1.0);
            px[ty * tw + tx] = Color32::from_rgba_unmultiplied(120, 170, 255, (m * 110.0) as u8);
        }
    }
    let uv = [w as f32 / (tw * k) as f32, h as f32 / (th * k) as f32];
    let texture = crate::transform_tex::PreviewTextures::new(ctx, format!("transform-{session}"), egui::ColorImage::new([tw, th], px), uv);
    let mut pd = (*doc).clone();
    pd.selection = None;
    app.transform_preview = Some(TransformPreview {
        session,
        doc: Arc::new(pd),
        texture,
        opacity: 1.0,
        above: None,
        styled: None,
        follow: None,
        committing: None,
        gesture: None,
        warp_drag: None,
        split_tool: None,
        split_pointer: None,
        split_placing: false,
        split_quick: false,
        steps: Steps::default(),
        tool: app.ui.tool,
        path: None,
        content_aware: None,
    });
    app.ui.transform = Some(TransformSession {
        session,
        layer: layer.0,
        rect,
        quad: corners(rect),
        pivot: [(rect[0] + rect[2]) / 2.0, (rect[1] + rect[3]) / 2.0],
        interpolation: "bilinear".into(),
        warp: None,
        selection: true,
        target: None,
        path: None,
        made: None,
        mode: Default::default(),
        content_aware: None,
        skew_v: 0.0,
    });
    start_steps(app);
    Ok(())
}

/// Start (or switch an active Free Transform into) Warp mode.
pub fn begin_warp(app: &mut PhotocraftApp, ctx: &egui::Context) -> Result<(), String> {
    if app.ui.transform.as_ref().is_some_and(|t| t.content_aware.is_some()) {
        return Err(CAS_ONLY_SCALES.into());
    }
    let fresh = app.ui.transform.is_none();
    if fresh {
        begin(app, ctx)?;
    }
    enter_warp(app, ctx);
    // Started in Warp: that is where Undo stops. Switching an open transform to Warp is a step.
    if fresh {
        start_steps(app);
    }
    Ok(())
}

/// Switch to Warp mode: a smart object's existing warp (when the box is untouched), else a
/// custom mesh following the current box (so a transform made first carries over). Editable type
/// gets its own Warp Text style instead ([`enter_type_warp`]).
pub fn enter_warp(app: &mut PhotocraftApp, ctx: &egui::Context) {
    if app.ui.transform.as_ref().is_some_and(|t| t.warp.is_none()) && warps_type(app) {
        return enter_type_warp(app, ctx);
    }
    let existing =
        app.session.active().and_then(|d| d.doc.layer(LayerId(app.ui.transform.as_ref()?.layer)).and_then(photocraft_engine::warp_cmds::smart_warp_doc_space));
    let Some(t) = app.ui.transform.as_mut() else { return };
    // Warp moves layers and paths (#3027): a lone mask or channel keeps the box.
    if t.warp.is_some() || t.target.is_some() {
        return;
    }
    if t.quad == corners(t.rect)
        && !t.selection
        && t.path.is_none()
        && let Some(w) = existing
    {
        t.rect = w.bounds;
        t.quad = corners(w.bounds);
        t.warp = Some(w);
        return;
    }
    let h = Homography::rect_to_quad(t.rect, t.quad).unwrap_or(Homography::IDENTITY);
    let mesh = BezierMesh::identity(t.rect, 1, 1).map_points(|p| {
        let (x, y) = h.apply(p[0], p[1]);
        [x, y]
    });
    t.warp = Some(Warp::custom(mesh, t.rect));
}

/// The open transform moves an editable type layer as a whole (not a lone mask, channel, path or
/// the selection). Its Warp is Warp Text: only the styles, and the type stays editable (#3004).
pub fn warps_type(app: &PhotocraftApp) -> bool {
    let Some(t) = &app.ui.transform else { return false };
    if t.selection || t.target.is_some() || t.path.is_some() {
        return false;
    }
    app.session.active().and_then(|st| st.doc.layer(LayerId(t.layer))).is_some_and(|l| matches!(l.content, LayerContent::Text(_)))
}

/// Warp on editable type, as in Photoshop: no rasterizing, no mesh to drag, just the warp styles
/// of the options bar (Warp Text's). A box scaled or rotated first is applied first (that keeps
/// type editable). The preview shows the type without its current Warp Text, under that style.
fn enter_type_warp(app: &mut PhotocraftApp, ctx: &egui::Context) {
    if app.ui.transform.as_ref().is_some_and(|t| t.quad != corners(t.rect)) {
        commit(app);
        if app.ui.transform.is_some() {
            return; // refused: the box stays as it was
        }
        if let Err(e) = begin(app, ctx) {
            app.ui.status = e;
            return;
        }
    }
    let (Some(t), Some(st)) = (app.ui.transform.as_ref(), app.session.active()) else { return };
    let (id, session) = (LayerId(t.layer), t.session);
    let doc = st.doc.clone();
    let Some(LayerContent::Text(text)) = doc.layer(id).map(|l| &l.content) else { return };
    let current = text.warp.clone();
    let mut plain = (*doc).clone();
    if let Some(LayerContent::Text(tt)) = plain.layer_mut(id).map(|l| &mut l.content)
        && current.is_some()
    {
        tt.warp = None;
        photocraft_engine::type_cmds::refresh(&doc, tt);
    }
    let Some(b) = plain.layer(id).map(|l| photocraft_engine::transform_cmds::transform_bounds(&plain, l)).filter(|b| !b.is_empty()) else { return };
    let rect = [b.x0 as f64, b.y0 as f64, b.x1 as f64, b.y1 as f64];
    if current.is_some()
        && let Some(pv) = app.transform_preview.as_mut()
    {
        let max_side = ctx.input(|i| i.max_texture_side).max(app.gpu.as_ref().map_or(1, |g| g.max_texture_side()));
        // Read as the session's texels were: without the style when the box draws it itself.
        let (image, uv) = preview_image(&plain, id, None, b, max_side, pv.styled.is_some());
        pv.texture = crate::transform_tex::PreviewTextures::new(ctx, format!("transform-{session}"), image, uv);
    }
    let warp =
        current.as_ref().and_then(|w| WarpStyle::parse(&w.style).filter(|s| s.is_preset()).map(|s| (w, s))).map_or(Warp::none(rect), |(w, style)| Warp {
            style,
            bend: f64::from(w.value),
            h_distort: f64::from(w.horizontal_distortion),
            v_distort: f64::from(w.vertical_distortion),
            vertical: !w.horizontal,
            ..Warp::none(rect)
        });
    if let Some(t) = app.ui.transform.as_mut() {
        t.rect = rect;
        t.quad = corners(rect);
        t.pivot = [(rect[0] + rect[2]) / 2.0, (rect[1] + rect[3]) / 2.0];
        t.warp = Some(warp);
    }
}

/// Leave Warp mode (back to the box; warp edits are dropped, like Photoshop's mode toggle on an
/// untouched warp).
pub fn leave_warp(app: &mut PhotocraftApp) {
    if let Some(t) = app.ui.transform.as_mut() {
        t.warp = None;
    }
    if let Some(pv) = app.transform_preview.as_mut() {
        pv.split_tool = None;
        pv.split_pointer = None;
        pv.split_placing = false;
        pv.split_quick = false;
    }
}

/// Texels of the moving pixels over `b`: full resolution unless the long side exceeds the GPU's
/// `max_side` (#91: this used to be capped at 2048 px, so big layers previewed blurry), plus the
/// uv extent covering `b`. `bare` leaves the layer's own style, opacity and fill out of a
/// flattened render: the styled preview applies them to the moved pixels itself.
fn preview_image(
    doc: &Document,
    id: LayerId,
    lifted: Option<&photocraft_raster::Surface>,
    b: photocraft_geom::Rect,
    max_side: usize,
    bare: bool,
) -> (egui::ColorImage, [f32; 2]) {
    let layer = doc.layer(id);
    // Linked masks move with the pixels, so the moving texels carry them (#205).
    let mask = layer.and_then(|l| preview_mask(l, b));
    // Groups and type/fill layers preview from a flattened render of just that layer.
    let rendered;
    let surf: Option<&photocraft_raster::Surface> = match (lifted, layer.and_then(|l| l.surface())) {
        (Some(s), _) => Some(s),
        (None, Some(s)) if layer.is_some_and(|l| matches!(l.content, LayerContent::Raster(_))) => Some(s),
        _ => {
            let mut solo = doc.clone();
            if layer.is_some() {
                solo_layer(&mut solo.layers, id);
            } else {
                // Keep the previous fallback for an invalid id: render the document, with only
                // top-level clipping disabled. Production callers get ids from this document.
                for l in &mut solo.layers {
                    l.clipped = false;
                }
            }
            if let Some(l) = solo.layer_mut(id).filter(|_| bare) {
                l.effects.enabled = false;
                l.opacity = 1.0;
                l.fill_opacity = 1.0;
            }
            let buf = photocraft_compose::render(&solo, b);
            let mut s = photocraft_raster::Surface::new(photocraft_color::PixelFormat::RGBA8);
            let flat: Vec<f32> = buf.px.iter().flat_map(|p| *p).collect();
            s.write_region(b, &flat);
            rendered = s;
            Some(&rendered)
        }
    };
    let Some(surf) = surf else { return (egui::ColorImage::new([1, 1], vec![Color32::TRANSPARENT]), [1.0, 1.0]) };
    // A flattened render already has the masks applied.
    let raw = lifted.is_some() || layer.is_some_and(|l| matches!(l.content, LayerContent::Raster(_)));
    crate::transform_tex::read_surface(surf, mask.as_ref().filter(|_| raw), b, max_side)
}

/// The layer's masks as the moving texels carry them: the enabled masks when all of them are linked
/// (the vector mask and feathering folded in as the compositor applies them), else the linked
/// pixel mask alone. `None` when no enabled mask moves with the pixels.
fn preview_mask(l: &photocraft_doc::Layer, b: photocraft_geom::Rect) -> Option<crate::transform_tex::PreviewMask> {
    use crate::transform_tex::PreviewMask;
    let pixel = l.mask.as_ref().filter(|m| m.enabled);
    let vector = l.vector_mask.as_ref().filter(|v| v.enabled);
    let linked_pixel = pixel.filter(|m| m.linked);
    if pixel.is_none_or(|m| m.linked)
        && vector.is_none_or(|v| v.linked)
        && let Some(surface) = photocraft_compose::masks::combined_mask(l, b)
    {
        return Some(PreviewMask { surface, density: 1.0 });
    }
    linked_pixel.map(|m| PreviewMask { surface: m.surface.clone(), density: m.density })
}

/// The layers covering the moving one ([`covering`]), composited on their own and drawn over its
/// preview, so the layer keeps its place in the stack while the box is open (#2439): they cover
/// it, as in Photoshop, instead of the moving pixels covering the whole document. The canvas
/// under the box leaves them out ([`Above::hide_in`]). Built when the session starts and again
/// when an edit changes the document; the box itself moves without re-compositing.
struct Above {
    ctx: egui::Context,
    name: String,
    max_side: usize,
    /// The covering layers (whole layers or groups, at any depth).
    ids: Vec<LayerId>,
    /// Their composite and the canvas area it covers; `None` when nothing visible covers the layer.
    shown: Option<(crate::transform_tex::PreviewTextures, photocraft_geom::Rect)>,
}

impl Above {
    fn new(ctx: &egui::Context, name: String, doc: &Document, id: LayerId, max_side: usize) -> Self {
        let mut a = Self { ctx: ctx.clone(), name, max_side, ids: Vec::new(), shown: None };
        a.rebuild(doc, id);
        a
    }

    /// Composite what covers layer `id` of `doc` (the live document) again.
    fn rebuild(&mut self, doc: &Document, id: LayerId) {
        self.ids.clear();
        covering(&doc.layers, id, &mut self.ids);
        let mut up = doc.clone();
        show_only(&mut up.layers, &self.ids);
        let canvas = up.bounds();
        let rect = up
            .layers
            .iter()
            .filter(|l| l.visible)
            .fold(photocraft_geom::Rect::EMPTY, |r, l| r.union(&photocraft_compose::composite_bounds(l, canvas).unwrap_or(canvas)))
            .intersect(&canvas);
        self.shown = (!rect.is_empty()).then(|| {
            let (image, uv) = composite_image(&up, rect, self.max_side);
            (crate::transform_tex::PreviewTextures::new(&self.ctx, self.name.clone(), image, uv), rect)
        });
    }

    /// Leave the covering layers out of `layers` (the document under the box).
    fn hide_in(&self, layers: &mut [photocraft_doc::Layer]) {
        for l in layers.iter_mut() {
            if self.ids.contains(&l.id) {
                l.visible = false;
            } else if let LayerContent::Group(g) = &mut l.content {
                self.hide_in(&mut g.children);
            }
        }
    }

    /// Draw the composite over the moving pixels.
    fn draw(&self, painter: &egui::Painter, xf: &ViewXform, display: Option<&photocraft_engine::display_color::CanvasDisplay>) {
        let Some((texture, r)) = &self.shown else { return };
        let (tex, uv) = texture.pick_display(painter.ctx(), xf.zoom * painter.ctx().pixels_per_point(), 1.0, false, display);
        let mut mesh = egui::Mesh::with_texture(tex);
        let corners = [(r.x0, r.y0, 0.0, 0.0), (r.x1, r.y0, uv[0], 0.0), (r.x1, r.y1, uv[0], uv[1]), (r.x0, r.y1, 0.0, uv[1])];
        for (x, y, u, v) in corners {
            mesh.vertices.push(egui::epaint::Vertex { pos: xf.to_screen(x as f32, y as f32), uv: pos2(u, v), color: Color32::WHITE });
        }
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        painter.add(mesh);
    }
}

/// `rect` of `doc`'s composite as premultiplied texels: full resolution when it fits in
/// `max_side`, composited in bands (no full-size float copy), else area-averaged down to fit.
fn composite_image(doc: &Document, rect: photocraft_geom::Rect, max_side: usize) -> (egui::ColorImage, [f32; 2]) {
    let (w, h) = (rect.width() as usize, rect.height() as usize);
    let to_color = |p: &[u8; 4]| Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]);
    if w.max(h) <= max_side.max(1) {
        let mut px = Vec::with_capacity(w * h);
        let _ = photocraft_compose::render_bands(doc, rect, 0, |band| {
            px.extend(band.to_rgba8().pixels.as_chunks::<4>().0.iter().map(to_color));
            Ok::<(), ()>(())
        });
        px.resize(w * h, Color32::TRANSPARENT);
        return (egui::ColorImage::new([w, h], px), [1.0, 1.0]);
    }
    let k = w.max(h).div_ceil(max_side.max(1));
    let (tw, th) = (w.div_ceil(k).max(1), h.div_ceil(k).max(1));
    let buf = photocraft_compose::render_reduced_rect(doc, rect, tw as u32, th as u32);
    let px: Vec<Color32> = buf.to_rgba8().pixels.as_chunks::<4>().0.iter().map(to_color).collect();
    let [bw, bh] = [buf.rect.width() as usize, buf.rect.height() as usize];
    if px.len() != bw * bh || bw == 0 || bh == 0 {
        return (egui::ColorImage::new([1, 1], vec![Color32::TRANSPARENT]), [1.0, 1.0]);
    }
    (egui::ColorImage::new([bw, bh], px), [1.0, 1.0])
}

/// A layer that composites over anything the same as over transparency, so it can be drawn over
/// the moving pixels on its own: a Normal layer that isn't an adjustment, or a Normal or Pass
/// Through group of such layers (hidden layers count, they draw nothing). Adjustment layers and
/// the other blend modes depend on what is under them.
fn plain(l: &photocraft_doc::Layer) -> bool {
    use photocraft_color::BlendMode;
    !l.visible
        || match &l.content {
            LayerContent::Adjustment(_) => false,
            LayerContent::Group(g) => matches!(l.blend, BlendMode::Normal | BlendMode::PassThrough) && g.children.iter().all(plain),
            _ => l.blend == BlendMode::Normal,
        }
}

/// The layers over `id` in `layers` (bottom to top, groups holding `id` searched too) that cover
/// it in the preview: from the top down, while they are [`plain`]. The first layer that isn't
/// stops the search, so it and everything under it stay in the document under the box as before
/// (an adjustment there keeps applying to the layers under it). A clipping group goes as a whole,
/// and the layers clipped to `id` (or to the group holding it) stay with it.
fn covering(layers: &[photocraft_doc::Layer], id: LayerId, out: &mut Vec<LayerId>) {
    let mut end = layers.len();
    while end > 0 {
        // The clipping group ending at `end`: its base and the layers clipped to it.
        let mut start = end - 1;
        while start > 0 && layers.get(start).is_some_and(|l| l.clipped) {
            start -= 1;
        }
        let unit = layers.get(start..end).unwrap_or_default();
        if unit.iter().any(|l| contains(l, id)) {
            // A group holding `id` with nothing clipped to it: go on inside it.
            if let [g] = unit
                && g.id != id
                && let LayerContent::Group(group) = &g.content
                && matches!(g.blend, photocraft_color::BlendMode::Normal | photocraft_color::BlendMode::PassThrough)
            {
                covering(&group.children, id, out);
            }
            return;
        }
        if !unit.iter().all(plain) {
            return;
        }
        out.extend(unit.iter().map(|l| l.id));
        end = start;
    }
}

/// Hide everything in `layers` but the `keep` layers and the groups holding them.
fn show_only(layers: &mut [photocraft_doc::Layer], keep: &[LayerId]) {
    for l in layers.iter_mut() {
        if keep.contains(&l.id) {
            continue;
        }
        match &mut l.content {
            LayerContent::Group(g) if g.children.iter().any(|c| keep.iter().any(|k| contains(c, *k))) => {
                // This ancestor only clips the covering children. Its background is already
                // on the base canvas; drawing it again would cover the moving layer in white.
                if let Some(board) = &mut g.artboard {
                    board.background = photocraft_doc::ArtboardBackground::Transparent;
                }
                show_only(&mut g.children, keep);
            }
            _ => l.visible = false,
        }
    }
}

/// Leave only `id`, its descendants and the groups holding it visible. Every layer on that path
/// is detached from its clipping base because the base is hidden in this isolated render (#5239).
/// Hide every layer but `id` and the groups holding it, and unclip those, so a render shows that
/// layer alone (the Free Transform preview, Liquify's one-layer backdrop).
pub(crate) fn solo_layer(layers: &mut [photocraft_doc::Layer], id: LayerId) {
    for l in layers {
        if l.id == id {
            l.clipped = false;
            continue;
        }
        match &mut l.content {
            LayerContent::Group(g) if g.children.iter().any(|c| contains(c, id)) => {
                l.clipped = false;
                // Isolate the child, not the artboard's paper. Keep the board for clipping.
                if let Some(board) = &mut g.artboard {
                    board.background = photocraft_doc::ArtboardBackground::Transparent;
                }
                solo_layer(&mut g.children, id);
            }
            _ => l.visible = false,
        }
    }
}

fn contains(l: &photocraft_doc::Layer, id: LayerId) -> bool {
    l.id == id || matches!(&l.content, LayerContent::Group(g) if g.children.iter().any(|c| contains(c, id)))
}

/// Undo and Redo while the commit runs in the background (Free Transform, or Warp).
const STILL_APPLYING: &str = "Free Transform is still being applied";
const STILL_WARPING: &str = "Warp is still being applied";

pub fn commit(app: &mut PhotocraftApp) {
    if committing(app).is_some() {
        return;
    }
    let document = app.transform_preview.as_ref().map(|pv| pv.doc.id);
    if document.is_some_and(|id| app.session.active().is_none_or(|st| st.doc.id != id)) {
        cancel(app);
        return;
    }
    let type_layer = warps_type(app);
    let Some(t) = app.ui.transform.take() else { return };
    let mut preview = app.transform_preview.take();
    if let Some(options) = &t.content_aware {
        crate::cas_transform::commit(app, &t, options);
        return;
    }
    if t.selection {
        let mut p = json!({"rect": t.rect, "interpolation": t.interpolation});
        match &t.warp {
            Some(w) if !w.is_identity() => p["warp"] = json!(w),
            Some(_) => return,
            None if t.quad == corners(t.rect) => return,
            None => p["quad"] = json!(t.quad),
        }
        if let Err(e) = app.run("select.transformSelection", p) {
            keep_open(app, t, preview, e);
        }
        return;
    }
    if let Some(target) = &t.path {
        let mut p = target.clone();
        match &t.warp {
            Some(w) if w.is_identity() => return,
            Some(w) => p["warp"] = json!(w),
            None if t.quad == corners(t.rect) => return,
            // Distort and Perspective map the path through the box (#3027).
            None => match rect_to_quad_affine(t.rect, t.quad) {
                Some(a) => p["matrix"] = json!(a.m),
                None => {
                    p["rect"] = json!(t.rect);
                    p["quad"] = json!(t.quad);
                }
            },
        }
        if let Err(e) = app.run("path.transform", p) {
            keep_open(app, t, preview, e);
        }
        return;
    }
    let made = t.made;
    let r = if let Some(w) = &t.warp {
        // Type with a Warp Text can be warped back to None; the engine skips an unchanged style.
        if w.is_identity() && !type_layer {
            return;
        }
        app.run("edit.transform.warp", json!({"layer": t.layer, "rect": t.rect, "warp": w, "interpolation": t.interpolation}))
    } else {
        if t.quad == corners(t.rect) {
            return; // untouched: nothing to do and no history step; a ⌥⌘T copy or a place stays
        }
        let mut p = json!({"layer": t.layer, "rect": t.rect, "quad": t.quad, "interpolation": t.interpolation});
        if let Some(target) = &t.target {
            p["target"] = target.clone();
        }
        app.run("edit.transform", p)
    };
    // A large layer's warp runs in the background: the session stays until the job ends.
    if let Ok(v) = &r
        && let Some(job) = v.get("pending").and_then(serde_json::Value::as_bool).filter(|p| *p).and(v.get("job")).and_then(serde_json::Value::as_u64)
        && let Some(pv) = preview.as_mut()
    {
        pv.committing = Some(JobId(job));
        app.ui.transform = Some(t);
        app.transform_preview = preview;
        return;
    }
    match r {
        // Nothing changed (an untouched cylinder or Warp Text): no step to fold a copy into, so
        // it stays its own step, as for an untouched box.
        Ok(v) if v.get("changed") == Some(&json!(false)) => {}
        Ok(_) => finish(app, document, LayerId(t.layer), made, true),
        Err(e) => keep_open(app, t, preview, e),
    }
}

/// After the transform landed or not: fold the layer the session made into its step, or take it
/// back.
fn finish(app: &mut PhotocraftApp, document: Option<DocId>, layer: LayerId, made: Option<MadeLayer>, landed: bool) {
    let (Some(made), Some(document)) = (made, document) else { return };
    if landed {
        fold_made(app, document, layer, made);
    } else {
        take_back_made(app, document, layer);
    }
}

/// The job applying the transform, while it runs.
pub(crate) fn committing(app: &PhotocraftApp) -> Option<JobId> {
    app.transform_preview.as_ref().and_then(|pv| pv.committing)
}

/// A job ended (see [`crate::jobs_ui`]): when it was the commit, the session ends with it. Its
/// status and errors are reported like any job's.
pub fn on_job_event(app: &mut PhotocraftApp, e: &JobEvent) {
    if committing(app) != Some(e.id) {
        return;
    }
    let (Some(t), Some(mut pv)) = (app.ui.transform.take(), app.transform_preview.take()) else { return };
    // A refused commit (a result over the pixel budget, #3773) keeps the box open, as inline,
    // unless picking another tool or layer started it: then the box ends, as Esc would.
    if let JobOutcome::Failed(msg) = &e.outcome {
        pv.committing = None;
        keep_open(app, t, Some(pv), msg.clone());
        if switched_away(app) {
            cancel(app);
        }
        return;
    }
    finish(app, Some(pv.doc.id), LayerId(t.layer), t.made, matches!(e.outcome, JobOutcome::Done(_)));
}

/// A commit the engine refused (a result over the pixel budget, #3773): the box stays open as it
/// was, with the reason in the status bar, so the transform can be changed or cancelled. A copy or
/// a placed layer it made goes only when it is cancelled.
fn keep_open(app: &mut PhotocraftApp, t: TransformSession, preview: Option<TransformPreview>, e: String) {
    app.ui.transform = Some(t);
    app.transform_preview = preview;
    app.ui.status = e;
}

/// A transform whose layer or document went away (undo, close) ends silently; picking another tool
/// or selecting another layer (Layers panel, ⌘-click, the control channel) applies it, so the box
/// never stays on a layer that is no longer the active one (#1400). Checked every frame and before
/// each pointer event, so a press with a tool chosen just before it (`ui.pointer`'s `tool`) goes to
/// that tool.
pub fn end_if_left(app: &mut PhotocraftApp) {
    let Some(t) = &app.ui.transform else { return };
    if committing(app).is_some() {
        return;
    }
    let Some(st) = app.session.active() else { return cancel(app) };

    let layer = LayerId(t.layer);
    let made = t.made;
    let preview_doc_id = app.transform_preview.as_ref().map(|pv| pv.doc.id);
    let switched_document = preview_doc_id.is_some_and(|id| id != st.doc.id);

    let editing_placed_smart_object = switched_document
        && made == Some(MadeLayer::Place)
        && preview_doc_id
            .is_some_and(|parent_id| app.session.smart_links.iter().any(|link| link.parent == parent_id && link.child == st.doc.id && link.layer == layer));

    if editing_placed_smart_object {
        app.ui.transform = None;
        app.transform_preview = None;
    } else if switched_document || st.doc.layer(layer).is_none() {
        cancel(app);
    } else if switched_away(app) {
        commit(app);
        // Picking another tool or layer can't leave a refused transform open: it ends, as Esc would.
        // A commit still running in the background isn't refused: cancelling it would throw the
        // transform away (and a dropped file with it, #5665). It ends the session when it lands,
        // or ends it as Esc would if it is refused then (`on_job_event`).
        if app.ui.transform.is_some() && committing(app).is_none() {
            cancel(app);
        }
    }
}

/// Another tool picked, or (outside Transform Selection) another layer selected, since the box
/// opened: the session ends ([`end_if_left`], #1400).
fn switched_away(app: &PhotocraftApp) -> bool {
    let (Some(t), Some(st)) = (&app.ui.transform, app.session.active()) else { return false };
    app.transform_preview.as_ref().is_some_and(|pv| pv.tool != app.ui.tool) || (!t.selection && st.active_layer.is_some_and(|active| active.0 != t.layer))
}

/// Edits made while the box is open show in the preview at once, not only after the commit
/// (#1384): the moving layer's opacity, fill and visibility tint the moving pixels, and the
/// document under them (other layers, and the part of the layer a selection leaves behind) is
/// rebuilt from the current one. The moving pixels keep the session's. Called before the canvas
/// reads the preview, so it never caches a revision with the old preview.
pub fn follow_edits(app: &mut PhotocraftApp) {
    let (Some(t), Some(pv)) = (&app.ui.transform, app.transform_preview.as_mut()) else { return };
    let Some(follow) = pv.follow.as_mut() else { return };
    let Some(st) = app.session.active().filter(|st| st.doc.id == pv.doc.id && st.revision != follow.revision) else { return };
    let id = LayerId(t.layer);
    let (Some(live), Some(moving)) = (st.doc.layer(id), pv.doc.layer(id)) else { return };
    let mut pd = (*st.doc).clone();
    if let Some(l) = pd.layer_mut(id) {
        l.content = moving.content.clone();
        l.visible = l.visible && !follow.hidden;
    }
    if let Some(a) = pv.above.as_mut() {
        a.rebuild(&st.doc, id);
        a.hide_in(&mut pd.layers);
    }
    pv.opacity = if live.visible { live.opacity * live.fill_opacity } else { 0.0 };
    follow.revision = st.revision;
    pv.doc = Arc::new(pd);
}

pub fn cancel(app: &mut PhotocraftApp) {
    // Esc while the commit runs cancels the job; the session ends when it does.
    if let Some(job) = committing(app) {
        crate::jobs_ui::cancel(app, job);
        return;
    }
    let transform = app.ui.transform.take();
    let preview = app.transform_preview.take();
    if let (Some(t), Some(pv)) = (transform, preview)
        && t.made.is_some()
    {
        take_back_made(app, pv.doc.id, LayerId(t.layer));
    }
}

/// The session's starting box: where Undo stops.
fn start_steps(app: &mut PhotocraftApp) {
    if let (Some(t), Some(pv)) = (&app.ui.transform, app.transform_preview.as_mut()) {
        pv.steps = Steps { settled: Some(Step::of(t)), ..Steps::default() };
    }
}

/// Record the box as an undo step whenever it comes to rest in a new state: once per drag (on
/// release), nudge or option edit (once typing in its field ends). Call once per frame.
pub fn track_steps(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let (Some(t), Some(pv)) = (&app.ui.transform, app.transform_preview.as_mut()) else { return };
    if pv.gesture.is_some() || pv.warp_drag.is_some() || ctx.input(|i| i.pointer.any_down()) || ctx.text_edit_focused() {
        // Whatever comes to rest after this is more than the nudge.
        pv.steps.fold = false;
        return;
    }
    pv.steps.settle(Step::of(t));
}

/// Arrow keys while Free Transform is active: move the box (and its pivot) by `(dx, dy)`.
/// Nudges that follow each other closely are one Undo step of the session (`nudge_bundle`).
pub(crate) fn nudge(app: &mut PhotocraftApp, dx: f64, dy: f64) {
    let (pause, now) = (crate::nudge_bundle::pause(app), app.nudge_bundle.now);
    let Some(t) = app.ui.transform.as_mut().filter(|t| t.warp.is_none()) else { return };
    let before = Step::of(t);
    t.quad = t.quad.map(|q| [q[0] + dx, q[1] + dy]);
    t.pivot = [t.pivot[0] + dx, t.pivot[1] + dy];
    if let Some(pv) = app.transform_preview.as_mut() {
        pv.steps.nudged(&before, Step::of(t), now, pause);
    }
}

/// Save for Web (Legacy): refused under an open box, as it would export without the preview (#6737).
pub(crate) const SAVE_FOR_WEB: &str = "file.export.saveForWebLegacy";

/// While transforming, Undo and Redo apply to the box: `Some(enabled)` for those commands (and
/// Toggle Last State, which would change the document under the box), `None` for the rest.
pub fn is_enabled(app: &PhotocraftApp, id: &str) -> Option<bool> {
    let t = app.ui.transform.as_ref()?;
    if committing(app).is_some() {
        return box_command(id).then_some(false);
    }
    let steps = app.transform_preview.as_ref().map(|pv| &pv.steps);
    match id {
        // Content-Aware Scale's box only scales: no switching it to another transform or Warp.
        "edit.freeTransform" | "edit.freeTransformCopy" if t.content_aware.is_some() => Some(false),
        i if i.starts_with("edit.transform.") && t.content_aware.is_some() => Some(false),
        "edit.undo" => Some(steps.is_some_and(|s| s.can_undo(&Step::of(t)))),
        "edit.redo" => Some(steps.is_some_and(|s| !s.redo.is_empty())),
        // No Toggle Last State or history jump under the box, and one box at a time.
        "edit.toggleLastState" | "history.selectState" | "edit.contentAwareScale" | "edit.puppetWarp" => Some(false),
        // Save for Web exports the committed document and would ignore the pending box (#6737).
        SAVE_FOR_WEB => Some(false),
        // Rotate and Flip turn the box (not while warping, where the mesh is the box).
        _ if box_preset(id).is_some() => Some(t.warp.is_none()),
        // Editable type can't be distorted (#2630, as #2701 rules for the menus outside a box).
        "edit.transform.distort" | "edit.transform.perspective" if moves_type(app, t) => Some(false),
        _ => None,
    }
}

/// Does the box move an editable type layer (not its selection outline, a lone mask or a path)?
fn moves_type(app: &PhotocraftApp, t: &TransformSession) -> bool {
    !t.selection
        && t.target.is_none()
        && t.path.is_none()
        && app.session.active().and_then(|st| st.doc.layer(LayerId(t.layer))).is_some_and(|l| matches!(l.content, LayerContent::Text(_)))
}

/// Edit › Transform's Rotate and Flip presets while a box is open: what they do to the box, as
/// `(kx, ky, angle)`: a scale along the box's own axes, then a rotation (radians, clockwise on
/// screen), both about the reference point.
fn box_preset(id: &str) -> Option<(f64, f64, f64)> {
    use std::f64::consts::{FRAC_PI_2, PI};
    Some(match id {
        "edit.transform.rotate180" => (1.0, 1.0, PI),
        "edit.transform.rotate90Cw" => (1.0, 1.0, FRAC_PI_2),
        "edit.transform.rotate90Ccw" => (1.0, 1.0, -FRAC_PI_2),
        "edit.transform.flipHorizontal" => (-1.0, 1.0, 0.0),
        "edit.transform.flipVertical" => (1.0, -1.0, 0.0),
        _ => return None,
    })
}

/// While transforming, the box owns the history for every caller (menus, shortcuts, the control
/// channel, MCP): Undo and Redo step through it, and Toggle Last State and History state jumps,
/// which would change the document under the box, are refused. Edit › Transform's Rotate and
/// Flip turn the box itself, as in Photoshop, instead of the layer under it; committing stays
/// with ↩ and the options bar.
/// `None` for other commands or when not transforming.
/// The commands the open box answers itself ([`is_enabled`], [`intercept`]).
fn box_command(id: &str) -> bool {
    matches!(id, "edit.undo" | "edit.redo" | "edit.toggleLastState" | "edit.contentAwareScale" | "edit.puppetWarp" | SAVE_FOR_WEB) || box_preset(id).is_some()
}

pub fn intercept(app: &mut PhotocraftApp, id: &str) -> Option<Result<serde_json::Value, String>> {
    let warping = app.ui.transform.as_ref()?.warp.is_some();
    if committing(app).is_some() {
        return box_command(id).then(|| Err(if warping { STILL_WARPING } else { STILL_APPLYING }.into()));
    }
    match id {
        "edit.undo" => Some(Ok(step(app, false))),
        "edit.redo" => Some(Ok(step(app, true))),
        "edit.toggleLastState" | "history.selectState" | "edit.contentAwareScale" | "edit.puppetWarp" | SAVE_FOR_WEB => {
            Some(Err("Commit or cancel the transform first".into()))
        }
        _ => {
            let (kx, ky, angle) = box_preset(id)?;
            if warping {
                return Some(Err("Commit or cancel the transform first".into()));
            }
            scale_about_pivot(app, kx, ky);
            rotate_about_pivot(app, angle);
            Some(Ok(json!({"transform": app.ui.transform})))
        }
    }
}

/// Undo (or redo) one step of the open session; nothing mid-drag or with no step to take.
fn step(app: &mut PhotocraftApp, redo: bool) -> serde_json::Value {
    let Some(t) = app.ui.transform.as_mut() else { return serde_json::Value::Null };
    let Some(pv) = app.transform_preview.as_mut().filter(|pv| pv.gesture.is_none() && pv.warp_drag.is_none()) else {
        return serde_json::Value::Null;
    };
    let s = &mut pv.steps;
    s.settle(Step::of(t));
    let (from, to) = if redo { (&mut s.redo, &mut s.undo) } else { (&mut s.undo, &mut s.redo) };
    let Some(step) = from.pop() else { return serde_json::Value::Null };
    to.push(Step::of(t));
    s.settled = Some(step.clone());
    step.restore(t);
    json!({"transform": t})
}

/// Which part of the box a document point hits.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Hit {
    Corner(usize),
    Edge(usize),
    Pivot,
    Inside,
    Outside,
}

/// The nearest handle within `tol` (on a small box their grab areas overlap), else inside or
/// outside the box.
pub(crate) fn hit(t: &TransformSession, p: [f64; 2], tol: f64) -> Hit {
    let d = |a: [f64; 2]| ((a[0] - p[0]).powi(2) + (a[1] - p[1]).powi(2)).sqrt();
    let mid = |i: usize| {
        let (a, b) = (t.quad[i], t.quad[(i + 1) % 4]);
        [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0]
    };
    let handles = (0..4).map(|i| (t.quad[i], Hit::Corner(i))).chain((0..4).map(|i| (mid(i), Hit::Edge(i)))).chain([(t.pivot, Hit::Pivot)]);
    let nearest = handles.map(|(q, h)| (d(q), h)).filter(|(dist, _)| *dist < tol).min_by(|a, b| a.0.total_cmp(&b.0));
    match nearest {
        Some((_, h)) => h,
        None if inside(&t.quad, p) => Hit::Inside,
        None => Hit::Outside,
    }
}

fn inside(q: &[[f64; 2]; 4], p: [f64; 2]) -> bool {
    let mut c = false;
    for i in 0..4 {
        let (a, b) = (q[i], q[(i + 3) % 4]);
        if (a[1] > p[1]) != (b[1] > p[1]) && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0] {
            c = !c;
        }
    }
    c
}

/// Transient drag state (kept in egui memory; not part of the serialised session).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Gesture {
    pub(crate) hit: Hit,
    pub(crate) start: [f64; 2],
    pub(crate) quad0: [[f64; 2]; 4],
    pub(crate) pivot0: [f64; 2],
}

/// A click outside the box accepts the transform; a drag outside still rotates it.
/// Decide on release, so the rotation zone keeps working for drags. The screen-sized
/// click tolerance absorbs pointer jitter without accepting a real rotation gesture.
fn accepts_outside_click(g: Gesture, end: [f64; 2], tol: f64) -> bool {
    g.hit == Hit::Outside && (end[0] - g.start[0]).hypot(end[1] - g.start[1]) <= 2.0 * tol / HANDLE_PX
}

/// Pointer input while transforming. Returns false when no transform is active.
pub fn pointer(app: &mut PhotocraftApp, ev: ToolEvent, mods: egui::Modifiers) -> bool {
    let Some(t) = app.ui.transform.clone() else { return false };
    if committing(app).is_some() {
        return true;
    }
    let tol = handle_tolerance(app);
    if t.warp.is_some() {
        warp_pointer(app, ev, tol, mods);
        return true;
    }
    let Some(pv) = app.transform_preview.as_mut() else { return false };
    match ev {
        ToolEvent::Down { x, y, .. } => {
            let mut h = hit(&t, [x, y], tol);
            // ⌥-click away from the handles moves the reference point there (and drags it).
            if mods.alt
                && matches!(h, Hit::Inside | Hit::Outside)
                && let Some(s) = app.ui.transform.as_mut()
            {
                s.pivot = [x, y];
                h = Hit::Pivot;
            }
            // Distort only moves corners (and the whole box from inside): no rotating, no edges.
            // Even in Distort mode, remember an outside press so a plain click can
            // accept the transform. A drag outside remains inert in that mode, in Skew, and in
            // Content-Aware Scale, which only scales and moves.
            pv.gesture = (mode_allows(t.mode, h) || h == Hit::Outside).then_some(Gesture { hit: h, start: [x, y], quad0: t.quad, pivot0: t.pivot });
        }
        ToolEvent::Move { x, y, .. } | ToolEvent::Up { x, y } => {
            let g = pv.gesture;
            if matches!(ev, ToolEvent::Up { .. }) {
                pv.gesture = None;
            }
            // Clicking an empty spot commits instead of silently arming rotation (#2851).
            // Dragging the rotation zone and Option-clicking to move the pivot
            // keep their existing behavior.
            if matches!(ev, ToolEvent::Up { .. }) && g.is_some_and(|g| accepts_outside_click(g, [x, y], tol)) {
                commit(app);
                return true;
            }
            let type_layer = moves_type(app, &t);
            let adjusted = g.map(|g| drag_mods(app, &t, g.hit, mods));
            if let (Some(g), Some((mode, mods)), Some(s)) = (g, adjusted, app.ui.transform.as_mut())
                && mode_allows(t.mode, g.hit)
                && (t.content_aware.is_none() || g.hit != Hit::Outside)
            {
                if t.content_aware.is_some() {
                    let mut mods = mode_mods(mode, g.hit, mods);
                    // Content-Aware Scale: ⌘ neither skews nor distorts.
                    mods.command = false;
                    apply_drag(s, g, [x, y], mods);
                } else {
                    apply_mode_drag(s, g, [x, y], mode, type_layer, mods);
                }
                round_to_pixel_grid(app);
            }
        }
    }
    crate::snap_ui::retain_transform_lines(app, mods);
    true
}

/// Preferences › Tools › Snap Vector Tools and Transforms to Pixel Grid (#5751): while a shape
/// layer or a path is moved or scaled (the box still an upright rectangle), its edges stay on
/// whole pixels, whatever the tool. Rounding the pointer alone left the edges off by the grab
/// offset, and a proportional scale off by its ratio: a shape dragged to 540 px came out 539.6.
/// Rotated, skewed and distorted boxes are left as dragged.
fn round_to_pixel_grid(app: &mut PhotocraftApp) {
    if !app.session.prefs().tools.snap_vector_tools_and_transforms_to_pixel_grid {
        return;
    }
    let Some(t) = app.ui.transform.as_ref() else { return };
    if t.selection || t.warp.is_some() || t.content_aware.is_some() || t.target.is_some() {
        return;
    }
    let vector =
        t.path.is_some() || app.session.active().and_then(|st| st.doc.layer(LayerId(t.layer))).is_some_and(|l| matches!(l.content, LayerContent::Shape(_)));
    if !vector || !upright(&t.quad) {
        return;
    }
    let q = t.quad.map(|p| p.map(f64::round));
    // A box narrower than a pixel keeps its size rather than collapsing.
    if (q[1][0] - q[0][0]).abs() < 1.0 || (q[3][1] - q[0][1]).abs() < 1.0 || q.iter().flatten().any(|v| !v.is_finite()) {
        return;
    }
    if let Some(s) = app.ui.transform.as_mut() {
        s.quad = q;
    }
}

/// Whether corners clockwise from top-left form an axis-aligned rectangle (flips included).
fn upright(q: &[[f64; 2]; 4]) -> bool {
    let near = |a: f64, b: f64| (a - b).abs() < 1e-6;
    near(q[0][1], q[1][1]) && near(q[2][1], q[3][1]) && near(q[0][0], q[3][0]) && near(q[1][0], q[2][0])
}

/// The same effective modifiers drive the geometry and decide whether Ctrl is a distortion
/// gesture rather than the ordinary snap override. Keep type and Content-Aware Scale refusals.
fn drag_mods(app: &PhotocraftApp, t: &TransformSession, hit: Hit, mods: egui::Modifiers) -> (TransformMode, egui::Modifiers) {
    let (mode, mods) = if distort_refused(app, t) { scale_not_distort(t.mode, hit, mods) } else { (t.mode, mods) };
    let legacy = app.session.prefs().general.use_legacy_free_transform;
    (mode, legacy_mods(legacy, hit, unlinked_mods(app.ui.transform_unlinked, legacy, hit, mods)))
}

/// Only a freely distorted corner can snap its proposed position. Perspective moves paired
/// corners on one axis; Skew, Warp and Content-Aware Scale have their own constrained geometry.
pub(crate) fn corner_distorts(app: &PhotocraftApp, index: usize, mods: egui::Modifiers) -> bool {
    let Some(t) = app.ui.transform.as_ref() else { return false };
    if t.quad.get(index).is_none() || t.warp.is_some() || t.content_aware.is_some() || t.mode == TransformMode::Skew {
        return false;
    }
    let (mode, mods) = drag_mods(app, t, Hit::Corner(index), mods);
    let mods = mode_mods(mode, Hit::Corner(index), mods);
    mods.command && !(mods.alt && mods.shift)
}

/// Editable type can be scaled, rotated and skewed but not distorted (#2630): the rule that greys
/// Distort and Perspective in `enable_rules::disabled`, for the box's own layer. A box on a path,
/// on the selection outline or on a lone mask or channel moves no type.
fn distort_refused(app: &PhotocraftApp, t: &TransformSession) -> bool {
    t.path.is_none()
        && !t.selection
        && t.target.is_none()
        && app.session.active().and_then(|st| st.doc.layer(LayerId(t.layer))).is_some_and(|l| matches!(l.content, LayerContent::Text(_)))
}

/// Where Distort and Perspective are refused ([`distort_refused`]), ⌘-drag and ⌘⌥⇧-drag of a
/// corner scale like a plain corner drag (with ⌥ and ⇧ keeping their meaning), so the box never
/// takes a shape the commit would refuse (#3010).
fn scale_not_distort(mode: TransformMode, hit: Hit, mut mods: egui::Modifiers) -> (TransformMode, egui::Modifiers) {
    if matches!(hit, Hit::Corner(_)) {
        mods.command = false;
    }
    let mode = if matches!(mode, TransformMode::Distort | TransformMode::Perspective) { TransformMode::Free } else { mode };
    (mode, mods)
}

/// Preferences › General › Use Legacy Free Transform, Photoshop's scaling before CC 2019: corner
/// drags stretch freely and ⇧ keeps the proportions, edge drags stretch their one axis whatever
/// the keys. The default is proportional for both, with ⇧ freeing them.
pub(crate) fn legacy_mods(legacy: bool, hit: Hit, mut mods: egui::Modifiers) -> egui::Modifiers {
    if legacy && !mods.command {
        match hit {
            Hit::Corner(_) => mods.shift = !mods.shift,
            Hit::Edge(_) => mods.shift = true,
            _ => {}
        }
    }
    mods
}

/// The options bar's Maintain Aspect Ratio link turned off: corner and edge drags stretch freely
/// and ⇧ keeps the proportions, as in Photoshop CC 2019 and later (#4174). The legacy preference
/// already works that way, so it is left alone there.
pub(crate) fn unlinked_mods(unlinked: bool, legacy: bool, hit: Hit, mut mods: egui::Modifiers) -> egui::Modifiers {
    if unlinked && !legacy && !mods.command && matches!(hit, Hit::Corner(_) | Hit::Edge(_)) {
        mods.shift = !mods.shift;
    }
    mods
}

/// Distort / Perspective modes: a corner drag acts as if that gesture's keys were held (⌘-drag a
/// corner distorts, ⌘⌥⇧-drag a corner adds perspective). Skew has its own ([`apply_mode_drag`]).
fn mode_mods(mode: TransformMode, hit: Hit, mut mods: egui::Modifiers) -> egui::Modifiers {
    match (mode, hit) {
        (TransformMode::Distort, Hit::Corner(_)) => mods.command = true,
        (TransformMode::Perspective, Hit::Corner(_)) => {
            mods.command = true;
            mods.alt = true;
            mods.shift = true;
        }
        _ => {}
    }
    mods
}

/// A handle drag in Edit › Transform `mode` (`Free` for Free Transform). `type_layer`: the box
/// moves editable type, which only takes affine transforms.
pub(crate) fn apply_mode_drag(s: &mut TransformSession, g: Gesture, p: [f64; 2], mode: TransformMode, type_layer: bool, mut mods: egui::Modifiers) {
    if !mode_allows(mode, g.hit) {
        return;
    }
    match (mode, g.hit) {
        // Skew, as in Photoshop (#3437): a corner moves alone along one of its two edges,
        // whichever the drag mostly follows; ⌥ moves the opposite corner the other way.
        (TransformMode::Skew, Hit::Corner(i)) if i < 4 => {
            let (d, q0) = ([p[0] - g.start[0], p[1] - g.start[1]], g.quad0);
            // Corners 0..3 = TL, TR, BR, BL: the neighbour along the top/bottom edge, and along
            // the left/right one.
            let along = |j: usize| {
                let e = [q0[i][0] - q0[j][0], q0[i][1] - q0[j][1]];
                let l = e[0].hypot(e[1]);
                let u = if l > 0.0 { [e[0] / l, e[1] / l] } else { [0.0, 0.0] };
                let t = d[0] * u[0] + d[1] * u[1];
                [t * u[0], t * u[1]]
            };
            let (hn, vn) = ([1, 0, 3, 2][i], [3, 2, 1, 0][i]);
            let (h, v) = (along(hn), along(vn));
            let (m, partner) = if h[0].hypot(h[1]) >= v[0].hypot(v[1]) { (h, hn) } else { (v, vn) };
            // Editable type stays a parallelogram: the corner's whole edge slides with it.
            let moved: &[usize] = if type_layer { &[i, partner] } else { &[i] };
            let mut q = q0;
            for k in 0..4 {
                let sign = if moved.contains(&k) {
                    1.0
                } else if mods.alt && (if type_layer { true } else { k == (i + 2) % 4 }) {
                    // ⌥: the opposite corner (on type, the opposite edge) the other way.
                    -1.0
                } else {
                    continue;
                };
                if let (Some(dst), Some(src)) = (q.get_mut(k), q0.get(k)) {
                    *dst = [src[0] + sign * m[0], src[1] + sign * m[1]];
                }
            }
            s.quad = keep_convex(q0, q);
            s.pivot = carry_pivot(q0, s.quad, g.pivot0);
        }
        // Rotate (#3941): a corner or edge turns about the reference point, the same drag as the
        // zone just outside the box. Free Transform still scales from those handles.
        (TransformMode::Rotate, Hit::Corner(_) | Hit::Edge(_)) => {
            let mut g = g;
            g.hit = Hit::Outside;
            apply_drag(s, g, p, mods);
        }
        // Skew: an edge slides only along itself (⌘⇧ in Free Transform).
        (TransformMode::Skew, Hit::Edge(_)) => {
            mods.command = true;
            mods.shift = true;
            apply_drag(s, g, p, mods);
        }
        _ => apply_drag(s, g, p, mode_mods(mode, g.hit, mods)),
    }
}

/// What a drag can do in each Edit › Transform mode: in Distort only the corners (and moving the
/// box from inside); in Skew everything but rotating from outside the box (#3437). A click
/// outside still commits.
fn mode_allows(mode: TransformMode, h: Hit) -> bool {
    match mode {
        TransformMode::Distort => !matches!(h, Hit::Outside | Hit::Edge(_)),
        TransformMode::Skew => h != Hit::Outside,
        _ => true,
    }
}

/// The turn at each corner of a quad (cross products of consecutive edges): all the same sign
/// for a convex quad, mixed for a concave or self-intersecting one.
fn turns(q: &[[f64; 2]; 4]) -> [f64; 4] {
    std::array::from_fn(|k| {
        let (a, b, c) = (q[k], q[(k + 1) % 4], q[(k + 2) % 4]);
        (b[0] - a[0]) * (c[1] - b[1]) - (b[1] - a[1]) * (c[0] - b[0])
    })
}

/// Is `q` convex with the orientation `sign`, every corner turning by more than a sliver?
fn convex(q: &[[f64; 2]; 4], sign: f64, min_turn: f64) -> bool {
    turns(q).iter().all(|t| t * sign > min_turn)
}

/// Distort and Perspective stop a corner where the box would turn concave or fold over itself,
/// as Photoshop does (#1323): the result moves from `from` towards `to` as far as the quad stays
/// convex. A box that is not convex to begin with is left free.
fn keep_convex(from: [[f64; 2]; 4], to: [[f64; 2]; 4]) -> [[f64; 2]; 4] {
    let t0 = turns(&from);
    let sign = t0[0].signum();
    // A sliver: a thousandth of the starting box's smallest turn, so the stop is short of flat.
    let min_turn = t0.iter().map(|t| t.abs()).fold(f64::INFINITY, f64::min) * 1e-3;
    if !(min_turn.is_finite() && min_turn > 0.0 && convex(&from, sign, 0.0)) || convex(&to, sign, min_turn) {
        return to;
    }
    let lerp = |t: f64| -> [[f64; 2]; 4] { std::array::from_fn(|k| [from[k][0] + (to[k][0] - from[k][0]) * t, from[k][1] + (to[k][1] - from[k][1]) * t]) };
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    for _ in 0..40 {
        let mid = (lo + hi) / 2.0;
        if convex(&lerp(mid), sign, min_turn) { lo = mid } else { hi = mid }
    }
    lerp(lo)
}

pub(crate) fn apply_drag(s: &mut TransformSession, g: Gesture, p: [f64; 2], mods: egui::Modifiers) {
    let (dx, dy) = (p[0] - g.start[0], p[1] - g.start[1]);
    match g.hit {
        Hit::Inside => {
            // ⇧ locks the move to the axes and diagonals; the current offset keeps the lock stable.
            let (dx, dy) = if mods.shift {
                let prev = [s.quad[0][0] - g.quad0[0][0], s.quad[0][1] - g.quad0[0][1]];
                let d = crate::move_mods::constrain([dx, dy], Some(prev), crate::move_mods::TRANSFORM_DIRECTIONS);
                (d[0], d[1])
            } else {
                (dx, dy)
            };
            s.quad = g.quad0.map(|q| [q[0] + dx, q[1] + dy]);
            s.pivot = [g.pivot0[0] + dx, g.pivot0[1] + dy];
        }
        Hit::Pivot => s.pivot = p,
        Hit::Outside => {
            let a0 = (g.start[1] - g.pivot0[1]).atan2(g.start[0] - g.pivot0[0]);
            let a1 = (p[1] - g.pivot0[1]).atan2(p[0] - g.pivot0[0]);
            let mut da = a1 - a0;
            if mods.shift {
                // Snap the resulting angle to 15° steps.
                let base = (g.quad0[1][1] - g.quad0[0][1]).atan2(g.quad0[1][0] - g.quad0[0][0]);
                let step = 15f64.to_radians();
                da = ((base + da) / step).round() * step - base;
            }
            let (sn, cs) = da.sin_cos();
            let c = g.pivot0;
            s.quad = g.quad0.map(|q| {
                let (x, y) = (q[0] - c[0], q[1] - c[1]);
                [c[0] + x * cs - y * sn, c[1] + x * sn + y * cs]
            });
        }
        Hit::Corner(i) if mods.command && mods.alt && mods.shift => {
            // Perspective: the corner moves along the dominant axis and the corner sharing that
            // side moves the opposite way.
            s.quad = g.quad0;
            let horizontal = dx.abs() >= dy.abs();
            // Corners 0..3 = TL, TR, BR, BL: horizontal drags pair along the top/bottom edge,
            // vertical ones along the left/right edge.
            let j = match (i, horizontal) {
                (0, true) => 1,
                (1, true) => 0,
                (2, true) => 3,
                (3, true) => 2,
                (0, false) => 3,
                (3, false) => 0,
                (1, false) => 2,
                _ => 1,
            };
            let (mx, my) = if horizontal { (dx, 0.0) } else { (0.0, dy) };
            let mut q = g.quad0;
            q[i] = [g.quad0[i][0] + mx, g.quad0[i][1] + my];
            q[j] = [g.quad0[j][0] - mx, g.quad0[j][1] - my];
            s.quad = keep_convex(g.quad0, q);
            s.pivot = carry_pivot(g.quad0, s.quad, g.pivot0);
        }
        Hit::Corner(i) if mods.command => {
            // Distort: move the corner freely, up to where the box would fold over (#1323).
            let mut q = g.quad0;
            q[i] = [g.quad0[i][0] + dx, g.quad0[i][1] + dy];
            s.quad = keep_convex(g.quad0, q);
            s.pivot = carry_pivot(g.quad0, s.quad, g.pivot0);
        }
        Hit::Edge(i) if mods.command => {
            // Skew: the edge's two corners move together (⇧: only along the edge).
            let (a, b) = (i, (i + 1) % 4);
            let (mut mx, mut my) = (dx, dy);
            if mods.shift {
                let e = [g.quad0[b][0] - g.quad0[a][0], g.quad0[b][1] - g.quad0[a][1]];
                let l2 = e[0] * e[0] + e[1] * e[1];
                let t = if l2 > 0.0 { (dx * e[0] + dy * e[1]) / l2 } else { 0.0 };
                (mx, my) = (t * e[0], t * e[1]);
            }
            s.quad = g.quad0;
            s.quad[a] = [g.quad0[a][0] + mx, g.quad0[a][1] + my];
            s.quad[b] = [g.quad0[b][0] + mx, g.quad0[b][1] + my];
            if mods.alt {
                // ⌥: the opposite edge skews the other way (about the centre).
                let (c, d) = ((i + 2) % 4, (i + 3) % 4);
                s.quad[c] = [g.quad0[c][0] - mx, g.quad0[c][1] - my];
                s.quad[d] = [g.quad0[d][0] - mx, g.quad0[d][1] - my];
            }
            s.pivot = carry_pivot(g.quad0, s.quad, g.pivot0);
        }
        Hit::Corner(_) | Hit::Edge(_) => {
            // Work in the box's own (unit) frame so rotated/skewed boxes scale along their axes.
            let Some(h) = Homography::rect_to_quad([0.0, 0.0, 1.0, 1.0], g.quad0) else { return };
            let Some(inv) = h.inverse() else { return };
            let (u, v) = inv.apply(p[0], p[1]);
            let (pu, pv) = inv.apply(g.pivot0[0], g.pivot0[1]);
            // Which sides of the unit box move: (u = 0, u = 1) and (v = 0, v = 1).
            let (mu, mv) = match g.hit {
                Hit::Corner(0) => ((true, false), (true, false)),
                Hit::Corner(1) => ((false, true), (true, false)),
                Hit::Corner(2) => ((false, true), (false, true)),
                Hit::Corner(3) => ((true, false), (false, true)),
                Hit::Edge(0) => ((false, false), (true, false)),
                Hit::Edge(1) => ((false, true), (false, false)),
                Hit::Edge(2) => ((false, false), (false, true)),
                _ => ((true, false), (false, false)),
            };
            // Each axis scales by a factor `k` about an anchor: the opposite side, or with ⌥ the
            // reference point, wherever it was set (#1329). The dragged handle follows the cursor.
            let axis = |moving: (bool, bool), at: f64, pivot: f64| -> (f64, f64) {
                if !moving.0 && !moving.1 {
                    return (0.5, 1.0);
                }
                let handle = if moving.0 { 0.0 } else { 1.0 };
                let anchor = if mods.alt { pivot } else { 1.0 - handle };
                let d = handle - anchor;
                // A reference point on the dragged side leaves nothing to scale against.
                let k = if d.abs() > 1e-9 { (at - anchor) / d } else { 1.0 };
                (anchor, if k.is_finite() { k } else { 1.0 })
            };
            let (mut au, mut ku) = axis(mu, u, pu);
            let (mut av, mut kv) = axis(mv, v, pv);
            // Corners and edges scale proportionally by default; ⇧ frees them (the legacy
            // preference swaps the two for corners and always stretches edges, `legacy_mods`).
            let corner = matches!(g.hit, Hit::Corner(_));
            if !corner && !mods.shift {
                // An edge sets the scale of its own axis, and the other axis follows by the same
                // factor about the middle of the box (⌥: about the reference point, as for
                // corners), so the opposite edge's handle stays put. Past the opposite edge only
                // the dragged axis flips.
                if mu.0 || mu.1 {
                    (av, kv) = (if mods.alt { pv } else { 0.5 }, ku.abs());
                } else {
                    (au, ku) = (if mods.alt { pu } else { 0.5 }, kv.abs());
                }
            }
            if corner && !mods.shift {
                let k = ku.abs().max(kv.abs());
                (ku, kv) = (k * ku.signum(), k * kv.signum());
            }
            let r = [au - au * ku, av - av * kv, au + (1.0 - au) * ku, av + (1.0 - av) * kv];
            let map = |a: f64, b: f64| {
                let (x, y) = h.apply(a, b);
                [x, y]
            };
            s.quad = [map(r[0], r[1]), map(r[2], r[1]), map(r[2], r[3]), map(r[0], r[3])];
            // ⌥ scales about the reference point, so it stays put; otherwise it rides along with
            // the box, keeping its place in it (#1329).
            s.pivot = if mods.alt { g.pivot0 } else { carry_pivot(g.quad0, s.quad, g.pivot0) };
        }
    }
}

fn anchor_uv(anchor: usize) -> (f64, f64) {
    match anchor {
        0 => (0.0, 0.0),
        1 => (0.5, 0.0),
        2 => (1.0, 0.0),
        3 => (0.0, 0.5),
        4 => (0.5, 0.5),
        5 => (1.0, 0.5),
        6 => (0.0, 1.0),
        7 => (0.5, 1.0),
        8 => (1.0, 1.0),
        _ => (0.5, 0.5),
    }
}

fn point_on_quad(quad: [[f64; 2]; 4], anchor: usize) -> [f64; 2] {
    let (u, v) = anchor_uv(anchor);
    if let Some(h) = Homography::rect_to_quad([0.0, 0.0, 1.0, 1.0], quad) {
        let (x, y) = h.apply(u, v);
        if x.is_finite() && y.is_finite() {
            return [x, y];
        }
    }
    match anchor {
        0 => quad[0],
        1 => [(quad[0][0] + quad[1][0]) / 2.0, (quad[0][1] + quad[1][1]) / 2.0],
        2 => quad[1],
        3 => [(quad[0][0] + quad[3][0]) / 2.0, (quad[0][1] + quad[3][1]) / 2.0],
        4 => [(quad[0][0] + quad[1][0] + quad[2][0] + quad[3][0]) / 4.0, (quad[0][1] + quad[1][1] + quad[2][1] + quad[3][1]) / 4.0],
        5 => [(quad[1][0] + quad[2][0]) / 2.0, (quad[1][1] + quad[2][1]) / 2.0],
        6 => quad[3],
        7 => [(quad[2][0] + quad[3][0]) / 2.0, (quad[2][1] + quad[3][1]) / 2.0],
        8 => quad[2],
        _ => [(quad[0][0] + quad[1][0] + quad[2][0] + quad[3][0]) / 4.0, (quad[0][1] + quad[1][1] + quad[2][1] + quad[3][1]) / 4.0],
    }
}

/// Where the reference point lands when the box goes from `quad0` to `quad`: the same place
/// within the box, as in Photoshop, where it is attached to the bounding box (#1329). A box
/// that has collapsed leaves it where it was.
fn carry_pivot(quad0: [[f64; 2]; 4], quad: [[f64; 2]; 4], pivot0: [f64; 2]) -> [f64; 2] {
    let unit = [0.0, 0.0, 1.0, 1.0];
    let (Some(h0), Some(h1)) = (Homography::rect_to_quad(unit, quad0), Homography::rect_to_quad(unit, quad)) else { return pivot0 };
    let Some(inv) = h0.inverse() else { return pivot0 };
    let (u, v) = inv.apply(pivot0[0], pivot0[1]);
    let (x, y) = h1.apply(u, v);
    if x.is_finite() && y.is_finite() { [x, y] } else { pivot0 }
}

/// An anchor or a handle on a section line. The inner control points of a patch are not shown.
fn on_section_edge(i: usize, j: usize) -> bool {
    i.is_multiple_of(3) || j.is_multiple_of(3)
}

/// Index of the warp control point within `tol` of `p` (nearest anchor or handle).
fn warp_hit(w: &Warp, p: [f64; 2], tol: f64) -> Option<usize> {
    let m = w.mesh.as_ref()?;
    let nx = m.nx();
    m.points
        .iter()
        .enumerate()
        .filter(|(i, _)| on_section_edge(i % nx, i / nx))
        .map(|(i, q)| (i, ((q[0] - p[0]).powi(2) + (q[1] - p[1]).powi(2)).sqrt()))
        .filter(|(_, d)| *d < tol)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// A 1×1 mesh (Grid › Default) draws rule-of-thirds guides. Denser presets are real patch
/// boundaries only — a 3×3 is three cells, not a third-line lattice inside every cell.
fn single_patch(mesh: &BezierMesh) -> bool {
    mesh.us.len() == 2 && mesh.vs.len() == 2
}

/// The surface parameters under `p` when it is inside the warped mesh, else None.
fn surface_at(mesh: &BezierMesh, p: [f64; 2]) -> Option<(f64, f64)> {
    let (s, t) = mesh.param_at(p);
    let q = mesh.eval(s, t);
    ((q[0] - p[0]).hypot(q[1] - p[1]) < 1.0).then_some((s, t))
}

/// Ctrl-click (or Option-click) picks the split: crosswise in the open, and the perpendicular
/// split when the pointer's grid position `(s, t)` is close to an existing grid line.
fn quick_split_kind(mesh: &BezierMesh, bounds: [f64; 4], (s, t): (f64, f64)) -> SplitTool {
    let w = (bounds[2] - bounds[0]).abs().max(1.0);
    let h = (bounds[3] - bounds[1]).abs().max(1.0);
    let su = (8.0 / w).clamp(0.02, 0.12);
    let sv = (8.0 / h).clamp(0.02, 0.12);
    let thirds = single_patch(mesh);
    let near = |knots: &[f64], v: f64, tol: f64| {
        knots.iter().any(|k| *k > 0.0 && *k < 1.0 && (k - v).abs() < tol) || (thirds && [1.0 / 3.0, 2.0 / 3.0].iter().any(|k| (k - v).abs() < tol))
    };
    match (near(&mesh.us, s, su), near(&mesh.vs, t, sv)) {
        (true, false) => SplitTool::Horizontal,
        (false, true) => SplitTool::Vertical,
        _ => SplitTool::Cross,
    }
}

/// The cursor for a split armed at grid position `(s, t)` of warp grid `m`: the arrow across the
/// split line as it looks on screen. A vertical split line runs along the grid's columns and is
/// placed by moving across them, in the grid's own direction there, so the arrow turns with a
/// turned or bent grid and with the view.
fn split_cursor(m: &BezierMesh, tool: SplitTool, (s, t): (f64, f64), xf: &ViewXform) -> CursorIcon {
    let e = 1e-3;
    let (a, b) = match tool {
        SplitTool::Vertical => (m.eval((s - e).max(0.0), t), m.eval((s + e).min(1.0), t)),
        SplitTool::Horizontal => (m.eval(s, (t - e).max(0.0)), m.eval(s, (t + e).min(1.0))),
        SplitTool::Cross => return CursorIcon::Crosshair,
    };
    crate::tool_cursor::resize([b[0] - a[0], b[1] - a[1]], xf)
}

/// Armed split tool, or Option held: the guide tracks the pointer and the split lands on release.
/// Returns false when this event is an ordinary control-point drag.
fn split_gesture(app: &mut PhotocraftApp, ev: ToolEvent, mods: egui::Modifiers) -> bool {
    let armed = app.transform_preview.as_ref().and_then(|p| p.split_tool);
    let placing = app.transform_preview.as_ref().is_some_and(|p| p.split_placing);
    match ev {
        ToolEvent::Down { x, y, .. } if armed.is_some() || mods.alt || mods.command => {
            if let Some(pv) = app.transform_preview.as_mut() {
                pv.split_placing = true;
                pv.split_pointer = Some([x, y]);
                pv.split_quick = armed.is_none();
            }
            true
        }
        ToolEvent::Move { x, y, .. } if placing => {
            if let Some(pv) = app.transform_preview.as_mut() {
                pv.split_pointer = Some([x, y]);
            }
            true
        }
        ToolEvent::Up { x, y } if placing => {
            let quick = app.transform_preview.as_ref().is_some_and(|p| p.split_quick);
            if let Some(pv) = app.transform_preview.as_mut() {
                pv.split_placing = false;
                pv.split_pointer = None;
                pv.split_quick = false;
            }
            let kind = if let Some(tool) = armed {
                Some(tool)
            } else if quick {
                app.ui.transform.as_ref().and_then(|t| t.warp.as_ref()).map(|w| {
                    let m = w.to_mesh(1, 1);
                    quick_split_kind(&m, w.bounds, m.param_at([x, y]))
                })
            } else {
                None
            };
            if let Some(kind) = kind {
                // On an existing line, or a click the drag path also delivers, this is a no-op.
                let _ = split(app, kind.command(), Some([x, y]));
            }
            true
        }
        _ => false,
    }
}

/// Drags the warp grid. A press on an anchor or handle moves that point (an anchor carries its
/// handles along); a press anywhere else inside the grid pulls the surface under the pointer.
/// Preset warps turn into a custom mesh when the press grabs something. An armed split tool, or
/// Ctrl / Option held, places a split where the pointer is released instead.
fn warp_pointer(app: &mut PhotocraftApp, ev: ToolEvent, tol: f64, mods: egui::Modifiers) {
    // Type takes only the warp styles: nothing on the canvas to drag or split (#3004).
    if warps_type(app) {
        return;
    }
    if split_gesture(app, ev, mods) {
        return;
    }
    let (Some(t), Some(pv)) = (app.ui.transform.as_mut(), app.transform_preview.as_mut()) else { return };
    let Some(w) = t.warp.as_mut() else { return };
    match ev {
        ToolEvent::Down { x, y, .. } => {
            // A preset warp becomes a custom mesh only when the press grabs one of its points.
            let custom = if w.mesh.is_none() || w.style != WarpStyle::Custom { Warp::custom(w.to_mesh(1, 1), w.bounds) } else { w.clone() };
            let pts0 = custom.mesh.as_ref().map(|m| m.points.clone()).unwrap_or_default();
            pv.warp_drag = warp_hit(&custom, [x, y], tol).map(|index| WarpDrag::Point { index, start: [x, y], pts0: pts0.clone() }).or_else(|| {
                let (s, t) = surface_at(custom.mesh.as_ref()?, [x, y])?;
                Some(WarpDrag::Surface { s, t, start: [x, y], pts0 })
            });
            if pv.warp_drag.is_some() {
                *w = custom;
            }
        }
        ToolEvent::Move { x, y, .. } | ToolEvent::Up { x, y } => {
            match (&pv.warp_drag, w.mesh.as_mut()) {
                (Some(WarpDrag::Point { index, start, pts0 }), Some(m)) => m.drag_point(pts0, *index, [x - start[0], y - start[1]]),
                (Some(WarpDrag::Surface { s, t, start, pts0 }), Some(m)) => m.pull(pts0, *s, *t, [x - start[0], y - start[1]]),
                _ => {}
            }
            if matches!(ev, ToolEvent::Up { .. }) {
                pv.warp_drag = None;
            }
        }
    }
}

/// Runs a warp-edit command against the mesh in the open Warp session and writes the mesh back.
pub fn edit_session_warp(app: &mut PhotocraftApp, id: &str, params: &serde_json::Value) -> Result<(), String> {
    let Some(w) = app.ui.transform.as_ref().and_then(|t| t.warp.clone()) else { return Err("not warping".into()) };
    if warps_type(app) {
        return Err("Warp on type takes only the warp styles".into());
    }
    let mut p = if params.is_object() { params.clone() } else { json!({}) };
    if p.get("warp").is_none() {
        p["warp"] = serde_json::to_value(&w).map_err(|e| e.to_string())?;
    }
    let r = app.session.execute(id, p).map_err(|e| e.to_string())?;
    let raw = r.get("warp").cloned().ok_or_else(|| "warp command returned no mesh".to_string())?;
    let nw: Warp = serde_json::from_value(raw).map_err(|e| e.to_string())?;
    // A warp-edit command can replace the mesh while pointer input is split across control
    // requests. Any drag index captured from the old mesh is invalid once that happens.
    if let Some(pv) = app.transform_preview.as_mut() {
        pv.warp_drag = None;
    }
    if let Some(t) = app.ui.transform.as_mut() {
        t.warp = Some(nw);
    }
    Ok(())
}

/// Applies a split-warp command to the mesh being edited (at the box centre unless `at`).
pub fn split(app: &mut PhotocraftApp, id: &str, at: Option<[f64; 2]>) -> Result<(), String> {
    let mut p = json!({});
    if let Some(a) = at {
        p["at"] = json!(a);
    }
    edit_session_warp(app, id, &p)
}

/// True just outside a corner: rotate handle feedback lives here, not over the entire
/// empty canvas. The actual rotation gesture continues to work farther away.
fn near_rotate_corner(t: &TransformSession, p: [f64; 2], tol: f64) -> bool {
    t.warp.is_none()
        && t.mode != TransformMode::Distort
        && {
            let h = hit(t, p, tol);
            h == Hit::Outside || (t.mode == TransformMode::Rotate && matches!(h, Hit::Corner(_) | Hit::Edge(_)))
        }
        && t.quad.iter().any(|q| (p[0] - q[0]).hypot(p[1] - q[1]) <= tol * 3.0)
}

/// Clockwise degrees in canvas coordinates (positive Y points down).
fn rotation_degrees(quad: [[f64; 2]; 4]) -> f64 {
    let dx = quad[1][0] - quad[0][0];
    let dy = quad[1][1] - quad[0][1];
    if dx.is_finite() && dy.is_finite() && dx.hypot(dy) > 1e-9 { dy.atan2(dx).to_degrees() } else { 0.0 }
}

/// Where the rotate cursor's arc bends at document point `p`, or `None` outside the rotate zone:
/// just outside a corner, or anywhere while a rotate drag is on. It points into the box along the
/// bisector of the nearest corner, so each corner gets its own arrow and they turn with the box.
pub(crate) fn rotate_cursor_dir(app: &PhotocraftApp, p: [f64; 2]) -> Option<[f64; 2]> {
    rotate_dir(app.ui.transform.as_ref()?, app.transform_preview.as_ref(), p, handle_tolerance(app))
}

fn rotate_dir(t: &TransformSession, pv: Option<&TransformPreview>, p: [f64; 2], tol: f64) -> Option<[f64; 2]> {
    // Content-Aware Scale's box only scales, and Skew doesn't rotate: no rotate arrow.
    if !(p[0].is_finite() && p[1].is_finite())
        || t.warp.is_some()
        || t.content_aware.is_some()
        || matches!(t.mode, TransformMode::Distort | TransformMode::Skew)
    {
        return None;
    }
    let rotating = pv.is_some_and(|pv| pv.gesture.is_some_and(|g| g.hit == Hit::Outside));
    if !rotating && !near_rotate_corner(t, p, tol) {
        return None;
    }
    let dist = |q: [f64; 2]| (p[0] - q[0]).hypot(p[1] - q[1]);
    let i = (0..4).min_by(|&a, &b| dist(t.quad[a]).total_cmp(&dist(t.quad[b])))?;
    let unit = |to: [f64; 2]| {
        let d = [to[0] - t.quad[i][0], to[1] - t.quad[i][1]];
        let l = d[0].hypot(d[1]);
        (l.is_finite() && l > 1e-9).then(|| [d[0] / l, d[1] / l])
    };
    let (a, b) = (unit(t.quad[(i + 1) % 4])?, unit(t.quad[(i + 3) % 4])?);
    let s = [a[0] + b[0], a[1] + b[1]];
    let l = s[0].hypot(s[1]);
    (l.is_finite() && l > 1e-9).then(|| [s[0] / l, s[1] / l])
}

/// Cursor for hovering a document point while transforming. `xf` is the canvas's view;
/// `quick_split` is Ctrl or Option, which arms a quick split while a warp is active.
pub fn cursor(app: &PhotocraftApp, xf: &ViewXform, p: [f64; 2], quick_split: bool) -> Option<CursorIcon> {
    let t = app.ui.transform.as_ref()?;
    let tol = handle_tolerance(app);
    if let Some(w) = &t.warp {
        if warps_type(app) {
            return Some(CursorIcon::Default);
        }
        let armed = app.transform_preview.as_ref().and_then(|pv| pv.split_tool);
        if armed.is_some() || quick_split {
            // One search for the grid position under the pointer, for both the kind and the arrow.
            let m = w.to_mesh(1, 1);
            let at = m.param_at(p);
            let tool = armed.unwrap_or_else(|| quick_split_kind(&m, w.bounds, at));
            return Some(split_cursor(&m, tool, at, xf));
        }
        if w.style != WarpStyle::Custom || warp_hit(w, p, tol).is_some() {
            return Some(CursorIcon::Crosshair);
        }
        return Some(if w.mesh.as_ref().and_then(|m| surface_at(m, p)).is_some() { CursorIcon::Grab } else { CursorIcon::Default });
    }
    let h = hit(t, p, tol);
    if !mode_allows(t.mode, h) || (t.content_aware.is_some() && h == Hit::Outside) {
        return Some(CursorIcon::Default);
    }
    if rotate_cursor_dir(app, p).is_some() {
        // The overlay draws the curved rotate arrow (`rotate_cursor_dir`), also during the drag.
        return Some(CursorIcon::None);
    }
    // The box's corners run clockwise from the top-left; edge k joins corners k and k + 1.
    let corner = |k: usize| t.quad[k % 4];
    let edge_mid = |k: usize| {
        let (a, b) = (corner(k), corner(k + 1));
        [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0]
    };
    // A handle resizes the box away from the opposite handle.
    let resize = |from: [f64; 2], to: [f64; 2]| crate::tool_cursor::resize([to[0] - from[0], to[1] - from[1]], xf);
    Some(match h {
        Hit::Corner(i) => crate::tool_cursor::corner_resize(&t.quad, i, xf),
        Hit::Edge(i) => resize(edge_mid(i + 2), edge_mid(i)),
        Hit::Pivot => CursorIcon::Crosshair,
        Hit::Inside => CursorIcon::Move,
        Hit::Outside => CursorIcon::Alias,
    })
}

/// For each vertex of the preview's `n` × `n` grid over the unit square (row by row), whether it
/// maps to the front side of `h`'s horizon. A Distort dragged past the horizon sends the rest of
/// the image to the far side, where its cells would wrap round infinity; the preview leaves them
/// out, as the committed warp does (#4863).
fn front_vertices(h: &Homography, n: usize) -> Vec<bool> {
    let front = h.front_side([0.0, 0.0, 1.0, 1.0]);
    (0..=n).flat_map(|j| (0..=n).map(move |i| (i as f64 / n as f64, j as f64 / n as f64))).map(|(u, v)| front * h.w(u, v) > 1e-9).collect()
}

/// The moving pixels as the box shows them (warped, skewed or styled), plus the layers above
/// them. This is image content: the canvas draws it right after the document, under the grid
/// and guides, so a guide stays visible across a layer being transformed (#5086), as in
/// Photoshop. The box, handles and reference point are [`draw_overlay`], drawn over the guides.
pub fn draw_content(app: &PhotocraftApp, painter: &egui::Painter, xf: &ViewXform) {
    let (Some(t), Some(pv)) = (&app.ui.transform, &app.transform_preview) else { return };
    let output = crate::monitor_status::view_display(app, painter.ctx());
    let (display, _) = crate::canvas::canvas_display(app, &pv.doc, output);
    // Selection tint and lone mask/channel values are UI diagnostics, not document colors.
    let display = display.as_deref().filter(|_| !t.selection && t.target.is_none() && t.path.is_none());
    if let Some(w) = &t.warp {
        draw_warp_content(painter, xf, t, w, pv, display);
        return;
    }
    let ctx = painter.ctx();
    let styled =
        styled_frame(app, xf.zoom * ctx.pixels_per_point(), ctx.input(|i| i.pointer.any_down())).and_then(|(s, frame)| s.texture(ctx, &frame, display));
    if let Some((tex, r)) = styled {
        let mut mesh = egui::Mesh::with_texture(tex);
        for (x, y, u, v) in [(r[0], r[1], 0.0, 0.0), (r[2], r[1], 1.0, 0.0), (r[2], r[3], 1.0, 1.0), (r[0], r[3], 0.0, 1.0)] {
            mesh.vertices.push(egui::epaint::Vertex { pos: pos2(x, y), uv: pos2(u, v), color: Color32::WHITE });
        }
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        painter.add(clip_to_canvas(mesh, canvas_size(pv), |p| xf.to_screen(p.x, p.y)));
    } else if let Some(h) = Homography::rect_to_quad([0.0, 0.0, 1.0, 1.0], t.quad) {
        // A 24×24 grid keeps perspective previews straight.
        let n = 24;
        let zoom = xf.zoom * painter.ctx().pixels_per_point();
        // Content-Aware Scale shows its latest live result once there is one.
        let (tex, uv) = match pv.content_aware.as_ref().and_then(|l| l.shown(t)) {
            Some((texture, texel)) => texture.pick_display(painter.ctx(), zoom, texel, false, display),
            None => pv.texture.pick_display(painter.ctx(), zoom, quad_scale(t), t.interpolation == "nearest", display),
        };
        let mut mesh = egui::Mesh::with_texture(tex);
        let tint = Color32::from_white_alpha((pv.opacity.clamp(0.0, 1.0) * 255.0) as u8);
        let ahead = front_vertices(&h, n);
        for j in 0..=n {
            for i in 0..=n {
                let (u, v) = (i as f64 / n as f64, j as f64 / n as f64);
                let (x, y) = h.apply(u, v);
                mesh.vertices.push(egui::epaint::Vertex { pos: pos2(x as f32, y as f32), uv: pos2(u as f32 * uv[0], v as f32 * uv[1]), color: tint });
            }
        }
        for j in 0..n {
            for i in 0..n {
                let a = (j * (n + 1) + i) as u32;
                for tri in [[a, a + 1, a + n as u32 + 2], [a, a + n as u32 + 2, a + n as u32 + 1]] {
                    // A cell with a corner past the horizon would stretch across the canvas (#4863).
                    if tri.iter().all(|&k| ahead[k as usize]) {
                        mesh.add_triangle(tri[0], tri[1], tri[2]);
                    }
                }
            }
        }
        painter.add(clip_to_canvas(mesh, canvas_size(pv), |p| xf.to_screen(p.x, p.y)));
    }
    if let Some(a) = &pv.above {
        a.draw(painter, xf, display);
    }
}

/// The box, handles, reference point and path outline over the preview that [`draw_content`]
/// drew; the warp grid and its split guides for a warp.
pub fn draw_overlay(app: &PhotocraftApp, painter: &egui::Painter, xf: &ViewXform) {
    let (Some(t), Some(pv)) = (&app.ui.transform, &app.transform_preview) else { return };
    let scr = |q: [f64; 2]| xf.to_screen(q[0] as f32, q[1] as f32);
    if let Some(w) = &t.warp {
        let over_canvas = painter.ctx().input(|i| i.pointer.hover_pos()).filter(|p| xf.rect.contains(*p)).map(|p| xf.to_doc(p));
        let at = pv.split_pointer.or(over_canvas);
        let alt = painter.ctx().input(|i| i.modifiers.alt || i.modifiers.command);
        let guide = at.filter(|_| !warps_type(app)).and_then(|p| {
            let tool = pv.split_tool.or_else(|| {
                (alt || pv.split_quick).then(|| {
                    let m = w.to_mesh(1, 1);
                    quick_split_kind(&m, w.bounds, m.param_at(p))
                })
            });
            tool.map(|tool| (tool, p))
        });
        draw_warp_overlay(painter, xf, w, pv, guide);
        return;
    }
    if pv.committing.is_some() {
        return;
    }
    let accent = crate::theme::Tokens::get(painter.ctx()).accent;
    if let (Some(path), Some(h)) = (&pv.path, Homography::rect_to_quad(t.rect, t.quad)) {
        crate::vector_ui::draw_outline(
            painter,
            path,
            &|q| {
                let (x, y) = h.apply(q[0], q[1]);
                xf.to_screen(x as f32, y as f32)
            },
            accent,
        );
    }
    let pts: Vec<Pos2> = t.quad.iter().map(|q| scr(*q)).collect();
    painter.add(egui::Shape::closed_line(pts.clone(), Stroke::new(1.0, accent)));
    let mids: Vec<Pos2> = (0..4).map(|i| pts[i].lerp(pts[(i + 1) % 4], 0.5)).collect();
    for p in pts.iter().chain(mids.iter()) {
        let r = egui::Rect::from_center_size(*p, vec2(7.0, 7.0));
        painter.rect_filled(r, 0.0, Color32::WHITE);
        painter.rect_stroke(r, 0.0, Stroke::new(1.0, accent), egui::StrokeKind::Inside);
    }
    // Reference point (Photoshop's crosshair circle).
    let c = scr(t.pivot);
    painter.circle_stroke(c, 5.0, Stroke::new(1.0, Color32::WHITE));
    painter.circle_stroke(c, 5.0, Stroke::new(0.5, Color32::BLACK));
    painter.line_segment([c - vec2(8.0, 0.0), c + vec2(8.0, 0.0)], Stroke::new(1.0, Color32::WHITE));
    painter.line_segment([c - vec2(0.0, 8.0), c + vec2(0.0, 8.0)], Stroke::new(1.0, Color32::WHITE));
    if let Some(at) = painter.ctx().pointer_hover_pos().filter(|p| xf.rect.contains(*p)) {
        let p = xf.to_doc(at);
        let tol = HANDLE_PX / f64::from(xf.zoom.max(0.01));
        if let Some(k) = rotate_dir(t, Some(pv), p, tol) {
            // Through the view, so the arc bends round the corner as it shows under a turned or
            // flipped view.
            let toward = xf.to_screen((p[0] + k[0] * 16.0) as f32, (p[1] + k[1] * 16.0) as f32) - at;
            crate::crop_ui::draw_turn_cursor(painter.ctx(), at, toward);
        }
        // Preferences › Tools › Show Transformation Values off hides it, as in Photoshop (#204).
        if let Some(r) = live_readout(app).filter(|_| app.session.prefs().tools.show_transformation_values) {
            draw_live_readout(painter.ctx(), at, r);
        }
    }
}

/// Applying the open box moves the selection outline with it: `edit.transform` and
/// `edit.transform.warp` map it like the pixels. Not Transform Selection (its ants are hidden), a
/// path, Content-Aware Scale or a type layer's Warp Text, which leave the selection alone.
fn carries_selection(app: &PhotocraftApp, t: &TransformSession) -> bool {
    !t.selection && t.path.is_none() && t.content_aware.is_none() && !(t.warp.is_some() && warps_type(app))
}

/// A transform of `document` is open that carries its selection along (#5297): the canvas traces
/// the whole outline, not only the part in view where it started, for [`draw_selection`].
pub(crate) fn carries_selection_of(app: &PhotocraftApp, document: DocId) -> bool {
    matches!((&app.ui.transform, &app.transform_preview), (Some(t), Some(pv)) if pv.doc.id == document && carries_selection(app, t))
}

/// The marching ants where the open box puts them (#5297): the selection follows a move, scale,
/// rotation, skew, distort, perspective or warp while it is made, to where applying it leaves it.
/// `segs` is the outline of `document`'s selection at rest. False when no open transform carries
/// that selection: the caller draws it where it is.
pub(crate) fn draw_selection(
    app: &PhotocraftApp,
    painter: &egui::Painter,
    xf: &ViewXform,
    document: DocId,
    segs: &[crate::outline::Segment],
    time: f64,
) -> bool {
    let Some(t) = app.ui.transform.as_ref().filter(|_| carries_selection_of(app, document)) else { return false };
    let scr = |(x, y): (f64, f64)| xf.to_screen(x as f32, y as f32);
    let doc = |p: [i32; 2]| [f64::from(p[0]), f64::from(p[1])];
    let along = |a: [f64; 2], b: [f64; 2], k: f64| [a[0] + (b[0] - a[0]) * k, a[1] + (b[1] - a[1]) * k];
    if let Some(w) = &t.warp {
        // A warp bends the outline's straight runs: each is drawn in pieces about 8 points long.
        let piece = (8.0 / f64::from(xf.zoom.max(1e-3))).max(1.0);
        let lines = segs.iter().flat_map(|(a, b)| {
            let (a, b) = (doc(*a), doc(*b));
            let n = (((b[0] - a[0]).abs() + (b[1] - a[1]).abs()) / piece).ceil().clamp(1.0, 256.0) as usize;
            let at = move |i: usize| {
                let p = along(a, b, i as f64 / n as f64);
                scr(w.map(p[0], p[1]))
            };
            (0..n).map(move |i| (at(i), at(i + 1)))
        });
        crate::canvas::marching_ants_lines(painter, lines, time);
    } else if let Some(h) = Homography::rect_to_quad(t.rect, t.quad) {
        // Past a Distort's horizon the outline has no place, like the pixels (#4863): a run that
        // crosses it stops short of it, where its points are still a finite way off.
        let front = h.front_side(t.rect);
        let depth = |p: [f64; 2]| front * h.w(p[0], p[1]);
        let near = corners(t.rect).iter().map(|c| depth(*c)).fold(0.0, f64::max) * 1e-3;
        let lines = segs.iter().filter_map(|(a, b)| {
            let (mut a, mut b) = (doc(*a), doc(*b));
            let (da, db) = (depth(a), depth(b));
            if da < near && db < near {
                return None;
            }
            if da < near {
                a = along(a, b, (near - da) / (db - da));
            } else if db < near {
                b = along(b, a, (near - db) / (da - db));
            }
            Some((scr(h.apply(a[0], a[1])), scr(h.apply(b[0], b[1]))))
        });
        crate::canvas::marching_ants_lines(painter, lines, time);
    }
    true
}

/// What the styled preview of the moving layer is drawn from (transform_fx.rs), at `zoom` screen
/// pixels per document pixel. `None` for a layer without a style (also once it is switched off
/// while the box is open) or a hidden one: the bare pixels are drawn instead. The style, opacity
/// and fill are the layer's as they are now, so edits made while the box is open show.
pub(crate) fn styled_frame(app: &PhotocraftApp, zoom: f32, dragging: bool) -> Option<(&crate::transform_fx::StyledPreview, crate::transform_fx::Frame<'_>)> {
    let (t, pv) = (app.ui.transform.as_ref()?, app.transform_preview.as_ref()?);
    let styled = pv.styled.as_ref().filter(|_| t.warp.is_none())?;
    let st = app.session.active().filter(|st| st.doc.id == pv.doc.id)?;
    let layer = st.doc.layer(LayerId(t.layer)).filter(|l| l.visible && photocraft_compose::effects::has_effects(l))?;
    let frame = crate::transform_fx::Frame {
        doc: &st.doc,
        layer,
        revision: st.revision,
        texels: &pv.texture,
        quad: t.quad,
        quad_scale: quad_scale(t),
        nearest: t.interpolation == "nearest",
        zoom,
        dragging,
    };
    Some((styled, frame))
}

/// The value shown beside the pointer while the box is dragged, like Photoshop's Show
/// Transformation Values: the box's angle while rotating, its scale while resizing, the offset
/// while moving.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Readout {
    /// Clockwise degrees in document space (`rotation_degrees`), whatever the view's turn or flip.
    Angle(f64),
    /// Width and height in percent of the original box.
    Scale([f64; 2]),
    /// Document pixels moved since the press.
    Offset([f64; 2]),
}

/// The readout for the drag in progress, or `None` when nothing is dragged (or the drag is the
/// reference point, a warp, or a distortion that has no single scale).
pub(crate) fn live_readout(app: &PhotocraftApp) -> Option<Readout> {
    let t = app.ui.transform.as_ref().filter(|t| t.warp.is_none())?;
    let g = app.transform_preview.as_ref()?.gesture?;
    match g.hit {
        Hit::Outside => Some(Readout::Angle(rotation_degrees(t.quad))),
        Hit::Inside => Some(Readout::Offset([t.quad[0][0] - g.quad0[0][0], t.quad[0][1] - g.quad0[0][1]])),
        Hit::Corner(_) | Hit::Edge(_) if parallelogram(&t.quad) => {
            let (w, h, _, _) = readout(t);
            (w.is_finite() && h.is_finite()).then_some(Readout::Scale([w, h]))
        }
        Hit::Corner(_) | Hit::Edge(_) | Hit::Pivot => None,
    }
}

/// True when `q` is still an affine image of the box (opposite sides parallel), so a W/H scale
/// describes it; a distorted or perspective box has none.
fn parallelogram(q: &[[f64; 2]; 4]) -> bool {
    let scale = q.iter().flatten().fold(1.0f64, |m, v| m.max(v.abs()));
    (0..2).all(|k| ((q[0][k] + q[2][k]) - (q[1][k] + q[3][k])).abs() <= scale * 1e-9)
}

/// [`live_readout`] as the rounded label beside the pointer (the marquee's and Crop's readout).
fn draw_live_readout(ctx: &egui::Context, at: Pos2, r: Readout) {
    // Round first so a hair below zero never reads "-0".
    let fixed = |v: f64, places: i32| {
        let k = 10f64.powi(places);
        let v = (v * k).round() / k;
        if v == 0.0 { 0.0 } else { v }
    };
    let id = "transform-readout";
    match r {
        Readout::Angle(a) => crate::canvas::draw_readout(ctx, id, at, ["Angle:"], [format!("{:.1}°", fixed(a, 1))]),
        Readout::Scale([w, h]) => crate::canvas::draw_readout(ctx, id, at, ["W:", "H:"], [w, h].map(|v| format!("{:.1}%", fixed(v, 1)))),
        Readout::Offset([dx, dy]) => crate::canvas::draw_readout(ctx, id, at, ["ΔX:", "ΔY:"], [dx, dy].map(|v| format!("{:.0} px", fixed(v, 0)))),
    }
}

/// Width and height of the document being transformed, in document pixels.
fn canvas_size(pv: &TransformPreview) -> [f32; 2] {
    [pv.doc.size.width as f32, pv.doc.size.height as f32]
}

/// Clips `mesh` (positions in document pixels) to the canvas `[0, 0, size]` and maps it to the
/// screen. Pixels past the canvas edge are never shown, also not while transforming (Photoshop;
/// #2027): only the box and its handles reach over the pasteboard. Clipping in document space
/// stays exact under a rotated or flipped view, where a screen clip rectangle would not.
fn clip_to_canvas(mut mesh: egui::Mesh, size: [f32; 2], to_screen: impl Fn(Pos2) -> Pos2) -> egui::Mesh {
    let inside = |p: Pos2| p.x >= 0.0 && p.y >= 0.0 && p.x <= size[0] && p.y <= size[1];
    if mesh.vertices.iter().all(|v| inside(v.pos)) {
        for v in &mut mesh.vertices {
            v.pos = to_screen(v.pos);
        }
        return mesh;
    }
    let mut out = egui::Mesh::with_texture(mesh.texture_id);
    for tri in mesh.indices.as_chunks::<3>().0 {
        let mut poly: Vec<egui::epaint::Vertex> = tri.iter().filter_map(|i| mesh.vertices.get(*i as usize).copied()).collect();
        // Sutherland–Hodgman against the four canvas edges (`dist` >= 0 is the canvas side).
        for edge in 0..4 {
            let dist = |v: &egui::epaint::Vertex| match edge {
                0 => v.pos.x,
                1 => size[0] - v.pos.x,
                2 => v.pos.y,
                _ => size[1] - v.pos.y,
            };
            let mut next = Vec::with_capacity(poly.len() + 2);
            for (k, a) in poly.iter().enumerate() {
                let Some(b) = poly.get((k + 1) % poly.len()) else { continue };
                let (da, db) = (dist(a), dist(b));
                if da >= 0.0 {
                    next.push(*a);
                }
                if (da >= 0.0) != (db >= 0.0) && da != db {
                    let t = da / (da - db);
                    next.push(egui::epaint::Vertex { pos: a.pos.lerp(b.pos, t), uv: a.uv.lerp(b.uv, t), color: a.color });
                }
            }
            poly = next;
        }
        if poly.len() < 3 {
            continue;
        }
        let base = out.vertices.len() as u32;
        out.vertices.extend(poly.iter().map(|v| egui::epaint::Vertex { pos: to_screen(v.pos), ..*v }));
        for k in 1..poly.len() as u32 - 1 {
            out.add_triangle(base, base + k, base + k + 1);
        }
    }
    out
}

/// Warp preview (the moving pixels on a fine textured mesh) plus the control mesh. Patch
/// boundaries are solid. A single patch (Grid › Default) also draws its rule-of-thirds guides;
/// 3×3, 4×4 and 5×5 are just those even cells, with an anchor at every intersection. `guide` is
/// the split line following the pointer.
/// How near the pointer (screen px) a handle's anchor or the handle itself must be for the
/// handle to show. Everything else stays out of the way: only anchors are always drawn.
const HANDLE_REVEAL_PX: f32 = 56.0;

/// The anchor a handle belongs to, as `(i, j)` in the control grid (an anchor is its own).
fn handle_anchor(i: usize, j: usize) -> (usize, usize) {
    if i.is_multiple_of(3) {
        let aj = match j % 3 {
            0 => j,
            1 => j - 1,
            _ => j + 1,
        };
        (i, aj)
    } else {
        (if i % 3 == 1 { i - 1 } else { i + 1 }, j)
    }
}
/// Anchors always, small and quiet. A handle and its arm show only while the pointer is close to
/// them or they are being dragged, so a dense grid does not bury the picture in dots.
fn draw_warp_points(painter: &egui::Painter, m: &BezierMesh, scr: impl Fn([f64; 2]) -> Pos2, pv: &TransformPreview, line: Stroke) {
    let (nx, ny) = (m.nx(), m.ny());
    let hover = painter.ctx().input(|i| i.pointer.hover_pos());
    let dragged = match &pv.warp_drag {
        Some(WarpDrag::Point { index, .. }) => Some(handle_anchor(index % nx, index / nx)),
        _ => None,
    };
    let near = |p: Pos2| hover.is_some_and(|h| h.distance(p) <= HANDLE_REVEAL_PX);
    let faint = Stroke::new(0.75, line.color.gamma_multiply(0.8));
    for j in 0..ny {
        for i in 0..nx {
            if !on_section_edge(i, j) {
                continue;
            }
            let p = scr(m.point(i, j));
            if i.is_multiple_of(3) && j.is_multiple_of(3) {
                let r = egui::Rect::from_center_size(p, vec2(5.0, 5.0));
                painter.rect_filled(r, 0.0, Color32::WHITE);
                painter.rect_stroke(r, 0.0, faint, egui::StrokeKind::Inside);
                continue;
            }
            let (ai, aj) = handle_anchor(i, j);
            let a = scr(m.point(ai, aj));
            if !(near(p) || near(a) || dragged == Some((ai, aj))) {
                continue;
            }
            painter.line_segment([a, p], faint);
            painter.circle_filled(p, 2.75, Color32::WHITE);
            painter.circle_stroke(p, 2.75, faint);
        }
    }
}

/// The warped pixels and the layers above them ([`draw_content`] for a warp).
fn draw_warp_content(
    painter: &egui::Painter,
    xf: &ViewXform,
    t: &TransformSession,
    w: &Warp,
    pv: &TransformPreview,
    display: Option<&photocraft_engine::display_color::CanvasDisplay>,
) {
    let r = t.rect;
    let m = w.to_mesh(1, 1);
    let (us, vs) = crate::warp_preview::samples(&m, xf.zoom as f64);
    let (tex, uv) = pv.texture.pick_display(painter.ctx(), xf.zoom * painter.ctx().pixels_per_point(), 1.0, t.interpolation == "nearest", display);
    let mut mesh = egui::Mesh::with_texture(tex);
    let tint = Color32::from_white_alpha((pv.opacity.clamp(0.0, 1.0) * 255.0) as u8);
    for v in &vs {
        for u in &us {
            let (x, y) = w.map(r[0] + u * (r[2] - r[0]), r[1] + v * (r[3] - r[1]));
            mesh.vertices.push(egui::epaint::Vertex { pos: pos2(x as f32, y as f32), uv: pos2(*u as f32 * uv[0], *v as f32 * uv[1]), color: tint });
        }
    }
    let pos: Vec<[f32; 2]> = mesh.vertices.iter().map(|v| [v.pos.x, v.pos.y]).collect();
    let rest: Vec<[f32; 2]> = vs.iter().flat_map(|v| us.iter().map(move |u| [(r[0] + u * (r[2] - r[0])) as f32, (r[1] + v * (r[3] - r[1])) as f32])).collect();
    mesh.indices = crate::warp_preview::triangle_order(&pos, &rest, us.len());
    painter.add(clip_to_canvas(mesh, canvas_size(pv), |p| xf.to_screen(p.x, p.y)));
    if let Some(a) = &pv.above {
        a.draw(painter, xf, display);
    }
}

/// The warp grid, its control points and split guides ([`draw_overlay`] for a warp).
fn draw_warp_overlay(painter: &egui::Painter, xf: &ViewXform, w: &Warp, pv: &TransformPreview, guide: Option<(SplitTool, [f64; 2])>) {
    let scr = |q: [f64; 2]| xf.to_screen(q[0] as f32, q[1] as f32);
    let m = w.to_mesh(1, 1);
    let accent = crate::theme::Tokens::get(painter.ctx()).accent;
    // Transform Path › Warp: the path drawn through the warp.
    if let Some(path) = &pv.path {
        crate::vector_ui::draw_outline(
            painter,
            path,
            &|q| {
                let (x, y) = w.map(q[0], q[1]);
                xf.to_screen(x as f32, y as f32)
            },
            accent,
        );
    }
    let line = Stroke::new(1.0, accent);
    let thin = Stroke::new(0.75, accent.gamma_multiply(0.7));
    let curve = |fixed_u: Option<f64>, fixed_v: Option<f64>| -> Vec<Pos2> {
        (0..=40)
            .map(|k| {
                let s = k as f64 / 40.0;
                let p = m.eval(fixed_u.unwrap_or(s), fixed_v.unwrap_or(s));
                scr(p)
            })
            .collect()
    };
    // Patch boundaries are the grid. Only Default (one patch) draws the rule-of-thirds guides;
    // a 3×3 preset is three even cells, not those guides repeated inside every cell.
    let thirds = single_patch(&m);
    for (k, win) in m.us.windows(2).enumerate() {
        if thirds {
            for third in [1.0, 2.0] {
                painter.add(egui::Shape::line(curve(Some(win[0] + (win[1] - win[0]) * third / 3.0), None), thin));
            }
        }
        if k == 0 {
            painter.add(egui::Shape::line(curve(Some(win[0]), None), line));
        }
        painter.add(egui::Shape::line(curve(Some(win[1]), None), line));
    }
    for (k, win) in m.vs.windows(2).enumerate() {
        if thirds {
            for third in [1.0, 2.0] {
                painter.add(egui::Shape::line(curve(None, Some(win[0] + (win[1] - win[0]) * third / 3.0)), thin));
            }
        }
        if k == 0 {
            painter.add(egui::Shape::line(curve(None, Some(win[0])), line));
        }
        painter.add(egui::Shape::line(curve(None, Some(win[1])), line));
    }
    if w.style == WarpStyle::Custom {
        draw_warp_points(painter, &m, scr, pv, line);
    }
    if let Some((tool, p)) = guide {
        let (s, t) = m.param_at(p);
        let under = Stroke::new(3.0, Color32::BLACK);
        let over = Stroke::new(1.25, Color32::WHITE);
        let mut strokes = Vec::new();
        if matches!(tool, SplitTool::Vertical | SplitTool::Cross) {
            strokes.push(curve(Some(s), None));
        }
        if matches!(tool, SplitTool::Horizontal | SplitTool::Cross) {
            strokes.push(curve(None, Some(t)));
        }
        for pts in strokes {
            painter.add(egui::Shape::line(pts.clone(), under));
            painter.add(egui::Shape::line(pts, over));
        }
    }
}

/// How much the box enlarges its pixels: the longest edge relative to the original (picks the
/// preview's texture level and filter).
fn quad_scale(t: &TransformSession) -> f32 {
    let (w0, h0) = ((t.rect[2] - t.rect[0]).max(1e-9), (t.rect[3] - t.rect[1]).max(1e-9));
    let len = |a: [f64; 2], b: [f64; 2]| (b[0] - a[0]).hypot(b[1] - a[1]);
    let q = &t.quad;
    let s = (len(q[0], q[1]) / w0).max(len(q[3], q[2]) / w0).max(len(q[0], q[3]) / h0).max(len(q[1], q[2]) / h0);
    if s.is_finite() { s as f32 } else { 1.0 }
}

/// W and H (%), the angle (°) and the horizontal skew (°) the options bar shows for the box.
/// A mirrored box reads as a negative W or H, whichever keeps the angle nearer 0 (Photoshop:
/// Flip Horizontal is W -100 % at 0°, Flip Vertical H -100 % at 0°). Both are the same box:
/// negating W and H and turning by a half turn cancel out.
fn readout(t: &TransformSession) -> (f64, f64, f64, f64) {
    let p = Parts::of(t);
    let wrap = |a: f64| {
        let a = a.rem_euclid(std::f64::consts::TAU);
        if a > std::f64::consts::PI { a - std::f64::consts::TAU } else { a }
    };
    let turned = wrap(p.angle + std::f64::consts::PI);
    let (sx, sy, angle) = if p.sy < 0.0 && turned.abs() < wrap(p.angle).abs() { (-p.sx, -p.sy, turned) } else { (p.sx, p.sy, p.angle) };
    (sx * 100.0, sy * 100.0, angle.to_degrees(), p.skew_h.to_degrees())
}

/// `v`, kept off zero with its sign: a divisor for a W or H scale that may be negative.
fn nonzero(v: f64) -> f64 {
    if v.abs() < 1e-9 { 1e-9f64.copysign(v) } else { v }
}

/// The box's affine part read as Photoshop's options bar does: the frame scaled (W, H), skewed
/// (H, V) and then rotated about the reference point. Angles are radians; positive turns
/// clockwise on screen, so a positive H skew leans the box's sides like the angle field would
/// turn them, and a positive V skew its top and bottom.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Parts {
    sx: f64,
    /// Negative when the box is flipped vertically relative to its rotation.
    sy: f64,
    angle: f64,
    skew_h: f64,
    skew_v: f64,
}

/// The skew fields' reach: a skew of ±90° would flatten the box.
const SKEW_LIMIT_DEG: f32 = 89.0;

impl Parts {
    /// The box's columns: where the frame's unit X and Y edges went (from the top-left corner).
    fn columns(t: &TransformSession) -> Option<[[f64; 2]; 2]> {
        let (w0, h0) = (t.rect[2] - t.rect[0], t.rect[3] - t.rect[1]);
        if w0.abs() < 1e-9 || h0.abs() < 1e-9 {
            return None;
        }
        let q = &t.quad;
        let m = [[(q[1][0] - q[0][0]) / w0, (q[1][1] - q[0][1]) / w0], [(q[3][0] - q[0][0]) / h0, (q[3][1] - q[0][1]) / h0]];
        m.iter().flatten().all(|v| v.is_finite()).then_some(m)
    }

    /// The box as scale, skew and rotation, keeping the session's typed V skew (any box can be
    /// written with any V skew short of ±90°).
    fn of(t: &TransformSession) -> Parts {
        let skew_v = t.skew_v.to_radians();
        let none = Parts { sx: 1.0, sy: 1.0, angle: 0.0, skew_h: 0.0, skew_v };
        Parts::columns(t).map_or(none, |m| Parts::from_columns(m, skew_v))
    }

    fn from_columns([c0, c1]: [[f64; 2]; 2], skew_v: f64) -> Parts {
        // X edge = sx · R(angle) · (1, tan v): its direction is the angle plus v.
        let angle = c0[1].atan2(c0[0]) - skew_v;
        let sx = c0[0].hypot(c0[1]) * skew_v.cos();
        // Y edge = sy · R(angle) · (−tan h, 1).
        let (gx, gy) = unturn(c1, angle);
        let skew_h = if gy.abs() < 1e-12 { 0.0 } else { (-gx / gy).atan() };
        Parts { sx, sy: gy, angle, skew_h, skew_v }
    }

    fn columns_of(self) -> [[f64; 2]; 2] {
        let (sn, cs) = self.angle.sin_cos();
        let rot = |x: f64, y: f64| [x * cs - y * sn, x * sn + y * cs];
        let c0 = rot(self.sx, self.sx * self.skew_v.tan());
        let c1 = rot(-self.sy * self.skew_h.tan(), self.sy);
        [c0, c1]
    }
}

/// `c` turned back by `angle` (radians).
fn unturn(c: [f64; 2], angle: f64) -> (f64, f64) {
    let (sn, cs) = angle.sin_cos();
    (c[0] * cs + c[1] * sn, -c[0] * sn + c[1] * cs)
}

/// Sets the box's H and V skew (degrees), keeping its scale, angle and reference point: the
/// options bar's skew fields.
fn skew_about_pivot(app: &mut PhotocraftApp, skew_h: f64, skew_v: f64) {
    let Some(s) = app.ui.transform.as_mut() else { return };
    let Some(m) = Parts::columns(s) else { return };
    let limit = f64::from(SKEW_LIMIT_DEG);
    let (skew_h, skew_v) = (skew_h.clamp(-limit, limit), skew_v.clamp(-limit, limit));
    let from = Parts::from_columns(m, s.skew_v.to_radians());
    let to = Parts { skew_h: skew_h.to_radians(), skew_v: skew_v.to_radians(), ..from }.columns_of();
    // The map from the old box to the new one: to · m⁻¹.
    let det = m[0][0] * m[1][1] - m[1][0] * m[0][1];
    if det.abs() < 1e-12 {
        return;
    }
    let inv = [[m[1][1] / det, -m[0][1] / det], [-m[1][0] / det, m[0][0] / det]];
    let l = |x: f64, y: f64| {
        let (u, v) = (inv[0][0] * x + inv[1][0] * y, inv[0][1] * x + inv[1][1] * y);
        [to[0][0] * u + to[1][0] * v, to[0][1] * u + to[1][1] * v]
    };
    let c = s.pivot;
    let quad = s.quad.map(|q| {
        let d = l(q[0] - c[0], q[1] - c[1]);
        [c[0] + d[0], c[1] + d[1]]
    });
    if quad.iter().flatten().all(|v| v.is_finite()) {
        s.quad = quad;
        s.skew_v = skew_v;
    }
}

/// Width of the mode, cancel and commit cluster kept on the right of the options bar.
const ACTIONS_W: f32 = 140.0;
/// Width of the action buttons when they follow the Warp fields (one more than the box bar: Reset).
const WARP_ACTIONS_W: f32 = 176.0;

/// Reach of the reference point X/Y fields: the largest document side (`image.canvasSize`'s limit).
const POSITION_LIMIT_PX: f32 = 300_000.0;
/// Reach of the W/H fields. The engine takes any scale; this lets a 30 px layer span the largest
/// document (#1267).
const SCALE_LIMIT_PCT: f32 = 1_000_000.0;

/// The W or H field's limit for a box side of `side` px: as far as the result stays within the
/// largest document side, so a typed scale can't ask for an image too large to allocate.
fn scale_limit_pct(side: f64) -> f32 {
    let side = side.abs();
    if !side.is_finite() || side < 1e-6 {
        return SCALE_LIMIT_PCT;
    }
    ((f64::from(POSITION_LIMIT_PX) / side * 100.0) as f32).clamp(100.0, SCALE_LIMIT_PCT)
}

/// Where Free Transform remembers the unit of its W and H fields, picked by right-clicking either
/// of them as in Photoshop (Percent until another is picked).
const SIZE_UNIT_KEY: &str = "edit.freeTransform";

/// The W/H unit, with the document's resolution and the points per inch it converts with.
fn size_unit(app: &PhotocraftApp) -> crate::layer_props_ui::LenUnit {
    use photocraft_engine::prefs::Unit;
    let unit = app.session.prefs().dialogs.get(SIZE_UNIT_KEY).and_then(|v| v.get("sizeUnit")).and_then(serde_json::Value::as_str).and_then(Unit::parse);
    crate::layer_props_ui::LenUnit { unit: unit.unwrap_or(Unit::Percent), ..crate::layer_props_ui::LenUnit::of(app) }
}

fn set_size_unit(app: &mut PhotocraftApp, name: &str) {
    app.session.prefs.edit(|p| {
        let entry = p.dialogs.entry(SIZE_UNIT_KEY.to_string()).or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({});
        }
        entry["sizeUnit"] = json!(name);
    });
}

/// A W or H value shown in `u`: the box side `side` px wide at the original size, scaled by `pct`.
/// Percent stays the scale itself (100 % is the original size), lengths are the side's length.
fn size_shown(u: &crate::layer_props_ui::LenUnit, pct: f64, side: f64) -> f32 {
    u.shown(pct / 100.0 * side, side)
}

/// The scale (%) a W or H value typed in `u` asks for, for a box side `side` px wide originally.
fn size_pct(u: &crate::layer_props_ui::LenUnit, v: f32, side: f64) -> f64 {
    let side = side.abs().max(1e-9);
    u.px(v, side) / side * 100.0
}

/// Options bar while transforming: the fields (reference point X/Y, W/H %, angle, interpolation,
/// or the warp controls), then the mode switch, cancel and commit right after them. A bar too
/// narrow for both pins the buttons to the right edge and clips the fields, so Commit and Cancel
/// are always reachable.
pub fn options_bar(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let Some(t) = app.ui.transform.clone() else { return };
    let bar = ui.available_rect_before_wrap();
    let warping = t.warp.is_some();
    let buttons_w = if warping { WARP_ACTIONS_W } else { ACTIONS_W };
    let split = (bar.right() - buttons_w).max(bar.left());
    let fields = egui::Rect::from_min_max(bar.min, egui::pos2(split, bar.bottom()));
    let mut row = ui.new_child(egui::UiBuilder::new().id_salt("transform-fields").max_rect(fields).layout(egui::Layout::left_to_right(egui::Align::Center)));
    row.set_clip_rect(fields);
    if let Some(w) = t.warp.clone() {
        warp_fields(app, &mut row, &w);
    } else if t.content_aware.is_some() {
        crate::cas_transform::fields(app, &mut row, &t);
    } else {
        transform_fields(app, &mut row, &t);
    }
    crate::widgets::vline(&mut row, 22.0);
    let start = row.min_rect().right().min(split);
    let actions = egui::Rect::from_min_max(egui::pos2(start, bar.top()), egui::pos2(start + buttons_w, bar.bottom()));
    let mut btns = ui.new_child(egui::UiBuilder::new().id_salt("transform-actions").max_rect(actions).layout(egui::Layout::right_to_left(egui::Align::Center)));
    transform_actions(app, &mut btns, warping, t.content_aware.is_none());
}

fn transform_fields(app: &mut PhotocraftApp, ui: &mut egui::Ui, t: &TransformSession) {
    let tk = crate::theme::Tokens::get(ui.ctx());
    let (_, _, angle, skew_h) = readout(t);
    let lbl = |ui: &mut egui::Ui, s: &str| {
        ui.label(egui::RichText::new(s).color(tk.text_dim).size(12.0));
    };
    position_size_fields(app, ui, t);
    crate::widgets::vline(ui, 22.0);
    let mut a = angle as f32;
    let (r, resp) = ui.allocate_exact_size(vec2(18.0, 22.0), egui::Sense::hover());
    crate::icons::paint(ui, r, "rotate-cw", 13.0, tk.text_dim);
    resp.on_hover_text(tl!("Rotate"));
    if crate::widgets::value_field(ui, &mut a, -180.0..=180.0, "°", 60.0).accessible_name(tl!("Rotate")).changed() {
        rotate_about_pivot(app, (a as f64 - angle).to_radians());
    }
    // Photoshop's H and V skew fields: the box's sides lean by H, its top and bottom by V.
    let (mut h, mut v) = (skew_h as f32, t.skew_v as f32);
    let lim = -SKEW_LIMIT_DEG..=SKEW_LIMIT_DEG;
    lbl(ui, "H:");
    let rh =
        crate::widgets::value_field(ui, &mut h, lim.clone(), "°", 56.0).accessible_name(tl!("Set horizontal skew")).on_hover_text(tl!("Set horizontal skew"));
    lbl(ui, "V:");
    let rv = crate::widgets::value_field(ui, &mut v, lim, "°", 56.0).accessible_name(tl!("Set vertical skew")).on_hover_text(tl!("Set vertical skew"));
    if rh.changed() || rv.changed() {
        skew_about_pivot(app, f64::from(h), f64::from(v));
    }
    crate::widgets::vline(ui, 22.0);
    lbl(ui, tl!("Interpolation:"));
    let mut interp = t.interpolation.clone();
    let opts = [("bicubic".to_string(), tl!("Bicubic")), ("bilinear".to_string(), tl!("Bilinear")), ("nearest".to_string(), tl!("Nearest Neighbor"))];
    if crate::widgets::dropdown(ui, "transform-interp", tl!("Interpolation"), &mut interp, &opts, 130.0)
        && let Some(s) = app.ui.transform.as_mut()
    {
        s.interpolation = interp;
    }
}

fn matching_anchor(quad: [[f64; 2]; 4], pivot: [f64; 2]) -> Option<usize> {
    (0..9).find(|&i| {
        let p = point_on_quad(quad, i);
        (p[0] - pivot[0]).abs() <= 0.05 && (p[1] - pivot[1]).abs() <= 0.05
    })
}

fn anchor_grid_widget(ui: &mut egui::Ui, current_anchor: Option<usize>) -> Option<usize> {
    let size = egui::vec2(16.0, 16.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let tk = crate::theme::Tokens::get(ui.ctx());
    let painter = ui.painter();
    painter.rect_stroke(rect, 1.0, egui::Stroke::new(1.0, tk.card_border), egui::StrokeKind::Inside);
    let mut clicked = None;
    let pointer = resp.interact_pointer_pos();
    for row in 0..3 {
        for col in 0..3 {
            let idx = row * 3 + col;
            let cx = rect.min.x + (col as f32 + 0.5) * (rect.width() / 3.0);
            let cy = rect.min.y + (row as f32 + 0.5) * (rect.height() / 3.0);
            let pt_rect = egui::Rect::from_center_size(egui::pos2(cx, cy), egui::vec2(3.5, 3.5));
            if resp.clicked() && pointer.is_some_and(|p| (p.x - cx).abs() <= 3.0 && (p.y - cy).abs() <= 3.0) {
                clicked = Some(idx);
            }
            if current_anchor == Some(idx) {
                painter.rect_filled(pt_rect, 0.5, tk.accent);
            } else {
                painter.rect_filled(pt_rect, 0.5, tk.text_dim);
            }
        }
    }
    resp.on_hover_text(tl!("Reference point location"));
    clicked
}

/// The reference point's X/Y and the W/H scale fields (with the aspect link) of the options bar.
pub(crate) fn position_size_fields(app: &mut PhotocraftApp, ui: &mut egui::Ui, t: &TransformSession) {
    let tk = crate::theme::Tokens::get(ui.ctx());
    let (sx, sy, _, _) = readout(t);
    let lbl = |ui: &mut egui::Ui, s: &str| {
        ui.label(egui::RichText::new(s).color(tk.text_dim).size(12.0));
    };
    let current_anchor = matching_anchor(t.quad, t.pivot);
    let mut pivot = t.pivot;
    if let Some(new_anchor) = anchor_grid_widget(ui, current_anchor) {
        let new_p = point_on_quad(t.quad, new_anchor);
        if let Some(s) = app.ui.transform.as_mut() {
            s.pivot = new_p;
        }
        pivot = new_p;
    }
    ui.add_space(4.0);
    let mut px = pivot[0] as f32;
    let mut py = pivot[1] as f32;
    lbl(ui, "X:");
    let rx = crate::widgets::value_field(ui, &mut px, -POSITION_LIMIT_PX..=POSITION_LIMIT_PX, "px", 72.0).accessible_name("X");
    lbl(ui, "Y:");
    let ry = crate::widgets::value_field(ui, &mut py, -POSITION_LIMIT_PX..=POSITION_LIMIT_PX, "px", 72.0).accessible_name("Y");
    if (rx.changed() || ry.changed())
        && let Some(s) = app.ui.transform.as_mut()
    {
        let (dx, dy) = (px as f64 - s.pivot[0], py as f64 - s.pivot[1]);
        s.quad = s.quad.map(|q| [q[0] + dx, q[1] + dy]);
        s.pivot = [px as f64, py as f64];
    }
    crate::widgets::vline(ui, 22.0);
    // W and H in the unit picked from their right-click menu: the scale in percent, or the box's
    // length (Photoshop's units, at the document's resolution).
    let u = size_unit(app);
    let (side_w, side_h) = (t.rect[2] - t.rect[0], t.rect[3] - t.rect[1]);
    let (wlim, hlim) = (scale_limit_pct(side_w), scale_limit_pct(side_h));
    let (wlim_u, hlim_u) = (size_shown(&u, f64::from(wlim), side_w), size_shown(&u, f64::from(hlim), side_h));
    let (mut w, mut h) = (size_shown(&u, sx, side_w), size_shown(&u, sy, side_h));
    let (suffix, places) = (u.unit.suffix(), u.unit.decimals() as u32);
    // Lengths need more room than a percentage ("2116.67 mm").
    let size_w = if u.unit == photocraft_engine::prefs::Unit::Percent { 66.0 } else { 84.0 };
    // Whatever their unit, W and H take any other typed in (`500 px` or `2 in` in percent, `50%`
    // in a length), converted at the document's resolution against the box's original side (#6284).
    let units = crate::widgets::FieldUnits { unit: u.unit, dpi: u.dpi, ppi: u.ppi };
    lbl(ui, "W:");
    let rw = crate::widgets::value_field_extent(ui, &mut w, -wlim_u..=wlim_u, suffix, size_w, places, units, side_w).accessible_name(tl!("Width"));
    let link = !app.ui.transform_unlinked;
    if crate::icons::button(ui, if link { "link" } else { "unlink" }, 22.0, link, tl!("Maintain aspect ratio")).clicked() {
        app.ui.transform_unlinked = link;
    }
    lbl(ui, "H:");
    let rh = crate::widgets::value_field_extent(ui, &mut h, -hlim_u..=hlim_u, suffix, size_w, places, units, side_h).accessible_name(tl!("Height"));
    for r in [&rw, &rh] {
        r.context_menu(|ui| {
            if let Some(name) = crate::rulers::unit_menu(ui, u.unit) {
                set_size_unit(app, name);
            }
        });
    }
    let (w, h) = (size_pct(&u, w, side_w) as f32, size_pct(&u, h, side_h) as f32);
    if rw.changed() || rh.changed() {
        let (kx, ky) = if link {
            let k = if rw.changed() { w as f64 / nonzero(sx) } else { h as f64 / nonzero(sy) };
            // Linked, the other side follows: keep it within its own limit too.
            let k = k.min(f64::from(wlim) / sx.abs().max(1e-9)).min(f64::from(hlim) / sy.abs().max(1e-9));
            (k, k)
        } else {
            (w as f64 / nonzero(sx), h as f64 / nonzero(sy))
        };
        scale_about_pivot(app, kx, ky);
    }
}

/// Options bar fields in Warp mode: style, bend, the split icons and the grid menu.
fn warp_fields(app: &mut PhotocraftApp, ui: &mut egui::Ui, w: &Warp) {
    let tk = crate::theme::Tokens::get(ui.ctx());
    let lbl = |ui: &mut egui::Ui, s: &str| {
        ui.label(egui::RichText::new(s).color(tk.text_dim).size(12.0));
    };
    // Type takes only the styles (#3004): Split and Grid are greyed, and there is no Custom.
    let on_type = warps_type(app);
    // Split, Grid, then the warp style with its bend fields.
    ui.add_enabled_ui(!on_type, |ui| warp_mesh_fields(app, ui, w));
    crate::widgets::vline(ui, 22.0);
    lbl(ui, tl!("Warp:"));
    let mut style = w.style;
    let opts: Vec<(WarpStyle, &str)> = WarpStyle::all().filter(|s| !(on_type && *s == WarpStyle::Custom)).map(|s| (s, s.label())).collect();
    if crate::widgets::dropdown(ui, "warp-style", tl!("Warp"), &mut style, &opts, 120.0)
        && let Some(t) = app.ui.transform.as_mut()
    {
        let b = t.rect;
        t.warp = Some(match style {
            WarpStyle::Custom => Warp::custom(w.to_mesh(1, 1), b),
            WarpStyle::None if on_type => Warp::none(b),
            WarpStyle::None => Warp::custom(BezierMesh::identity(b, 1, 1), b),
            s => Warp { style: s, bend: if w.style.is_preset() { w.bend } else { 50.0 }, ..Warp::none(b) },
        });
    }
    // Orientation and the bend fields are always there, and only act on a preset style.
    let preset = w.style.is_preset();
    ui.add_enabled_ui(preset, |ui| {
        let orient = crate::icons::button(ui, "arrow-left-right", 26.0, w.vertical, tl!("Vertical"));
        orient.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, preset, "Change warp orientation"));
        if orient.clicked()
            && let Some(Some(cur)) = app.ui.transform.as_mut().map(|t| t.warp.as_mut())
        {
            cur.vertical = !cur.vertical;
        }
        let fields: [(&str, f64); 3] = [(tl!("Bend:"), w.bend), ("H:", w.h_distort), ("V:", w.v_distort)];
        for (k, (name, v)) in fields.iter().enumerate() {
            lbl(ui, name);
            let mut f = *v as f32;
            if crate::widgets::value_field(ui, &mut f, -100.0..=100.0, "%", 58.0).accessible_name(*name).changed()
                && let Some(Some(cur)) = app.ui.transform.as_mut().map(|t| t.warp.as_mut())
            {
                match k {
                    0 => cur.bend = f64::from(f),
                    1 => cur.h_distort = f64::from(f),
                    _ => cur.v_distort = f64::from(f),
                }
            }
        }
    });
}

/// The Split icons and the Grid menu of the Warp options bar.
fn warp_mesh_fields(app: &mut PhotocraftApp, ui: &mut egui::Ui, w: &Warp) {
    let tk = crate::theme::Tokens::get(ui.ctx());
    let lbl = |ui: &mut egui::Ui, s: &str| {
        ui.label(egui::RichText::new(s).color(tk.text_dim).size(12.0));
    };
    lbl(ui, tl!("Split:"));
    let armed = app.transform_preview.as_ref().and_then(|p| p.split_tool);
    for (tool, tip) in [
        (SplitTool::Cross, tl!("Split Warp Crosswise")),
        (SplitTool::Vertical, tl!("Split Warp Vertically")),
        (SplitTool::Horizontal, tl!("Split Warp Horizontally")),
    ] {
        let on = armed == Some(tool);
        if split_icon(ui, tool.icon(), tip, on).clicked()
            && let Some(pv) = app.transform_preview.as_mut()
        {
            pv.split_tool = if on { None } else { Some(tool) };
            pv.split_placing = false;
            pv.split_pointer = None;
            pv.split_quick = false;
        }
    }
    crate::widgets::vline(ui, 22.0);
    lbl(ui, tl!("Grid:"));
    let mesh = w.mesh.clone().unwrap_or_else(|| BezierMesh::identity(w.bounds, 1, 1));
    let mut grid = warp_grid_id(&mesh).to_string();
    let opts = [
        ("custom".to_string(), tl!("Custom")),
        ("default".to_string(), tl!("Default")),
        ("3".to_string(), tl!("3 x 3")),
        ("4".to_string(), tl!("4 x 4")),
        ("5".to_string(), tl!("5 x 5")),
    ];
    if crate::widgets::dropdown(ui, "warp-grid", tl!("Grid"), &mut grid, &opts, 92.0)
        && let Some(n) = match grid.as_str() {
            "default" => Some(1),
            "3" => Some(3),
            "4" => Some(4),
            "5" => Some(5),
            _ => None,
        }
    {
        let _ = edit_session_warp(app, "edit.transform.warpGrid", &json!({ "size": n }));
    }
}

/// Back to the untouched box: the default one-patch grid over the box, no bend.
pub fn reset_warp(app: &mut PhotocraftApp) {
    let Some(t) = app.ui.transform.as_mut() else { return };
    if t.warp.is_none() {
        return;
    }
    let b = t.rect;
    t.warp = Some(Warp::custom(BezierMesh::identity(b, 1, 1), b));
    if warps_type(app)
        && let Some(t) = app.ui.transform.as_mut()
    {
        t.warp = Some(Warp::none(b));
    }
    if let Some(pv) = app.transform_preview.as_mut() {
        pv.warp_drag = None;
        pv.split_tool = None;
        pv.split_pointer = None;
        pv.split_placing = false;
        pv.split_quick = false;
    }
}

/// Grid menu readout: Default is 1×1, the numbered entries are uniform n×n patches, and anything
/// split by hand reads as Custom.
fn warp_grid_id(mesh: &BezierMesh) -> &'static str {
    let cols = mesh.us.len().saturating_sub(1);
    let rows = mesh.vs.len().saturating_sub(1);
    let even = |k: &[f64]| {
        let n = k.len().saturating_sub(1);
        n > 0 && k.iter().enumerate().all(|(i, v)| (v - i as f64 / n as f64).abs() < 1e-4)
    };
    if cols == rows && even(&mesh.us) && even(&mesh.vs) {
        match cols {
            1 => "default",
            3 => "3",
            4 => "4",
            5 => "5",
            _ => "custom",
        }
    } else {
        "custom"
    }
}

/// One of the three Split icons. `on` is the armed tool: the line then follows the pointer.
fn split_icon(ui: &mut egui::Ui, kind: &str, tip: &str, on: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), egui::Sense::click());
    let ink = crate::icons::button_chrome(ui, rect, on, resp.hovered());
    let c = rect.center();
    let stroke = Stroke::new(1.15, ink);
    let box_r = egui::Rect::from_center_size(c, vec2(13.0, 13.0));
    ui.painter().rect_stroke(box_r, 2.0, stroke, egui::StrokeKind::Inside);
    let h = 5.2;
    match kind {
        "vertical" => {
            ui.painter().line_segment([c - vec2(0.0, h), c + vec2(0.0, h)], stroke);
        }
        "horizontal" => {
            ui.painter().line_segment([c - vec2(h, 0.0), c + vec2(h, 0.0)], stroke);
        }
        _ => {
            ui.painter().line_segment([c - vec2(h, 0.0), c + vec2(h, 0.0)], stroke);
            ui.painter().line_segment([c - vec2(0.0, h), c + vec2(0.0, h)], stroke);
        }
    }
    if kind == "cross" {
        ui.painter().circle_filled(c, 1.5, ink);
    } else {
        ui.painter().circle_stroke(c, 2.1, stroke);
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, tip));
    resp.on_hover_text(tip)
}

/// Warp switch, cancel and commit, kept on the right of the options bar.
/// `mode_switch`: the Free Transform / Warp switch (not for Content-Aware Scale, which only scales).
fn transform_actions(app: &mut PhotocraftApp, ui: &mut egui::Ui, warping: bool, mode_switch: bool) {
    ui.add_space(8.0);
    let commit_tip = if warping {
        crate::i18n::fmt(tl!("Commit warp ({key})"), &[("key", &crate::shortcuts::pretty("Enter"))])
    } else {
        crate::i18n::fmt(tl!("Commit transform ({key})"), &[("key", &crate::shortcuts::pretty("Enter"))])
    };
    let commit_btn = crate::icons::button(ui, "check", 26.0, false, &commit_tip);
    let label = "Commit transform";
    commit_btn.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, tl!(label)));
    if commit_btn.clicked() {
        commit(app);
    }
    let cancel_tip = if warping { tl!("Cancel warp (Esc)") } else { tl!("Cancel transform (Esc)") };
    let cancel_btn = crate::icons::button(ui, "ban", 26.0, false, cancel_tip);
    let label = "Cancel transform";
    cancel_btn.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, tl!(label)));
    if cancel_btn.clicked() {
        cancel(app);
    }
    if warping {
        let reset = crate::icons::button(ui, "undo-2", 26.0, false, tl!("Reset"));
        reset.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Reset warp"));
        if reset.clicked() {
            reset_warp(app);
        }
    }
    if !mode_switch {
        return;
    }
    crate::widgets::vline(ui, 22.0);
    let mode = crate::icons::button(ui, "grid-3x3", 26.0, warping, tl!("Switch between free transform and warp modes"));
    mode.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Switch between free transform and warp modes"));
    if mode.clicked() {
        if warping {
            leave_warp(app);
        } else {
            enter_warp(app, &ui.ctx().clone());
        }
    }
}

/// Scale the box along its own axes about the reference point.
fn scale_about_pivot(app: &mut PhotocraftApp, kx: f64, ky: f64) {
    let Some(s) = app.ui.transform.as_mut() else { return };
    let Some(h) = Homography::rect_to_quad([0.0, 0.0, 1.0, 1.0], s.quad) else { return };
    let Some(inv) = h.inverse() else { return };
    let (pu, pv) = inv.apply(s.pivot[0], s.pivot[1]);
    let m = |u: f64, v: f64| {
        let (x, y) = h.apply(pu + (u - pu) * kx, pv + (v - pv) * ky);
        [x, y]
    };
    s.quad = [m(0.0, 0.0), m(1.0, 0.0), m(1.0, 1.0), m(0.0, 1.0)];
}

fn rotate_about_pivot(app: &mut PhotocraftApp, da: f64) {
    let Some(s) = app.ui.transform.as_mut() else { return };
    let (sn, cs) = da.sin_cos();
    let c = s.pivot;
    s.quad = s.quad.map(|q| {
        let (x, y) = (q[0] - c[0], q[1] - c[1]);
        [c[0] + x * cs - y * sn, c[1] + x * sn + y * cs]
    });
}

#[cfg(test)]
mod tests;
