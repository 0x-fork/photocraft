//! A new foreground colour recolours the selected objects: the Color panel, the Swatches panel and
//! the Color Picker set the foreground, and depending on the tool in hand it also goes to what is
//! selected:
//!
//! * characters selected while editing type: those characters (`type_tool::foreground_changed`);
//! * a Type tool, not editing: the colour of every selected type layer (`type.setStyle`);
//! * a shape tool, a Pen or a path selection tool: the fill of every selected shape layer
//!   (`shape.edit` with `fill`);
//! * the Move tool (layers picked by a click, ⇧-click or its marquee): both, every selected type
//!   layer and shape;
//! * any other tool (the brushes, the pixel selection tools…): nothing, only the foreground.
//!
//! One pick is one history step for all the layers it recolours; a drag in the Color panel's field
//! or sliders is one step too.

use egui::Context;
use photocraft_doc::LayerContent;
use serde_json::json;

use crate::PhotocraftApp;
use crate::state::Tool;

fn hex(c: [f32; 4]) -> String {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", b(c[0]), b(c[1]), b(c[2]))
}

/// Does a new foreground recolour selected shape layers with `tool`?
fn recolours_shapes(tool: Tool) -> bool {
    crate::vector_ui::is_shape_tool(tool)
        || matches!(
            tool,
            Tool::PathSelection
                | Tool::DirectSelection
                | Tool::Pen
                | Tool::FreeformPen
                | Tool::CurvaturePen
                | Tool::AddAnchorPoint
                | Tool::DeleteAnchorPoint
                | Tool::ConvertPoint
        )
}

/// Is the pointer still held from an earlier frame (a drag in the Color panel), so this pick
/// joins the previous one's history step?
pub fn continuing(ctx: &Context) -> bool {
    ctx.input(|i| i.pointer.any_down() && !i.pointer.any_pressed())
}

/// The foreground colour just changed through the Color panel, the Swatches panel or the Color
/// Picker: recolour what is selected, as the tool in hand says (see the module docs).
/// `continuing` (a drag still in progress) joins the previous pick's history step.
pub fn foreground_changed(app: &mut PhotocraftApp, continuing: bool) {
    // Editing type: the selected characters, in the editing session's step; nothing else.
    if app.ui.text_edit.is_some() {
        crate::type_tool::foreground_changed(app);
        return;
    }
    let tool = app.ui.tool;
    // What the tool in hand recolours: type layers, shape layers, or both (the Move tool).
    let (do_texts, do_shapes) = match tool {
        Tool::Move => (true, true),
        t if t.is_type() && !t.is_type_mask() => (true, false),
        t if recolours_shapes(t) => (false, true),
        _ => return,
    };
    let Some(st) = app.session.active() else { return };
    let (texts, shapes): (Vec<u64>, Vec<u64>) = {
        let selected: Vec<_> = st.selected_layers().into_iter().filter_map(|id| st.doc.layer(id)).collect();
        let pick = |want: fn(&LayerContent) -> bool| selected.iter().filter(|l| want(&l.content)).map(|l| l.id.0).collect::<Vec<_>>();
        (pick(|c| matches!(c, LayerContent::Text(_))), pick(|c| matches!(c, LayerContent::Shape(_))))
    };
    let texts = if do_texts { texts } else { Vec::new() };
    let shapes = if do_shapes { shapes } else { Vec::new() };
    if texts.is_empty() && shapes.is_empty() {
        return;
    }
    let key = match app.ui.apply_color_key.clone().filter(|_| continuing) {
        Some(k) => k,
        None => {
            let k = format!("apply-color-{}", app.ui.alloc_id());
            app.ui.apply_color_key = Some(k.clone());
            k
        }
    };
    let color = hex(app.session.tools.foreground);
    let result = (|| {
        if !texts.is_empty() {
            app.run("type.setStyle", json!({"layers": texts, "color": color, "coalesce": key}))?;
        }
        shapes.iter().try_for_each(|id| app.run("shape.edit", json!({"layer": id, "fill": color, "coalesce": key})).map(|_| ()))
    })();
    if let Err(e) = result {
        app.ui.status = e;
        app.ui.status_error = true;
    }
}

#[cfg(test)]
#[path = "apply_color_tests.rs"]
mod tests;
