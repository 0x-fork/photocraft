//! The History Brush's source (#2966): the History panel's source column, at the left of every
//! state, sets which state the History Brush (and Fill › History) restores from. Exactly one
//! source per document; by default the oldest state held (Photoshop's default is the first
//! snapshot, the document as opened, which that state is until it falls off the history).
//!
//! The source is kept by state, not by index ([`photocraft_ops::History::set_brush_source`]), so
//! it stays on its state while older states fall off, and that state is kept when it would fall
//! off the history itself. Once it is deleted, or discarded by a new step after stepping back,
//! there is no source until one is set, and the History Brush refuses to paint without an
//! explicit `state`, as Photoshop does. (Photoshop keeps a source discarded by a new step in the
//! list; that needs states outside the linear history, so here it is gone.)

use std::sync::Arc;

use photocraft_doc::Document;
use serde_json::{Value, json};

use crate::commands::CommandSpec;
use crate::{DocState, EngineError, Result, Session};

const CMD: &str = "history.setSource";

fn bad(msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: CMD.into(), msg: msg.into() }
}

/// The document the History Brush paints from when no `state` is passed: the source state set in
/// the History panel, or the default (the oldest state held).
pub(crate) fn source_doc(st: &DocState) -> Result<Arc<Document>> {
    st.history
        .brush_source()
        .and_then(|i| st.history.row_doc(i, &st.doc))
        .ok_or_else(|| EngineError::Other("the History Brush source state was deleted: set a new source in the History panel".into()))
}

/// `history.setSource {state}`: make a History panel row the History Brush's source. Rows are
/// numbered as the panel lists them: the states (0 = oldest, then the current one), then the
/// greyed states a step back left redoable.
fn set_source(s: &mut Session, p: &Value) -> Result<Value> {
    if p.get("snapshot").is_some_and(|v| !v.is_null()) {
        return Err(EngineError::Other("History snapshots are not supported yet: set the source to a `state`".into()));
    }
    let v = match p.get("state") {
        None | Some(Value::Null) => return Err(bad("pass the `state` to paint from (a History panel row, 0 = oldest)")),
        Some(v) => v,
    };
    let st = s.active_mut().ok_or(EngineError::NoDocument)?;
    let rows = st.history.entries().len() + st.history.redo_labels().count();
    let i = v.as_u64().and_then(|i| usize::try_from(i).ok()).ok_or_else(|| bad(format!("`state` must be a History panel row (0..{rows}), not {v}")))?;
    if !st.history.set_brush_source(i) {
        return Err(bad(format!("no history state {i} (0..{rows})")));
    }
    Ok(source_json(st))
}

/// The source as `doc_inspect` reports it: its History panel row and label (`null` when its state
/// was deleted).
pub fn source_json(st: &DocState) -> Value {
    let Some(i) = st.history.brush_source() else { return Value::Null };
    let label = st.history.entries().into_iter().chain(st.history.redo_labels().map(str::to_string)).nth(i);
    json!({ "state": i, "label": label })
}

fn has_doc(s: &Session) -> std::result::Result<(), String> {
    s.active().map(|_| ()).ok_or_else(|| "no document open".into())
}

pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: CMD,
        label: "Set History Brush Source",
        menu: &[],
        shortcut: None,
        params: r##"{"state":index} (History panel row: 0 = oldest state, then the current one, then the greyed redo states; `snapshot` is not supported yet) → {state,label}: paint.historyBrush and Fill › History use this state when no `state` is passed"##,
        enabled: has_doc,
        run: set_source,
        journal: true,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A white 8×8 document filled red, then green: rows Open, Fill (red), Fill (green).
    fn session() -> Session {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 8, "height": 8, "depth": 16, "background": "white"})).unwrap();
        s.execute("edit.fill", json!({"color": "#ff0000"})).unwrap();
        s.execute("edit.fill", json!({"color": "#00ff00"})).unwrap();
        s
    }

    fn rgb(s: &Session) -> [f32; 3] {
        let st = s.active().unwrap();
        let px = st.doc.layers[0].surface().unwrap().rgba(4, 4);
        [px[0], px[1], px[2]]
    }

    fn brush(s: &mut Session, extra: Value) {
        let mut p = json!({"points": [[4, 4]], "size": 40, "hardness": 100});
        if let (Some(p), Some(e)) = (p.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        s.execute("paint.historyBrush", p).unwrap();
    }

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.01)
    }

    #[test]
    fn default_source_is_the_oldest_state() {
        let mut s = session();
        assert_eq!(crate::inspect::document(s.active().unwrap())["historySource"], json!({"state": 0, "label": "Open"}));
        brush(&mut s, json!({}));
        assert!(close(rgb(&s), [1.0, 1.0, 1.0]), "{:?}", rgb(&s));
    }

    #[test]
    fn the_history_brush_and_fill_paint_from_the_set_source() {
        let mut s = session();
        let r = s.execute(CMD, json!({"state": 1})).unwrap();
        assert_eq!(r, json!({"state": 1, "label": "Fill"}));
        assert_eq!(crate::inspect::document(s.active().unwrap())["historySource"]["state"], 1);
        brush(&mut s, json!({}));
        assert!(close(rgb(&s), [1.0, 0.0, 0.0]), "the red state: {:?}", rgb(&s));
        // An explicit `state` still wins.
        brush(&mut s, json!({"state": 0}));
        assert!(close(rgb(&s), [1.0, 1.0, 1.0]), "{:?}", rgb(&s));
        s.execute("edit.fill", json!({"contents": "history"})).unwrap();
        assert!(close(rgb(&s), [1.0, 0.0, 0.0]), "Fill › History uses the source too: {:?}", rgb(&s));
        // Setting the source is not a step.
        assert_eq!(s.active().unwrap().history.entries().len(), 6);
    }

    /// The common Photoshop workflow: apply, step back, set the source on the greyed state, paint.
    #[test]
    fn a_redo_state_can_be_the_source_until_a_new_step_discards_it() {
        let mut s = session();
        assert!(s.undo());
        s.execute(CMD, json!({"state": 2})).unwrap();
        brush(&mut s, json!({}));
        assert!(close(rgb(&s), [0.0, 1.0, 0.0]), "the greyed green state: {:?}", rgb(&s));
        // Painting was a new step, which discarded the green state: no source until one is set.
        assert!(crate::inspect::document(s.active().unwrap())["historySource"].is_null());
        let before = rgb(&s);
        assert!(s.execute("paint.historyBrush", json!({"points": [[4, 4]], "size": 40})).is_err());
        assert!(s.execute("edit.fill", json!({"contents": "history"})).is_err());
        assert!(close(rgb(&s), before), "nothing painted");
        brush(&mut s, json!({"state": 0}));
        assert!(close(rgb(&s), [1.0, 1.0, 1.0]), "an explicit state still paints: {:?}", rgb(&s));
        s.execute(CMD, json!({"state": 0})).unwrap();
        brush(&mut s, json!({}));
    }

    #[test]
    fn the_source_follows_its_state_as_older_states_fall_off() {
        let mut s = session();
        s.active_mut().unwrap().history.max_states = 2;
        s.execute(CMD, json!({"state": 1})).unwrap();
        s.execute("edit.fill", json!({"color": "#0000ff"})).unwrap();
        // Open fell off: the red state is row 0 now and still the source.
        let st = s.active().unwrap();
        assert_eq!((st.history.entries().len(), st.history.brush_source()), (3, Some(0)));
        // It would fall off next: it is kept, as in Photoshop, and paints red.
        s.execute("edit.fill", json!({"color": "#ffffff"})).unwrap();
        brush(&mut s, json!({}));
        let st = s.active().unwrap();
        assert_eq!((st.history.entries()[0].as_str(), st.history.brush_source()), ("Fill", Some(0)));
        assert!(close(rgb(&s), [1.0, 0.0, 0.0]), "{:?}", rgb(&s));
    }

    #[test]
    fn deleting_the_source_state_leaves_no_source() {
        let mut s = session();
        s.execute(CMD, json!({"state": 2})).unwrap();
        s.execute("history.deleteState", json!({"state": 2})).unwrap();
        assert!(crate::inspect::document(s.active().unwrap())["historySource"].is_null());
        assert!(s.execute("paint.historyBrush", json!({"points": [[4, 4]], "size": 40})).is_err());
        s.execute(CMD, json!({"state": 1})).unwrap();
        brush(&mut s, json!({}));
        assert!(close(rgb(&s), [1.0, 0.0, 0.0]), "{:?}", rgb(&s));
    }

    #[test]
    fn bad_params_and_no_document_are_errors() {
        assert!(matches!(Session::new().execute(CMD, json!({"state": 0})), Err(EngineError::NoDocument | EngineError::Disabled(..))));
        let mut s = session();
        for p in [
            json!({}),
            json!({"state": null}),
            json!({"state": -1}),
            json!({"state": 3}),
            json!({"state": 1e300}),
            json!({"state": 1.5}),
            json!({"state": "1"}),
            json!({"state": [1]}),
            json!({"snapshot": 0}),
            json!({"snapshot": "Open", "state": 1}),
        ] {
            assert!(s.execute(CMD, p.clone()).is_err(), "{p}");
        }
        assert_eq!(s.active().unwrap().history.brush_source(), Some(0), "nothing changed");
    }
}
