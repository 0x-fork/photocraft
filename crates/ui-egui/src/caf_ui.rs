//! Edit › Content-Aware Fill…: Photoshop's Content-Aware Fill workspace, a full-window dialog
//! like Liquify's.
//!
//! Measured on Photoshop 25.4 (black box) and laid out the same way: the tools on the left
//! (Sampling Brush, Lasso, Hand, Zoom) with their options bar above, the document in the middle
//! with the sampling area tinted (green, 50 %) and the fill area outlined, a Preview panel, and
//! the Content-Aware Fill panel (Sampling Area Overlay, Sampling Area Options, Fill Settings,
//! Output Settings) with OK, Apply and Cancel.
//!
//! * **Sampling Brush**: a hard round brush. In Auto and Rectangular it subtracts from the
//!   sampling area, in Custom (which starts empty) it adds; Alt reverses. Painting keeps the
//!   sampling option, and added strokes may reach past the sampling window. Its starting size is
//!   a tenth of the window's side (20 px for a 50 px selection).
//! * **Lasso**: edits the fill area itself (New, Add, Subtract, Intersect; Expand and Contract by
//!   a few pixels). The window and the Auto area follow the new fill area; brush strokes stay.
//! * **Preview**: the fill computed in the background after each change, opening framed on the
//!   sampling window.
//! * **Apply** fills (one history step) and keeps the workspace open; **OK** fills and closes;
//!   **Cancel** closes, keeping what was applied. Ctrl+Z undoes the last stroke or lasso edit.
//!
//! What the workspace holds is plain data (the sampling option, brush strokes, lasso edits and
//! fill settings); OK and Apply run `edit.contentAwareFill` with it, and the preview runs the same
//! plan ([`photocraft_engine::edit_menu_cmds::content_aware_plan`]), so the preview is exactly
//! what gets written. The settings are remembered for the next time (`prefs.dialogs`).

use egui::{Align2, Color32, FontId, Pos2, Rect as ERect, Sense, Stroke, TextureHandle, pos2, vec2};
use photocraft_algo::content_aware::{SamplingStroke, paint_strokes};
use photocraft_doc::LayerId;
use photocraft_engine::edit_menu_cmds::{MAX_CAF_EDITS, MAX_CAF_POINTS};
use photocraft_geom::Rect;
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::canvas::ToolEvent;
use crate::theme::Tokens;
use crate::widgets;

/// Longest side of the document proxy shown in the view (px).
const PROXY_SIDE: usize = 1600;
const LEFT_W: f32 = 48.0;
const BAR_H: f32 = 36.0;
const PANEL_W: f32 = 300.0;
const PREVIEW_W: f32 = 380.0;
const UNDO_LIMIT: usize = 100;
/// Outlines with more edges than this are drawn at the proxy's resolution.
const MAX_OUTLINE_EDGES: usize = 200_000;
/// Photoshop's overlay colour as it shows (a 50 % tint of grey 200 reads 171, 204, 153).
const OVERLAY_COLOR: [u8; 3] = [142, 208, 106];
/// The `prefs.dialogs` key under which the settings are remembered.
const REMEMBERED: &str = "edit.contentAwareFill.workspace";
const CMD: &str = "edit.contentAwareFill";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CafTool {
    Brush,
    Lasso,
    Hand,
    Zoom,
}

impl CafTool {
    fn name(self) -> &'static str {
        match self {
            CafTool::Brush => "brush",
            CafTool::Lasso => "lasso",
            CafTool::Hand => "hand",
            CafTool::Zoom => "zoom",
        }
    }
    fn from_name(s: &str) -> Option<CafTool> {
        Some(match s {
            "brush" => CafTool::Brush,
            "lasso" => CafTool::Lasso,
            "hand" => CafTool::Hand,
            "zoom" => CafTool::Zoom,
            _ => return None,
        })
    }
}

/// The lasso's selection modes, in options-bar order.
const LASSO_OPS: [&str; 4] = ["new", "add", "subtract", "intersect"];

/// The panel's settings (remembered between uses).
#[derive(Clone, Debug, PartialEq)]
pub struct CafOpts {
    pub show_overlay: bool,
    /// Overlay opacity, 0..100.
    pub opacity: f32,
    pub color: [u8; 3],
    /// The overlay marks what is left out (Indicates: Excluded Area) rather than the sampling area.
    pub excluded: bool,
    /// auto | rectangular | custom
    pub sampling: String,
    pub sample_all: bool,
    /// default | none | high | veryHigh
    pub color_adaptation: String,
    /// none | low | medium | high | full
    pub rotation: String,
    pub scale: bool,
    pub mirror: bool,
    /// current | new | duplicate (Photoshop opens on New Layer)
    pub output: String,
}

impl Default for CafOpts {
    fn default() -> Self {
        CafOpts {
            show_overlay: true,
            opacity: 50.0,
            color: OVERLAY_COLOR,
            excluded: false,
            sampling: "auto".into(),
            sample_all: false,
            color_adaptation: "default".into(),
            rotation: "none".into(),
            scale: false,
            mirror: false,
            output: "new".into(),
        }
    }
}

impl CafOpts {
    /// Applies the settings named in `v` (control channel and remembered keys); others stay.
    pub fn apply(&mut self, v: &Value) -> Result<(), String> {
        let flag = |k: &str| v.get(k).and_then(Value::as_bool);
        let text = |k: &str| v.get(k).and_then(Value::as_str);
        for (k, slot) in [
            ("showOverlay", &mut self.show_overlay),
            ("excluded", &mut self.excluded),
            ("sampleAllLayers", &mut self.sample_all),
            ("scale", &mut self.scale),
            ("mirror", &mut self.mirror),
        ] {
            if let Some(b) = flag(k) {
                *slot = b;
            }
        }
        if let Some(n) = v.get("opacity").and_then(Value::as_f64).filter(|n| n.is_finite()) {
            self.opacity = (n as f32).clamp(0.0, 100.0);
        }
        if let Some(c) = v.get("color").and_then(Value::as_array).filter(|c| c.len() == 3) {
            let ch = |i: usize| c[i].as_u64().map(|v| v.min(255) as u8);
            if let (Some(r), Some(g), Some(b)) = (ch(0), ch(1), ch(2)) {
                self.color = [r, g, b];
            }
        }
        let pick = |k: &str, allowed: &[&str]| -> Result<Option<String>, String> {
            match text(k) {
                None => Ok(None),
                Some(s) if allowed.contains(&s) => Ok(Some(s.to_string())),
                Some(s) => Err(format!("bad {k} `{s}` ({})", allowed.join("|"))),
            }
        };
        if let Some(s) = pick("sampling", &["auto", "rectangular", "custom"])? {
            self.sampling = s;
        }
        if let Some(s) = pick("colorAdaptation", &["none", "default", "high", "veryHigh"])? {
            self.color_adaptation = s;
        }
        if let Some(s) = pick("rotationAdaptation", &["none", "low", "medium", "high", "full"])? {
            self.rotation = s;
        }
        if let Some(s) = pick("output", &["current", "new", "duplicate"])? {
            self.output = s;
        }
        Ok(())
    }

    /// The settings as [`CafOpts::apply`] reads them.
    pub fn to_json(&self) -> Value {
        json!({
            "showOverlay": self.show_overlay,
            "opacity": self.opacity,
            "color": self.color,
            "excluded": self.excluded,
            "sampling": self.sampling,
            "sampleAllLayers": self.sample_all,
            "colorAdaptation": self.color_adaptation,
            "rotationAdaptation": self.rotation,
            "scale": self.scale,
            "mirror": self.mirror,
            "output": self.output,
        })
    }
}

/// One undoable step.
#[derive(Clone, Debug)]
enum Step {
    Stroke(SamplingStroke),
    Fill(Value),
    /// A change of the sampling option (or a reset), with what it replaced.
    State {
        sampling: String,
        strokes: Vec<SamplingStroke>,
        fill_edits: Vec<Value>,
    },
}

fn stroke_json(s: &SamplingStroke) -> Value {
    json!({"mode": if s.add { "add" } else { "subtract" }, "size": s.size, "points": s.points})
}

/// What the plan (without the brush strokes) says: the window, the fill area and the sampling
/// option's area, over `rect`.
struct Base {
    window: Rect,
    rect: Rect,
    hole: Vec<bool>,
    base: Vec<bool>,
}

impl Base {
    /// The index into `hole` and `base` of document point `(x, y)`, when it is inside `rect`.
    fn index(&self, x: f64, y: f64) -> Option<usize> {
        let (xi, yi) = (x.floor() as i32, y.floor() as i32);
        self.rect.contains(xi, yi).then(|| (yi - self.rect.y0) as usize * self.rect.width() as usize + (xi - self.rect.x0) as usize)
    }
}

/// A computed preview: the filled rectangle as an image, or why it failed.
#[cfg(not(target_arch = "wasm32"))]
type PreviewResult = Result<(Rect, egui::ColorImage), String>;

/// The preview being computed or shown.
#[derive(Default)]
struct Preview {
    /// Bumped whenever the state changes; a result for an older generation is dropped.
    want: u64,
    #[cfg(not(target_arch = "wasm32"))]
    running: Option<(u64, std::sync::mpsc::Receiver<PreviewResult>, photocraft_engine::jobs::JobCtx)>,
    shown: u64,
    /// A finished preview waiting to be uploaded (on the UI thread, which has the context).
    pending_image: Option<(Rect, egui::ColorImage)>,
    tex: Option<(TextureHandle, Rect)>,
    /// Screen px per document px (0 = frame the window on the next frame) and the view centre.
    zoom: f32,
    center: [f64; 2],
    error: Option<String>,
}

impl Preview {
    /// Takes the result of the preview of state `generation`: an image to upload, or the error,
    /// which stands for that state until it changes (the same preview is not started again).
    fn finish(&mut self, generation: u64, r: Result<(Rect, egui::ColorImage), String>) {
        self.shown = generation;
        self.tex = None;
        match r {
            Ok(img) => {
                self.pending_image = Some(img);
                self.error = None;
            }
            Err(e) => {
                self.pending_image = None;
                self.error = Some(e);
            }
        }
    }
}

pub struct CafWorkspace {
    pub layer: LayerId,
    layer_name: String,
    canvas: Rect,
    /// The document revision the textures and the base were made from.
    revision: u64,
    pub opts: CafOpts,
    pub tool: CafTool,
    pub brush_add: bool,
    pub brush_size: f64,
    pub lasso_op: usize,
    pub expand_by: f32,
    pub strokes: Vec<SamplingStroke>,
    pub fill_edits: Vec<Value>,
    undo: Vec<Step>,
    redo: Vec<Step>,
    cur: Option<SamplingStroke>,
    /// The lasso polygon being drawn and its selection mode.
    lasso: Option<(usize, Vec<[f64; 2]>)>,
    base: Base,
    /// Fill-area outline edges (document px), or `None` when there are too many.
    outline: Option<Vec<[[f32; 2]; 2]>>,
    /// Document px per proxy px, and the proxy size.
    k: usize,
    pw: usize,
    ph: usize,
    /// Per proxy pixel: the sampling option's area (before the strokes), the fill area, and the
    /// sampling area after the strokes so far (the first painted with them in order, as the
    /// engine's `paint_strokes` paints the full-resolution one).
    base_px: Vec<bool>,
    hole_px: Vec<bool>,
    mask: Vec<bool>,
    /// The pointer is down on the view with a tool: one press is one `Down`, egui's drag (which
    /// starts a few pixels later) continues it.
    pressed: bool,
    /// The device's texture side limit, as the context reports it each frame.
    texture_limit: usize,
    image_tex: Option<TextureHandle>,
    overlay_tex: Option<TextureHandle>,
    overlay_dirty: bool,
    zoom: f32,
    center: [f64; 2],
    hover: Option<[f64; 2]>,
    preview: Preview,
    pub error: Option<String>,
}

/// The preview's error when the sampling area has no pixels outside the fill area (the engine's
/// fill refuses that case with the same words).
const NOTHING_TO_SAMPLE: &str = "nothing to sample from: the sampling area has no pixels outside the fill area";

/// The plan of the current state (without strokes when `strokes` is false).
fn plan_params(w: &CafWorkspace, strokes: bool) -> Value {
    let o = &w.opts;
    let mut p = json!({
        "sampling": o.sampling,
        "sampleAllLayers": o.sample_all,
        "colorAdaptation": o.color_adaptation,
        "rotationAdaptation": o.rotation,
        "scale": o.scale,
        "mirror": o.mirror,
        "output": o.output,
        "layer": w.layer.0,
        "fillEdits": w.fill_edits,
    });
    if strokes && !w.strokes.is_empty() {
        p["samplingStrokes"] = Value::Array(w.strokes.iter().map(stroke_json).collect());
    }
    p
}

impl CafWorkspace {
    pub fn describe(&self) -> Value {
        let sampled = self.mask.iter().zip(&self.hole_px).filter(|(m, h)| **m && !**h).count();
        json!({
            "layer": self.layer.0,
            "tool": self.tool.name(),
            "brushSize": self.brush_size,
            "brushMode": if self.brush_add { "add" } else { "subtract" },
            "lassoMode": LASSO_OPS[self.lasso_op],
            "expandBy": self.expand_by,
            "strokes": self.strokes.len() + usize::from(self.cur.is_some()),
            "fillEdits": self.fill_edits.len(),
            "undo": self.undo.len(),
            "redo": self.redo.len(),
            "window": [self.base.window.x0, self.base.window.y0, self.base.window.width(), self.base.window.height()],
            "fillArea": self.base.hole.iter().filter(|h| **h).count(),
            "sampledProxyPixels": sampled,
            "proxyScale": self.k,
            "settings": self.opts.to_json(),
            "previewReady": self.preview.tex.is_some() && self.preview.shown == self.preview.want,
            "previewError": self.preview.error,
            "error": self.error,
        })
    }

    /// The proxy, in proxy px.
    fn proxy_area(&self) -> Rect {
        Rect::new(0, 0, self.pw as i32, self.ph as i32)
    }

    /// `s` in proxy coordinates: proxy pixel `(px, py)` is centred on document point
    /// `canvas.x0 + (px + 0.5) k`, so the stroke's points and size scale by `1 / k`.
    fn proxy_stroke(&self, s: &SamplingStroke) -> SamplingStroke {
        let k = self.k as f64;
        let (ox, oy) = (f64::from(self.canvas.x0), f64::from(self.canvas.y0));
        SamplingStroke { add: s.add, size: s.size / k, points: s.points.iter().map(|p| [(p[0] - ox) / k, (p[1] - oy) / k]).collect() }
    }

    /// Paints `s` (a stroke, or one segment of the one being drawn) into the proxy's sampling
    /// area: only the pixels within the brush of its path are visited, whatever came before.
    fn paint(&mut self, s: &SamplingStroke) {
        let (s, area) = (self.proxy_stroke(s), self.proxy_area());
        paint_strokes(&mut self.mask, area, &[s]);
        self.overlay_dirty = true;
    }

    /// Takes a new base: its option's area and fill area at the proxy's pixel centres, then the
    /// strokes over it.
    fn set_base(&mut self, b: Base) {
        self.outline = outline(&b);
        let k = self.k as f64;
        for py in 0..self.ph {
            for px in 0..self.pw {
                let (x, y) = (f64::from(self.canvas.x0) + (px as f64 + 0.5) * k, f64::from(self.canvas.y0) + (py as f64 + 0.5) * k);
                let i = b.index(x, y);
                let at = |v: &[bool]| i.is_some_and(|i| v.get(i).copied().unwrap_or(false));
                self.base_px[py * self.pw + px] = at(&b.base);
                self.hole_px[py * self.pw + px] = at(&b.hole);
            }
        }
        self.base = b;
        self.restate_all();
    }

    /// Rebuilds the proxy's sampling area: the base painted with every stroke in order. Each
    /// stroke costs its own pixels, so this is linear in the strokes, not strokes × pixels.
    fn restate_all(&mut self) {
        self.mask.clone_from(&self.base_px);
        let strokes: Vec<SamplingStroke> = self.strokes.iter().chain(self.cur.iter()).map(|s| self.proxy_stroke(s)).collect();
        let area = self.proxy_area();
        paint_strokes(&mut self.mask, area, &strokes);
        self.overlay_dirty = true;
    }

    /// Points in the strokes and lasso edits so far, against the engine's budget.
    fn points_used(&self) -> usize {
        let strokes: usize = self.strokes.iter().chain(self.cur.iter()).map(|s| s.points.len()).sum();
        let lassos: usize = self.fill_edits.iter().map(|e| e.get("points").and_then(Value::as_array).map_or(0, Vec::len)).sum();
        strokes + lassos
    }

    /// The preview is out of date.
    fn stale(&mut self) {
        self.preview.want += 1;
    }

    fn begin_stroke(&mut self, p: [f64; 2], alt: bool) {
        // The engine takes at most MAX_CAF_EDITS strokes and MAX_CAF_POINTS points in all: the
        // brush stops there, so Apply and the preview never fail on what was painted.
        if self.strokes.len() >= MAX_CAF_EDITS {
            self.error = Some(format!("at most {MAX_CAF_EDITS} sampling strokes: undo some, or reset the sampling area"));
            return;
        }
        if self.points_used() >= MAX_CAF_POINTS {
            self.error = Some(points_limit());
            return;
        }
        self.error = None;
        let add = self.brush_add != alt;
        let s = SamplingStroke { add, size: self.brush_size, points: vec![p] };
        self.paint(&s);
        self.cur = Some(s);
    }

    fn extend_stroke(&mut self, p: [f64; 2]) {
        if self.cur.is_none() {
            return;
        }
        if self.points_used() >= MAX_CAF_POINTS {
            self.error = Some(points_limit());
            return;
        }
        let Some(s) = self.cur.as_mut() else { return };
        if s.points.last().is_some_and(|l| l[0] == p[0] && l[1] == p[1]) {
            return;
        }
        let last = *s.points.last().unwrap_or(&p);
        s.points.push(p);
        let seg = SamplingStroke { add: s.add, size: s.size, points: vec![last, p] };
        self.paint(&seg);
    }

    fn end_stroke(&mut self) {
        if let Some(s) = self.cur.take() {
            self.strokes.push(s.clone());
            self.push(Step::Stroke(s));
            self.stale();
        }
    }

    fn push(&mut self, s: Step) {
        self.redo.clear();
        if self.undo.len() == UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.undo.push(s);
    }
}

fn points_limit() -> String {
    format!("at most {MAX_CAF_POINTS} points in all sampling strokes and lasso edits: undo some, or reset")
}

/// The plan without strokes: the window, fill area and the option's area. Custom starts empty
/// (the window and fill area are those of Rectangular).
fn compute_base(app: &PhotocraftApp, w: &CafWorkspace) -> Result<Base, String> {
    let st = app.session.active().ok_or("no document")?;
    let mut p = plan_params(w, false);
    let custom = w.opts.sampling == "custom";
    if custom {
        p["sampling"] = json!("rectangular");
    }
    let plan = photocraft_engine::edit_menu_cmds::content_aware_plan(&st.doc, w.layer, &p, CMD).map_err(|e| e.to_string())?;
    let base = if custom { vec![false; plan.base.len()] } else { plan.base };
    Ok(Base { window: plan.window, rect: plan.rect, hole: plan.hole, base })
}

/// Unit edges between the fill area and the rest, in document px.
fn outline(b: &Base) -> Option<Vec<[[f32; 2]; 2]>> {
    let (w, h) = (b.rect.width() as usize, b.rect.height() as usize);
    let (ox, oy) = (b.rect.x0 as f32, b.rect.y0 as f32);
    let at = |x: i64, y: i64| x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h && b.hole.get(y as usize * w + x as usize).copied().unwrap_or(false);
    let mut out = Vec::new();
    for y in 0..h as i64 {
        for x in 0..w as i64 {
            if !at(x, y) {
                continue;
            }
            let (fx, fy) = (ox + x as f32, oy + y as f32);
            if !at(x - 1, y) {
                out.push([[fx, fy], [fx, fy + 1.0]]);
            }
            if !at(x + 1, y) {
                out.push([[fx + 1.0, fy], [fx + 1.0, fy + 1.0]]);
            }
            if !at(x, y - 1) {
                out.push([[fx, fy], [fx + 1.0, fy]]);
            }
            if !at(x, y + 1) {
                out.push([[fx, fy + 1.0], [fx + 1.0, fy + 1.0]]);
            }
            if out.len() > MAX_OUTLINE_EDGES {
                return None;
            }
        }
    }
    Some(out)
}

/// Recompute the base after a change of the fill area, the sampling option or the document.
fn rebase(app: &mut PhotocraftApp) {
    let Some(w) = app.distort.caf.as_ref() else { return };
    let r = compute_base(app, w);
    let Some(w) = app.distort.caf.as_mut() else { return };
    match r {
        Ok(b) => {
            w.set_base(b);
            w.error = None;
        }
        Err(e) => {
            w.error = Some(e);
            w.restate_all();
        }
    }
    w.stale();
}

/// Opens the workspace on the active layer and its selection.
pub fn open(app: &mut PhotocraftApp) -> Result<(), String> {
    if !app.session.is_enabled(CMD) {
        return Err("Content-Aware Fill needs a selection on a pixel layer".into());
    }
    let st = app.session.active().ok_or("no document")?;
    let layer = st.active_layer.ok_or("no active layer")?;
    let canvas = st.doc.bounds();
    let revision = st.revision;
    let name = st.doc.layer(layer).map(|l| l.name.clone()).unwrap_or_default();
    let k = (canvas.width().max(canvas.height()) as usize).div_ceil(PROXY_SIDE).max(1);
    let (pw, ph) = ((canvas.width() as usize).div_ceil(k), (canvas.height() as usize).div_ceil(k));
    let mut opts = CafOpts::default();
    if let Some(saved) = app.session.prefs().dialogs.get(REMEMBERED) {
        let _ = opts.apply(saved);
    }
    let mut w = CafWorkspace {
        layer,
        layer_name: name,
        canvas,
        revision,
        opts,
        tool: CafTool::Brush,
        brush_add: false,
        brush_size: 20.0,
        lasso_op: 0,
        expand_by: 3.0,
        strokes: Vec::new(),
        fill_edits: Vec::new(),
        undo: Vec::new(),
        redo: Vec::new(),
        cur: None,
        lasso: None,
        base: Base { window: Rect::EMPTY, rect: Rect::EMPTY, hole: Vec::new(), base: Vec::new() },
        outline: None,
        k,
        pw,
        ph,
        base_px: vec![false; pw * ph],
        hole_px: vec![false; pw * ph],
        mask: vec![false; pw * ph],
        pressed: false,
        texture_limit: 2048,
        image_tex: None,
        overlay_tex: None,
        overlay_dirty: true,
        zoom: 0.0,
        center: [f64::from(canvas.x0 + canvas.x1) / 2.0, f64::from(canvas.y0 + canvas.y1) / 2.0],
        hover: None,
        preview: Preview::default(),
        error: None,
    };
    w.brush_add = w.opts.sampling == "custom";
    let base = compute_base(app, &w)?;
    w.brush_size = photocraft_algo::content_aware::default_brush_size(bounds(&base));
    w.set_base(base);
    w.stale();
    app.distort.caf = Some(w);
    Ok(())
}

/// Bounds of the fill area.
fn bounds(b: &Base) -> Rect {
    let w = b.rect.width() as usize;
    b.hole.iter().enumerate().filter(|(_, h)| **h).fold(Rect::EMPTY, |r, (i, _)| {
        let (x, y) = (b.rect.x0 + (i % w) as i32, b.rect.y0 + (i / w) as i32);
        r.union(&Rect::new(x, y, x + 1, y + 1))
    })
}

fn remember(app: &mut PhotocraftApp, opts: &CafOpts) {
    let v = opts.to_json();
    app.session.prefs.edit(|p| p.dialogs.insert(REMEMBERED.into(), v));
}

/// Apply: fill with the current state (one history step); the workspace stays open.
pub fn apply(app: &mut PhotocraftApp) -> Result<Value, String> {
    let Some(w) = app.distort.caf.as_mut() else { return Err("Content-Aware Fill is not open".into()) };
    w.end_stroke();
    let p = plan_params(w, true);
    let opts = w.opts.clone();
    remember(app, &opts);
    let r = app.run(CMD, p);
    if r.is_ok() {
        refresh(app);
    }
    r
}

/// OK: fill and close. A fill that fails (nothing to sample from, a fill area too large, …)
/// leaves the workspace open with the error in its panel, so the areas can be fixed and OK
/// pressed again instead of the work being thrown away.
pub fn commit(app: &mut PhotocraftApp) {
    match apply(app) {
        Ok(_) => close(app),
        Err(e) => report(app, e),
    }
}

/// An Apply or OK that failed: the error in the panel and the status bar.
fn report(app: &mut PhotocraftApp, e: String) {
    if let Some(w) = app.distort.caf.as_mut() {
        w.error = Some(e.clone());
    }
    app.ui.status = e;
    app.ui.status_error = true;
}

/// Cancel (button, Esc): closes; what was applied stays. The settings are kept.
pub fn cancel(app: &mut PhotocraftApp) {
    if let Some(w) = app.distort.caf.as_ref() {
        let opts = w.opts.clone();
        remember(app, &opts);
    }
    close(app);
}

fn close(app: &mut PhotocraftApp) {
    if let Some(w) = app.distort.caf.take() {
        cancel_preview(w);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn cancel_preview(w: CafWorkspace) {
    if let Some((_, _, job)) = w.preview.running {
        job.cancel();
    }
}

#[cfg(target_arch = "wasm32")]
fn cancel_preview(_: CafWorkspace) {}

/// The document changed under the workspace (an Apply): new textures, a new base.
fn refresh(app: &mut PhotocraftApp) {
    let rev = app.session.active().map(|s| s.revision);
    if let Some(w) = app.distort.caf.as_mut() {
        w.image_tex = None;
        w.revision = rev.unwrap_or(w.revision);
    }
    rebase(app);
}

fn undo(app: &mut PhotocraftApp) {
    let Some(w) = app.distort.caf.as_mut() else { return };
    w.end_stroke();
    let Some(step) = w.undo.pop() else { return };
    let rebuild = match &step {
        Step::Stroke(_) => {
            w.strokes.pop();
            w.redo.push(step);
            w.restate_all();
            w.stale();
            // The sampling area changed, as with a new stroke: an error a failed OK or Apply left
            // in the panel no longer applies (the other steps rebase, which clears it too).
            w.error = None;
            false
        }
        Step::Fill(_) => {
            w.fill_edits.pop();
            w.redo.push(step);
            true
        }
        Step::State { sampling, strokes, fill_edits } => {
            let now = Step::State { sampling: w.opts.sampling.clone(), strokes: w.strokes.clone(), fill_edits: w.fill_edits.clone() };
            w.opts.sampling = sampling.clone();
            w.strokes = strokes.clone();
            w.fill_edits = fill_edits.clone();
            w.brush_add = w.opts.sampling == "custom";
            w.redo.push(now);
            true
        }
    };
    if rebuild {
        rebase(app);
    }
}

fn redo(app: &mut PhotocraftApp) {
    let Some(w) = app.distort.caf.as_mut() else { return };
    w.end_stroke();
    let Some(step) = w.redo.pop() else { return };
    let rebuild = match &step {
        Step::Stroke(s) => {
            w.strokes.push(s.clone());
            w.undo.push(step);
            w.restate_all();
            w.stale();
            w.error = None;
            false
        }
        Step::Fill(e) => {
            w.fill_edits.push(e.clone());
            w.undo.push(step);
            true
        }
        Step::State { sampling, strokes, fill_edits } => {
            let now = Step::State { sampling: w.opts.sampling.clone(), strokes: w.strokes.clone(), fill_edits: w.fill_edits.clone() };
            w.opts.sampling = sampling.clone();
            w.strokes = strokes.clone();
            w.fill_edits = fill_edits.clone();
            w.brush_add = w.opts.sampling == "custom";
            w.undo.push(now);
            true
        }
    };
    if rebuild {
        rebase(app);
    }
}

/// A lasso edit of the fill area (or Expand / Contract), undoable.
fn fill_edit(app: &mut PhotocraftApp, edit: Value) {
    let Some(w) = app.distort.caf.as_mut() else { return };
    w.end_stroke();
    // The engine's limits, kept here (see `begin_stroke`).
    if w.fill_edits.len() >= MAX_CAF_EDITS {
        w.error = Some(format!("at most {MAX_CAF_EDITS} lasso edits: undo some, or reset the fill area"));
        return;
    }
    if w.points_used() + edit.get("points").and_then(Value::as_array).map_or(0, Vec::len) > MAX_CAF_POINTS {
        w.error = Some(points_limit());
        return;
    }
    w.fill_edits.push(edit.clone());
    w.push(Step::Fill(edit));
    rebase(app);
    // An edit that empties the fill area is refused (the plan says so): undo it.
    if app.distort.caf.as_ref().is_some_and(|w| w.error.is_some()) {
        let msg = app.distort.caf.as_ref().and_then(|w| w.error.clone());
        undo(app);
        if let Some(w) = app.distort.caf.as_mut() {
            w.redo.clear();
            w.error = msg;
        }
    }
}

/// A new sampling option (Auto, Rectangular, Custom): the brush strokes start over and the brush
/// takes the option's mode (Custom adds, the others subtract).
fn set_sampling(app: &mut PhotocraftApp, sampling: &str) {
    let Some(w) = app.distort.caf.as_mut() else { return };
    if w.opts.sampling == sampling && w.strokes.is_empty() {
        return;
    }
    w.end_stroke();
    let prev = Step::State { sampling: w.opts.sampling.clone(), strokes: std::mem::take(&mut w.strokes), fill_edits: w.fill_edits.clone() };
    w.push(prev);
    w.opts.sampling = sampling.to_string();
    w.brush_add = sampling == "custom";
    rebase(app);
}

/// Reset buttons: the sampling options, the fill settings, or everything.
fn reset(app: &mut PhotocraftApp, what: &str) {
    let Some(w) = app.distort.caf.as_mut() else { return };
    let d = CafOpts::default();
    match what {
        "fill" => {
            w.opts.color_adaptation = d.color_adaptation;
            w.opts.rotation = d.rotation;
            w.opts.scale = d.scale;
            w.opts.mirror = d.mirror;
            w.stale();
        }
        "sampling" => {
            w.opts.sample_all = false;
            set_sampling(app, "auto");
        }
        "lasso" => {
            if !w.fill_edits.is_empty() {
                let prev = Step::State { sampling: w.opts.sampling.clone(), strokes: w.strokes.clone(), fill_edits: std::mem::take(&mut w.fill_edits) };
                w.push(prev);
                rebase(app);
            }
        }
        _ => {
            let prev =
                Step::State { sampling: w.opts.sampling.clone(), strokes: std::mem::take(&mut w.strokes), fill_edits: std::mem::take(&mut w.fill_edits) };
            w.push(prev);
            w.opts = CafOpts::default();
            w.brush_add = false;
            rebase(app);
        }
    }
}

/// Control channel: `edit.contentAwareFill {"ui": {...}}` while the workspace is open.
pub fn control(app: &mut PhotocraftApp, ui: &Value) -> Result<Value, String> {
    let flag = |k: &str| ui.get(k).and_then(Value::as_bool) == Some(true);
    if flag("cancel") {
        cancel(app);
        return Ok(json!({"cancelled": true}));
    }
    let before = app.distort.caf.as_ref().ok_or("Content-Aware Fill is not open")?.opts.clone();
    {
        let w = app.distort.caf.as_mut().ok_or("Content-Aware Fill is not open")?;
        let mut o = w.opts.clone();
        o.apply(ui)?;
        // The sampling option goes through `set_sampling` (it resets the strokes).
        let sampling = std::mem::replace(&mut o.sampling, w.opts.sampling.clone());
        w.opts = o;
        if let Some(t) = ui.get("tool").and_then(Value::as_str) {
            w.tool = CafTool::from_name(t).ok_or(format!("bad tool `{t}` (brush|lasso|hand|zoom)"))?;
        }
        if let Some(n) = ui.get("brushSize").and_then(Value::as_f64).filter(|n| n.is_finite()) {
            w.brush_size = n.clamp(1.0, 5000.0);
        }
        if let Some(m) = ui.get("brushMode").and_then(Value::as_str) {
            w.brush_add = m == "add";
        }
        if let Some(m) = ui.get("lassoMode").and_then(Value::as_str) {
            w.lasso_op = LASSO_OPS.iter().position(|o| *o == m).ok_or(format!("bad lassoMode `{m}`"))?;
        }
        if let Some(n) = ui.get("expandBy").and_then(Value::as_f64).filter(|n| n.is_finite()) {
            w.expand_by = (n as f32).clamp(1.0, 100.0);
        }
        if sampling != w.opts.sampling {
            set_sampling(app, &sampling);
        }
    }
    let after = app.distort.caf.as_ref().map(|w| w.opts.clone());
    if after.as_ref().is_some_and(|a| a.sample_all != before.sample_all) {
        rebase(app);
    } else if after.as_ref().is_some_and(|a| *a != before)
        && let Some(w) = app.distort.caf.as_mut()
    {
        w.stale();
        w.restate_all();
    }
    if let Some(s) = ui.get("stroke") {
        let pts: Vec<[f64; 2]> = serde_json::from_value(s.get("points").cloned().unwrap_or(Value::Null)).map_err(|e| format!("bad stroke points: {e}"))?;
        let w = app.distort.caf.as_mut().ok_or("Content-Aware Fill is not open")?;
        if let Some(n) = s.get("size").and_then(Value::as_f64) {
            w.brush_size = n.clamp(1.0, 5000.0);
        }
        let alt = s.get("alt").and_then(Value::as_bool).unwrap_or(false);
        if let Some(first) = pts.first() {
            w.begin_stroke(*first, alt);
            for p in &pts[1..] {
                w.extend_stroke(*p);
            }
            w.end_stroke();
        }
    }
    if let Some(l) = ui.get("lasso") {
        let pts: Vec<[f64; 2]> = serde_json::from_value(l.get("points").cloned().unwrap_or(Value::Null)).map_err(|e| format!("bad lasso points: {e}"))?;
        let op = match l.get("op").and_then(Value::as_str) {
            Some(o) if LASSO_OPS.contains(&o) => o.to_string(),
            Some(o) => return Err(format!("bad lasso op `{o}`")),
            None => LASSO_OPS[app.distort.caf.as_ref().map_or(0, |w| w.lasso_op)].to_string(),
        };
        fill_edit(app, json!({"op": op, "points": pts}));
    }
    for k in ["expand", "contract"] {
        if let Some(n) = ui.get(k).and_then(Value::as_f64) {
            fill_edit(app, json!({"op": k, "by": n.clamp(1.0, 100.0)}));
        }
    }
    if let Some(r) = ui.get("reset").and_then(Value::as_str) {
        reset(app, r);
    }
    if flag("undo") {
        undo(app);
    }
    if flag("redo") {
        redo(app);
    }
    if flag("apply") {
        apply(app)?;
    }
    if flag("waitPreview") {
        wait_preview(app)?;
    }
    // OK last, with the settings given alongside. A failed fill keeps the workspace open, as
    // the OK button does.
    if flag("commit") {
        let r = apply(app);
        match &r {
            Ok(_) => close(app),
            Err(e) => report(app, e.clone()),
        }
        return r.map(|v| json!({"committed": true, "result": v}));
    }
    Ok(app.distort.caf.as_ref().map(CafWorkspace::describe).unwrap_or(Value::Null))
}

/// Computes the preview now, on this thread (tests and the control channel).
fn wait_preview(app: &mut PhotocraftApp) -> Result<(), String> {
    let (doc, layer, p, want, limit) = {
        let w = app.distort.caf.as_ref().ok_or("Content-Aware Fill is not open")?;
        let st = app.session.active().ok_or("no document")?;
        (st.doc.clone(), w.layer, plan_params(w, true), w.preview.want, w.texture_limit)
    };
    let r = compute_preview(&doc, layer, &p, &photocraft_raster::Interrupt::NONE, limit);
    let w = app.distort.caf.as_mut().ok_or("Content-Aware Fill is not open")?;
    let err = r.as_ref().err().cloned();
    w.preview.finish(want, r);
    err.map_or(Ok(()), Err)
}

/// The filled rectangle as an image (what Apply would write there), as a texture no wider than
/// `max_side`: past the device's limit it is box-averaged to fit (the rectangle it is drawn over
/// stays the plan's).
fn compute_preview(
    doc: &photocraft_doc::Document,
    layer: LayerId,
    p: &Value,
    ctl: &photocraft_raster::Interrupt,
    max_side: usize,
) -> Result<(Rect, egui::ColorImage), String> {
    let plan = photocraft_engine::edit_menu_cmds::content_aware_plan_with(doc, layer, p, CMD, ctl).map_err(|e| match e {
        photocraft_engine::EngineError::Cancelled => "cancelled".to_string(),
        e => e.to_string(),
    })?;
    // Patches come only from the sampling area outside the fill area. With none (the Sampling
    // Brush erased it all, a Custom area not painted yet) the fill would come out black, so the
    // preview says so instead of showing that.
    if !plan.source.iter().any(|s| *s) {
        return Err(NOTHING_TO_SAMPLE.into());
    }
    let (w, h) = (plan.rect.width() as usize, plan.rect.height() as usize);
    let n = plan.fmt.channels();
    let filled =
        photocraft_algo::content_aware::fill_with(w, h, n, &plan.img, &plan.hole, &plan.source, &plan.opts, ctl).map_err(|_| "cancelled".to_string())?;
    let own = plan.layer_pixels();
    // Pixel `i`: the fill over the fill area (a partly selected pixel by its coverage), the
    // layer's own pixels elsewhere.
    let pixel = |i: usize| -> [f32; 4] {
        let src = if plan.hole[i] { &filled[i * n..(i + 1) * n] } else { &own[i * n..(i + 1) * n] };
        let mut c = photocraft_raster::to_rgba(&plan.fmt, src);
        if plan.hole[i] {
            let k = plan.cover[i];
            let under = photocraft_raster::to_rgba(&plan.fmt, &own[i * n..(i + 1) * n]);
            for j in 0..4 {
                c[j] = under[j] + (c[j] - under[j]) * k;
            }
        }
        c
    };
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let color = |c: [f32; 4]| Color32::from_rgba_unmultiplied(b(c[0]), b(c[1]), b(c[2]), b(c[3]));
    let f = crate::filter_preview_ui::texture_factor(w as u32, h as u32, max_side) as usize;
    if f == 1 {
        let px = (0..w * h).map(|i| color(pixel(i))).collect();
        return Ok((plan.rect, egui::ColorImage::new([w, h], px)));
    }
    let (tw, th) = (w.div_ceil(f), h.div_ceil(f));
    let mut px = Vec::with_capacity(tw * th);
    for ty in 0..th {
        for tx in 0..tw {
            // The block's premultiplied average.
            let (mut acc, mut count) = ([0.0f32; 4], 0.0f32);
            for y in ty * f..((ty + 1) * f).min(h) {
                for x in tx * f..((tx + 1) * f).min(w) {
                    let c = pixel(y * w + x);
                    for j in 0..3 {
                        acc[j] += c[j] * c[3];
                    }
                    acc[3] += c[3];
                    count += 1.0;
                }
            }
            let c = if acc[3] > 0.0 { [acc[0] / acc[3], acc[1] / acc[3], acc[2] / acc[3], acc[3] / count] } else { [0.0; 4] };
            px.push(color(c));
        }
    }
    Ok((plan.rect, egui::ColorImage::new([tw, th], px)))
}

/// Starts a background preview when the shown one is out of date (and the pointer is up), and
/// collects a finished one.
#[cfg(not(target_arch = "wasm32"))]
fn drive_preview(app: &mut PhotocraftApp, ctx: &egui::Context, dragging: bool) {
    let doc = app.session.active().map(|s| s.doc.clone());
    let Some(w) = app.distort.caf.as_mut() else { return };
    let pv = &mut w.preview;
    if let Some((generation, rx, _)) = &pv.running {
        match rx.try_recv() {
            Ok(r) => {
                let generation = *generation;
                pv.running = None;
                if generation == pv.want {
                    match r {
                        Err(e) if e == "cancelled" => {}
                        r => pv.finish(generation, r),
                    }
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // Superseded: stop it, a new one starts when it has gone.
                if *generation != pv.want
                    && let Some((_, _, job)) = &pv.running
                {
                    job.cancel();
                }
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
                return;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => pv.running = None,
        }
    }
    if pv.running.is_some() || pv.shown == pv.want || dragging {
        return;
    }
    let Some(doc) = doc else { return };
    let (layer, p, generation, limit) = (w.layer, plan_params(w, true), w.preview.want, w.texture_limit);
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let job = photocraft_engine::jobs::JobCtx::new();
    let work = job.clone();
    let repaint = ctx.clone();
    let spawned = std::thread::Builder::new().name("caf-preview".into()).spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work.stage(0.0, 1.0, "", |ctl| compute_preview(&doc, layer, &p, ctl, limit))))
            .unwrap_or_else(|_| Err("the preview failed".into()));
        let _ = tx.send(r);
        repaint.request_repaint();
    });
    match spawned {
        Ok(_) => w.preview.running = Some((generation, rx, job)),
        Err(e) => w.preview.error = Some(format!("could not start the preview: {e}")),
    }
}

/// The web build has no threads: the preview is computed when the pointer comes up.
#[cfg(target_arch = "wasm32")]
fn drive_preview(app: &mut PhotocraftApp, _ctx: &egui::Context, dragging: bool) {
    let stale = app.distort.caf.as_ref().is_some_and(|w| w.preview.shown != w.preview.want);
    if stale && !dragging {
        let _ = wait_preview(app);
    }
}

/// Pointer in document coordinates (from the view or the control channel).
pub fn pointer(app: &mut PhotocraftApp, ev: ToolEvent, mods: egui::Modifiers) {
    let Some(w) = app.distort.caf.as_mut() else { return };
    match (w.tool, ev) {
        (CafTool::Brush, ToolEvent::Down { x, y, .. }) => w.begin_stroke([x, y], mods.alt),
        (CafTool::Brush, ToolEvent::Move { x, y, .. }) => {
            w.hover = Some([x, y]);
            w.extend_stroke([x, y]);
        }
        (CafTool::Brush, ToolEvent::Up { x, y }) => {
            w.extend_stroke([x, y]);
            w.end_stroke();
        }
        (CafTool::Lasso, ToolEvent::Down { x, y, .. }) => {
            // Shift adds, Alt subtracts, both intersect (as Photoshop's selection tools).
            let op = match (mods.shift, mods.alt) {
                (true, true) => 3,
                (true, false) => 1,
                (false, true) => 2,
                _ => w.lasso_op,
            };
            w.lasso = Some((op, vec![[x, y]]));
        }
        (CafTool::Lasso, ToolEvent::Move { x, y, .. }) => {
            if let Some((_, pts)) = &mut w.lasso
                && pts.last().is_none_or(|l| l[0] != x || l[1] != y)
            {
                pts.push([x, y]);
            }
        }
        (CafTool::Lasso, ToolEvent::Up { .. }) => {
            if let Some((op, pts)) = w.lasso.take()
                && pts.len() >= 3
            {
                fill_edit(app, json!({"op": LASSO_OPS[op], "points": pts}));
            }
        }
        (CafTool::Zoom, ToolEvent::Down { x, y, .. }) => {
            let f = if mods.alt { 0.5 } else { 2.0 };
            w.zoom = (w.zoom * f).clamp(0.01, 32.0);
            w.center = [x, y];
        }
        _ => {}
    }
}

pub fn keys(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let cmd_shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
    if ctx.input_mut(|i| i.consume_key(cmd_shift, egui::Key::Z)) {
        redo(app);
        return;
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z)) {
        undo(app);
        return;
    }
    let Some(w) = app.distort.caf.as_mut() else { return };
    // Ctrl+H hides the overlay (Hide Extras).
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::H)) {
        w.opts.show_overlay = !w.opts.show_overlay;
        w.overlay_dirty = true;
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::OpenBracket)) {
        w.brush_size = (w.brush_size * 0.9).max(1.0).round();
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::CloseBracket)) {
        w.brush_size = (w.brush_size * 1.1).min(5000.0).round().max(w.brush_size + 1.0);
    }
    for (k, t) in [(egui::Key::B, CafTool::Brush), (egui::Key::L, CafTool::Lasso), (egui::Key::H, CafTool::Hand), (egui::Key::Z, CafTool::Zoom)] {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, k)) {
            w.tool = t;
        }
    }
}

fn tool_icon(t: CafTool) -> &'static str {
    match t {
        CafTool::Brush => "brush",
        CafTool::Lasso => "lasso",
        CafTool::Hand => "hand",
        CafTool::Zoom => "zoom-in",
    }
}

fn tool_tip(t: CafTool) -> String {
    match t {
        CafTool::Brush => format!("{} (B)", tl!("Sampling Brush Tool")),
        CafTool::Lasso => format!("{} (L)", tl!("Lasso Tool")),
        CafTool::Hand => format!("{} (H)", tl!("Hand Tool")),
        CafTool::Zoom => format!("{} (Z)", tl!("Zoom Tool")),
    }
}

/// A view: screen px per document px and the document point at the centre of `area`.
#[derive(Clone, Copy)]
struct View {
    area: ERect,
    zoom: f32,
    center: [f64; 2],
}

impl View {
    fn to_screen(self, p: [f64; 2]) -> Pos2 {
        let c = self.area.center();
        pos2(c.x + ((p[0] - self.center[0]) as f32) * self.zoom, c.y + ((p[1] - self.center[1]) as f32) * self.zoom)
    }
    fn to_doc(self, s: Pos2) -> [f64; 2] {
        let c = self.area.center();
        [self.center[0] + f64::from((s.x - c.x) / self.zoom), self.center[1] + f64::from((s.y - c.y) / self.zoom)]
    }
    fn rect(self, r: Rect) -> ERect {
        ERect::from_min_max(self.to_screen([f64::from(r.x0), f64::from(r.y0)]), self.to_screen([f64::from(r.x1), f64::from(r.y1)]))
    }
}

/// Upload what changed: the document proxy, the overlay, a finished preview.
fn upload(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let need_image = app.distort.caf.as_ref().is_some_and(|w| w.image_tex.is_none());
    if need_image {
        let img = view_image(app);
        if let (Some(w), Some(img)) = (app.distort.caf.as_mut(), img) {
            w.image_tex = Some(ctx.load_texture("caf-image", img, egui::TextureOptions::LINEAR));
        }
    }
    let Some(w) = app.distort.caf.as_mut() else { return };
    if w.overlay_dirty || w.overlay_tex.is_none() {
        w.overlay_dirty = false;
        let [r, g, b] = w.opts.color;
        let a = (w.opts.opacity / 100.0 * 255.0).round() as u8;
        let tint = Color32::from_rgba_unmultiplied(r, g, b, a);
        // The tint marks the sampling area (with Indicates: Excluded Area, what is left out);
        // the fill area is never tinted.
        let px: Vec<Color32> =
            w.mask.iter().zip(&w.hole_px).map(|(m, h)| if w.opts.show_overlay && !*h && *m != w.opts.excluded { tint } else { Color32::TRANSPARENT }).collect();
        let img = egui::ColorImage::new([w.pw, w.ph], px);
        match &mut w.overlay_tex {
            Some(t) => t.set(img, egui::TextureOptions::NEAREST),
            None => w.overlay_tex = Some(ctx.load_texture("caf-overlay", img, egui::TextureOptions::NEAREST)),
        }
    }
    if let Some((rect, img)) = w.preview.pending_image.take() {
        w.preview.tex = Some((ctx.load_texture("caf-preview", img, egui::TextureOptions::LINEAR), rect));
    }
}

/// The document as the view shows it: the visible layers composited, as on the canvas (so an
/// Apply to a new layer shows), reduced to the proxy.
fn view_image(app: &PhotocraftApp) -> Option<egui::ColorImage> {
    let w = app.distort.caf.as_ref()?;
    let st = app.session.active()?;
    let buf = photocraft_compose::render_reduced(&st.doc, w.pw as u32, w.ph as u32);
    let (display, _) = crate::canvas::canvas_display(app, &st.doc, None);
    Some(crate::canvas::display_image(display.as_deref(), &buf))
}

/// Draws the workspace (a full-window layer over the app).
pub fn show(app: &mut PhotocraftApp, ctx: &egui::Context) {
    // An Apply (or anything else) changed the document: refresh.
    let rev = app.session.active().map(|s| s.revision);
    if app.distort.caf.as_ref().is_some_and(|w| Some(w.revision) != rev) {
        refresh(app);
    }
    upload(app, ctx);
    let t = Tokens::get(ctx);
    let screen = ctx.content_rect();
    let mut action: Option<&str> = None;
    let mut events: Vec<ToolEvent> = Vec::new();
    let mut mods = egui::Modifiers::NONE;
    let mut new_sampling: Option<&str> = None;
    let mut fill_btn: Option<Value> = None;
    let mut reset_what: Option<&str> = None;
    let mut rebase_needed = false;
    let mut dragging = false;
    egui::Area::new(egui::Id::unique("caf-workspace")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
        let Some(w) = app.distort.caf.as_mut() else { return };
        let before = w.opts.clone();
        w.texture_limit = ctx.input(|i| i.max_texture_side);
        let (full, _) = ui.allocate_exact_size(screen.size(), Sense::hover());
        let painter = ui.painter().clone();
        painter.rect_filled(full, 0.0, t.chrome);
        // Title strip.
        let title = ERect::from_min_size(full.min, vec2(full.width(), 30.0));
        painter.rect_filled(title, 0.0, t.dock);
        painter.line_segment([title.left_bottom(), title.right_bottom()], Stroke::new(1.0, t.separator));
        let pct = if w.zoom > 0.0 { w.zoom * 100.0 } else { 100.0 };
        painter.text(
            title.center(),
            Align2::CENTER_CENTER,
            format!("{} ({}, {:.0}%)", tl!("Content-Aware Fill…").trim_end_matches('…'), w.layer_name, pct),
            FontId::proportional(13.0),
            t.text,
        );
        // Options bar.
        let bar = ERect::from_min_size(pos2(full.left(), title.bottom()), vec2(full.width(), BAR_H));
        painter.rect_filled(bar, 0.0, t.dock);
        painter.line_segment([bar.left_bottom(), bar.right_bottom()], Stroke::new(1.0, t.separator));
        let mut bui =
            ui.new_child(egui::UiBuilder::new().max_rect(bar.shrink2(vec2(LEFT_W + 8.0, 4.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        match w.tool {
            CafTool::Brush => {
                if crate::icons::button(&mut bui, "plus", 26.0, w.brush_add, tl!("Add")).clicked() {
                    w.brush_add = true;
                }
                if crate::icons::button(&mut bui, "minus", 26.0, !w.brush_add, tl!("Subtract")).clicked() {
                    w.brush_add = false;
                }
                bui.add_space(8.0);
                bui.label(tl!("Size:"));
                let mut size = w.brush_size as f32;
                if widgets::value_field(&mut bui, &mut size, 1.0..=5000.0, " px", 64.0).accessible_name(tl!("Size:")).changed() {
                    w.brush_size = f64::from(size);
                }
            }
            CafTool::Lasso => {
                // The selection tools' mode buttons (the lasso here edits the fill area).
                let tips = [
                    tl!("New selection").to_string(),
                    crate::i18n::fmt(tl!("Add to selection  ({key})"), &[("key", &crate::shortcuts::pretty("Shift"))]),
                    crate::i18n::fmt(tl!("Subtract from selection  ({key})"), &[("key", &crate::shortcuts::pretty("Alt"))]),
                    crate::i18n::fmt(tl!("Intersect with selection  ({key})"), &[("key", &crate::shortcuts::pretty("Shift+Alt"))]),
                ];
                for (i, (icon, tip)) in ["square", "plus", "minus", "squares-subtract"].into_iter().zip(tips.iter()).enumerate() {
                    if crate::icons::button(&mut bui, icon, 26.0, w.lasso_op == i, tip).clicked() {
                        w.lasso_op = i;
                    }
                }
                bui.add_space(8.0);
                if widgets::secondary_button(&mut bui, tl!("Expand"), 70.0).clicked() {
                    fill_btn = Some(json!({"op": "expand", "by": w.expand_by}));
                }
                if widgets::secondary_button(&mut bui, tl!("Contract"), 70.0).clicked() {
                    fill_btn = Some(json!({"op": "contract", "by": w.expand_by}));
                }
                widgets::value_field(&mut bui, &mut w.expand_by, 1.0..=100.0, " px", 56.0).accessible_name(tl!("Expand"));
                if crate::icons::button(&mut bui, "undo-2", 26.0, false, tl!("Reset")).clicked() {
                    reset_what = Some("lasso");
                }
            }
            _ => {}
        }
        let body = ERect::from_min_max(pos2(full.left(), bar.bottom()), full.max);
        // Left tool strip.
        let left = ERect::from_min_size(pos2(full.left(), title.bottom()), vec2(LEFT_W, full.height() - title.height()));
        painter.rect_filled(left, 0.0, t.dock);
        painter.line_segment([left.right_top(), left.right_bottom()], Stroke::new(1.0, t.separator));
        let mut strip = ui.new_child(egui::UiBuilder::new().max_rect(left.shrink2(vec2(6.0, 8.0))));
        strip.spacing_mut().item_spacing.y = 4.0;
        for tool in [CafTool::Brush, CafTool::Lasso, CafTool::Hand, CafTool::Zoom] {
            if crate::icons::button(&mut strip, tool_icon(tool), 34.0, w.tool == tool, &tool_tip(tool)).clicked() {
                w.tool = tool;
            }
        }
        // Right: the Content-Aware Fill panel.
        let right = ERect::from_min_size(pos2(body.right() - PANEL_W, body.top()), vec2(PANEL_W, body.height()));
        painter.rect_filled(right, 0.0, t.dock);
        painter.line_segment([right.left_top(), right.left_bottom()], Stroke::new(1.0, t.separator));
        let mut props =
            ui.new_child(egui::UiBuilder::new().max_rect(ERect::from_min_max(right.min + vec2(14.0, 12.0), pos2(right.right() - 14.0, right.bottom() - 60.0))));
        egui::ScrollArea::vertical().id_salt("caf-props").show(&mut props, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            egui::CollapsingHeader::new(tl!("Sampling Area Overlay")).id_salt("caf-overlay").default_open(false).show(ui, |ui| {
                widgets::checkbox(ui, &mut w.opts.show_overlay, tl!("Show Sampling Area"));
                widgets::slider_row(ui, tl!("Opacity"), &mut w.opts.opacity, 0.0..=100.0, "%", None);
                ui.horizontal(|ui| {
                    ui.label(tl!("Color"));
                    widgets::color_edit_button_srgb(ui, &mut w.opts.color);
                    ui.add_space(8.0);
                    ui.label(tl!("Indicates"));
                    widgets::dropdown(
                        ui,
                        "caf-indicates",
                        tl!("Indicates"),
                        &mut w.opts.excluded,
                        &[(false, tl!("Sampling Area")), (true, tl!("Excluded Area"))],
                        120.0,
                    );
                });
            });
            widgets::hairline(ui);
            ui.horizontal(|ui| {
                widgets::section_label(ui, tl!("Sampling Area Options"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if crate::icons::button(ui, "undo-2", 22.0, false, tl!("Reset")).clicked() {
                        reset_what = Some("sampling");
                    }
                });
            });
            ui.horizontal(|ui| {
                for (k, l) in [("auto", tl!("Auto")), ("rectangular", tl!("Rectangular")), ("custom", tl!("Custom"))] {
                    if ui.selectable_label(w.opts.sampling == k, l).clicked() && w.opts.sampling != k {
                        new_sampling = Some(k);
                    }
                }
            });
            if widgets::checkbox(ui, &mut w.opts.sample_all, tl!("Sample All Layers")).changed() {
                rebase_needed = true;
            }
            widgets::hairline(ui);
            ui.horizontal(|ui| {
                widgets::section_label(ui, tl!("Fill Settings"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if crate::icons::button(ui, "undo-2", 22.0, false, tl!("Reset")).clicked() {
                        reset_what = Some("fill");
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label(tl!("Color Adaptation"));
                let o = [
                    ("none".to_string(), tl!("None")),
                    ("default".to_string(), tl!("Default")),
                    ("high".to_string(), tl!("High")),
                    ("veryHigh".to_string(), tl!("Very High")),
                ];
                widgets::dropdown(ui, "caf-color", tl!("Color Adaptation"), &mut w.opts.color_adaptation, &o, 110.0);
            });
            ui.horizontal(|ui| {
                ui.label(tl!("Rotation Adaptation"));
                let o = [
                    ("none".to_string(), tl!("None")),
                    ("low".to_string(), tl!("Low")),
                    ("medium".to_string(), tl!("Medium")),
                    ("high".to_string(), tl!("High")),
                    ("full".to_string(), tl!("Full")),
                ];
                widgets::dropdown(ui, "caf-rotation", tl!("Rotation Adaptation"), &mut w.opts.rotation, &o, 110.0);
            });
            ui.horizontal(|ui| {
                widgets::checkbox(ui, &mut w.opts.scale, tl!("Scale"));
                widgets::checkbox(ui, &mut w.opts.mirror, tl!("Mirror"));
            });
            widgets::hairline(ui);
            widgets::section_label(ui, tl!("Output Settings"));
            ui.horizontal(|ui| {
                ui.label(tl!("Output To"));
                let o =
                    [("current".to_string(), tl!("Current Layer")), ("new".to_string(), tl!("New Layer")), ("duplicate".to_string(), tl!("Duplicate Layer"))];
                widgets::dropdown(ui, "caf-output", tl!("Output To"), &mut w.opts.output, &o, 140.0);
            });
            if let Some(e) = w.error.as_ref().or(w.preview.error.as_ref()) {
                ui.label(egui::RichText::new(e.as_str()).color(t.danger).size(11.0));
            }
        });
        // Footer, in Photoshop's order on every platform: reset all at the left, then OK, Apply,
        // Cancel against the right edge.
        let foot = ERect::from_min_size(pos2(right.left() + 10.0, right.bottom() - 48.0), vec2(PANEL_W - 20.0, 32.0));
        let mut fb = ui.new_child(egui::UiBuilder::new().max_rect(foot).layout(egui::Layout::right_to_left(egui::Align::Center)));
        if widgets::secondary_button(&mut fb, tl!("Cancel"), 70.0).clicked() {
            action = Some("cancel");
        }
        if widgets::secondary_button(&mut fb, tl!("Apply"), 70.0).clicked() {
            action = Some("apply");
        }
        if widgets::primary_button(&mut fb, tl!("OK"), 64.0).clicked() {
            action = Some("ok");
        }
        if crate::icons::button(&mut fb, "undo-2", 24.0, false, tl!("Reset")).clicked() {
            reset_what = Some("all");
        }
        // The Preview panel.
        let pv_rect = ERect::from_min_size(pos2(right.left() - PREVIEW_W, body.top()), vec2(PREVIEW_W, body.height()));
        painter.rect_filled(pv_rect, 0.0, t.dock);
        painter.line_segment([pv_rect.left_top(), pv_rect.left_bottom()], Stroke::new(1.0, t.separator));
        painter.text(pv_rect.left_top() + vec2(12.0, 10.0), Align2::LEFT_TOP, tl!("Preview"), FontId::proportional(12.0), t.text);
        let pv_area = ERect::from_min_max(pv_rect.min + vec2(0.0, 30.0), pv_rect.max - vec2(0.0, 44.0));
        painter.rect_filled(pv_area, 0.0, t.canvas);
        if w.preview.zoom <= 0.0 {
            // Opens framed on the sampling window, as Photoshop's.
            let win = w.base.window;
            if !win.is_empty() {
                w.preview.zoom = (pv_area.width() / win.width().max(win.height()) as f32).clamp(0.01, 32.0);
                w.preview.center = [f64::from(win.x0 + win.x1) / 2.0, f64::from(win.y0 + win.y1) / 2.0];
            }
        }
        let pv = View { area: pv_area, zoom: w.preview.zoom.max(0.01), center: w.preview.center };
        let pclip = painter.with_clip_rect(pv_area);
        let uv = ERect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        let img_rect = pv.rect(w.canvas);
        widgets::checker(&pclip, img_rect.intersect(pv_area), 8.0);
        if let Some(tex) = &w.image_tex {
            pclip.image(tex.id(), img_rect, uv, Color32::WHITE);
        }
        if let Some((tex, r)) = &w.preview.tex {
            pclip.image(tex.id(), pv.rect(*r), uv, Color32::WHITE);
        }
        if w.preview.shown != w.preview.want {
            // Being recomputed: a spinner in the corner.
            let sp = ERect::from_min_size(pv_area.right_bottom() - vec2(26.0, 26.0), vec2(18.0, 18.0));
            ui.put(sp, egui::Spinner::new().size(16.0));
        }
        let presp = ui.interact(pv_area, egui::Id::unique("caf-preview-view"), Sense::click_and_drag());
        if presp.dragged() {
            let d = presp.drag_delta();
            w.preview.center = [w.preview.center[0] - f64::from(d.x / pv.zoom), w.preview.center[1] - f64::from(d.y / pv.zoom)];
        }
        if presp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                w.preview.zoom = (w.preview.zoom * (scroll / 200.0).exp()).clamp(0.01, 32.0);
            }
        }
        let zrow = ERect::from_min_size(pos2(pv_rect.left() + 10.0, pv_rect.bottom() - 38.0), vec2(PREVIEW_W - 20.0, 30.0));
        let mut zr = ui.new_child(egui::UiBuilder::new().max_rect(zrow).layout(egui::Layout::left_to_right(egui::Align::Center)));
        let mut pct = (w.preview.zoom * 100.0).round();
        if widgets::value_field(&mut zr, &mut pct, 1.0..=3200.0, "%", 64.0).accessible_name(tl!("Zoom")).changed() {
            w.preview.zoom = pct / 100.0;
        }
        let mut lz = pct.max(1.0).ln();
        if zr.add(egui::Slider::new(&mut lz, 0.0..=3200f32.ln()).show_value(false)).accessible_name(tl!("Zoom")).changed() {
            w.preview.zoom = lz.exp() / 100.0;
        }
        // The document view.
        let area = ERect::from_min_max(pos2(left.right(), body.top()), pos2(pv_rect.left(), body.bottom()));
        painter.rect_filled(area, 0.0, t.canvas);
        let (cw, chh) = (w.canvas.width() as f32, w.canvas.height() as f32);
        if w.zoom <= 0.0 {
            w.zoom = ((area.width() - 40.0) / cw).min((area.height() - 40.0) / chh).clamp(0.01, 4.0);
        }
        let resp = ui.interact(area, egui::Id::unique("caf-view"), Sense::click_and_drag());
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0
                && let Some(hp) = resp.hover_pos()
            {
                let v = View { area, zoom: w.zoom, center: w.center };
                let before = v.to_doc(hp);
                w.zoom = (w.zoom * (scroll / 200.0).exp()).clamp(0.01, 32.0);
                let after = View { area, zoom: w.zoom, center: w.center }.to_doc(hp);
                w.center = [w.center[0] + before[0] - after[0], w.center[1] + before[1] - after[1]];
            }
        }
        let v = View { area, zoom: w.zoom, center: w.center };
        let clip = painter.with_clip_rect(area);
        let img = v.rect(w.canvas);
        widgets::checker(&clip, img.intersect(area), 8.0);
        if let Some(tex) = &w.image_tex {
            clip.image(tex.id(), img, uv, Color32::WHITE);
        }
        if let Some(tex) = &w.overlay_tex {
            // The proxy's blocks are `k` document px; its last row and column may reach past the
            // canvas, so the overlay is clipped to it.
            let pr = ERect::from_min_size(img.min, vec2((w.pw * w.k) as f32 * v.zoom, (w.ph * w.k) as f32 * v.zoom));
            clip.with_clip_rect(img.intersect(area)).image(tex.id(), pr, uv, Color32::WHITE);
        }
        // The fill area's outline (marching ants: black dashes on white).
        let phase = (ui.input(|i| i.time) * 8.0) as usize;
        match &w.outline {
            Some(edges) => {
                for e in edges {
                    let (a, b) = (v.to_screen([f64::from(e[0][0]), f64::from(e[0][1])]), v.to_screen([f64::from(e[1][0]), f64::from(e[1][1])]));
                    let dark = ((e[0][0] + e[0][1]) as usize / 4 + phase).is_multiple_of(2);
                    clip.line_segment([a, b], Stroke::new(1.0, if dark { Color32::BLACK } else { Color32::WHITE }));
                }
            }
            None => {
                let b = v.rect(bounds(&w.base));
                clip.rect_stroke(b, 0.0, Stroke::new(1.0, Color32::WHITE), egui::StrokeKind::Outside);
            }
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(120));
        if let Some((_, pts)) = &w.lasso
            && pts.len() > 1
        {
            let line: Vec<Pos2> = pts.iter().map(|p| v.to_screen(*p)).collect();
            clip.add(egui::Shape::line(line, Stroke::new(1.0, t.accent)));
        }
        if let Some(hp) = resp.hover_pos() {
            if w.tool == CafTool::Brush {
                let r = w.brush_size as f32 * 0.5 * v.zoom;
                clip.circle_stroke(hp, r, Stroke::new(1.5, Color32::BLACK));
                clip.circle_stroke(hp, r, Stroke::new(0.75, Color32::WHITE));
            }
            w.hover = Some(v.to_doc(hp));
        }
        clip.rect_stroke(img, 0.0, Stroke::new(1.0, t.separator), egui::StrokeKind::Outside);
        // Mouse → tools (Space, the middle button or the Hand pan).
        let pan = ui.input(|i| i.key_down(egui::Key::Space) || i.pointer.middle_down()) || w.tool == CafTool::Hand;
        if pan && resp.dragged() {
            let dl = resp.drag_delta();
            w.center = [w.center[0] - f64::from(dl.x / w.zoom), w.center[1] - f64::from(dl.y / w.zoom)];
        } else if !pan
            && resp.is_pointer_button_down_on()
            && let Some(pp) = resp.interact_pointer_pos()
        {
            // The press is the first point; the frames before egui's drag threshold and the drag
            // itself continue it (a second `Down` would start the stroke over, dropping them).
            let q = v.to_doc(pp);
            mods = ui.input(|i| i.modifiers);
            if w.pressed {
                events.push(ToolEvent::Move { x: q[0], y: q[1], pressure: 1.0 });
                ctx.request_repaint();
            } else {
                w.pressed = true;
                events.push(ToolEvent::Down { x: q[0], y: q[1], pressure: 1.0 });
            }
            dragging = true;
        }
        if w.pressed && !resp.is_pointer_button_down_on() {
            w.pressed = false;
            let q = w.hover.unwrap_or(w.center);
            events.push(ToolEvent::Up { x: q[0], y: q[1] });
            dragging = false;
        }
        // Settings changed in the panel: the overlay and the preview follow.
        if w.opts != before {
            w.overlay_dirty = true;
            w.stale();
        }
    });
    if let Some(s) = new_sampling {
        set_sampling(app, s);
    }
    if let Some(r) = reset_what {
        reset(app, r);
    }
    if let Some(e) = fill_btn {
        fill_edit(app, e);
    }
    if rebase_needed {
        rebase(app);
    }
    for ev in events {
        pointer(app, ev, mods);
    }
    drive_preview(app, ctx, dragging);
    match action {
        Some("ok") => commit(app),
        Some("cancel") => cancel(app),
        Some("apply") => {
            if let Err(e) = apply(app) {
                report(app, e);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
