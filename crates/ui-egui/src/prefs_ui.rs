//! Edit › Preferences, Keyboard Shortcuts and Menus, Color Settings and the rest of the Edit
//! menu's dialogs, plus the shell side of preferences: loading and saving them through
//! [`crate::Services`], applying the interface theme, autosave and crash recovery, and the
//! history log.
//!
//! The values live in the engine ([`photocraft_engine::prefs::Preferences`], so agents read and
//! change them with `prefs.get` / `prefs.set`); this module only edits a working copy in a dialog
//! and commits it with those commands on Apply or OK.

use std::collections::{BTreeMap, HashMap, VecDeque};

use egui::{Color32, RichText, Sense, vec2};
use photocraft_doc::DocId;
use photocraft_engine::jobs::{JobEvent, JobId, JobOutcome, Started};
#[cfg(test)]
use photocraft_engine::prefs::Theme;
use photocraft_engine::prefs::{self, AppearanceMode, DarkTheme, LightTheme, SECTIONS, Theme as SavedTheme};
use photocraft_engine::snap::{SnapLine, SnapOptions, SnapTargets};
use serde_json::{Map, Value, json};

use crate::PhotocraftApp;
use crate::state::DialogKind;
use crate::theme::{ThemeKind, Tokens};

/// Shell runtime state for preferences, autosave and snapping (not serialised).
#[derive(Default)]
pub struct Runtime {
    loaded: bool,
    /// Preferences revision last written to storage (or loaded from it).
    saved_rev: u64,
    /// This instance's preferences as of `saved_rev`: the base of the three-way merge on save.
    saved_value: Option<Value>,
    save_retry: SaveRetry,
    next_autosave_ms: f64,
    /// Last revision whose recovery file was actually written successfully.
    autosaved: HashMap<DocId, u64>,
    /// Queued writes must not masquerade as durable snapshots.
    autosave_pending: HashMap<DocId, u64>,
    recovery_started: bool,
    recovery_queue: VecDeque<crate::Recoverable>,
    recovery_pending: Option<(JobId, String)>,
    log_len: usize,
    /// Snapping state of the drag in progress (see `snap_ui`).
    pub(crate) snap: Option<crate::snap_ui::ActiveSnap>,
    pub(crate) snap_lines: Vec<SnapLine>,
    pub(crate) guide_targets: Option<((DocId, u64, SnapOptions), SnapTargets)>,
    /// Checkerboard colours the cached CPU checker texture was built with.
    pub(crate) checker_key: Option<[[u8; 3]; 2]>,
    /// Style last sent to the GPU canvas.
    pub(crate) gpu_style: Option<crate::gpu_canvas::CanvasStyle>,
    /// `unitsAndRulers.showRulers` as last applied to View › Rulers.
    show_rulers: Option<bool>,
    /// Tool options as last remembered in the preferences (see `tool_memory`).
    pub(crate) tool_options: Option<crate::state::ToolOptions>,
    pub(crate) selected_tool: Option<crate::state::Tool>,
    /// The tool owning the session brush and the other tools' brushes, as last remembered.
    pub(crate) tool_brushes: Option<crate::tool_memory::ToolBrushes>,
}

/// The GPU canvas colours from Preferences › Transparency & Gamut.
pub fn canvas_style(app: &PhotocraftApp) -> crate::gpu_canvas::CanvasStyle {
    let t = &app.session.prefs().transparency_and_gamut;
    let [l, d] = t.colors();
    let f = |c: [u8; 3]| c.map(|v| v as f32 / 255.0);
    let g = prefs::parse_hex(&t.gamut_warning_color).unwrap_or([128; 3]);
    crate::gpu_canvas::CanvasStyle {
        checker_square: t.square().unwrap_or(0.0),
        checker_light: f(l),
        checker_dark: f(d),
        gamut_color: f(g),
        gamut_opacity: t.gamut_warning_opacity as f32 / 100.0,
    }
}

/// Pasteboard colour from Preferences › Interface (`None` = the theme's default canvas).
pub fn pasteboard_color(app: &PhotocraftApp) -> Option<Color32> {
    use prefs::CanvasColor;
    let i = &app.session.prefs().interface;
    Some(match i.canvas_color {
        CanvasColor::Default => return None,
        CanvasColor::Black => Color32::BLACK,
        CanvasColor::DarkGray => Color32::from_gray(40),
        CanvasColor::MediumGray => Color32::from_gray(83),
        CanvasColor::LightGray => Color32::from_gray(184),
        CanvasColor::Custom => color_of(&i.canvas_custom_color),
    })
}

fn selected_theme(interface: &prefs::Interface, system: Option<egui::Theme>) -> ThemeKind {
    let light = match interface.appearance_mode {
        AppearanceMode::Light => true,
        AppearanceMode::Dark => false,
        AppearanceMode::Auto => system == Some(egui::Theme::Light),
    };
    if light {
        match interface.light_theme {
            LightTheme::StudioLight => ThemeKind::StudioLight,
            LightTheme::ProLightGray => ThemeKind::ProLightGray,
            LightTheme::ProLight => ThemeKind::ProLight,
            LightTheme::Classic => ThemeKind::Classic,
            LightTheme::Adwaita => ThemeKind::Adwaita,
            LightTheme::BreezeLight => ThemeKind::BreezeLight,
            LightTheme::SoftLight => ThemeKind::SoftLight,
        }
    } else {
        match interface.dark_theme {
            DarkTheme::Pro => ThemeKind::Pro,
            DarkTheme::ProMedium => ThemeKind::ProMedium,
            DarkTheme::Studio => ThemeKind::Studio,
            DarkTheme::SolarizedDark => ThemeKind::SolarizedDark,
            DarkTheme::AdwaitaDark => ThemeKind::AdwaitaDark,
            DarkTheme::BreezeDark => ThemeKind::BreezeDark,
            DarkTheme::SoftDark => ThemeKind::SoftDark,
        }
    }
}

pub(crate) fn cycle_appearance(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let next = match app.session.prefs().interface.appearance_mode {
        AppearanceMode::Auto => AppearanceMode::Light,
        AppearanceMode::Light => AppearanceMode::Dark,
        AppearanceMode::Dark => AppearanceMode::Auto,
    };
    if let Err(e) = app.run("prefs.set", json!({"path": "interface.appearanceMode", "value": next.name()})) {
        app.ui.status = e;
        return;
    }
    let selected = selected_theme(&app.session.prefs().interface, system_theme(app, ctx));
    if app.ui.theme != selected {
        app.apply_theme(ctx, selected);
    }
}

/// Photoshop's four interface brightnesses, darkest first.
const BRIGHTNESSES: [ThemeKind; 4] = [ThemeKind::Pro, ThemeKind::ProMedium, ThemeKind::ProLightGray, ThemeKind::ProLight];

/// Darken Interface (⇧⌘F1, `lighter` false) and Lighten Interface (⇧⌘F2): the next of
/// Photoshop's four brightnesses from the theme on screen, stopping at either end, as in
/// Photoshop. Another theme steps from the nearest brightness: Medium Gray for a dark one, Light
/// Gray for a light one. The choice is saved like picking it in Preferences › Interface.
pub(crate) fn step_brightness(app: &mut PhotocraftApp, ctx: &egui::Context, lighter: bool) -> Result<(), String> {
    let current = app.ui.theme;
    let at = BRIGHTNESSES.iter().position(|k| *k == current).unwrap_or(if crate::theme::Tokens::for_kind(current).dark() { 1 } else { 2 });
    let next = if lighter { (at + 1).min(BRIGHTNESSES.len() - 1) } else { at.saturating_sub(1) };
    let Some(&kind) = BRIGHTNESSES.get(next) else { return Ok(()) };
    if kind == current {
        return Ok(());
    }
    app.run("prefs.set", json!({"path": "interface.theme", "value": kind.id()}))?;
    sync_appearance(app, ctx);
    Ok(())
}

const LIGHT_THEME_OPTIONS: &[(&str, ThemeKind)] = &[
    ("proLight", ThemeKind::ProLight),
    ("proLightGray", ThemeKind::ProLightGray),
    ("studioLight", ThemeKind::StudioLight),
    ("classic", ThemeKind::Classic),
    ("adwaita", ThemeKind::Adwaita),
    ("breezeLight", ThemeKind::BreezeLight),
    ("softLight", ThemeKind::SoftLight),
];
const DARK_THEME_OPTIONS: &[(&str, ThemeKind)] = &[
    ("proMedium", ThemeKind::ProMedium),
    ("pro", ThemeKind::Pro),
    ("studio", ThemeKind::Studio),
    ("solarizedDark", ThemeKind::SolarizedDark),
    ("adwaitaDark", ThemeKind::AdwaitaDark),
    ("breezeDark", ThemeKind::BreezeDark),
    ("softDark", ThemeKind::SoftDark),
];

fn favorite_theme_groups(favorites: &[SavedTheme]) -> (Vec<ThemeKind>, Vec<ThemeKind>) {
    let group = |options: &[(&str, ThemeKind)]| {
        options.iter().filter_map(|(id, kind)| SavedTheme::parse(id).filter(|theme| favorites.contains(theme)).map(|_| *kind)).collect()
    };
    (group(LIGHT_THEME_OPTIONS), group(DARK_THEME_OPTIONS))
}

/// Right-click menu on the top-right appearance button. A plain click still cycles modes.
pub(crate) fn appearance_context_menu(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    if ui.button(tl!("Appearance Settings…")).clicked() {
        open_preferences(app, "interface");
        ui.close();
    }
    let (light, dark) = favorite_theme_groups(&app.session.prefs().interface.favorite_themes);
    let has_light = !light.is_empty();
    if !light.is_empty() || !dark.is_empty() {
        ui.separator();
    }
    for kind in light {
        if ui.selectable_label(app.ui.theme == kind, tl!(kind.label())).clicked() {
            app.set_theme(ui.ctx(), kind);
            ui.close();
        }
    }
    if has_light && !dark.is_empty() {
        ui.separator();
    }
    for kind in dark {
        if ui.selectable_label(app.ui.theme == kind, tl!(kind.label())).clicked() {
            app.set_theme(ui.ctx(), kind);
            ui.close();
        }
    }
}

fn system_theme(app: &PhotocraftApp, ctx: &egui::Context) -> Option<egui::Theme> {
    app.services.system_theme.as_ref().and_then(|read| read(ctx)).or_else(|| ctx.system_theme())
}

/// View › Rulers follows `unitsAndRulers.showRulers` when the preference changes (at launch, from
/// `prefs.set` or a reset), so documents open with the rulers as last set.
fn sync_rulers(app: &mut PhotocraftApp) {
    let want = app.session.prefs().units_and_rulers.show_rulers;
    if app.prefs_rt.show_rulers != Some(want) {
        app.ui.extras.rulers = want;
        app.prefs_rt.show_rulers = Some(want);
    }
}

/// View › Rulers was toggled: remember it for the next launch (Photoshop keeps the last state).
pub(crate) fn remember_rulers(app: &mut PhotocraftApp, on: bool) {
    if app.session.prefs().units_and_rulers.show_rulers != on {
        app.session.prefs.edit(|p| p.units_and_rulers.show_rulers = on);
    }
    app.prefs_rt.show_rulers = Some(on);
}

// ------------------------------------------------------------------ lifecycle

/// Load saved preferences once, when the app is created. Recovery starts during frame upkeep.
pub fn load(app: &mut PhotocraftApp) {
    if app.prefs_rt.loaded {
        return;
    }
    app.prefs_rt.loaded = true;
    let mut invalid = Vec::new();
    let mut stored = None;
    if let Some(text) = app.services.load_prefs.as_mut().and_then(|f| f()) {
        match app.session.load_prefs_json(&text) {
            Ok(found) => {
                invalid = found;
                stored = serde_json::from_str::<Value>(&text).ok();
            }
            Err(e) => {
                let template = "Preferences were reset: {error}";
                app.ui.status = crate::i18n::fmt(tl!(template), &[("error", &e)]);
            }
        }
    }
    crate::dock::restore(app);
    crate::brush_picker::restore(app);
    crate::last_brush::restore(app);
    sync_rulers(app);
    crate::tool_memory::restore(app);
    crate::view_cmds::restore(app);
    app.sync_recent();
    app.prefs_rt.saved_rev = app.session.prefs.rev();
    let loaded = app.session.prefs_value();
    app.prefs_rt.saved_value = Some(match stored {
        Some(stored) => stored_base(&loaded, &stored),
        None => loaded,
    });
    if !invalid.is_empty() {
        repair_invalid(app, &invalid);
    }
}

/// The save base after loading `stored`: what was loaded, with the keys the shift tool switch
/// migration may change (#5647) put back as the file had them, the way [`repair_invalid`] puts
/// back unreadable values. The migrated value and the new version then differ from the base, so
/// the next save writes them rather than keeping the old value under a newer version. Every other
/// key keeps its loaded value, so the three-way merge leaves another instance's changes alone,
/// also for keys the file doesn't have yet. A file from a newer version keeps its version.
fn stored_base(loaded: &Value, stored: &Value) -> Value {
    let mut base = loaded.clone();
    let Some(obj) = base.as_object_mut() else {
        return base;
    };
    match stored.get("version") {
        Some(v) if v.as_u64().is_some_and(|v| v > prefs::PREFS_VERSION) => {}
        Some(v) => {
            obj.insert("version".into(), v.clone());
        }
        None => {
            obj.remove("version");
        }
    }
    if let (Some(v), Some(tools)) = (stored.pointer("/tools/useShiftKeyForToolSwitch"), obj.get_mut("tools").and_then(Value::as_object_mut)) {
        tools.insert("useShiftKeyForToolSwitch".into(), v.clone());
    }
    base
}

/// Some saved values couldn't be read and took their defaults (#6611): say which, and write
/// the defaults over them on the next save so the next launch reads a clean file. The save
/// base holds the unreadable values, so the three-way merge sees them as changed here and
/// replaces (or removes) exactly those, keeping everything else in the stored file.
fn repair_invalid(app: &mut PhotocraftApp, invalid: &[prefs::InvalidPref]) {
    if let Some(base) = app.prefs_rt.saved_value.as_mut() {
        for bad in invalid {
            let Some((key, parent)) = bad.keys.split_last() else {
                continue;
            };
            if let Some(obj) = parent.iter().try_fold(&mut *base, |cur, k| cur.get_mut(k.as_str())).and_then(Value::as_object_mut) {
                obj.insert(key.clone(), bad.value.clone());
            }
        }
    }
    app.session.prefs.edit(|_| ());
    let paths = invalid.iter().map(prefs::InvalidPref::path).collect::<Vec<_>>().join(", ");
    let (title, line) = (
        "Some preferences were reset",
        "The preferences file held values PhotoCraft can't read for {paths}. Those settings are back to their defaults; the rest of your preferences loaded.",
    );
    // Notices keep their English source and are translated when drawn.
    let id = crate::notices::post(app, title, vec![line.into()], false, None);
    if let Some(notice) = app.ui.notices.iter_mut().find(|n| n.id == id) {
        notice.args.push(("paths".into(), paths));
    }
}

/// Discover recovery entries after construction, then decode one at a time without blocking
/// the window. Only the completed job's apply step touches the session.
fn recovery(app: &mut PhotocraftApp) {
    if !app.prefs_rt.recovery_started {
        app.prefs_rt.recovery_started = true;
        if app.session.prefs().file_handling.recover_on_launch
            && let Some(recover) = app.services.recover.as_mut()
        {
            app.prefs_rt.recovery_queue.extend(recover());
        }
    }
    start_next_recovery(app);
}

fn start_next_recovery(app: &mut PhotocraftApp) {
    while app.prefs_rt.recovery_pending.is_none() {
        let Some(r) = app.prefs_rt.recovery_queue.pop_front() else { return };
        let label = format!("Recovering {}", r.name);
        let started = app.session.start_job(
            "file.recover",
            json!({}),
            &label,
            false,
            move |ctx| {
                ctx.check()?;
                ctx.progress(0.0, "Loading recovery data");
                let doc = (r.load)().map_err(photocraft_engine::EngineError::Other)?;
                ctx.check()?;
                Ok(doc)
            },
            move |s, doc| {
                let active = s.active().map(|st| st.doc.id);
                let first = s.documents().is_empty();
                s.add_document(doc, r.path);
                let st = s.active_mut().ok_or(photocraft_engine::EngineError::NoDocument)?;
                st.saved_revision = 0;
                let (id, revision) = (st.doc.id, st.revision);
                if let Some(index) = active.and_then(|id| s.documents().iter().position(|st| st.doc.id == id)) {
                    s.set_active(index);
                }
                Ok(json!({"documentId": id.0, "revision": revision, "firstDocument": first}))
            },
        );
        match started {
            Ok(Started::Job(job)) => app.prefs_rt.recovery_pending = Some((job, r.key)),
            Ok(Started::Done(v)) => finish_recovery(app, &r.key, &v),
            Err(e) => crate::notices::error(app, format!("{label}: {e}")),
        }
    }
}

fn finish_recovery(app: &mut PhotocraftApp, key: &str, v: &Value) {
    let recovered = v
        .get("documentId")
        .and_then(Value::as_u64)
        .zip(v.get("revision").and_then(Value::as_u64))
        .and_then(|(id, revision)| app.session.documents().iter().find(|st| st.doc.id.0 == id).map(|st| (st.doc.id, revision, st.doc.name.clone())));
    let Some((id, revision, name)) = recovered else {
        crate::notices::error(app, "The recovered document is no longer open".into());
        return;
    };
    // Track the admitted snapshot's revision, even if another job has already edited it.
    app.prefs_rt.autosaved.insert(id, revision);
    if let Some(adopt) = app.services.adopt_autosave.as_mut() {
        adopt(id.0, key);
    }
    if v.get("firstDocument").and_then(Value::as_bool) == Some(true) {
        app.ui.chrome.home = None;
    }
    app.sync_views();
    let template = "Recovered {name}";
    app.ui.status = crate::i18n::fmt(tl!(template), &[("name", &name)]);
    app.ui.status_error = false;
}

/// Recovery uses the existing job polling/progress/cancel path. Its successful result adopts
/// the original entry on the UI thread, using identity rather than a mutable tab position.
pub(crate) fn on_recovery_event(app: &mut PhotocraftApp, e: &JobEvent) -> bool {
    if !app.prefs_rt.recovery_pending.as_ref().is_some_and(|(job, _)| *job == e.id) {
        return false;
    }
    let Some((_, key)) = app.prefs_rt.recovery_pending.take() else { return false };
    match &e.outcome {
        JobOutcome::Done(v) => finish_recovery(app, &key, v),
        JobOutcome::Failed(err) => crate::notices::error(app, format!("{}: {err}", e.label)),
        JobOutcome::Cancelled => {
            // Native bundle decoding cannot stop midway. Cancel admission and the rest of the
            // batch so another large decoder isn't started while this worker finishes.
            app.prefs_rt.recovery_queue.clear();
            app.ui.status = crate::i18n::fmt(tl!("Cancelled {label}"), &[("label", tl!(&e.label))]);
            app.ui.status_error = false;
        }
    }
    start_next_recovery(app);
    true
}

/// Attach the brush preset store once its background load finishes, and surface write
/// failures in the status bar.
fn presets_store(app: &mut PhotocraftApp) {
    if let Some(rx) = &app.services.preset_store {
        match rx.try_recv() {
            Ok(opened) => {
                app.services.preset_store = None;
                let warnings = app.session.attach_preset_store(opened);
                if let Some(w) = warnings.first() {
                    app.ui.status = if warnings.len() == 1 {
                        w.clone()
                    } else {
                        crate::i18n::fmt(
                            &crate::i18n::trn(
                                crate::i18n::current(),
                                (warnings.len() - 1) as u64,
                                "{warning} (+{n} more brush preset warning)",
                                "{warning} (+{n} more brush preset warnings)",
                            ),
                            &[("warning", w)],
                        )
                    };
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => app.services.preset_store = None,
        }
    }
    if let Some(w) = app.session.preset_store.as_mut().map(|s| s.take_warnings()).and_then(|w| w.into_iter().next()) {
        app.ui.status = w;
    }
}

/// Resolve an absolute pixels-per-point preference without multiplying the OS DPI twice.
/// Monitor dimensions are physical pixels, independent of window size and UI zoom.
fn display_scale(pref: prefs::UiScale, native: Option<f32>) -> f32 {
    let native = native.filter(|v| v.is_finite() && *v > 0.0).unwrap_or(1.0);
    match pref {
        // The system's scale, fractional ones included (125%, 150%, 175%: #1072), and 100% on a
        // large 4K display run unscaled (#1278); 200% there is a fixed choice.
        prefs::UiScale::Auto => native,
        fixed => fixed.name().parse::<f32>().map_or(1.0, |pct| pct / 100.0),
    }
}

fn sync_display_scale(app: &PhotocraftApp, ctx: &egui::Context) {
    let native = ctx.native_pixels_per_point();
    let native_scale = native.filter(|v| v.is_finite() && *v > 0.0).unwrap_or(1.0);
    let scale = display_scale(app.session.prefs().interface.ui_scale, native);
    ctx.set_zoom_factor(scale / native_scale);
}

/// Interface › Show Tooltips and Tools › Show Tooltips (either one off hides them): egui never
/// shows a tooltip whose delay is infinite. Re-checked every frame because a theme change
/// rebuilds the style; that's one style read, and a write only when it differs.
fn sync_tooltips(app: &PhotocraftApp, ctx: &egui::Context) {
    let p = app.session.prefs();
    let delay = if p.interface.show_tooltips && p.tools.show_tooltips { crate::theme::TOOLTIP_DELAY } else { f32::INFINITY };
    if ctx.global_style().interaction.tooltip_delay != delay {
        ctx.global_style_mut(|s| s.interaction.tooltip_delay = delay);
    }
}

/// Interface › Enable Animations: off, egui's fades and slides take no time and scroll areas jump
/// straight to their target (#3273). Re-checked every frame because a theme change rebuilds the
/// style; both the dark and light styles are set, so a system appearance switch keeps it.
fn sync_animations(app: &PhotocraftApp, ctx: &egui::Context) {
    let on = app.session.prefs().interface.enable_animations;
    let time = if on { egui::Style::default().animation_time } else { 0.0 };
    if ctx.global_style().animation_time != time {
        let scroll = if on { egui::style::ScrollAnimation::default() } else { egui::style::ScrollAnimation::none() };
        ctx.all_styles_mut(|s| {
            s.animation_time = time;
            s.scroll_animation = scroll;
        });
    }
}

/// Resolve the saved appearance mode without changing it or either theme slot.
pub(crate) fn sync_appearance(app: &mut PhotocraftApp, ctx: &egui::Context) {
    // A restored or embedding-supplied pin must not suppress native appearance events.
    ctx.set_theme(egui::ThemePreference::System);
    // The native service also covers Wayland sessions where winit has no system theme.
    let selected = selected_theme(&app.session.prefs().interface, system_theme(app, ctx));
    if app.ui.theme != selected {
        app.apply_theme(ctx, selected);
    }
}

/// Per-frame upkeep: theme sync, persistence, autosave and the history log.
pub fn tick(app: &mut PhotocraftApp, ctx: &egui::Context) {
    if !app.prefs_rt.loaded {
        load(app);
    }
    recovery(app);
    sync_rulers(app);
    sync_display_scale(app, ctx);
    sync_appearance(app, ctx);
    crate::theme::set_ui_font_size(ctx, app.session.prefs().interface.ui_font_size);
    presets_store(app);
    sync_tooltips(app, ctx);
    sync_animations(app, ctx);
    app.sync_recent();
    // Monotonic, not `ctx.input(|i| i.time)`: logic also runs on hidden ticks, where egui's input
    // time is that of the last UI pass.
    if let Some(wait) = persist(app, crate::gpu_canvas::now_ms() / 1000.0) {
        ctx.request_repaint_after(std::time::Duration::try_from_secs_f64(wait).unwrap_or_default());
    }
    let style = canvas_style(app);
    if let Some(gpu) = app.gpu.as_ref()
        && app.prefs_rt.gpu_style != Some(style)
    {
        gpu.set_style(style);
        app.prefs_rt.gpu_style = Some(style);
    }
    autosave(app);
    #[cfg(not(target_arch = "wasm32"))]
    link_updates(app, ctx);
    history_log(app);
}

/// First retry delay after a failed preferences write, in seconds; it doubles with every further
/// failure up to [`SAVE_RETRY_MAX_S`]. A new change retries sooner, but no sooner than this.
const SAVE_RETRY_S: f64 = 2.0;
const SAVE_RETRY_MAX_S: f64 = 30.0;
/// Failed writes in a row before a notice says preferences aren't being saved.
const SAVE_NOTICE_AFTER: u32 = 3;

/// Retry state of a failed preferences write (times are egui input time, in seconds).
#[derive(Default)]
struct SaveRetry {
    failures: u32,
    /// When the next retry is due.
    at: f64,
    /// When the last write was tried, and the revision it carried.
    tried_at: f64,
    tried_rev: u64,
    /// The "not saved" notice, while it's shown.
    notice: Option<u64>,
}

/// Save changed preferences. A failed write stays pending and is retried with a backoff (a newer
/// change retries sooner); the status bar reports the first failure and a notice a persistent
/// one. Returns the seconds until the next retry while one is pending.
fn persist(app: &mut PhotocraftApp, now: f64) -> Option<f64> {
    let rev = app.session.prefs.rev();
    if rev == app.prefs_rt.saved_rev {
        return None;
    }
    let r = &app.prefs_rt.save_retry;
    if r.failures > 0 {
        let due = if rev == r.tried_rev { r.at } else { r.at.min(r.tried_at + SAVE_RETRY_S) };
        if now < due {
            return Some(due - now);
        }
    }
    let ours = app.session.prefs_value();
    // A change that was undone (or set a value to what it already was) leaves nothing to write.
    let written = if app.prefs_rt.saved_value.as_ref() == Some(&ours) { Ok(()) } else { write_prefs(app, &ours) };
    match written {
        Ok(()) => {
            app.prefs_rt.saved_rev = rev;
            app.prefs_rt.saved_value = Some(ours);
            let r = std::mem::take(&mut app.prefs_rt.save_retry);
            if r.failures > 0 {
                let message = "Preferences saved";
                app.ui.status = tl!(message).into();
                app.ui.status_error = false;
                if let Some(id) = r.notice {
                    app.ui.notices.retain(|n| n.id != id);
                }
            }
            None
        }
        Err(e) => {
            let r = &mut app.prefs_rt.save_retry;
            r.failures = r.failures.saturating_add(1);
            (r.tried_at, r.tried_rev) = (now, rev);
            let delay = (SAVE_RETRY_S * 2f64.powi(r.failures.min(16) as i32 - 1)).min(SAVE_RETRY_MAX_S);
            r.at = now + delay;
            let failures = r.failures;
            if failures == 1 {
                let template = "Couldn't save preferences: {error}. Retrying…";
                app.ui.status = crate::i18n::fmt(tl!(template), &[("error", &e)]);
                app.ui.status_error = true;
            } else if failures == SAVE_NOTICE_AFTER {
                let lines = vec![e, "PhotoCraft keeps retrying; until a save succeeds, preference changes are lost when it closes.".into()];
                let id = crate::notices::post(app, "Preferences can't be saved", lines, true, None);
                app.prefs_rt.save_retry.notice = Some(id);
            }
            Some(delay)
        }
    }
}

/// Write `ours` over the stored preferences, keeping what another instance (a second browser
/// tab, another app window) changed there since this one last loaded or saved them.
fn write_prefs(app: &mut PhotocraftApp, ours: &Value) -> Result<(), String> {
    let Some(save) = app.services.save_prefs.as_mut() else { return Ok(()) };
    let stored = app.services.load_prefs.as_mut().and_then(|load| load()).and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let text = match (stored, &app.prefs_rt.saved_value) {
        (Some(mut theirs @ Value::Object(_)), Some(base)) => {
            let base = &without_stale_migration(base, &theirs);
            merge_changes(base, ours, &mut theirs);
            serde_json::to_string_pretty(&theirs)
        }
        _ => serde_json::to_string_pretty(ours),
    }
    .map_err(|e| e.to_string())?;
    save(&text)
}

/// The save base without the shift tool switch migration (#5647) once storage already holds a
/// version 2 file: another instance wrote it, so its Use Shift Key for Tool Switch is a choice,
/// not the old default this instance migrated. The base then holds the migrated value (on) instead
/// of the old file's off, so only a change made here since loading is written over that choice.
fn without_stale_migration(base: &Value, theirs: &Value) -> Value {
    let mut base = base.clone();
    let new_file = |v: &Value| v.get("version").and_then(Value::as_u64).is_some_and(|v| v >= 2);
    if new_file(theirs) && !new_file(&base) {
        let key = "useShiftKeyForToolSwitch";
        if let Some(tools) = base.get_mut("tools").and_then(Value::as_object_mut)
            && tools.get(key) == Some(&Value::Bool(false))
        {
            tools.insert(key.into(), Value::Bool(true));
        }
    }
    base
}

/// Three-way merge of preference trees: whatever `ours` changed since `base` goes over `theirs`
/// (what storage holds now) and the rest of `theirs` is kept. Objects merge key by key; any
/// other value, arrays included, is replaced whole.
fn merge_changes(base: &Value, ours: &Value, theirs: &mut Value) {
    if ours == base {
        return;
    }
    match (base, ours, theirs) {
        (Value::Object(b), Value::Object(o), Value::Object(t)) => {
            for (k, ov) in o {
                match (b.get(k), t.get_mut(k)) {
                    (Some(bv), Some(tv)) => merge_changes(bv, ov, tv),
                    // Removed there, unchanged here: it stays removed.
                    (Some(bv), None) if bv == ov => {}
                    _ => {
                        t.insert(k.clone(), ov.clone());
                    }
                }
            }
            for k in b.keys().filter(|k| !o.contains_key(*k)) {
                t.remove(k);
            }
        }
        (_, _, theirs) => *theirs = ours.clone(),
    }
}

/// Write the preferences now, through the same merge as [`persist`], for recovery choices that
/// need the result immediately. The pending revision stays unsaved when the write fails.
pub(crate) fn save_preferences(app: &mut PhotocraftApp) -> Result<(), String> {
    let rev = app.session.prefs.rev();
    let ours = app.session.prefs_value();
    write_prefs(app, &ours)?;
    app.prefs_rt.saved_rev = rev;
    app.prefs_rt.saved_value = Some(ours);
    if let Some(id) = std::mem::take(&mut app.prefs_rt.save_retry).notice {
        app.ui.notices.retain(|n| n.id != id);
    }
    Ok(())
}

/// Linked smart objects follow their files while the document is open, as in Photoshop: every two
/// seconds, and as soon as the window comes back to the front (after saving in another app), stamp
/// the active document's linked files on a worker thread and re-render the layers of the ones that
/// changed ([`photocraft_engine::link_watch`]).
#[cfg(not(target_arch = "wasm32"))]
fn link_updates(app: &mut PhotocraftApp, ctx: &egui::Context) {
    match app.session.poll_link_scan() {
        Some(Ok(r)) => {
            if let Some(n) = r["updated"].as_array().map(Vec::len).filter(|n| *n > 0) {
                app.ui.status = crate::i18n::trn(
                    crate::i18n::current(),
                    n as u64,
                    "Updated {n} linked smart object changed on disk",
                    "Updated {n} linked smart objects changed on disk",
                );
            }
        }
        Some(Err(e)) => app.ui.status = e.to_string(),
        None => {}
    }
    let (last_key, focus_key) = (egui::Id::unique("linkWatch.lastScan"), egui::Id::unique("linkWatch.focused"));
    let now = crate::gpu_canvas::now_ms() / 1000.0;
    let focused = ctx.input(|i| i.focused);
    let was_focused: bool = ctx.data(|d| d.get_temp(focus_key)).unwrap_or(focused);
    ctx.data_mut(|d| d.insert_temp(focus_key, focused));
    let has_links = app.session.active().is_some_and(|d| !photocraft_engine::link_watch::linked_files(&d.doc).is_empty());
    if !has_links {
        return;
    }
    let last: f64 = ctx.data(|d| d.get_temp(last_key)).unwrap_or(f64::NEG_INFINITY);
    if now - last >= 2.0 || (focused && !was_focused) {
        ctx.data_mut(|d| d.insert_temp(last_key, now));
        app.session.start_link_scan();
    }
    let wait = if app.session.link_scan.is_some() { 100 } else { 2000 };
    ctx.request_repaint_after(std::time::Duration::from_millis(wait));
}

/// Background autosave of documents with unsaved changes every N minutes (File Handling).
fn autosave(app: &mut PhotocraftApp) {
    let fh = &app.session.prefs().file_handling;
    let (on, minutes) = (fh.autosave, fh.autosave_minutes.max(1));
    if app.services.autosave.is_none() {
        return;
    }
    // A successful queue operation is not a successful disk write. Poll each frame,
    // including frames before the next autosave interval, so failures are visible.
    if let Some(results) = app.services.autosave_results.as_mut() {
        for (raw_id, revision, result) in results() {
            let id = DocId(raw_id);
            if app.prefs_rt.autosave_pending.get(&id) != Some(&revision) {
                continue;
            }
            app.prefs_rt.autosave_pending.remove(&id);
            match result {
                Ok(()) => {
                    app.prefs_rt.autosaved.insert(id, revision);
                }
                Err(e) => {
                    let template = "Autosave failed: {error}";
                    app.ui.status = crate::i18n::fmt(tl!(template), &[("error", &e)]);
                    app.ui.status_error = true;
                }
            }
        }
    }
    let now = crate::gpu_canvas::now_ms();
    // Saved or closed documents drop their recovery data.
    let live: HashMap<DocId, bool> = app.session.documents().iter().map(|d| (d.doc.id, d.is_dirty())).collect();
    let mut stale: Vec<DocId> = app.prefs_rt.autosaved.keys().filter(|id| live.get(id) != Some(&true)).copied().collect();
    stale.extend(app.prefs_rt.autosave_pending.keys().filter(|id| live.get(*id) != Some(&true) && !app.prefs_rt.autosaved.contains_key(*id)).copied());
    for id in stale {
        app.prefs_rt.autosaved.remove(&id);
        app.prefs_rt.autosave_pending.remove(&id);
        if let Some(d) = app.services.discard_autosave.as_mut() {
            d(id.0);
        }
    }
    if !on {
        return;
    }
    let interval = minutes as f64 * 60_000.0;
    if app.prefs_rt.next_autosave_ms == 0.0 || app.prefs_rt.next_autosave_ms > now + interval {
        app.prefs_rt.next_autosave_ms = now + interval;
        return;
    }
    if now < app.prefs_rt.next_autosave_ms {
        return;
    }
    app.prefs_rt.next_autosave_ms = now + interval;
    let jobs: Vec<_> = app
        .session
        .documents()
        .iter()
        .filter(|d| d.is_dirty() && app.prefs_rt.autosaved.get(&d.doc.id) != Some(&d.revision) && !app.prefs_rt.autosave_pending.contains_key(&d.doc.id))
        .map(|d| (d.doc.clone(), d.revision, d.path.clone()))
        .collect();
    for (doc, rev, path) in jobs {
        if let Some(save) = app.services.autosave.as_mut() {
            match save(&doc, rev, path.as_deref()) {
                Ok(()) => {
                    if app.services.autosave_results.is_some() {
                        app.prefs_rt.autosave_pending.insert(doc.id, rev);
                    } else {
                        // Synchronous or mocked services report completion on return.
                        app.prefs_rt.autosaved.insert(doc.id, rev);
                    }
                }
                Err(e) => {
                    let template = "Autosave failed: {error}";
                    app.ui.status = crate::i18n::fmt(tl!(template), &[("error", &e)]);
                    app.ui.status_error = true;
                }
            }
        }
    }
}

/// Force the autosave timer to fire on the next tick (tests, `prefs` changes).
pub fn autosave_now(app: &mut PhotocraftApp) {
    app.prefs_rt.next_autosave_ms = f64::MIN_POSITIVE;
}

/// History Log preference: append executed commands to a text file.
fn history_log(app: &mut PhotocraftApp) {
    let n = app.session.journal.len();
    if n <= app.prefs_rt.log_len {
        app.prefs_rt.log_len = n;
        return;
    }
    let hl = app.session.prefs().history_log.clone();
    let start = app.prefs_rt.log_len;
    app.prefs_rt.log_len = n;
    if !hl.enabled || hl.file_path.is_empty() || hl.destination == prefs::LogDestination::Metadata {
        return;
    }
    let Some(append) = app.services.append_text.as_mut() else { return };
    let mut text = String::new();
    for (id, params) in &app.session.journal[start..] {
        let label = photocraft_engine::commands::find(id).map_or(id.as_str(), |c| c.label).trim_end_matches('…');
        match hl.detail {
            prefs::LogDetail::SessionsOnly => {
                if matches!(id.as_str(), "file.new" | "file.open" | "file.close" | "file.openAs") {
                    text.push_str(&format!("{label}\n"));
                }
            }
            prefs::LogDetail::Concise => text.push_str(&format!("{label}\n")),
            prefs::LogDetail::Detailed => text.push_str(&format!("{label}  {id} {params}\n")),
        }
    }
    if !text.is_empty() {
        let _ = append(&hl.file_path, &text);
    }
}

// ------------------------------------------------------------------ shortcuts

pub use crate::shortcuts::{default_shortcut, effective_shortcut};

/// Every shortcut-bearing command (engine registry plus shell commands): (id, label, menu path,
/// default shortcut), menu items first in Photoshop order.
pub fn shortcut_items(app: &PhotocraftApp) -> Vec<(String, String, Vec<String>, Option<String>)> {
    let mut out: Vec<(String, String, Vec<String>, Option<String>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for it in crate::menus::menu_items(app) {
        if it.label == "---" || !seen.insert(it.id.clone()) {
            continue;
        }
        let default = default_shortcut(&it.id).or(it.shortcut.clone().filter(|_| !app.session.prefs().shortcuts.contains_key(&it.id)));
        out.push((it.id, it.label, it.path, default));
    }
    // Commands without a menu item: the colour and fill keys (D, X, ⌥⌫…) sit under Tools, as
    // Photoshop lists its colour keys in the Tools shortcut set, after the rest; then the held
    // temporary tools (#249).
    let tools_key = |id: &str| id.starts_with("tools.") || photocraft_engine::fill_key_cmds::IDS.contains(&id);
    let mut tools = Vec::new();
    let mut layer = Vec::new();
    for c in photocraft_engine::command_specs() {
        if seen.insert(c.id.to_string()) && c.shortcut.is_some() {
            let item = (c.id.to_string(), c.label.to_string(), c.menu.iter().map(|s| s.to_string()).collect::<Vec<_>>(), c.shortcut.map(Into::into));
            if c.menu.is_empty() && tools_key(c.id) {
                tools.push((item.0, item.1, vec!["Tools".to_string()], item.3));
            } else if c.menu.is_empty() && c.id.starts_with("layer.") {
                // Stamp Visible / Stamp Down (#217): no menu item in Photoshop; listed at the end
                // of the Layer section.
                layer.push((item.0, item.1, vec!["Layer".to_string()], item.3));
            } else {
                out.push(item);
            }
        }
    }
    let at = out.iter().rposition(|i| i.2.first().map(String::as_str) == Some("Layer")).map_or(out.len(), |i| i + 1);
    out.splice(at..at, layer);
    out.extend(tools);
    out.extend(
        crate::state::Tool::ALL
            .into_iter()
            .map(|tool| (crate::tool_shortcuts::id(tool), tool.label().to_string(), vec!["Tools".into()], crate::tool_shortcuts::default(tool))),
    );
    for (id, label, def) in prefs::TEMPORARY_TOOLS {
        if seen.insert(id.to_string()) {
            out.push((id.to_string(), label.to_string(), vec!["Tools".into(), "Temporary".into()], Some(def.to_string())));
        }
    }
    out
}

/// A fingerprint of the bindings a shortcut conflict describes.
fn conflict_state(overrides: &BTreeMap<String, String>, alternates: &BTreeMap<String, Vec<String>>, selected: &str, set: Option<&Value>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (overrides, alternates, selected, set.map(Value::to_string)).hash(&mut h);
    h.finish()
}

/// Keys the system takes before the app sees them (switching apps, closing the window, force
/// quit): Edit › Keyboard Shortcuts won't assign them, as Photoshop won't. `sc` is in our
/// notation, where `Cmd` is Ctrl off the Mac.
pub fn reserved_shortcut(sc: &str) -> bool {
    prefs::reserved_shortcut(sc)
}

/// Text of a key press for the shortcut editor (`Cmd+Shift+K`), `None` for bare modifiers.
pub fn shortcut_text(key: egui::Key, m: egui::Modifiers) -> Option<String> {
    if is_modifier_key(key) {
        return None;
    }
    let mut parts = Vec::new();
    if m.command || m.mac_cmd {
        parts.push(tl!("Cmd").to_string());
    }
    if m.ctrl && !m.command {
        parts.push(tl!("Ctrl").into());
    }
    if m.alt {
        parts.push(tl!("Alt").into());
    }
    if m.shift {
        parts.push(tl!("Shift").into());
    }
    let name = match key {
        egui::Key::Equals => "=",
        egui::Key::Minus => "-",
        egui::Key::OpenBracket => "[",
        egui::Key::CloseBracket => "]",
        egui::Key::Semicolon => ";",
        egui::Key::Quote => "'",
        egui::Key::Plus => "",
        k => k.name(),
    };
    parts.push(name.to_string());
    prefs::normalize_shortcut(&parts.join("+"))
}

/// A modifier key on its own. egui reports Ctrl, Shift, Alt and ⌘ presses as key events too
/// (`ControlLeft`…), before the key pressed with them: they are never a shortcut's key, so
/// shortcut capture waits for the real key (#292: Ctrl+F was recorded as "Ctrl+ControlLeft").
pub fn is_modifier_key(key: egui::Key) -> bool {
    use egui::Key::*;
    matches!(key, ShiftLeft | ShiftRight | ControlLeft | ControlRight | AltLeft | AltRight | SuperLeft | SuperRight)
}

/// Modifier bits a physical modifier-key press contributes when egui leaves them out of
/// [`Modifiers`] on the key event (#7190).
fn modifier_key_modifiers(key: egui::Key) -> egui::Modifiers {
    use egui::Key::*;
    match key {
        ShiftLeft | ShiftRight => egui::Modifiers::SHIFT,
        ControlLeft | ControlRight => egui::Modifiers::CTRL,
        AltLeft | AltRight => egui::Modifiers::ALT,
        SuperLeft | SuperRight => egui::Modifiers::COMMAND,
        _ => egui::Modifiers::NONE,
    }
}

fn capture_mods_field(v: Option<&Value>) -> egui::Modifiers {
    let o = v.and_then(Value::as_object);
    egui::Modifiers {
        shift: o.and_then(|o| o.get("shift")).and_then(Value::as_bool).unwrap_or(false),
        ctrl: o.and_then(|o| o.get("ctrl")).and_then(Value::as_bool).unwrap_or(false),
        alt: o.and_then(|o| o.get("alt")).and_then(Value::as_bool).unwrap_or(false),
        command: o.and_then(|o| o.get("command")).and_then(Value::as_bool).unwrap_or(false),
        mac_cmd: o.and_then(|o| o.get("mac_cmd")).and_then(Value::as_bool).unwrap_or(false),
    }
}

fn capture_mods_json(m: egui::Modifiers) -> Value {
    json!({
        "shift": m.shift,
        "ctrl": m.ctrl,
        "alt": m.alt,
        "command": m.command,
        "mac_cmd": m.mac_cmd,
    })
}

/// While capture is active: refresh held modifiers (including across frames via dialog
/// `captureMods`) and, if a non-modifier key was pressed this frame, return it with the
/// modifiers held with it.
///
/// #7190 root cause: capture used to [`find_map`](Iterator::find_map) the first non-modifier
/// [`Event::Key`] and pass only that event's `modifiers` into [`shortcut_text`]. On Linux and
/// Windows, winit/egui often reports `ShiftLeft`/`ShiftRight` (and sometimes the letter key) with
/// [`Modifiers::NONE`], and the modifier down and the letter down often land in separate UI
/// frames. Shift was then missing from the stored binding (`F6` instead of `Shift+F6`). Like #292
/// for `ControlLeft`, we walk the frame's events, OR in [`modifier_key_modifiers`] for modifier
/// keys, and persist accumulated modifiers in `captureMods` until capture ends.
pub(crate) fn capture_key_press(capture_mods: egui::Modifiers, i: &egui::InputState) -> (egui::Modifiers, Option<(egui::Key, egui::Modifiers)>) {
    let mut mods = capture_mods | i.modifiers;
    let mut pressed = None;
    for e in &i.events {
        if let egui::Event::ModifiersChanged(m) = e {
            mods = *m;
        }
        if let egui::Event::Key { key, pressed: true, modifiers, .. } = e {
            if is_modifier_key(*key) {
                mods = *modifiers | modifier_key_modifiers(*key);
                continue;
            }
            pressed = Some((*key, mods | *modifiers));
            break;
        }
        if let egui::Event::Key { key, pressed: false, modifiers, .. } = e
            && is_modifier_key(*key)
        {
            mods = *modifiers;
        }
    }
    (mods, pressed)
}

// ------------------------------------------------------------------ menu routing

/// Shell front ends for Edit-menu commands invoked without parameters (dialogs, pickers).
/// `None` when `id` isn't handled here.
pub fn invoke(app: &mut PhotocraftApp, ctx: &egui::Context, id: &str, params: &Value) -> Option<Result<Value, String>> {
    let empty = params.as_object().is_none_or(|o| o.is_empty());
    if !empty {
        return None;
    }
    if let Some(section) = id.strip_prefix("edit.preferences.") {
        return Some(Ok(json!({"dialog": open_preferences(app, section)})));
    }
    let dialog = |d: Option<u64>| Some(d.map(|d| json!({"dialog": d})).ok_or_else(|| "not available".to_string()));
    match id {
        "edit.keyboardShortcuts" => Some(Ok(json!({"dialog": open_shortcuts(app, 0)}))),
        "edit.menus" => Some(Ok(json!({"dialog": open_shortcuts(app, 1)}))),
        "edit.toolbar" => Some(Ok(json!({"dialog": open_shortcuts(app, 2)}))),
        "edit.colorSettings" => {
            let d = crate::filter_dialog::open(app, "edit.colorSettings");
            let cur = serde_json::to_value(&app.session.color.settings).unwrap_or_default();
            // What Monitor Profile resolves to, so a fallback to sRGB is visible (#569): read the
            // displays again now, and the dialog refreshes this line as it draws.
            crate::monitor_status::read_now(app);
            let note = crate::monitor_status::note(app);
            if let Some(dm) = d.and_then(|d| app.ui.dialog_mut(d)) {
                for (k, v) in cur.as_object().into_iter().flatten() {
                    if dm.fields.contains_key(k) {
                        dm.fields.insert(k.clone(), v.clone());
                    }
                }
                dm.fields.insert("__note".into(), json!(note));
            }
            dialog(d)
        }
        "edit.fade" | "edit.findAndReplaceText" | "edit.defineBrushPreset" | "edit.defineCustomShape" | "edit.autoAlignLayers" | "edit.autoBlendLayers" => {
            if !app.session.is_enabled(id) {
                return Some(Err(photocraft_engine::commands::find(id)
                    .map_or("not available".into(), |c| format!("{} is not available right now", c.label.trim_end_matches('…')))));
            }
            let d = crate::filter_dialog::open(app, id);
            if let Some(dm) = d.and_then(|d| app.ui.dialog_mut(d)) {
                // Optional parameters the generic dialog can't express keep their defaults.
                for k in ["reference", "path"] {
                    dm.fields.remove(k);
                }
            }
            dialog(d)
        }
        // Photoshop's Content-Aware Fill workspace (caf_ui); the engine command with params
        // still runs directly.
        "edit.contentAwareFill" => Some(crate::caf_ui::open(app).map(|_| json!({"contentAwareFill": app.distort.describe()["contentAwareFill"]}))),
        // Photoshop has no dialog: the menu opens the scale box (with params: the engine).
        crate::cas_transform::COMMAND => Some(crate::cas_transform::begin(app, ctx)),
        "edit.presets.presetManager" => {
            Some(Ok(json!({"dialog": open_kind(app, "presets", "Preset Manager", json!({"kind": "brushes", "selected": 0, "newName": ""}))})))
        }
        "edit.presets.exportImportPresets" => Some(Ok(
            json!({"dialog": open_kind(app, "presetsIO", "Export/Import Presets", json!({"action": "export", "brushes": true, "customShapes": true, "tools": true}))}),
        )),
        _ => None,
    }
}

fn open_kind(app: &mut PhotocraftApp, kind: &str, label: &str, fields: Value) -> u64 {
    let mut f = Map::new();
    f.insert("__prefsui".into(), json!(kind));
    f.insert("__label".into(), json!(label));
    if let Value::Object(m) = fields {
        f.extend(m);
    }
    app.ui.open_dialog(DialogKind::Command, f)
}

/// Is this a dialog rendered by this module?
pub fn owns(fields: &Map<String, Value>) -> bool {
    fields.contains_key("__prefsui")
}

/// Only Preferences supports applying changes without closing the dialog.
pub fn is_preferences(fields: &Map<String, Value>) -> bool {
    fields.get("__prefsui").and_then(Value::as_str) == Some("prefs")
}

fn preference_values(p: &prefs::Preferences) -> Map<String, Value> {
    let values = p.to_json();
    SECTIONS.iter().filter_map(|(id, _)| Some((id.to_string(), values.get(id)?.clone()))).collect()
}

/// Compare the editable sections, excluding settings managed outside Preferences.
pub fn preferences_changed(app: &PhotocraftApp, fields: &Map<String, Value>) -> bool {
    is_preferences(fields) && fields.get("values").and_then(Value::as_object).is_some_and(|values| *values != preference_values(app.session.prefs()))
}

/// Commit the working copy and keep the current section open. Failure leaves the draft intact.
pub fn apply(app: &mut PhotocraftApp, id: u64) -> Result<Value, String> {
    let d = app.ui.dialogs.iter().find(|d| d.id == id).ok_or_else(|| format!("no dialog {id}"))?;
    if d.kind != DialogKind::Command || !is_preferences(&d.fields) {
        return Err("Apply is only available for Preferences".into());
    }
    let fields = d.fields.clone();
    let result = confirm(app, &fields)?;
    let values = Value::Object(preference_values(app.session.prefs()));
    if let Some(d) = app.ui.dialog_mut(id) {
        // Use the validated values as the next draft, including any normalisation by prefs.set.
        d.fields.insert("values".into(), values);
    }
    Ok(result)
}

/// The size Preferences opens at: room for the widest section and about 30 settings rows. The
/// dialog's resize grip changes it, and its contents follow (#7420).
pub const PREFERENCES_SIZE: egui::Vec2 = egui::vec2(1040.0, 800.0);

/// The height of the settings area (section list and fields) when `available` is left below the
/// search field: everything but the dialog's button row (`item_spacing` + 8 pt + the buttons) and
/// a little slack, never less than a few rows.
fn settings_height(available: f32, item_spacing: f32, button_height: f32) -> f32 {
    (available - (item_spacing + 8.0 + button_height + 4.0)).max(160.0)
}

/// Max dialog width for our dialogs. Preferences fills the width it is given (`available`, its
/// resizable area).
pub fn width(fields: &Map<String, Value>, available: f32) -> Option<f32> {
    match fields.get("__prefsui").and_then(Value::as_str)? {
        "prefs" => Some(available.max(380.0)),
        "shortcuts" => Some(720.0),
        _ => Some(460.0),
    }
}

/// Open Edit › Preferences on `section`.
pub fn open_preferences(app: &mut PhotocraftApp, section: &str) -> u64 {
    let section = if SECTIONS.iter().any(|(id, _)| *id == section) { section } else { "general" };
    let working = preference_values(app.session.prefs());
    let order = field_order(app.session.prefs(), &working);
    open_kind(app, "prefs", "Preferences", json!({"section": section, "values": working, "__order": order}))
}

/// Each section's keys in declaration order (JSON objects sort their keys; the serialised text
/// keeps the struct's order, which groups related settings like Photoshop's dialog).
fn field_order(p: &prefs::Preferences, sections: &Map<String, Value>) -> Value {
    let text = serde_json::to_string(p).unwrap_or_default();
    let mut out = Map::new();
    for (id, v) in sections {
        let Some(start) = text.find(&format!("\"{id}\":{{")) else { continue };
        let body = &text[start..];
        let end = body.find('}').unwrap_or(body.len());
        let region = &body[..end];
        let mut keys: Vec<(usize, String)> =
            v.as_object().into_iter().flatten().map(|(k, _)| (region.find(&format!("\"{k}\":")).unwrap_or(usize::MAX), k.clone())).collect();
        keys.sort();
        out.insert(id.clone(), json!(keys.into_iter().map(|(_, k)| k).collect::<Vec<_>>()));
    }
    Value::Object(out)
}

/// Open Keyboard Shortcuts and Menus on a tab (0 shortcuts, 1 menus, 2 toolbar).
pub fn open_shortcuts(app: &mut PhotocraftApp, tab: u64) -> u64 {
    let p = app.session.prefs();
    let fields = json!({
        "tab": tab,
        "filter": "",
        "overrides": p.shortcuts,
        "alternates": p.alternate_shortcuts,
        "sets": p.shortcut_sets,
        "activeSet": p.shortcut_set,
        "hidden": p.menus.hidden,
        "colors": p.menus.colors,
        "toolbarHidden": p.toolbar.hidden,
        "selected": "",
        "capture": false,
        "message": "",
    });
    open_kind(app, "shortcuts", tl!("Keyboard Shortcuts and Menus"), fields)
}

/// Open the Embedded Profile Mismatch prompt for a file waiting to open (`open_prompt`): its
/// `message`, the answer selected first (`action`), and `marker`, the field that routes OK and
/// Cancel back to `open_prompt`.
/// `names` carries the embedded and working profile names and the Color Settings policy a
/// remembered choice is saved to (`policyKey`, null for a mode without one).
pub fn open_mismatch(app: &mut PhotocraftApp, message: &str, action: &str, missing: bool, marker: &str, names: Value) -> u64 {
    let mut fields = json!({"message": message, "action": action, "missing": missing, "remember": false});
    if let (Some(f), Value::Object(n)) = (fields.as_object_mut(), names) {
        f.extend(n);
    }
    fields[marker] = json!(true);
    open_kind(app, "mismatch", tl!("Embedded Profile Mismatch"), fields)
}

// ------------------------------------------------------------------ bodies

/// Render one of our dialogs' bodies.
pub fn body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    match f.get("__prefsui").and_then(Value::as_str).unwrap_or("") {
        "prefs" => {
            let mut info = app.perf.gpu_info.lines();
            info.extend(gpu_memory_lines(app));
            f.insert("__gpuInfo".into(), json!(info));
            f.insert("__gpuAdapters".into(), json!(app.perf.gpu_info.adapters));
            let system = system_theme(app, ui.ctx());
            prefs_body(ui, f, system);
        }
        "shortcuts" => shortcuts_body(app, ui, f),
        "presets" => presets_body(app, ui, f),
        "presetsIO" => presets_io_body(ui, f),
        "mismatch" => mismatch_body(ui, f),
        _ => {}
    }
}

/// Performance › Graphics: the compositor's GPU memory budget and whether the active document's
/// layers fit it (the GPU Memory preference sets it).
fn gpu_memory_lines(app: &PhotocraftApp) -> Vec<String> {
    let mut v = Vec::new();
    if app.perf.gpu_budget_mb > 0 {
        let budget = crate::chrome_ui::fmt_bytes(app.perf.gpu_budget_mb.saturating_mul(1 << 20));
        v.push(crate::i18n::fmt(tl!("GPU memory budget: {budget}"), &[("budget", &budget)]));
    }
    let doc = app.session.active().map(|st| st.doc.id.0);
    if let Some(fit) = app.perf.gpu_layer_fit.filter(|f| Some(f.doc) == doc) {
        let args = [("needed", crate::chrome_ui::fmt_bytes(fit.needed)), ("budget", crate::chrome_ui::fmt_bytes(fit.budget))];
        let args: Vec<(&str, &str)> = args.iter().map(|(k, v)| (*k, v.as_str())).collect();
        v.push(if fit.fits() {
            crate::i18n::fmt(tl!("This document's layers: {needed} of {budget} for layers"), &args)
        } else {
            crate::i18n::fmt(tl!("This document's layers need {needed} but may hold {budget}: whole-document redraws use the CPU"), &args)
        });
    }
    v
}

/// The label a preference key shows in the dialog: a few keys have their own wording, the rest
/// split camelCase into words (`convertToSrgb` → "Convert to srgb").
fn humanize(key: &str) -> String {
    match key {
        "appearanceMode" => return "Appearance Mode".into(),
        "exrAcesCg" => return "Open color-less EXR as ACEScg".into(),
        "darkTheme" => return "Dark Theme".into(),
        "lightTheme" => return "Light Theme".into(),
        "automaticallyCheckUpdates" => return "Automatically Check for Updates".into(),
        _ => {}
    }
    // These controls appear only for WebP, so reuse the existing translated labels.
    if key == "webpLossless" {
        return "Lossless".into();
    }
    if key == "webpQuality" {
        return "Quality".into();
    }
    // The auto-hide notice settings read best with their own labels (Interface › Notification, #2022).
    if key == "notificationAutoHide" {
        return "Auto Hide Notifications".into();
    }
    if key == "notificationDurationSeconds" {
        return "Notification Duration (seconds)".into();
    }
    // Tools: arrow-key nudges bundle into one History state (#2259).
    if key == "bundleNudges" {
        return "Bundle Arrow-Key Nudges".into();
    }
    if key == "nudgeBundlePauseMs" {
        return "Nudge Bundle Pause (ms)".into();
    }
    // Performance: presentation without vsync (#1343); like the other graphics settings, it
    // takes effect when the window next opens.
    // File Handling: Photoshop's wording (#8080).
    if key == "doNotAppendCopy" {
        return "Do not append \"copy\" to filename when saving a copy".into();
    }
    if key == "lowLatencyDrawing" {
        return "Low-Latency Drawing (vsync off, applies at next launch)".into();
    }
    // Performance: the canvas compositor's GPU memory, 0 for the automatic budget (L3).
    if key == "gpuMemoryMb" {
        return "GPU Memory (MB, 0 = Automatic)".into();
    }
    let mut s = String::new();
    for (i, ch) in key.chars().enumerate() {
        if i == 0 {
            s.extend(ch.to_uppercase());
        } else if ch.is_uppercase() {
            s.push(' ');
            s.extend(ch.to_lowercase());
        } else {
            s.push(ch);
        }
    }
    s.replace("Psd", "PSD")
        .replace("Gpu", "GPU")
        .replace("Ui ", "UI ")
        .replace("Mb", "(MB)")
        .replace("Exif", "EXIF")
        .replace("Hud", "HUD")
        .replace("Lut ", "LUT ")
}

fn choice_label(v: &str) -> String {
    match v {
        "exr" => "EXR".into(),
        "cm" => "Centimeters".into(),
        "mm" => "Millimeters".into(),
        "75" | "80" | "85" | "90" | "95" | "100" | "105" | "110" | "115" | "120" | "125" | "150" | "175" | "200" | "250" | "300" => format!("{v}%"),
        "8" => "8 Bits/Channel".into(),
        "16" => "16 Bits/Channel".into(),
        "postScript" => "PostScript (72 points/inch)".into(),
        "traditional" => "Traditional (72.27 points/inch)".into(),
        "vulkan" => "Vulkan".into(),
        "dx12" => "DirectX 12".into(),
        "metal" => "Metal".into(),
        "gl" => "OpenGL".into(),
        "ahd" => "AHD (adaptive homogeneity)".into(),
        "mhc" => "Malvar–He–Cutler".into(),
        "cpu" => "CPU (no GPU acceleration)".into(),
        v => humanize(v),
    }
}

/// An editable `#rrggbb` field beside a colour swatch (#668): typing a valid value sets the
/// colour; while it's being typed a partial value is kept, and it shows the colour otherwise.
fn hex_field(ui: &mut egui::Ui, key: &str, rgb: &mut [u8; 3]) {
    let id = egui::Id::unique(("pref-hex", key));
    let shown = format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
    let mut text = ui.data(|d| d.get_temp::<String>(id)).unwrap_or_else(|| shown.clone());
    let r = crate::widgets::text_field_ex(ui, id, &mut text, "", 72.0, true, Some(7)).accessible_name(tl!("Color"));
    if r.changed()
        && let Some(c) = prefs::parse_hex(&text)
    {
        *rgb = c;
    }
    if r.has_focus() {
        ui.data_mut(|d| d.insert_temp(id, text));
    } else {
        ui.data_mut(|d| d.remove::<String>(id));
    }
}

fn color_of(s: &str) -> Color32 {
    prefs::parse_hex(s).map_or(Color32::GRAY, |c| Color32::from_rgb(c[0], c[1], c[2]))
}

/// Search the same visible, editable preference labels used in the dialog.
fn preference_matches(section: &str, key: &str, query: &str) -> bool {
    let label = humanize(key);
    format!("{section}.{key}").to_lowercase().contains(query) || label.to_lowercase().contains(query) || tl!(&label).to_lowercase().contains(query)
}

fn preference_sections(values: &Value, search: &str) -> Vec<(&'static str, &'static str)> {
    let query = search.trim().to_lowercase();
    SECTIONS
        .iter()
        .copied()
        .filter(|(id, title)| {
            query.is_empty()
                || title.to_lowercase().contains(&query)
                || tl!(title).to_lowercase().contains(&query)
                || values
                    .get(*id)
                    .and_then(Value::as_object)
                    .is_some_and(|fields| fields.keys().any(|key| !prefs::is_hidden(&format!("{id}.{key}")) && preference_matches(id, key, &query)))
        })
        .collect()
}

/// Preferences: section list on the left, the section's settings on the right.
fn prefs_body(ui: &mut egui::Ui, f: &mut Map<String, Value>, system: Option<egui::Theme>) {
    let t = Tokens::get(ui.ctx());
    let mut section = f.get("section").and_then(Value::as_str).unwrap_or("general").to_string();
    let mut values = f.get("values").cloned().unwrap_or(Value::Null);
    // The dialog follows the language being edited, so a change shows before OK.
    let lang = crate::i18n::Lang::from_pref(values.pointer("/interface/language").and_then(Value::as_str).unwrap_or("auto"));
    let mut search = f.get("__search").and_then(Value::as_str).unwrap_or("").to_string();
    crate::widgets::text_field(ui, ui.scope_id().with("prefs-search"), &mut search, tl!("Search settings…"), ui.available_width(), false);
    let found = preference_sections(&values, &search);
    if !found.iter().any(|(id, _)| *id == section)
        && let Some((first, _)) = found.first()
    {
        section = (*first).to_string();
    }
    ui.add_space(6.0);
    ui.horizontal_top(|ui| {
        // Section list.
        ui.vertical(|ui| {
            ui.set_width(170.0);
            if found.is_empty() {
                ui.label(RichText::new(tl!("No matching settings")).color(t.text_dim));
            }
            for &(id, title) in &found {
                let sel = section == id;
                let empty = !has_visible_fields(&values, id);
                let (rect, resp) = ui.allocate_exact_size(vec2(170.0, 22.0), Sense::click());
                if sel {
                    ui.painter().rect_filled(rect, t.radius_sm, t.row_selected);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, t.radius_sm, t.hover);
                }
                let color = if sel {
                    t.text
                } else if empty {
                    t.text_faint
                } else {
                    t.text_dim
                };
                // Translated section names can be longer than the column: elide them (the full
                // name is the tooltip) instead of drawing over the settings.
                let mut job = egui::text::LayoutJob::simple_singleline(tl!(title).to_string(), crate::theme::medium(12.5), color);
                job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - 12.0);
                let galley = ui.painter().layout_job(job);
                let elided = galley.elided;
                ui.painter().galley(rect.left_center() + vec2(8.0, -galley.size().y / 2.0), galley, color);
                let resp = if elided { resp.on_hover_text(tl!(title)) } else { resp };
                if resp.clicked() {
                    section = id.to_string();
                }
            }
        });
        // The settings fill the dialog's resizable area down to the button row (#7420). Sizing them
        // from the window instead left a resized dialog with empty space and the buttons mid-way.
        // The area grows to fit its content, so this must never ask for more than it was given.
        let content_height = settings_height(ui.available_height(), ui.spacing().item_spacing.y, crate::widgets::button_height(ui.ctx()));
        crate::widgets::vline(ui, content_height);
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - 12.0).max(340.0));
            let title = SECTIONS.iter().find(|(id, _)| *id == section).map_or("General", |(_, t)| *t);
            ui.label(RichText::new(tl!(&title)).font(crate::theme::semibold(14.0)).color(t.text));
            let show_all = search.trim().is_empty()
                || title.to_lowercase().contains(&search.trim().to_lowercase())
                || tl!(&title).to_lowercase().contains(&search.trim().to_lowercase());
            let filter = if show_all { "" } else { search.trim() };
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(content_height - 30.0).id_salt("prefs-scroll").show(ui, |ui| {
                let order: Vec<String> =
                    f.get("__order").and_then(|o| o.get(&section)).and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
                if !has_visible_fields(&values, &section) {
                    ui.add_space(4.0);
                    ui.label(RichText::new(tl!("These settings aren't available in PhotoCraft yet.")).color(t.text_faint));
                } else if let Some(obj) = values.get_mut(&section).and_then(Value::as_object_mut) {
                    section_fields(ui, &section, obj, &order, lang, system, filter);
                    if section == "fileHandling" {
                        ui.label(
                            RichText::new(tl!("0 turns this threshold off; SVG groups nested deeper than 100 levels are always rasterized."))
                                .color(t.text_faint),
                        );
                    }
                    if section == "performance" {
                        gpu_status_rows(ui, f.get("__gpuInfo"), f.get("__gpuAdapters"), obj);
                    }
                    ui.add_space(8.0);
                }
                if search.trim().is_empty()
                    && has_visible_fields(&values, &section)
                    && crate::widgets::secondary_button(ui, tl!("Reset Section"), 110.0).clicked()
                    && let Some(def) = prefs::Preferences::default().get(&section)
                {
                    values[&section] = def;
                }
            });
        });
    });
    f.insert("section".into(), json!(section));
    f.insert("__search".into(), json!(search));
    f.insert("values".into(), values);
    // A colour swatch was clicked: open the Color Picker on that value (#2144).
    if let Some((path, name)) = ctx_take_pick(ui) {
        crate::color_picker_ui::request_field(f, &format!("/values/{}", path.replace('.', "/")), &crate::color_picker_ui::title_for(&name));
    }
}

/// Preferences › Performance: what the app renders with now, and a reset of the GPU backend
/// (a crashed start may have moved it to a safer choice).
fn gpu_status_rows(ui: &mut egui::Ui, info: Option<&Value>, adapters: Option<&Value>, obj: &mut Map<String, Value>) {
    let t = Tokens::get(ui.ctx());
    ui.add_space(10.0);
    ui.label(RichText::new(tl!("Graphics")).font(crate::theme::semibold(12.5)).color(t.text));
    for line in info.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        ui.label(RichText::new(line).color(t.text_dim));
    }
    ui.add_space(4.0);
    ui.label(RichText::new(tl!("Applies at next launch.")).color(t.text_faint));
    ui.collapsing(tl!("Advanced"), |ui| {
        ui.horizontal(|ui| {
            ui.label(tl!("GPU Backend"));
            let mut current = obj.get("gpuBackend").and_then(Value::as_str).unwrap_or("auto").to_string();
            let options = prefs::choices("performance.gpuBackend").unwrap_or(&[]);
            let labels: Vec<String> = options.iter().map(|o| choice_label(o)).collect();
            let pairs: Vec<(String, &str)> = options.iter().map(|o| o.to_string()).zip(labels.iter().map(String::as_str)).collect();
            crate::widgets::dropdown(ui, "graphics-backend", tl!("GPU Backend"), &mut current, &pairs, 220.0);
            obj.insert("gpuBackend".into(), json!(current));
        });
        if !prefs::is_hidden("performance.gpuAdapter") {
            graphics_adapter_row(ui, adapters, obj);
        }
    });
}

/// Preferences › Performance › Graphics Adapter: which GPU draws on a machine with several. The
/// value is the adapter's name; empty is automatic. A saved adapter that isn't present now (an
/// unplugged eGPU) stays listed, marked ⚠, so opening the dialog doesn't silently change the choice.
fn graphics_adapter_row(ui: &mut egui::Ui, adapters: Option<&Value>, obj: &mut Map<String, Value>) {
    let mut current = obj.get("gpuAdapter").and_then(Value::as_str).unwrap_or("").to_string();
    let mut names: Vec<String> = adapters.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
    // A saved name fragment ("nvidia") isn't an adapter's full name but still selects one; only a
    // value no listed adapter matches is marked.
    let kept = !current.is_empty() && !names.contains(&current);
    let missing = kept && !names.iter().any(|n| prefs::gpu_adapter_matches(n, &current));
    if kept {
        names.push(current.clone());
    }
    let automatic = tl!("Automatic");
    let labels: Vec<String> = names.iter().map(|n| if missing && *n == current { format!("⚠ {n}") } else { n.clone() }).collect();
    let mut pairs: Vec<(String, &str)> = vec![(String::new(), automatic)];
    pairs.extend(names.iter().cloned().zip(labels.iter().map(String::as_str)));
    ui.horizontal(|ui| {
        ui.label(tl!("GPU adapter"));
        crate::widgets::dropdown_literal(ui, "graphics-adapter", tl!("GPU adapter"), &mut current, &pairs, 320.0);
    });
    ui.label(RichText::new(tl!("Applies at next launch.")).color(Tokens::get(ui.ctx()).text_faint));
    obj.insert("gpuAdapter".into(), json!(current));
}

/// One user-facing choice; legacy flags remain compatible with older settings.
fn rendering_mode_row(ui: &mut egui::Ui, obj: &mut Map<String, Value>) {
    let mut current = rendering_mode_value(obj);
    ui.horizontal(|ui| {
        ui.label(tl!("Rendering Mode"));
        let pairs =
            vec![("auto".to_string(), tl!("Automatic (recommended)")), ("gpu".to_string(), tl!("GPU")), ("cpu".to_string(), tl!("CPU / Compatibility"))];
        let previous = current.clone();
        crate::widgets::dropdown(ui, "rendering-mode", tl!("Rendering Mode"), &mut current, &pairs, 240.0);
        if current != previous {
            obj.insert("renderingMode".into(), json!(current));
            obj.insert("useGpu".into(), json!(current != "cpu"));
        }
    });
    ui.label(tl!("Automatic uses GPU acceleration when available and falls back to CPU rendering on errors."));
    // Next to the setting, as for System Title Bar (#1677): the renderer is picked when the
    // window opens, so a change otherwise looks like it did nothing (#7939).
    ui.label(RichText::new(tl!("Applies at next launch.")).color(Tokens::get(ui.ctx()).text_dim));
    ui.add_space(8.0);
}

fn rendering_mode_value(obj: &Map<String, Value>) -> String {
    obj.get("renderingMode").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| {
        if obj.get("useGpu").and_then(Value::as_bool) == Some(false) || obj.get("gpuBackend").and_then(Value::as_str) == Some("cpu") {
            "cpu".into()
        } else {
            "auto".into()
        }
    })
}

/// Does `section` have any setting the dialog shows (see [`prefs::HIDDEN_UNTIL_IMPLEMENTED`])?
fn has_visible_fields(values: &Value, section: &str) -> bool {
    values.get(section).and_then(Value::as_object).is_some_and(|o| o.keys().any(|k| !prefs::is_hidden(&format!("{section}.{k}"))))
}

/// JPEG and WebP have independent settings; unrelated format controls stay out of view.
fn export_field_visible(obj: &Map<String, Value>, key: &str) -> bool {
    let format = obj.get("quickExportFormat").and_then(Value::as_str).unwrap_or("png");
    match key {
        "convertToSrgb" | "metadata" => format != "exr",
        "jpegQuality" => format == "jpg",
        "webpLossless" => format == "webp",
        "webpQuality" => format == "webp" && obj.get("webpLossless").and_then(Value::as_bool) != Some(true),
        _ => true,
    }
}

/// The pause that bundles arrow-key nudges means nothing with Bundle Arrow-Key Nudges off (#2259).
fn tools_field_visible(obj: &Map<String, Value>, key: &str) -> bool {
    key != "nudgeBundlePauseMs" || obj.get("bundleNudges").and_then(Value::as_bool) != Some(false)
}

/// One theme as a Color Theme swatch: its panel colour under a chrome strip with an accent dot,
/// outlined when chosen. The name is the tooltip and the accessible label. A star badge on its
/// corner adds the theme to (or removes it from) the favourites listed by the appearance
/// button's menu; it shows while the swatch is hovered, and stays for a favourite.
fn theme_swatch(ui: &mut egui::Ui, kind: ThemeKind, id: &str, selected: &mut String, favorites: &mut Vec<SavedTheme>) {
    const BADGE: f32 = 7.0;
    let t = Tokens::get(ui.ctx());
    let p = Tokens::for_kind(kind);
    let label = tl!(&choice_label(id)).to_string();
    let on = selected.as_str() == id;
    let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 26.0), Sense::click());
    let painter = ui.painter();
    let inner = rect.shrink(3.0);
    painter.rect_filled(inner, 2.0, p.dock);
    painter.rect_filled(egui::Rect::from_min_size(inner.min, vec2(inner.width(), 5.0)), 0.0, p.chrome);
    painter.circle_filled(inner.right_bottom() - vec2(6.0, 6.0), 2.5, p.accent);
    let stroke = if on {
        egui::Stroke::new(2.0, t.accent)
    } else if resp.hovered() {
        egui::Stroke::new(1.0, t.text_dim)
    } else {
        egui::Stroke::new(1.0, t.card_border)
    };
    painter.rect_stroke(rect.shrink(1.0), 3.0, stroke, egui::StrokeKind::Inside);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::RadioButton, true, on, &label));
    if resp.on_hover_text(&label).clicked() {
        *selected = id.to_string();
    }
    let Some(theme) = SavedTheme::parse(id) else { return };
    let favorite = favorites.contains(&theme);
    let center = rect.right_top() + vec2(-3.0, 3.0);
    let badge = egui::Rect::from_center_size(center, vec2(BADGE * 2.0, BADGE * 2.0));
    if !(favorite || ui.rect_contains_pointer(rect.union(badge))) {
        return;
    }
    ui.painter().circle(center, BADGE, t.card, egui::Stroke::new(1.0, t.card_border));
    let (icon, tint) = if favorite { ("star-filled", t.accent) } else { ("star", t.text_dim) };
    let action = if favorite { tl!("Remove from favorites") } else { tl!("Add to favorites") };
    // Interact and paint without allocating: a widget placed at `badge` would grow the row by the
    // 4 px the badge overhangs the swatch, re-centring the swatches after this one whenever the
    // pointer arrives, and oscillating when that moves the swatch out from under it.
    let response = ui.interact(badge, ui.scope_id().with((id, "favorite")), Sense::click()).on_hover_text(action);
    crate::icons::paint(ui, badge, icon, 10.0, tint);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("{action}: {label}")));
    if response.clicked() {
        if favorite {
            favorites.retain(|saved| *saved != theme);
        } else {
            favorites.push(theme);
        }
    }
}

/// A grid row of theme swatches, marked Active when the appearance mode shows this family.
fn theme_row(ui: &mut egui::Ui, title: &str, active: bool, selected: &mut String, favorites: &mut Vec<SavedTheme>, options: &[(&str, ThemeKind)]) {
    let t = Tokens::get(ui.ctx());
    ui.label(RichText::new(tl!(title)).color(t.text_dim));
    ui.horizontal(|ui| {
        // Room for the favourite star badges on the swatch corners.
        ui.spacing_mut().item_spacing.x = 6.0;
        for (id, kind) in options {
            theme_swatch(ui, *kind, id, selected, favorites);
        }
        if active {
            ui.add_space(6.0);
            ui.label(RichText::new(tl!("Active")).color(t.accent_text).small());
        }
    });
    ui.end_row();
}

/// Which of the Appearance rows drawn by `appearance_rows` a search shows.
#[derive(Clone, Copy)]
struct AppearanceRows {
    mode: bool,
    light: bool,
    dark: bool,
}

impl AppearanceRows {
    /// `query` is lowercased; empty shows every row.
    fn matching(query: &str) -> Self {
        let shown = |key: &str| query.is_empty() || preference_matches("interface", key, query);
        Self { mode: shown("appearanceMode"), light: shown("lightTheme"), dark: shown("darkTheme") }
    }

    fn any(self) -> bool {
        self.mode || self.light || self.dark
    }
}

/// Appearance Mode and the light and dark Color Theme swatches, as rows of the Appearance grid.
fn appearance_rows(ui: &mut egui::Ui, obj: &mut Map<String, Value>, system: Option<egui::Theme>, rows: AppearanceRows) {
    let t = Tokens::get(ui.ctx());
    let mut mode = obj.get("appearanceMode").and_then(Value::as_str).unwrap_or("auto").to_string();
    if rows.mode {
        ui.label(RichText::new(tl!("Appearance Mode")).color(t.text_dim));
        let choices: [(String, &str); 3] = [("auto".into(), tl!("Sync with system")), ("dark".into(), tl!("Dark")), ("light".into(), tl!("Light"))];
        let pairs: Vec<(String, &str)> = choices.iter().map(|(id, label)| (id.clone(), *label)).collect();
        crate::widgets::dropdown(ui, "appearance-mode", tl!("Appearance Mode"), &mut mode, &pairs, 220.0);
        ui.end_row();
    }
    let light_active = mode == "light" || (mode == "auto" && system == Some(egui::Theme::Light));
    obj.insert("appearanceMode".into(), json!(mode));
    let mut light = obj.get("lightTheme").and_then(Value::as_str).unwrap_or("studioLight").to_string();
    let mut dark = obj.get("darkTheme").and_then(Value::as_str).unwrap_or("proMedium").to_string();
    let mut favorites: Vec<SavedTheme> = obj.get("favoriteThemes").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    if rows.light {
        theme_row(ui, "Light Theme", light_active, &mut light, &mut favorites, LIGHT_THEME_OPTIONS);
    }
    if rows.dark {
        theme_row(ui, "Dark Theme", !light_active, &mut dark, &mut favorites, DARK_THEME_OPTIONS);
    }
    obj.insert("lightTheme".into(), json!(light));
    obj.insert("darkTheme".into(), json!(dark));
    obj.insert("favoriteThemes".into(), json!(favorites));
}

/// A titled, outlined group of settings (Photoshop's Interface page has Appearance,
/// Presentation and Options).
fn pref_group(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    let t = Tokens::get(ui.ctx());
    egui::Frame::new().stroke(egui::Stroke::new(1.0, t.card_border)).corner_radius(t.radius).inner_margin(10.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(tl!(title)).strong().color(t.text));
        ui.add_space(4.0);
        add(ui);
    });
    ui.add_space(8.0);
}

/// Preferences › Interface laid out like Photoshop's: the theme and canvas settings, then UI
/// language, font size and scaling, then everything else. Compact enough that the language
/// shows without scrolling in a small window (#2532). A search (`query`, lowercased) hides the
/// rows that don't match and skips a group left empty.
fn interface_groups(ui: &mut egui::Ui, obj: &mut Map<String, Value>, keys: &[String], lang: crate::i18n::Lang, system: Option<egui::Theme>, query: &str) {
    const APPEARANCE: [&str; 3] = ["canvasColor", "canvasCustomColor", "canvasBorder"];
    const PRESENTATION: [&str; 3] = ["language", "uiFontSize", "uiScale"];
    let shown = |k: &str| keys.iter().any(|key| key == k) && field_visible("interface", obj, k, query);
    // In Photoshop's order, not the stored one.
    let pick = |names: &[&str]| names.iter().filter(|n| shown(n)).map(|n| n.to_string()).collect::<Vec<_>>();
    let appearance = pick(&APPEARANCE);
    let presentation = pick(&PRESENTATION);
    let rest = keys.iter().filter(|k| !APPEARANCE.contains(&k.as_str()) && !PRESENTATION.contains(&k.as_str()) && shown(k));
    // Checkboxes line up at the left edge, as in Photoshop; the other options keep a label column.
    let (checks, other): (Vec<String>, Vec<String>) = rest.cloned().partition(|k| obj.get(k).is_some_and(Value::is_boolean));
    let rows = AppearanceRows::matching(query);
    let grid = |id: &str| egui::Grid::new(("prefs-grid", id.to_string())).num_columns(2).spacing([14.0, 7.0]).min_col_width(150.0);
    if rows.any() || !appearance.is_empty() {
        pref_group(ui, "Appearance", |ui| {
            grid("interface-appearance").show(ui, |ui| {
                appearance_rows(ui, obj, system, rows);
                field_rows(ui, "interface", obj, appearance, lang);
            });
        });
    }
    if !presentation.is_empty() {
        pref_group(ui, "Presentation", |ui| {
            grid("interface-presentation").show(ui, |ui| field_rows(ui, "interface", obj, presentation, lang));
        });
    }
    if !checks.is_empty() || !other.is_empty() {
        pref_group(ui, "Options", |ui| {
            egui::Grid::new("prefs-grid-interface-checks").num_columns(2).spacing([0.0, 7.0]).show(ui, |ui| field_rows(ui, "interface", obj, checks, lang));
            grid("interface-options").show(ui, |ui| field_rows(ui, "interface", obj, other, lang));
        });
    }
}

/// Where a clicked colour swatch leaves its preference path and label for the dialog body, which
/// holds the fields the Color Picker request goes in.
const PICK_ID: &str = "prefs-pick-color";

/// The colour swatch clicked this frame, if any: (preference path, label).
fn ctx_take_pick(ui: &egui::Ui) -> Option<(String, String)> {
    ui.ctx().data_mut(|d| d.remove_temp::<(String, String)>(egui::Id::unique(PICK_ID)))
}

/// Whether `section.key` gets a generic row for this search (`query`, lowercased; empty matches
/// everything). Settings nothing reads yet stay out of the dialog (issue #204), as do those drawn
/// by their own widgets; their stored values pass through untouched.
fn field_visible(section: &str, obj: &Map<String, Value>, k: &str, query: &str) -> bool {
    let path = format!("{section}.{k}");
    let drawn_elsewhere = (section == "interface" && matches!(k, "theme" | "appearanceMode" | "darkTheme" | "lightTheme" | "favoriteThemes"))
        // View › Rulers (⌘R) sets it, as in Photoshop.
        || path == "unitsAndRulers.showRulers"
        || (section == "performance" && matches!(k, "useGpu" | "gpuBackend" | "gpuAdapter" | "renderingMode" | "memoryUsageMb"))
        // The browser presents frames itself: Low-Latency Drawing does nothing there.
        || (cfg!(target_arch = "wasm32") && path == "performance.lowLatencyDrawing")
        || (section == "export" && !export_field_visible(obj, k))
        || (section == "tools" && !tools_field_visible(obj, k));
    (query.is_empty() || preference_matches(section, k, query))
        && !drawn_elsewhere
        && !prefs::is_hidden(&path)
        && obj.get(k).is_some_and(|v| !matches!(v, Value::Null | Value::Object(_)))
}

/// Generic editor for a section's fields: checkboxes, dropdowns for choices, colour swatches,
/// number fields with the preference's range, text fields.
fn section_fields(
    ui: &mut egui::Ui,
    section: &str,
    obj: &mut Map<String, Value>,
    order: &[String],
    lang: crate::i18n::Lang,
    system: Option<egui::Theme>,
    search: &str,
) {
    let query = search.to_lowercase();
    let mut keys: Vec<String> = order.iter().filter(|k| obj.contains_key(*k)).cloned().collect();
    keys.extend(obj.keys().filter(|k| !order.contains(k)).cloned());
    if section == "interface" {
        interface_groups(ui, obj, &keys, lang, system, &query);
        return;
    }
    if section == "performance" && (query.is_empty() || preference_matches(section, "renderingMode", &query)) {
        rendering_mode_row(ui, obj);
    }
    if section == "performance" && (query.is_empty() || preference_matches(section, "memoryUsageMb", &query)) {
        memory_usage_row(ui, obj);
    }
    keys.retain(|k| field_visible(section, obj, k, &query));
    egui::Grid::new(("prefs-grid", section)).num_columns(2).spacing([14.0, 7.0]).show(ui, |ui| field_rows(ui, section, obj, keys, lang));
}

/// Explain the per-document RAM limit separately from the graphics-memory preference.
fn memory_usage_row(ui: &mut egui::Ui, obj: &mut Map<String, Value>) {
    let t = Tokens::get(ui.ctx());
    ui.group(|ui| {
        ui.label(RichText::new(tl!("Memory Usage")).strong());
        let mut mb = obj.get("memoryUsageMb").and_then(Value::as_u64).unwrap_or(8192);
        ui.horizontal(|ui| {
            let limit_label = ui.label(tl!("Per-document RAM limit (MB)"));
            if ui.add(egui::DragValue::new(&mut mb).range(256..=1_048_576).suffix(" MB")).labelled_by(limit_label.id).changed() {
                obj.insert("memoryUsageMb".into(), json!(mb));
            }
        });
        if let Some(ram) = crate::gpu_canvas::physical_memory().filter(|n| *n > 0) {
            let installed = ram / (1 << 20);
            let percent = mb as f64 * 100.0 / (ram as f64 / (1 << 20) as f64);
            ui.label(crate::i18n::fmt(tl!("Installed RAM: {ram} MB; this limit: {percent}%"), &[("ram", &installed.to_string()), ("percent", &format!("{percent:.1}"))]));
        }
        ui.label(RichText::new(tl!("Limits each open document's pixels plus History. Oldest undo states are discarded when the limit is exceeded.")).color(t.text_dim));
        ui.label(RichText::new(tl!("Applies immediately when saved. Memory is not reserved in advance.")).color(t.text_dim));
        ui.label(RichText::new(tl!("With GPU Memory set to Automatic, this also limits the GPU layer budget. That budget is capped at one quarter of installed RAM and, when known, three eighths of the graphics card's memory, with a 512 MB minimum. Raising this limit beyond those caps does not increase the GPU budget.")).color(t.text_dim));
    });
    ui.add_space(8.0);
}

/// Settings read only when the app starts, which say "Applies at next launch." under their row.
fn read_at_launch(path: &str) -> bool {
    match path {
        // Only Windows and Linux draw their own title bar.
        "interface.systemTitleBar" => cfg!(all(not(target_arch = "wasm32"), not(target_os = "macos"))),
        // The desktop app decides on the control server before its window opens.
        "integrations.allowAgentControl" | "integrations.controlPort" => cfg!(not(target_arch = "wasm32")),
        _ => false,
    }
}

/// One grid row per setting in `keys` (already filtered by `field_visible`).
fn field_rows(ui: &mut egui::Ui, section: &str, obj: &mut Map<String, Value>, keys: Vec<String>, lang: crate::i18n::Lang) {
    let t = Tokens::get(ui.ctx());
    for k in keys {
        let path = format!("{section}.{k}");
        let v = obj.get(&k).cloned().unwrap_or(Value::Null);
        let human = humanize(&k);
        let label = tl!(&human).to_string();
        match &v {
            Value::Bool(b) => {
                ui.label("");
                let mut b = *b;
                if read_at_launch(&path) {
                    ui.vertical(|ui| {
                        crate::widgets::checkbox(ui, &mut b, &label);
                        ui.label(RichText::new(tl!("Applies at next launch.")).color(t.text_dim));
                    });
                } else {
                    crate::widgets::checkbox(ui, &mut b, &label);
                }
                obj.insert(k, json!(b));
            }
            Value::String(s) if path == "interface.language" => {
                ui.label(RichText::new(tl!(&label)).color(t.text_dim));
                let mut pairs: Vec<(String, &str)> = vec![("auto".into(), tl!("Auto"))];
                pairs.extend(crate::i18n::Lang::for_picker().into_iter().map(|l| (l.code().to_string(), l.name())));
                let mut cur = s.clone();
                crate::widgets::dropdown(ui, &format!("pref-{path}"), tl!(&label), &mut cur, &pairs, 220.0);
                obj.insert(k, json!(cur));
            }
            Value::String(s) if prefs::choices(&path).is_some() => {
                ui.label(RichText::new(tl!(&label)).color(t.text_dim));
                let opts = prefs::choices(&path).unwrap_or(&[]);
                let labels: Vec<String> = opts.iter().map(|o| choice_label(o)).collect();
                let pairs: Vec<(String, &str)> = opts.iter().map(|o| o.to_string()).zip(labels.iter().map(String::as_str)).collect();
                let mut cur = s.clone();
                crate::widgets::dropdown(ui, &format!("pref-{path}"), tl!(&label), &mut cur, &pairs, 220.0);
                obj.insert(k, json!(cur));
            }
            Value::String(s) if prefs::is_color(&path) => {
                ui.label(RichText::new(tl!(&label)).color(t.text_dim));
                let c = prefs::parse_hex(s).unwrap_or([128, 128, 128]);
                let mut rgb = c;
                ui.horizontal(|ui| {
                    // PhotoCraft's Color Picker, as everywhere else (#2144).
                    if crate::widgets::color_swatch_button(ui, egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]), &label).clicked() {
                        ui.ctx().data_mut(|d| d.insert_temp(egui::Id::unique(PICK_ID), (path.clone(), label.clone())));
                    }
                    hex_field(ui, &path, &mut rgb);
                });
                obj.insert(k, json!(format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])));
                let _ = color_of(s);
            }
            Value::String(s) => {
                ui.label(RichText::new(tl!(&label)).color(t.text_dim));
                let mut s = s.clone();
                ui.add(egui::TextEdit::singleline(&mut s).desired_width(260.0)).accessible_name(tl!(&label));
                obj.insert(k, json!(s));
            }
            Value::Number(n) => {
                let value_label = ui.label(RichText::new(tl!(&label)).color(t.text_dim));
                let (lo, hi) = prefs::range(&path).unwrap_or((-1e9, 1e9));
                if n.is_u64() || n.is_i64() {
                    let mut x = n.as_i64().unwrap_or(0);
                    ui.add(egui::DragValue::new(&mut x).range(lo as i64..=hi as i64).custom_parser(crate::widgets::parse_num)).labelled_by(value_label.id);
                    obj.insert(k, json!(x));
                    // A row of its own, so the label stays level with its field.
                    if read_at_launch(&path) {
                        ui.end_row();
                        ui.label("");
                        ui.label(RichText::new(tl!("Applies at next launch.")).color(t.text_dim));
                    }
                } else {
                    let mut x = n.as_f64().unwrap_or(0.0);
                    ui.add(egui::DragValue::new(&mut x).range(lo..=hi).speed(0.1).max_decimals(3).custom_parser(crate::widgets::parse_num))
                        .labelled_by(value_label.id);
                    obj.insert(k, json!(x));
                }
            }
            Value::Array(_) if path == "tools.pressureCurve" => {
                ui.label(RichText::new(tl!(&label)).color(t.text_dim));
                if let Some(next) = crate::pressure_curve_ui::editor(ui, &v) {
                    obj.insert(k, next);
                }
            }
            Value::Array(items) if k == "disks" => {
                ui.label(RichText::new(tl!("Scratch disks")).color(t.text_dim));
                let mut items = items.clone();
                ui.vertical(|ui| {
                    for d in &mut items {
                        ui.horizontal(|ui| {
                            let mut on = d.get("enabled").and_then(Value::as_bool).unwrap_or(false);
                            let mut path = d.get("path").and_then(Value::as_str).unwrap_or("").to_string();
                            crate::widgets::checkbox(ui, &mut on, tl!("Enabled"));
                            ui.add(egui::TextEdit::singleline(&mut path).desired_width(240.0)).accessible_name(tl!("Path"));
                            *d = json!({"enabled": on, "path": path});
                        });
                    }
                    if ui.small_button(tl!("Add disk")).clicked() {
                        items.push(json!({"enabled": true, "path": ""}));
                    }
                });
                obj.insert(k, Value::Array(items));
            }
            Value::Array(items) => {
                ui.label(RichText::new(tl!(&label)).color(t.text_dim));
                ui.label(RichText::new(crate::i18n::trn(lang, items.len() as u64, "{n} item", "{n} items")).color(t.text_faint));
                if !items.is_empty() && ui.small_button(tl!("Clear")).clicked() {
                    obj.insert(k, json!([]));
                }
            }
            _ => continue,
        }
        ui.end_row();
    }
}

/// Keyboard Shortcuts and Menus: a searchable list of commands by menu with editable shortcuts
/// (click a shortcut, press keys; ⌫ removes it), menu visibility and colours, toolbar tools.
fn shortcuts_body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let t = Tokens::get(ui.ctx());
    let mut tab = f.get("tab").and_then(Value::as_u64).unwrap_or(0);
    ui.horizontal(|ui| {
        for (i, name) in [tl!("Keyboard Shortcuts"), tl!("Menus"), tl!("Toolbar")].iter().enumerate() {
            if crate::widgets::pill_tab(ui, name, tab == i as u64).clicked() {
                tab = i as u64;
                f.insert("capture".into(), json!(false));
            }
        }
    });
    f.insert("tab".into(), json!(tab));
    ui.add_space(6.0);
    if tab == 2 {
        toolbar_tab(ui, f);
        return;
    }
    // A malformed map (set over automation) is left as it is, so OK rejects it instead of
    // saving the empty map drawn in its place.
    let parsed = f.get("overrides").map(|v| serde_json::from_value::<BTreeMap<String, String>>(v.clone()));
    let overrides_ok = parsed.as_ref().is_none_or(Result::is_ok);
    let mut overrides = parsed.and_then(Result::ok).unwrap_or_default();
    // A command's further keys (#3665): id → keys, replacing its default alternates.
    let parsed_alts = f.get("alternates").map(|v| serde_json::from_value::<BTreeMap<String, Vec<String>>>(v.clone()));
    let alternates_ok = parsed_alts.as_ref().is_none_or(Result::is_ok);
    let mut alt_overrides = parsed_alts.and_then(Result::ok).unwrap_or_default();
    let mut capture_alt = f.get("captureAlt").and_then(Value::as_bool).unwrap_or(false);
    // A key just assigned that another command holds: Photoshop's Accept and Go to Conflict /
    // Undo Changes (an object: the key, the other command, and this one's keys before).
    let mut conflict: Option<Value> = f.get("conflict").filter(|c| c.is_object()).cloned();
    let mut hidden: Vec<String> = f.get("hidden").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
    let mut colors: BTreeMap<String, String> = f.get("colors").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
    let mut selected = f.get("selected").and_then(Value::as_str).unwrap_or("").to_string();
    let mut capture = f.get("capture").and_then(Value::as_bool).unwrap_or(false);
    let mut message = f.get("message").and_then(Value::as_str).unwrap_or("").to_string();
    let mut message_warning = f.get("messageWarning").and_then(Value::as_bool).unwrap_or(false);
    let items = shortcut_items(app);
    // The Set row, then the search (the Menus tab searches too).
    if tab == 0 {
        set_row(app, ui, f, &mut overrides, &mut alt_overrides, &items, &mut message, &mut message_warning);
        ui.add_space(4.0);
    }
    let mut filter = f.get("filter").and_then(Value::as_str).unwrap_or("").to_string();
    ui.horizontal(|ui| {
        ui.label(RichText::new(tl!("Search")).color(t.text_dim));
        ui.add(egui::TextEdit::singleline(&mut filter).desired_width(260.0).hint_text(tl!("command or shortcut")));
    });
    f.insert("filter".into(), json!(filter));
    // Menu path and command label as shown in the menus, in the UI language.
    let shown = |id: &str, label: &str, path: &[String]| -> (Vec<String>, String) {
        let lang = crate::i18n::current();
        (path.iter().map(|p| crate::i18n::tr(lang, p).to_string()).collect(), crate::i18n::tr_id(lang, id, label).to_string())
    };
    let eff = |overrides: &BTreeMap<String, String>, id: &str, def: &Option<String>| -> Option<String> {
        match overrides.get(id) {
            Some(s) if s.is_empty() => None,
            Some(s) => Some(s.clone()),
            None => def.clone(),
        }
    };
    // A command's further keys as the dialog stands: its edited list, else its defaults.
    let eff_alts = |alts: &BTreeMap<String, Vec<String>>, id: &str, main: Option<&str>| -> Vec<String> {
        let mut p = prefs::Preferences::default();
        if let Some(list) = alts.get(id) {
            p.alternate_shortcuts.insert(id.to_string(), list.clone());
        }
        p.alternates(id, main)
    };
    let mut capture_mods = capture_mods_field(f.get("captureMods"));
    // Key capture for the selected command: its main key, or (Add Shortcut) one more key.
    if capture && tab == 0 && !selected.is_empty() {
        let (next_mods, pressed) = ui.input(|i| capture_key_press(capture_mods, i));
        capture_mods = next_mods;
        if let Some((key, m)) = pressed {
            ui.input_mut(|i| {
                i.consume_key(m, key);
            });
            if key == egui::Key::Escape {
                capture = false;
                conflict = None;
            } else if matches!(key, egui::Key::Backspace | egui::Key::Delete) && m.is_none() {
                if capture_alt {
                    alt_overrides.insert(selected.clone(), Vec::new());
                } else {
                    overrides.insert(selected.clone(), String::new());
                }
                capture = false;
                message = tl!("Shortcut removed.").into();
                message_warning = false;
            } else if let Some(sc) = shortcut_text(key, m).filter(|sc| reserved_shortcut(sc)) {
                message = crate::i18n::fmt(tl!("{shortcut} is reserved by the system and can't be assigned."), &[("shortcut", &crate::shortcuts::pretty(&sc))]);
                message_warning = true;
                capture = false;
            } else if let Some(sc) = shortcut_text(key, m) {
                let clash_ids: Vec<&str> = items
                    .iter()
                    .filter(|(id, _, _, def)| {
                        let cur = eff(&overrides, id, def);
                        *id != selected
                            && (cur.as_deref().and_then(prefs::normalize_shortcut) == Some(sc.clone())
                                || eff_alts(&alt_overrides, id, cur.as_deref()).contains(&sc))
                    })
                    .map(|(id, ..)| id.as_str())
                    .collect();
                let clash: Vec<String> = items
                    .iter()
                    .filter(|(id, ..)| clash_ids.contains(&id.as_str()))
                    .map(|(id, label, path, _)| {
                        let (path, label) = shown(id, label, path);
                        format!("{} › {}", path.join(" › "), label.trim_end_matches('…'))
                    })
                    .collect();
                // What Undo Changes puts back: the command's main key and further keys as they were.
                if let Some(first) = clash_ids.first() {
                    conflict = Some(json!({
                        "shortcut": sc,
                        "with": first,
                        // Every command holding the key: Accept takes it from all of them.
                        "holders": clash_ids,
                        "command": selected,
                        "main": overrides.get(&selected),
                        "alternates": alt_overrides.get(&selected),
                    }));
                } else {
                    conflict = None;
                }
                if capture_alt {
                    let def = items.iter().find(|i| i.0 == selected).and_then(|i| i.3.clone());
                    let cur = eff(&overrides, &selected, &def);
                    let mut keys = eff_alts(&alt_overrides, &selected, cur.as_deref());
                    if cur.as_deref().and_then(prefs::normalize_shortcut) != Some(sc.clone()) && !keys.contains(&sc) && keys.len() < prefs::MAX_ALTERNATES {
                        keys.push(sc.clone());
                    }
                    alt_overrides.insert(selected.clone(), keys);
                } else {
                    overrides.insert(selected.clone(), sc.clone());
                }
                message_warning = !clash.is_empty();
                message = if clash.is_empty() {
                    let template = "{shortcut} assigned.";
                    crate::i18n::fmt(tl!(template), &[("shortcut", &crate::shortcuts::pretty(&sc))])
                } else {
                    let template = "{shortcut} is already in use by {commands} and will be removed from it when you click OK.";
                    crate::i18n::fmt(tl!(template), &[("shortcut", &crate::shortcuts::pretty(&sc)), ("commands", &clash.join(", "))])
                };
                capture = false;
            }
        }
    }
    let needle = filter.to_ascii_lowercase();
    egui::ScrollArea::vertical().max_height(420.0).id_salt("shortcuts-scroll").show(ui, |ui| {
        let mut last_top = String::new();
        egui::Grid::new("shortcut-grid").num_columns(3).spacing([12.0, 3.0]).striped(true).show(ui, |ui| {
            for (id, label, path, def) in &items {
                if tab == 1 && crate::tool_shortcuts::tool(id).is_some() {
                    continue;
                }
                let cur = eff(&overrides, id, def);
                let alts = eff_alts(&alt_overrides, id, cur.as_deref());
                let (shown_path, shown_label) = shown(id, label, path);
                // Match what is shown and the English name (commands are documented in English).
                let hay =
                    format!("{} {} {} {} {} {}", shown_path.join(" "), shown_label, path.join(" "), label, cur.clone().unwrap_or_default(), alts.join(" "))
                        .to_lowercase();
                if !needle.is_empty() && !hay.contains(&needle) && !id.to_ascii_lowercase().contains(&needle) {
                    continue;
                }
                if tab == 0 && path.is_empty() && def.is_none() && cur.is_none() && alts.is_empty() {
                    continue;
                }
                let top = shown_path.first().cloned().unwrap_or_else(|| tl!("Other").into());
                if top != last_top {
                    ui.label(RichText::new(&top).font(crate::theme::semibold(12.5)).color(t.text));
                    ui.label("");
                    ui.label("");
                    ui.end_row();
                    last_top = top;
                }
                let name = match shown_path.get(1..) {
                    Some(rest) if !rest.is_empty() => format!("{} › {}", rest.join(" › "), shown_label),
                    _ => shown_label,
                };
                let sel = selected == *id;
                let name_row = ui.selectable_label(sel, RichText::new(format!("   {name}")).color(t.text_dim));
                if name_row.clicked() {
                    selected = id.clone();
                }
                if tab == 0 {
                    let text = if sel && capture && !capture_alt {
                        tl!("Press keys…").to_string()
                    } else {
                        cur.as_deref().map(crate::shortcuts::pretty).unwrap_or_default()
                    };
                    let changed = overrides.contains_key(id) || alt_overrides.contains_key(id);
                    let r = ui.add(
                        egui::Button::new(RichText::new(tl!(&text)).size(12.0).color(if changed { t.accent_text } else { t.text })).min_size(vec2(120.0, 18.0)),
                    );
                    // An unassigned shortcut shows no text: name it by the command it binds.
                    let r = if text.is_empty() { r.labelled_by(name_row.id) } else { r };
                    if r.clicked() {
                        selected = id.clone();
                        capture = true;
                        capture_alt = false;
                        message.clear();
                        message_warning = false;
                    }
                    // The further keys (F3 beside ⌘C), each removable on its own, as Photoshop
                    // lists them as rows of their own (#3665).
                    ui.horizontal(|ui| {
                        for k in &alts {
                            let b = egui::Button::new(RichText::new(format!("{}  ×", crate::shortcuts::pretty(k))).size(11.0).color(t.text)).small();
                            if ui.add(b).on_hover_text(tl!("Delete Shortcut")).clicked() {
                                alt_overrides.insert(id.clone(), alts.iter().filter(|x| *x != k).cloned().collect());
                                selected = id.clone();
                                message = tl!("Shortcut removed.").into();
                                message_warning = false;
                            }
                        }
                        if sel && capture && capture_alt {
                            ui.label(RichText::new(tl!("Press keys…")).size(11.0).color(t.accent_text));
                        }
                        if changed {
                            ui.label(RichText::new(tl!("modified")).color(t.text_faint).size(10.5));
                        }
                    });
                } else {
                    // Edit › Menus, Keyboard Shortcuts, Preferences, Save and Exit always stay (#5096).
                    let hideable = photocraft_engine::prefs::MenuCustomization::hideable(id);
                    let mut visible = !hideable || !hidden.contains(id);
                    ui.add_enabled_ui(hideable, |ui| crate::widgets::checkbox(ui, &mut visible, tl!("Visible")));
                    if visible {
                        hidden.retain(|h| h != id);
                    } else if !hidden.contains(id) {
                        hidden.push(id.clone());
                    }
                    let mut col = colors.get(id).cloned().unwrap_or_else(|| "none".into());
                    let opts: Vec<(String, &str)> =
                        ["none", "red", "orange", "yellow", "green", "blue", "violet", "gray"].iter().map(|c| (c.to_string(), *c)).collect();
                    crate::widgets::dropdown(ui, &format!("menu-color-{id}"), tl!("Color"), &mut col, &opts, 90.0);
                    if col == "none" {
                        colors.remove(id);
                    } else {
                        colors.insert(id.clone(), col);
                    }
                }
                ui.end_row();
            }
        });
    });
    ui.add_space(6.0);
    if tab == 0 {
        ui.horizontal(|ui| {
            let has_sel = !selected.is_empty();
            if ui.add_enabled(has_sel, egui::Button::new(tl!("Add Shortcut"))).clicked() {
                capture = true;
                capture_alt = true;
                message.clear();
                message_warning = false;
            }
            if ui.add_enabled(has_sel, egui::Button::new(tl!("Use Default"))).clicked() {
                overrides.remove(&selected);
                alt_overrides.remove(&selected);
                message = tl!("Default restored.").into();
                message_warning = false;
            }
            if ui.add_enabled(has_sel, egui::Button::new(tl!("Delete Shortcut"))).clicked() {
                overrides.insert(selected.clone(), String::new());
                message = tl!("Shortcut removed.").into();
                message_warning = false;
            }
            if ui.button(tl!("Reset All to Defaults")).clicked() {
                overrides.clear();
                alt_overrides.clear();
                message = tl!("All shortcuts reset to PhotoCraft defaults.").into();
                message_warning = false;
            }
            // A Photoshop set (`.kys`) fills the overrides; the file arrives after this frame
            // and `kys_import` writes the dialog's fields then (#1163).
            if ui.button(tl!("Import Shortcuts…")).on_hover_text(tl!("Load a keyboard shortcut set saved as .kys")).clicked()
                && let Err(e) = crate::kys_import::import_dialog(app)
            {
                message = e;
                message_warning = false;
            }
        });
    } else if ui.button(tl!("Show All Menu Items")).clicked() {
        hidden.clear();
        colors.clear();
    }
    if !message.is_empty() {
        ui.label(RichText::new(&message).color(if message_warning { t.warning } else { t.text_dim }));
    }
    // The conflict describes the bindings as the capture left them: any later change (Use
    // Default, Delete, ×, Reset All, another command or set) makes it stale, and it goes.
    let state = conflict_state(&overrides, &alt_overrides, &selected, f.get("activeSet"));
    if conflict.as_ref().and_then(|c| c.get("state")).is_some_and(|s| s.as_u64() != Some(state)) {
        conflict = None;
    }
    if tab == 0
        && let Some(c) = conflict.clone()
    {
        let (key, with, command) =
            (c["shortcut"].as_str().unwrap_or_default(), c["with"].as_str().unwrap_or_default(), c["command"].as_str().unwrap_or_default());
        ui.horizontal(|ui| {
            if ui.button(tl!("Accept and Go to Conflict")).on_hover_text(tl!("Take the shortcut from the other command and select it")).clicked() {
                let holders: Vec<String> = serde_json::from_value(c["holders"].clone()).unwrap_or_else(|_| vec![with.to_string()]);
                for holder in &holders {
                    let def = items.iter().find(|i| i.0 == *holder).and_then(|i| i.3.clone());
                    let cur = eff(&overrides, holder, &def);
                    if cur.as_deref().and_then(prefs::normalize_shortcut).as_deref() == Some(key) {
                        overrides.insert(holder.clone(), String::new());
                    }
                    let keys = eff_alts(&alt_overrides, holder, eff(&overrides, holder, &def).as_deref());
                    if keys.iter().any(|k| k == key) {
                        alt_overrides.insert(holder.clone(), keys.into_iter().filter(|k| k != key).collect());
                    }
                }
                selected = with.to_string();
                filter.clear();
                message = tl!("Choose a new shortcut for this command, or leave it without one.").into();
                message_warning = false;
                conflict = None;
            }
            if ui.button(tl!("Undo Changes")).on_hover_text(tl!("Put this command's shortcuts back as they were")).clicked() {
                match c["main"].as_str() {
                    Some(m) => overrides.insert(command.to_string(), m.to_string()),
                    None => overrides.remove(command),
                };
                match serde_json::from_value::<Vec<String>>(c["alternates"].clone()) {
                    Ok(list) => alt_overrides.insert(command.to_string(), list),
                    Err(_) => alt_overrides.remove(command),
                };
                selected = command.to_string();
                message = tl!("Changes undone.").into();
                message_warning = false;
                conflict = None;
            }
        });
    }
    if overrides_ok {
        f.insert("overrides".into(), json!(overrides));
    }
    if alternates_ok {
        f.insert("alternates".into(), json!(alt_overrides));
    }
    f.insert("captureAlt".into(), json!(capture && capture_alt));
    // A conflict just found is stamped with the bindings it describes.
    if let Some(c) = conflict.as_mut().filter(|c| c.get("state").is_none()) {
        c["state"] = json!(conflict_state(&overrides, &alt_overrides, &selected, f.get("activeSet")));
    }
    f.insert("conflict".into(), conflict.unwrap_or(Value::Null));
    f.insert("hidden".into(), json!(hidden));
    f.insert("colors".into(), json!(colors));
    f.insert("selected".into(), json!(selected));
    f.insert("capture".into(), json!(capture));
    if capture {
        f.insert("captureMods".into(), capture_mods_json(capture_mods));
    } else {
        f.remove("captureMods");
    }
    f.insert("message".into(), json!(message));
    f.insert("messageWarning".into(), json!(message_warning));
}

/// A command's keys as the dialog stands, main key first: the edited main key (or its default)
/// and its further keys.
fn dialog_keys(overrides: &BTreeMap<String, String>, alternates: &BTreeMap<String, Vec<String>>, id: &str, default: Option<&str>) -> Vec<String> {
    let main = match overrides.get(id) {
        Some(s) if s.is_empty() => None,
        Some(s) => Some(s.as_str()),
        None => default,
    };
    let mut p = prefs::Preferences::default();
    if let Some(list) = alternates.get(id) {
        p.alternate_shortcuts.insert(id.to_string(), list.clone());
    }
    let mut keys: Vec<String> = main.and_then(prefs::normalize_shortcut).into_iter().collect();
    keys.extend(p.alternates(id, main));
    keys
}

/// Edit › Keyboard Shortcuts › Set, as in Photoshop: choose a saved set (its bindings replace
/// the dialog's), save the bindings as a new set or over the one in use, delete it, export it as
/// a `.kys` file or summarize it as an HTML page. Everything but Export and Summarize waits for
/// OK, like the rest of the dialog.
#[allow(clippy::too_many_arguments)]
fn set_row(
    app: &mut PhotocraftApp,
    ui: &mut egui::Ui,
    f: &mut Map<String, Value>,
    overrides: &mut BTreeMap<String, String>,
    alternates: &mut BTreeMap<String, Vec<String>>,
    items: &[(String, String, Vec<String>, Option<String>)],
    message: &mut String,
    warning: &mut bool,
) {
    use photocraft_engine::shortcut_sets::{self, ShortcutSet};
    let mut sets: BTreeMap<String, ShortcutSet> = f.get("sets").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
    let mut active = f.get("activeSet").and_then(Value::as_str).unwrap_or("").to_string();
    if !active.is_empty() && !sets.contains_key(&active) {
        active.clear();
    }
    // The engine decides what a set holds and when it is modified (not the actions' F-keys).
    let now = ShortcutSet::from_bindings(overrides, alternates);
    let modified = shortcut_sets::differs(&sets, &active, overrides, alternates);
    let defaults = tl!("PhotoCraft Defaults").to_string();
    let shown = |name: &str| -> String {
        let base = if name.is_empty() { defaults.clone() } else { name.to_string() };
        if name == active && modified { crate::i18n::fmt(tl!("{set} (modified)"), &[("set", &base)]) } else { base }
    };
    let names: Vec<(String, String)> = std::iter::once(String::new()).chain(sets.keys().cloned()).map(|n| (n.clone(), shown(&n))).collect();
    let mut new_name: Option<String> = f.get("newSetName").and_then(Value::as_str).map(str::to_string);
    let set_label = if active.is_empty() { defaults.clone() } else { active.clone() };
    let mut export = false;
    let mut summarize = false;
    ui.horizontal(|ui| {
        ui.label(tl!("Set:"));
        let mut chosen = active.clone();
        let pairs: Vec<(String, &str)> = names.iter().map(|(n, l)| (n.clone(), l.as_str())).collect();
        if crate::widgets::dropdown(ui, "shortcut-set", tl!("Set:"), &mut chosen, &pairs, 220.0) && chosen != active {
            // As `useSet`: the set's bindings, keeping the actions' F-keys.
            sets.get(&chosen).cloned().unwrap_or_default().apply(overrides, alternates);
            active = chosen;
            message.clear();
        }
        if ui.button(tl!("New Set…")).on_hover_text(tl!("Save these shortcuts as a new set")).clicked() {
            new_name = Some(String::new());
        }
        if ui.add_enabled(!active.is_empty() && modified, egui::Button::new(tl!("Save"))).on_hover_text(tl!("Save the changes to this set")).clicked() {
            sets.insert(active.clone(), now.clone());
            *message = tl!("Set saved.").into();
            *warning = false;
        }
        if ui.add_enabled(!active.is_empty(), egui::Button::new(tl!("Delete"))).on_hover_text(tl!("Delete this set")).clicked() {
            sets.remove(&active);
            active.clear();
            *message = tl!("Set deleted.").into();
            *warning = false;
        }
        export = ui.button(tl!("Export…")).on_hover_text(tl!("Save these shortcuts as a .kys file")).clicked();
        summarize = ui.button(tl!("Summarize…")).on_hover_text(tl!("Save a list of these shortcuts as a web page")).clicked();
    });
    if let Some(name) = new_name.as_mut() {
        let mut create = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            ui.label(tl!("Name:"));
            let r = ui.add(egui::TextEdit::singleline(name).desired_width(220.0).hint_text(tl!("set name")));
            create = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            create |= ui.button(tl!("Create")).clicked();
            cancel = ui.button(tl!("Cancel")).clicked();
        });
        if create {
            match shortcut_sets::set_name(name) {
                Ok(n) if !sets.contains_key(&n) && sets.len() >= shortcut_sets::MAX_SETS => {
                    *message = tl!("There are too many sets: delete one first.").into();
                    *warning = true;
                }
                Ok(n) => {
                    sets.insert(n.clone(), now.clone());
                    active = n;
                    new_name = None;
                    *message = tl!("Set saved.").into();
                    *warning = false;
                }
                Err(e) => {
                    *message = e;
                    *warning = true;
                }
            }
        } else if cancel {
            new_name = None;
        }
    }
    if export || summarize {
        let result = if export {
            let entries: Vec<photocraft_engine::kys::Entry> = items
                .iter()
                .map(|(id, label, _, def)| photocraft_engine::kys::Entry {
                    id: id.clone(),
                    label: label.clone(),
                    tool: id.starts_with("tools.select."),
                    keys: dialog_keys(overrides, alternates, id, def.as_deref()),
                })
                .collect();
            let text = photocraft_engine::kys::write(&set_label, &entries);
            save_text(app, &format!("{set_label}.kys"), text)
        } else {
            let mut sections: Vec<shortcut_sets::Section> = Vec::new();
            for (id, label, path, def) in items {
                let section = path.first().cloned().unwrap_or_else(|| tl!("Other").to_string());
                let name = path.iter().skip(1).chain(std::iter::once(label)).cloned().collect::<Vec<_>>().join(" › ");
                let row = (name, dialog_keys(overrides, alternates, id, def.as_deref()));
                match sections.iter_mut().find(|(s, _)| *s == section) {
                    Some((_, rows)) => rows.push(row),
                    None => sections.push((section, vec![row])),
                }
            }
            let html = shortcut_sets::summarize(&set_label, &sections, cfg!(target_os = "macos"));
            save_text(app, &format!("{set_label}.html"), html)
        };
        if let Err(e) = result {
            *message = e;
            *warning = true;
        }
    }
    f.insert("sets".into(), json!(sets));
    f.insert("activeSet".into(), json!(active));
    f.insert("newSetName".into(), new_name.map_or(Value::Null, Value::String));
}

/// Save `text` through the save dialog under the suggested file name.
fn save_text(app: &mut PhotocraftApp, suggested: &str, text: String) -> Result<Value, String> {
    // A set name can hold characters a file name can't.
    let name: String = suggested.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c }).collect();
    app.pick_save(&name, move |app, path| {
        let write = app.services.write.as_mut().ok_or("no writer configured")?;
        write(&path, text.as_bytes())?;
        Ok(json!({"path": path}))
    })
}

fn toolbar_tab(ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let mut hidden: Vec<String> = f.get("toolbarHidden").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
    egui::ScrollArea::vertical().max_height(380.0).id_salt("toolbar-scroll").show(ui, |ui| {
        egui::Grid::new("toolbar-grid").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            for tool in crate::state::Tool::ALL {
                let name = format!("{tool:?}");
                let mut on = !hidden.contains(&name);
                crate::widgets::checkbox(ui, &mut on, tl!(tool.label()));
                if on {
                    hidden.retain(|h| *h != name);
                } else if !hidden.contains(&name) {
                    hidden.push(name);
                }
                let k = tool.key();
                ui.label(if k == '\0' { String::new() } else { k.to_string() });
                ui.end_row();
            }
        });
    });
    if ui.button(tl!("Restore Defaults")).clicked() {
        hidden.clear();
    }
    f.insert("toolbarHidden".into(), json!(hidden));
}

fn presets_body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let t = Tokens::get(ui.ctx());
    let mut kind = f.get("kind").and_then(Value::as_str).unwrap_or("brushes").to_string();
    ui.horizontal(|ui| {
        ui.label(RichText::new(tl!("Preset Type")).color(t.text_dim));
        let opts = [("brushes".to_string(), tl!("Brushes")), ("customShapes".to_string(), tl!("Custom Shapes")), ("patterns".to_string(), tl!("Patterns"))];
        crate::widgets::dropdown(ui, "preset-kind", tl!("Type"), &mut kind, &opts, 180.0);
    });
    let list = app
        .session
        .execute("edit.presets.presetManager", json!({"kind": kind}))
        .ok()
        .and_then(|v| v.get(&kind).cloned())
        .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
        .unwrap_or_default();
    let mut selected = f.get("selected").and_then(Value::as_u64).unwrap_or(0) as usize;
    egui::ScrollArea::vertical().max_height(260.0).id_salt("preset-list").show(ui, |ui| {
        for (i, name) in list.iter().enumerate() {
            if ui.selectable_label(i == selected, name).clicked() {
                selected = i;
                f.insert("newName".into(), json!(name));
            }
        }
        if list.is_empty() {
            ui.label(RichText::new(tl!("No presets of this type.")).color(t.text_faint));
        }
    });
    let mut new_name = f.get("newName").and_then(Value::as_str).unwrap_or("").to_string();
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut new_name).desired_width(200.0)).accessible_name(tl!("Name"));
        if ui.add_enabled(selected < list.len() && !new_name.trim().is_empty(), egui::Button::new(tl!("Rename"))).clicked() {
            let _ = app.run("edit.presets.presetManager", json!({"action": "rename", "kind": kind, "index": selected, "newName": new_name}));
        }
        if ui.add_enabled(selected < list.len(), egui::Button::new(tl!("Delete"))).clicked() {
            let _ = app.run("edit.presets.presetManager", json!({"action": "delete", "kind": kind, "index": selected}));
        }
        // Load Photoshop brushes (.abr) into the library.
        if kind == "brushes" && ui.button(tl!("Load…")).on_hover_text(tl!("Import brushes (.abr)")).clicked() {
            let _ = app.open_dialog_file();
        }
    });
    f.insert("kind".into(), json!(kind));
    f.insert("selected".into(), json!(selected));
    f.insert("newName".into(), json!(new_name));
}

fn presets_io_body(ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let t = Tokens::get(ui.ctx());
    let mut action = f.get("action").and_then(Value::as_str).unwrap_or("export").to_string();
    ui.horizontal(|ui| {
        ui.label(RichText::new(tl!("Action")).color(t.text_dim));
        crate::widgets::dropdown(
            ui,
            "presets-io",
            tl!("Action"),
            &mut action,
            &[("export".to_string(), tl!("Export Presets")), ("import".to_string(), tl!("Import Presets"))],
            180.0,
        );
    });
    for (k, label) in [("brushes", tl!("Brushes")), ("customShapes", tl!("Custom Shapes")), ("tools", tl!("Tool Presets"))] {
        let mut on = f.get(k).and_then(Value::as_bool).unwrap_or(true);
        crate::widgets::checkbox(ui, &mut on, label);
        f.insert(k.into(), json!(on));
    }
    f.insert("action".into(), json!(action));
}

fn mismatch_body(ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let t = Tokens::get(ui.ctx());
    ui.label(RichText::new(f.get("message").and_then(Value::as_str).unwrap_or("")).color(t.text));
    ui.add_space(6.0);
    let mut action = f.get("action").and_then(Value::as_str).unwrap_or("preserve").to_string();
    let missing = f.get("missing").and_then(Value::as_bool).unwrap_or(false);
    let name = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("?").to_string();
    let opts: Vec<(&str, String)> = if missing {
        vec![("preserve", tl!("Leave as is (don't color manage)").into()), ("assignWorking", tl!("Assign working space").into())]
    } else {
        vec![
            ("preserve", crate::i18n::fmt(tl!("Use the embedded profile ({profile})"), &[("profile", &name("embedded"))])),
            ("convert", crate::i18n::fmt(tl!("Convert to the working space ({profile})"), &[("profile", &name("working"))])),
            ("discard", tl!("Discard the embedded profile (don't color manage)").into()),
        ]
    };
    for (v, label) in opts {
        ui.radio_value(&mut action, v.to_string(), label);
    }
    f.insert("action".into(), json!(action));
    // Remembering sets the Color Settings policy and stops asking, so it needs a mode with a policy.
    if !missing && f.get("policyKey").is_some_and(Value::is_string) {
        ui.add_space(8.0);
        let mut remember = f.get("remember").and_then(Value::as_bool).unwrap_or(false);
        crate::widgets::checkbox(ui, &mut remember, tl!("Remember this choice for future images"))
            .on_hover_text(tl!("Turn the prompt back on with Ask On Mismatch in Edit › Color Settings."));
        f.insert("remember".into(), json!(remember));
    }
}

// ------------------------------------------------------------------ confirm

/// OK on one of our dialogs.
pub fn confirm(app: &mut PhotocraftApp, f: &Map<String, Value>) -> Result<Value, String> {
    match f.get("__prefsui").and_then(Value::as_str).unwrap_or("") {
        "prefs" => {
            let values = f.get("values").cloned().unwrap_or(Value::Null);
            app.run("prefs.set", json!({"path": "", "value": values}))
        }
        "shortcuts" => {
            let overrides = f.get("overrides").cloned().unwrap_or(json!({}));
            // Shortcuts moved to another command are taken from their old owner (shell
            // commands included, which the engine doesn't know).
            // The map replaces every stored override, so a malformed one is an error, not "none".
            let mut ov: BTreeMap<String, String> =
                serde_json::from_value(overrides).map_err(|e| format!("overrides must map command IDs to shortcut strings: {e}"))?;
            let alternates = f.get("alternates").cloned().unwrap_or(json!({}));
            let mut alt: BTreeMap<String, Vec<String>> =
                serde_json::from_value(alternates).map_err(|e| format!("alternates must map command IDs to lists of shortcuts: {e}"))?;
            let items = shortcut_items(app);
            // Newly assigned shortcuts are checked first and take theirs from any holder, custom
            // overrides included; unchanged overrides only take theirs from defaults. A
            // command's further keys (#3665) take and lose keys the same way.
            let before = &app.session.prefs().shortcuts;
            let before_alt = &app.session.prefs().alternate_shortcuts;
            let mut todo: Vec<(String, String, bool, bool)> =
                ov.iter().filter(|(_, s)| !s.is_empty()).map(|(k, s)| (k.clone(), s.clone(), before.get(k) != Some(s), false)).collect();
            for (k, keys) in &alt {
                let new = before_alt.get(k) != Some(keys);
                todo.extend(keys.iter().map(|s| (k.clone(), s.clone(), new, true)));
            }
            todo.sort_by_key(|(_, _, new, _)| !new);
            let main_of = |ov: &BTreeMap<String, String>, id: &str, def: Option<&str>| -> Option<String> {
                match ov.get(id) {
                    Some(s) if s.is_empty() => None,
                    Some(s) => Some(s.clone()),
                    None => def.map(str::to_string),
                }
            };
            for (id, sc, new, is_alt) in todo {
                let norm = prefs::normalize_shortcut(&sc);
                let Some(norm) = norm else { continue };
                // Taken by a newer assignment?
                if (!is_alt && ov.get(&id).is_some_and(String::is_empty)) || (is_alt && alt.get(&id).is_some_and(|k| !k.contains(&norm))) {
                    continue;
                }
                for (other, _, _, def) in &items {
                    if *other == id {
                        continue;
                    }
                    let held = match ov.get(other) {
                        Some(s) if new => Some(s.as_str()),
                        Some(_) => None,
                        None => def.as_deref(),
                    };
                    if held.and_then(prefs::normalize_shortcut).as_deref() == Some(norm.as_str()) {
                        ov.insert(other.clone(), String::new());
                    }
                    // Its further keys: the edited list when this assignment is new, else only
                    // the defaults.
                    if alt.contains_key(other) && !new {
                        continue;
                    }
                    let mut p = prefs::Preferences::default();
                    if let Some(list) = alt.get(other) {
                        p.alternate_shortcuts.insert(other.clone(), list.clone());
                    }
                    let keys = p.alternates(other, main_of(&ov, other, def.as_deref()).as_deref());
                    if keys.contains(&norm) {
                        alt.insert(other.clone(), keys.into_iter().filter(|k| *k != norm).collect());
                    }
                }
            }
            let hidden = f.get("hidden").cloned().unwrap_or(json!([]));
            let colors = f.get("colors").cloned().unwrap_or(json!({}));
            let toolbar = f.get("toolbarHidden").cloned().unwrap_or(json!([]));
            let sets = f.get("sets").cloned().unwrap_or(json!({}));
            let active = f.get("activeSet").cloned().unwrap_or(json!(""));
            app.run(
                "edit.keyboardShortcuts",
                json!({"reset": true, "set": ov, "alternates": alt, "allowUnknown": true, "removeConflicts": false, "sets": sets, "activeSet": active}),
            )?;
            app.run("prefs.set", json!({"values": {"menus": {"hidden": hidden, "colors": colors}, "toolbar": {"hidden": toolbar}}}))
        }
        "presets" => Ok(Value::Null),
        "presetsIO" => {
            let kinds: Vec<&str> = ["brushes", "customShapes", "tools"].into_iter().filter(|k| f.get(*k).and_then(Value::as_bool).unwrap_or(true)).collect();
            if f.get("action").and_then(Value::as_str) == Some("import") {
                app.pick_file_bytes(move |app, name, bytes| {
                    let text = String::from_utf8(bytes).map_err(|_| format!("{name} is not a preset file"))?;
                    app.run("edit.presets.exportImportPresets", json!({"action": "import", "kinds": kinds, "data": text}))
                })
            } else {
                let out = app.run("edit.presets.exportImportPresets", json!({"action": "export", "kinds": kinds}))?;
                let text = serde_json::to_string_pretty(&out["data"]).map_err(|e| e.to_string())?;
                app.pick_save("Presets.pcpresets", move |app, path| {
                    let write = app.services.write.as_mut().ok_or("no writer configured")?;
                    write(&path, text.as_bytes())?;
                    let template = "Exported presets to {path}";
                    app.ui.status = crate::i18n::fmt(tl!(template), &[("path", &path)]);
                    Ok(json!({"path": path}))
                })
            }
        }
        // The file it asks about opens with this answer (`open_prompt`).
        "mismatch" => {
            let action = f.get("action").and_then(Value::as_str).unwrap_or("preserve");
            if f.get("remember").and_then(Value::as_bool) == Some(true) {
                return crate::open_prompt::answer_remembering(app, action);
            }
            crate::open_prompt::answer(app, Some(action))
        }
        _ => Err("unknown dialog".into()),
    }
}

// Test modules stay after the production code: the prefs_usage test reads each file up to
// its first `#[cfg(test)] mod`.
#[cfg(test)]
mod recovery_tests;

#[cfg(test)]
#[path = "shortcut_capture_tests.rs"]
mod shortcut_capture_tests;
#[cfg(test)]
#[path = "shortcut_set_tests.rs"]
mod shortcut_set_tests;

#[cfg(test)]
mod tests;
