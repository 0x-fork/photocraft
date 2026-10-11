//! Layers-panel pieces for smart objects: the thumbnail badge and the Smart Filters sub-rows
//! (eye toggles; double-click a filter to re-open its dialog with the recorded parameters).
//! The Smart Filters row carries the filter mask thumbnail: click it to paint the filter mask,
//! ⌥-click to view it, ⇧-click to disable it (FILE-215-3).

use egui::{Align2, Color32, Rect, Sense, Stroke, pos2, vec2};
use photocraft_doc::{Layer, LayerContent, SmartObject};
use serde_json::{Value, json};

use crate::theme::Tokens;
use crate::{PhotocraftApp, icons};

/// Smart-object badge in the bottom-right corner of a layer thumbnail.
pub fn thumb_badge(ui: &egui::Ui, l: &Layer, thumb: Rect) {
    if !matches!(l.content, LayerContent::Smart(_)) {
        return;
    }
    let t = Tokens::get(ui.ctx());
    let r = Rect::from_min_size(pos2(thumb.right() - 9.0, thumb.bottom() - 9.0), vec2(11.0, 11.0));
    ui.painter().rect_filled(r, 2.0, t.field);
    ui.painter().rect_stroke(r, 2.0, Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
    icons::paint(ui, r, "app-window", 9.0, t.icon);
}

/// The display name of a smart filter (its command's label without the ellipsis). A Photoshop
/// filter PhotoCraft doesn't implement shows its own name and is marked as kept as is.
fn filter_label(f: &photocraft_doc::SmartFilter) -> String {
    let command = f.command.as_str();
    if command == photocraft_engine::smart_cmds::UNSUPPORTED_FILTER {
        let name = f.params.get("name").and_then(Value::as_str).unwrap_or("Smart filter");
        return format!("{} (kept, not editable)", name.trim_end_matches("...").trim_end_matches('…'));
    }
    photocraft_engine::commands::find(command)
        .map_or_else(|| command.rsplit('.').next().unwrap_or(command).to_string(), |c| tl!(c.label).trim_end_matches('…').to_string())
}

/// Smart Filters header + one row per filter (top filter first, as in Photoshop) under a smart
/// layer. Edits are queued in `actions` as engine commands.
pub fn filter_rows(app: &mut PhotocraftApp, ui: &mut egui::Ui, l: &Layer, depth: usize, actions: &mut Vec<(String, Value)>) {
    let LayerContent::Smart(sm) = &l.content else { return };
    if sm.smart_filters.is_empty() {
        return;
    }
    let t = Tokens::get(ui.ctx());
    let indent = 30.0 + depth as f32 * 14.0 + 34.0;
    let rows = std::iter::once(None).chain((0..sm.smart_filters.len()).rev().map(Some));
    for index in rows {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(rect, 0.0, t.hover.gamma_multiply(0.35));
        }
        if t.pro {
            ui.painter().line_segment([pos2(rect.left() + 30.0, rect.top()), pos2(rect.left() + 30.0, rect.bottom())], Stroke::new(1.0, t.separator));
        }
        let (name, on) = match index {
            None => ("Smart Filters".to_string(), sm.filters_enabled),
            Some(i) => (filter_label(&sm.smart_filters[i]), sm.smart_filters[i].visible && sm.filters_enabled),
        };
        let eye = Rect::from_min_size(pos2(rect.left() + 6.0, rect.center().y - 9.0), vec2(18.0, 18.0));
        let eye_resp = ui.interact(eye, ui.id().with(("sf-eye", l.id.0, index)), Sense::click());
        let shown = match index {
            None => sm.filters_enabled,
            Some(i) => sm.smart_filters[i].visible,
        };
        if shown {
            icons::paint(ui, eye, "eye", 12.0, t.icon);
        }
        if eye_resp.clicked() {
            actions.push(match index {
                None => ("layer.smartFilter.disableSmartFilters".into(), json!({"layer": l.id.0})),
                Some(i) => ("layer.smartFilter.setVisible".into(), json!({"layer": l.id.0, "index": i})),
            });
        }
        let x = rect.left() + indent + if index.is_none() { 0.0 } else { 16.0 };
        if index.is_none() {
            let sw = Rect::from_center_size(pos2(x - 12.0, rect.center().y), vec2(14.0, 10.0));
            paint_filter_mask_thumb(app, ui, sw, l, sm);
            let sw_resp = ui.interact(sw.expand(2.0), ui.id().with(("sf-mask", l.id.0)), Sense::click());
            if sw_resp.clicked() {
                actions.extend(filter_mask_click(l, sm, ui.input(|i| i.modifiers)));
            }
        }
        ui.painter().text(pos2(x, rect.center().y), Align2::LEFT_CENTER, name, egui::FontId::proportional(11.5), if on { t.text_dim } else { t.text_faint });
        if let Some(i) = index
            && resp.double_clicked()
        {
            open_editor(app, ui.ctx(), l.id.0, sm, i);
        }
    }
}

/// The filter mask thumbnail: the mask itself (white where the filters show), crossed out when
/// it is disabled, with the target brackets when painting goes into it.
fn paint_filter_mask_thumb(app: &PhotocraftApp, ui: &egui::Ui, sw: Rect, l: &Layer, sm: &SmartObject) {
    let t = Tokens::get(ui.ctx());
    let p = ui.painter();
    p.rect_filled(sw, 1.0, Color32::WHITE);
    if let Some((m, canvas)) = sm.filter_mask.as_ref().zip(app.session.active().map(|d| d.doc.bounds())) {
        // A coarse sample of the canvas, one cell per thumbnail point, as a mesh (rectangles
        // this small would be rounded away to nothing).
        let (cols, rows) = (sw.width() as i32, sw.height() as i32);
        let mut mesh = egui::Mesh::default();
        for j in 0..rows {
            for i in 0..cols {
                let x = canvas.x0 + (((i as f32 + 0.5) / cols as f32) * canvas.width() as f32) as i32;
                let y = canvas.y0 + (((j as f32 + 0.5) / rows as f32) * canvas.height() as f32) as i32;
                let v = m.surface.sample_channel(x, y, 0).clamp(0.0, 1.0);
                if v < 1.0 {
                    let cell = Rect::from_min_size(pos2(sw.left() + i as f32, sw.top() + j as f32), vec2(1.0, 1.0));
                    mesh.add_colored_rect(cell, Color32::from_gray((v * 255.0).round() as u8));
                }
            }
        }
        p.add(egui::Shape::mesh(mesh));
        if !m.enabled {
            p.line_segment([sw.left_top(), sw.right_bottom()], Stroke::new(1.5, Color32::from_rgb(200, 40, 40)));
            p.line_segment([sw.left_bottom(), sw.right_top()], Stroke::new(1.5, Color32::from_rgb(200, 40, 40)));
        }
    }
    p.rect_stroke(sw, 1.0, Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
    if app.ui.filter_mask_target == Some(l.id.0) && app.filter_mask_targeted() {
        crate::mask_thumbs_ui::paint_brackets(p, sw, t.text);
    }
}

/// What a click on the filter mask thumbnail does: target it for painting (selecting the smart
/// object), ⌥ view it alone (⇧⌥ as an overlay), ⇧ disable or enable it.
fn filter_mask_click(l: &Layer, sm: &SmartObject, m: egui::Modifiers) -> Vec<(String, Value)> {
    let layer = l.id.0;
    if m.alt {
        let mode = if m.shift { "toggleOverlay" } else { "toggleGray" };
        return sm
            .filter_mask
            .is_some()
            .then(|| (photocraft_engine::mask_view_cmds::ID.to_string(), json!({"layer": layer, "mode": mode, "mask": "filter"})))
            .into_iter()
            .collect();
    }
    if m.shift {
        return sm
            .filter_mask
            .as_ref()
            .map(|f| ("layer.smartFilter.disableFilterMask".to_string(), json!({"layer": layer, "enabled": !f.enabled})))
            .into_iter()
            .collect();
    }
    vec![("layer.select".into(), json!({"layer": layer})), ("ui.filterMaskTarget".into(), json!(layer))]
}

/// Re-opens a smart filter's dialog with its recorded parameters; OK runs
/// `layer.smartFilter.setParams` instead of adding another filter (see `dialogs::confirm`).
pub fn open_editor(app: &mut PhotocraftApp, ctx: &egui::Context, layer: u64, sm: &SmartObject, index: usize) {
    let Some(f) = sm.smart_filters.get(index) else { return };
    // Camera Raw has its own full-window dialog rather than a generic parameter form.
    if f.command == photocraft_engine::lens_cmds::RAW {
        if let Err(e) = crate::camera_raw_ui::open_smart_filter(app, ctx, photocraft_doc::LayerId(layer), index) {
            app.ui.status = e;
            app.ui.status_error = true;
        }
        return;
    }
    if !crate::filter_dialog::has_dialog(&f.command) {
        return;
    }
    let Some(id) = crate::filter_dialog::open(app, &f.command) else { return };
    if let Some(d) = app.ui.dialog_mut(id) {
        // The live preview would stack a second copy of the filter on the smart object.
        d.fields.remove("__preview");
        d.fields.insert("__smartFilter".into(), json!({"layer": layer, "index": index}));
        if let Value::Object(p) = &f.params {
            for (k, v) in p {
                if d.fields.contains_key(k) {
                    d.fields.insert(k.clone(), v.clone());
                }
            }
        }
    }
}

/// The command a confirmed filter dialog runs: smart-filter edits update the recorded filter.
pub fn confirm_command(fields: &serde_json::Map<String, Value>, command: String, params: Value) -> (String, Value) {
    match fields.get("__smartFilter") {
        Some(sf) => ("layer.smartFilter.setParams".into(), json!({"layer": sf["layer"], "index": sf["index"], "params": params})),
        None => (command, params),
    }
}

#[cfg(test)]
mod tests {
    use photocraft_doc::LayerContent;
    use photocraft_engine::Session;
    use serde_json::json;

    use crate::{PhotocraftApp, Services};

    fn smart(app: &PhotocraftApp) -> (photocraft_doc::Layer, photocraft_doc::SmartObject) {
        let st = app.session.active().unwrap();
        let l = st.doc.layer(st.active_layer.unwrap()).unwrap().clone();
        let LayerContent::Smart(sm) = l.content.clone() else { panic!("not a smart object") };
        (l, sm)
    }

    /// FILE-215-3: clicking the filter mask thumbnail targets it, the Brush paints into it (not
    /// the layer mask, no rasterize prompt), and clicking the layer thumbnail goes back.
    #[test]
    fn the_filter_mask_thumbnail_targets_the_filter_mask() {
        let mut app = PhotocraftApp::new(Session::new(), Services::default());
        app.run("file.new", json!({"width": 32, "height": 32})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        app.run("edit.fill", json!({"contents": "color", "color": "#ff0000"})).unwrap();
        app.run("layer.smartObjects.convertToSmartObject", json!({})).unwrap();
        app.run("filter.blur.gaussianBlur", json!({"radius": 2})).unwrap();
        let (l, sm) = smart(&app);
        let actions = super::filter_mask_click(&l, &sm, egui::Modifiers::NONE);
        assert_eq!(actions.last().unwrap().0, "ui.filterMaskTarget");
        assert!(super::filter_mask_click(&l, &sm, egui::Modifiers::ALT).is_empty(), "nothing to view yet");
        app.ui.filter_mask_target = Some(l.id.0);
        app.sync_views();
        assert!(app.filter_mask_targeted());
        assert_eq!(crate::canvas::paint_target(&app), json!("filterMask"));
        assert_eq!(app.with_mask_target("filter.blur.gaussianBlur", json!({}))["target"], "filterMask");
        assert!(crate::rasterize_prompt::needed(&app).is_none(), "painting the filter mask needs no rasterizing");
        app.run(
            "paint.stroke",
            json!({"points": [[4, 0], [4, 31]], "size": 4, "hardness": 1, "color": "#000000", "target": crate::canvas::paint_target(&app)}),
        )
        .unwrap();
        let (l, sm) = smart(&app);
        assert_eq!(sm.filter_mask.as_ref().unwrap().value(4, 16), 0.0);
        assert!(l.mask.is_none(), "no layer mask");
        assert_eq!(super::filter_mask_click(&l, &sm, egui::Modifiers::ALT)[0].1["mask"], "filter");
        // Targeting a layer mask (or the pixels, `ui.maskTarget`) ends it.
        app.ui.mask_target = true;
        app.sync_views();
        assert_eq!(app.ui.filter_mask_target, None, "a layer mask target replaces it");
    }
}
