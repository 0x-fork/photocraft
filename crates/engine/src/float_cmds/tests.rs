use photocraft_geom::Rect;
use serde_json::json;

use super::*;

/// An 80×60 transparent document whose layer is painted red over (10..30)²; (10..30)² selected.
fn session() -> (Session, LayerId) {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 80, "height": 60, "background": "transparent"})).unwrap();
    let id = s.active().unwrap().active_layer.unwrap();
    s.edit("paint", |doc, _| {
        doc.layer_mut(id).unwrap().surface_mut().unwrap().fill_rect(Rect::new(10, 10, 30, 30), &[1.0, 0.0, 0.0, 1.0]);
        Ok(())
    })
    .unwrap();
    s.execute("select.rect", json!({"x": 10, "y": 10, "width": 20, "height": 20})).unwrap();
    (s, id)
}

fn alpha(d: &Document, id: LayerId, x: i32, y: i32) -> f32 {
    d.layer(id).unwrap().surface().unwrap().rgba(x, y)[3]
}

#[test]
fn float_moves_a_cut_piece_without_touching_the_document_until_dropped() {
    let (mut s, id) = session();
    let steps = s.active().unwrap().history.past_len();
    assert_eq!(s.execute("select.float", json!({"dx": 15, "dy": 0})).unwrap()["offset"], json!([15, 0]));
    // Moving it again doesn't cut anything new.
    s.execute("select.float", json!({"dx": 0, "dy": 10})).unwrap();
    let st = s.active().unwrap();
    assert_eq!(floating(st).unwrap().offset, (15, 10));
    assert_eq!(alpha(&st.doc, id, 12, 12), 1.0, "the document is untouched while floating");
    assert_eq!(st.history.past_len(), steps);
    let shown = displayed(st, (0, 0)).unwrap();
    assert!(alpha(&shown, id, 12, 12) == 0.0 && alpha(&shown, id, 27, 22) == 1.0, "shown cut and moved");
    // Drop: one history step, pixels and selection where they were shown.
    s.execute("select.drop", json!({})).unwrap();
    let st = s.active().unwrap();
    assert!(floating(st).is_none());
    assert_eq!(st.history.past_len(), steps + 1);
    for (x, y) in [(12, 12), (27, 22), (44, 39), (40, 40)] {
        assert_eq!(alpha(&st.doc, id, x, y), alpha(&shown, id, x, y), "dropped as shown at ({x}, {y})");
    }
    assert_eq!(st.doc.selection.as_ref().unwrap().content_bounds(), Rect::new(25, 20, 45, 40));
    // Undo takes the whole move back.
    s.undo();
    assert_eq!(alpha(&s.active().unwrap().doc, id, 12, 12), 1.0);
}

#[test]
fn any_other_command_drops_it_and_undo_puts_it_back() {
    let (mut s, id) = session();
    let steps = s.active().unwrap().history.past_len();
    s.execute("select.float", json!({"dx": 20, "dy": 0})).unwrap();
    // Undo while floating: the piece goes back; nothing in the document or history changed.
    assert_eq!(s.execute("edit.undo", json!({})).unwrap()["floating"], json!("returned"));
    let st = s.active().unwrap();
    assert!(floating(st).is_none() && alpha(&st.doc, id, 12, 12) == 1.0 && st.history.past_len() == steps);
    // Float again, then deselect: dropped first (where it was shown), then deselected.
    s.execute("select.float", json!({"dx": 20, "dy": 0})).unwrap();
    s.execute("select.deselect", json!({})).unwrap();
    let st = s.active().unwrap();
    assert!(st.doc.selection.is_none() && floating(st).is_none());
    assert!(alpha(&st.doc, id, 12, 12) == 0.0 && alpha(&st.doc, id, 35, 20) == 1.0);
}

#[test]
fn needs_a_selection_and_an_unlocked_pixel_layer() {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 20, "height": 20})).unwrap();
    assert!(!s.is_enabled("select.float"), "no selection");
    s.execute("select.all", json!({})).unwrap();
    assert!(s.is_enabled("select.float"), "the Background's selected pixels move");
    s.execute("layer.new.layer", json!({})).unwrap();
    assert!(s.is_enabled("select.float"));
    s.execute("layer.lockLayers", json!({"position": true})).unwrap();
    assert!(!s.is_enabled("select.float"), "a position-locked layer");
}

/// On the Background, which can't hold transparency, the hole takes the background colour.
#[test]
fn on_the_background_the_hole_takes_the_background_colour() {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 40, "height": 40})).unwrap();
    s.tools.background = [0.0, 0.0, 1.0, 1.0];
    let id = s.active().unwrap().active_layer.unwrap();
    s.execute("select.rect", json!({"x": 10, "y": 10, "width": 10, "height": 10})).unwrap();
    s.execute("select.float", json!({"dx": 15, "dy": 0})).unwrap();
    s.execute("select.drop", json!({})).unwrap();
    let px = |x, y| s.active().unwrap().doc.layer(id).unwrap().surface().unwrap().rgba(x, y);
    assert_eq!(px(12, 12), [0.0, 0.0, 1.0, 1.0], "the hole");
    assert_eq!(px(27, 12), [1.0, 1.0, 1.0, 1.0], "the moved pixels");
    assert_eq!(px(5, 5), [1.0, 1.0, 1.0, 1.0], "the rest");
}

/// Copying leaves the original pixels, at every depth; the move is one undo step.
#[test]
fn a_copy_leaves_the_original_at_every_depth_in_one_undo_step() {
    for depth in [8, 16, 32] {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 80, "height": 60, "background": "transparent", "depth": depth})).unwrap();
        let id = s.active().unwrap().active_layer.unwrap();
        s.execute("select.rect", json!({"x": 10, "y": 10, "width": 20, "height": 20})).unwrap();
        s.execute("edit.fill", json!({"color": "#ff0000"})).unwrap();
        let steps = s.active().unwrap().history.past_len();
        s.execute("select.float", json!({"dx": 30, "dy": 0, "copy": true})).unwrap();
        s.execute("select.drop", json!({})).unwrap();
        let st = s.active().unwrap();
        assert!(alpha(&st.doc, id, 12, 12) == 1.0 && alpha(&st.doc, id, 42, 12) == 1.0, "{depth}-bit: both");
        assert_eq!((st.history.past_len(), st.history.undo_label()), (steps + 1, Some("Move")), "{depth}-bit");
        s.undo();
        let st = s.active().unwrap();
        assert!(alpha(&st.doc, id, 12, 12) == 1.0 && alpha(&st.doc, id, 42, 12) == 0.0, "{depth}-bit: undone");
    }
}

/// `cargo test --release -p photocraft-engine float_drag_bench -- --ignored --nocapture`
#[test]
#[ignore]
fn float_drag_bench() {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 6000, "height": 4000, "background": "transparent"})).unwrap();
    s.execute("layer.new.layer", json!({})).unwrap();
    s.edit("paint", |doc, a| {
        doc.layer_mut(a.unwrap()).unwrap().surface_mut().unwrap().fill_rect(Rect::new(0, 0, 6000, 4000), &[0.2, 0.4, 0.6, 1.0]);
        Ok(())
    })
    .unwrap();
    s.execute("select.rect", json!({"x": 1000, "y": 1000, "width": 500, "height": 400})).unwrap();
    let t = std::time::Instant::now();
    s.execute("select.float", json!({})).unwrap();
    let cut = t.elapsed();
    let t = std::time::Instant::now();
    for i in 0..20 {
        displayed(s.active().unwrap(), (i * 7, i * 3)).unwrap();
    }
    eprintln!("6000×4000 layer, 500×400 piece: cut {cut:?}, then {:?} per pointer move", t.elapsed() / 20);
}

#[test]
fn copy_floats_a_duplicate_and_leaves_the_original() {
    let (mut s, id) = session();
    s.execute("select.float", json!({"dx": 30, "dy": 0, "copy": true})).unwrap();
    let shown = displayed(s.active().unwrap(), (0, 0)).unwrap();
    assert!(alpha(&shown, id, 12, 12) == 1.0 && alpha(&shown, id, 42, 12) == 1.0, "original kept, copy shown");
    s.execute("select.drop", json!({})).unwrap();
    let doc = &s.active().unwrap().doc;
    assert!(alpha(doc, id, 12, 12) == 1.0 && alpha(doc, id, 42, 12) == 1.0, "dropped: both");
}

#[test]
fn float_with_the_mask_target_moves_the_mask_not_the_layer() {
    let (mut s, id) = session();
    s.edit("mask", |doc, _| {
        let l = doc.layer_mut(id).unwrap();
        let mut m = photocraft_doc::LayerMask::reveal_all();
        m.surface.fill_rect(Rect::new(10, 10, 30, 30), &[0.0]);
        l.mask = Some(m);
        Ok(())
    })
    .unwrap();
    let mask_at = |s: &Session, x, y| s.active().unwrap().doc.layer(id).unwrap().mask.as_ref().unwrap().surface.sample_channel(x, y, 0);
    s.execute("select.float", json!({"dx": 25, "dy": 0, "copy": true, "target": "mask"})).unwrap();
    s.execute("select.drop", json!({})).unwrap();
    assert_eq!(mask_at(&s, 12, 12), 0.0, "the copy leaves the mask where it was");
    assert_eq!(mask_at(&s, 37, 12), 0.0, "the mask's selected part moved");
    assert_eq!(mask_at(&s, 60, 12), 1.0);
    assert_eq!(alpha(&s.active().unwrap().doc, id, 37, 12), 0.0, "the layer's pixels did not move");
    assert_eq!(alpha(&s.active().unwrap().doc, id, 12, 12), 1.0);
}

#[test]
fn cutting_an_untouched_part_of_the_mask_moves_it_and_leaves_the_background_colour() {
    let (mut s, id) = session();
    s.edit("mask", |doc, _| {
        let mut m = photocraft_doc::LayerMask::hide_all();
        m.surface.fill_rect(Rect::new(50, 10, 70, 30), &[1.0]);
        doc.layer_mut(id).unwrap().mask = Some(m);
        Ok(())
    })
    .unwrap();
    s.tools.background = [0.5, 0.5, 0.5, 1.0];
    let mask_at = |s: &Session, x, y| s.active().unwrap().doc.layer(id).unwrap().mask.as_ref().unwrap().surface.sample_channel(x, y, 0);
    // (10..30)² is the mask's untouched black; it moves over the white at (50..70, 10..30).
    s.execute("select.float", json!({"dx": 40, "dy": 0, "target": "mask"})).unwrap();
    s.execute("select.drop", json!({})).unwrap();
    assert_eq!(mask_at(&s, 55, 12), 0.0, "the untouched black moved");
    assert!((mask_at(&s, 12, 12) - 0.5).abs() < 0.01, "the hole takes the background colour");
    assert_eq!(mask_at(&s, 40, 12), 0.0);
}

/// Coverage of `d`'s selection at (x, y).
fn selected(d: &Document, x: i32, y: i32) -> f32 {
    d.selection.as_ref().map_or(0.0, |s| s.sample_channel(x, y, 0))
}

/// #7538: a marquee drawn loosely around an object on a transparent layer shrinks to the object
/// once its pixels move, as in Photoshop, at every depth and when moving a copy.
#[test]
fn moving_shrinks_the_selection_to_the_moved_pixels_at_every_depth() {
    for depth in [8, 16, 32] {
        for copy in [false, true] {
            let mut s = Session::new();
            s.execute("file.new", json!({"width": 80, "height": 60, "background": "transparent", "depth": depth})).unwrap();
            s.execute("select.rect", json!({"x": 10, "y": 10, "width": 20, "height": 20})).unwrap();
            s.execute("edit.fill", json!({"color": "#ff0000"})).unwrap();
            // A marquee with empty space all round the red square.
            s.execute("select.rect", json!({"x": 4, "y": 6, "width": 34, "height": 30})).unwrap();
            s.execute("select.float", json!({"dx": 20, "dy": 5, "copy": copy})).unwrap();
            let st = s.active().unwrap();
            let outline = floating(st).unwrap().parts.outline().unwrap();
            assert_eq!(outline.content_bounds(), Rect::new(10, 10, 30, 30), "{depth}-bit, copy {copy}: the outline while floating");
            assert_eq!(st.doc.selection.as_ref().unwrap().content_bounds(), Rect::new(4, 6, 38, 36), "the document's is untouched");
            s.execute("select.drop", json!({})).unwrap();
            let doc = &s.active().unwrap().doc;
            assert_eq!(doc.selection.as_ref().unwrap().content_bounds(), Rect::new(30, 15, 50, 35), "{depth}-bit, copy {copy}");
            assert_eq!((selected(doc, 40, 25), selected(doc, 26, 13)), (1.0, 0.0), "{depth}-bit, copy {copy}: the moved square only");
            // Undo brings back the marquee as it was drawn.
            s.undo();
            assert_eq!(s.active().unwrap().doc.selection.as_ref().unwrap().content_bounds(), Rect::new(4, 6, 38, 36), "{depth}-bit, copy {copy}");
        }
    }
}

/// The new selection is the old one where the layer has pixels: a soft selection keeps its soft
/// edge, and half-transparent pixels stay as selected as they were, so they move whole next time.
#[test]
fn the_outline_keeps_soft_edges_and_selects_half_transparent_pixels_as_before() {
    let (mut s, id) = session();
    s.edit("soften", |doc, _| {
        // Half-transparent red over (30..40, 10..30), beside the opaque (10..30)² square.
        doc.layer_mut(id).unwrap().surface_mut().unwrap().fill_rect(Rect::new(30, 10, 40, 30), &[1.0, 0.0, 0.0, 0.5]);
        let mut sel = Surface::new(PixelFormat::GRAY8);
        sel.fill_rect(Rect::new(0, 0, 60, 50), &[0.5]);
        sel.fill_rect(Rect::new(10, 10, 20, 30), &[1.0]);
        doc.selection = Some(sel);
        Ok(())
    })
    .unwrap();
    s.execute("select.float", json!({"dx": 10, "dy": 0})).unwrap();
    s.execute("select.drop", json!({})).unwrap();
    let doc = &s.active().unwrap().doc;
    let near = |x: i32, y: i32, want: f32| assert!((selected(doc, x, y) - want).abs() < 0.01, "({x}, {y}): {} for {want}", selected(doc, x, y));
    near(25, 20, 1.0); // fully selected, opaque
    near(35, 20, 0.5); // half selected, opaque
    near(45, 20, 0.5); // half selected, half transparent: as selected as before
    near(5, 5, 0.0); // selected, but nothing there
    assert_eq!(doc.selection.as_ref().unwrap().content_bounds(), Rect::new(20, 10, 50, 30));
}

/// Where the selection is the moved piece already (the Background, which has no transparency)
/// or the piece has no outline (a mask, or only transparent pixels), the selection just moves.
#[test]
fn the_selection_just_moves_without_transparency_to_shrink_to() {
    // The Background.
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 40, "height": 40})).unwrap();
    s.execute("select.rect", json!({"x": 5, "y": 5, "width": 10, "height": 10})).unwrap();
    s.execute("select.float", json!({"dx": 15, "dy": 0})).unwrap();
    s.execute("select.drop", json!({})).unwrap();
    assert_eq!(s.active().unwrap().doc.selection.as_ref().unwrap().content_bounds(), Rect::new(20, 5, 30, 15), "the Background");
    // Only transparent pixels selected.
    let (mut s, _) = session();
    s.execute("select.rect", json!({"x": 40, "y": 5, "width": 10, "height": 10})).unwrap();
    s.execute("select.float", json!({"dx": 5, "dy": 0})).unwrap();
    assert!(floating(s.active().unwrap()).unwrap().parts.outline().is_none());
    s.execute("select.drop", json!({})).unwrap();
    assert_eq!(s.active().unwrap().doc.selection.as_ref().unwrap().content_bounds(), Rect::new(45, 5, 55, 15), "an empty piece");
    // The mask: it has no transparency of its own.
    let (mut s, id) = session();
    s.edit("mask", |doc, _| {
        doc.layer_mut(id).unwrap().mask = Some(photocraft_doc::LayerMask::reveal_all());
        Ok(())
    })
    .unwrap();
    s.execute("select.rect", json!({"x": 5, "y": 5, "width": 40, "height": 40})).unwrap();
    s.execute("select.float", json!({"dx": 5, "dy": 0, "target": "mask"})).unwrap();
    s.execute("select.drop", json!({})).unwrap();
    assert_eq!(s.active().unwrap().doc.selection.as_ref().unwrap().content_bounds(), Rect::new(10, 5, 50, 45), "the mask");
}

/// #7538: an anti-aliased shape moved, dropped, then moved again from its shrunk selection leaves
/// nothing behind. With the selection shrunk to the pixels' alpha, the second move left
/// a × (1 − a) of each edge pixel (up to 25 %), a faint outline where the shape had been.
#[test]
fn moving_again_from_the_shrunk_selection_leaves_no_edge_behind() {
    for depth in [8, 16, 32] {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 120, "height": 100, "background": "transparent", "depth": depth})).unwrap();
        s.execute("select.rect", json!({"x": 20, "y": 20, "width": 50, "height": 40, "ellipse": true})).unwrap();
        s.execute("edit.fill", json!({"color": "#0000ff"})).unwrap();
        let id = s.active().unwrap().active_layer.unwrap();
        let total = |doc: &Document| (0..100).flat_map(|y| (0..120).map(move |x| (x, y))).map(|(x, y)| alpha(doc, id, x, y)).sum::<f32>();
        let (before, soft) = {
            let doc = &s.active().unwrap().doc;
            (total(doc), (0..100).flat_map(|y| (0..120).map(move |x| (x, y))).filter(|&(x, y)| (0.01..0.99).contains(&alpha(doc, id, x, y))).count())
        };
        assert!(soft > 20, "{depth}-bit: the ellipse has anti-aliased edges ({soft})");
        s.execute("select.rect", json!({"x": 10, "y": 10, "width": 70, "height": 60})).unwrap();
        s.execute("select.float", json!({"dx": 40, "dy": 30})).unwrap();
        s.execute("select.drop", json!({})).unwrap();
        // The ellipse now sits at (60..110, 50..90); move it back where it started.
        s.execute("select.float", json!({"dx": -40, "dy": -30})).unwrap();
        s.execute("select.drop", json!({})).unwrap();
        let doc = &s.active().unwrap().doc;
        let leftover = (50..100)
            .flat_map(|y| (55..115).map(move |x| (x, y)))
            .filter(|&(x, y)| x >= 72 || y >= 62)
            .map(|(x, y)| alpha(doc, id, x, y))
            .fold(0.0f32, f32::max);
        assert_eq!(leftover, 0.0, "{depth}-bit: nothing left where it was moved from");
        assert!((total(doc) - before).abs() < 0.05, "{depth}-bit: every edge pixel came back whole ({} vs {before})", total(doc));
    }
}
