//! The mouse wheel on value fields (#5473, #4061): one step a notch, as the Up/Down keys (×10
//! with Shift).
//!
//! - While a field has keyboard focus, the wheel steps it wherever the pointer is. [`begin_frame`]
//!   takes the wheel out of the frame's input before anything is laid out, so neither the canvas
//!   nor a panel scrolls or zooms with it. ⌥/Alt and ⌘/Ctrl + wheel stay the view's.
//! - Over a field without focus, the wheel steps it too, but only a gesture that starts over it:
//!   a panel being scrolled keeps scrolling when a field passes under the pointer.

use egui::{Context, Event, Id, MouseWheelUnit, Rect, Ui, Vec2};

/// Wheel events this close together (seconds) are one gesture.
const GESTURE_GAP: f64 = 0.3;

/// Who a wheel gesture belongs to.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Owner {
    /// It started this frame and no field has taken it yet.
    New,
    Field(Id),
    /// It started where no field took it (a panel, the canvas).
    Elsewhere,
}

#[derive(Clone, Copy, Debug)]
struct Gesture {
    last: f64,
    owner: Owner,
}

fn gesture_id() -> Id {
    Id::new("pc-field-wheel-gesture")
}

fn focused_id() -> Id {
    Id::new("pc-field-wheel-focused")
}

fn taken_id() -> Id {
    Id::new("pc-field-wheel-taken")
}

/// A wheel event the focused field takes: plain or Shift, not the view's Alt or Ctrl/Cmd zoom.
fn for_field(e: &Event) -> bool {
    matches!(e, Event::MouseWheel { modifiers, .. } if !modifiers.alt && !modifiers.ctrl && !modifiers.command && !modifiers.mac_cmd)
}

/// Before this frame's UI is laid out: hand the wheel to the focused value field, if any, and
/// follow which gesture the wheel is in.
pub fn begin_frame(ctx: &Context) {
    ctx.data_mut(|d| d.remove::<Vec<Event>>(taken_id()));
    // (the field's widget id, its slot id): focus is the widget's, a gesture the slot's.
    let focused = ctx.data(|d| d.get_temp::<(Id, Id)>(focused_id())).filter(|(id, _)| ctx.memory(|m| m.has_focus(*id)));
    let (now, wheel) = ctx.input(|i| (i.time, i.events.iter().any(|e| matches!(e, Event::MouseWheel { .. }))));
    if let Some((_, field)) = focused {
        let taken: Vec<Event> = ctx.input_mut(|i| {
            let (taken, rest) = std::mem::take(&mut i.events).into_iter().partition(for_field);
            i.events = rest;
            // egui spreads each notch over a few frames: none of it scrolls anything meanwhile.
            if !i.events.iter().any(|e| matches!(e, Event::MouseWheel { .. })) {
                i.smooth_scroll_delta = Vec2::ZERO;
            }
            taken
        });
        if !taken.is_empty() {
            ctx.data_mut(|d| {
                d.insert_temp(taken_id(), taken);
                d.insert_temp(gesture_id(), Gesture { last: now, owner: Owner::Field(field) });
            });
        }
        return;
    }
    if !wheel {
        return;
    }
    let gesture = ctx.data(|d| d.get_temp::<Gesture>(gesture_id()));
    let owner = match gesture {
        Some(g) if now - g.last <= GESTURE_GAP => match g.owner {
            // No field took it when it started.
            Owner::New => Owner::Elsewhere,
            owner => owner,
        },
        _ => Owner::New,
    };
    ctx.data_mut(|d| d.insert_temp(gesture_id(), Gesture { last: now, owner }));
}

/// Record that value field `id` (whose slot is `field`, as given to [`notches`]) has keyboard
/// focus this frame, so the wheel is its next frame.
pub fn set_focused(ctx: &Context, id: Id, field: Id) {
    ctx.data_mut(|d| d.insert_temp(focused_id(), (id, field)));
}

/// Wheel notches for the value field `field` in `rect` this frame (up positive, Shift ×10):
/// all of the wheel while it is `focused`, else a gesture that started with the pointer over it.
pub fn notches(ui: &Ui, field: Id, focused: bool, rect: Rect) -> f32 {
    let events: Vec<Event> = if focused {
        ui.data_mut(|d| d.remove_temp::<Vec<Event>>(taken_id())).unwrap_or_default()
    } else {
        if !ui.is_enabled() || !ui.rect_contains_pointer(rect) {
            return 0.0;
        }
        let Some(mut g) = ui.data(|d| d.get_temp::<Gesture>(gesture_id())) else { return 0.0 };
        let events: Vec<Event> = ui.input(|i| i.events.iter().filter(|e| for_field(e)).cloned().collect());
        match g.owner {
            Owner::New if !events.is_empty() => {
                g.owner = Owner::Field(field);
                ui.data_mut(|d| d.insert_temp(gesture_id(), g));
            }
            Owner::Field(id) if id == field => {}
            _ => return 0.0,
        }
        // Nothing under the field scrolls with the wheel that steps it, smoothed tail included.
        ui.ctx().input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
        events
    };
    let wheel: Vec<(MouseWheelUnit, Vec2, bool)> = events
        .iter()
        .filter_map(|e| match e {
            Event::MouseWheel { unit, delta, modifiers, .. } if for_field(e) => Some((*unit, *delta, modifiers.shift)),
            _ => None,
        })
        .collect();
    crate::widgets::count_notches(ui, field.with("wheel-acc"), &wheel, 10.0)
}

#[cfg(test)]
mod tests {
    use egui::accesskit::Role;
    use egui::{Modifiers, Pos2, vec2};
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    /// The value, the scroll area's offset, and whether the field is enabled.
    type State = (f32, f32, bool);

    /// A value field (0–1000) at the top of a scroll area 100 pt tall, above a tall filler.
    fn harness(value: f32) -> Harness<'static, State> {
        let mut h = Harness::builder().with_size(vec2(300.0, 200.0)).build_ui_state(
            |ui, s: &mut State| {
                super::begin_frame(ui.ctx());
                let out = egui::ScrollArea::vertical().max_height(100.0).show(ui, |ui| {
                    ui.add_enabled_ui(s.2, |ui| crate::widgets::value_field(ui, &mut s.0, 0.0..=1000.0, "px", 90.0));
                    ui.allocate_space(vec2(200.0, 1000.0));
                });
                s.1 = out.state.offset.y;
            },
            (value, 0.0, true),
        );
        h.run();
        h
    }

    fn field(h: &Harness<'static, State>) -> egui::Rect {
        h.get_by_role(Role::SpinButton).rect()
    }

    /// `lines` wheel lines (up positive) at `at`, then let the gesture end.
    fn wheel(h: &mut Harness<'static, State>, at: Pos2, lines: f32, modifiers: Modifiers) {
        h.hover_at(at);
        h.run_steps(1);
        h.event(egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta: vec2(0.0, lines), phase: egui::TouchPhase::Move, modifiers });
        h.run_steps(4);
    }

    #[test]
    fn the_wheel_over_a_field_steps_it_and_scrolls_nothing() {
        let mut h = harness(100.0);
        let at = field(&h).center();
        wheel(&mut h, at, 1.0, Modifiers::NONE);
        assert_eq!(h.state().0, 101.0);
        wheel(&mut h, at, -3.0, Modifiers::NONE);
        assert_eq!(h.state().0, 98.0);
        wheel(&mut h, at, 1.0, Modifiers::SHIFT);
        assert_eq!(h.state().0, 108.0, "Shift: ten a notch");
        assert_eq!(h.state().1, 0.0, "the panel under the field didn't scroll");
    }

    #[test]
    fn a_focused_field_takes_the_wheel_wherever_the_pointer_is() {
        let mut h = harness(100.0);
        h.get_by_role(Role::SpinButton).click();
        h.run();
        let away = field(&h).center() + vec2(0.0, 60.0);
        wheel(&mut h, away, -2.0, Modifiers::NONE);
        assert_eq!(h.state().0, 98.0);
        assert_eq!(h.state().1, 0.0, "the panel under the pointer didn't scroll");
        assert!(h.get_by_role(Role::SpinButton).is_focused(), "the field keeps focus");
        // Alt + wheel stays the view's (zoom), not the field's.
        wheel(&mut h, away, 1.0, Modifiers::ALT);
        assert_eq!(h.state().0, 98.0);
    }

    #[test]
    fn a_gesture_that_starts_over_the_panel_keeps_scrolling_it_over_a_field() {
        let mut h = harness(100.0);
        let below = field(&h).center() + vec2(0.0, 40.0);
        h.hover_at(below);
        h.run_steps(1);
        let down =
            || egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta: vec2(0.0, -1.0), phase: egui::TouchPhase::Move, modifiers: Modifiers::NONE };
        h.event(down());
        h.run_steps(1);
        // Still the same gesture as the pointer reaches a field.
        h.hover_at(field(&h).center());
        h.event(down());
        h.run_steps(1);
        assert_eq!(h.state().0, 100.0, "the field passing under the pointer is not stepped");
        assert!(h.state().1 > 0.0, "the panel scrolled");
    }

    #[test]
    fn a_disabled_field_ignores_the_wheel() {
        let mut h = harness(100.0);
        h.state_mut().2 = false;
        h.run();
        let at = field(&h).center();
        wheel(&mut h, at, 1.0, Modifiers::NONE);
        assert_eq!(h.state().0, 100.0);
    }

    #[test]
    fn wild_wheel_deltas_leave_a_finite_value_in_range() {
        // egui itself asserts on a non-finite wheel event in debug builds: `count_notches` is checked
        // with those below.
        for lines in [1e30, -1e30, 1e6, -1e6] {
            for focus in [false, true] {
                let mut h = harness(100.0);
                if focus {
                    h.get_by_role(Role::SpinButton).click();
                    h.run();
                }
                let at = field(&h).center();
                wheel(&mut h, at, lines, Modifiers::SHIFT);
                let v = h.state().0;
                assert!(v.is_finite() && (0.0..=1000.0).contains(&v), "{lines} (focus {focus}): {v}");
            }
        }
    }

    #[test]
    fn notches_of_nan_or_infinite_deltas_are_none() {
        use egui::MouseWheelUnit::{Line, Point};
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(Default::default(), |ui| {
            let id = egui::Id::new("acc");
            for d in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                for unit in [Line, Point] {
                    assert_eq!(crate::widgets::count_notches(ui, id, &[(unit, vec2(0.0, d), false)], 10.0), 0.0, "{d} {unit:?}");
                    // Nothing poisoned is carried over to the next notch.
                    assert_eq!(crate::widgets::count_notches(ui, id, &[(Line, vec2(0.0, 1.0), true)], 10.0), 10.0);
                }
            }
        });
        out.textures_delta.clear();
    }
}
