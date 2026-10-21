//! #2966: a click on a History panel state returns the document there with one command
//! (`history.selectState`), not one Undo / Redo per state, so the canvas refreshes once and only
//! where the jump changed pixels.

use egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use super::*;

/// Open, then New Layer, Fill, New Layer, Fill.
fn harness() -> Harness<'static, PhotocraftApp> {
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
    app.run("file.new", json!({"width": 64, "height": 48})).unwrap();
    for (cmd, p) in
        [("layer.new.layer", json!({})), ("edit.fill", json!({"color": "#ff0000"})), ("layer.new.layer", json!({})), ("edit.fill", json!({"color": "#00ff00"}))]
    {
        app.run(cmd, p).unwrap();
    }
    let mut ready = false;
    let mut h = Harness::builder().with_size(vec2(300.0, 400.0)).build_ui_state(
        move |ui, app: &mut PhotocraftApp| {
            if !ready {
                PhotocraftApp::setup_context(ui.ctx(), theme::ThemeKind::ProMedium);
                ready = true;
                return;
            }
            history(app, ui);
        },
        app,
    );
    h.run_steps(3);
    h
}

fn state(h: &Harness<'_, PhotocraftApp>) -> (usize, usize, u64) {
    let st = h.state().session.active().unwrap();
    (st.history.past_len(), st.history.redo_labels().count(), st.revision)
}

#[test]
fn clicking_a_state_jumps_there_with_one_command() {
    let mut h = harness();
    let (past, redo, rev) = state(&h);
    assert_eq!((past, redo), (4, 0));
    // Back three states: one command, one revision (the canvas sees one change).
    h.get_all_by_label("New Layer").next().expect("the first New Layer row").click();
    h.run_steps(2);
    assert_eq!(state(&h), (1, 3, rev + 1));
    assert_eq!(h.state().perf.last_command, "history.selectState");
    // Forward to the last (dimmed) state, again in one step.
    h.get_all_by_label("Fill").last().expect("the last Fill row").click();
    h.run_steps(2);
    assert_eq!(state(&h), (4, 0, rev + 2));
    // The current state: nothing to do.
    h.get_all_by_label("Fill").last().expect("the last Fill row").click();
    h.run_steps(2);
    assert_eq!(state(&h), (4, 0, rev + 2));
}

/// The ghost label that follows a History row being dragged to the trash (the text painted at
/// the pointer, `layer_transfer::ghost`), if one is drawn.
fn ghost_label(h: &Harness<'_, PhotocraftApp>, p: egui::Pos2) -> Option<String> {
    let at = p + vec2(20.0, -6.0);
    h.output().shapes.iter().find_map(|c| match &c.shape {
        egui::Shape::Text(t) if (t.pos - at).length() < 1.0 => Some(t.galley.job.text.clone()),
        _ => None,
    })
}

/// Allow Non-Linear History lets a later (dimmed) state be dragged onto the trash: its row's
/// label follows the pointer, as a past state's does.
#[test]
fn dragging_a_later_state_shows_its_label_at_the_pointer() {
    let mut h = harness();
    h.state_mut().run("history.options", json!({"allowNonLinearHistory": true})).unwrap();
    h.state_mut().run("edit.undo", json!({})).unwrap();
    h.run_steps(2);
    // Rows: Open, New Layer, Fill, New Layer (current), the dimmed Fill.
    for (label, row) in [("New Layer", 1), ("Fill", 1)] {
        let from = h.get_all_by_label(label).nth(row).expect("the row").rect().center();
        let to = from + vec2(-60.0, 40.0);
        h.hover_at(from);
        h.run_steps(1);
        h.event(egui::Event::PointerButton { pos: from, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE });
        h.run_steps(1);
        for i in 1..=4 {
            h.hover_at(from + (to - from) * (i as f32 / 4.0));
            h.run_steps(1);
        }
        assert_eq!(ghost_label(&h, to).as_deref(), Some(label), "the dragged {label} row's label follows the pointer");
        // Released off the trash: nothing is deleted.
        h.event(egui::Event::PointerButton { pos: to, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE });
        h.run_steps(2);
    }
    assert_eq!(h.state().session.active().unwrap().history.entries().len() + h.state().session.active().unwrap().history.redo_labels().count(), 5);
}
