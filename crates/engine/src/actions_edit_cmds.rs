//! Editing an action's steps and its set, as the Actions panel does beyond recording:
//! `actions.insert` (Photoshop's Insert Stop… and Insert Menu Item…), `actions.dialog` (the
//! panel's dialog toggle column) and `actions.assignSet` (which set folder an action sits in).
//!
//! All three refuse to run during playback, commit pending recorded steps first, and keep the
//! per-step flag sets (`disabled`, `dialogs`) in step with the moved indices, like
//! `actions.move` and `actions.delete`.

use serde_json::{Value, json};

use crate::actions_cmds::{flush_recording, not_playing, pending_steps, resolve};
use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

fn always(_: &Session) -> std::result::Result<(), String> {
    Ok(())
}

fn index(p: &Value, key: &str, cmd: &str) -> Result<Option<usize>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().and_then(|n| usize::try_from(n).ok()).map(Some).ok_or_else(|| bad(cmd, format!("`{key}` must be a non-negative integer"))),
    }
}

/// Whether `id` can be a step: a registered command, a Play Action call, or one of the desktop
/// shell's recorded save/view commands.
fn is_step_command(id: &str) -> bool {
    crate::commands::find(id).is_some() || id == "actions.play" || crate::actions_cmds::shell_save_command(id) || crate::actions_cmds::shell_view_command(id)
}

fn insert(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "actions.insert";
    not_playing(s, cmd)?;
    let idx = resolve(&s.actions.list, p, cmd)?;
    let id = match p.get("id") {
        Some(Value::String(id)) if !id.trim().is_empty() => id.trim().to_string(),
        _ => return Err(bad(cmd, "needs `id`, the command the step runs")),
    };
    if !is_step_command(&id) {
        return Err(bad(cmd, format!("unknown command `{id}`")));
    }
    let params = match p.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => return Err(bad(cmd, "`params` must be an object")),
    };
    if id == "actions.play" {
        // Refuse the one cycle visible here; deeper ones are caught at playback.
        let target = params.get("action");
        let is_self = target.and_then(Value::as_str) == Some(s.actions.list[idx].name.as_str()) || target.and_then(Value::as_u64) == Some(idx as u64);
        if is_self {
            return Err(bad(cmd, "an action cannot play itself"));
        }
    }
    let len = s.actions.list[idx].steps.len().saturating_add(pending_steps(s, idx).count());
    let at = index(p, "at", cmd)?.unwrap_or(len);
    if at > len {
        return Err(bad(cmd, format!("`at` {at} is past the last step ({len})")));
    }
    if s.actions.recording.is_some_and(|(_, i)| i == idx) {
        flush_recording(s)?;
    }
    let action = &mut s.actions.list[idx];
    action.steps.insert(at, (id.clone(), params));
    action.remap_steps(|i| Some(if i >= at { i + 1 } else { i }));
    let result = json!({"action": action.name, "step": at, "id": id, "steps": action.steps.len()});
    s.actions.touch();
    Ok(result)
}

/// The panel's dialog toggle column. With `step`, switch one step's dialog on or off; without
/// it, every step (all on unless all already are, like clicking the action's own toggle).
fn dialog(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "actions.dialog";
    not_playing(s, cmd)?;
    let idx = resolve(&s.actions.list, p, cmd)?;
    let enabled = match p.get("enabled") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(bad(cmd, "`enabled` must be true or false")),
    };
    let step = index(p, "step", cmd)?;
    let len = s.actions.list[idx].steps.len().saturating_add(pending_steps(s, idx).count());
    if step.is_some_and(|step| step >= len) {
        return Err(bad(cmd, "step is out of range"));
    }
    if s.actions.recording.is_some_and(|(_, i)| i == idx) {
        flush_recording(s)?;
    }
    let action = &mut s.actions.list[idx];
    match step {
        Some(step) => {
            if enabled.unwrap_or(!action.dialogs.contains(&step)) {
                action.dialogs.insert(step);
            } else {
                action.dialogs.remove(&step);
            }
        }
        None if enabled.unwrap_or(action.dialogs.len() < action.steps.len()) => action.dialogs = (0..action.steps.len()).collect(),
        None => action.dialogs.clear(),
    }
    let result = json!({"action": action.name, "dialogs": action.dialogs});
    s.actions.touch();
    Ok(result)
}

fn assign_set(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "actions.assignSet";
    not_playing(s, cmd)?;
    let idx = resolve(&s.actions.list, p, cmd)?;
    let set = match p.get("set") {
        None | Some(Value::Null) => None,
        Some(Value::String(n)) if !n.trim().is_empty() => Some(n.trim().to_string()),
        Some(Value::String(_)) => return Err(bad(cmd, "`set` is empty; pass null to take the action out of its set")),
        Some(_) => return Err(bad(cmd, "`set` must be a name or null")),
    };
    let action = &mut s.actions.list[idx];
    action.set = set;
    let result = json!({"action": action.name, "set": action.set});
    s.actions.touch();
    Ok(result)
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "actions.insert",
            label: "Insert Step",
            menu: &[],
            shortcut: None,
            params: r##"{"action":name|index, "id":command id, "params":{}?, "at":index?=end} → {action, step, id, steps}. Inserts a step at the 0-based position (Photoshop's Insert Stop… with id actions.stopMessage, Insert Menu Item… with any command). Pending recorded steps count; flags of the steps after it follow them."##,
            enabled: always,
            run: insert,
            journal: false,
        },
        CommandSpec {
            id: "actions.dialog",
            label: "Toggle Step Dialog",
            menu: &[],
            shortcut: None,
            params: r##"{"action":name|index, "step":index?, "enabled":bool?} → {action, dialogs:[step…]}. The Actions panel's dialog toggle: the desktop shell shows the step's dialog, seeded with its recorded params, and resumes playback when it is confirmed; headless playback runs the recorded params. Omitted enabled flips it; without step, every step: on unless all already are, or as enabled says."##,
            enabled: always,
            run: dialog,
            journal: false,
        },
        CommandSpec {
            id: "actions.assignSet",
            label: "Move to Set",
            menu: &[],
            shortcut: None,
            params: r##"{"action":name|index, "set":name|null} → {action, set}. The set folder the Actions panel shows the action under (what Load Actions fills and Save Actions writes); null takes it out of any set."##,
            enabled: always,
            run: assign_set,
            journal: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions_cmds::Action;

    fn session_with(steps: &[&str]) -> Session {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
        s.actions.list.push(Action::new("A", steps.iter().map(|id| (id.to_string(), json!({}))).collect()));
        s
    }

    #[test]
    fn insert_places_the_step_and_shifts_the_flags() {
        let mut s = session_with(&["select.all", "select.inverse", "select.deselect"]);
        s.execute("actions.toggle", json!({"action": "A", "step": 1})).unwrap();
        s.execute("actions.dialog", json!({"action": "A", "step": 2})).unwrap();
        let r = s
            .execute("actions.insert", json!({"action": "A", "at": 1, "id": "actions.stopMessage", "params": {"message": "Check", "allowContinue": true}}))
            .unwrap();
        assert_eq!((r["step"].as_u64(), r["steps"].as_u64()), (Some(1), Some(4)));
        let a = &s.actions.list[0];
        assert_eq!(a.steps[1], ("actions.stopMessage".to_string(), json!({"message": "Check", "allowContinue": true})));
        assert_eq!(a.disabled.iter().copied().collect::<Vec<_>>(), [2], "the switched-off step moved down one");
        assert_eq!(a.dialogs.iter().copied().collect::<Vec<_>>(), [3]);
        // Default position: the end. A bare id works for a parameterless command.
        let r = s.execute("actions.insert", json!({"action": 0, "id": "layer.new.layer"})).unwrap();
        assert_eq!(r["step"], 4);
        assert_eq!(s.actions.list[0].steps[4].0, "layer.new.layer");
        // Playback runs the inserted stop and continues past it.
        let played = s.execute("actions.play", json!({"action": "A"})).unwrap();
        assert_eq!(played["ran"], 4, "{played}");
        assert!(played.get("failed").is_none(), "{played}");
    }

    #[test]
    fn insert_while_recording_keeps_pending_steps_in_order() {
        let mut s = session_with(&[]);
        s.execute("actions.record", json!({"action": "A"})).unwrap();
        s.execute("layer.new.layer", json!({"name": "One"})).unwrap();
        s.execute("actions.insert", json!({"action": "A", "at": 0, "id": "select.all"})).unwrap();
        s.execute("layer.new.layer", json!({"name": "Two"})).unwrap();
        s.execute("actions.stop", json!({})).unwrap();
        let ids: Vec<&str> = s.actions.list[0].steps.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["select.all", "layer.new.layer", "layer.new.layer"]);
    }

    #[test]
    fn dialog_toggles_one_step_or_all() {
        let mut s = session_with(&["select.all", "select.inverse"]);
        assert_eq!(s.execute("actions.dialog", json!({"action": "A", "step": 0})).unwrap()["dialogs"], json!([0]));
        assert_eq!(s.execute("actions.dialog", json!({"action": "A", "step": 0})).unwrap()["dialogs"], json!([]));
        assert_eq!(s.execute("actions.dialog", json!({"action": "A", "step": 1, "enabled": true})).unwrap()["dialogs"], json!([1]));
        assert_eq!(s.execute("actions.dialog", json!({"action": "A"})).unwrap()["dialogs"], json!([0, 1]), "not all on: all on");
        assert_eq!(s.execute("actions.dialog", json!({"action": "A"})).unwrap()["dialogs"], json!([]), "all on: all off");
        assert_eq!(s.execute("actions.dialog", json!({"action": "A", "enabled": true})).unwrap()["dialogs"], json!([0, 1]));
        assert_eq!(s.execute("actions.get", json!({"action": "A"})).unwrap()["dialogs"], json!([0, 1]));
        // Deleting a step keeps the flags of the others in place.
        s.execute("actions.delete", json!({"action": "A", "step": 0})).unwrap();
        assert_eq!(s.actions.list[0].dialogs.iter().copied().collect::<Vec<_>>(), [0]);
    }

    #[test]
    fn assign_set_moves_an_action_between_folders() {
        let mut s = session_with(&["select.all"]);
        assert_eq!(s.execute("actions.assignSet", json!({"action": "A", "set": " Retouch "})).unwrap()["set"], "Retouch");
        assert_eq!(s.actions.list[0].set.as_deref(), Some("Retouch"));
        assert_eq!(s.execute("actions.list", json!({})).unwrap()["actions"][0]["set"], "Retouch");
        assert_eq!(s.execute("actions.assignSet", json!({"action": 0, "set": null})).unwrap()["set"], Value::Null);
        assert_eq!(s.actions.list[0].set, None);
    }

    #[test]
    fn bad_params_fail_gracefully() {
        let mut s = session_with(&["select.all"]);
        let before = s.actions.list.clone();
        for p in [
            json!({}),
            json!({"action": "A"}),
            json!({"action": "A", "id": ""}),
            json!({"action": "A", "id": 5}),
            json!({"action": "A", "id": "not.a.command"}),
            json!({"action": "A", "id": "select.all", "at": 2}),
            json!({"action": "A", "id": "select.all", "at": -1}),
            json!({"action": "A", "id": "select.all", "params": "x"}),
            json!({"action": "A", "id": "actions.play", "params": {"action": "A"}}),
            json!({"action": 9, "id": "select.all"}),
        ] {
            assert!(s.execute("actions.insert", p.clone()).is_err(), "{p}");
        }
        for p in
            [json!({}), json!({"action": "A", "step": 1}), json!({"action": "A", "step": "x"}), json!({"action": "A", "enabled": "yes"}), json!({"action": 3})]
        {
            assert!(s.execute("actions.dialog", p.clone()).is_err(), "{p}");
        }
        for p in [json!({}), json!({"action": "A", "set": ""}), json!({"action": "A", "set": 7}), json!({"action": "B", "set": "S"})] {
            assert!(s.execute("actions.assignSet", p.clone()).is_err(), "{p}");
        }
        assert_eq!(s.actions.list, before, "rejected edits change nothing");
        // Nothing edits during playback.
        s.actions.list.push(Action::new("Loop", vec![("actions.insert".into(), json!({"action": "A", "id": "select.all"}))]));
        let played = s.execute("actions.play", json!({"action": "Loop"})).unwrap();
        assert!(played["failed"]["error"].as_str().unwrap().contains("playback"), "{played}");
    }
}
