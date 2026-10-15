use super::*;
use photocraft_doc::text::TextShape;
use photocraft_doc::vector::{Path, PathOp, Subpath};
use photocraft_doc::{LayerContent, LayerId, TextLayer};
use serde_json::json;

fn session(depth: u32) -> Session {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 400, "height": 300, "depth": depth, "background": "white"})).unwrap();
    s
}

fn text_layer(s: &mut Session, text: &str) -> LayerId {
    let r = s.execute("type.create", json!({"x": 100, "y": 100, "text": text, "size": 14, "color": "#000000"})).unwrap();
    LayerId(r["layer"].as_u64().unwrap())
}

fn text(s: &Session, id: LayerId) -> &TextLayer {
    match &s.active().unwrap().doc.layer(id).unwrap().content {
        LayerContent::Text(t) => t,
        c => panic!("not text: {}", c.kind_name()),
    }
}

fn set_work_path(s: &mut Session, p: Path) {
    s.edit("path", |doc, _| {
        doc.work_path = Some(p);
        Ok(())
    })
    .unwrap();
}

const WORDS: &str = "Well, that went better than expected. Now where did the cat hide the keys?";

#[test]
fn fill_path_wraps_text_inside_the_work_path_in_one_step() {
    for depth in [8, 16, 32] {
        let mut s = session(depth);
        let id = text_layer(&mut s, WORDS);
        set_work_path(&mut s, Path::new(vec![Subpath::polygon(&[(50.0, 50.0), (250.0, 50.0), (250.0, 200.0), (50.0, 200.0)])]));
        let steps = s.active().unwrap().history.entries().len();
        let r = s.execute("type.fillPath", json!({})).unwrap();
        assert_eq!(r["layer"], json!(id.0));
        assert_eq!(s.active().unwrap().history.entries().len(), steps + 1, "one history step");
        assert!(matches!(text(&s, id).shape, TextShape::Area { .. }));
        // The ink stays inside the path, in document space.
        let ink = s.active().unwrap().doc.layer(id).unwrap().surface().unwrap().content_bounds();
        assert!(ink.x0 >= 49 && ink.x1 <= 251 && ink.y0 >= 49 && ink.y1 <= 201, "depth {depth}: {ink:?}");
        assert!(ink.height() > 30, "several lines");
        assert!(s.undo());
        assert_eq!(text(&s, id).shape, TextShape::Point);
        assert!(s.redo());
        assert!(matches!(text(&s, id).shape, TextShape::Area { .. }));
    }
}

#[test]
fn fill_path_uses_a_shape_layer_with_its_hole() {
    let mut s = session(8);
    let shape = s.execute("shape.create", json!({"kind": "rect", "rect": [20, 20, 300, 200], "fill": "#cccccc"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("shape.create", json!({"kind": "rect", "rect": [140, 80, 60, 80], "addTo": shape, "op": "subtract"})).unwrap();
    let id = text_layer(&mut s, &WORDS.repeat(3));
    s.execute("type.fillPath", json!({"layer": id.0, "shape": shape})).unwrap();
    let TextShape::Area { path } = &text(&s, id).shape else { panic!("area") };
    assert_eq!(path.subpaths.len(), 2);
    assert_eq!(path.subpaths[1].op, PathOp::Subtract);
}

#[test]
fn fill_path_maps_the_path_into_text_space() {
    let mut s = session(8);
    let id = text_layer(&mut s, WORDS); // transform: translate(100, 100)
    set_work_path(&mut s, Path::new(vec![Subpath::polygon(&[(100.0, 100.0), (300.0, 100.0), (300.0, 250.0), (100.0, 250.0)])]));
    s.execute("type.fillPath", json!({})).unwrap();
    let TextShape::Area { path } = &text(&s, id).shape else { panic!("area") };
    let a = path.subpaths[0].knots[0].anchor;
    assert!((a.x, a.y) == (0.0, 0.0), "{a:?}");
}

#[test]
fn fill_path_rejects_bad_paths_without_history() {
    let mut s = session(8);
    assert!(!s.is_enabled("type.fillPath"), "no type layer");
    let id = text_layer(&mut s, WORDS);
    assert!(s.is_enabled("type.fillPath"));
    let history = |s: &Session| s.active().unwrap().history.entries().len();
    // No work path.
    assert!(s.execute("type.fillPath", json!({})).is_err());
    for bad in [
        Path::new(vec![Subpath::polyline(&[(0.0, 0.0), (90.0, 90.0), (0.0, 90.0)])]), // open only
        Path::new(vec![Subpath::polygon(&[(0.0, 0.0), (90.0, 0.0), (180.0, 0.0)])]),  // no area
        Path::new(vec![Subpath::polygon(&[(0.0, 0.0), (f64::NAN, 0.0), (9.0, 9.0)])]),
        {
            let mut p = Path::new(vec![Subpath::polygon(&[(0.0, 0.0), (90.0, 0.0), (90.0, 90.0)])]);
            p.inverted = true;
            p
        },
        Path::new(vec![Subpath::polygon(&(0..100_001).map(|i| ((i as f64).cos() * 90.0, (i as f64).sin() * 90.0)).collect::<Vec<_>>())]),
    ] {
        set_work_path(&mut s, bad);
        let h = history(&s);
        assert!(s.execute("type.fillPath", json!({})).is_err());
        assert_eq!(history(&s), h, "no history step on error");
        assert_eq!(text(&s, id).shape, TextShape::Point);
    }
    for p in [json!({"shape": "x"}), json!({"shape": u64::MAX}), json!({"layer": u64::MAX})] {
        assert!(s.execute("type.fillPath", p).is_err());
    }
    // A shape param that is not a shape layer.
    assert!(s.execute("type.fillPath", json!({"shape": id.0})).is_err());
    // Locked layer.
    set_work_path(&mut s, Path::new(vec![Subpath::polygon(&[(0.0, 0.0), (200.0, 0.0), (200.0, 200.0)])]));
    s.execute("layer.setProps", json!({"layer": id.0, "locked": true})).unwrap();
    assert!(s.execute("type.fillPath", json!({})).is_err());
}
