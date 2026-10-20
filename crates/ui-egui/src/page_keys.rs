//! Photoshop's page keys scroll the canvas (#8057): Page Up and Page Down a screen up or down,
//! ⌘/Ctrl turns them sideways (⌘ Page Up left, ⌘ Page Down right), and ⇧ makes the step ten
//! screen points. Home and End show the document's top-left and bottom-right corners. The zoom
//! stays as it is. A binding of the user's on the same keys has already run and taken them.

use egui::{Event, Key, Modifiers, Vec2, vec2};

use crate::PhotocraftApp;

/// Screen points a ⇧ page key scrolls.
pub const SMALL_STEP: f32 = 10.0;

/// What a page key does to the view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PageKey {
    /// Scroll by this many screen points (positive: show what is below or to the right).
    Scroll(Vec2),
    /// Show the document's top-left corner (Home) or its bottom-right one (End).
    Corner { end: bool },
}

/// The page key `key` with `mods` held, in a canvas of `size` points. `None` for other keys, ⌥,
/// or ⇧ on Home and End.
pub fn page_key(key: Key, mods: Modifiers, size: Vec2) -> Option<PageKey> {
    if mods.alt {
        return None;
    }
    let sign = match key {
        Key::PageUp => -1.0,
        Key::PageDown => 1.0,
        Key::Home | Key::End if !mods.shift && !mods.command => return Some(PageKey::Corner { end: key == Key::End }),
        _ => return None,
    };
    let sideways = mods.command;
    let step = match (mods.shift, sideways) {
        (true, _) => SMALL_STEP,
        (false, true) => size.x,
        (false, false) => size.y,
    };
    Some(PageKey::Scroll(if sideways { vec2(sign * step, 0.0) } else { vec2(0.0, sign * step) }))
}

/// Consumes this frame's page keys and moves the active document's view. True when one ran.
pub fn handle(app: &mut PhotocraftApp, ctx: &egui::Context) -> bool {
    let Some(i) = app.session.active_index() else { return false };
    let rect = crate::rulers::content_rect(app, app.last_canvas_rect);
    let size = rect.size();
    let presses: Vec<PageKey> = ctx.input_mut(|input| {
        let mut out = Vec::new();
        input.events.retain(|e| match e {
            Event::Key { key, pressed: true, modifiers, .. } => match page_key(*key, *modifiers, size) {
                Some(p) => {
                    out.push(p);
                    false
                }
                None => true,
            },
            _ => true,
        });
        out
    });
    if presses.is_empty() {
        return false;
    }
    app.sync_views();
    let Some(doc_size) = app.session.active().map(|st| [st.doc.size.width as f32, st.doc.size.height as f32]) else { return false };
    let (ppp, aspect, flip, overscroll) = (app.ppp, app.ui.view.display_aspect(), app.ui.view.flip_horizontal, app.session.prefs().tools.overscroll);
    let ppp = if ppp.is_finite() && ppp > 0.0 { ppp } else { 1.0 };
    let Some(view) = app.ui.views.get_mut(i) else { return false };
    let zoom = view.zoom / ppp;
    if !zoom.is_finite() || zoom <= 0.0 {
        return false;
    }
    view.fit_pending = false;
    view.fill_pending = false;
    for p in presses {
        match p {
            PageKey::Scroll(d) => {
                let xf = crate::canvas::ViewXform { rect, zoom, center: view.center, flip, rotation: view.rotation, aspect };
                let d = xf.unmap_vec(d) / zoom;
                if d.is_finite() {
                    view.center[0] += d.x;
                    view.center[1] += d.y;
                }
            }
            PageKey::Corner { end } => {
                // The document corner goes to the canvas corner the document runs on from: the
                // top-left one in an upright view, another when it is rotated or flipped.
                let xf = crate::canvas::ViewXform { rect, zoom, center: view.center, flip, rotation: view.rotation, aspect };
                let [w, h] = doc_size;
                let (corner, inward) = if end { ([w, h], vec2(-1.0, -1.0)) } else { ([0.0, 0.0], vec2(1.0, 1.0)) };
                let dir = xf.map_vec(inward);
                let at = vec2(-dir.x.signum() * size.x / 2.0, -dir.y.signum() * size.y / 2.0);
                let off = xf.unmap_vec(at) / zoom;
                if off.is_finite() {
                    view.center = [corner[0] - off.x, corner[1] - off.y];
                }
            }
        }
    }
    // Overscroll off: the view stops at the edge, as the scrollbars do.
    if !overscroll {
        crate::scrollbars::clamp_view_with_aspect(view, size, ppp, aspect);
    }
    ctx.request_repaint();
    true
}
