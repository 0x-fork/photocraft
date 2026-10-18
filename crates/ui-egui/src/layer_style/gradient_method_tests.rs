use egui_kittest::kittest::Queryable;

use super::*;

fn app() -> PhotocraftApp {
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
    app.run("file.new", json!({"width": 32, "height": 32})).unwrap();
    app.run("layer.new.layer", json!({})).unwrap();
    app
}

fn effect(app: &PhotocraftApp) -> &Effect {
    let state = app.session.active().unwrap();
    &state.doc.layer(state.active_layer.unwrap()).unwrap().effects.items[0]
}

fn fields(app: &PhotocraftApp) -> Map<String, Value> {
    let state = app.session.active().unwrap();
    initial_fields(state.doc.layer(state.active_layer.unwrap()).unwrap(), Some("gradientOverlay"), 120.0)
}

#[test]
fn method_picker_is_pending_until_confirm_and_survives_reopening() {
    let mut app = app();
    app.run("layer.layerStyle.gradientOverlay", json!({"from": "#ff0000", "to": "#0000ff"})).unwrap();
    let original = effect(&app).clone();
    let pending_fields = fields(&app);
    let mut harness = egui_kittest::Harness::builder()
        .with_size(vec2(1000.0, 1000.0))
        .build_ui_state(|ui, state: &mut (PhotocraftApp, Map<String, Value>)| body(&mut state.0, ui, &mut state.1), (app, pending_fields));
    PhotocraftApp::setup_context(&harness.ctx, crate::theme::ThemeKind::ALL[0]);
    harness.run_steps(3);
    harness.get_by_label("Custom").click();
    harness.run_steps(2);
    harness.get_by_label("Smooth").click();
    harness.run_steps(2);
    assert_eq!(effect(&harness.state().0), &original, "cancel can still discard all pending edits");
    assert_eq!(harness.state().1["effects"][0]["params"]["method"], "smooth");
    let (app, pending) = harness.state_mut();
    confirm(app, pending).unwrap();
    let saved = effect(app).clone();
    app.run("edit.undo", json!({})).unwrap();
    assert_eq!(effect(app), &original);
    app.run("edit.redo", json!({})).unwrap();
    assert_eq!(effect(app), &saved);
    let reopened = fields(app);
    assert_eq!(reopened["effects"][0]["params"]["method"], "smooth");
    assert_eq!(reopened["effects"][0]["params"]["from"], "#ff0000");
    confirm(app, &reopened).unwrap();
    assert_eq!(effect(app), &saved, "OK without edits leaves the rendered curve exact");
}

#[test]
fn all_four_methods_roundtrip_through_the_dialog() {
    for method in ["classic", "linear", "perceptual", "smooth"] {
        let mut app = app();
        app.run("layer.layerStyle.gradientOverlay", json!({"from": [0.123456, 0.234567, 0.345678], "to": "#abcdef", "method": method})).unwrap();
        let before = effect(&app).clone();
        let fields = fields(&app);
        assert_eq!(fields["effects"][0]["params"]["method"], method);
        confirm(&mut app, &fields).unwrap();
        assert_eq!(effect(&app), &before);
    }
}
