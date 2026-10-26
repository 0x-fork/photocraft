//! Multi-layer selection and the commands that act on it: ⌘/⇧-click selection, Select › All /
//! Deselect / Find Layers, Layer › Align and Distribute, Link Layers, Merge Layers, Group from
//! Layers, Arrange › Reverse, Lock Layers and Rename Layer.
//!
//! The selection itself lives in [`DocState::selected_layers`] (UI state, like the active layer):
//! changing it never adds a history step. Every structural change below is one history step.

use photocraft_color::PixelFormat;
use photocraft_doc::{Document, Layer, LayerContent, LayerId, VideoData};
use photocraft_geom::Rect;
use serde_json::{Value, json};

use crate::commands::{CommandSpec, int_i32};
use crate::{DocState, EngineError, Result, Session};

// ---------- selection state ----------

fn doc_state(s: &Session) -> std::result::Result<&DocState, String> {
    s.active().ok_or_else(|| "no document open".into())
}

/// Selected layers of the active document, bottom-to-top.
pub fn selected(s: &Session) -> Vec<LayerId> {
    s.active().map(DocState::selected_layers).unwrap_or_default()
}

/// The layers a layer command acts on: the `layer` param when given, else every selected layer
/// (bottom-to-top), else the active layer. A single layer behaves exactly as before.
pub(crate) fn targets(s: &Session, p: &serde_json::Value) -> Result<Vec<LayerId>> {
    if p.get("layer").is_some() {
        return Ok(vec![crate::commands::layer_param(s, p)?]);
    }
    match selected(s) {
        sel if sel.is_empty() => Ok(vec![crate::commands::layer_param(s, p)?]),
        sel => Ok(sel),
    }
}

/// [`targets`] for a command that adds a layer mask: with several layers selected, those that can
/// take one (no layer mask yet, not the Background); one layer is taken as given.
pub(crate) fn mask_targets(s: &Session, p: &serde_json::Value) -> Result<Vec<LayerId>> {
    let ids = targets(s, p)?;
    if ids.len() < 2 {
        return Ok(ids);
    }
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let ids: Vec<LayerId> = ids.into_iter().filter(|id| d.doc.layer(*id).is_some_and(|l| l.mask.is_none() && !d.doc.is_background(l.id))).collect();
    if ids.is_empty() {
        return Err(EngineError::Other("none of the selected layers can take a layer mask".into()));
    }
    Ok(ids)
}

/// Ids in `ids` that have no ancestor also in `ids` (a selected group already carries its
/// selected children), bottom-to-top.
pub(crate) fn top_level(doc: &Document, ids: &[LayerId]) -> Vec<LayerId> {
    let walk = doc.walk();
    let paths: Vec<_> = walk.iter().filter(|(_, _, l)| ids.contains(&l.id)).map(|(p, _, l)| (p.clone(), l.id)).collect();
    paths.iter().filter(|(p, _)| !paths.iter().any(|(q, _)| q.len() < p.len() && p.starts_with(q))).map(|(_, id)| *id).collect()
}

/// Replace the layer selection without a history step (like [`Session::select_layer`]).
pub fn set_selection(s: &mut Session, ids: Vec<LayerId>, active: Option<LayerId>, anchor: Option<LayerId>) -> Result<()> {
    let st = s.active_mut().ok_or(EngineError::NoDocument)?;
    if let Some(bad) = ids.iter().chain(active.iter()).find(|id| st.doc.layer(**id).is_none()) {
        return Err(EngineError::NoLayer(*bad));
    }
    let active = active.or_else(|| ids.last().copied());
    st.selected_layers = ids;
    if let Some(a) = active
        && !st.selected_layers.contains(&a)
    {
        st.selected_layers.push(a);
    }
    st.active_layer = active;
    st.layer_anchor = anchor.or(active);
    crate::fix_selection(st);
    // Selecting layers is not an edit: keep a clean document clean.
    let clean = st.saved_revision == st.revision;
    st.revision += 1;
    if clean {
        st.saved_revision = st.revision;
    }
    st.last_damage = Some(Rect::EMPTY);
    Ok(())
}

/// After a structural edit, make `ids` the selection (dropping any that no longer exist), which
/// is also what the edit's history state targets.
pub(crate) fn reselect(s: &mut Session, ids: Vec<LayerId>, active: Option<LayerId>) {
    if let Some(st) = s.active_mut() {
        st.selected_layers = ids;
        if let Some(a) = active {
            st.active_layer = Some(a);
            st.layer_anchor = Some(a);
        }
        crate::fix_selection(st);
        let layers = st.layer_target();
        st.history.set_current_layers(layers);
    }
}

/// Capture a selection target while its name is still known. Later renames/deletions must not
/// change an earlier recorded selection. Direct, non-recording calls retain their numeric IDs.
pub(crate) fn recorded_selection(s: &Session, mut p: Value) -> Value {
    let Some(id) = p.get("layer").and_then(Value::as_u64).map(LayerId) else { return p };
    let Some(st) = s.active() else { return p };
    let Some(layer) = st.doc.layer(id) else { return p };
    let matches: Vec<_> = st.doc.walk().into_iter().filter(|(_, _, l)| l.name == layer.name).map(|(_, _, l)| l.id).collect();
    let Some(occurrence) = matches.iter().position(|candidate| *candidate == id) else { return p };
    if let Some(fields) = p.as_object_mut() {
        fields.remove("layer");
        fields.insert("name".into(), json!(layer.name));
        if matches.len() > 1 {
            fields.insert("occurrence".into(), json!(occurrence));
        }
    }
    p
}

/// The layers named `name`, in document order, other than the ones being moved and (groups) their
/// contents. Moving layers never changes the others' order, so an occurrence counted among these
/// means the same layer before the move (when an action replays it) and after it (when the step
/// is recorded).
fn same_named_targets(doc: &photocraft_doc::Document, name: &str, moved: &[LayerId]) -> Vec<LayerId> {
    fn subtree(l: &Layer, out: &mut Vec<LayerId>, depth: usize) {
        out.push(l.id);
        if depth < 256
            && let Some(children) = l.children()
        {
            children.iter().for_each(|c| subtree(c, out, depth + 1));
        }
    }
    let mut skip = Vec::new();
    for l in moved.iter().filter_map(|id| doc.layer(*id)) {
        subtree(l, &mut skip, 0);
    }
    doc.walk().into_iter().filter(|(_, _, l)| l.name == name && !skip.contains(&l.id)).map(|(_, _, l)| l.id).collect()
}

/// A Layers-panel drag of the selected layer records its drop target by name (and which of
/// several same-named layers it is), as [`recorded_selection`] does: the id means nothing in the
/// document the action plays in later, so playback failed with "no such layer" (#7845). Drags of
/// several layers, or of a layer that isn't the selected one, keep their ids.
fn recorded_move(s: &Session, mut p: Value, pre_selected: Option<&[LayerId]>) -> Value {
    if p.get("layers").is_some() {
        return p;
    }
    let Some(st) = s.active() else { return p };
    let moved = match p.get("layer") {
        None => st.active_layer,
        Some(v) => match v.as_u64().map(LayerId) {
            Some(id) if pre_selected.map_or_else(|| st.selected_layers().as_slice() == [id], |sel| sel == [id]) => Some(id),
            _ => return p,
        },
    };
    if let Some(fields) = p.as_object_mut() {
        fields.remove("layer");
    }
    let Some(id) = p.get("target").and_then(Value::as_u64).map(LayerId) else { return p };
    let Some(layer) = st.doc.layer(id) else { return p };
    // A copy leaves its source where it is (it may even land beside it), so the source stays a
    // candidate; the new copy (the active layer once the step has run, which is when it is
    // recorded) is left out instead, as it doesn't exist yet when the step replays.
    let copy = p.get("copy").and_then(Value::as_bool).unwrap_or(false);
    let skip = if copy { st.active_layer.filter(|a| Some(*a) != moved) } else { moved };
    let matches = same_named_targets(&st.doc, &layer.name, skip.as_slice());
    let Some(occurrence) = matches.iter().position(|candidate| *candidate == id) else { return p };
    if let Some(fields) = p.as_object_mut() {
        fields.insert("target".into(), json!(layer.name));
        if matches.len() > 1 {
            fields.insert("targetOccurrence".into(), json!(occurrence));
        }
    }
    p
}

/// `layer.moveTo`'s drop target: an id, or a name with `targetOccurrence` choosing among layers
/// that share it (in document order, the `moved` layers aside; required when several do).
fn move_target(s: &Session, p: &Value, moved: &[LayerId]) -> Result<LayerId> {
    const CMD: &str = "layer.moveTo";
    match p.get("target") {
        Some(Value::String(name)) => {
            let occurrence = p
                .get("targetOccurrence")
                .map(|v| v.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| bad(CMD, "`targetOccurrence` must be a non-negative integer")))
                .transpose()?;
            let st = s.active().ok_or(EngineError::NoDocument)?;
            let matches = same_named_targets(&st.doc, name, moved);
            if matches.len() > 1 && occurrence.is_none() {
                return Err(bad(CMD, format!("multiple layers named `{name}`; specify `targetOccurrence`")));
            }
            let index = occurrence.unwrap_or(0);
            matches.get(index).copied().ok_or_else(|| bad(CMD, format!("no layer named `{name}` at occurrence {index}")))
        }
        Some(v) => v.as_u64().map(LayerId).ok_or_else(|| bad(CMD, "`target` must be a layer id or name")),
        None => Err(bad(CMD, "missing `target`")),
    }
}

/// Normalise parameters when recording an action step.
///
/// For `layer.select`, captures the layer by name. For other commands, if a numeric `layer`
/// parameter references the document's sole selected layer, normalises it to the target form
/// (omitting `layer`) so that the recorded step applies to whatever layer is targeted when
/// replayed or exported to `.atn` (#7248).
pub(crate) fn recorded_step(s: &Session, id: &str, p: Value, pre_selected: Option<&[LayerId]>) -> Value {
    if id == "layer.select" {
        return recorded_selection(s, p);
    }
    if id == "layer.moveTo" {
        return recorded_move(s, p, pre_selected);
    }
    let Some(layer_id) = p.get("layer").and_then(Value::as_u64).map(LayerId) else { return p };
    let is_sole_selected = match pre_selected {
        Some(sel) => sel == [layer_id],
        None => s.active().is_some_and(|st| st.selected_layers().as_slice() == [layer_id]),
    };
    if is_sole_selected {
        let mut p = p;
        if let Some(fields) = p.as_object_mut() {
            fields.remove("layer");
        }
        return p;
    }
    p
}

fn selection_target(s: &Session, p: &Value) -> Result<LayerId> {
    let Some(name) = p.get("name") else {
        if p.get("occurrence").is_some() {
            return Err(bad("layer.select", "`occurrence` requires `name`"));
        }
        return crate::commands::explicit_layer(s, p)?.ok_or_else(|| bad("layer.select", "needs `layer` or `name`"));
    };
    if p.get("layer").is_some() {
        return Err(bad("layer.select", "use either `layer` or `name`, not both"));
    }
    let name = name.as_str().ok_or_else(|| bad("layer.select", "`name` must be a string"))?;
    let occurrence = p
        .get("occurrence")
        .map(|v| v.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| bad("layer.select", "`occurrence` must be a non-negative integer")))
        .transpose()?;
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let matches: Vec<_> = st.doc.walk().into_iter().filter(|(_, _, l)| l.name == name).map(|(_, _, l)| l.id).collect();
    if matches.len() > 1 && occurrence.is_none() {
        return Err(bad("layer.select", format!("multiple layers named `{name}`; specify `occurrence`")));
    }
    let index = occurrence.unwrap_or(0);
    matches.get(index).copied().ok_or_else(|| bad("layer.select", format!("no layer named `{name}` at occurrence {index}")))
}

/// Layer rows follow the Layers panel's visual order, not Document::walk's storage order.
/// Closed groups hide their children unless both endpoints were selected programmatically.
fn range_order(layers: &[Layer], include_collapsed: bool, out: &mut Vec<LayerId>) {
    for layer in layers.iter().rev() {
        out.push(layer.id);
        if let LayerContent::Group(group) = &layer.content
            && (group.expanded || include_collapsed)
        {
            range_order(&group.children, include_collapsed, out);
        }
    }
}

/// `layer.select`: `{"layer": id, "mode": "replace|toggle|range|add|remove"}`.
pub fn select(s: &mut Session, p: &Value) -> Result<Value> {
    let id = selection_target(s, p)?;
    let mode = p.get("mode").and_then(Value::as_str).unwrap_or("replace");
    let st = s.active().ok_or(EngineError::NoDocument)?;
    if st.doc.layer(id).is_none() {
        return Err(EngineError::NoLayer(id));
    }
    let mut sel = st.selected_layers();
    match mode {
        "replace" => s.select_layer(id)?,
        "add" => {
            if !sel.contains(&id) {
                sel.push(id);
            }
            set_selection(s, sel, Some(id), Some(id))?;
        }
        "toggle" => {
            if sel.contains(&id) {
                if sel.len() == 1 {
                    // Photoshop keeps at least the clicked layer targeted.
                    return Ok(json!({"selected": [id.0]}));
                }
                sel.retain(|x| *x != id);
                let active = if st.active_layer == Some(id) { sel.last().copied() } else { st.active_layer };
                set_selection(s, sel, active, active)?;
            } else {
                sel.push(id);
                set_selection(s, sel, Some(id), Some(id))?;
            }
        }
        // Toggle's removing half: a layer that isn't selected stays unselected (Photoshop's
        // `removeFromSelection`, recorded by actions).
        "remove" => {
            if sel.contains(&id) && sel.len() > 1 {
                sel.retain(|x| *x != id);
                let active = if st.active_layer == Some(id) { sel.last().copied() } else { st.active_layer };
                set_selection(s, sel, active, active)?;
            }
        }
        "range" => {
            let anchor = st.layer_anchor.filter(|a| st.doc.layer(*a).is_some()).or(st.active_layer).unwrap_or(id);
            let mut order = Vec::new();
            range_order(&st.doc.layers, false, &mut order);
            // Engine clients may target a child of a collapsed group that the panel cannot
            // click. Retain that capability by using expanded visual order for such calls.
            if !order.contains(&anchor) || !order.contains(&id) {
                order.clear();
                range_order(&st.doc.layers, true, &mut order);
            }
            let (a, b) = (order.iter().position(|x| *x == anchor).unwrap_or(0), order.iter().position(|x| *x == id).unwrap_or(0));
            let range = order[a.min(b)..=a.max(b)].to_vec();
            set_selection(s, range, Some(id), Some(anchor))?;
        }
        other => return Err(bad("layer.select", format!("unknown mode `{other}`"))),
    }
    Ok(json!({"selected": selected(s).iter().map(|l| l.0).collect::<Vec<_>>()}))
}

fn select_all_layers(s: &mut Session) -> Result<Value> {
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let ids: Vec<LayerId> = d.doc.walk().into_iter().filter(|(_, _, l)| !d.doc.is_background(l.id)).map(|(_, _, l)| l.id).collect();
    if ids.is_empty() {
        return Err(EngineError::Other("there are no layers to select".into()));
    }
    let active = d.active_layer.filter(|a| ids.contains(a)).or(ids.last().copied());
    let n = ids.len();
    set_selection(s, ids, active, None)?;
    Ok(json!({"selected": n}))
}

fn find_layers(s: &mut Session, p: &Value) -> Result<Value> {
    let needle = p.get("name").and_then(Value::as_str).ok_or_else(|| bad("select.findLayers", "missing `name`"))?.to_lowercase();
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let ids: Vec<LayerId> = d.doc.walk().into_iter().filter(|(_, _, l)| l.name.to_lowercase().contains(&needle)).map(|(_, _, l)| l.id).collect();
    if ids.is_empty() {
        return Err(EngineError::Other(format!("no layer name contains \"{needle}\"")));
    }
    let out = ids.iter().map(|l| l.0).collect::<Vec<_>>();
    set_selection(s, ids, None, None)?;
    Ok(json!({"selected": out}))
}

fn select_linked(s: &mut Session) -> Result<Value> {
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let groups = link_groups(&d.doc, &d.selected_layers());
    let ids: Vec<LayerId> = d.doc.walk().into_iter().filter(|(_, _, l)| l.link_group.is_some_and(|g| groups.contains(&g))).map(|(_, _, l)| l.id).collect();
    let active = d.active_layer.filter(|a| ids.contains(a)).or(ids.last().copied());
    let n = ids.len();
    set_selection(s, ids, active, active)?;
    Ok(json!({"selected": n}))
}

// ---------- enabled predicates ----------

fn has_layer(s: &Session) -> std::result::Result<(), String> {
    if selected(s).is_empty() { Err("no layer selected".into()) } else { Ok(()) }
}
fn has_doc(s: &Session) -> std::result::Result<(), String> {
    doc_state(s).map(|_| ())
}
fn two_plus(s: &Session) -> std::result::Result<(), String> {
    if selected(s).len() >= 2 { Ok(()) } else { Err("select two or more layers".into()) }
}
fn can_align(s: &Session) -> std::result::Result<(), String> {
    let d = doc_state(s)?;
    match d.selected_layers().len() {
        0 => Err("no layer selected".into()),
        // Photoshop aligns to an active selection however many layers are selected; without one,
        // a single layer aligns to the canvas and 2+ align to each other.
        _ => Ok(()),
    }
}
fn can_distribute(s: &Session) -> std::result::Result<(), String> {
    if selected(s).len() >= 3 { Ok(()) } else { Err("select three or more layers to distribute".into()) }
}
fn can_link(s: &Session) -> std::result::Result<(), String> {
    let d = doc_state(s)?;
    let sel = d.selected_layers();
    match sel.len() {
        0 => Err("no layer selected".into()),
        1 if d.doc.layer(sel[0]).is_none_or(|l| l.link_group.is_none()) => Err("select two or more layers to link".into()),
        _ => Ok(()),
    }
}
fn has_linked(s: &Session) -> std::result::Result<(), String> {
    let d = doc_state(s)?;
    if link_groups(&d.doc, &d.selected_layers()).is_empty() { Err("the selected layers are not linked".into()) } else { Ok(()) }
}

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

// ---------- moving layers ----------

fn link_groups(doc: &Document, ids: &[LayerId]) -> Vec<u64> {
    let mut g: Vec<u64> = ids.iter().filter_map(|id| doc.layer(*id)?.link_group).collect();
    g.sort_unstable();
    g.dedup();
    g
}

/// `ids` plus every layer linked to one of them.
pub(crate) fn with_links(doc: &Document, ids: &[LayerId]) -> Vec<LayerId> {
    let groups = link_groups(doc, ids);
    let mut out = ids.to_vec();
    if !groups.is_empty() {
        for (_, _, l) in doc.walk() {
            if l.link_group.is_some_and(|g| groups.contains(&g)) && !out.contains(&l.id) {
                out.push(l.id);
            }
        }
    }
    out
}

/// Move layers by whole pixels the way the Move tool does: pixels, linked masks, type and
/// vector content (shapes, linked vector masks) all move together. `before` is `doc` as it was
/// before any of the moves (the edit's starting document), read when type and shapes re-render.
pub(crate) fn move_layers(doc: &mut Document, before: &Document, moves: &[(LayerId, i32, i32)]) -> Result<()> {
    for &(id, dx, dy) in moves {
        if dx == 0 && dy == 0 {
            continue;
        }
        let locks = doc.effective_locks(id);
        let l = doc.layer_mut(id).ok_or(EngineError::NoLayer(id))?;
        if locks.position || locks.all {
            return Err(EngineError::Other(format!("layer \"{}\" is position-locked", l.name)));
        }
        crate::commands::translate_layer(before, l, dx, dy);
        crate::vector_cmds::translate_vectors(before, l, dx as f64, dy as f64);
    }
    Ok(())
}

/// Does a move aimed at the layer mask (`"target":"mask"`) move `l`'s mask alone? Only an
/// unlinked mask does (the chain off); a linked one moves with its layer, as in Photoshop.
pub fn moves_mask_alone(l: &Layer) -> bool {
    l.mask.as_ref().is_some_and(|m| !m.linked)
}

/// `layer.translate`: the explicit layer, or every selected layer, plus their linked layers.
/// With `"target":"mask"` a layer whose mask is unlinked moves that mask alone.
pub fn translate(s: &mut Session, p: &Value) -> Result<Value> {
    let (dx, dy) = match p.get("units").and_then(Value::as_str) {
        None | Some("px") => (int_i32("layer.translate", p, "dx")?.unwrap_or(0), int_i32("layer.translate", p, "dy")?.unwrap_or(0)),
        // Points (1/72 in), as a Photoshop action records a nudge: whole pixels at this
        // document's resolution.
        Some("pt") => {
            let dpi = s.active().map_or(72.0, |d| f64::from(d.doc.resolution_dpi));
            let to_px = |key: &str| -> Result<i32> {
                let pt = match p.get(key) {
                    None => 0.0,
                    Some(v) => v.as_f64().filter(|f| f.is_finite()).ok_or_else(|| bad("layer.translate", format!("`{key}` must be a number")))?,
                };
                let px = (pt * dpi / 72.0).round();
                if px.abs() > f64::from(i32::MAX) {
                    return Err(bad("layer.translate", format!("`{key}` = {pt} pt is outside the 32-bit coordinate range")));
                }
                Ok(px as i32)
            };
            (to_px("dx")?, to_px("dy")?)
        }
        Some(other) => return Err(bad("layer.translate", format!("unknown units `{other}` (px|pt)"))),
    };
    let carry = match p.get("selection") {
        None | Some(Value::Null) => false,
        Some(v) => v.as_bool().ok_or_else(|| bad("layer.translate", "`selection` must be a boolean"))?,
    };
    if dx == 0 && dy == 0 {
        return Ok(Value::Null);
    }
    let roots = match crate::commands::explicit_layer(s, p)? {
        Some(id) => vec![id],
        None => selected(s),
    };
    if roots.is_empty() {
        return Err(EngineError::Other("no active layer".into()));
    }
    let mask_target = p.get("target").and_then(Value::as_str) == Some("mask");
    let before = s.active().ok_or(EngineError::NoDocument)?.doc.clone();
    let ids = s.edit("Move", |doc, _| {
        let (alone, rest): (Vec<LayerId>, Vec<LayerId>) = roots.iter().partition(|id| mask_target && doc.layer(**id).is_some_and(moves_mask_alone));
        let mut ids = move_targets(doc, &rest);
        if ids.is_empty() && alone.is_empty() {
            return Err(EngineError::NoLayer(roots[0]));
        }
        let moves: Vec<_> = ids.iter().map(|&id| (id, dx, dy)).collect();
        move_layers(doc, &before, &moves)?;
        for id in alone {
            let locks = doc.effective_locks(id);
            let l = doc.layer_mut(id).ok_or(EngineError::NoLayer(id))?;
            if locks.position || locks.all {
                return Err(EngineError::Other(format!("layer \"{}\" is position-locked", l.name)));
            }
            if let Some(m) = &mut l.mask {
                m.surface = shift_surface(&m.surface, dx, dy);
            }
            ids.push(id);
        }
        // A board moved past any canvas edge grows the canvas, as a new one does (#1531, #3070).
        if ids.iter().any(|id| doc.layer(*id).is_some_and(|l| l.artboard().is_some())) {
            crate::artboard_cmds::fit_canvas(doc);
        }
        // The Move tool carries the selection along when it moves several layers whole, as
        // Photoshop does (#7538): one step, so Undo takes both back.
        if carry {
            doc.selection = doc.selection.as_ref().map(|sel| shift_surface(sel, dx, dy));
        }
        Ok(ids)
    })?;
    note_damage(s, &before, &ids);
    Ok(Value::Null)
}

/// The layers a move of `roots` takes along: their linked layers, without layers whose group
/// is also moving (the group carries them).
pub fn move_targets(doc: &Document, roots: &[LayerId]) -> Vec<LayerId> {
    top_level(doc, &with_links(doc, roots))
}

/// `doc` with the layers `ids` ([`move_targets`]) moved by whole pixels, as `layer.translate`
/// leaves them: the Move tool's live preview while it drags. Type, shape and smart-object pixels
/// shift as they are instead of re-rendering (the commit re-renders them), so a drag frame costs
/// copying the moving pixels only.
pub fn moved(doc: &Document, ids: &[LayerId], dx: i32, dy: i32) -> Result<Document> {
    moved_target(doc, ids, dx, dy, false)
}

/// [`moved`] for a drag aimed at the layer mask (`mask_target`): unlinked masks move alone.
pub fn moved_target(doc: &Document, ids: &[LayerId], dx: i32, dy: i32, mask_target: bool) -> Result<Document> {
    let mut out = doc.clone();
    for &id in ids {
        let locks = doc.effective_locks(id);
        let l = out.layer_mut(id).ok_or(EngineError::NoLayer(id))?;
        if locks.position || locks.all {
            return Err(EngineError::Other(format!("layer \"{}\" is position-locked", l.name)));
        }
        if mask_target && moves_mask_alone(l) {
            if let Some(m) = &mut l.mask {
                m.surface = shift_surface(&m.surface, dx, dy);
            }
            continue;
        }
        shift_shown(doc, l, dx, dy);
    }
    Ok(out)
}

/// `photocraft_algo::resample::translate_surface` with the content scan cached per tile (a Move
/// drag shifts the same layers every frame; scanning them each time cost more than the copy).
pub(crate) fn shift_surface(s: &photocraft_raster::Surface, dx: i32, dy: i32) -> photocraft_raster::Surface {
    s.translated(dx, dy, photocraft_compose::bounds::content_bounds(s))
}

/// [`crate::commands::translate_layer`] plus the vector side, without re-rendering anything.
fn shift_shown(doc: &Document, l: &mut Layer, dx: i32, dy: i32) {
    use self::shift_surface as translate_surface;
    let a = photocraft_geom::Affine::translate(f64::from(dx), f64::from(dy));
    if let Some(r) = &mut l.effects.reference {
        *r = (r.0 + f64::from(dx), r.1 + f64::from(dy));
    }
    if let Some(m) = l.mask.as_mut().filter(|m| m.linked) {
        m.surface = translate_surface(&m.surface, dx, dy);
    }
    if let Some(vm) = l.vector_mask.as_mut().filter(|v| v.linked) {
        vm.path = vm.path.transform(&a);
    }
    match &mut l.content {
        LayerContent::Raster(s) => *s = translate_surface(s, dx, dy),
        LayerContent::Text(t) => {
            t.transform = a.mul(&t.transform);
            if let Some(c) = &mut t.cache {
                *c = translate_surface(c, dx, dy);
            }
        }
        LayerContent::Shape(sh) => crate::vector_cmds::shift_shape(doc, sh, f64::from(dx), f64::from(dy)),
        LayerContent::Smart(sm) => crate::smart_cmds::shift_smart(sm, dx, dy),
        LayerContent::Group(g) => {
            if let Some(ab) = &mut g.artboard {
                ab.rect = ab.rect.translate(dx, dy);
            }
            g.children.iter_mut().for_each(|c| shift_shown(doc, c, dx, dy));
        }
        LayerContent::Adjustment(_) | LayerContent::Fill(_) => {}
    }
}

/// The document pixels an edit of the layers `ids` (moved, shown, hidden or restyled; contents
/// otherwise as they were) can have changed from `before` to `after`, or `None` for anywhere.
pub fn layers_damage(before: &Document, after: &Document, ids: &[LayerId]) -> Option<Rect> {
    let mut out = Rect::EMPTY;
    for d in [before, after] {
        let canvas = d.bounds();
        for id in ids {
            // A layer gone from one side (never for a move) changes who knows what.
            let (siblings, k, outer) = siblings_of(d, *id)?;
            let b = photocraft_compose::change_bounds_in(siblings, k, outer, canvas)?;
            if !b.is_empty() {
                out = if out.is_empty() { b } else { out.union(&b) };
            }
        }
    }
    Some(out)
}

/// The sibling list holding layer `id` in `doc` (bottom→top), its index there, and the list
/// holding its parent group with the group's index (`None` at the top level).
type Siblings<'d> = (&'d [Layer], usize, Option<(&'d [Layer], usize)>);
fn siblings_of(doc: &Document, id: LayerId) -> Option<Siblings<'_>> {
    let path = doc.path_of(id)?;
    let (&k, parent) = path.split_last()?;
    let Some((&g, grand)) = parent.split_last() else { return Some((doc.layers.as_slice(), k, None)) };
    let outer = if grand.is_empty() { doc.layers.as_slice() } else { doc.layer_at(grand)?.children()? };
    Some((outer.get(g)?.children()?, k, Some((outer, g))))
}

/// Report [`layers_damage`] as the active document's last change, so the canvas recomposites
/// only that (grown by the effect reach around it) instead of everything.
pub(crate) fn note_damage(s: &mut Session, before: &Document, ids: &[LayerId]) {
    let Some(st) = s.active_mut() else { return };
    if before.size != st.doc.size {
        return;
    }
    st.last_damage = layers_damage(before, &st.doc, ids);
}

/// Whether two surfaces hold identical pixels: same format and default pixel, and the same
/// copy-on-write tile pointers at the same coordinates. Both documents stay alive during the
/// comparison, so tile addresses cannot be reused under it.
fn surfaces_equal(a: &photocraft_raster::Surface, b: &photocraft_raster::Surface) -> bool {
    a.format() == b.format()
        && a.default_pixel() == b.default_pixel()
        && a.tile_count() == b.tile_count()
        && a.tiles().zip(b.tiles()).all(|((ca, ta), (cb, tb))| ca == cb && std::sync::Arc::ptr_eq(ta, tb))
}

/// Byte blobs kept in `Arc`s (PSD raw blocks, embedded smart-object sources): pointer-identical
/// or content-equal (they are small compared to pixels, and undo steps rarely re-parse them).
fn blob_equal(x: &std::sync::Arc<Vec<u8>>, y: &std::sync::Arc<Vec<u8>>) -> bool {
    std::sync::Arc::ptr_eq(x, y) || (x.len() == y.len() && x.as_slice() == y.as_slice())
}

fn blobs_equal(a: &Option<std::sync::Arc<Vec<u8>>>, b: &Option<std::sync::Arc<Vec<u8>>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => blob_equal(x, y),
        _ => false,
    }
}

/// Layer masks (also the smart-filter mask): scalars plus tile-pointer surface identity.
fn masks_equal(a: &Option<photocraft_doc::LayerMask>, b: &Option<photocraft_doc::LayerMask>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.enabled == y.enabled && x.linked == y.linked && x.density == y.density && x.feather == y.feather && surfaces_equal(&x.surface, &y.surface)
        }
        _ => false,
    }
}

fn texts_equal(a: &photocraft_doc::TextLayer, b: &photocraft_doc::TextLayer) -> bool {
    a.text == b.text
        && a.font_family == b.font_family
        && a.size_pt == b.size_pt
        && a.color == b.color
        && a.transform == b.transform
        && a.runs == b.runs
        && a.paragraphs == b.paragraphs
        && a.shape == b.shape
        && a.orientation == b.orientation
        && a.antialias == b.antialias
        && a.warp == b.warp
        && match (&a.cache, &b.cache) {
            (None, None) => true,
            (Some(x), Some(y)) => surfaces_equal(x, y),
            _ => false,
        }
        && blobs_equal(&a.psd_raw, &b.psd_raw)
}

fn smarts_equal(a: &photocraft_doc::SmartObject, b: &photocraft_doc::SmartObject) -> bool {
    a.filters_enabled == b.filters_enabled
        && a.transform == b.transform
        && a.smart_filters == b.smart_filters
        && a.stack_mode == b.stack_mode
        && a.perspective == b.perspective
        && a.warp == b.warp
        && match (&a.source, &b.source) {
            (photocraft_doc::SmartSource::Embedded { file_name: x, bytes: xb }, photocraft_doc::SmartSource::Embedded { file_name: y, bytes: yb }) => {
                x == y && blob_equal(xb, yb)
            }
            (photocraft_doc::SmartSource::Linked { path: x }, photocraft_doc::SmartSource::Linked { path: y }) => x == y,
            _ => false,
        }
        && match (&a.cache, &b.cache) {
            (None, None) => true,
            (Some(x), Some(y)) => surfaces_equal(x, y),
            _ => false,
        }
        && blobs_equal(&a.psd_raw, &b.psd_raw)
        && masks_equal(&a.filter_mask, &b.filter_mask)
}

/// Extra alpha/spot channels (the image itself in Multichannel mode).
fn alpha_channels_equal(a: &photocraft_doc::AlphaChannel, b: &photocraft_doc::AlphaChannel) -> bool {
    a.name == b.name && a.spot == b.spot && a.color == b.color && a.opacity == b.opacity && a.indicates == b.indicates && surfaces_equal(&a.surface, &b.surface)
}

/// Whether two layers render to identical pixels in place. Everything the compositor reads is
/// compared structurally — no hashing, a hash collision would under-report damage; pixels are
/// identified by their copy-on-write tile pointers. Group children are deliberately *not*
/// compared here: the caller walks into them, so an edit inside a group damages only the
/// edited child's extent, not the whole group's. The destructure is exhaustive, so a new
/// `Layer` field fails to compile until it is classified: compared, or explicitly ignored
/// (name, locks, label, `psd_blocks`, `psd_id`, link group, XMP — none of them composite; video
/// bails out earlier in the walk).
fn layer_pixels_equal(a: &Layer, b: &Layer) -> bool {
    let Layer {
        id,
        name: _,
        visible,
        locks: _,
        blend,
        opacity,
        fill_opacity,
        clipped,
        mask,
        vector_mask,
        effects,
        label: _,
        content,
        psd_blocks: _,
        psd_id: _,
        fill_cache,
        link_group: _,
        excluded_channels,
        blend_if,
        advanced,
        video: _,
        xmp: _,
    } = a;
    *id == b.id
        && *visible == b.visible
        && *blend == b.blend
        && *opacity == b.opacity
        && *fill_opacity == b.fill_opacity
        && *clipped == b.clipped
        && masks_equal(mask, &b.mask)
        && *vector_mask == b.vector_mask
        && *effects == b.effects
        && *excluded_channels == b.excluded_channels
        && *blend_if == b.blend_if
        && *advanced == b.advanced
        && match (content, &b.content) {
            (LayerContent::Raster(x), LayerContent::Raster(y)) => surfaces_equal(x, y),
            // Children are walked separately; only the group's own clipping frame counts.
            (LayerContent::Group(x), LayerContent::Group(y)) => x.artboard == y.artboard,
            (LayerContent::Fill(x), LayerContent::Fill(y)) => x == y,
            (LayerContent::Adjustment(x), LayerContent::Adjustment(y)) => x == y,
            (LayerContent::Text(x), LayerContent::Text(y)) => texts_equal(x, y),
            (LayerContent::Shape(x), LayerContent::Shape(y)) => x == y,
            (LayerContent::Smart(x), LayerContent::Smart(y)) => smarts_equal(x, y),
            _ => false,
        }
        && match (fill_cache, &b.fill_cache) {
            (None, None) => true,
            (Some(x), Some(y)) => x.fill == y.fill && surfaces_equal(&x.surface, &y.surface),
            _ => false,
        }
}

/// Document-level fields the compositor reads: all must match for a bounded damage claim. The
/// destructure is exhaustive, so a new `Document` field fails to compile until it is classified
/// here — compared when it composites, or explicitly ignored (the pixel-irrelevant allowlist:
/// identity, name, selection, guides, paths, layer comps, annotations, automation state).
fn doc_pixels_equal(a: &Document, b: &Document) -> bool {
    let Document {
        id: _,
        name: _,
        size,
        resolution_dpi,
        mode,
        depth,
        icc_profile,
        layers: _,
        channels,
        guides: _,
        ruler_origin: _,
        selection: _,
        metadata: _,
        global_light,
        paths: _,
        work_path: _,
        clipping_path: _,
        quick_mask,
        quick_mask_unselected: _,
        patterns,
        color_table,
        duotone,
        layer_comps: _,
        last_applied_comp: _,
        last_document_state: _,
        measurement: _,
        notes: _,
        text_styles: _,
        slices: _,
        variables: _,
        timeline: _,
        file_encoding: _,
    } = a;
    *size == b.size
        && resolution_dpi.to_bits() == b.resolution_dpi.to_bits()
        && *mode == b.mode
        && *depth == b.depth
        && icc_profile.as_deref() == b.icc_profile.as_deref()
        && channels.len() == b.channels.len()
        && channels.iter().zip(&b.channels).all(|(x, y)| alpha_channels_equal(x, y))
        && *global_light == b.global_light
        && match (quick_mask, &b.quick_mask) {
            (None, None) => true,
            (Some(x), Some(y)) => alpha_channels_equal(x, y),
            _ => false,
        }
        && *patterns == b.patterns
        && *color_table == b.color_table
        && *duotone == b.duotone
}

/// Whether `a` and `b` composite to the same pixels: they differ at most in fields the
/// compositor never reads (selection, guides, paths, names, …), by the same classification as
/// [`step_damage`] but without computing any bounds. Cheap: pixels compare by tile pointer.
/// Conservative (`false`) past a nesting depth no real document reaches.
pub fn same_pixels(a: &Document, b: &Document) -> bool {
    pixels_equal_with(a, b, |_, _| false)
}

/// [`same_pixels`], with video layers equal when `video_equal` says their frame stacks are.
fn pixels_equal_with(a: &Document, b: &Document, video_equal: fn(&VideoData, &VideoData) -> bool) -> bool {
    fn walk(xs: &[Layer], ys: &[Layer], depth: usize, video_equal: fn(&VideoData, &VideoData) -> bool) -> bool {
        depth < 256
            && xs.len() == ys.len()
            && xs.iter().zip(ys).all(|(x, y)| {
                layer_pixels_equal(x, y)
                    && match (&x.video, &y.video) {
                        (None, None) => true,
                        (Some(vx), Some(vy)) => video_equal(vx, vy),
                        _ => false,
                    }
                    && match (x.children(), y.children()) {
                        (Some(xc), Some(yc)) => walk(xc, yc, depth + 1, video_equal),
                        (None, None) => true,
                        _ => false,
                    }
            })
    }
    doc_pixels_equal(a, b) && walk(&a.layers, &b.layers, 0, video_equal)
}

/// Whether two video layers hold the same frames: the same settings and stack shape
/// (`VideoData`'s own, shallow equality) and the same tile pointers in every frame, painted and
/// original. Pointer compares only, like [`surfaces_equal`].
fn video_frames_equal(a: &VideoData, b: &VideoData) -> bool {
    let frames = |x: &[photocraft_raster::Surface], y: &[photocraft_raster::Surface]| x.len() == y.len() && x.iter().zip(y).all(|(f, g)| surfaces_equal(f, g));
    *a == *b
        && frames(&a.frames, &b.frames)
        && match (&a.original_frames, &b.original_frames) {
            (Some(x), Some(y)) => frames(x, y),
            (None, None) => true,
            _ => false,
        }
}

/// Whether `a` and `b` differ at most in their selection (#6865): a selection step, which must
/// not mark a clean document modified. Pixels compare by tile pointer (as in [`same_pixels`],
/// plus every frame of a video layer, whose own equality is shallow); every other field compares
/// structurally, where copy-on-write `Arc`s of `Eq` data short-circuit on pointer identity. The
/// destructure is exhaustive, so a new `Document` field fails to compile until it is classified.
pub fn selection_only_change(a: &Document, b: &Document) -> bool {
    let Document {
        id,
        name,
        size: _,
        resolution_dpi: _,
        mode: _,
        depth: _,
        icc_profile: _,
        layers,
        channels: _,
        guides,
        ruler_origin,
        selection: _,
        metadata,
        global_light: _,
        paths,
        work_path,
        clipping_path,
        quick_mask: _,
        quick_mask_unselected,
        patterns: _,
        color_table: _,
        duotone: _,
        layer_comps,
        last_applied_comp,
        last_document_state,
        measurement,
        notes,
        text_styles,
        slices,
        variables,
        timeline,
        file_encoding,
    } = a;
    // The ignored fields above are the pixel ones `pixels_equal_with` already compares.
    pixels_equal_with(a, b, video_frames_equal)
        && *id == b.id
        && *name == b.name
        && *layers == b.layers
        && *guides == b.guides
        && *ruler_origin == b.ruler_origin
        && *metadata == b.metadata
        && *paths == b.paths
        && *work_path == b.work_path
        && *clipping_path == b.clipping_path
        && *quick_mask_unselected == b.quick_mask_unselected
        && *layer_comps == b.layer_comps
        && *last_applied_comp == b.last_applied_comp
        && *last_document_state == b.last_document_state
        && *measurement == b.measurement
        && *notes == b.notes
        && *text_styles == b.text_styles
        && *slices == b.slices
        && *variables == b.variables
        && *timeline == b.timeline
        && *file_encoding == b.file_encoding
}

/// Pixels a single history step can have changed between `a` and `b` (the documents on either
/// side of an edit, its undo or its redo): the union of the change bounds of every layer whose
/// pixels differ, so a local edit recomposites only that area instead of the whole canvas. A
/// layer only one side has (a duplicate, a paste, a deleted layer) counts where it draws (see
/// [`one_sided_bounds`]); one that draws nothing ([`inert_at`]) changes nothing. `None`
/// (composite everything) when the step touched anything the layer walk cannot bound:
/// document-level fields the compositor reads (canvas size, mode, depth, profile, global light,
/// patterns, channels, palettes), layer order and kind changes, layers added or removed that
/// reach outside their own bounds, layer properties that do (clipping, excluded channels —
/// what `layer.setProps` already reports as unbounded), and video layers. Inserting a copy of a
/// 12 MP layer used to recomposite every layer of the document (#2883, P8/P9 in
/// `perf/budgets.toml`).
pub fn step_damage(a: &Document, b: &Document) -> Option<Rect> {
    if !doc_pixels_equal(a, b) {
        return None;
    }
    /// The list holding a sibling list's parent group, and the group's index there.
    type Outer<'d> = Option<(&'d [Layer], usize)>;
    fn walk(xs: &[Layer], ys: &[Layer], (ox, oy): (Outer, Outer), a: &Document, b: &Document, out: &mut Rect) -> bool {
        // Pair the layers by id, stepping over layers only one side has: one that draws nothing
        // changes no pixels (a new empty layer or group, #1771), any other only where it draws.
        let one_sided = |layers: &[Layer], k: usize, outer: Outer, canvas: Rect, out: &mut Rect| {
            let Some(r) = one_sided_bounds(layers, k, outer, canvas) else { return false };
            if !r.is_empty() {
                *out = if out.is_empty() { r } else { out.union(&r) };
            }
            true
        };
        let (mut i, mut j) = (0, 0);
        loop {
            let (x, y) = match (xs.get(i), ys.get(j)) {
                (None, None) => return true,
                (Some(x), Some(y)) if x.id == y.id => (x, y),
                _ if inert_at(ys, j) => {
                    j += 1;
                    continue;
                }
                _ if inert_at(xs, i) => {
                    i += 1;
                    continue;
                }
                (_, Some(y)) if xs.iter().all(|x| x.id != y.id) => {
                    if !one_sided(ys, j, oy, b.bounds(), out) {
                        return false;
                    }
                    j += 1;
                    continue;
                }
                (Some(x), _) if ys.iter().all(|y| y.id != x.id) => {
                    if !one_sided(xs, i, ox, a.bounds(), out) {
                        return false;
                    }
                    i += 1;
                    continue;
                }
                // Both sides have the layer, in another order.
                _ => return false,
            };
            let (xk, yk) = (i, j);
            i += 1;
            j += 1;
            if x.video.is_some() || y.video.is_some() {
                return false;
            }
            // Clipping and channel exclusion change how *other* layers render, outside this
            // layer's own bounds; they have no bounded damage.
            if x.clipped != y.clipped || x.excluded_channels != y.excluded_channels {
                return false;
            }
            if !layer_pixels_equal(x, y) {
                // Both sides' extents: a move changes pixels where it left and where it landed.
                // A clipped adjustment reaches as far as its base (#6535).
                let (Some(bx), Some(by)) =
                    (photocraft_compose::change_bounds_in(xs, xk, ox, a.bounds()), photocraft_compose::change_bounds_in(ys, yk, oy, b.bounds()))
                else {
                    return false;
                };
                *out = if out.is_empty() { bx.union(&by) } else { out.union(&bx).union(&by) };
            }
            match (x.children(), y.children()) {
                (Some(xc), Some(yc)) => {
                    if !walk(xc, yc, (Some((xs, xk)), Some((ys, yk))), a, b, out) {
                        return false;
                    }
                }
                (None, None) => {}
                _ => return false,
            }
        }
    }
    let mut out = Rect::EMPTY;
    if walk(&a.layers, &b.layers, (None, None), a, b, &mut out) { Some(out) } else { None }
}

/// The pixels adding or removing `layers[k]` can change, when that is bounded: its change bounds
/// (`photocraft_compose::change_bounds_in`: not for an unclipped adjustment, or a pass-through
/// group holding one; a clipped adjustment's are its base's), provided it isn't the clipping base
/// of clipped layers above it (they would change base), and neither it nor anything inside it is
/// a video layer or excludes channels (both reach outside the layer's bounds, see
/// [`step_damage`]).
fn one_sided_bounds(layers: &[Layer], k: usize, outer: Option<(&[Layer], usize)>, canvas: Rect) -> Option<Rect> {
    fn contained(l: &Layer, depth: usize) -> bool {
        depth < 256 && l.video.is_none() && l.excluded_channels == 0 && l.children().is_none_or(|c| c.iter().all(|c| contained(c, depth + 1)))
    }
    let l = layers.get(k)?;
    let new_base = !l.clipped && layers.get(k + 1).is_some_and(|above| above.clipped);
    if new_base || !contained(l, 0) {
        return None;
    }
    photocraft_compose::change_bounds_in(layers, k, outer, canvas)
}

/// Whether `layers[k]` draws nothing wherever it sits, so adding or removing it leaves the
/// composite as it was: an empty pixel layer (no tiles, transparent default) or an empty group,
/// in a Normal or Pass Through blend with no mask, vector mask, effects or Advanced Blending.
/// A clipping base that would gain or lose the clipped layers above it is not inert.
fn inert_at(layers: &[Layer], k: usize) -> bool {
    let Some(l) = layers.get(k) else { return false };
    let empty = match &l.content {
        LayerContent::Raster(s) => s.tile_count() == 0 && s.format().alpha && s.default_bytes().iter().all(|b| *b == 0),
        LayerContent::Group(g) => g.children.is_empty() && g.artboard.is_none(),
        _ => false,
    };
    empty
        && matches!(l.blend, photocraft_doc::BlendMode::Normal | photocraft_doc::BlendMode::PassThrough)
        && l.mask.is_none()
        && l.vector_mask.is_none()
        && !photocraft_compose::effects::has_effects(l)
        && l.fill_cache.is_none()
        && l.video.is_none()
        && l.advanced == photocraft_doc::AdvancedBlending::default()
        && (l.clipped || layers.get(k + 1).is_none_or(|above| !above.clipped))
}

/// Report that adding layer `id` to the active document changed no pixels when it draws
/// nothing ([`inert_at`]), so the canvas doesn't recomposite every layer for it (#1771). Looks
/// only at the new layer and its neighbour, never at other layers' tiles.
pub(crate) fn note_inert_insert(s: &mut Session, id: LayerId) {
    let Some(st) = s.active_mut() else { return };
    let Some(path) = st.doc.path_of(id) else { return };
    let Some((&k, parent)) = path.split_last() else { return };
    let siblings = if parent.is_empty() { Some(st.doc.layers.as_slice()) } else { st.doc.layer_at(parent).and_then(Layer::children) };
    if siblings.is_some_and(|sib| inert_at(sib, k)) {
        st.last_damage = Some(Rect::EMPTY);
    }
}

/// Report the pixels a command made of more than one edit changed since `before` ([`step_damage`]),
/// so the canvas recomposites only where its new layers draw instead of the whole document.
/// A single [`Session::edit`] already reports its own damage: don't call this after one (#5695).
pub(crate) fn note_insert(s: &mut Session, before: &Document) {
    let Some(st) = s.active_mut() else { return };
    st.last_damage = step_damage(before, &st.doc);
}

/// Content bounds used by Align/Distribute: the layer's pixels (type and shape layers use their
/// rendered pixels), or the union of a group's children. Fill and adjustment layers have none.
pub fn layer_bounds(l: &Layer) -> Option<Rect> {
    let r = match &l.content {
        LayerContent::Group(g) => g.children.iter().filter_map(layer_bounds).fold(Rect::EMPTY, |a, b| a.union(&b)),
        _ => l.surface().map(|s| s.content_bounds()).unwrap_or(Rect::EMPTY),
    };
    (!r.is_empty()).then_some(r)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Edge {
    Top,
    VCenter,
    Bottom,
    Left,
    HCenter,
    Right,
}

impl Edge {
    fn parse(k: &str) -> Option<Edge> {
        Some(match k {
            "topEdges" => Edge::Top,
            "verticalCenters" => Edge::VCenter,
            "bottomEdges" => Edge::Bottom,
            "leftEdges" => Edge::Left,
            "horizontalCenters" => Edge::HCenter,
            "rightEdges" => Edge::Right,
            _ => return None,
        })
    }
    fn vertical(self) -> bool {
        matches!(self, Edge::Top | Edge::VCenter | Edge::Bottom)
    }
    /// The edge's coordinate, doubled so centres stay integral.
    fn value2(self, r: &Rect) -> i64 {
        let (a, b) = if self.vertical() { (r.y0, r.y1) } else { (r.x0, r.x1) };
        match self {
            Edge::Top | Edge::Left => 2 * a as i64,
            Edge::Bottom | Edge::Right => 2 * b as i64,
            Edge::VCenter | Edge::HCenter => a as i64 + b as i64,
        }
    }
    fn delta(self, d: i32) -> (i32, i32) {
        if self.vertical() { (0, d) } else { (d, 0) }
    }
}

/// Selected top-level layers that have content bounds, bottom-to-top.
fn bounded_selection(doc: &Document, sel: &[LayerId]) -> Vec<(LayerId, Rect)> {
    top_level(doc, sel).into_iter().filter_map(|id| Some((id, layer_bounds(doc.layer(id)?)?))).collect()
}

fn align(s: &mut Session, p: &Value, kind: &str) -> Result<Value> {
    let cmd = format!("layer.align.{kind}");
    let edge = Edge::parse(kind).ok_or_else(|| bad(&cmd, "unknown edge"))?;
    let sel = selected(s);
    let to = p.get("to").and_then(Value::as_str).unwrap_or("auto").to_string();
    let before = s.active().ok_or(EngineError::NoDocument)?.doc.clone();
    let moved = s.edit("Align", |doc, _| {
        let items = bounded_selection(doc, &sel);
        if items.is_empty() {
            return Err(EngineError::Other("the selected layers have no content to align".into()));
        }
        let target = match to.as_str() {
            "canvas" => doc.bounds(),
            "selection" => doc.selection.as_ref().map(|m| m.content_bounds()).ok_or_else(|| EngineError::Other("there is no selection to align to".into()))?,
            "auto" | "layers" => match (&doc.selection, sel.len()) {
                // Photoshop: with an active selection, the layers align to its bounds (Layer › Align
                // Layers to Selection), however many are selected.
                (Some(m), _) if to == "auto" && !m.content_bounds().is_empty() => m.content_bounds(),
                // One layer with no selection aligns to the canvas (otherwise it would align to
                // itself), or to the artboard it sits in, as in Photoshop (#7945).
                (None, 1) if to == "auto" => items.first().map_or_else(|| doc.bounds(), |(id, _)| board_or_canvas(doc, *id)),
                _ => items.iter().fold(Rect::EMPTY, |a, (_, b)| a.union(b)),
            },
            other => return Err(bad(&cmd, format!("`to` must be auto|layers|selection|canvas, not `{other}`"))),
        };
        if target.is_empty() {
            return Err(EngineError::Other("nothing to align to".into()));
        }
        let t2 = edge.value2(&target);
        let moves: Vec<(LayerId, i32, i32)> = items
            .iter()
            .flat_map(|(id, r)| {
                let d = (t2 - edge.value2(r)).div_euclid(2) as i32;
                let (dx, dy) = edge.delta(d);
                // Linked layers travel with the layer they are linked to.
                with_links(doc, &[*id]).into_iter().map(move |l| (l, dx, dy))
            })
            .collect();
        let moves = dedup_moves(doc, moves);
        move_layers(doc, &before, &moves)?;
        Ok(moves.iter().filter(|m| m.1 != 0 || m.2 != 0).count())
    })?;
    Ok(json!({"moved": moved}))
}

/// The bounds a lone layer aligns to: the artboard holding it, else the canvas. An artboard
/// itself aligns to the canvas.
fn board_or_canvas(doc: &Document, id: LayerId) -> Rect {
    doc.artboard_of(id).filter(|board| *board != id).and_then(|board| doc.layer(board)?.artboard().map(|a| a.rect)).unwrap_or_else(|| doc.bounds())
}

/// One move per layer (first wins), skipping layers nested in another moved layer.
fn dedup_moves(doc: &Document, moves: Vec<(LayerId, i32, i32)>) -> Vec<(LayerId, i32, i32)> {
    let mut out: Vec<(LayerId, i32, i32)> = Vec::new();
    for m in moves {
        if !out.iter().any(|o| o.0 == m.0) {
            out.push(m);
        }
    }
    let ids: Vec<LayerId> = out.iter().map(|m| m.0).collect();
    let keep = top_level(doc, &ids);
    out.retain(|m| keep.contains(&m.0));
    out
}

fn distribute(s: &mut Session, kind: &str) -> Result<Value> {
    let sel = selected(s);
    let kind = kind.to_string();
    let before = s.active().ok_or(EngineError::NoDocument)?.doc.clone();
    let moved = s.edit("Distribute", |doc, _| {
        let mut items = bounded_selection(doc, &sel);
        if items.len() < 3 {
            return Err(EngineError::Other("distributing needs three or more layers with content".into()));
        }
        let n = items.len();
        let mut moves = Vec::new();
        match kind.as_str() {
            "horizontally" | "vertically" => {
                // Equal gaps between neighbours; the outermost layers stay put.
                let v = kind == "vertically";
                let lo = |r: &Rect| if v { r.y0 } else { r.x0 } as i64;
                let hi = |r: &Rect| if v { r.y1 } else { r.x1 } as i64;
                items.sort_by_key(|(_, r)| (lo(r) + hi(r), lo(r)));
                let span = hi(&items[n - 1].1) - lo(&items[0].1);
                let total: i64 = items.iter().map(|(_, r)| hi(r) - lo(r)).sum();
                let gap = (span - total) as f64 / (n - 1) as f64;
                let mut cursor = lo(&items[0].1) as f64;
                for (id, r) in &items {
                    let want = cursor.round() as i64;
                    let d = (want - lo(r)) as i32;
                    moves.push(if v { (*id, 0, d) } else { (*id, d, 0) });
                    cursor += (hi(r) - lo(r)) as f64 + gap;
                }
            }
            k => {
                let edge = Edge::parse(k).ok_or_else(|| bad("layer.distribute", format!("unknown kind `{k}`")))?;
                items.sort_by_key(|(_, r)| edge.value2(r));
                let (first, last) = (edge.value2(&items[0].1) as f64, edge.value2(&items[n - 1].1) as f64);
                for (i, (id, r)) in items.iter().enumerate() {
                    let want2 = first + (last - first) * i as f64 / (n - 1) as f64;
                    let d = ((want2 - edge.value2(r) as f64) / 2.0).round() as i32;
                    let (dx, dy) = edge.delta(d);
                    moves.push((*id, dx, dy));
                }
            }
        }
        let moves: Vec<_> = moves.into_iter().flat_map(|(id, dx, dy)| with_links(doc, &[id]).into_iter().map(move |l| (l, dx, dy))).collect();
        let moves = dedup_moves(doc, moves);
        move_layers(doc, &before, &moves)?;
        Ok(moves.iter().filter(|m| m.1 != 0 || m.2 != 0).count())
    })?;
    Ok(json!({"moved": moved}))
}

/// Reorder one or multiple layers to a single destination in one undoable edit.
///
/// Explicit `layers` are ordered according to the source document, not caller ordering.
/// A transient snapshot for the Layers panel's "Preview on canvas when reordering layers".
/// Uses the same source ordering and insertion semantics as `move_to`, but never edits history.
/// Invalid/self-referential hover targets return None (the committed image stays visible).
pub fn preview_move(doc: &Document, p: &Value, add_copy: bool) -> Option<Document> {
    let target = LayerId(p.get("target")?.as_u64()?);
    let position = p.get("position")?.as_str()?;
    if !matches!(position, "above" | "below" | "into") {
        return None;
    }
    let copy = p.get("copy").and_then(Value::as_bool).unwrap_or(false);
    let ids = if let Some(ids) = p.get("layers") {
        ids.as_array()?.iter().map(|v| v.as_u64().map(LayerId)).collect::<Option<Vec<_>>>()?
    } else {
        vec![LayerId(p.get("layer")?.as_u64()?)]
    };
    if ids.is_empty() || doc.layer(target).is_none() {
        return None;
    }
    let moved_ids = top_level(doc, &ids);
    if moved_ids.is_empty() || moved_ids.iter().any(|id| doc.layer(*id).is_none()) {
        return None;
    }
    if moved_ids.len() == 1 && moved_ids[0] == target && !copy {
        return None;
    }
    let target_path = doc.path_of(target)?;
    for id in &moved_ids {
        let path = doc.path_of(*id)?;
        if target_path.starts_with(&path) && !(copy && *id == target && position != "into") {
            return None;
        }
    }
    if position == "into" && !doc.layer(target)?.is_group() {
        return None;
    }
    // The very structural move the drop commits, so the preview shows the same clip groups,
    // Background handling and group-depth refusals; a drop `move_to` would refuse has no preview.
    let mut preview = doc.clone();
    apply_move(&mut preview, &moved_ids, target, position, copy, add_copy).ok()?;
    Some(preview)
}

/// The document-only part of `move_to`, shared with [`preview_move`] so the Layers panel's hover
/// preview cannot drift from what the drop commits. Moves (or, with `copy`, duplicates) the
/// top-level `moved_ids` to `pos` (`above`, `below` or `into`) `target`, then applies the
/// group-depth cap, the Background-stays-at-the-bottom rule and the clip-group rules
/// (`clip_group::settle`). Returns the ids of the new copies (empty for a move). On `Err` `doc` is
/// half edited: run it inside `Session::edit` or on a scratch copy.
fn apply_move(doc: &mut Document, moved_ids: &[LayerId], target: LayerId, pos: &str, copy: bool, add_copy: bool) -> Result<Vec<LayerId>> {
    const CMD: &str = "layer.moveTo";
    let background = crate::background_cmds::background_id(doc);
    let before = if copy { Default::default() } else { crate::clip_group::Before::read(doc, moved_ids) };
    let mut moved = Vec::with_capacity(moved_ids.len());
    for &id in moved_ids {
        moved.push(if copy { crate::commands::layer_copy(doc, id, add_copy)? } else { doc.remove(id).ok_or(EngineError::NoLayer(id))? });
    }
    let copies: Vec<LayerId> = if copy { moved.iter().map(|l| l.id).collect() } else { Vec::new() };
    // Look up the destination AFTER removing every source: its indices may have shifted.
    let path = doc.path_of(target).ok_or(EngineError::NoLayer(target))?;
    if pos == "into" {
        let children = doc.layer_at_mut(&path).and_then(Layer::children_mut).ok_or_else(|| bad(CMD, "target is not a group"))?;
        children.extend(moved);
    } else {
        let (&idx, parent) = path.split_last().ok_or(EngineError::NoLayer(target))?;
        let siblings = if parent.is_empty() {
            &mut doc.layers
        } else {
            doc.layer_at_mut(parent).and_then(Layer::children_mut).ok_or_else(|| bad(CMD, "target's parent is not a group"))?
        };
        let at = if pos == "below" { idx } else { idx + 1 };
        siblings.splice(at..at, moved);
    }
    check_group_depth(doc, "Reorder Layer")?;
    crate::background_cmds::keep_background_at_bottom(background, doc)?;
    crate::clip_group::settle(doc, if copy { &copies } else { moved_ids }, &before, false);
    Ok(copies)
}

/// Selected descendants of a moved group remain inside their parent; only top-level
/// selected nodes are detached, and every reference is validated before editing.
pub fn move_to(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "layer.moveTo";
    let pos = p.get("position").and_then(Value::as_str).unwrap_or("above");
    if !matches!(pos, "above" | "below" | "into") {
        return Err(bad(CMD, "position must be above, below, or into"));
    }
    // ⌥-drag in the Layers panel: duplicates land at the target and the layers stay put.
    let copy = p.get("copy").and_then(Value::as_bool).unwrap_or(false);

    let batch = p.get("layers");
    if batch.is_some() && p.get("layer").is_some() {
        return Err(bad(CMD, "give either `layer` or `layers`, not both"));
    }
    let requested = if let Some(raw) = batch {
        let arr = raw.as_array().filter(|a| !a.is_empty()).ok_or_else(|| bad(CMD, "`layers` must be a nonempty array of ids"))?;
        let mut ids = Vec::with_capacity(arr.len());
        for item in arr {
            let id = LayerId(item.as_u64().ok_or_else(|| bad(CMD, "every layer id must be an unsigned integer"))?);
            if ids.contains(&id) {
                return Err(bad(CMD, "duplicate layer id"));
            }
            ids.push(id);
        }
        ids
    } else {
        vec![crate::commands::layer_param(s, p)?]
    };
    // A copy moves nothing, so its source stays a candidate (⌥-dragging a layer beside itself).
    let target = move_target(s, p, if copy { &[] } else { &requested })?;
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let doc = &st.doc;
    let target_path = doc.path_of(target).ok_or(EngineError::NoLayer(target))?;
    for &id in &requested {
        if doc.layer(id).is_none() {
            return Err(EngineError::NoLayer(id));
        }
    }
    let moved_ids = top_level(doc, &requested);
    if moved_ids.len() == 1 && moved_ids[0] == target && batch.is_none() && !copy {
        // Preserve single-layer drag-to-self semantics.
        return Ok(Value::Null);
    }
    // Also rejects dragging a selected group onto any of its descendants.
    for id in &moved_ids {
        let path = doc.path_of(*id).ok_or(EngineError::NoLayer(*id))?;
        // A copy may land beside its own source (not inside it).
        let beside_itself = copy && *id == target && pos != "into";
        if target_path.starts_with(&path) && !beside_itself {
            return Err(bad(CMD, "target cannot be one of the moved layers or its descendant"));
        }
    }
    if pos == "into" && doc.layer(target).is_none_or(|l| !l.is_group()) {
        return Err(bad(CMD, "target is not a group"));
    }

    let original_active = st.active_layer;
    let add_copy = crate::commands::layer_panel_option(s, "addCopyToCopiedLayersAndGroups", true);
    let (count, copies) = s.edit(if copy { "Duplicate Layer" } else { "Reorder Layers" }, |doc, active| {
        let copies = apply_move(doc, &moved_ids, target, pos, copy, add_copy)?;
        *active = if copy {
            copies.last().copied()
        } else if batch.is_some() {
            original_active.filter(|id| requested.contains(id)).or_else(|| requested.last().copied())
        } else {
            moved_ids.first().copied()
        };
        Ok((moved_ids.len(), copies))
    })?;
    if copy {
        let ids: Vec<u64> = copies.iter().map(|id| id.0).collect();
        if batch.is_some() {
            let active = copies.last().copied();
            reselect(s, copies, active);
            return Ok(json!({"moved": count, "layers": ids}));
        }
        return Ok(json!({"layer": ids.first()}));
    }
    if batch.is_some() {
        let active = original_active.filter(|id| requested.contains(id)).or_else(|| requested.last().copied());
        reselect(s, requested, active);
        Ok(json!({"moved": count}))
    } else {
        Ok(Value::Null)
    }
}

// ---------- structural commands ----------

fn link_layers(s: &mut Session) -> Result<Value> {
    let sel = selected(s);
    let linked = s.edit("Link Layers", |doc, _| {
        let groups: Vec<Option<u64>> = sel.iter().map(|id| doc.layer(*id).and_then(|l| l.link_group)).collect();
        // Already one link group (or a single linked layer): unlink, like Photoshop's toggle.
        let first = groups.first().copied().flatten();
        let unlink = first.is_some() && groups.iter().all(|g| *g == first);
        // A loaded file can store any id up to u64::MAX, so the new id must not be a bare
        // max + 1 (#961). Unlinking needs no new id at all.
        let next = if unlink { None } else { Some(doc.unused_link_group().ok_or_else(|| bad("layer.linkLayers", "no free link group id is left"))?) };
        for id in &sel {
            let l = doc.layer_mut(*id).ok_or(EngineError::NoLayer(*id))?;
            l.link_group = next;
        }
        // A group left with a single member is no longer a link.
        let mut counts = std::collections::HashMap::<u64, usize>::new();
        for (_, _, l) in doc.walk() {
            if let Some(g) = l.link_group {
                *counts.entry(g).or_default() += 1;
            }
        }
        let lonely: Vec<LayerId> =
            doc.walk().iter().filter(|(_, _, l)| l.link_group.is_some_and(|g| counts.get(&g).is_some_and(|&c| c < 2))).map(|(_, _, l)| l.id).collect();
        for id in lonely {
            if let Some(l) = doc.layer_mut(id) {
                l.link_group = None;
            }
        }
        Ok(!unlink)
    })?;
    Ok(json!({"linked": linked}))
}

fn lock_layers(s: &mut Session, p: &Value) -> Result<Value> {
    let sel = selected(s);
    let keys = ["transparency", "pixels", "position", "artboard", "all"];
    let given: Vec<(&str, bool)> = keys.iter().filter_map(|k| Some((*k, p.get(*k)?.as_bool()?))).collect();
    s.edit("Lock Layers", |doc, _| {
        // No explicit locks: toggle "lock all" (on unless every selected layer is already locked).
        let all_locked = sel.iter().all(|id| doc.layer(*id).is_some_and(|l| l.locks.all));
        let given: Vec<(&str, bool)> = if given.is_empty() { vec![("all", !all_locked)] } else { given.clone() };
        for id in &sel {
            let l = doc.layer_mut(*id).ok_or(EngineError::NoLayer(*id))?;
            for (k, v) in &given {
                l.locks.set(k, *v);
            }
        }
        Ok(())
    })?;
    Ok(json!({"layers": sel.len()}))
}

fn rename_layer(s: &mut Session, p: &Value) -> Result<Value> {
    let name = p.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty()).ok_or_else(|| bad("layer.renameLayer", "missing `name`"))?.to_string();
    let id = crate::commands::layer_param(s, p)?;
    s.edit("Rename Layer", |doc, _| {
        doc.layer_mut(id).ok_or(EngineError::NoLayer(id))?.name = name;
        Ok(())
    })?;
    Ok(Value::Null)
}

fn reverse(s: &mut Session) -> Result<Value> {
    let sel = selected(s);
    s.edit("Reverse", |doc, _| {
        let background = crate::background_cmds::background_id(doc);
        let ids = top_level(doc, &sel);
        if ids.len() < 2 {
            return Err(EngineError::Other("select two or more layers to reverse".into()));
        }
        let paths: Vec<Vec<usize>> = ids.iter().map(|id| doc.path_of(*id).ok_or(EngineError::NoLayer(*id))).collect::<Result<_>>()?;
        let layers: Vec<Layer> = ids.iter().map(|id| doc.layer(*id).cloned().ok_or(EngineError::NoLayer(*id))).collect::<Result<_>>()?;
        // Slots are disjoint (no layer is nested in another), so writing whole layers back to the
        // same paths never shifts another slot.
        for (path, l) in paths.iter().zip(layers.into_iter().rev()) {
            *doc.layer_at_mut(path).ok_or_else(|| EngineError::Other("layer tree changed".into()))? = l;
        }
        crate::background_cmds::keep_background_at_bottom(background, doc)
    })?;
    Ok(Value::Null)
}

/// Refuses an edit that left `doc` nested deeper than [`photocraft_doc::MAX_GROUP_DEPTH`]
/// groups. Called at the end of a `Session::edit` closure, so an `Err` leaves the document and
/// history untouched.
pub(crate) fn check_group_depth(doc: &Document, what: &str) -> Result<()> {
    if doc.max_group_depth() > photocraft_doc::MAX_GROUP_DEPTH {
        return Err(EngineError::Other(format!("{what} would nest layers deeper than {} groups", photocraft_doc::MAX_GROUP_DEPTH)));
    }
    Ok(())
}

/// Group Layers (⌘G) / Group from Layers: the explicit layer, or every selected layer, moves
/// into a new, collapsed group placed where the top-most of them was. Bottom-to-top order is
/// preserved.
pub fn group_layers(s: &mut Session, p: &Value) -> Result<Value> {
    let ids = match crate::commands::explicit_layer(s, p)? {
        Some(id) => vec![id],
        None => selected(s),
    };
    if ids.is_empty() {
        return Err(EngineError::Other("no layer selected".into()));
    }
    let name = p.get("name").and_then(Value::as_str).map(str::to_string);
    let gid = s.edit("Group Layers", |doc, active| {
        let ids = top_level(doc, &ids);
        let top = *ids.last().ok_or(EngineError::NoLayer(LayerId(0)))?;
        let name = name.unwrap_or_else(|| doc.next_layer_name("Group"));
        // Photoshop creates the group collapsed, so the panel shows one row in place of the layers.
        let mut group = Layer::group(name, vec![]);
        if let LayerContent::Group(g) = &mut group.content {
            g.expanded = false;
        }
        let gid = doc.insert_above(Some(top), group);
        let mut children = Vec::with_capacity(ids.len());
        for id in &ids {
            children.push(doc.remove(*id).ok_or(EngineError::NoLayer(*id))?);
        }
        *doc.layer_mut(gid).and_then(Layer::children_mut).ok_or(EngineError::NoLayer(gid))? = children;
        check_group_depth(doc, "Group Layers")?;
        *active = Some(gid);
        Ok(gid)
    })?;
    reselect(s, vec![gid], Some(gid));
    Ok(json!({ "layer": gid.0 }))
}

fn merge_layers(s: &mut Session) -> Result<Value> {
    let sel = selected(s);
    if sel.len() < 2 {
        // ⌘E on a lone group is Photoshop's Merge Group (#3832); on a layer, Merge Down.
        if let Some(&id) = sel.first()
            && s.active().and_then(|d| d.doc.layer(id)).is_some_and(|l| matches!(l.content, LayerContent::Group(_)))
        {
            return s.execute("layer.mergeGroup", json!({ "layer": id.0 }));
        }
        return s.execute("layer.mergeDown", json!({}));
    }
    let mid = s.edit("Merge Layers", |doc, active| {
        let ids = top_level(doc, &sel);
        let lowest_id = *ids.first().ok_or_else(|| EngineError::Other("Merge Layers needs two or more layers".into()))?;
        let is_background = doc.is_background(lowest_id);

        let mut solo = doc.clone();
        solo.layers = ids.iter().filter_map(|id| doc.layer(*id)).filter(|l| l.visible).cloned().collect();
        if solo.layers.is_empty() {
            return Err(EngineError::Other("the selected layers are all hidden".into()));
        }

        if is_background {
            let base = doc.layer(lowest_id).ok_or(EngineError::NoLayer(lowest_id))?.clone();
            let fmt = doc.pixel_format();
            let mut merged = Layer::raster(base.name.clone(), fmt);
            merged.locks = base.locks;
            merged.label = base.label;
            merged.id = lowest_id;
            merged.blend = base.blend;
            merged.opacity = 1.0;
            *crate::pixels_mut(&mut merged)? = crate::pixels::composite_layers(&solo, fmt, None);
            for id in &ids[1..] {
                doc.remove(*id);
            }
            *doc.layer_mut(lowest_id).ok_or(EngineError::NoLayer(lowest_id))? = merged;
            *active = Some(lowest_id);
            Ok(lowest_id)
        } else {
            let top_id = *ids.last().ok_or_else(|| EngineError::Other("Merge Layers needs two or more layers".into()))?;
            let top = doc.layer(top_id).ok_or(EngineError::NoLayer(top_id))?.clone();
            let fmt = doc.pixel_format();
            let fmt = PixelFormat::new(fmt.mode, fmt.sample, true);
            let mut merged = Layer::raster(top.name.clone(), fmt);
            merged.locks = top.locks;
            merged.label = top.label;
            // Clipped layers of one chain merge into a layer still clipped to their base. Clipped
            // layers of different chains don't: the result, at the top one's slot, would clip the
            // lower chain's pixels to the upper chain's base.
            let base = crate::clip_group::base_of(doc, top_id);
            merged.clipped = base.is_some() && solo.layers.iter().all(|l| l.clipped && crate::clip_group::base_of(doc, l.id) == base);
            *crate::pixels_mut(&mut merged)? = crate::pixels::composite_layers_uncropped(&solo, fmt);
            let mid = merged.id;
            // Hidden selected layers are discarded, as in Photoshop.
            for id in &ids[..ids.len() - 1] {
                doc.remove(*id);
            }
            *doc.layer_mut(top_id).ok_or(EngineError::NoLayer(top_id))? = merged;
            *active = Some(mid);
            Ok(mid)
        }
    })?;
    reselect(s, vec![mid], Some(mid));
    Ok(json!({ "layer": mid.0 }))
}

/// Next visible layer row below the deleted target, then rows above in reverse order.
/// Group headers precede their children in the UI, unlike Document::walk's storage order.
pub(crate) fn deletion_neighbours(doc: &Document, target: LayerId) -> Vec<LayerId> {
    let rows = |expand_all: bool| {
        let mut stack: Vec<&Layer> = doc.layers.iter().collect();
        let mut order = Vec::new();
        while let Some(layer) = stack.pop() {
            order.push(layer.id);
            if let LayerContent::Group(group) = &layer.content
                && (expand_all || group.expanded)
            {
                stack.extend(group.children.iter());
            }
        }
        order
    };
    let mut order = rows(false);
    if !order.contains(&target) {
        order = rows(true);
    }
    let Some(index) = order.iter().position(|id| *id == target) else {
        return Vec::new();
    };
    order.iter().skip(index + 1).chain(order.iter().take(index).rev()).copied().collect()
}

/// Delete Layer with several layers selected.
pub fn delete_selected(s: &mut Session) -> Result<Value> {
    let sel = selected(s);
    s.edit("Delete Layers", |doc, active| {
        let ids = top_level(doc, &sel);
        let neighbours = active.map(|id| deletion_neighbours(doc, id)).unwrap_or_default();
        let before = crate::clip_group::Before::read(doc, &ids);
        for id in &ids {
            doc.remove(*id).ok_or(EngineError::NoLayer(*id))?;
        }
        crate::clip_group::settle(doc, &[], &before, false);
        if doc.layers.is_empty() {
            return Err(EngineError::Other("a document must keep at least one layer".into()));
        }
        *active = neighbours.into_iter().find(|candidate| doc.layer(*candidate).is_some());
        Ok(())
    })?;
    Ok(json!({"deleted": sel.len()}))
}

/// Duplicate Layer with several layers selected: each copy goes above its original and the
/// copies become the selection. Artboard copies go beside their boards unless `in_place`
/// ([`crate::artboard_cmds::place_copy`]). `label` names the history step (Layer via Copy reuses
/// this path, #2777).
pub fn duplicate_selected(s: &mut Session, in_place: bool, label: &str) -> Result<Value> {
    let sel = selected(s);
    let old_active = s.active().and_then(|d| d.active_layer);
    let add_copy = crate::commands::layer_panel_option(s, "addCopyToCopiedLayersAndGroups", true);
    let (copies, active) = s.edit(label, |doc, active| {
        let mut copies = Vec::new();
        let mut new_active = None;
        for id in top_level(doc, &sel) {
            // Match the single-layer duplicate path: Background copies become
            // ordinary layers, while normal copies retain their original locks.
            let dup = crate::commands::layer_copy(doc, id, add_copy)?;
            let nid = doc.insert_above(Some(id), dup);
            if !in_place {
                crate::artboard_cmds::place_copy(doc, nid)?;
            }
            if Some(id) == old_active {
                new_active = Some(nid);
            }
            copies.push(nid);
        }
        let a = new_active.or(copies.last().copied());
        *active = a;
        Ok((copies, a))
    })?;
    let out = copies.iter().map(|l| l.0).collect::<Vec<_>>();
    reselect(s, copies, active);
    Ok(json!({"layers": out, "layer": active.map(|l| l.0)}))
}

/// Show/hide every selected layer in one step.
pub fn set_visible_selected(s: &mut Session, visible: bool) -> Result<Value> {
    let sel = selected(s);
    s.edit(if visible { "Show Layers" } else { "Hide Layers" }, |doc, _| {
        for id in &sel {
            doc.layer_mut(*id).ok_or(EngineError::NoLayer(*id))?.visible = visible;
        }
        Ok(())
    })?;
    Ok(Value::Null)
}

/// Is more than one layer selected (so single-layer commands should act on the whole set)?
pub fn multi(s: &Session, p: &Value) -> bool {
    p.get("layer").is_none() && selected(s).len() > 1
}

// ---------- registry ----------

pub fn specs() -> Vec<CommandSpec> {
    macro_rules! spec {
        ($id:expr, $label:expr, $menu:expr, $sc:expr, $params:expr, $enabled:expr, $run:expr) => {
            CommandSpec { id: $id, label: $label, menu: $menu, shortcut: $sc, params: $params, enabled: $enabled, journal: true, run: $run }
        };
    }
    const ALIGN: &str = r##"{"to":"auto|layers|selection|canvas"="auto"} (auto: the active selection's bounds when there is one, however many layers are selected; else one layer aligns to the canvas and several to their combined bounds)"##;
    vec![
        spec!("select.allLayers", "All Layers", &["Select"], Some("Cmd+Alt+A"), "{}", has_doc, |s, _| select_all_layers(s)),
        spec!("select.deselectLayers", "Deselect Layers", &["Select"], None, "{}", has_doc, |s, _| {
            set_selection(s, Vec::new(), None, None)?;
            if let Some(st) = s.active_mut() {
                st.active_layer = None;
                st.selected_layers.clear();
            }
            Ok(Value::Null)
        }),
        spec!("select.findLayers", "Find Layers", &["Select"], Some("Cmd+Alt+Shift+F"), r##"{"name":str (case-insensitive substring)}"##, has_doc, find_layers),
        spec!("layer.selectLinkedLayers", "Select Linked Layers", &["Layer"], None, "{}", has_linked, |s, _| select_linked(s)),
        spec!("layer.align.topEdges", "Top Edges", &["Layer", "Align"], None, ALIGN, can_align, |s, p| align(s, p, "topEdges")),
        spec!("layer.align.verticalCenters", "Vertical Centers", &["Layer", "Align"], None, ALIGN, can_align, |s, p| align(s, p, "verticalCenters")),
        spec!("layer.align.bottomEdges", "Bottom Edges", &["Layer", "Align"], None, ALIGN, can_align, |s, p| align(s, p, "bottomEdges")),
        spec!("layer.align.leftEdges", "Left Edges", &["Layer", "Align"], None, ALIGN, can_align, |s, p| align(s, p, "leftEdges")),
        spec!("layer.align.horizontalCenters", "Horizontal Centers", &["Layer", "Align"], None, ALIGN, can_align, |s, p| align(s, p, "horizontalCenters")),
        spec!("layer.align.rightEdges", "Right Edges", &["Layer", "Align"], None, ALIGN, can_align, |s, p| align(s, p, "rightEdges")),
        spec!("layer.distribute.topEdges", "Top Edges", &["Layer", "Distribute"], None, "{}", can_distribute, |s, _| distribute(s, "topEdges")),
        spec!("layer.distribute.verticalCenters", "Vertical Centers", &["Layer", "Distribute"], None, "{}", can_distribute, |s, _| distribute(
            s,
            "verticalCenters"
        )),
        spec!("layer.distribute.bottomEdges", "Bottom Edges", &["Layer", "Distribute"], None, "{}", can_distribute, |s, _| distribute(s, "bottomEdges")),
        spec!("layer.distribute.leftEdges", "Left Edges", &["Layer", "Distribute"], None, "{}", can_distribute, |s, _| distribute(s, "leftEdges")),
        spec!("layer.distribute.horizontalCenters", "Horizontal Centers", &["Layer", "Distribute"], None, "{}", can_distribute, |s, _| distribute(
            s,
            "horizontalCenters"
        )),
        spec!("layer.distribute.rightEdges", "Right Edges", &["Layer", "Distribute"], None, "{}", can_distribute, |s, _| distribute(s, "rightEdges")),
        spec!(
            "layer.distribute.horizontally",
            "Horizontally",
            &["Layer", "Distribute"],
            None,
            "{} (equal horizontal gaps)",
            can_distribute,
            |s, _| distribute(s, "horizontally")
        ),
        spec!("layer.distribute.vertically", "Vertically", &["Layer", "Distribute"], None, "{} (equal vertical gaps)", can_distribute, |s, _| distribute(
            s,
            "vertically"
        )),
        spec!("layer.linkLayers", "Link Layers", &["Layer"], None, "{} (toggles: unlinks when the selection is already one link group)", can_link, |s, _| {
            link_layers(s)
        }),
        spec!("layer.mergeLayers", "Merge Layers", &["Layer"], Some("Cmd+E"), "{} (one layer selected: Merge Down)", has_layer, |s, _| merge_layers(s)),
        spec!(
            "layer.new.groupFromLayers",
            "Group from Layers…",
            &["Layer", "New"],
            None,
            r##"{"layer":id?,"name":str?} (no layer: every selected layer)"##,
            has_layer,
            group_layers
        ),
        spec!("layer.arrange.reverse", "Reverse", &["Layer", "Arrange"], None, "{}", two_plus, |s, _| reverse(s)),
        spec!(
            "layer.lockLayers",
            "Lock Layers…",
            &["Layer"],
            None,
            r##"{"transparency":bool?,"pixels":bool?,"position":bool?,"artboard":bool?,"all":bool?} (none given: toggle lock all)"##,
            has_layer,
            lock_layers
        ),
        spec!("layer.renameLayer", "Rename Layer", &["Layer"], None, r##"{"layer":id?,"name":str}"##, has_layer, rename_layer),
    ]
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod layers_panel_parity_tests;
