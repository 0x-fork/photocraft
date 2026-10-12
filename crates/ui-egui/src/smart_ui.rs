//! Layers-panel pieces for smart objects: the thumbnail badge and the Smart Filters sub-rows
//! (eye toggles; double-click a filter to re-open its dialog with the recorded parameters).

use egui::{Color32, Rect, Sense, Stroke, pos2, vec2};
use photocraft_doc::{Layer, LayerContent, SmartObject};
use serde_json::{Value, json};

use crate::state::LayerThumbnailSize;
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

/// Height of a Smart Filter or effect sub-row. As in Photoshop, these stay compact at every
/// Layers panel thumbnail size; only the layer row itself grows with its thumbnails.
pub(crate) const SUB_ROW_H: f32 = 22.0;

/// The Smart Filters header row: (row height, shared-mask thumbnail side). The mask follows the
/// Layers panel thumbnail size (hidden at None) and the row grows just enough to hold it.
pub(crate) fn smart_filters_header_geometry(size: LayerThumbnailSize, pro: bool) -> (f32, f32) {
    let (thumb, _) = crate::layers_panel_ui::thumbnail_geometry(size, pro);
    if thumb == 0.0 { (SUB_ROW_H, 0.0) } else { ((thumb + 6.0).max(SUB_ROW_H), thumb) }
}

/// Smart Filters header + one row per filter (top filter first, as in Photoshop) under a smart
/// layer. Edits are queued in `actions` as engine commands.
pub fn filter_rows(app: &mut PhotocraftApp, ui: &mut egui::Ui, l: &Layer, depth: usize, actions: &mut Vec<(String, Value)>) {
    let LayerContent::Smart(sm) = &l.content else { return };
    if sm.smart_filters.is_empty() {
        return;
    }
    let t = Tokens::get(ui.ctx());
    let (header_height, mask_size) = smart_filters_header_geometry(app.ui.layers_panel_options.thumbnail_size, t.pro);
    let indent = 30.0 + depth as f32 * 14.0 + 34.0;
    let rows = std::iter::once(None).chain((0..sm.smart_filters.len()).rev().map(Some));
    for index in rows {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), if index.is_none() { header_height } else { SUB_ROW_H }), Sense::click());
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
        if index.is_none() && mask_size > 0.0 {
            // The Smart Filters mask scales along with the thumbnail size, not
            // independently of the selected Layers panel display preference.
            let sw = Rect::from_center_size(pos2(x + mask_size / 2.0, rect.center().y), vec2(mask_size, mask_size));
            ui.painter().rect_filled(sw, 1.0, if sm.filter_mask.as_ref().is_some_and(|m| m.enabled) { Color32::from_gray(200) } else { Color32::WHITE });
            ui.painter().rect_stroke(sw, 1.0, Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
        }
        let text_x = if index.is_none() && mask_size > 0.0 { x + mask_size + 7.0 } else { x };
        crate::layer_row_ui::label(
            ui.painter(),
            text_x,
            rect.center().y,
            rect.right() - crate::layer_row_ui::RIGHT_PAD,
            &name,
            egui::FontId::proportional(11.5),
            if on { t.text_dim } else { t.text_faint },
        );
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, name.clone()));
        if let Some(i) = index
            && resp.double_clicked()
        {
            open_editor(app, ui.ctx(), l.id.0, sm, i);
        }
    }
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
mod layer_thumbnail_scaling_tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    #[test]
    fn sub_rows_stay_compact_while_the_smart_filter_mask_follows_thumbnail_setting() {
        for setting in [LayerThumbnailSize::None, LayerThumbnailSize::Small, LayerThumbnailSize::Medium, LayerThumbnailSize::Large] {
            let mut session = photocraft_engine::Session::new();
            session.execute("file.new", json!({"width": 50, "height": 38})).unwrap();
            session.execute("edit.fill", json!({"color": "#8A42B3"})).unwrap();
            session.execute("layer.smartObjects.convertToSmartObject", json!({})).unwrap();
            session.execute("filter.blur.gaussianBlur", json!({"radius": 3})).unwrap();
            session.execute("filter.sharpen.unsharpMask", json!({"amount": 80, "radius": 1, "threshold": 0})).unwrap();
            session.execute("layer.layerStyle.dropShadow", json!({"size": 2})).unwrap();
            assert!(matches!(session.active().unwrap().doc.layers.last().unwrap().content, LayerContent::Smart(_)));
            let mut h = Harness::builder().with_size(vec2(1440.0, 1400.0)).with_max_steps(64).build_eframe(move |cc| {
                PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
                PhotocraftApp::new(session, crate::Services::default())
            });
            h.state_mut().ui.layers_panel_options.thumbnail_size = setting;
            h.run_steps(8);
            // Filter and effect rows keep Photoshop's compact height at every thumbnail size.
            for label in ["Gaussian Blur", "Unsharp Mask", "Effects", "Drop Shadow"] {
                let rect = h.get_by_label(label).rect();
                assert!((rect.height() - SUB_ROW_H).abs() < 1.0, "{setting:?}: {label} was {rect:?}, expected {SUB_ROW_H}");
            }
            // Only the Smart Filters header grows, just enough for its shared-mask thumbnail.
            let (header, mask) = smart_filters_header_geometry(setting, true);
            let rect = h.get_by_label("Smart Filters").rect();
            assert!((rect.height() - header).abs() < 1.0, "{setting:?}: Smart Filters was {rect:?}, expected {header}");
            assert!(mask <= header - 6.0 || mask == 0.0, "{setting:?}: a {mask} mask must fit a {header} row");
            assert_eq!(mask == 0.0, setting == LayerThumbnailSize::None, "{setting:?}: the mask hides only at None");
            if let Some(dir) = std::env::var_os("PHOTOCRAFT_LAYERS_SNAPSHOTS") {
                let dir = std::path::PathBuf::from(dir);
                std::fs::create_dir_all(&dir).unwrap();
                h.render().unwrap().save(dir.join(format!("layers-sub-rows-{setting:?}.png"))).unwrap();
            }
        }
    }
}
