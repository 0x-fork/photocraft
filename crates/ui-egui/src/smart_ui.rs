//! Layers-panel pieces for smart objects: the thumbnail badge and the Smart Filters sub-rows
//! (eye toggles; double-click a filter to re-open its dialog with the recorded parameters).

use egui::{Color32, Rect, Sense, Stroke, pos2, vec2};
use photocraft_doc::{Document, Layer, LayerContent, SmartObject};
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
pub(crate) fn filter_label(f: &photocraft_doc::SmartFilter) -> String {
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
pub fn has_filters(l: &Layer) -> bool {
    matches!(&l.content, LayerContent::Smart(sm) if !sm.smart_filters.is_empty())
}

/// The two rings in the layer row indicate an attached Smart Filters stack.
pub fn paint_badge(painter: &egui::Painter, center: egui::Pos2, color: Color32) {
    for dx in [-2.5, 2.5] {
        painter.circle_stroke(center + vec2(dx, 0.0), 3.5, Stroke::new(1.0, color));
    }
}

pub fn filter_rows(app: &mut PhotocraftApp, ui: &mut egui::Ui, doc: &Document, l: &Layer, depth: usize, actions: &mut Vec<(String, Value)>) {
    let LayerContent::Smart(sm) = &l.content else { return };
    if sm.smart_filters.is_empty() {
        return;
    }
    let t = Tokens::get(ui.ctx());
    let indent = 30.0 + crate::layer_row_ui::indent(depth, ui.available_width(), 130.0);
    let rows = std::iter::once(None).chain((0..sm.smart_filters.len()).rev().map(Some));
    for index in rows {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), if index.is_none() { 30.0 } else { 22.0 }), Sense::hover());
        if resp.hovered() {
            ui.painter().rect_filled(rect, 0.0, t.hover.gamma_multiply(0.35));
        }
        if t.pro {
            ui.painter().line_segment([pos2(rect.left() + 30.0, rect.top()), pos2(rect.left() + 30.0, rect.bottom())], Stroke::new(1.0, t.separator));
        }
        let (name, on) = match index {
            None => (tl!("Smart Filters").to_string(), sm.filters_enabled),
            Some(i) => (filter_label(&sm.smart_filters[i]), sm.smart_filters[i].visible && sm.filters_enabled),
        };
        let eye = Rect::from_center_size(pos2(rect.left() + indent + if index.is_none() { 9.0 } else { 41.0 }, rect.center().y), vec2(18.0, 18.0));
        let eye_resp = ui.interact(eye, ui.id().with(("sf-eye", l.id.0, index)), Sense::click());
        let shown = match index {
            None => sm.filters_enabled,
            Some(i) => sm.smart_filters[i].visible,
        };
        if shown {
            icons::paint(ui, eye, "eye", 12.0, if on { t.icon } else { t.text_faint });
        }
        eye_resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, shown, format!("{} {name}", tl!("Visibility"))));
        if eye_resp.clicked() {
            actions.push(match index {
                None => ("layer.smartFilter.disableSmartFilters".into(), json!({"layer": l.id.0})),
                Some(i) => ("layer.smartFilter.setVisible".into(), json!({"layer": l.id.0, "index": i})),
            });
        }
        let x = rect.left() + indent + 60.0;
        if index.is_none() {
            let sw = Rect::from_center_size(pos2(x - 20.0, rect.center().y), vec2(24.0, 24.0));
            if let Some(mask) = &sm.filter_mask {
                let tex = app.mask_thumb_kind(ui.ctx(), doc, l.id, mask, crate::mask_thumbs_ui::THUMB_SMART_MASK);
                ui.painter().image(tex, sw, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                if !mask.enabled {
                    ui.painter().line_segment([sw.left_top(), sw.right_bottom()], Stroke::new(1.0, t.danger));
                    ui.painter().line_segment([sw.right_top(), sw.left_bottom()], Stroke::new(1.0, t.danger));
                }
            } else {
                ui.painter().rect_filled(sw, 0.0, Color32::WHITE);
            }
            ui.painter().rect_stroke(sw, 1.0, Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
        }
        let blend = index.map(|_| Rect::from_center_size(pos2(rect.right() - crate::layer_row_ui::RIGHT_PAD - 9.0, rect.center().y), vec2(18.0, 18.0)));
        let right = blend.map_or(rect.right() - crate::layer_row_ui::RIGHT_PAD, |r| r.left() - 4.0);
        crate::layer_row_ui::label(
            ui.painter(),
            x,
            rect.center().y,
            right,
            &name,
            egui::FontId::proportional(11.5),
            if on { t.text_dim } else { t.text_faint },
        );
        if let Some(i) = index {
            let name_rect = Rect::from_min_max(pos2(x.min(right), rect.top()), pos2(right, rect.bottom()));
            let name_resp = ui.interact(name_rect, ui.id().with(("sf-name", l.id.0, i)), Sense::click());
            name_resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &name));
            if name_resp.double_clicked() {
                open_editor(app, ui.ctx(), l.id.0, sm, i);
            }
            if let Some(r) = blend {
                let blend_resp = ui.interact(r, ui.id().with(("sf-blend", l.id.0, i)), Sense::click());
                icons::paint(ui, r, "sliders-horizontal", 13.0, if blend_resp.hovered() { t.text } else { t.icon });
                let label = crate::i18n::fmt(tl!("Blending Options ({filter})"), &[("filter", &name)]);
                blend_resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
                if blend_resp.double_clicked()
                    && let Err(e) = crate::smart_filter_dialog::open(app, l.id, i)
                {
                    crate::notices::error(app, e);
                }
                blend_resp.on_hover_text(label);
            }
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
    let target = match crate::smart_filter_dialog::target(app, photocraft_doc::LayerId(layer), index) {
        Ok(target) => target,
        Err(e) => {
            crate::notices::error(app, e);
            return;
        }
    };
    let Some(id) = crate::adjust_dialog::open(app, &f.command)
        .or_else(|| crate::filter_dialog::has_dialog(&f.command).then(|| crate::filter_dialog::open(app, &f.command)).flatten())
    else {
        return;
    };
    if let Some(d) = app.ui.dialog_mut(id) {
        d.fields.insert("__smartFilter".into(), target);
        if let Value::Object(p) = &f.params {
            for (k, v) in p {
                if !k.starts_with("__") {
                    d.fields.insert(k.clone(), v.clone());
                }
            }
        }
    }
}

/// The command a confirmed filter dialog runs: smart-filter edits update the recorded filter.
pub fn confirm_command(app: &PhotocraftApp, fields: &serde_json::Map<String, Value>, command: String, params: Value) -> Result<(String, Value), String> {
    if crate::smart_filter_dialog::owns(fields) {
        return crate::smart_filter_dialog::edit_command(app, fields);
    }
    match fields.get("__smartFilter") {
        Some(sf) => {
            let (layer, index) = crate::smart_filter_dialog::validate_target(app, sf)?;
            Ok(("layer.smartFilter.setParams".into(), json!({"layer": layer.0, "index": index, "params": params})))
        }
        None => Ok((command, params)),
    }
}

#[cfg(test)]
mod tests;
