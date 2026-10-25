//! Why a view left the GPU (#9328): one typed reason per document, recorded at **one** entry
//! point ([`note`]) by every exit from the GPU path, logged once when it changes, kept in a short
//! history, shown in the status bar (`chrome_ui::render_path_status`) and returned by the control
//! channel (`perf.renderPath`). `perf.gpuFallback` mirrors the current reason as text.
//!
//! Three slots, each written only by its own events: the canvas's reason per document ([`note`]),
//! a reason for the whole window ([`note_app`]) and an adjustment dialog's CPU proxy preview
//! ([`adjust_preview_cpu`] / [`adjust_preview_ok`]). [`RenderPath::current`] reads them in that
//! order of precedence: the window, then the canvas, then the preview.

use std::collections::{HashMap, VecDeque};
use std::fmt;

use serde_json::{Value, json};

use crate::PhotocraftApp;

/// Changes the history keeps.
pub const HISTORY: usize = 32;

/// Why a view is drawn by the CPU compositor or on the CPU canvas.
#[derive(Clone, Debug, PartialEq)]
pub enum CpuReason {
    /// The layer pages a full (or large) refresh needs don't fit the GPU memory budget (bytes).
    OverBudget { needed: u64, budget: u64 },
    /// The display textures don't fit the GPU texture budget (bytes).
    CanvasTooLarge { bytes: u64, budget: u64 },
    /// The driver couldn't build the compositor's pipelines (such as DX12 FXC X3507, #8733).
    CompositorBuild(String),
    /// The device was lost (the fault's text).
    DeviceLost(String),
    /// An uncaptured GPU error (the fault's text).
    GpuError(String),
    /// What the wgpu compositor can't express (Advanced Blending, CMYK, effects…), its own text.
    Unsupported(String),
    /// An adjustment dialog previews through the CPU proxy (#7487).
    AdjustPreview(&'static str),
    /// A flipped or pixel-aspect-corrected view: the GPU canvas shader has neither.
    ViewTransform,
    /// The window's adapter is a software renderer.
    SoftwareAdapter,
    /// Startup chose CPU rendering (the preference, `--safe-gpu`, an earlier crash…).
    CpuMode(String),
}

impl CpuReason {
    /// The reason from a device fault.
    pub fn from_fault(f: &photocraft_gpu::Fault) -> Self {
        if f.is_lost() { CpuReason::DeviceLost(f.to_string()) } else { CpuReason::GpuError(f.to_string()) }
    }

    /// A stable id of the kind of reason (the control channel's `kind`): reasons compare by it,
    /// not by their numbers or text.
    pub fn kind(&self) -> &'static str {
        match self {
            CpuReason::OverBudget { .. } => "overBudget",
            CpuReason::CanvasTooLarge { .. } => "canvasTooLarge",
            CpuReason::CompositorBuild(_) => "compositorBuild",
            CpuReason::DeviceLost(_) => "deviceLost",
            CpuReason::GpuError(_) => "gpuError",
            CpuReason::Unsupported(_) => "unsupported",
            CpuReason::AdjustPreview(_) => "adjustPreview",
            CpuReason::ViewTransform => "viewTransform",
            CpuReason::SoftwareAdapter => "softwareAdapter",
            CpuReason::CpuMode(_) => "cpuMode",
        }
    }

    /// Reasons that apply to full refreshes only: a small GPU edit doesn't end them.
    fn full_refresh_only(&self) -> bool {
        matches!(self, CpuReason::OverBudget { .. } | CpuReason::CanvasTooLarge { .. })
    }
}

impl fmt::Display for CpuReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use crate::chrome_ui::fmt_bytes;
        match self {
            CpuReason::OverBudget { needed, budget } => {
                write!(f, "layers need {} of GPU memory, budget {}; full refresh on the CPU", fmt_bytes(*needed), fmt_bytes(*budget))
            }
            CpuReason::CanvasTooLarge { bytes, budget } => {
                write!(f, "the canvas needs {} of GPU textures, budget {}; drawing it on the CPU canvas", fmt_bytes(*bytes), fmt_bytes(*budget))
            }
            CpuReason::CompositorBuild(e) => write!(f, "the GPU compositor couldn't be built: {e}"),
            CpuReason::DeviceLost(e) | CpuReason::GpuError(e) | CpuReason::Unsupported(e) => f.write_str(e),
            CpuReason::AdjustPreview(why) => write!(f, "the adjustment preview uses the CPU proxy: {why}"),
            CpuReason::ViewTransform => f.write_str("a flipped or pixel-aspect-corrected view is drawn on the CPU"),
            CpuReason::SoftwareAdapter => f.write_str("the graphics adapter is a software renderer"),
            CpuReason::CpuMode(why) => write!(f, "CPU rendering: {why}"),
        }
    }
}

/// What a refresh or view did.
#[derive(Clone, Debug, PartialEq)]
pub enum Path {
    /// The wgpu compositor drew it (`full`: the whole document).
    Gpu {
        full: bool,
    },
    Cpu(CpuReason),
}

/// One change of a document's (or, with `doc` 0, the app's) reason.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub t_ms: f64,
    pub doc: u64,
    pub reason: Option<CpuReason>,
    /// A change of the adjustment preview's slot: `reason` None means the preview stopped using
    /// the CPU proxy, not that the canvas is back on the GPU.
    pub preview: bool,
}

/// The current reasons and their history.
#[derive(Clone, Debug, Default)]
pub struct RenderPath {
    docs: HashMap<u64, CpuReason>,
    /// A reason for every document: the GPU canvas was dropped, or never started.
    app_wide: Option<CpuReason>,
    /// The open adjustment dialog's CPU proxy preview: (doc, [`CpuReason::AdjustPreview`]). Only
    /// one dialog is open at a time.
    preview: Option<(u64, CpuReason)>,
    startup_checked: bool,
    history: VecDeque<Change>,
    /// Tests without a GPU: act as if the window had a GPU canvas.
    #[cfg(test)]
    pub(crate) assume_gpu: bool,
}

impl RenderPath {
    /// Why document `doc` is drawn on the CPU now, or None on the GPU: the window's reason, else
    /// the canvas's, else the adjustment preview's.
    pub fn current(&self, doc: u64) -> Option<&CpuReason> {
        self.app_wide.as_ref().or_else(|| self.docs.get(&doc)).or_else(|| self.preview.as_ref().filter(|(d, _)| *d == doc).map(|(_, r)| r))
    }

    /// The changes, oldest first.
    pub fn history(&self) -> impl Iterator<Item = &Change> {
        self.history.iter()
    }

    fn push(&mut self, doc: u64, reason: Option<CpuReason>, preview: bool) {
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(Change { t_ms: crate::gpu_canvas::now_ms(), doc, reason, preview });
    }
}

fn doc_name(app: &PhotocraftApp, doc: u64) -> String {
    app.session.documents().iter().find(|st| st.doc.id.0 == doc).map_or_else(|| format!("document {doc}"), |st| st.doc.name.clone())
}

fn log_change(who: &str, reason: Option<&CpuReason>) {
    match reason {
        Some(r) => log::info!("render path: {who}: CPU ({}: {r})", r.kind()),
        None => log::info!("render path: {who}: back on the GPU"),
    }
}

/// Record what drew document `doc`: the one entry point of every exit from the GPU path. A new
/// kind of reason (not new numbers) is logged once and kept in the history; a partial GPU refresh
/// leaves a reason that applies to full refreshes only.
pub fn note(app: &mut PhotocraftApp, doc: u64, path: Path) {
    let rp = &mut app.render_path;
    let prev = rp.docs.get(&doc);
    let next = match path {
        Path::Gpu { full: false } if prev.is_some_and(CpuReason::full_refresh_only) => return,
        Path::Gpu { .. } => None,
        Path::Cpu(r) => Some(r),
    };
    let changed = prev.map(CpuReason::kind) != next.as_ref().map(CpuReason::kind);
    match &next {
        Some(r) => rp.docs.insert(doc, r.clone()),
        None => rp.docs.remove(&doc),
    };
    app.perf.gpu_fallback = app.render_path.current(doc).map(ToString::to_string);
    if changed {
        log_change(&doc_name(app, doc), next.as_ref());
        app.render_path.push(doc, next, false);
    }
}

/// Record a reason for every document (the GPU canvas was dropped or never started), or its end.
pub fn note_app(app: &mut PhotocraftApp, reason: Option<CpuReason>) {
    let rp = &mut app.render_path;
    let changed = rp.app_wide.as_ref().map(CpuReason::kind) != reason.as_ref().map(CpuReason::kind);
    rp.app_wide = reason.clone();
    app.perf.gpu_fallback = reason.as_ref().map(ToString::to_string);
    if changed {
        log_change("all documents", reason.as_ref());
        app.render_path.push(0, reason, false);
    }
}

/// A view that may draw on the GPU again: end a [`CpuReason::ViewTransform`] reason.
pub fn view_ok(app: &mut PhotocraftApp, doc: u64) {
    if app.render_path.docs.get(&doc) == Some(&CpuReason::ViewTransform) {
        note(app, doc, Path::Gpu { full: true });
    }
}

/// Whether the window composites on the GPU at all: no reason for every document, and a GPU canvas.
fn window_on_gpu(app: &PhotocraftApp) -> bool {
    #[cfg(test)]
    let gpu = app.gpu.is_some() || app.render_path.assume_gpu;
    #[cfg(not(test))]
    let gpu = app.gpu.is_some();
    gpu && app.render_path.app_wide.is_none()
}

/// An adjustment dialog on document `doc` previews through the CPU proxy for `why`. It has its
/// own slot, so it never replaces or ends the canvas's or the window's reason. Not recorded when
/// the whole window is off the GPU: the proxy is then simply part of a CPU window.
pub fn adjust_preview_cpu(app: &mut PhotocraftApp, doc: u64, why: &'static str) {
    if !window_on_gpu(app) {
        adjust_preview_ok(app);
        return;
    }
    let reason = CpuReason::AdjustPreview(why);
    let changed = app.render_path.preview.as_ref().is_none_or(|(d, _)| *d != doc);
    app.render_path.preview = Some((doc, reason.clone()));
    app.perf.gpu_fallback = app.render_path.current(doc).map(ToString::to_string);
    if changed {
        log::info!("render path: {}: adjustment preview uses the CPU proxy: {why}", doc_name(app, doc));
        app.render_path.push(doc, Some(reason), true);
    }
}

/// The adjustment dialog closed, or retargeted to a layer the layer path can preview: end its
/// CPU proxy preview, if one was recorded. The canvas's and the window's reasons are untouched.
pub fn adjust_preview_ok(app: &mut PhotocraftApp) {
    let Some((doc, _)) = app.render_path.preview.take() else { return };
    app.perf.gpu_fallback = app.render_path.current(doc).map(ToString::to_string);
    log::info!("render path: {}: adjustment preview no longer uses the CPU proxy", doc_name(app, doc));
    app.render_path.push(doc, None, true);
}

/// Once, when the window has no GPU canvas: a software adapter or CPU mode chosen at startup.
pub fn frame(app: &mut PhotocraftApp) {
    if app.render_path.startup_checked || app.gpu.is_some() || app.gpu_fault.is_some() {
        return;
    }
    let info = &app.perf.gpu_info;
    let reason = if info.device_type == "cpu" {
        CpuReason::SoftwareAdapter
    } else if info.canvas == "cpu" {
        CpuReason::CpuMode(info.fallback.clone().unwrap_or_else(|| format!("performance.gpuBackend is {}", info.preference)))
    } else {
        // The adapter isn't known yet (or this host has none to report).
        return;
    };
    app.render_path.startup_checked = true;
    note_app(app, Some(reason));
}

fn reason_json(doc: u64, r: Option<&CpuReason>) -> Value {
    json!({"doc": doc, "kind": r.map(CpuReason::kind), "detail": r.map(ToString::to_string)})
}

/// A history entry: a preview entry's `kind` is always `adjustPreview` (`detail` null when the
/// preview stopped using the CPU proxy), so its end never reads as the canvas back on the GPU.
fn change_json(c: &Change) -> Value {
    let mut v = reason_json(c.doc, c.reason.as_ref());
    if c.preview {
        v["kind"] = json!("adjustPreview");
    }
    v["t"] = json!(c.t_ms);
    v
}

/// The control channel's `perf.renderPath`: the active document's reason (`null` on the GPU) and
/// the history, oldest first.
pub fn inspect(app: &PhotocraftApp) -> Value {
    let active = app.session.active().map(|st| st.doc.id.0);
    let current = active.and_then(|d| app.render_path.current(d).map(|r| reason_json(d, Some(r))));
    let history: Vec<Value> = app.render_path.history().map(change_json).collect();
    json!({"current": current, "history": history})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> PhotocraftApp {
        PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default())
    }

    fn over(needed: u64) -> Path {
        Path::Cpu(CpuReason::OverBudget { needed, budget: 2 << 30 })
    }

    #[test]
    fn reasons_compare_by_kind_not_numbers() {
        let mut app = app();
        note(&mut app, 1, over(3 << 30));
        note(&mut app, 1, over(4 << 30));
        assert_eq!(app.render_path.history().count(), 1);
        assert_eq!(app.render_path.current(1), Some(&CpuReason::OverBudget { needed: 4 << 30, budget: 2 << 30 }), "the numbers update");
        assert!(app.perf.gpu_fallback.as_deref().is_some_and(|f| f.contains("4.0G")), "{:?}", app.perf.gpu_fallback);
        note(&mut app, 1, Path::Cpu(CpuReason::Unsupported("Advanced Blending on `A`".into())));
        assert_eq!(app.render_path.history().count(), 2, "another kind is a change");
    }

    #[test]
    fn a_partial_gpu_refresh_keeps_an_over_budget_reason() {
        let mut app = app();
        note(&mut app, 1, over(3 << 30));
        note(&mut app, 1, Path::Gpu { full: false });
        assert!(app.render_path.current(1).is_some(), "a brush dab doesn't end it");
        assert!(app.perf.gpu_fallback.is_some());
        note(&mut app, 1, Path::Gpu { full: true });
        assert!(app.render_path.current(1).is_none());
        assert!(app.perf.gpu_fallback.is_none());
        assert_eq!(app.render_path.history().count(), 2);
        // Other reasons end at any GPU refresh.
        note(&mut app, 1, Path::Cpu(CpuReason::ViewTransform));
        note(&mut app, 1, Path::Gpu { full: false });
        assert!(app.render_path.current(1).is_none());
    }

    #[test]
    fn the_fallback_log_does_not_repeat_between_partial_refreshes() {
        // Over budget, a dab, a full refresh, a dab, a full refresh: one change, not five.
        let mut app = app();
        note(&mut app, 1, over(3 << 30));
        note(&mut app, 1, Path::Gpu { full: false });
        note(&mut app, 1, over(3 << 30));
        note(&mut app, 1, Path::Gpu { full: false });
        note(&mut app, 1, over(3 << 30));
        assert_eq!(app.render_path.history().count(), 1);
    }

    #[test]
    fn history_keeps_the_last_32_changes() {
        let mut app = app();
        for i in 0..40 {
            note(&mut app, i, Path::Cpu(CpuReason::ViewTransform));
        }
        let docs: Vec<u64> = app.render_path.history().map(|c| c.doc).collect();
        assert_eq!(docs, (8..40).collect::<Vec<_>>());
    }

    #[test]
    fn over_budget_reason_names_the_working_set_and_budget() {
        let text = CpuReason::OverBudget { needed: 3600 << 20, budget: 512 << 20 }.to_string();
        assert!(text.contains("3.5G") && text.contains("512.0M"), "{text}");
        let text = CpuReason::CanvasTooLarge { bytes: 3 << 30, budget: 1 << 30 }.to_string();
        assert!(text.contains("3.0G") && text.contains("1.0G"), "{text}");
    }

    #[test]
    fn a_software_adapter_is_the_reason_at_startup() {
        let mut app = app();
        assert!(app.gpu.is_none());
        frame(&mut app);
        assert!(app.render_path.current(1).is_none(), "no adapter known yet");
        app.perf.gpu_info.adapter = "llvmpipe".into();
        app.perf.gpu_info.device_type = "cpu".into();
        frame(&mut app);
        assert_eq!(app.render_path.current(1), Some(&CpuReason::SoftwareAdapter));
        frame(&mut app);
        assert_eq!(app.render_path.history().count(), 1, "once");

        let mut app = self::app();
        app.perf.gpu_info.device_type = "integratedGpu".into();
        app.perf.gpu_info.canvas = "cpu".into();
        app.perf.gpu_info.fallback = Some("--safe-gpu".into());
        frame(&mut app);
        assert_eq!(app.render_path.current(7), Some(&CpuReason::CpuMode("--safe-gpu".into())));
        assert_eq!(inspect(&app)["history"][0]["kind"], "cpuMode");
    }
}
