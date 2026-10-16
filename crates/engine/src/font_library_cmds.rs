//! The font menu's local library: favourite font families (the star beside each font, kept until
//! un-starred) and recently used ones (most recent first, at most
//! [`MAX_RECENT_FONTS`] and fewer when Preferences ▸ Type ▸ Number of Recent Fonts says so). Both
//! live in the preferences, so they survive a restart, and are plain family names: a favourite
//! whose font is uninstalled stays listed until removed, as in Photoshop.

use serde_json::{Value, json};

use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

/// The most recent fonts the font menu lists.
pub const MAX_RECENT_FONTS: u32 = 5;
/// The most favourites kept (a hand-edited preferences file can't grow the menu without bound).
pub const MAX_FAVORITE_FONTS: usize = 1000;
/// The longest family name accepted, in characters.
const MAX_FAMILY_CHARS: usize = 256;

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

fn always(_: &Session) -> std::result::Result<(), String> {
    Ok(())
}

/// The `family` param: a non-empty name without control characters, trimmed.
fn family(p: &Value, cmd: &str) -> Result<String> {
    let name = p.get("family").and_then(Value::as_str).ok_or_else(|| bad(cmd, "pass `family`, a font family name"))?.trim();
    if name.is_empty() {
        return Err(bad(cmd, "`family` is empty"));
    }
    if name.chars().count() > MAX_FAMILY_CHARS {
        return Err(bad(cmd, format!("`family` is longer than {MAX_FAMILY_CHARS} characters")));
    }
    if name.chars().any(char::is_control) {
        return Err(bad(cmd, "`family` contains control characters"));
    }
    Ok(name.to_string())
}

/// How many recent fonts are kept: the preference, capped at [`MAX_RECENT_FONTS`].
pub fn recent_capacity(s: &Session) -> usize {
    s.prefs().type_.recent_fonts.min(MAX_RECENT_FONTS) as usize
}

/// The recent fonts as the menu shows them: valid names, once each, at most the capacity.
pub fn recent_fonts(s: &Session) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in &s.prefs().type_.recent_font_list {
        if out.len() >= recent_capacity(s) {
            break;
        }
        if !f.trim().is_empty() && !out.contains(f) {
            out.push(f.clone());
        }
    }
    out
}

/// The favourites as the menu shows them: valid names, once each, at most [`MAX_FAVORITE_FONTS`].
pub fn favorite_fonts(s: &Session) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in &s.prefs().type_.favorite_fonts {
        if out.len() >= MAX_FAVORITE_FONTS {
            break;
        }
        if !f.trim().is_empty() && !out.contains(f) {
            out.push(f.clone());
        }
    }
    out
}

/// Star or un-star a family (`on` sets it; without it the star toggles).
fn favorite(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "type.font.favorite";
    let name = family(p, CMD)?;
    let mut list = favorite_fonts(s);
    let on = match p.get("on") {
        None => !list.contains(&name),
        Some(v) => v.as_bool().ok_or_else(|| bad(CMD, "`on` must be true or false"))?,
    };
    list.retain(|f| *f != name);
    if on {
        if list.len() >= MAX_FAVORITE_FONTS {
            return Err(bad(CMD, format!("at most {MAX_FAVORITE_FONTS} favourite fonts")));
        }
        list.push(name.clone());
    }
    let stored = list.clone();
    s.prefs.edit(|p| p.type_.favorite_fonts = stored);
    Ok(json!({"family": name, "favorite": on, "favorites": list}))
}

/// Record a family as just used: it moves to the front of the recent list.
fn note_recent(s: &mut Session, p: &Value) -> Result<Value> {
    let name = family(p, "type.font.noteRecent")?;
    let cap = recent_capacity(s);
    let mut list = recent_fonts(s);
    list.retain(|f| *f != name);
    if cap > 0 {
        list.insert(0, name);
    }
    list.truncate(cap);
    let stored = list.clone();
    s.prefs.edit(|p| p.type_.recent_font_list = stored);
    Ok(json!({"recent": list}))
}

fn clear_recent(s: &mut Session, _: &Value) -> Result<Value> {
    s.prefs.edit(|p| p.type_.recent_font_list.clear());
    Ok(json!({"recent": []}))
}

fn library(s: &mut Session, _: &Value) -> Result<Value> {
    Ok(json!({"recent": recent_fonts(s), "favorites": favorite_fonts(s), "recentCapacity": recent_capacity(s)}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "type.font.favorite",
            label: "Add to Favorites",
            menu: &[],
            shortcut: None,
            params: r#"{"family":str,"on":bool?=toggle} → family, favorite, favorites"#,
            enabled: always,
            run: favorite,
            journal: true,
        },
        CommandSpec {
            id: "type.font.noteRecent",
            label: "Note Recent Font",
            menu: &[],
            shortcut: None,
            params: r#"{"family":str} → recent (most recent first, at most 5)"#,
            enabled: always,
            run: note_recent,
            journal: true,
        },
        CommandSpec {
            id: "type.font.clearRecent",
            label: "Clear Recent Font List",
            menu: &[],
            shortcut: None,
            params: "{}",
            enabled: always,
            run: clear_recent,
            journal: true,
        },
        CommandSpec {
            id: "type.font.library",
            label: "Font Library",
            menu: &[],
            shortcut: None,
            params: "{} → recent, favorites, recentCapacity",
            enabled: always,
            run: library,
            journal: false,
        },
    ]
}

#[cfg(test)]
mod tests;
