//! A new foreground colour recolours the selected type or shape layers, as the tool in hand says.

use photocraft_doc::{Fill, LayerContent, LayerId};
use serde_json::json;

use crate::PhotocraftApp;
use crate::state::Tool;

const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];

/// Two type layers and two shape layers (black, the default colours), all selected.
fn app(tool: Tool) -> (PhotocraftApp, Vec<LayerId>, Vec<LayerId>) {
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
    app.run("file.new", json!({"width": 300, "height": 200})).unwrap();
    app.sync_views();
    app.run("tools.setColors", json!({"foreground": "#000000"})).unwrap();
    let mut texts = Vec::new();
    for y in [40, 90] {
        texts.push(LayerId(app.run("type.create", json!({"x": 20, "y": y, "text": "Hello", "size": 20})).unwrap()["layer"].as_u64().unwrap()));
    }
    let mut shapes = Vec::new();
    for x in [150, 220] {
        app.run("shape.create", json!({"kind": "rect", "rect": [x, 20, 50, 50], "fill": "#000000"})).unwrap();
        shapes.push(app.session.active().unwrap().active_layer.unwrap());
    }
    for id in texts.iter().chain(&shapes) {
        app.run("layer.select", json!({"layer": id.0, "mode": "add"})).unwrap();
    }
    app.ui.tool = tool;
    (app, texts, shapes)
}

fn text_rgb(app: &PhotocraftApp, id: LayerId) -> [f32; 3] {
    match &app.session.active().unwrap().doc.layer(id).unwrap().content {
        LayerContent::Text(t) => {
            let c = t.char_runs().into_iter().next().map_or(t.color, |r| r.style.color);
            [c.c[0], c.c[1], c.c[2]]
        }
        _ => panic!("type layer"),
    }
}

fn fill_rgb(app: &PhotocraftApp, id: LayerId) -> Option<[f32; 3]> {
    match &app.session.active().unwrap().doc.layer(id).unwrap().content {
        LayerContent::Shape(sh) => match &sh.fill {
            Some(Fill::Solid(c)) => Some([c.c[0], c.c[1], c.c[2]]),
            _ => None,
        },
        _ => panic!("shape layer"),
    }
}

fn red(c: [f32; 3]) -> bool {
    (c[0] - 1.0).abs() < 1e-3 && c[1].abs() < 1e-3 && c[2].abs() < 1e-3
}

fn steps(app: &PhotocraftApp) -> usize {
    app.session.active().unwrap().history.entries().len()
}

fn pick(app: &mut PhotocraftApp, c: [f32; 4], continuing: bool) {
    app.session.tools.foreground = c;
    super::foreground_changed(app, continuing);
}

#[test]
fn a_type_tool_recolours_every_selected_type_layer_in_one_step() {
    for tool in [Tool::Type, Tool::VerticalType] {
        let (mut app, texts, shapes) = app(tool);
        let n = steps(&app);
        pick(&mut app, RED, false);
        assert!(texts.iter().all(|id| red(text_rgb(&app, *id))), "{tool:?}");
        assert!(shapes.iter().all(|id| !red(fill_rgb(&app, *id).unwrap())), "{tool:?}: shapes keep their fill");
        assert_eq!(steps(&app), n + 1, "one history step");
        assert!(app.session.undo());
        assert!(texts.iter().all(|id| !red(text_rgb(&app, *id))));
    }
}

#[test]
fn shape_and_path_tools_recolour_every_selected_shape_fill_in_one_step() {
    for tool in [Tool::Rectangle, Tool::Polygon, Tool::PathSelection, Tool::DirectSelection, Tool::Pen, Tool::CurvaturePen] {
        let (mut app, texts, shapes) = app(tool);
        let n = steps(&app);
        pick(&mut app, RED, false);
        assert!(shapes.iter().all(|id| fill_rgb(&app, *id).is_some_and(red)), "{tool:?}");
        assert!(texts.iter().all(|id| !red(text_rgb(&app, *id))), "{tool:?}: type keeps its colour");
        assert_eq!(steps(&app), n + 1, "{tool:?}: one history step for both layers");
    }
}

#[test]
fn other_tools_only_change_the_foreground() {
    for tool in [Tool::Brush, Tool::RectMarquee, Tool::Eyedropper, Tool::HorizontalTypeMask] {
        let (mut app, texts, shapes) = app(tool);
        let n = steps(&app);
        pick(&mut app, RED, false);
        assert_eq!(app.session.tools.foreground, RED);
        assert!(texts.iter().all(|id| !red(text_rgb(&app, *id))) && shapes.iter().all(|id| !red(fill_rgb(&app, *id).unwrap())), "{tool:?}");
        assert_eq!(steps(&app), n, "{tool:?}: nothing recorded");
    }
}

#[test]
fn a_color_panel_drag_is_one_history_step_and_a_new_pick_another() {
    let (mut app, _, shapes) = app(Tool::Rectangle);
    let n = steps(&app);
    pick(&mut app, [0.2, 0.0, 0.0, 1.0], false);
    pick(&mut app, [0.6, 0.0, 0.0, 1.0], true);
    pick(&mut app, RED, true);
    assert!(fill_rgb(&app, shapes[0]).is_some_and(red));
    assert_eq!(steps(&app), n + 1, "the drag is one step");
    pick(&mut app, [0.0, 0.0, 1.0, 1.0], false);
    assert_eq!(steps(&app), n + 2, "a new pick is another");
}

#[test]
fn a_swatch_and_the_color_picker_go_through_it() {
    let (mut app, _, shapes) = app(Tool::Rectangle);
    let same = |app: &PhotocraftApp, id: LayerId| {
        let f = app.session.tools.foreground;
        fill_rgb(app, id).is_some_and(|c| (0..3).all(|i| (c[i] - f[i]).abs() < 1e-3))
    };
    // A swatch from the default library (the first one that isn't black).
    let names = app.run("swatches.list", json!({})).unwrap();
    let name = names["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|g| g["swatches"].as_array().cloned().unwrap_or_default())
        .find(|w| w["hex"].as_str().is_some_and(|h| h != "#000000"))
        .and_then(|w| w["name"].as_str().map(str::to_string))
        .expect("a default swatch");
    crate::swatches_ui::use_swatch(&mut app, &name, false);
    assert_ne!(app.session.tools.foreground, [0.0, 0.0, 0.0, 1.0]);
    assert!(shapes.iter().all(|id| same(&app, *id)), "the swatch recoloured the shapes");
    // The Color Picker's OK for the foreground.
    let mut f = serde_json::Map::new();
    f.insert("__colorPicker".into(), json!("foreground"));
    f.insert("color".into(), json!("#0000ff"));
    crate::color_picker_ui::confirm(&mut app, &f).unwrap();
    assert_eq!(app.session.tools.foreground, [0.0, 0.0, 1.0, 1.0]);
    assert!(shapes.iter().all(|id| same(&app, *id)), "the picker recoloured the shapes");
}

#[test]
fn the_move_tool_recolours_the_selected_type_and_shapes_together_in_one_step() {
    // Layers picked with the Move tool (a click, ⇧-click or its marquee) take the colour, type and
    // shapes alike.
    let (mut app, texts, shapes) = app(Tool::Move);
    let n = steps(&app);
    pick(&mut app, RED, false);
    assert!(texts.iter().all(|id| red(text_rgb(&app, *id))), "the type is red");
    assert!(shapes.iter().all(|id| fill_rgb(&app, *id).is_some_and(red)), "the shapes are red");
    assert_eq!(steps(&app), n + 1, "one history step for all four layers");
    assert!(app.session.undo());
    assert!(texts.iter().all(|id| !red(text_rgb(&app, *id))) && shapes.iter().all(|id| !red(fill_rgb(&app, *id).unwrap())));
    // Only the selected ones: with one shape selected, the rest keep their colour.
    app.run("layer.select", json!({"layer": shapes[0].0})).unwrap();
    pick(&mut app, RED, false);
    assert!(fill_rgb(&app, shapes[0]).is_some_and(red));
    assert!(!red(fill_rgb(&app, shapes[1]).unwrap()) && texts.iter().all(|id| !red(text_rgb(&app, *id))));
}

#[test]
fn a_move_tool_marquee_selection_takes_the_colour() {
    // The video case: shapes box-selected with the Move tool's marquee, then a swatch.
    let (mut app, _, shapes) = app(Tool::Move);
    app.run("layer.select", json!({"layer": shapes[0].0})).unwrap();
    // The marquee from an empty spot across both shapes.
    for (kind, x, y) in [("down", 140.0, 10.0), ("move", 200.0, 50.0), ("move", 290.0, 90.0), ("up", 290.0, 90.0)] {
        let ev = match kind {
            "down" => crate::canvas::ToolEvent::Down { x, y, pressure: 1.0 },
            "move" => crate::canvas::ToolEvent::Move { x, y, pressure: 1.0 },
            _ => crate::canvas::ToolEvent::Up { x, y },
        };
        crate::canvas::tool_event(&mut app, ev, egui::Modifiers::NONE);
    }
    let mut sel = app.session.active().unwrap().selected_layers();
    sel.sort_by_key(|l| l.0);
    assert_eq!(sel, shapes, "the marquee selected both shapes");
    pick(&mut app, RED, false);
    assert!(shapes.iter().all(|id| fill_rgb(&app, *id).is_some_and(red)));
}
