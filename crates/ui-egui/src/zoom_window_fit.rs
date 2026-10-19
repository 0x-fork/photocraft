//! Preferences › General › Zoom Resizes Windows, which is also the Zoom tool's Resize Windows to
//! Fit (#204).
//!
//! As in Photoshop the two are one setting, so turning it on in either place shows it on in the
//! other. With it on, zooming a document that floats in its own window resizes that window to wrap
//! the image at the new zoom, no bigger than the screen it is on; with it off (the default) the
//! image zooms inside a window that keeps its size. Tabbed documents never resize the main window.
//!
//! Only a change of zoom resizes. Fit on Screen derives the zoom from the window, so a fit leaves
//! the window alone and the two can never chase each other.

use egui::{Context, Id, Rect, Vec2, ViewportClass, ViewportCommand, ViewportId, vec2};

use crate::PhotocraftApp;
use crate::state::{DocWindow, View};

/// The smallest window a zoom asks for (points), so zooming far out keeps the window usable.
pub const MIN_SIZE: Vec2 = vec2(240.0, 160.0);

/// Room left on the screen for the window's title bar and the taskbar or Dock (points).
pub const SCREEN_MARGIN: f32 = 48.0;

/// What wrapping a document window needs besides the view itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    /// The window's device pixels per egui point (`View::zoom` is in device pixels).
    pub ppp: f32,
    /// View › Pixel Aspect Ratio Correction: document pixels are shown this much wider.
    pub aspect: f32,
    /// The window's own chrome around the image, the rulers, in points.
    pub chrome: Vec2,
    /// The size of the screen the window is on, in points, when the platform reports it.
    pub monitor: Option<Vec2>,
}

/// The inner size (points) of a window that wraps `view`'s document at its zoom and rotation,
/// at least [`MIN_SIZE`] but never bigger than the screen, even a screen smaller than that
/// minimum. `None` until the view has a document size.
pub fn wrapped_size(view: &View, layout: &Layout) -> Option<Vec2> {
    let usable = |x: f32| x.is_finite() && x > 0.0;
    if !usable(view.zoom) || !usable(layout.ppp) {
        return None;
    }
    let aspect = if usable(layout.aspect) { layout.aspect } else { 1.0 };
    let w = view.doc_size[0] as f32 * aspect * view.zoom / layout.ppp;
    let h = view.doc_size[1] as f32 * view.zoom / layout.ppp;
    if !usable(w) || !usable(h) {
        return None;
    }
    // A turned view (View › Rotate View) shows the image's bounding box.
    let (sin, cos) = view.rotation.to_radians().sin_cos();
    let (sin, cos) = (sin.abs(), cos.abs());
    let image = vec2(w * cos + h * sin, w * sin + h * cos);
    let chrome = if layout.chrome.is_finite() { layout.chrome.max(Vec2::ZERO) } else { Vec2::ZERO };
    // The screen is the hard cap: applied last, it wins over the minimum on a tiny screen.
    let screen =
        layout.monitor.filter(|m| usable(m.x) && usable(m.y)).map_or(Vec2::splat(f32::INFINITY), |m| (m - Vec2::splat(SCREEN_MARGIN)).max(Vec2::splat(1.0)));
    Some((image + chrome).max(MIN_SIZE).min(screen))
}

/// Where the zoom a window was last wrapped for (or first seen at) is kept between frames.
fn last_zoom_id(window: u64) -> Id {
    Id::new(("pc-zoom-resizes-windows", window))
}

/// The inner size floating window `window` should take this frame, if any: when `on` (the
/// preference) and its zoom moved from `before` (its view as the frame began) to `after` (the view
/// its canvas left), other than by a fit. Every call records the zoom, so the first frame of a new
/// window and the frames with the preference off never resize later.
pub fn resize_for(ctx: &Context, window: u64, on: bool, before: &View, after: &View, layout: &Layout) -> Option<Vec2> {
    let id = last_zoom_id(window);
    let last = ctx.data_mut(|d| {
        let last = d.get_temp::<f32>(id);
        d.insert_temp(id, after.zoom);
        last
    });
    let fitting = before.fit_pending || before.fill_pending || after.fit_pending || after.fill_pending;
    // Compare the bits: any change of zoom counts, and NaN never matches itself into a resize loop.
    if !on || fitting || last?.to_bits() == after.zoom.to_bits() {
        return None;
    }
    wrapped_size(after, layout)
}

/// Called by a floating document window (`canvas::extra_windows`) after its canvas drew in `rect`:
/// with Zoom Resizes Windows on, a zoom resizes the window around the image. A window drawn
/// embedded in the main one (a backend without multiple windows, the web) is never resized, as the
/// command would resize the main window instead. Returns the size asked for.
pub fn follow(app: &PhotocraftApp, ctx: &Context, class: ViewportClass, viewport: ViewportId, window: &DocWindow, after: &View, rect: Rect) -> Option<Vec2> {
    if class != ViewportClass::Immediate {
        return None;
    }
    let layout = Layout {
        ppp: ctx.pixels_per_point(),
        aspect: app.ui.view.display_aspect(),
        chrome: rect.size() - crate::rulers::content_rect(app, rect).size(),
        monitor: ctx.input(|i| i.viewport().monitor_size),
    };
    let size = resize_for(ctx, window.id, app.session.prefs().general.zoom_resizes_windows, &window.view, after, &layout)?;
    ctx.send_viewport_cmd_to(viewport, ViewportCommand::InnerSize(size));
    Some(size)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn view(doc_size: [u32; 2], zoom: f32) -> View {
        View { zoom, doc_size, fit_pending: false, ..View::default() }
    }

    fn layout() -> Layout {
        Layout { ppp: 1.0, aspect: 1.0, chrome: Vec2::ZERO, monitor: Some(vec2(1920.0, 1080.0)) }
    }

    #[test]
    fn the_window_wraps_the_image_at_its_zoom() {
        assert_eq!(wrapped_size(&view([400, 300], 1.0), &layout()), Some(vec2(400.0, 300.0)));
        assert_eq!(wrapped_size(&view([400, 300], 2.0), &layout()), Some(vec2(800.0, 600.0)));
        // `zoom` is device pixels: on a 2× display 100% is half as many points.
        assert_eq!(wrapped_size(&view([800, 600], 1.0), &Layout { ppp: 2.0, ..layout() }), Some(vec2(400.0, 300.0)));
        // The rulers sit inside the window, around the image.
        assert_eq!(wrapped_size(&view([400, 300], 1.0), &Layout { chrome: vec2(16.0, 16.0), ..layout() }), Some(vec2(416.0, 316.0)));
        // Pixel Aspect Ratio Correction shows the pixels wider.
        assert_eq!(wrapped_size(&view([400, 300], 1.0), &Layout { aspect: 2.0, ..layout() }), Some(vec2(800.0, 300.0)));
    }

    #[test]
    fn the_window_stays_between_the_minimum_and_the_screen() {
        assert_eq!(wrapped_size(&view([400, 300], 0.05), &layout()), Some(MIN_SIZE));
        let screen = vec2(1920.0, 1080.0) - Vec2::splat(SCREEN_MARGIN);
        assert_eq!(wrapped_size(&view([400, 300], 16.0), &layout()), Some(screen));
        // One axis can be clamped while the other still wraps.
        assert_eq!(wrapped_size(&view([4000, 300], 1.0), &layout()), Some(vec2(screen.x, 300.0)));
        // Without a reported screen only the minimum applies.
        assert_eq!(wrapped_size(&view([400, 300], 16.0), &Layout { monitor: None, ..layout() }), Some(vec2(6400.0, 4800.0)));
        // A nonsense screen size is ignored rather than shrinking the window to nothing.
        assert_eq!(wrapped_size(&view([400, 300], 1.0), &Layout { monitor: Some(vec2(0.0, f32::NAN)), ..layout() }), Some(vec2(400.0, 300.0)));
        // A screen smaller than the minimum still caps the window: it never outgrows the screen.
        let tiny = Layout { monitor: Some(vec2(260.0, 190.0)), ..layout() };
        assert_eq!(wrapped_size(&view([400, 300], 1.0), &tiny), Some(vec2(260.0, 190.0) - Vec2::splat(SCREEN_MARGIN)));
        assert_eq!(wrapped_size(&view([400, 300], 0.05), &tiny), Some(vec2(260.0, 190.0) - Vec2::splat(SCREEN_MARGIN)));
    }

    #[test]
    fn a_turned_view_wraps_the_bounding_box() {
        let turned = View { rotation: 90.0, ..view([400, 300], 1.0) };
        let size = wrapped_size(&turned, &layout()).unwrap();
        assert!((size - vec2(300.0, 400.0)).length() < 0.01, "{size:?}");
        let turned = View { rotation: 45.0, ..view([400, 400], 1.0) };
        let size = wrapped_size(&turned, &layout()).unwrap();
        assert!((size.x - 400.0 * std::f32::consts::SQRT_2).abs() < 0.01, "{size:?}");
    }

    #[test]
    fn nothing_to_wrap_asks_for_nothing() {
        assert_eq!(wrapped_size(&view([0, 0], 1.0), &layout()), None, "the view hasn't shown its document yet");
        assert_eq!(wrapped_size(&view([400, 300], 0.0), &layout()), None);
        assert_eq!(wrapped_size(&view([400, 300], f32::NAN), &layout()), None);
        assert_eq!(wrapped_size(&view([400, 300], 1.0), &Layout { ppp: 0.0, ..layout() }), None);
    }

    #[test]
    fn only_a_zoom_change_with_the_preference_on_resizes() {
        let ctx = Context::default();
        let at = |zoom| view([400, 300], zoom);
        // The first frame of a window only records its zoom.
        assert_eq!(resize_for(&ctx, 7, true, &at(1.0), &at(1.0), &layout()), None);
        // The same zoom again: nothing to do.
        assert_eq!(resize_for(&ctx, 7, true, &at(1.0), &at(1.0), &layout()), None);
        // Zoom in (the Zoom tool, the wheel, Window › Arrange › Match Zoom): the window follows.
        assert_eq!(resize_for(&ctx, 7, true, &at(1.0), &at(2.0), &layout()), Some(vec2(800.0, 600.0)));
        assert_eq!(resize_for(&ctx, 7, true, &at(2.0), &at(2.0), &layout()), None, "once per change");
        // A zoom changed outside the canvas (Match Zoom) still counts on the next frame.
        assert_eq!(resize_for(&ctx, 7, true, &at(0.5), &at(0.5), &layout()), Some(vec2(240.0, 160.0)));
        // Another window keeps its own record.
        assert_eq!(resize_for(&ctx, 8, true, &at(0.5), &at(0.5), &layout()), None);
    }

    #[test]
    fn with_the_preference_off_the_window_keeps_its_size() {
        let ctx = Context::default();
        let at = |zoom| view([400, 300], zoom);
        assert_eq!(resize_for(&ctx, 1, false, &at(1.0), &at(1.0), &layout()), None);
        assert_eq!(resize_for(&ctx, 1, false, &at(1.0), &at(3.0), &layout()), None);
        // Turning it on later doesn't catch up on zooms made while it was off.
        assert_eq!(resize_for(&ctx, 1, true, &at(3.0), &at(3.0), &layout()), None);
        assert_eq!(resize_for(&ctx, 1, true, &at(3.0), &at(1.0), &layout()), Some(vec2(400.0, 300.0)));
    }

    #[test]
    fn a_fit_never_resizes_the_window() {
        let ctx = Context::default();
        let at = |zoom| view([400, 300], zoom);
        assert_eq!(resize_for(&ctx, 2, true, &at(1.0), &at(1.0), &layout()), None);
        // The fit takes its zoom from the window; resizing for it would start a loop.
        let fitting = View { fit_pending: true, ..at(1.0) };
        assert_eq!(resize_for(&ctx, 2, true, &fitting, &at(0.4), &layout()), None);
        let filling = View { fill_pending: true, ..at(0.4) };
        assert_eq!(resize_for(&ctx, 2, true, &filling, &at(0.6), &layout()), None);
        // The zoom the fit chose is the new baseline.
        assert_eq!(resize_for(&ctx, 2, true, &at(0.6), &at(0.6), &layout()), None);
    }

    #[test]
    fn follow_resizes_only_real_floating_windows_with_the_preference_on() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 400, "height": 300})).unwrap();
        app.sync_views();
        app.ui.extras.rulers = false;
        let ctx = Context::default();
        let viewport = ViewportId::from_hash_of(("docwin", 3_u64));
        let rect = Rect::from_min_size(egui::Pos2::ZERO, vec2(800.0, 600.0));
        let at = |zoom| view([400, 300], zoom);
        let window = DocWindow { id: 3, document: 0, view: at(1.0), open: true, position: None };
        let frame = |app: &PhotocraftApp, class, after: &View| follow(app, &ctx, class, viewport, &window, after, rect);
        // Off by default, as in Photoshop.
        assert!(!app.session.prefs().general.zoom_resizes_windows);
        assert_eq!(frame(&app, ViewportClass::Immediate, &at(1.0)), None);
        assert_eq!(frame(&app, ViewportClass::Immediate, &at(2.0)), None);
        app.run("prefs.set", json!({"path": "general.zoomResizesWindows", "value": true})).unwrap();
        assert_eq!(frame(&app, ViewportClass::Immediate, &at(1.0)), Some(vec2(400.0, 300.0)));
        // Drawn inside the main window, a resize command would resize the main window: skipped.
        assert_eq!(frame(&app, ViewportClass::EmbeddedWindow, &at(2.0)), None);
        // With the rulers on, the window makes room for them.
        app.ui.extras.rulers = true;
        let ruled = frame(&app, ViewportClass::Immediate, &at(2.0)).unwrap();
        assert!(ruled.x > 800.0 && ruled.y > 600.0, "{ruled:?}");
    }

    #[test]
    fn the_zoom_tool_options_bar_shows_the_same_setting() {
        // Photoshop's Zoom tool option Resize Windows to Fit is Preferences › Zoom Resizes Windows.
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 64, "height": 64})).unwrap();
        app.sync_views();
        app.ui.tool = crate::state::Tool::Zoom;
        let mut h = Harness::builder().with_size(egui::vec2(1400.0, 60.0)).build_ui_state(
            |ui, app: &mut PhotocraftApp| {
                if !ui.ctx().fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                    return;
                }
                crate::panels::options_bar(app, ui);
            },
            app,
        );
        PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::default());
        h.run_steps(4);
        assert!(!h.state().session.prefs().general.zoom_resizes_windows);
        h.get_by_label("Resize Windows to Fit").click();
        h.run_steps(2);
        assert!(h.state().session.prefs().general.zoom_resizes_windows, "the option turns the preference on");
        h.get_by_label("Resize Windows to Fit").click();
        h.run_steps(2);
        assert!(!h.state().session.prefs().general.zoom_resizes_windows, "and off again");
    }
}
