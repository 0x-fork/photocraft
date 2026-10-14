//! Blend mode keys, as in Photoshop. ⇧+ and ⇧− step through the modes (#3102), and ⇧⌥ plus a
//! letter picks one (⇧⌥M Multiply, ⇧⌥S Screen…). With a painting tool that has a mode of its own
//! they set that tool's mode and never the layer's: the Brush's or Pencil's (which also offer
//! Clear), the Gradient's; the other painting tools are left alone until their options bar has a
//! Mode. With any other tool (Move, the selection tools…) they set every selected layer's mode.
//! The step list wraps at either end. Each press is one call of an engine command per layer
//! (`tools.setBrush`, `layer.setProps`), coalesced into one undo step.

use std::sync::atomic::{AtomicU64, Ordering};

use egui::{Event, Key};
use photocraft_color::BlendMode;
use serde_json::json;

use crate::PhotocraftApp;
use crate::state::Tool;

/// The mode `step` places after (or before, when negative) `current` in `modes`, wrapping around.
/// A mode that isn't listed steps from the top of the list.
pub fn stepped<T: Copy + PartialEq>(modes: &[T], current: T, step: i32) -> Option<T> {
    let n = i32::try_from(modes.len()).ok().filter(|n| *n > 0)?;
    let at = modes.iter().position(|m| *m == current).and_then(|i| i32::try_from(i).ok());
    let next = match at {
        Some(i) => (i + step).rem_euclid(n),
        None if step > 0 => 0,
        None => n - 1,
    };
    modes.get(usize::try_from(next).ok()?).copied()
}

/// What a blend mode key asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pick {
    Mode(BlendMode),
    /// Painting tools only: paint transparency.
    Clear,
    /// Painting tools only: paint behind the layer's pixels. Not implemented yet, so ⇧⌥Q is taken
    /// (it must not reach another binding) and does nothing.
    Behind,
}

/// Photoshop's ⇧⌥ letter for each mode. Darker Color, Lighter Color, Subtract and Divide have none.
pub fn letter_pick(key: Key) -> Option<Pick> {
    use BlendMode as B;
    Some(Pick::Mode(match key {
        Key::N => B::Normal,
        Key::I => B::Dissolve,
        Key::Q => return Some(Pick::Behind),
        Key::R => return Some(Pick::Clear),
        Key::P => B::PassThrough,
        Key::K => B::Darken,
        Key::M => B::Multiply,
        Key::B => B::ColorBurn,
        Key::A => B::LinearBurn,
        Key::G => B::Lighten,
        Key::S => B::Screen,
        Key::D => B::ColorDodge,
        Key::W => B::LinearDodge,
        Key::O => B::Overlay,
        Key::F => B::SoftLight,
        Key::H => B::HardLight,
        Key::V => B::VividLight,
        Key::J => B::LinearLight,
        Key::Z => B::PinLight,
        Key::L => B::HardMix,
        Key::E => B::Difference,
        Key::X => B::Exclusion,
        Key::U => B::Hue,
        Key::T => B::Saturation,
        Key::C => B::Color,
        Key::Y => B::Luminosity,
        _ => return None,
    }))
}

/// One key's request: a step through the list, or a mode picked by letter.
enum Press {
    Step(i32),
    Pick(Pick),
}

/// Consumes this frame's ⇧+ / ⇧− presses (⇧= on a US layout is `+`; on a German one
/// `layout_keys::apply` has already turned ⇧+ and ⇧- into `+` and `-`) and ⇧⌥ letters, and
/// applies them. Runs before the tool letters, which would otherwise take ⇧⌥M as M.
pub fn handle(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let presses: Vec<Press> = ctx.input_mut(|i| {
        let mut out = Vec::new();
        i.events.retain(|e| match e {
            Event::Key { key, physical_key, pressed: true, modifiers, .. } if modifiers.shift && !modifiers.command && !modifiers.ctrl => {
                if modifiers.alt {
                    // The layout's letter (⇧⌥Z on a German keyboard is Pin Light), else the
                    // physical one: on a Mac ⌥ turns the letter into another character (⇧⌥M
                    // types Â) that may arrive as some other key.
                    match letter_pick(*key).or_else(|| physical_key.and_then(letter_pick)) {
                        Some(p) => out.push(Press::Pick(p)),
                        None => return true,
                    }
                } else {
                    match key {
                        Key::Plus | Key::Equals => out.push(Press::Step(1)),
                        Key::Minus => out.push(Press::Step(-1)),
                        _ => return true,
                    }
                }
                false
            }
            _ => true,
        });
        out
    });
    for p in presses {
        match p {
            Press::Step(step) => press(app, step),
            Press::Pick(pick) => pick_mode(app, pick),
        }
    }
}

/// Where the active tool's blend mode keys go.
enum Target {
    /// The Brush's or Pencil's mode, Clear included.
    Paint,
    Gradient,
    /// A painting tool whose mode has no options-bar control yet (or the Eraser, which has none):
    /// the keys do nothing, and never reach the layer.
    Nothing,
    Layers,
}

fn target(tool: Tool) -> Target {
    match tool {
        Tool::Brush | Tool::Pencil => Target::Paint,
        Tool::Gradient => Target::Gradient,
        t if t.is_brushlike() || t == Tool::MagicEraser || t == Tool::PaintBucket => Target::Nothing,
        _ => Target::Layers,
    }
}

/// The Brush's or Pencil's mode list: the layer modes with Clear (`None`) after Dissolve, as in
/// its options-bar dropdown.
fn paint_modes() -> Vec<Option<BlendMode>> {
    let mut modes: Vec<Option<BlendMode>> = BlendMode::LAYER_MODES.iter().copied().map(Some).collect();
    let at = modes.iter().position(|m| *m == Some(BlendMode::Dissolve)).map_or(modes.len(), |i| i + 1);
    modes.insert(at, None);
    modes
}

/// The Brush's or Pencil's Clear flag.
fn clear_flag(app: &mut PhotocraftApp) -> &mut bool {
    if app.ui.tool == Tool::Pencil { &mut app.ui.tool_options.pencil_clear } else { &mut app.ui.tool_options.brush_clear }
}

/// Sets the Brush's or Pencil's mode; `None` is Clear.
fn set_paint_mode(app: &mut PhotocraftApp, mode: Option<BlendMode>) {
    *clear_flag(app) = mode.is_none();
    if let Some(m) = mode
        && let Ok(m) = serde_json::to_value(m)
    {
        let _ = app.run("tools.setBrush", json!({ "brush": { "mode": m } }));
    }
}

fn set_gradient_mode(app: &mut PhotocraftApp, mode: BlendMode) {
    let before = app.ui.tool_options.clone();
    app.ui.tool_options.gradient_blend_mode = mode;
    // Live mode: a selected gradient fill layer follows, as with the dropdown.
    crate::gradient_ui::options_changed(app, &before);
}

/// Sets `mode` on every selected layer that can take it, in one undo step: the locked Background
/// has no blend mode, and only groups pass through.
fn set_layers_mode(app: &mut PhotocraftApp, mode: BlendMode) {
    static PRESSES: AtomicU64 = AtomicU64::new(0);
    let Some(st) = app.session.active() else { return };
    let ids: Vec<_> = st
        .selected_layers()
        .into_iter()
        .filter_map(|id| st.doc.layer(id))
        .filter(|l| !crate::doc_props_ui::is_background(&st.doc, l) && (mode != BlendMode::PassThrough || l.is_group()) && l.blend != mode)
        .map(|l| l.id)
        .collect();
    let coalesce = format!("blend-keys-{}", PRESSES.fetch_add(1, Ordering::Relaxed));
    for id in ids {
        let _ = app.run("layer.setProps", json!({ "layer": id.0, "blend": mode.label(), "coalesce": coalesce }));
    }
}

/// One ⇧+ (`step` 1) or ⇧− (`step` -1) press. Selected layers all take the mode after the active
/// layer's.
pub fn press(app: &mut PhotocraftApp, step: i32) {
    match target(app.ui.tool) {
        Target::Paint => {
            let current = if *clear_flag(app) { None } else { Some(app.session.tools.brush.mode) };
            if let Some(mode) = stepped(&paint_modes(), current, step) {
                set_paint_mode(app, mode);
            }
        }
        Target::Gradient => {
            if let Some(mode) = stepped(&BlendMode::LAYER_MODES, app.ui.tool_options.gradient_blend_mode, step) {
                set_gradient_mode(app, mode);
            }
        }
        Target::Nothing => {}
        Target::Layers => {
            let Some(st) = app.session.active() else { return };
            let Some(layer) = st.active_layer.and_then(|id| st.doc.layer(id)) else { return };
            // The locked Background has no blend mode.
            if crate::doc_props_ui::is_background(&st.doc, layer) {
                return;
            }
            let modes: Vec<BlendMode> = std::iter::once(BlendMode::PassThrough).filter(|_| layer.is_group()).chain(BlendMode::LAYER_MODES).collect();
            if let Some(mode) = stepped(&modes, layer.blend, step) {
                set_layers_mode(app, mode);
            }
        }
    }
}

/// One ⇧⌥ letter press. A mode the target doesn't have (Clear or Behind for layers, Pass Through
/// for a painting tool) does nothing.
pub fn pick_mode(app: &mut PhotocraftApp, pick: Pick) {
    match (target(app.ui.tool), pick) {
        (Target::Paint, Pick::Mode(m)) if m != BlendMode::PassThrough => set_paint_mode(app, Some(m)),
        (Target::Paint, Pick::Clear) => set_paint_mode(app, None),
        (Target::Gradient, Pick::Mode(m)) if m != BlendMode::PassThrough => set_gradient_mode(app, m),
        (Target::Layers, Pick::Mode(m)) => set_layers_mode(app, m),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(tool: Tool) -> PhotocraftApp {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        app.run("file.new", json!({"width": 32, "height": 32})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        app.ui.tool = tool;
        app
    }

    fn layer_blend(app: &PhotocraftApp) -> BlendMode {
        let st = app.session.active().unwrap();
        st.doc.layer(st.active_layer.unwrap()).unwrap().blend
    }

    /// Feeds one key press with `modifiers` through the app's shortcut handling.
    fn key(app: &mut PhotocraftApp, ctx: &egui::Context, key: Key, modifiers: egui::Modifiers) {
        let raw = egui::RawInput {
            events: vec![egui::Event::ModifiersChanged(modifiers), egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }],
            ..Default::default()
        };
        ctx.begin_pass(raw);
        crate::shortcuts::handle(app, ctx);
        ctx.end_pass().textures_delta.clear();
    }

    /// #3102: on a German layout ⇧+ types `*`, which egui reports as the physical `]`, and ⇧- types
    /// `_` as the physical `/`. With the Brush they step its mode, not its hardness; a US ⇧] (`}`)
    /// still raises the hardness.
    #[test]
    fn german_shift_plus_and_minus_step_the_mode_and_us_shift_bracket_keeps_the_hardness() {
        let ctx = egui::Context::default();
        let mut a = app(Tool::Brush);
        a.session.tools.brush.hardness = 0.5;
        let shift = egui::Modifiers::SHIFT;
        let typed = |a: &mut PhotocraftApp, key: Key, physical_key: Key, text: &str| {
            let press = egui::Event::Key { key, physical_key: Some(physical_key), pressed: true, repeat: false, modifiers: shift };
            let raw = egui::RawInput { events: vec![egui::Event::ModifiersChanged(shift), press, egui::Event::Text(text.into())], ..Default::default() };
            ctx.begin_pass(raw);
            crate::shortcuts::handle(a, &ctx);
            ctx.end_pass().textures_delta.clear();
        };
        typed(&mut a, Key::CloseBracket, Key::CloseBracket, "*");
        assert_eq!(a.session.tools.brush.mode, BlendMode::Dissolve);
        assert_eq!(a.session.tools.brush.hardness, 0.5, "not Increase Brush Hardness");
        typed(&mut a, Key::Slash, Key::Slash, "_");
        assert_eq!(a.session.tools.brush.mode, BlendMode::Normal);
        // US: ⇧] arrives as `}` from the physical `]`.
        typed(&mut a, Key::CloseCurlyBracket, Key::CloseBracket, "}");
        assert!(a.session.tools.brush.hardness > 0.5);
        assert_eq!(a.session.tools.brush.mode, BlendMode::Normal);
    }

    #[test]
    fn stepping_wraps_at_both_ends() {
        let m = BlendMode::LAYER_MODES;
        assert_eq!(stepped(&m, BlendMode::Normal, 1), Some(BlendMode::Dissolve));
        assert_eq!(stepped(&m, BlendMode::Normal, -1), m.last().copied());
        assert_eq!(stepped(&m, m[m.len() - 1], 1), Some(BlendMode::Normal));
        assert_eq!(stepped(&[], BlendMode::Normal, 1), None);
    }

    /// #3102: with the Move tool ⇧+ / ⇧− step the active layer's mode, one undo step each; with
    /// the Brush they step the brush's mode and leave the layer alone.
    #[test]
    fn shift_plus_and_minus_step_the_layer_or_the_brush_mode() {
        let ctx = egui::Context::default();
        let mut a = app(Tool::Move);
        let undo = a.session.active().unwrap().history.past_len();
        key(&mut a, &ctx, Key::Plus, egui::Modifiers::SHIFT);
        assert_eq!(layer_blend(&a), BlendMode::Dissolve);
        key(&mut a, &ctx, Key::Plus, egui::Modifiers::SHIFT);
        assert_eq!(layer_blend(&a), BlendMode::Darken);
        key(&mut a, &ctx, Key::Minus, egui::Modifiers::SHIFT);
        assert_eq!(layer_blend(&a), BlendMode::Dissolve);
        assert_eq!(a.session.active().unwrap().history.past_len(), undo + 3, "one undo step per press");
        // Unshifted or ⌘ presses are not blend-mode keys.
        key(&mut a, &ctx, Key::Minus, egui::Modifiers::NONE);
        key(&mut a, &ctx, Key::Plus, egui::Modifiers::COMMAND | egui::Modifiers::SHIFT);
        assert_eq!(layer_blend(&a), BlendMode::Dissolve);
        a.ui.tool = Tool::Brush;
        key(&mut a, &ctx, Key::Minus, egui::Modifiers::SHIFT);
        assert_eq!(a.session.tools.brush.mode, BlendMode::LAYER_MODES[BlendMode::LAYER_MODES.len() - 1]);
        assert_eq!(layer_blend(&a), BlendMode::Dissolve, "the layer is untouched");
        // The Background keeps Normal; no document is a no-op.
        a.ui.tool = Tool::Move;
        let bg = a.session.active().unwrap().doc.layers[0].id;
        a.run("layer.select", json!({"layer": bg.0})).unwrap();
        key(&mut a, &ctx, Key::Plus, egui::Modifiers::SHIFT);
        assert_eq!(a.session.active().unwrap().doc.layers[0].blend, BlendMode::Normal);
        let mut none = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        press(&mut none, 1);
    }

    /// Feeds one ⇧⌥ press of `key`, reported by the platform as `physical` too.
    fn alt_shift(app: &mut PhotocraftApp, ctx: &egui::Context, key: Key, physical: Key) {
        let m = egui::Modifiers::SHIFT | egui::Modifiers::ALT;
        let raw = egui::RawInput {
            events: vec![egui::Event::ModifiersChanged(m), egui::Event::Key { key, physical_key: Some(physical), pressed: true, repeat: false, modifiers: m }],
            ..Default::default()
        };
        ctx.begin_pass(raw);
        crate::shortcuts::handle(app, ctx);
        ctx.end_pass().textures_delta.clear();
    }

    fn blend_of(app: &PhotocraftApp, i: usize) -> BlendMode {
        app.session.active().unwrap().doc.layers[i].blend
    }

    /// Photoshop's letters: 26 keys, each a different mode, and the four modes without one.
    #[test]
    fn every_letter_picks_its_own_mode() {
        let keys: Vec<Key> = "NIQRPKMBAGSDWOFHVJZLEXUTCY".chars().filter_map(|c| Key::from_name(&c.to_string())).collect();
        assert_eq!(keys.len(), 26);
        let picks: Vec<Pick> = keys.iter().filter_map(|k| letter_pick(*k)).collect();
        assert_eq!(picks.len(), keys.len());
        for (i, p) in picks.iter().enumerate() {
            assert!(!picks[..i].contains(p), "{p:?} has two letters");
        }
        assert_eq!(letter_pick(Key::M), Some(Pick::Mode(BlendMode::Multiply)));
        assert_eq!(letter_pick(Key::W), Some(Pick::Mode(BlendMode::LinearDodge)));
        assert_eq!(letter_pick(Key::Y), Some(Pick::Mode(BlendMode::Luminosity)));
        for m in [BlendMode::DarkerColor, BlendMode::LighterColor, BlendMode::Subtract, BlendMode::Divide] {
            assert!(!picks.contains(&Pick::Mode(m)), "{m:?} has no letter in Photoshop");
        }
        for k in [Key::Num1, Key::Space] {
            assert_eq!(letter_pick(k), None);
        }
    }

    /// With the Move tool ⇧⌥M makes the active layer Multiply in one undo step, and doesn't switch
    /// to the Marquee tool's M. Clear and Behind are painting modes: a layer ignores them.
    #[test]
    fn alt_shift_letter_sets_the_layer_mode_and_keeps_the_tool() {
        let ctx = egui::Context::default();
        let mut a = app(Tool::Move);
        let undo = a.session.active().unwrap().history.past_len();
        alt_shift(&mut a, &ctx, Key::M, Key::M);
        assert_eq!(layer_blend(&a), BlendMode::Multiply);
        assert_eq!(a.ui.tool, Tool::Move, "not the Marquee tool");
        assert_eq!(a.session.active().unwrap().history.past_len(), undo + 1);
        alt_shift(&mut a, &ctx, Key::R, Key::R);
        alt_shift(&mut a, &ctx, Key::Q, Key::Q);
        alt_shift(&mut a, &ctx, Key::P, Key::P);
        assert_eq!(layer_blend(&a), BlendMode::Multiply, "no Clear, Behind or Pass Through on a layer");
        assert_eq!(a.ui.tool, Tool::Move);
        // ⇧⌥ letters that are tool keys pick modes too.
        alt_shift(&mut a, &ctx, Key::S, Key::S);
        assert_eq!(layer_blend(&a), BlendMode::Screen);
        assert_eq!(a.ui.tool, Tool::Move);
    }

    /// A Mac's ⌥ turns the letter into another character; egui then reports the physical key.
    /// A German layout's Z (physical Y) is still Pin Light: the typed letter wins.
    #[test]
    fn the_typed_letter_wins_and_the_physical_key_is_the_fallback() {
        let ctx = egui::Context::default();
        let mut a = app(Tool::Move);
        alt_shift(&mut a, &ctx, Key::Backtick, Key::O);
        assert_eq!(layer_blend(&a), BlendMode::Overlay);
        alt_shift(&mut a, &ctx, Key::Z, Key::Y);
        assert_eq!(layer_blend(&a), BlendMode::PinLight);
    }

    /// Every selected layer takes the mode, all in one undo step; the Background keeps Normal.
    /// ⇧+ steps them all to the mode after the active layer's.
    #[test]
    fn mode_keys_set_every_selected_layer_in_one_undo_step() {
        let ctx = egui::Context::default();
        let mut a = app(Tool::Move);
        let ids: Vec<u64> = a.session.active().unwrap().doc.layers.iter().map(|l| l.id.0).collect();
        a.run("layer.new.layer", json!({})).unwrap();
        for id in &ids {
            a.run("layer.select", json!({"layer": id, "mode": "add"})).unwrap();
        }
        assert_eq!(a.session.active().unwrap().selected_layers().len(), 3);
        let undo = a.session.active().unwrap().history.past_len();
        alt_shift(&mut a, &ctx, Key::S, Key::S);
        assert_eq!([blend_of(&a, 0), blend_of(&a, 1), blend_of(&a, 2)], [BlendMode::Normal, BlendMode::Screen, BlendMode::Screen]);
        assert_eq!(a.session.active().unwrap().history.past_len(), undo + 1, "one undo step for both layers");
        key(&mut a, &ctx, Key::Plus, egui::Modifiers::SHIFT);
        assert_eq!([blend_of(&a, 1), blend_of(&a, 2)], [BlendMode::ColorDodge, BlendMode::ColorDodge]);
        assert_eq!(a.session.active().unwrap().history.past_len(), undo + 2);
        a.session.undo();
        assert_eq!([blend_of(&a, 1), blend_of(&a, 2)], [BlendMode::Screen, BlendMode::Screen]);
    }

    /// ⇧⌥P is Pass Through, for groups only.
    #[test]
    fn pass_through_is_for_groups() {
        let ctx = egui::Context::default();
        let mut a = app(Tool::Move);
        a.run("layer.new.group", json!({})).unwrap();
        alt_shift(&mut a, &ctx, Key::N, Key::N);
        assert_eq!(layer_blend(&a), BlendMode::Normal);
        alt_shift(&mut a, &ctx, Key::P, Key::P);
        assert_eq!(layer_blend(&a), BlendMode::PassThrough);
    }

    /// With the Brush the letters set the brush's mode, Clear included, and never the layer's;
    /// ⇧+ from Dissolve reaches Clear, as in the dropdown. The Pencil keeps its own Clear.
    #[test]
    fn with_a_painting_tool_the_letters_set_only_its_mode() {
        let ctx = egui::Context::default();
        let mut a = app(Tool::Brush);
        alt_shift(&mut a, &ctx, Key::M, Key::M);
        assert_eq!(a.session.tools.brush.mode, BlendMode::Multiply);
        assert_eq!(layer_blend(&a), BlendMode::Normal, "the layer is untouched");
        assert_eq!(a.ui.tool, Tool::Brush);
        alt_shift(&mut a, &ctx, Key::R, Key::R);
        assert!(a.ui.tool_options.brush_clear);
        alt_shift(&mut a, &ctx, Key::P, Key::P);
        assert!(a.ui.tool_options.brush_clear, "no Pass Through for a brush");
        alt_shift(&mut a, &ctx, Key::I, Key::I);
        assert!(!a.ui.tool_options.brush_clear);
        assert_eq!(a.session.tools.brush.mode, BlendMode::Dissolve);
        key(&mut a, &ctx, Key::Plus, egui::Modifiers::SHIFT);
        assert!(a.ui.tool_options.brush_clear, "Clear follows Dissolve");
        key(&mut a, &ctx, Key::Plus, egui::Modifiers::SHIFT);
        assert!(!a.ui.tool_options.brush_clear);
        assert_eq!(a.session.tools.brush.mode, BlendMode::Darken);
        assert!(!a.ui.tool_options.pencil_clear);
        a.ui.tool = Tool::Pencil;
        alt_shift(&mut a, &ctx, Key::R, Key::R);
        assert!(a.ui.tool_options.pencil_clear);
        assert_eq!(layer_blend(&a), BlendMode::Normal);
    }

    /// The Gradient takes the letters; a painting tool without a Mode control yet (Clone Stamp)
    /// ignores them rather than changing the layer.
    #[test]
    fn gradient_takes_the_letters_and_clone_stamp_ignores_them() {
        let ctx = egui::Context::default();
        let mut a = app(Tool::Gradient);
        alt_shift(&mut a, &ctx, Key::O, Key::O);
        assert_eq!(a.ui.tool_options.gradient_blend_mode, BlendMode::Overlay);
        alt_shift(&mut a, &ctx, Key::R, Key::R);
        assert_eq!(a.ui.tool_options.gradient_blend_mode, BlendMode::Overlay);
        a.ui.tool = Tool::CloneStamp;
        alt_shift(&mut a, &ctx, Key::M, Key::M);
        assert_eq!(layer_blend(&a), BlendMode::Normal);
        assert_eq!(a.ui.tool, Tool::CloneStamp);
        let mut none = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        pick_mode(&mut none, Pick::Mode(BlendMode::Multiply));
    }
}
