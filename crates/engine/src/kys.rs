//! Photoshop keyboard shortcut sets (`.kys`, Edit › Keyboard Shortcuts › Save Set): parsing and
//! matching rows to our commands (#1163). `edit.keyboardShortcuts {"importKys": <xml>}` applies a
//! set here; the app's Keyboard Shortcuts dialog fills its overrides from the same plan
//! (`photocraft_ui_egui::kys_import`).
//!
//! A `.kys` is XML: `<command kind="static|dynamic" name="Gaussian Blur...">` rows, each with
//! one or more `<shortcut>Control+Opt+Cmd+G</shortcut>`, then the `<tool>` keys. Photoshop
//! names the command by its menu label alone, so rows are matched to our commands by label.
//! Where one label sits in several menus (Levels… under Image › Adjustments and Layer › New
//! Adjustment Layer; All under Select, Edit › Purge, View › Show), the command whose default is
//! one of the imported keys wins, then the one that has a default at all, then the first
//! candidate (the app lists them in Photoshop's menu order). A row's first shortcut becomes the
//! command's main key, unless one of its keys is already the command's default (Zoom In lists
//! `Cmd++` before `Cmd+=`); its other keys become the command's alternates (#3665: Copy is
//! `Cmd+C` and `F3`, and importing it must not leave `Cmd+C` unbound). Tool keys are not
//! imported: the tool letters are fixed here.

use std::collections::{BTreeMap, HashMap};

use crate::prefs::normalize_shortcut;

/// A set is a few tens of kilobytes; anything far larger is not one and is not parsed.
pub const MAX_BYTES: usize = 4 << 20;

/// One `<command>` row: Photoshop's label and its shortcuts, first one primary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KysCommand {
    pub name: String,
    pub shortcuts: Vec<String>,
}

/// The parsed set: its name, its command rows and how many tools carry a key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KysSet {
    /// The set's name as Photoshop shows it (`texcuts`), from the root's `filename` attribute,
    /// which a saved set spells `$$$/FileName/Presets/KeyboardCustomization/texcuts=texcuts`.
    pub name: Option<String>,
    pub commands: Vec<KysCommand>,
    pub tool_keys: usize,
}

/// Parse a `.kys` file (or Photoshop's live `Keyboard Shortcuts.psp`, the same XML). Rows
/// without a name or a shortcut are skipped.
pub fn parse(text: &str) -> Result<KysSet, String> {
    if text.len() > MAX_BYTES {
        return Err("not a keyboard shortcut set (.kys): larger than 4 MiB".into());
    }
    let text = text.trim_start_matches('\u{feff}');
    let doc = roxmltree::Document::parse(text).map_err(|e| format!("not a keyboard shortcut set (.kys): {e}"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "photoshop-keyboard-shortcuts" {
        return Err("not a keyboard shortcut set (.kys): no <photoshop-keyboard-shortcuts> root".into());
    }
    let name = root.attribute("filename").map(|f| f.rsplit(['=', '/']).next().unwrap_or(f).trim()).filter(|n| !n.is_empty()).map(str::to_string);
    let mut set = KysSet { name, ..Default::default() };
    for node in root.children().filter(|n| n.is_element()) {
        match node.tag_name().name() {
            "command" => {
                let Some(name) = node.attribute("name").map(str::trim).filter(|n| !n.is_empty()) else { continue };
                let shortcuts: Vec<String> = node
                    .children()
                    .filter(|n| n.is_element() && n.tag_name().name() == "shortcut")
                    .filter_map(|n| n.text().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string))
                    .collect();
                if !shortcuts.is_empty() {
                    set.commands.push(KysCommand { name: name.to_string(), shortcuts });
                }
            }
            "tool" if node.text().is_some_and(|t| !t.trim().is_empty()) => set.tool_keys += 1,
            _ => {}
        }
    }
    Ok(set)
}

/// What an import would change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Command id → the normalised shortcut from the file, for every matched row.
    pub set: BTreeMap<String, String>,
    /// Photoshop names with no command here (scripts, Bridge, the macOS application menu…).
    pub unknown: Vec<String>,
    /// Rows whose shortcut could not be read (`unknown` does not list them).
    pub unreadable: Vec<String>,
    /// Second and later shortcuts of the matched rows (counted; kept in `alternate_keys`).
    pub alternates: usize,
    /// Command id → the matched row's other keys (normalised, without the main key in `set`),
    /// for every matched row: a set lists each command's whole binding, so an empty list
    /// means the command has no alternates in it.
    pub alternate_keys: BTreeMap<String, Vec<String>>,
}

/// One command a row may match: its id, its label and its default shortcut.
pub struct Candidate {
    pub id: String,
    pub label: String,
    pub default: Option<String>,
}

/// Photoshop's label and ours compared loosely: case, a trailing ellipsis (`...` or `…`) and
/// surrounding space do not count.
fn key(label: &str) -> String {
    let l = label.trim().replace("...", "…");
    l.trim_end_matches('…').trim().to_lowercase()
}

/// The engine's own candidates: every command in a menu, in command order.
pub fn command_candidates() -> Vec<Candidate> {
    crate::command_specs()
        .iter()
        .filter(|c| !c.menu.is_empty())
        .map(|c| Candidate { id: c.id.to_string(), label: c.label.to_string(), default: c.shortcut.map(str::to_string) })
        .collect()
}

/// Match the rows to `candidates` (see the module notes for the tie-breaks). One command may be
/// listed under several labels (the menu's Settings… is the command's General…); under each
/// label it counts once.
pub fn plan(candidates: impl IntoIterator<Item = Candidate>, set: &KysSet) -> Plan {
    let mut by_label: HashMap<String, Vec<(String, Option<String>)>> = HashMap::new();
    for c in candidates {
        let entry = by_label.entry(key(&c.label)).or_default();
        if !entry.iter().any(|(id, _)| *id == c.id) {
            entry.push((c.id, c.default));
        }
    }
    let mut plan = Plan::default();
    for row in &set.commands {
        let Some(candidates) = by_label.get(&key(&row.name)) else {
            plan.unknown.push(row.name.clone());
            continue;
        };
        let Some(sc) = row.shortcuts.first().and_then(|s| normalize_shortcut(s)) else {
            plan.unreadable.push(row.name.clone());
            continue;
        };
        plan.alternates += row.shortcuts.len().saturating_sub(1);
        let imported: Vec<String> = row.shortcuts.iter().filter_map(|s| normalize_shortcut(s)).collect();
        let pick = candidates
            .iter()
            .find(|(_, d)| d.as_deref().and_then(normalize_shortcut).is_some_and(|d| imported.contains(&d)))
            .or_else(|| candidates.iter().find(|(_, d)| d.is_some()))
            .or_else(|| candidates.first());
        if let Some((id, default)) = pick {
            // A row that lists the command's own default among its keys keeps the default
            // (Zoom In is "Cmd++" then "Cmd+=" in Photoshop; ours is Cmd+=).
            let default = default.as_deref().and_then(normalize_shortcut);
            let sc = default.filter(|d| imported.contains(d)).unwrap_or(sc);
            let mut rest: Vec<String> = Vec::new();
            for k in imported {
                if k != sc && !rest.contains(&k) && rest.len() < crate::prefs::MAX_ALTERNATES {
                    rest.push(k);
                }
            }
            plan.alternate_keys.insert(id.clone(), rest);
            plan.set.insert(id.clone(), sc);
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    const SET: &str = "\u{feff}<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<photoshop-keyboard-shortcuts version=\"4\" filename=\"$$$/FileName/Presets/KeyboardCustomization/texcuts=texcuts\">
\t<command kind=\"static\" name=\"Levels...\" id=\"1801\"><shortcut>Cmd+L</shortcut></command>
\t<command kind=\"static\" name=\"Undo\" id=\"101\"><shortcut>Cmd+Z</shortcut><shortcut>F1</shortcut></command>
\t<command kind=\"static\" name=\"Hide Photoshop\" id=\"7001\"><shortcut>Cmd+H</shortcut></command>
\t<command kind=\"static\" name=\"Odd\" id=\"1\"><shortcut>Cmd+Shift+Ctrl</shortcut></command>
\t<tool name=\"Move Tool\" type=\"1\">V</tool>
</photoshop-keyboard-shortcuts>
";

    fn c(id: &str, label: &str, default: Option<&str>) -> Candidate {
        Candidate { id: id.into(), label: label.into(), default: default.map(str::to_string) }
    }

    #[test]
    fn rows_match_by_label_and_the_default_holder_wins_a_shared_label() {
        let set = parse(SET).unwrap();
        assert_eq!(set.name.as_deref(), Some("texcuts"));
        assert_eq!(set.tool_keys, 1);
        let p = plan(
            [c("layer.newLevels", "Levels…", None), c("image.levels", "Levels…", Some("Cmd+L")), c("edit.undo", "Undo", Some("Cmd+Z")), c("odd", "Odd", None)],
            &set,
        );
        assert_eq!(p.set.get("image.levels").map(String::as_str), Some("Cmd+L"));
        assert!(!p.set.contains_key("layer.newLevels"));
        assert_eq!(p.unknown, ["Hide Photoshop"]);
        assert_eq!(p.unreadable, ["Odd"]);
        assert_eq!(p.alternates, 1);
        // #3665: the row's second key is kept as the command's alternate, not dropped.
        assert_eq!(p.alternate_keys.get("edit.undo"), Some(&vec!["F1".to_string()]));
        assert_eq!(p.alternate_keys.get("image.levels"), Some(&Vec::new()));
    }

    #[test]
    fn a_row_with_the_default_and_a_function_key_keeps_both() {
        let xml =
            "<photoshop-keyboard-shortcuts><command name=\"Copy\"><shortcut>F3</shortcut><shortcut>Cmd+C</shortcut></command></photoshop-keyboard-shortcuts>";
        let p = plan([c("edit.copy", "Copy", Some("Cmd+C"))], &parse(xml).unwrap());
        assert_eq!(p.set.get("edit.copy").map(String::as_str), Some("Cmd+C"));
        assert_eq!(p.alternate_keys.get("edit.copy"), Some(&vec!["F3".to_string()]));
    }

    #[test]
    fn oversized_input_is_refused_before_parsing() {
        let big = format!("<photoshop-keyboard-shortcuts>{}</photoshop-keyboard-shortcuts>", " ".repeat(MAX_BYTES));
        assert!(parse(&big).unwrap_err().contains("4 MiB"));
        assert!(parse("<other/>").unwrap_err().contains("not a keyboard shortcut set"));
    }
}
