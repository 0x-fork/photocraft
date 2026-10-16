use serde_json::json;

use super::*;

fn session() -> Session {
    Session::new()
}

#[test]
fn starring_toggles_and_on_sets_a_favourite() {
    let mut s = session();
    s.execute("type.font.favorite", json!({"family": "Arial"})).unwrap();
    s.execute("type.font.favorite", json!({"family": "Bebas Neue"})).unwrap();
    assert_eq!(favorite_fonts(&s), ["Arial", "Bebas Neue"]);
    // Toggling again removes it; `on` sets it either way.
    let r = s.execute("type.font.favorite", json!({"family": "Arial"})).unwrap();
    assert_eq!(r["favorite"], false);
    s.execute("type.font.favorite", json!({"family": "Bebas Neue", "on": true})).unwrap();
    assert_eq!(favorite_fonts(&s), ["Bebas Neue"], "on: true doesn't duplicate");
    s.execute("type.font.favorite", json!({"family": "Bebas Neue", "on": false})).unwrap();
    assert!(favorite_fonts(&s).is_empty());
}

#[test]
fn recent_fonts_are_most_recent_first_once_each_and_at_most_five() {
    let mut s = session();
    assert_eq!(recent_capacity(&s), 5, "the default keeps five");
    for f in ["A", "B", "C", "D", "E", "F", "B"] {
        s.execute("type.font.noteRecent", json!({"family": f})).unwrap();
    }
    assert_eq!(recent_fonts(&s), ["B", "F", "E", "D", "C"]);
    // A larger preference (an older default of 10, or a hand-edited file) still keeps five.
    s.prefs.edit(|p| p.type_.recent_fonts = 50);
    s.execute("type.font.noteRecent", json!({"family": "G"})).unwrap();
    assert_eq!(recent_fonts(&s).len(), 5);
    // Fewer when the preference asks; 0 keeps none.
    s.prefs.edit(|p| p.type_.recent_fonts = 2);
    assert_eq!(recent_fonts(&s), ["G", "B"]);
    s.prefs.edit(|p| p.type_.recent_fonts = 0);
    s.execute("type.font.noteRecent", json!({"family": "H"})).unwrap();
    assert!(recent_fonts(&s).is_empty());
    s.prefs.edit(|p| p.type_.recent_fonts = 5);
    s.execute("type.font.noteRecent", json!({"family": "H"})).unwrap();
    s.execute("type.font.clearRecent", json!({})).unwrap();
    assert!(recent_fonts(&s).is_empty());
}

#[test]
fn the_library_survives_a_preferences_round_trip_and_hostile_lists_are_tidied() {
    let mut s = session();
    s.execute("type.font.favorite", json!({"family": "Arial"})).unwrap();
    s.execute("type.font.noteRecent", json!({"family": "Courier"})).unwrap();
    let saved = serde_json::to_value(s.prefs()).unwrap();
    assert_eq!(saved["type"]["favoriteFonts"], json!(["Arial"]));
    assert_eq!(saved["type"]["recentFontList"], json!(["Courier"]));
    let mut t = session();
    let prefs = serde_json::from_value(saved).unwrap();
    t.prefs.edit(|p| *p = prefs);
    let lib = t.execute("type.font.library", json!({})).unwrap();
    assert_eq!(lib, json!({"recent": ["Courier"], "favorites": ["Arial"], "recentCapacity": 5}));
    // Duplicates, blanks and an oversized list from a hand-edited file are tidied on read.
    t.prefs.edit(|p| {
        p.type_.favorite_fonts = (0..5000).map(|i| format!("F{}", i % 1200)).chain([String::new()]).collect();
        p.type_.recent_font_list = vec!["X".into(), "X".into(), " ".into(), "Y".into()];
    });
    assert_eq!(favorite_fonts(&t).len(), MAX_FAVORITE_FONTS);
    assert_eq!(recent_fonts(&t), ["X", "Y"]);
}

#[test]
fn an_older_recent_fonts_count_loads_capped_and_saving_still_works() {
    let mut s = session();
    let mut saved = serde_json::to_value(s.prefs()).unwrap();
    saved["type"]["recentFonts"] = json!(10);
    s.load_prefs_json(&saved.to_string()).unwrap();
    assert_eq!(s.prefs().type_.recent_fonts, MAX_RECENT_FONTS);
    // The whole Type section saves back through the range check.
    let ty = serde_json::to_value(s.prefs()).unwrap()["type"].clone();
    let values: serde_json::Map<String, serde_json::Value> =
        ty.as_object().unwrap().iter().filter(|(_, v)| !v.is_array()).map(|(k, v)| (format!("type.{k}"), v.clone())).collect();
    s.execute("prefs.set", json!({ "values": values })).unwrap();
    assert!(s.execute("prefs.set", json!({"values": {"type.recentFonts": 6}})).is_err(), "more than five is refused");
}

#[test]
fn bad_params_are_errors_and_change_nothing() {
    let mut s = session();
    s.execute("type.font.favorite", json!({"family": "Arial"})).unwrap();
    for (id, p) in [
        ("type.font.favorite", json!({})),
        ("type.font.favorite", json!({"family": 42})),
        ("type.font.favorite", json!({"family": "   "})),
        ("type.font.favorite", json!({"family": "a\u{0}b"})),
        ("type.font.favorite", json!({"family": "x".repeat(300)})),
        ("type.font.favorite", json!({"family": "Arial", "on": "yes"})),
        ("type.font.noteRecent", json!({"family": null})),
        ("type.font.noteRecent", json!({"family": ["Arial"]})),
    ] {
        assert!(s.execute(id, p.clone()).is_err(), "{id} {p}");
    }
    assert_eq!(favorite_fonts(&s), ["Arial"]);
    assert!(recent_fonts(&s).is_empty());
}
