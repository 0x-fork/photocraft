//! Photoshop Actions files (`.atn`): Load Actions (`actions.load`) and Save Actions
//! (`actions.save`), plus the two step commands a Photoshop set needs: `actions.stopMessage`
//! (Photoshop's Insert Stop) and `actions.photoshopStep` (a step PhotoCraft has no command for
//! yet, kept verbatim so Save Actions writes it back unchanged).
//!
//! Parsing and writing live in `photocraft-psd` (`atn`); this module maps Photoshop *events*
//! (`gaussianBlur` with `Rds`, `make` of an adjustment layer, `set` of the selection…) onto
//! PhotoCraft commands and back. An event is keyed by both its four-character code (`GsnB`,
//! older files) and its string name (`gaussianBlur`, Photoshop 2024 and later), as the two are
//! interchangeable in Photoshop. The table ([`EVENTS`]) is bidirectional: a step that was
//! imported exports to the same event. Events outside it become `actions.photoshopStep` steps,
//! which fail at playback with the step's Photoshop name, so an action never silently skips
//! work; tool selections and other steps with no document effect are imported switched off.
//!
//! Layer references by name (`select` of layer "Shadow") become `{"layer": "Shadow"}`, which
//! `commands::layer_param` resolves in whatever document the action plays on (#219).

use photocraft_psd::atn::{self, AtnAction, AtnEvent, AtnFile, AtnItem};
use photocraft_psd::descriptor::{Class, Descriptor, Id, ReferenceItem, UnicodeString, Value as D};
use serde_json::{Map, Value, json};

use crate::actions_cmds::Action;
use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

/// A step PhotoCraft cannot play yet, kept verbatim (`{"event", "name", "descriptor": base64}`).
pub const PHOTOSHOP_STEP: &str = "actions.photoshopStep";
/// Photoshop's Insert Stop: `{"message", "allowContinue"}`.
pub const STOP_MESSAGE: &str = "actions.stopMessage";
/// The set name Save Actions uses for actions that came from no set.
pub const DEFAULT_SET: &str = "PhotoCraft Actions";

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

fn always(_: &Session) -> std::result::Result<(), String> {
    Ok(())
}

// ------------------------------------------------------------------ descriptor access

fn num(v: &D) -> Option<f64> {
    match v {
        D::Integer(i) => Some(f64::from(*i)),
        D::LargeInteger(i) => Some(*i as f64),
        D::Double(d) | D::UnitFloat { value: d, .. } => Some(*d),
        _ => None,
    }
    .filter(|f| f.is_finite())
}

fn trimmed(id: &Id) -> String {
    String::from_utf8_lossy(id.as_bytes()).trim_end().to_string()
}

fn get_num(d: &Descriptor, k: &str) -> Option<f64> {
    d.get(k).and_then(num)
}

fn get_bool(d: &Descriptor, k: &str) -> Option<bool> {
    match d.get(k) {
        Some(D::Boolean(b)) => Some(*b),
        _ => None,
    }
}

fn get_text(d: &Descriptor, k: &str) -> Option<String> {
    match d.get(k) {
        Some(D::Text(t)) => Some(t.to_string_lossy()),
        _ => None,
    }
}

/// An enumerated value's id, trailing spaces removed (`Ordn.Trgt` → `"Trgt"`).
fn get_enum(d: &Descriptor, k: &str) -> Option<String> {
    match d.get(k) {
        Some(D::Enumerated { value, .. }) => Some(trimmed(value)),
        _ => None,
    }
}

fn get_obj<'a>(d: &'a Descriptor, k: &str) -> Option<&'a Descriptor> {
    match d.get(k) {
        Some(D::Descriptor(o) | D::GlobalObject(o)) => Some(o),
        _ => None,
    }
}

fn get_list<'a>(d: &'a Descriptor, k: &str) -> Option<&'a [D]> {
    match d.get(k) {
        Some(D::List(l)) => Some(l),
        _ => None,
    }
}

fn get_ref<'a>(d: &'a Descriptor, k: &str) -> Option<&'a [ReferenceItem]> {
    match d.get(k) {
        Some(D::Reference(r)) => Some(r),
        _ => None,
    }
}

/// The class of a reference's first item, trimmed (`Lyr`, `Chnl`, `AdjL`…).
fn ref_class(r: &[ReferenceItem]) -> Option<String> {
    Some(trimmed(match r.first()? {
        ReferenceItem::Property { class, .. }
        | ReferenceItem::Class(class)
        | ReferenceItem::Enumerated { class, .. }
        | ReferenceItem::Offset { class, .. }
        | ReferenceItem::Identifier { class, .. }
        | ReferenceItem::Index { class, .. }
        | ReferenceItem::Name { class, .. } => &class.class_id,
    }))
}

/// How a reference names its target: the ordinal (`Trgt`, `Frnt`…), a name, a property…
enum Target {
    Ordinal(String),
    Name(String),
    Property(String),
    Class,
    Offset(i32),
    Other,
}

fn target(r: &[ReferenceItem]) -> Target {
    match r.first() {
        Some(ReferenceItem::Enumerated { value, .. }) => Target::Ordinal(trimmed(value)),
        Some(ReferenceItem::Name { name, .. }) => Target::Name(name.to_string_lossy()),
        Some(ReferenceItem::Property { key, .. }) => Target::Property(trimmed(key)),
        Some(ReferenceItem::Class(_)) => Target::Class,
        Some(ReferenceItem::Offset { offset, .. }) => Target::Offset(*offset),
        _ => Target::Other,
    }
}

/// `{"layer": name}` for a reference that names a layer, `{}` for the target layer, `None`
/// for anything else (an index or id: meaningless in another document).
fn layer_target(r: &[ReferenceItem]) -> Option<Map<String, Value>> {
    let mut m = Map::new();
    match target(r) {
        Target::Ordinal(o) if o == "Trgt" => {}
        Target::Name(n) => {
            m.insert("layer".into(), json!(n));
        }
        Target::Property(p) if p == "Bckg" => {
            m.insert("layer".into(), json!("Background"));
        }
        _ => return None,
    }
    Some(m)
}

// ------------------------------------------------------------------ descriptor building

/// A Photoshop id: codes shorter than four characters are space-padded (`H   `, `Rd  `),
/// longer names stay strings.
fn code(s: &str) -> Id {
    if s.len() < 4 { Id::new(&format!("{s:<4}")) } else { Id::new(s) }
}

fn class(c: &str) -> Class {
    Class { name: UnicodeString::new_nul(""), class_id: code(c) }
}

/// The pixel unit of a `UntF` value.
const PX: [u8; 4] = *b"#Pxl";

fn px(v: f64) -> D {
    D::UnitFloat { unit: PX, value: v }
}

fn pct(v: f64) -> D {
    D::UnitFloat { unit: *b"#Prc", value: v }
}

fn ang(v: f64) -> D {
    D::UnitFloat { unit: *b"#Ang", value: v }
}

fn long(v: f64) -> D {
    D::Integer(v.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32)
}

fn text(s: &str) -> D {
    D::Text(UnicodeString::new_nul(s))
}

fn enm(ty: &str, v: &str) -> D {
    D::Enumerated { type_id: code(ty), value: code(v) }
}

fn target_ref(cls: &str) -> D {
    D::Reference(vec![ReferenceItem::Enumerated { class: class(cls), type_id: Id::new("Ordn"), value: Id::new("Trgt") }])
}

fn name_ref(cls: &str, name: &str) -> D {
    D::Reference(vec![ReferenceItem::Name { class: class(cls), name: UnicodeString::new_nul(name) }])
}

fn class_ref(cls: &str) -> D {
    D::Reference(vec![ReferenceItem::Class(class(cls))])
}

/// The `null` reference for a command's `layer` param: by name, else the target layer.
fn layer_ref(p: &Value) -> D {
    match p.get("layer") {
        Some(Value::String(n)) => name_ref("Lyr ", n),
        _ => target_ref("Lyr "),
    }
}

fn desc(cls: &str) -> Descriptor {
    Descriptor::new(cls)
}

// ------------------------------------------------------------------ JSON access

fn jnum(p: &Value, k: &str) -> Option<f64> {
    p.get(k).and_then(Value::as_f64).filter(|f| f.is_finite())
}

fn jbool(p: &Value, k: &str) -> Option<bool> {
    p.get(k).and_then(Value::as_bool)
}

fn jstr<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str)
}

// ------------------------------------------------------------------ colours and enums

/// A Photoshop colour object as `#rrggbb`: `RGBC` (0..255), `HSBC`, `Grsc`.
fn color_hex(c: &Descriptor) -> Option<String> {
    let hex = |r: f64, g: f64, b: f64| {
        format!("#{:02x}{:02x}{:02x}", r.round().clamp(0.0, 255.0) as u8, g.round().clamp(0.0, 255.0) as u8, b.round().clamp(0.0, 255.0) as u8)
    };
    if let (Some(r), Some(g), Some(b)) = (get_num(c, "Rd  "), get_num(c, "Grn "), get_num(c, "Bl  ")) {
        return Some(hex(r, g, b));
    }
    if let (Some(h), Some(s), Some(v)) = (get_num(c, "H   "), get_num(c, "Strt"), get_num(c, "Brgh")) {
        let (s, v) = ((s / 100.0).clamp(0.0, 1.0), (v / 100.0).clamp(0.0, 1.0));
        let h = h.rem_euclid(360.0) / 60.0;
        let i = h.floor();
        let f = h - i;
        let (p, q, t) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
        let (r, g, b) = match i as i32 {
            0 => (v, t, p),
            1 => (q, v, p),
            2 => (p, v, t),
            3 => (p, q, v),
            4 => (t, p, v),
            _ => (v, p, q),
        };
        return Some(hex(r * 255.0, g * 255.0, b * 255.0));
    }
    get_num(c, "Gry ").map(|g| {
        let v = 255.0 - g * 2.55;
        hex(v, v, v)
    })
}

fn color_desc(hex: &str) -> Option<Descriptor> {
    let h = hex.strip_prefix('#')?;
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let ch = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok().map(f64::from);
    Some(desc("RGBC").with("Rd  ", D::Double(ch(0)?)).with("Grn ", D::Double(ch(2)?)).with("Bl  ", D::Double(ch(4)?)))
}

/// Photoshop's `BlnM` enumeration ↔ PhotoCraft's blend mode labels.
const BLEND_MODES: &[(&str, &str)] = &[
    ("Nrml", "Normal"),
    ("Dslv", "Dissolve"),
    ("Drkn", "Darken"),
    ("Mltp", "Multiply"),
    ("CBrn", "Color Burn"),
    ("linearBurn", "Linear Burn"),
    ("darkerColor", "Darker Color"),
    ("Lghn", "Lighten"),
    ("Scrn", "Screen"),
    ("CDdg", "Color Dodge"),
    ("linearDodge", "Linear Dodge (Add)"),
    ("lighterColor", "Lighter Color"),
    ("Ovrl", "Overlay"),
    ("SftL", "Soft Light"),
    ("HrdL", "Hard Light"),
    ("vividLight", "Vivid Light"),
    ("linearLight", "Linear Light"),
    ("pinLight", "Pin Light"),
    ("hardMix", "Hard Mix"),
    ("Dfrn", "Difference"),
    ("Xclu", "Exclusion"),
    ("blendSubtraction", "Subtract"),
    ("blendDivide", "Divide"),
    ("H", "Hue"),
    ("Strt", "Saturation"),
    ("Clr", "Color"),
    ("Lmns", "Luminosity"),
    ("PsTh", "Pass Through"),
];

fn blend_label(ps: &str) -> Option<&'static str> {
    BLEND_MODES.iter().find(|(k, _)| *k == ps).map(|(_, v)| *v)
}

fn blend_ps(label: &str) -> Option<&'static str> {
    crate::commands::blend_from_str(label).and_then(|m| BLEND_MODES.iter().find(|(_, v)| crate::commands::blend_from_str(v) == Some(m))).map(|(k, _)| *k)
}

/// Photoshop ↔ PhotoCraft value tables; Photoshop codes compare without their space padding.
fn lookup<'a>(table: &'a [(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    table.iter().find(|(k, _)| k.trim_end() == key.trim_end()).map(|(_, v)| *v)
}

fn lookup_back<'a>(table: &'a [(&'a str, &'a str)], value: &str) -> Option<&'a str> {
    table.iter().find(|(_, v)| *v == value).map(|(k, _)| *k)
}

/// `Chnl` enumeration ↔ the per-channel keys of the adjustment commands.
const CHANNELS: &[(&str, &str)] = &[
    ("Rd", "red"),
    ("Grn", "green"),
    ("Bl", "blue"),
    ("Gry", "gray"),
    ("Cyn", "cyan"),
    ("Mgnt", "magenta"),
    ("Yllw", "yellow"),
    ("Blck", "black"),
    ("Lmnc", "lightness"),
    ("A", "a"),
    ("B", "b"),
];

/// The channel an adjustment entry's `Chnl` reference names: `None` = the composite.
fn channel_key(d: &Descriptor) -> Option<Option<&'static str>> {
    let r = get_ref(d, "Chnl")?;
    Some(match target(r) {
        Target::Ordinal(o) if o == "Trgt" || o == "Cmps" => None,
        Target::Ordinal(o) => Some(lookup(CHANNELS, &o)?),
        _ => return None,
    })
}

fn channel_ref(key: Option<&str>) -> D {
    let value = key.and_then(|k| lookup_back(CHANNELS, k));
    D::Reference(vec![ReferenceItem::Enumerated {
        class: class("Chnl"),
        type_id: code(if value.is_some() { "Chnl" } else { "Ordn" }),
        value: code(value.unwrap_or("Trgt")),
    }])
}

// ------------------------------------------------------------------ field tables

#[derive(Clone, Copy)]
enum Kind {
    Px,
    Pct,
    Ang,
    Long,
    Dbl,
    Bool,
    /// An enumeration: its type id and the Photoshop ↔ PhotoCraft value pairs.
    Enum(&'static str, &'static [(&'static str, &'static str)]),
    /// A blend mode (`BlnM`).
    Blend,
    /// A colour object ↔ `#rrggbb`.
    Color,
}

/// One descriptor key ↔ one command param.
struct F(&'static str, &'static str, Kind);

fn fields_from(d: &Descriptor, fields: &[F]) -> Option<Map<String, Value>> {
    let mut m = Map::new();
    for F(key, param, kind) in fields {
        let v = match kind {
            Kind::Px | Kind::Pct | Kind::Ang | Kind::Long | Kind::Dbl => get_num(d, key).map(Value::from),
            Kind::Bool => get_bool(d, key).map(Value::from),
            Kind::Enum(_, table) => match get_enum(d, key) {
                Some(e) => Some(Value::from(lookup(table, &e)?)),
                None => None,
            },
            Kind::Blend => match get_enum(d, key) {
                Some(e) => Some(Value::from(blend_label(&e)?)),
                None => None,
            },
            Kind::Color => match get_obj(d, key) {
                Some(c) => Some(Value::from(color_hex(c)?)),
                None => None,
            },
        };
        if let Some(v) = v {
            m.insert((*param).into(), v);
        }
    }
    Some(m)
}

fn fields_to(p: &Value, fields: &[F], mut d: Descriptor) -> Option<Descriptor> {
    for F(key, param, kind) in fields {
        let Some(v) = p.get(param) else { continue };
        let out = match kind {
            Kind::Px => px(v.as_f64()?),
            Kind::Pct => pct(v.as_f64()?),
            Kind::Ang => ang(v.as_f64()?),
            Kind::Long => long(v.as_f64()?),
            Kind::Dbl => D::Double(v.as_f64()?),
            Kind::Bool => D::Boolean(v.as_bool()?),
            Kind::Enum(ty, table) => enm(ty, lookup_back(table, v.as_str()?)?),
            Kind::Blend => enm("BlnM", blend_ps(v.as_str()?)?),
            Kind::Color => D::Descriptor(color_desc(v.as_str()?)?),
        };
        d = d.with(key, out);
    }
    Some(d)
}

// ------------------------------------------------------------------ adjustments

/// The `Adjs` entries of a per-channel adjustment (Levels, Curves): `(channel key, entry)`.
fn per_channel(d: &Descriptor) -> Option<Vec<(Option<&'static str>, &Descriptor)>> {
    get_list(d, "Adjs")?
        .iter()
        .map(|v| match v {
            D::Descriptor(e) => Some((channel_key(e)?, e)),
            _ => None,
        })
        .collect()
}

fn levels_from(d: &Descriptor) -> Option<Value> {
    if get_bool(d, "Auto") == Some(true) {
        return None;
    }
    let mut out = Map::new();
    for (chan, e) in per_channel(d)? {
        let mut m = Map::new();
        if let Some(i) = get_list(e, "Inpt") {
            if let Some(v) = i.first().and_then(num) {
                m.insert("inBlack".into(), v.into());
            }
            if let Some(v) = i.get(1).and_then(num) {
                m.insert("inWhite".into(), v.into());
            }
        }
        if let Some(o) = get_list(e, "Otpt") {
            if let Some(v) = o.first().and_then(num) {
                m.insert("outBlack".into(), v.into());
            }
            if let Some(v) = o.get(1).and_then(num) {
                m.insert("outWhite".into(), v.into());
            }
        }
        if let Some(g) = get_num(e, "Gmm ") {
            m.insert("gamma".into(), g.into());
        }
        match chan {
            None => out.extend(m),
            Some(k) => {
                out.insert(k.into(), Value::Object(m));
            }
        }
    }
    Some(Value::Object(out))
}

fn levels_entry(chan: Option<&str>, p: &Value) -> Descriptor {
    let mut e = desc("LvlA").with("Chnl", channel_ref(chan));
    if jnum(p, "inBlack").is_some() || jnum(p, "inWhite").is_some() {
        e = e.with("Inpt", D::List(vec![long(jnum(p, "inBlack").unwrap_or(0.0)), long(jnum(p, "inWhite").unwrap_or(255.0))]));
    }
    if let Some(g) = jnum(p, "gamma") {
        e = e.with("Gmm ", D::Double(g));
    }
    if jnum(p, "outBlack").is_some() || jnum(p, "outWhite").is_some() {
        e = e.with("Otpt", D::List(vec![long(jnum(p, "outBlack").unwrap_or(0.0)), long(jnum(p, "outWhite").unwrap_or(255.0))]));
    }
    e
}

fn levels_to(p: &Value) -> Option<Descriptor> {
    let mut entries = vec![D::Descriptor(levels_entry(None, p))];
    for (_, k) in CHANNELS {
        if let Some(c) = p.get(*k).filter(|v| v.is_object()) {
            entries.push(D::Descriptor(levels_entry(Some(k), c)));
        }
    }
    Some(desc("Lvls").with("presetKind", enm("presetKindType", "presetKindCustom")).with("Adjs", D::List(entries)))
}

fn curve_points(e: &Descriptor) -> Option<Value> {
    let pts: Option<Vec<Value>> = get_list(e, "Crv ")?
        .iter()
        .map(|v| match v {
            D::Descriptor(pt) => Some(json!([get_num(pt, "Hrzn")?, get_num(pt, "Vrtc")?])),
            _ => None,
        })
        .collect();
    pts.map(Value::Array)
}

fn curves_from(d: &Descriptor) -> Option<Value> {
    let mut out = Map::new();
    for (chan, e) in per_channel(d)? {
        let pts = curve_points(e)?;
        out.insert(chan.unwrap_or("points").into(), pts);
    }
    Some(Value::Object(out))
}

fn curve_entry(chan: Option<&str>, pts: &Value) -> Option<Descriptor> {
    let list: Option<Vec<D>> = pts
        .as_array()?
        .iter()
        .map(|pt| {
            let a = pt.as_array()?;
            Some(D::Descriptor(desc("Pnt ").with("Hrzn", D::Double(a.first()?.as_f64()?)).with("Vrtc", D::Double(a.get(1)?.as_f64()?))))
        })
        .collect();
    Some(desc("CrvA").with("Chnl", channel_ref(chan)).with("Crv ", D::List(list?)))
}

fn curves_to(p: &Value) -> Option<Descriptor> {
    let mut entries = Vec::new();
    if let Some(pts) = p.get("points") {
        entries.push(D::Descriptor(curve_entry(None, pts)?));
    }
    for (_, k) in CHANNELS {
        if let Some(pts) = p.get(*k) {
            entries.push(D::Descriptor(curve_entry(Some(k), pts)?));
        }
    }
    Some(desc("Crvs").with("presetKind", enm("presetKindType", "presetKindCustom")).with("Adjs", D::List(entries)))
}

const HUE_RANGES: [&str; 6] = ["reds", "yellows", "greens", "cyans", "blues", "magentas"];

fn hue_sat_from(d: &Descriptor) -> Option<Value> {
    let mut out = Map::new();
    if let Some(c) = get_bool(d, "Clrz") {
        out.insert("colorize".into(), c.into());
    }
    for e in get_list(d, "Adjs").unwrap_or(&[]) {
        let D::Descriptor(e) = e else { return None };
        let mut m = Map::new();
        for (key, param) in [("Hue ", "hue"), ("Strt", "saturation"), ("Lght", "lightness")] {
            if let Some(v) = get_num(e, key) {
                m.insert(param.into(), v.into());
            }
        }
        match get_num(e, "LclR") {
            None => out.extend(m),
            Some(r) => {
                let name = *HUE_RANGES.get((r as usize).checked_sub(1)?)?;
                if let (Some(a), Some(b), Some(c), Some(dd)) = (get_num(e, "BgnR"), get_num(e, "BgnS"), get_num(e, "EndS"), get_num(e, "EndR")) {
                    m.insert("range".into(), json!([a, b, c, dd]));
                }
                out.insert(name.into(), Value::Object(m));
            }
        }
    }
    Some(Value::Object(out))
}

fn hue_entry(p: &Value, range: Option<(usize, &Value)>) -> Option<Descriptor> {
    let mut e = desc("Hst2");
    if let Some((i, r)) = range {
        e = e.with("LclR", long(i as f64 + 1.0));
        if let Some(a) = r.get("range").and_then(Value::as_array)
            && a.len() == 4
        {
            for (k, v) in ["BgnR", "BgnS", "EndS", "EndR"].iter().zip(a) {
                e = e.with(k, long(v.as_f64()?));
            }
        }
    }
    for (key, param) in [("Hue ", "hue"), ("Strt", "saturation"), ("Lght", "lightness")] {
        e = e.with(key, long(jnum(p, param).unwrap_or(0.0)));
    }
    Some(e)
}

fn hue_sat_to(p: &Value) -> Option<Descriptor> {
    let mut entries = vec![D::Descriptor(hue_entry(p, None)?)];
    for (i, name) in HUE_RANGES.iter().enumerate() {
        if let Some(r) = p.get(*name).filter(|v| v.is_object()) {
            entries.push(D::Descriptor(hue_entry(r, Some((i, r)))?));
        }
    }
    Some(
        desc("HStr")
            .with("presetKind", enm("presetKindType", "presetKindCustom"))
            .with("Clrz", D::Boolean(jbool(p, "colorize").unwrap_or(false)))
            .with("Adjs", D::List(entries)),
    )
}

fn triple(d: &Descriptor, k: &str) -> Option<Value> {
    let l = get_list(d, k)?;
    let v: Option<Vec<f64>> = l.iter().map(num).collect();
    Some(json!(v?))
}

fn triple_to(p: &Value, k: &str) -> Option<D> {
    Some(D::List(p.get(k)?.as_array()?.iter().map(|v| v.as_f64().map(long)).collect::<Option<Vec<_>>>()?))
}

fn color_balance_from(d: &Descriptor) -> Option<Value> {
    let mut out = Map::new();
    for (key, param) in [("ShdL", "shadows"), ("MdtL", "midtones"), ("HghL", "highlights")] {
        if let Some(v) = triple(d, key) {
            out.insert(param.into(), v);
        }
    }
    if let Some(b) = get_bool(d, "PrsL") {
        out.insert("preserveLuminosity".into(), b.into());
    }
    Some(Value::Object(out))
}

fn color_balance_to(p: &Value) -> Option<Descriptor> {
    let mut d = desc("ClrB");
    for (key, param) in [("ShdL", "shadows"), ("MdtL", "midtones"), ("HghL", "highlights")] {
        d = d.with(key, triple_to(p, param).unwrap_or_else(|| D::List(vec![long(0.0), long(0.0), long(0.0)])));
    }
    Some(d.with("PrsL", D::Boolean(jbool(p, "preserveLuminosity").unwrap_or(true))))
}

const BW: &[F] = &[
    F("Rd  ", "reds", Kind::Long),
    F("Yllw", "yellows", Kind::Long),
    F("Grn ", "greens", Kind::Long),
    F("Cyn ", "cyans", Kind::Long),
    F("Bl  ", "blues", Kind::Long),
    F("Mgnt", "magentas", Kind::Long),
    F("useTint", "tint", Kind::Bool),
    F("tintColor", "tintColor", Kind::Color),
];

fn mixer_channel(e: &Descriptor) -> Option<Value> {
    Some(json!([get_num(e, "Rd  ").unwrap_or(0.0), get_num(e, "Grn ").unwrap_or(0.0), get_num(e, "Bl  ").unwrap_or(0.0), get_num(e, "Cnst").unwrap_or(0.0)]))
}

fn channel_mixer_from(d: &Descriptor) -> Option<Value> {
    let mut out = Map::new();
    for (key, param) in [("Rd  ", "red"), ("Grn ", "green"), ("Bl  ", "blue"), ("Gry ", "gray")] {
        if let Some(e) = get_obj(d, key) {
            out.insert(param.into(), mixer_channel(e)?);
        }
    }
    if let Some(m) = get_bool(d, "Mnch") {
        out.insert("monochrome".into(), m.into());
    }
    Some(Value::Object(out))
}

fn channel_mixer_to(p: &Value) -> Option<Descriptor> {
    let mut d = desc("ChnM").with("presetKind", enm("presetKindType", "presetKindCustom"));
    for (key, param) in [("Rd  ", "red"), ("Grn ", "green"), ("Bl  ", "blue"), ("Gry ", "gray")] {
        if let Some(a) = p.get(param).and_then(Value::as_array) {
            let v = |i: usize| a.get(i).and_then(Value::as_f64).unwrap_or(0.0);
            d = d.with(key, D::Descriptor(desc("ChMx").with("Rd  ", pct(v(0))).with("Grn ", pct(v(1))).with("Bl  ", pct(v(2))).with("Cnst", pct(v(3)))));
        }
    }
    Some(d.with("Mnch", D::Boolean(jbool(p, "monochrome").unwrap_or(false))))
}

const SELECTIVE_RANGES: &[(&str, &str)] = &[
    ("Rds", "reds"),
    ("Yllw", "yellows"),
    ("Grns", "greens"),
    ("Cyns", "cyans"),
    ("Bls", "blues"),
    ("Mgnt", "magentas"),
    ("Whts", "whites"),
    ("Ntrl", "neutrals"),
    ("Blks", "blacks"),
];

fn selective_from(d: &Descriptor) -> Option<Value> {
    let mut out = Map::new();
    if let Some(m) = get_enum(d, "Mthd") {
        out.insert("method".into(), (if m == "Absl" { "absolute" } else { "relative" }).into());
    }
    for e in get_list(d, "ClrC").unwrap_or(&[]) {
        let D::Descriptor(e) = e else { return None };
        let name = lookup(SELECTIVE_RANGES, &get_enum(e, "Clrs")?)?;
        let v = |k: &str| get_num(e, k).unwrap_or(0.0);
        out.insert(name.into(), json!([v("Cyn "), v("Mgnt"), v("Yllw"), v("Blck")]));
    }
    Some(Value::Object(out))
}

fn selective_to(p: &Value) -> Option<Descriptor> {
    let mut list = Vec::new();
    for (ps, name) in SELECTIVE_RANGES {
        if let Some(a) = p.get(*name).and_then(Value::as_array) {
            let v = |i: usize| a.get(i).and_then(Value::as_f64).unwrap_or(0.0);
            list.push(D::Descriptor(
                desc("ClrC").with("Clrs", enm("Clrs", ps)).with("Cyn ", pct(v(0))).with("Mgnt", pct(v(1))).with("Yllw", pct(v(2))).with("Blck", pct(v(3))),
            ));
        }
    }
    Some(
        desc("SlcC")
            .with("presetKind", enm("presetKindType", "presetKindCustom"))
            .with("Mthd", enm("CrcM", if jstr(p, "method") == Some("absolute") { "Absl" } else { "Rltv" }))
            .with("ClrC", D::List(list)),
    )
}

/// An adjustment: its Photoshop class (also the direct event's code), the command suffix (`image.adjustments.<suffix>`, `layer.newAdjustmentLayer.<suffix>`), and
/// the converters between the adjustment descriptor and the command params.
struct Adjustment {
    class: &'static str,
    suffix: &'static str,
    from: fn(&Descriptor) -> Option<Value>,
    to: fn(&Value) -> Option<Descriptor>,
}

macro_rules! adj_fields {
    ($class:literal, $suffix:literal, $fields:expr) => {
        Adjustment { class: $class, suffix: $suffix, from: |d| fields_from(d, $fields).map(Value::Object), to: |p| fields_to(p, $fields, desc($class)) }
    };
}

const ADJUSTMENTS: &[Adjustment] = &[
    Adjustment { class: "Lvls", suffix: "levels", from: levels_from, to: levels_to },
    Adjustment { class: "Crvs", suffix: "curves", from: curves_from, to: curves_to },
    Adjustment { class: "HStr", suffix: "hueSaturation", from: hue_sat_from, to: hue_sat_to },
    adj_fields!("BrgC", "brightnessContrast", &[F("Brgh", "brightness", Kind::Long), F("Cntr", "contrast", Kind::Long), F("useLegacy", "legacy", Kind::Bool)]),
    adj_fields!("Thrs", "threshold", &[F("Lvl ", "level", Kind::Long)]),
    adj_fields!("Pstr", "posterize", &[F("Lvls", "levels", Kind::Long)]),
    adj_fields!("Invr", "invert", &[]),
    Adjustment { class: "ClrB", suffix: "colorBalance", from: color_balance_from, to: color_balance_to },
    adj_fields!("Vbrn", "vibrance", &[F("vibrance", "vibrance", Kind::Long), F("Strt", "saturation", Kind::Long)]),
    adj_fields!("Exps", "exposure", &[F("Exps", "exposure", Kind::Dbl), F("Ofst", "offset", Kind::Dbl), F("gammaCorrection", "gamma", Kind::Dbl)]),
    adj_fields!("PhtF", "photoFilter", &[F("Clr ", "color", Kind::Color), F("Dnst", "density", Kind::Long), F("PrsL", "preserveLuminosity", Kind::Bool)]),
    adj_fields!("BlnW", "blackWhite", BW),
    Adjustment { class: "ChnM", suffix: "channelMixer", from: channel_mixer_from, to: channel_mixer_to },
    Adjustment { class: "SlcC", suffix: "selectiveColor", from: selective_from, to: selective_to },
];

fn adjustment_by_class(c: &str) -> Option<&'static Adjustment> {
    ADJUSTMENTS.iter().find(|a| a.class.trim_end() == c.trim_end())
}

fn adjustment_by_suffix(suffix: &str) -> Option<&'static Adjustment> {
    ADJUSTMENTS.iter().find(|a| a.suffix == suffix)
}

// ------------------------------------------------------------------ the event table

/// What one Photoshop step maps to.
enum Mapped {
    /// One or more PhotoCraft steps (a `make` with a name becomes new layer + rename).
    Steps(Vec<(String, Value)>),
    /// No PhotoCraft equivalent; `harmless` steps (tool selections…) are imported switched off.
    Unsupported { harmless: bool },
}

fn step(id: &str, params: Value) -> Mapped {
    Mapped::Steps(vec![(id.to_string(), params)])
}

fn steps(v: Vec<(&str, Value)>) -> Mapped {
    Mapped::Steps(v.into_iter().map(|(id, p)| (id.to_string(), p)).collect())
}

fn none(id: &str) -> Mapped {
    step(id, json!({}))
}

/// An event: code (empty when Photoshop only has a string name), string name, the panel
/// label, the command ids it exports (the first is the one simple events import to), and the
/// two converters.
struct Event {
    code: &'static str,
    name: &'static str,
    label: &'static str,
    ids: &'static [&'static str],
    from: fn(&Descriptor) -> Mapped,
    to: fn(&str, &Value) -> Option<Descriptor>,
}

macro_rules! ev {
    ($code:literal, $name:literal, $label:literal, $id:literal, $fields:expr) => {
        Event {
            code: $code,
            name: $name,
            label: $label,
            ids: &[$id],
            from: |d| match fields_from(d, $fields) {
                Some(m) => step($id, Value::Object(m)),
                None => Mapped::Unsupported { harmless: false },
            },
            to: |_, p| fields_to(p, $fields, desc("null")),
        }
    };
}

const INTERPOLATION: &[(&str, &str)] = &[
    ("Bcbc", "bicubic"),
    ("Blnr", "bilinear"),
    ("Nrst", "nearest"),
    ("bicubicSharper", "bicubicSharper"),
    ("bicubicSmoother", "bicubicSmoother"),
    ("bicubicAutomatic", "bicubicAutomatic"),
    ("automaticInterpolation", "automatic"),
    ("preserveDetailsUpscale", "preserveDetails"),
    ("deepUpscale", "preserveDetails"),
];
const NOISE: &[(&str, &str)] = &[("Unfr", "uniform"), ("Gsn ", "gaussian")];
const OFFSET_FILL: &[(&str, &str)] = &[("Wrp ", "wrap"), ("Rpt ", "repeat"), ("Bckg", "transparent")];
const TRIM: &[(&str, &str)] = &[("Trns", "transparent"), ("topLeftPixelColor", "topLeft"), ("bottomRightPixelColor", "bottomRight")];
const EXTENSION: &[(&str, &str)] = &[("Bckg", "background"), ("Frgd", "foreground"), ("Wht ", "white"), ("Blck", "black"), ("Trns", "transparent")];
const FILL_CONTENTS: &[(&str, &str)] = &[
    ("FrgC", "foreground"),
    ("BckC", "background"),
    ("Wht ", "white"),
    ("Blck", "black"),
    ("Gry ", "gray"),
    ("Clr ", "color"),
    ("HstS", "history"),
    ("contentAware", "contentAware"),
];
const STROKE_LOCATION: &[(&str, &str)] = &[("Insd", "inside"), ("Cntr", "center"), ("OtsF", "outside")];
const INTENT: &[(&str, &str)] = &[("Img ", "perceptual"), ("Clrm", "relative"), ("Grp ", "saturation"), ("AbsC", "absolute")];
const PROFILES: &[(&str, &str)] =
    &[("sRGB IEC61966-2.1", "srgb"), ("Display P3", "display-p3"), ("Adobe RGB (1998)", "adobe-rgb-compat"), ("ProPhoto RGB", "prophoto-compat")];
const SMART_BLUR_QUALITY: &[(&str, &str)] = &[("Hgh ", "high"), ("Mdm ", "medium"), ("Low ", "low")];
const SMART_BLUR_MODE: &[(&str, &str)] = &[("SBMN", "normal"), ("SBME", "edgeOnly"), ("SBMO", "overlayEdge")];
const RADIAL_METHOD: &[(&str, &str)] = &[("Spn ", "spin"), ("Zm  ", "zoom")];
const RADIAL_QUALITY: &[(&str, &str)] = &[("Drft", "draft"), ("Gd  ", "good"), ("Bst ", "best")];
const LABEL_COLORS: &[(&str, &str)] =
    &[("None", "none"), ("Rd", "red"), ("Orng", "orange"), ("Ylw", "yellow"), ("Grn", "green"), ("Bl", "blue"), ("Vlt", "violet"), ("Gry", "gray")];
const ALIGN: &[(&str, &str)] =
    &[("AdLf", "leftEdges"), ("AdCH", "horizontalCenters"), ("AdRg", "rightEdges"), ("AdTp", "topEdges"), ("AdCV", "verticalCenters"), ("AdBt", "bottomEdges")];
const PURGE: &[(&str, &str)] = &[("Al", "all"), ("Clpb", "clipboard"), ("Hsty", "histories"), ("Undo", "undo")];
const MODES: &[(&str, &str)] = &[("RGBM", "rgb"), ("Grys", "grayscale"), ("CMYM", "cmyk"), ("LbCM", "lab"), ("Mltc", "multichannel")];

// Events whose target depends on the descriptor (`make`, `set`, `select`…): custom converters.

fn adjustment_steps(a: &Adjustment, prefix: &str, type_desc: Option<&Descriptor>) -> Mapped {
    let params = match type_desc {
        Some(t) if get_enum(t, "presetKind").as_deref() != Some("presetKindDefault") => match (a.from)(t) {
            Some(p) => p,
            None => return Mapped::Unsupported { harmless: false },
        },
        _ => json!({}),
    };
    step(&format!("{prefix}{}", a.suffix), params)
}

fn make_from(d: &Descriptor) -> Mapped {
    if let Some(nw) = get_obj(d, "Nw  ") {
        // Layer via Copy / Cut (`Usng` = the selection), or a new document.
        if get_enum(d, "Usng").as_deref() == Some("Slct") {
            let id = if get_bool(d, "Cpy ") == Some(true) { "layer.new.layerViaCopy" } else { "layer.new.layerViaCut" };
            let mut v = vec![(id, json!({}))];
            if let Some(n) = get_text(nw, "Nm  ") {
                v.push(("layer.renameLayer", json!({"name": n})));
            }
            return steps(v);
        }
        if get_text(nw, "preset").is_some() {
            return none("file.newFromClipboard");
        }
        if let (Some(w), Some(h)) = (get_num(nw, "Wdth"), get_num(nw, "Hght")) {
            let res = get_num(nw, "Rslt").unwrap_or(72.0);
            // Dimensions are in points (`#Rlt`) unless given in pixels.
            let to_px = |k: &str, v: f64| match nw.get(k) {
                Some(D::UnitFloat { unit: PX, .. }) => v,
                _ => v * res / 72.0,
            };
            let mut p = json!({"width": to_px("Wdth", w).round(), "height": to_px("Hght", h).round(), "resolution": res});
            if let Some(n) = get_text(nw, "Nm  ") {
                p["name"] = json!(n);
            }
            if let Some(D::Class(c)) = nw.get("Md  ")
                && let Some(m) = lookup(MODES, &trimmed(&c.class_id))
            {
                p["mode"] = json!(m);
            }
            if let Some(depth) = get_num(nw, "Dpth") {
                p["depth"] = json!(depth);
            }
            if let Some(f) = get_enum(nw, "Fl  ") {
                p["background"] = json!(match f.as_str() {
                    "Wht" => "white",
                    "BckC" => "backgroundColor",
                    "Trns" => "transparent",
                    _ => "white",
                });
            }
            return step("file.new", p);
        }
        return Mapped::Unsupported { harmless: false };
    }
    let Some(r) = get_ref(d, "null") else { return Mapped::Unsupported { harmless: false } };
    let using = get_obj(d, "Usng");
    let type_desc = using.and_then(|u| get_obj(u, "Type"));
    match ref_class(r).as_deref() {
        Some("Lyr") => {
            let mut p = Map::new();
            if let Some(u) = using {
                if let Some(n) = get_text(u, "Nm  ") {
                    p.insert("name".into(), n.into());
                }
                if let Some(o) = get_num(u, "Opct") {
                    p.insert("opacity".into(), (o / 100.0).into());
                }
                if let Some(m) = get_enum(u, "Md  ").and_then(|m| blend_label(&m)) {
                    p.insert("mode".into(), m.into());
                }
            }
            step("layer.new.layer", Value::Object(p))
        }
        Some("AdjL") => match type_desc.and_then(|t| adjustment_by_class(&trimmed(&t.class_id))) {
            Some(a) => adjustment_steps(a, "layer.newAdjustmentLayer.", type_desc),
            None => Mapped::Unsupported { harmless: false },
        },
        Some("contentLayer") => match type_desc {
            Some(t) if t.class_id.is("solidColorLayer") => match get_obj(t, "Clr ").and_then(color_hex) {
                Some(c) => step("layer.newFillLayer.solidColor", json!({"color": c})),
                None => none("layer.newFillLayer.solidColor"),
            },
            _ => Mapped::Unsupported { harmless: false },
        },
        Some("layerSection") => {
            let mut p = Map::new();
            if let Some(n) = using.and_then(|u| get_text(u, "Nm  ")) {
                p.insert("name".into(), n.into());
            }
            let id = if get_ref(d, "From").is_some() { "layer.groupLayers" } else { "layer.new.group" };
            step(id, Value::Object(p))
        }
        _ => Mapped::Unsupported { harmless: false },
    }
}

fn make_to(id: &str, p: &Value) -> Option<Descriptor> {
    let with_name = |cls: &str| {
        let mut u = desc(cls);
        if let Some(n) = jstr(p, "name") {
            u = u.with("Nm  ", text(n));
        }
        u
    };
    Some(match id {
        "layer.new.layer" => {
            let mut u = with_name("Lyr ");
            if let Some(o) = jnum(p, "opacity") {
                u = u.with("Opct", pct(o * 100.0));
            }
            if let Some(m) = jstr(p, "mode").and_then(blend_ps) {
                u = u.with("Md  ", enm("BlnM", m));
            }
            desc("null").with("null", class_ref("Lyr ")).with("Usng", D::Descriptor(u))
        }
        "layer.new.group" => desc("null").with("null", class_ref("layerSection")).with("Usng", D::Descriptor(with_name("layerSection"))),
        "layer.groupLayers" => {
            desc("null").with("null", class_ref("layerSection")).with("From", target_ref("Lyr ")).with("Usng", D::Descriptor(with_name("layerSection")))
        }
        "layer.new.layerViaCopy" | "layer.new.layerViaCut" => {
            let mut d = desc("null").with("Nw  ", D::Descriptor(desc("Lyr "))).with("Usng", enm("ArSl", "Slct"));
            if id == "layer.new.layerViaCopy" {
                d = d.with("Cpy ", D::Boolean(true));
            }
            d
        }
        "layer.newFillLayer.solidColor" => {
            let mut t = desc("solidColorLayer");
            if let Some(c) = jstr(p, "color").and_then(color_desc) {
                t = t.with("Clr ", D::Descriptor(c));
            }
            desc("null").with("null", class_ref("contentLayer")).with("Usng", D::Descriptor(desc("contentLayer").with("Type", D::Descriptor(t))))
        }
        "file.newFromClipboard" => desc("null").with("Nw  ", D::Descriptor(desc("Dcmn").with("preset", text("Clipboard")))),
        "file.new" => {
            let mut n = desc("Dcmn");
            if let Some(name) = jstr(p, "name") {
                n = n.with("Nm  ", text(name));
            }
            if let Some(m) = jstr(p, "mode").and_then(|m| lookup_back(MODES, if m == "gray" { "grayscale" } else { m })) {
                n = n.with("Md  ", D::Class(class(m)));
            }
            n = n.with("Wdth", px(jnum(p, "width").unwrap_or(1920.0))).with("Hght", px(jnum(p, "height").unwrap_or(1080.0)));
            n = n.with("Rslt", D::UnitFloat { unit: *b"#Rsl", value: jnum(p, "resolution").unwrap_or(72.0) });
            let fill = match jstr(p, "background") {
                Some("transparent") => "Trns",
                Some("backgroundColor") => "BckC",
                _ => "Wht ",
            };
            n = n.with("Fl  ", enm("Fl  ", fill)).with("Dpth", long(jnum(p, "depth").unwrap_or(8.0)));
            desc("null").with("Nw  ", D::Descriptor(n))
        }
        _ => {
            let suffix = id.strip_prefix("layer.newAdjustmentLayer.")?;
            let a = adjustment_by_suffix(suffix)?;
            let t = if p.as_object().is_some_and(|o| o.is_empty()) {
                desc(a.class).with("presetKind", enm("presetKindType", "presetKindDefault"))
            } else {
                (a.to)(p)?
            };
            desc("null").with("null", class_ref("AdjL")).with("Usng", D::Descriptor(desc("AdjL").with("Type", D::Descriptor(t))))
        }
    })
}

fn set_from(d: &Descriptor) -> Mapped {
    let Some(r) = get_ref(d, "null") else { return Mapped::Unsupported { harmless: false } };
    let unsupported = Mapped::Unsupported { harmless: false };
    match (ref_class(r).as_deref(), target(r)) {
        (Some("Chnl"), Target::Property(p)) if p == "fsel" => match d.get("T   ") {
            Some(D::Enumerated { value, .. }) => match trimmed(value).as_str() {
                "Al" => none("select.all"),
                "None" => none("select.deselect"),
                "Prvs" => none("select.reselect"),
                _ => unsupported,
            },
            Some(D::Reference(t)) => match (ref_class(t).as_deref(), target(t)) {
                (Some("Chnl"), Target::Ordinal(o)) if o == "Trsp" => step("select.loadSelection", json!({"channel": "transparency"})),
                (Some("Chnl"), Target::Name(n)) => step("select.loadSelection", json!({"channel": n})),
                _ => unsupported,
            },
            Some(D::Descriptor(t)) if t.class_id.is("Rctn") || t.class_id.is("Elps") => {
                let (Some(top), Some(left), Some(bottom), Some(right)) = (get_num(t, "Top "), get_num(t, "Left"), get_num(t, "Btom"), get_num(t, "Rght"))
                else {
                    return unsupported;
                };
                let mut p = json!({"x": left.round(), "y": top.round(), "width": (right - left).round().max(0.0), "height": (bottom - top).round().max(0.0)});
                if t.class_id.is("Elps") {
                    p["ellipse"] = json!(true);
                }
                step("select.rect", p)
            }
            _ => unsupported,
        },
        (Some("Lyr"), _) => {
            let Some(mut p) = layer_target(r) else { return unsupported };
            let Some(t) = get_obj(d, "T   ") else { return unsupported };
            let mut v = Vec::new();
            if let Some(c) = get_enum(t, "Clr ").and_then(|c| lookup(LABEL_COLORS, &c)) {
                let mut lp = p.clone();
                lp.insert("color".into(), c.into());
                v.push(("layer.setLabelColor", Value::Object(lp)));
            }
            if let Some(n) = get_text(t, "Nm  ") {
                p.insert("name".into(), n.into());
            }
            if let Some(o) = get_num(t, "Opct") {
                p.insert("opacity".into(), (o / 100.0).into());
            }
            if let Some(f) = get_num(t, "fillOpacity") {
                p.insert("fill".into(), (f / 100.0).into());
            }
            if let Some(m) = get_enum(t, "Md  ") {
                let Some(label) = blend_label(&m) else { return unsupported };
                p.insert("blend".into(), label.into());
            }
            if let Some(vis) = get_bool(t, "Vsbl") {
                p.insert("visible".into(), vis.into());
            }
            if p.keys().any(|k| k != "layer") {
                v.push(("layer.setProps", Value::Object(p)));
            }
            if v.is_empty() { unsupported } else { steps(v) }
        }
        (Some("AdjL"), Target::Ordinal(o)) if o == "Trgt" => {
            match get_obj(d, "T   ").and_then(|t| adjustment_by_class(&trimmed(&t.class_id)).map(|a| (a, t))) {
                Some((a, t)) => match (a.from)(t) {
                    Some(p) => step("layer.setAdjustment", p),
                    None => unsupported,
                },
                None => unsupported,
            }
        }
        _ => unsupported,
    }
}

fn set_to(id: &str, p: &Value) -> Option<Descriptor> {
    let fsel = D::Reference(vec![ReferenceItem::Property { class: class("Chnl"), key: Id::new("fsel") }]);
    let sel = |t: D| desc("null").with("null", fsel.clone()).with("T   ", t);
    Some(match id {
        "select.all" => sel(enm("Ordn", "Al  ")),
        "select.deselect" => sel(enm("Ordn", "None")),
        "select.reselect" => sel(enm("Ordn", "Prvs")),
        "select.loadSelection" => match jstr(p, "channel")? {
            "transparency" => sel(D::Reference(vec![ReferenceItem::Enumerated { class: class("Chnl"), type_id: Id::new("Chnl"), value: Id::new("Trsp") }])),
            name => sel(name_ref("Chnl", name)),
        },
        "select.rect" => {
            let (x, y, w, h) = (jnum(p, "x")?, jnum(p, "y")?, jnum(p, "width")?, jnum(p, "height")?);
            let cls = if jbool(p, "ellipse") == Some(true) { "Elps" } else { "Rctn" };
            sel(D::Descriptor(desc(cls).with("Top ", px(y)).with("Left", px(x)).with("Btom", px(y + h)).with("Rght", px(x + w))))
        }
        "layer.setProps" | "layer.renameLayer" | "layer.setLabelColor" => {
            let mut t = desc("Lyr ");
            if let Some(n) = jstr(p, "name") {
                t = t.with("Nm  ", text(n));
            }
            if let Some(o) = jnum(p, "opacity") {
                t = t.with("Opct", pct(o * 100.0));
            }
            if let Some(f) = jnum(p, "fill") {
                t = t.with("fillOpacity", pct(f * 100.0));
            }
            if let Some(m) = jstr(p, "blend") {
                t = t.with("Md  ", enm("BlnM", blend_ps(m)?));
            }
            if let Some(v) = jbool(p, "visible") {
                t = t.with("Vsbl", D::Boolean(v));
            }
            if let Some(c) = jstr(p, "color") {
                t = t.with("Clr ", enm("Clr ", lookup_back(LABEL_COLORS, c)?));
            }
            if t.items.is_empty() {
                return None;
            }
            desc("null").with("null", layer_ref(p)).with("T   ", D::Descriptor(t))
        }
        "layer.setAdjustment" => {
            // The adjustment kind isn't in the params: Photoshop needs it, so only steps that
            // name one (`kind`) export.
            let a = adjustment_by_suffix(jstr(p, "kind")?)?;
            desc("null").with("null", target_ref("AdjL")).with("T   ", D::Descriptor((a.to)(p)?))
        }
        _ => return None,
    })
}

/// Tool classes Photoshop records when an action switches tools: no document effect.
fn is_tool_class(c: &str) -> bool {
    c.ends_with("Tl") || c.ends_with("Tool")
}

fn select_from(d: &Descriptor) -> Mapped {
    let Some(r) = get_ref(d, "null") else { return Mapped::Unsupported { harmless: false } };
    match (ref_class(r).as_deref(), target(r)) {
        (Some("Lyr"), Target::Name(n)) => {
            let mut p = json!({"layer": n});
            if get_enum(d, "selectionModifier").as_deref() == Some("addToSelection") {
                p["mode"] = json!("add");
            }
            step("layer.select", p)
        }
        (Some("Lyr"), Target::Ordinal(o)) => {
            // Shift+Alt+[ / ] add the next layer to the selection instead of replacing it.
            let add = get_enum(d, "selectionModifier").as_deref() == Some("addToSelection");
            match (o.as_str(), add) {
                ("Frwr", false) => none("layer.selectAbove"),
                ("Frwr", true) => none("layer.addAboveToSelection"),
                ("Bckw", false) => none("layer.selectBelow"),
                ("Bckw", true) => none("layer.addBelowToSelection"),
                ("Frnt", _) => none("layer.selectTop"),
                ("Back", _) => none("layer.selectBottom"),
                _ => Mapped::Unsupported { harmless: false },
            }
        }
        (Some(c), Target::Class) if is_tool_class(c) => Mapped::Unsupported { harmless: true },
        _ => Mapped::Unsupported { harmless: false },
    }
}

fn select_to(id: &str, p: &Value) -> Option<Descriptor> {
    let ord = |o: &str| D::Reference(vec![ReferenceItem::Enumerated { class: class("Lyr "), type_id: code("Ordn"), value: code(o) }]);
    Some(match id {
        "layer.select" => {
            let mut d = desc("null").with("null", name_ref("Lyr ", jstr(p, "layer")?));
            if jstr(p, "mode") == Some("add") {
                d = d.with("selectionModifier", enm("selectionModifierType", "addToSelection"));
            }
            d.with("MkVs", D::Boolean(false))
        }
        "layer.selectAbove" => desc("null").with("null", ord("Frwr")),
        "layer.selectBelow" => desc("null").with("null", ord("Bckw")),
        "layer.addAboveToSelection" => desc("null").with("null", ord("Frwr")).with("selectionModifier", enm("selectionModifierType", "addToSelection")),
        "layer.addBelowToSelection" => desc("null").with("null", ord("Bckw")).with("selectionModifier", enm("selectionModifierType", "addToSelection")),
        "layer.selectTop" => desc("null").with("null", ord("Frnt")),
        "layer.selectBottom" => desc("null").with("null", ord("Back")),
        _ => return None,
    })
}

fn delete_from(d: &Descriptor) -> Mapped {
    match get_ref(d, "null") {
        None => none("edit.clear"),
        Some(r) if ref_class(r).as_deref() == Some("Lyr") => match layer_target(r) {
            Some(p) => step("layer.delete", Value::Object(p)),
            None => Mapped::Unsupported { harmless: false },
        },
        Some(_) => Mapped::Unsupported { harmless: false },
    }
}

fn delete_to(id: &str, p: &Value) -> Option<Descriptor> {
    Some(match id {
        "edit.clear" => desc("null"),
        "layer.delete" => desc("null").with("null", layer_ref(p)),
        _ => return None,
    })
}

/// `hide` / `show` take a list of references (or one).
fn visibility_from(d: &Descriptor, id: &str) -> Mapped {
    let refs: Vec<&[ReferenceItem]> = match d.get("null") {
        Some(D::List(l)) => l.iter().filter_map(|v| if let D::Reference(r) = v { Some(r.as_slice()) } else { None }).collect(),
        Some(D::Reference(r)) => vec![r.as_slice()],
        _ => return Mapped::Unsupported { harmless: false },
    };
    let mut v = Vec::new();
    for r in refs {
        match layer_target(r) {
            Some(p) => v.push((id, Value::Object(p))),
            None => return Mapped::Unsupported { harmless: false },
        }
    }
    if v.is_empty() { Mapped::Unsupported { harmless: false } } else { steps(v) }
}

fn visibility_to(_: &str, p: &Value) -> Option<Descriptor> {
    Some(desc("null").with("null", D::List(vec![layer_ref(p)])))
}

fn move_from(d: &Descriptor) -> Mapped {
    let Some(r) = get_ref(d, "null") else { return Mapped::Unsupported { harmless: false } };
    if ref_class(r).as_deref() != Some("Lyr") {
        return Mapped::Unsupported { harmless: false };
    }
    let Some(mut p) = layer_target(r) else { return Mapped::Unsupported { harmless: false } };
    match d.get("T   ") {
        Some(D::Descriptor(t)) => {
            let in_px = |k: &str| matches!(t.get(k), Some(D::UnitFloat { unit: PX, .. }));
            let (Some(dx), Some(dy)) = (get_num(t, "Hrzn"), get_num(t, "Vrtc")) else { return Mapped::Unsupported { harmless: false } };
            if !in_px("Hrzn") || !in_px("Vrtc") {
                // Points at an unknown resolution.
                return Mapped::Unsupported { harmless: false };
            }
            p.insert("dx".into(), dx.round().into());
            p.insert("dy".into(), dy.round().into());
            step("layer.translate", Value::Object(p))
        }
        Some(D::Reference(t)) => {
            let id = match target(t) {
                Target::Ordinal(o) if o == "Frnt" => "layer.arrange.bringToFront",
                Target::Ordinal(o) if o == "Back" => "layer.arrange.sendToBack",
                Target::Ordinal(o) if o == "Nxt" => "layer.arrange.bringForward",
                Target::Ordinal(o) if o == "Prvs" => "layer.arrange.sendBackward",
                Target::Offset(1) => "layer.arrange.bringForward",
                Target::Offset(-1) => "layer.arrange.sendBackward",
                _ => return Mapped::Unsupported { harmless: false },
            };
            step(id, Value::Object(p))
        }
        _ => Mapped::Unsupported { harmless: false },
    }
}

fn move_to(id: &str, p: &Value) -> Option<Descriptor> {
    let ord = |o: &str| D::Reference(vec![ReferenceItem::Enumerated { class: class("Lyr "), type_id: code("Ordn"), value: code(o) }]);
    let t = match id {
        "layer.translate" => D::Descriptor(desc("Ofst").with("Hrzn", px(jnum(p, "dx").unwrap_or(0.0))).with("Vrtc", px(jnum(p, "dy").unwrap_or(0.0)))),
        "layer.arrange.bringToFront" => ord("Frnt"),
        "layer.arrange.sendToBack" => ord("Back"),
        "layer.arrange.bringForward" => ord("Nxt "),
        "layer.arrange.sendBackward" => ord("Prvs"),
        _ => return None,
    };
    Some(desc("null").with("null", layer_ref(p)).with("T   ", t))
}

fn duplicate_from(d: &Descriptor) -> Mapped {
    let Some(r) = get_ref(d, "null") else { return Mapped::Unsupported { harmless: false } };
    match ref_class(r).as_deref() {
        Some("Lyr") => {
            let Some(mut p) = layer_target(r) else { return Mapped::Unsupported { harmless: false } };
            if let Some(n) = get_text(d, "Nm  ") {
                p.insert("name".into(), n.into());
            }
            step("layer.duplicate", Value::Object(p))
        }
        Some("Dcmn") => {
            let mut p = Map::new();
            if let Some(n) = get_text(d, "Nm  ") {
                p.insert("name".into(), n.into());
            }
            if let Some(m) = get_bool(d, "Mrgd") {
                p.insert("mergedOnly".into(), m.into());
            }
            step("image.duplicate", Value::Object(p))
        }
        _ => Mapped::Unsupported { harmless: false },
    }
}

fn duplicate_to(id: &str, p: &Value) -> Option<Descriptor> {
    let mut d = match id {
        "layer.duplicate" => desc("null").with("null", layer_ref(p)),
        "image.duplicate" => {
            desc("null").with("null", D::Reference(vec![ReferenceItem::Enumerated { class: class("Dcmn"), type_id: Id::new("Ordn"), value: Id::new("Frst") }]))
        }
        _ => return None,
    };
    if let Some(n) = jstr(p, "name") {
        d = d.with("Nm  ", text(n));
    }
    if let Some(m) = jbool(p, "mergedOnly") {
        d = d.with("Mrgd", D::Boolean(m));
    }
    Some(d)
}

fn play_from(d: &Descriptor) -> Mapped {
    match get_ref(d, "null").map(target) {
        Some(Target::Name(n)) => step("actions.play", json!({"action": n})),
        _ => Mapped::Unsupported { harmless: false },
    }
}

fn play_to(_: &str, p: &Value) -> Option<Descriptor> {
    let name = jstr(p, "action")?;
    let mut items = vec![ReferenceItem::Name { class: class("Actn"), name: UnicodeString::new_nul(name) }];
    if let Some(set) = jstr(p, "set") {
        items.push(ReferenceItem::Name { class: class("ASet"), name: UnicodeString::new_nul(set) });
    }
    Some(desc("null").with("null", D::Reference(items)))
}

fn fill_from(d: &Descriptor) -> Mapped {
    if d.get("From").is_some() {
        // The Paint Bucket tool's click, not Edit › Fill.
        return Mapped::Unsupported { harmless: false };
    }
    let Some(contents) = get_enum(d, "Usng").and_then(|u| lookup(FILL_CONTENTS, &u)) else { return Mapped::Unsupported { harmless: false } };
    let mut p = json!({"contents": contents});
    if contents == "color" {
        match get_obj(d, "Clr ").and_then(color_hex) {
            Some(c) => p["color"] = json!(c),
            None => return Mapped::Unsupported { harmless: false },
        }
    }
    if let Some(o) = get_num(d, "Opct") {
        p["opacity"] = json!(o);
    }
    if let Some(m) = get_enum(d, "Md  ") {
        match blend_label(&m) {
            Some(l) => p["mode"] = json!(l),
            None => return Mapped::Unsupported { harmless: false },
        }
    }
    if let Some(t) = get_bool(d, "PrsT") {
        p["preserveTransparency"] = json!(t);
    }
    step("edit.fill", p)
}

fn fill_to(_: &str, p: &Value) -> Option<Descriptor> {
    let contents = jstr(p, "contents").unwrap_or("color");
    let mut d = desc("null").with("Usng", enm("FlCn", lookup_back(FILL_CONTENTS, contents)?));
    if contents == "color" {
        d = d.with("Clr ", D::Descriptor(color_desc(jstr(p, "color")?)?));
    }
    d = d.with("Opct", pct(jnum(p, "opacity").unwrap_or(100.0))).with("Md  ", enm("BlnM", blend_ps(jstr(p, "mode").unwrap_or("Normal"))?));
    if let Some(t) = jbool(p, "preserveTransparency") {
        d = d.with("PrsT", D::Boolean(t));
    }
    Some(d)
}

const STROKE: &[F] = &[
    F("Wdth", "width", Kind::Long),
    F("Lctn", "location", Kind::Enum("StrL", STROKE_LOCATION)),
    F("Opct", "opacity", Kind::Pct),
    F("Md  ", "mode", Kind::Blend),
    F("Clr ", "color", Kind::Color),
    F("PrsT", "preserveTransparency", Kind::Bool),
];

/// Whether an event's `null` reference targets a layer (Edit › Transform) rather than the
/// document (Image › Image Rotation).
fn targets_layer(d: &Descriptor) -> bool {
    get_ref(d, "null").and_then(ref_class).as_deref() == Some("Lyr")
}

fn rotate_from(d: &Descriptor) -> Mapped {
    let Some(angle) = get_num(d, "Angl") else { return Mapped::Unsupported { harmless: false } };
    let a = angle.rem_euclid(360.0);
    let (layer, doc) = if (a - 90.0).abs() < 1e-6 {
        ("edit.transform.rotate90Cw", "image.imageRotation.90cw")
    } else if (a - 270.0).abs() < 1e-6 {
        ("edit.transform.rotate90Ccw", "image.imageRotation.90ccw")
    } else if (a - 180.0).abs() < 1e-6 {
        ("edit.transform.rotate180", "image.imageRotation.180")
    } else if targets_layer(d) {
        return Mapped::Unsupported { harmless: false };
    } else {
        return step("image.rotation.arbitrary", json!({"angle": angle.abs(), "direction": if angle < 0.0 { "ccw" } else { "cw" }}));
    };
    none(if targets_layer(d) { layer } else { doc })
}

fn rotate_to(id: &str, p: &Value) -> Option<Descriptor> {
    let (angle, layer) = match id {
        "edit.transform.rotate90Cw" => (90.0, true),
        "edit.transform.rotate90Ccw" => (-90.0, true),
        "edit.transform.rotate180" => (180.0, true),
        "image.imageRotation.90cw" => (90.0, false),
        "image.imageRotation.90ccw" => (-90.0, false),
        "image.imageRotation.180" => (180.0, false),
        "image.rotation.arbitrary" => (jnum(p, "angle")? * if jstr(p, "direction") == Some("ccw") { -1.0 } else { 1.0 }, false),
        _ => return None,
    };
    let mut d = desc("null");
    if layer {
        d = d.with("null", target_ref("Lyr "));
    }
    Some(d.with("Angl", ang(angle)))
}

fn flip_from(d: &Descriptor) -> Mapped {
    let Some(axis) = get_enum(d, "Axis") else { return Mapped::Unsupported { harmless: false } };
    let horizontal = axis == "Hrzn";
    none(match (targets_layer(d), horizontal) {
        (true, true) => "edit.transform.flipHorizontal",
        (true, false) => "edit.transform.flipVertical",
        (false, true) => "image.imageRotation.flipCanvasHorizontal",
        (false, false) => "image.imageRotation.flipCanvasVertical",
    })
}

fn flip_to(id: &str, _: &Value) -> Option<Descriptor> {
    let (layer, axis) = match id {
        "edit.transform.flipHorizontal" => (true, "Hrzn"),
        "edit.transform.flipVertical" => (true, "Vrtc"),
        "image.imageRotation.flipCanvasHorizontal" => (false, "Hrzn"),
        "image.imageRotation.flipCanvasVertical" => (false, "Vrtc"),
        _ => return None,
    };
    let mut d = desc("null");
    if layer {
        d = d.with("null", target_ref("Lyr "));
    }
    Some(d.with("Axis", enm("Ornt", axis)))
}

fn convert_mode_from(d: &Descriptor) -> Mapped {
    if let Some(depth) = get_num(d, "Dpth") {
        return match depth as i64 {
            8 => none("image.mode.bits8"),
            16 => none("image.mode.bits16"),
            32 => none("image.mode.bits32"),
            _ => Mapped::Unsupported { harmless: false },
        };
    }
    match get_enum(d, "T   ").and_then(|t| lookup(MODES, &t)) {
        Some(m) => none(&format!("image.mode.{m}")),
        None => Mapped::Unsupported { harmless: false },
    }
}

fn convert_mode_to(id: &str, _: &Value) -> Option<Descriptor> {
    Some(match id {
        "image.mode.bits8" => desc("null").with("Dpth", long(8.0)),
        "image.mode.bits16" => desc("null").with("Dpth", long(16.0)),
        "image.mode.bits32" => desc("null").with("Dpth", long(32.0)),
        _ => desc("null").with("T   ", enm("ClrS", lookup_back(MODES, id.strip_prefix("image.mode.")?)?)),
    })
}

fn canvas_size_from(d: &Descriptor) -> Mapped {
    let mut p = Map::new();
    for (k, param) in [("Wdth", "width"), ("Hght", "height")] {
        match d.get(k) {
            Some(D::UnitFloat { unit: PX, value }) => {
                p.insert(param.into(), value.round().into());
            }
            Some(_) => return Mapped::Unsupported { harmless: false },
            None => {}
        }
    }
    if let Some(r) = get_bool(d, "Rltv") {
        p.insert("relative".into(), r.into());
    }
    let v = get_enum(d, "Vrtc").unwrap_or_else(|| "Cntr".into());
    let h = get_enum(d, "Hrzn").unwrap_or_else(|| "Cntr".into());
    let anchor = match (v.as_str(), h.as_str()) {
        ("Top", "Left") => "topLeft",
        ("Top", "Cntr") => "top",
        ("Top", "Rght") => "topRight",
        ("Cntr", "Left") => "left",
        ("Cntr", "Cntr") => "center",
        ("Cntr", "Rght") => "right",
        ("Bttm", "Left") => "bottomLeft",
        ("Bttm", "Cntr") => "bottom",
        ("Bttm", "Rght") => "bottomRight",
        _ => return Mapped::Unsupported { harmless: false },
    };
    p.insert("anchor".into(), anchor.into());
    if let Some(c) = get_enum(d, "canvasExtensionColorType") {
        match lookup(EXTENSION, &c) {
            Some(c) => {
                p.insert("extensionColor".into(), c.into());
            }
            None => match get_obj(d, "canvasExtensionColor").and_then(color_hex) {
                Some(hex) => {
                    p.insert("extensionColor".into(), hex.into());
                }
                None => return Mapped::Unsupported { harmless: false },
            },
        }
    }
    step("image.canvasSize", Value::Object(p))
}

fn canvas_size_to(_: &str, p: &Value) -> Option<Descriptor> {
    let anchor = jstr(p, "anchor").unwrap_or("center");
    let v = if anchor.starts_with("top") {
        "Top "
    } else if anchor.starts_with("bottom") {
        "Bttm"
    } else {
        "Cntr"
    };
    let h = if anchor.to_ascii_lowercase().ends_with("left") {
        "Left"
    } else if anchor.to_ascii_lowercase().ends_with("right") {
        "Rght"
    } else {
        "Cntr"
    };
    let mut d = desc("null");
    if let Some(r) = jbool(p, "relative") {
        d = d.with("Rltv", D::Boolean(r));
    }
    if let Some(w) = jnum(p, "width") {
        d = d.with("Wdth", px(w));
    }
    if let Some(hh) = jnum(p, "height") {
        d = d.with("Hght", px(hh));
    }
    d = d.with("Hrzn", enm("HrzL", h)).with("Vrtc", enm("VrtL", v));
    if let Some(c) = jstr(p, "extensionColor") {
        d = match lookup_back(EXTENSION, c) {
            Some(e) => d.with("canvasExtensionColorType", enm("canvasExtensionColorType", e)),
            None => d.with("canvasExtensionColorType", enm("canvasExtensionColorType", "Othr")).with("canvasExtensionColor", D::Descriptor(color_desc(c)?)),
        };
    }
    Some(d)
}

fn image_size_from(d: &Descriptor) -> Mapped {
    let mut p = Map::new();
    for (k, param) in [("Wdth", "width"), ("Hght", "height")] {
        match d.get(k) {
            Some(D::UnitFloat { unit: PX, value }) => {
                p.insert(param.into(), value.round().into());
            }
            // Percent or physical units need the document's size: not replayable headlessly.
            Some(_) => return Mapped::Unsupported { harmless: false },
            None => {}
        }
    }
    if let Some(r) = get_num(d, "Rslt") {
        p.insert("resolution".into(), r.into());
    }
    if let Some(i) = get_enum(d, "Intr") {
        match lookup(INTERPOLATION, &i) {
            Some(i) => {
                p.insert("resample".into(), i.into());
            }
            None => return Mapped::Unsupported { harmless: false },
        }
    } else if p.contains_key("resolution") && !p.contains_key("width") && !p.contains_key("height") {
        p.insert("resample".into(), "none".into());
    }
    if p.is_empty() {
        return Mapped::Unsupported { harmless: false };
    }
    step("image.imageSize", Value::Object(p))
}

fn image_size_to(_: &str, p: &Value) -> Option<Descriptor> {
    let mut d = desc("null");
    if let Some(w) = jnum(p, "width") {
        d = d.with("Wdth", px(w));
    }
    if let Some(h) = jnum(p, "height") {
        d = d.with("Hght", px(h));
    }
    if let Some(r) = jnum(p, "resolution") {
        d = d.with("Rslt", D::UnitFloat { unit: *b"#Rsl", value: r });
    }
    if let Some(i) = jstr(p, "resample").filter(|r| *r != "none") {
        d = d.with("Intr", enm("Intp", lookup_back(INTERPOLATION, i)?));
    }
    Some(d)
}

fn crop_from(d: &Descriptor) -> Mapped {
    let mut p = Map::new();
    if let Some(t) = get_obj(d, "To  ") {
        let (Some(top), Some(left), Some(bottom), Some(right)) = (get_num(t, "Top "), get_num(t, "Left"), get_num(t, "Btom"), get_num(t, "Rght")) else {
            return Mapped::Unsupported { harmless: false };
        };
        p.insert("x".into(), left.round().into());
        p.insert("y".into(), top.round().into());
        p.insert("width".into(), (right - left).round().into());
        p.insert("height".into(), (bottom - top).round().into());
    }
    if let Some(a) = get_num(d, "Angl") {
        p.insert("angle".into(), a.into());
    }
    if let Some(del) = get_bool(d, "Dlt ") {
        p.insert("deleteCroppedPixels".into(), del.into());
    }
    step("image.crop", Value::Object(p))
}

fn crop_to(_: &str, p: &Value) -> Option<Descriptor> {
    let mut d = desc("null");
    if let (Some(x), Some(y), Some(w), Some(h)) = (jnum(p, "x"), jnum(p, "y"), jnum(p, "width"), jnum(p, "height")) {
        d = d.with("To  ", D::Descriptor(desc("Rctn").with("Top ", px(y)).with("Left", px(x)).with("Btom", px(y + h)).with("Rght", px(x + w))));
    }
    if let Some(a) = jnum(p, "angle") {
        d = d.with("Angl", ang(a));
    }
    Some(d.with("Dlt ", D::Boolean(jbool(p, "deleteCroppedPixels").unwrap_or(true))))
}

fn stop_from(d: &Descriptor) -> Mapped {
    step(STOP_MESSAGE, json!({"message": get_text(d, "Msge").unwrap_or_default(), "allowContinue": get_bool(d, "Cntn").unwrap_or(false)}))
}

fn stop_to(_: &str, p: &Value) -> Option<Descriptor> {
    Some(desc("null").with("Msge", text(jstr(p, "message").unwrap_or(""))).with("Cntn", D::Boolean(jbool(p, "allowContinue").unwrap_or(false))))
}

fn paste_from(d: &Descriptor) -> Mapped {
    none(if get_bool(d, "inPlace") == Some(true) { "edit.pasteSpecial.pasteInPlace" } else { "edit.paste" })
}

fn paste_to(id: &str, _: &Value) -> Option<Descriptor> {
    let mut d = desc("null");
    if id == "edit.pasteSpecial.pasteInPlace" {
        d = d.with("inPlace", D::Boolean(true));
    }
    Some(d.with("AntA", enm("Annt", "Anno")))
}

fn merge_visible_from(d: &Descriptor) -> Mapped {
    none(if get_bool(d, "Dplc") == Some(true) { "layer.stampVisible" } else { "layer.mergeVisible" })
}

fn merge_visible_to(id: &str, _: &Value) -> Option<Descriptor> {
    Some(if id == "layer.stampVisible" { desc("null").with("Dplc", D::Boolean(true)) } else { desc("null") })
}

fn align_from(d: &Descriptor) -> Mapped {
    match get_enum(d, "Usng").and_then(|u| lookup(ALIGN, &u)) {
        Some(a) => step(&format!("layer.align.{a}"), if get_bool(d, "alignToCanvas") == Some(true) { json!({"to": "canvas"}) } else { json!({}) }),
        None => Mapped::Unsupported { harmless: false },
    }
}

fn align_to(id: &str, p: &Value) -> Option<Descriptor> {
    let a = lookup_back(ALIGN, id.strip_prefix("layer.align.")?)?;
    Some(desc("null").with("null", target_ref("Lyr ")).with("Usng", enm("ADSt", a)).with("alignToCanvas", D::Boolean(jstr(p, "to") == Some("canvas"))))
}

fn purge_from(d: &Descriptor) -> Mapped {
    match get_enum(d, "null").and_then(|u| lookup(PURGE, &u)) {
        Some(p) => none(&format!("edit.purge.{p}")),
        None => Mapped::Unsupported { harmless: true },
    }
}

fn purge_to(id: &str, _: &Value) -> Option<Descriptor> {
    Some(desc("null").with("null", enm("PrgI", lookup_back(PURGE, id.strip_prefix("edit.purge.")?)?)))
}

fn reset_from(d: &Descriptor) -> Mapped {
    match get_ref(d, "null").map(target) {
        Some(Target::Property(p)) if p == "Clrs" => none("tools.defaultColors"),
        _ => Mapped::Unsupported { harmless: true },
    }
}

fn reset_to(_: &str, _: &Value) -> Option<Descriptor> {
    Some(desc("null").with("null", D::Reference(vec![ReferenceItem::Property { class: class("Clr "), key: Id::new("Clrs") }])))
}

fn profile_from(d: &Descriptor, id: &str) -> Mapped {
    let name = get_text(d, "T   ").or_else(|| get_text(d, "profile"));
    let Some(profile) = name.as_deref().and_then(|n| lookup(PROFILES, n)) else { return Mapped::Unsupported { harmless: false } };
    let mut p = json!({"profile": profile});
    if id == "edit.convertToProfile" {
        if let Some(i) = get_enum(d, "Inte").and_then(|i| lookup(INTENT, &i)) {
            p["intent"] = json!(i);
        }
        if let Some(b) = get_bool(d, "MpBl") {
            p["bpc"] = json!(b);
        }
    }
    step(id, p)
}

fn profile_to(id: &str, p: &Value) -> Option<Descriptor> {
    let name = lookup_back(PROFILES, jstr(p, "profile")?)?;
    let mut d = desc("null").with("null", target_ref("Dcmn"));
    if id == "edit.convertToProfile" {
        d = d
            .with("T   ", text(name))
            .with("Inte", enm("Inte", lookup_back(INTENT, jstr(p, "intent").unwrap_or("relative"))?))
            .with("MpBl", D::Boolean(jbool(p, "bpc").unwrap_or(true)));
    } else {
        d = d.with("profile", text(name));
    }
    Some(d)
}

fn levels_event_from(d: &Descriptor) -> Mapped {
    if get_bool(d, "Auto") == Some(true) {
        return none("image.autoTone");
    }
    if get_bool(d, "AuCo") == Some(true) {
        return none("image.autoContrast");
    }
    match levels_from(d) {
        Some(p) => step("image.adjustments.levels", p),
        None => Mapped::Unsupported { harmless: false },
    }
}

fn levels_event_to(id: &str, p: &Value) -> Option<Descriptor> {
    Some(match id {
        "image.autoTone" => desc("null").with("Auto", D::Boolean(true)),
        "image.autoContrast" => desc("null").with("AuCo", D::Boolean(true)),
        _ => levels_to(p)?,
    })
}

/// A direct adjustment event (Image › Adjustments › …).
macro_rules! adjustment_event {
    ($code:literal, $class:literal, $name:literal, $label:literal) => {
        Event {
            code: $code,
            name: $name,
            label: $label,
            ids: &[concat!("image.adjustments.", $name)],
            from: |d| match adjustment_by_class($class) {
                Some(a) => adjustment_steps(a, "image.adjustments.", Some(d)),
                None => Mapped::Unsupported { harmless: false },
            },
            to: |_, p| adjustment_by_class($class).and_then(|a| (a.to)(p)),
        }
    };
}

/// Every Photoshop event PhotoCraft maps. The order matters only for the panel label.
const EVENTS: &[Event] = &[
    // Filters.
    ev!("GsnB", "gaussianBlur", "Gaussian Blur", "filter.blur.gaussianBlur", &[F("Rds ", "radius", Kind::Px)]),
    ev!(
        "UnsM",
        "unsharpMask",
        "Unsharp Mask",
        "filter.sharpen.unsharpMask",
        &[F("Amnt", "amount", Kind::Pct), F("Rds ", "radius", Kind::Px), F("Thsh", "threshold", Kind::Long)]
    ),
    ev!(
        "AdNs",
        "addNoise",
        "Add Noise",
        "filter.noise.addNoise",
        &[
            F("Dstr", "distribution", Kind::Enum("Dstr", NOISE)),
            F("Nose", "amount", Kind::Pct),
            F("Mnch", "monochromatic", Kind::Bool),
            F("FlRs", "seed", Kind::Long)
        ]
    ),
    ev!("Mdn ", "median", "Median", "filter.noise.median", &[F("Rds ", "radius", Kind::Px)]),
    ev!("DstS", "dustAndScratches", "Dust & Scratches", "filter.noise.dustAndScratches", &[F("Rds ", "radius", Kind::Px), F("Thsh", "threshold", Kind::Long)]),
    ev!("Dspc", "despeckle", "Despeckle", "filter.noise.despeckle", &[]),
    ev!("MtnB", "motionBlur", "Motion Blur", "filter.blur.motionBlur", &[F("Angl", "angle", Kind::Ang), F("Dstn", "distance", Kind::Px)]),
    ev!("BoxB", "boxblur", "Box Blur", "filter.blur.boxBlur", &[F("Rds ", "radius", Kind::Px)]),
    ev!("SrfB", "surfaceBlur", "Surface Blur", "filter.blur.surfaceBlur", &[F("Rds ", "radius", Kind::Px), F("Thsh", "threshold", Kind::Long)]),
    ev!(
        "SmrB",
        "smartBlur",
        "Smart Blur",
        "filter.blur.smartBlur",
        &[
            F("Rds ", "radius", Kind::Dbl),
            F("Thsh", "threshold", Kind::Dbl),
            F("SmBQ", "quality", Kind::Enum("SmBQ", SMART_BLUR_QUALITY)),
            F("SmBM", "mode", Kind::Enum("SmBM", SMART_BLUR_MODE))
        ]
    ),
    ev!(
        "RdlB",
        "radialBlur",
        "Radial Blur",
        "filter.blur.radialBlur",
        &[F("Amnt", "amount", Kind::Long), F("BlrM", "method", Kind::Enum("BlrM", RADIAL_METHOD)), F("BlrQ", "quality", Kind::Enum("BlrQ", RADIAL_QUALITY))]
    ),
    ev!("Blr ", "blur", "Blur", "filter.blur.blur", &[]),
    ev!("BlrM", "blurMore", "Blur More", "filter.blur.blurMore", &[]),
    ev!("Shrp", "sharpen", "Sharpen", "filter.sharpen.sharpen", &[]),
    ev!("ShrM", "sharpenMore", "Sharpen More", "filter.sharpen.sharpenMore", &[]),
    ev!("ShrE", "sharpenEdges", "Sharpen Edges", "filter.sharpen.sharpenEdges", &[]),
    ev!("HghP", "highPass", "High Pass", "filter.other.highPass", &[F("Rds ", "radius", Kind::Px)]),
    ev!("Mnm ", "minimum", "Minimum", "filter.other.minimum", &[F("Rds ", "radius", Kind::Px)]),
    ev!("Mxm ", "maximum", "Maximum", "filter.other.maximum", &[F("Rds ", "radius", Kind::Px)]),
    ev!(
        "Ofst",
        "offset",
        "Offset",
        "filter.other.offset",
        &[F("Hrzn", "horizontal", Kind::Long), F("Vrtc", "vertical", Kind::Long), F("Fl  ", "undefinedAreas", Kind::Enum("FlMd", OFFSET_FILL))]
    ),
    // Adjustments (Image › Adjustments).
    Event {
        code: "Lvls",
        name: "levels",
        label: "Levels",
        ids: &["image.adjustments.levels", "image.autoTone", "image.autoContrast"],
        from: levels_event_from,
        to: levels_event_to,
    },
    adjustment_event!("Crvs", "Crvs", "curves", "Curves"),
    adjustment_event!("HStr", "HStr", "hueSaturation", "Hue/Saturation"),
    Event {
        code: "BrgC",
        name: "brightnessEvent",
        label: "Brightness/Contrast",
        ids: &["image.adjustments.brightnessContrast"],
        from: |d| match adjustment_by_class("BrgC") {
            Some(a) => adjustment_steps(a, "image.adjustments.", Some(d)),
            None => Mapped::Unsupported { harmless: false },
        },
        to: |_, p| adjustment_by_class("BrgC").and_then(|a| (a.to)(p)),
    },
    adjustment_event!("Thrs", "Thrs", "threshold", "Threshold"),
    adjustment_event!("Pstr", "Pstr", "posterize", "Posterize"),
    adjustment_event!("Invr", "Invr", "invert", "Invert"),
    adjustment_event!("ClrB", "ClrB", "colorBalance", "Color Balance"),
    adjustment_event!("Vbrn", "Vbrn", "vibrance", "Vibrance"),
    adjustment_event!("Exps", "Exps", "exposure", "Exposure"),
    adjustment_event!("PhtF", "PhtF", "photoFilter", "Photo Filter"),
    Event {
        code: "BlnW",
        name: "blackAndWhite",
        label: "Black & White",
        ids: &["image.adjustments.blackWhite"],
        from: |d| match adjustment_by_class("BlnW") {
            Some(a) => adjustment_steps(a, "image.adjustments.", Some(d)),
            None => Mapped::Unsupported { harmless: false },
        },
        to: |_, p| adjustment_by_class("BlnW").and_then(|a| (a.to)(p)),
    },
    adjustment_event!("ChnM", "ChnM", "channelMixer", "Channel Mixer"),
    adjustment_event!("SlcC", "SlcC", "selectiveColor", "Selective Color"),
    ev!("Dstt", "desaturate", "Desaturate", "image.adjustments.desaturate", &[]),
    ev!("Eqlz", "equalize", "Equalize", "image.adjustments.equalize", &[]),
    ev!("", "autoColor", "Auto Color", "image.autoColor", &[]),
    // Image.
    Event { code: "ImgS", name: "imageSize", label: "Image Size", ids: &["image.imageSize"], from: image_size_from, to: image_size_to },
    Event { code: "CnvS", name: "canvasSize", label: "Canvas Size", ids: &["image.canvasSize"], from: canvas_size_from, to: canvas_size_to },
    Event {
        code: "Rtte",
        name: "rotateEventEnum",
        label: "Rotate",
        ids: &[
            "image.imageRotation.90cw",
            "image.imageRotation.90ccw",
            "image.imageRotation.180",
            "image.rotation.arbitrary",
            "edit.transform.rotate90Cw",
            "edit.transform.rotate90Ccw",
            "edit.transform.rotate180",
        ],
        from: rotate_from,
        to: rotate_to,
    },
    Event {
        code: "Flip",
        name: "flip",
        label: "Flip",
        ids: &[
            "image.imageRotation.flipCanvasHorizontal",
            "image.imageRotation.flipCanvasVertical",
            "edit.transform.flipHorizontal",
            "edit.transform.flipVertical",
        ],
        from: flip_from,
        to: flip_to,
    },
    Event { code: "Crop", name: "crop", label: "Crop", ids: &["image.crop"], from: crop_from, to: crop_to },
    ev!(
        "",
        "trim",
        "Trim",
        "image.trim",
        &[
            F("trimBasedOn", "basedOn", Kind::Enum("trimBasedOn", TRIM)),
            F("Top ", "top", Kind::Bool),
            F("Btom", "bottom", Kind::Bool),
            F("Left", "left", Kind::Bool),
            F("Rght", "right", Kind::Bool)
        ]
    ),
    ev!("RvlA", "revealAll", "Reveal All", "image.revealAll", &[]),
    Event {
        code: "CnvM",
        name: "convertMode",
        label: "Convert Mode",
        ids: &[
            "image.mode.rgb",
            "image.mode.grayscale",
            "image.mode.cmyk",
            "image.mode.lab",
            "image.mode.multichannel",
            "image.mode.bits8",
            "image.mode.bits16",
            "image.mode.bits32",
        ],
        from: convert_mode_from,
        to: convert_mode_to,
    },
    Event { code: "Dplc", name: "duplicate", label: "Duplicate", ids: &["layer.duplicate", "image.duplicate"], from: duplicate_from, to: duplicate_to },
    // Layers.
    Event {
        code: "Mk  ",
        name: "make",
        label: "Make",
        ids: &[
            "layer.new.layer",
            "layer.new.group",
            "layer.groupLayers",
            "layer.new.layerViaCopy",
            "layer.new.layerViaCut",
            "layer.newFillLayer.solidColor",
            "layer.newAdjustmentLayer.levels",
            "layer.newAdjustmentLayer.curves",
            "layer.newAdjustmentLayer.hueSaturation",
            "layer.newAdjustmentLayer.brightnessContrast",
            "layer.newAdjustmentLayer.threshold",
            "layer.newAdjustmentLayer.posterize",
            "layer.newAdjustmentLayer.invert",
            "layer.newAdjustmentLayer.colorBalance",
            "layer.newAdjustmentLayer.vibrance",
            "layer.newAdjustmentLayer.exposure",
            "layer.newAdjustmentLayer.photoFilter",
            "layer.newAdjustmentLayer.blackWhite",
            "layer.newAdjustmentLayer.channelMixer",
            "layer.newAdjustmentLayer.selectiveColor",
            "file.new",
            "file.newFromClipboard",
        ],
        from: make_from,
        to: make_to,
    },
    Event {
        code: "setd",
        name: "set",
        label: "Set",
        ids: &[
            "select.all",
            "select.deselect",
            "select.reselect",
            "select.loadSelection",
            "select.rect",
            "layer.setProps",
            "layer.renameLayer",
            "layer.setLabelColor",
            "layer.setAdjustment",
        ],
        from: set_from,
        to: set_to,
    },
    Event {
        code: "slct",
        name: "select",
        label: "Select",
        ids: &[
            "layer.select",
            "layer.selectAbove",
            "layer.selectBelow",
            "layer.addAboveToSelection",
            "layer.addBelowToSelection",
            "layer.selectTop",
            "layer.selectBottom",
        ],
        from: select_from,
        to: select_to,
    },
    Event { code: "Dlt ", name: "delete", label: "Delete", ids: &["layer.delete", "edit.clear"], from: delete_from, to: delete_to },
    Event { code: "Hd  ", name: "hide", label: "Hide", ids: &["layer.hideLayers"], from: |d| visibility_from(d, "layer.hideLayers"), to: visibility_to },
    Event { code: "Shw ", name: "show", label: "Show", ids: &["layer.showLayers"], from: |d| visibility_from(d, "layer.showLayers"), to: visibility_to },
    Event {
        code: "move",
        name: "move",
        label: "Move",
        ids: &["layer.translate", "layer.arrange.bringToFront", "layer.arrange.sendToBack", "layer.arrange.bringForward", "layer.arrange.sendBackward"],
        from: move_from,
        to: move_to,
    },
    ev!("Fltn", "flattenImage", "Flatten Image", "layer.flattenImage", &[]),
    Event {
        code: "MrgV",
        name: "mergeVisible",
        label: "Merge Visible",
        ids: &["layer.mergeVisible", "layer.stampVisible"],
        from: merge_visible_from,
        to: merge_visible_to,
    },
    ev!("Mrg2", "mergeLayersNew", "Merge Layers", "layer.mergeLayers", &[]),
    ev!("CpTL", "copyToLayer", "Layer Via Copy", "layer.new.layerViaCopy", &[]),
    ev!("CtTL", "cutToLayer", "Layer Via Cut", "layer.new.layerViaCut", &[]),
    ev!("", "rasterizeLayer", "Rasterize", "layer.rasterize.layer", &[]),
    ev!("", "selectAllLayers", "Select All Layers", "select.allLayers", &[]),
    ev!("", "selectNoLayers", "Deselect Layers", "select.deselectLayers", &[]),
    ev!("Dfrg", "defringe", "Defringe", "layer.matting.defringe", &[F("Wdth", "width", Kind::Px)]),
    Event {
        code: "Algn",
        name: "align",
        label: "Align",
        ids: &[
            "layer.align.leftEdges",
            "layer.align.horizontalCenters",
            "layer.align.rightEdges",
            "layer.align.topEdges",
            "layer.align.verticalCenters",
            "layer.align.bottomEdges",
        ],
        from: align_from,
        to: align_to,
    },
    // Selection.
    ev!("Invs", "inverse", "Inverse", "select.inverse", &[]),
    ev!("Fthr", "feather", "Feather", "select.modify.feather", &[F("Rds ", "radius", Kind::Px)]),
    ev!("Expn", "expand", "Expand", "select.modify.expand", &[F("By  ", "radius", Kind::Px)]),
    ev!("Cntc", "contract", "Contract", "select.modify.contract", &[F("By  ", "radius", Kind::Px)]),
    ev!("Smth", "smoothness", "Smooth", "select.modify.smooth", &[F("Rds ", "radius", Kind::Px)]),
    ev!("Brdr", "border", "Border", "select.modify.border", &[F("Wdth", "radius", Kind::Px)]),
    ev!("Grow", "grow", "Grow", "select.grow", &[F("Tlrn", "tolerance", Kind::Long)]),
    ev!("Smlr", "similar", "Similar", "select.similar", &[F("Tlrn", "tolerance", Kind::Long)]),
    // Edit.
    Event { code: "Fl  ", name: "fill", label: "Fill", ids: &["edit.fill"], from: fill_from, to: fill_to },
    ev!("Strk", "stroke", "Stroke", "edit.stroke", STROKE),
    ev!("copy", "copyEvent", "Copy", "edit.copy", &[]),
    ev!("CpyM", "copyMerged", "Copy Merged", "edit.copyMerged", &[]),
    ev!("cut ", "cut", "Cut", "edit.cut", &[]),
    Event { code: "past", name: "paste", label: "Paste", ids: &["edit.paste", "edit.pasteSpecial.pasteInPlace"], from: paste_from, to: paste_to },
    Event {
        code: "Prge",
        name: "purge",
        label: "Purge",
        ids: &["edit.purge.all", "edit.purge.clipboard", "edit.purge.histories", "edit.purge.undo"],
        from: purge_from,
        to: purge_to,
    },
    Event { code: "Rset", name: "reset", label: "Reset", ids: &["tools.defaultColors"], from: reset_from, to: reset_to },
    ev!("Exch", "exchange", "Exchange", "tools.swapColors", &[]),
    Event {
        code: "",
        name: "assignProfile",
        label: "Assign Profile",
        ids: &["edit.assignProfile"],
        from: |d| profile_from(d, "edit.assignProfile"),
        to: profile_to,
    },
    Event {
        code: "",
        name: "convertToProfile",
        label: "Convert to Profile",
        ids: &["edit.convertToProfile"],
        from: |d| profile_from(d, "edit.convertToProfile"),
        to: profile_to,
    },
    // File and actions.
    ev!("Cls ", "close", "Close", "file.close", &[]),
    ev!("save", "save", "Save", "file.save", &[]),
    ev!("Rvrt", "revert", "Revert", "file.revert", &[]),
    Event { code: "Ply ", name: "play", label: "Play", ids: &["actions.play"], from: play_from, to: play_to },
    Event { code: "Stop", name: "stop", label: "Stop", ids: &[STOP_MESSAGE], from: stop_from, to: stop_to },
];

fn event_for(item: &AtnEvent) -> Option<&'static Event> {
    let id = item.id();
    EVENTS.iter().find(|e| (!e.code.is_empty() && e.code.trim_end() == id.trim_end()) || e.name == id)
}

fn event_for_id(id: &str) -> Option<&'static Event> {
    EVENTS.iter().find(|e| e.ids.contains(&id))
}

// ------------------------------------------------------------------ import

/// A step with no PhotoCraft equivalent.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Unsupported {
    pub action: String,
    pub step: usize,
    pub event: String,
    pub name: String,
}

/// What a `.atn` file imports to.
#[derive(Clone, Debug, Default)]
pub struct Imported {
    pub set: String,
    pub actions: Vec<Action>,
    /// Function keys the file assigns, as `(action name, key)`.
    pub function_keys: Vec<(String, String)>,
    pub unsupported: Vec<Unsupported>,
    pub warnings: Vec<String>,
}

fn photoshop_step(item: &AtnItem) -> (String, Value) {
    let descriptor = item.descriptor.as_ref().map(|d| photocraft_paint::tile::b64_encode(&d.to_bytes()));
    (PHOTOSHOP_STEP.into(), json!({"event": item.event.id(), "name": item.dictionary, "descriptor": descriptor}))
}

/// Integral floats become integers (`2.0` → `2`), as the commands' own docs and recordings
/// use, so an imported step reads and compares like a recorded one.
fn normalise(v: &mut Value) {
    match v {
        Value::Number(n) => {
            if let Some(f) = n.as_f64()
                && f.fract() == 0.0
                && f.abs() < 9.0e15
                && n.as_i64().is_none()
            {
                *v = Value::from(f as i64);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(normalise),
        Value::Object(o) => o.values_mut().for_each(normalise),
        _ => {}
    }
}

fn convert_action(a: &AtnAction, set: &str, unsupported: &mut Vec<Unsupported>) -> Action {
    let name = a.name.to_string_lossy();
    let mut action = Action::new(name.clone(), Vec::new());
    action.set = Some(set.to_string());
    for (i, item) in a.items.iter().enumerate() {
        let mapped = match (event_for(&item.event), &item.descriptor) {
            (Some(e), Some(d)) => (e.from)(d),
            (Some(e), None) => (e.from)(&Descriptor::default()),
            (None, _) => Mapped::Unsupported { harmless: false },
        };
        let (new_steps, enabled) = match mapped {
            Mapped::Steps(s) => (s, item.enabled),
            Mapped::Unsupported { harmless } => {
                unsupported.push(Unsupported { action: name.clone(), step: i, event: item.event.id(), name: item.dictionary.clone() });
                (vec![photoshop_step(item)], item.enabled && !harmless)
            }
        };
        for mut s in new_steps {
            normalise(&mut s.1);
            let idx = action.steps.len();
            action.steps.push(s);
            if !enabled {
                action.disabled.insert(idx);
            }
            if item.with_dialog {
                action.dialogs.insert(idx);
            }
        }
    }
    action
}

/// Convert a `.atn` file. Fails only when the file is unreadable: every step imports, as a
/// command or as a kept Photoshop step.
pub fn import(bytes: &[u8]) -> std::result::Result<Imported, String> {
    let file = atn::parse(bytes).map_err(|e| e.to_string())?;
    let set = file.name.to_string_lossy();
    let set = if set.trim().is_empty() { DEFAULT_SET.to_string() } else { set };
    let mut out = Imported { set: set.clone(), warnings: file.warnings.clone(), ..Default::default() };
    for a in &file.actions {
        let action = convert_action(a, &set, &mut out.unsupported);
        if a.function_key > 0 && !a.shift && !a.command {
            out.function_keys.push((action.name.clone(), format!("F{}", a.function_key)));
        }
        out.actions.push(action);
    }
    Ok(out)
}

// ------------------------------------------------------------------ export

/// A step Save Actions could not express as a Photoshop event.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Skipped {
    pub action: String,
    pub step: usize,
    pub id: String,
}

fn export_item(id: &str, params: &Value, set: &str) -> Option<AtnItem> {
    if id == PHOTOSHOP_STEP {
        let event = jstr(params, "event")?;
        let event = match event.as_bytes() {
            [a, b, c, d] => AtnEvent::Code([*a, *b, *c, *d]),
            _ => AtnEvent::Name(event.to_string()),
        };
        let descriptor = match jstr(params, "descriptor") {
            Some(b64) => Some(Descriptor::from_bytes(&photocraft_paint::tile::b64_decode(b64)?).ok()?),
            None => None,
        };
        return Some(AtnItem { event, dictionary: jstr(params, "name").unwrap_or("").to_string(), descriptor, ..Default::default() });
    }
    let e = event_for_id(id)?;
    let mut params = params.clone();
    if id == "actions.play" && params.get("set").is_none() {
        params["set"] = json!(set);
    }
    let descriptor = (e.to)(id, &params)?;
    let event = if e.code.is_empty() { AtnEvent::Name(e.name.to_string()) } else { AtnEvent::Code(e.code.as_bytes().try_into().ok()?) };
    Some(AtnItem { event, dictionary: e.label.to_string(), descriptor: Some(descriptor), ..Default::default() })
}

/// The `.atn` bytes of `actions` as the set `set`. `function_keys` gives each action's key
/// (`F1`…`F15`). Steps with no Photoshop equivalent are an error unless `skip_unsupported`.
pub fn export(
    set: &str,
    actions: &[&Action],
    function_keys: &[(String, String)],
    skip_unsupported: bool,
) -> std::result::Result<(Vec<u8>, Vec<Skipped>), String> {
    let mut skipped = Vec::new();
    let mut file = AtnFile { name: UnicodeString::new_nul(set), expanded: true, actions: Vec::new(), warnings: Vec::new() };
    for a in actions {
        let mut out = AtnAction { name: UnicodeString::new_nul(&a.name), expanded: false, ..Default::default() };
        if let Some((_, key)) = function_keys.iter().find(|(n, _)| n == &a.name)
            && let Some(n) = key.strip_prefix('F').and_then(|n| n.parse::<u16>().ok())
            && (1..=15).contains(&n)
        {
            out.function_key = n;
        }
        for (i, (id, params)) in a.steps.iter().enumerate() {
            let Some(mut item) = export_item(id, params, set) else {
                if skip_unsupported {
                    skipped.push(Skipped { action: a.name.clone(), step: i, id: id.clone() });
                    continue;
                }
                return Err(format!("step {} of `{}` (`{id}`) has no Photoshop equivalent; pass \"skipUnsupported\": true to leave it out", i + 1, a.name));
            };
            item.enabled = a.step_enabled(i);
            item.with_dialog = a.dialogs.contains(&i);
            item.dialog_options = if item.with_dialog { 2 } else { 0 };
            out.items.push(item);
        }
        file.actions.push(out);
    }
    let bytes = atn::write(&file).map_err(|e| e.to_string())?;
    Ok((bytes, skipped))
}

// ------------------------------------------------------------------ commands

/// A name not used by any action in `taken`: `name`, `name 2`, `name 3`…
fn unique(name: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == name) {
        return name.to_string();
    }
    (2..).map(|i| format!("{name} {i}")).find(|n| !taken.contains(n)).unwrap_or_else(|| name.to_string())
}

fn load(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "actions.load";
    if s.actions.recording.is_some() || s.actions.playing > 0 {
        return Err(bad(cmd, "stop recording or playing before loading actions"));
    }
    let (bytes, _) = crate::preset_import_cmds::file_bytes(p, cmd, ".atn file")?;
    let mut imp = import(&bytes).map_err(|e| bad(cmd, e))?;
    match p.get("set") {
        None | Some(Value::Null) => {}
        Some(Value::String(n)) if !n.trim().is_empty() => {
            imp.set = n.trim().to_string();
            for a in &mut imp.actions {
                a.set = Some(imp.set.clone());
            }
        }
        Some(_) => return Err(bad(cmd, "`set` must be a non-empty string")),
    }
    let replace = match p.get("replace") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad(cmd, "`replace` must be true or false")),
    };
    if replace {
        s.actions.list.retain(|a| a.set.as_deref() != Some(imp.set.as_str()));
    }
    let mut taken: Vec<String> = s.actions.list.iter().map(|a| a.name.clone()).collect();
    let mut names = Vec::with_capacity(imp.actions.len());
    let mut renamed = Vec::new();
    let mut steps = 0usize;
    for mut a in imp.actions {
        let name = unique(&a.name, &taken);
        if name != a.name {
            renamed.push((a.name.clone(), name.clone()));
        }
        a.name = name.clone();
        taken.push(name.clone());
        steps = steps.saturating_add(a.steps.len());
        names.push(name);
        s.actions.list.push(a);
    }
    // A Play step calls its action by name: follow renames within the set.
    for (from, to) in &renamed {
        for a in s.actions.list.iter_mut().filter(|a| a.set.as_deref() == Some(imp.set.as_str())) {
            for (id, params) in &mut a.steps {
                if id == "actions.play" && params.get("action").and_then(Value::as_str) == Some(from) {
                    params["action"] = json!(to);
                }
            }
        }
    }
    let keys: Vec<(String, String)> =
        imp.function_keys.into_iter().map(|(n, k)| (renamed.iter().find(|(f, _)| f == &n).map_or(n, |(_, t)| t.clone()), k)).collect();
    if !keys.is_empty() {
        s.edit_prefs(|prefs| {
            for (name, key) in &keys {
                // A function key belongs to one action.
                prefs.shortcuts.retain(|k, v| !(k.starts_with(crate::actions_cmds::PLAY_SHORTCUT_PREFIX) && v == key));
                prefs.shortcuts.insert(format!("{}{name}", crate::actions_cmds::PLAY_SHORTCUT_PREFIX), key.clone());
            }
        });
    }
    s.actions.touch();
    Ok(json!({
        "set": imp.set,
        "actions": names,
        "steps": steps,
        "unsupported": imp.unsupported,
        "functionKeys": keys.iter().map(|(n, k)| json!({"action": n, "key": k})).collect::<Vec<_>>(),
        "warnings": imp.warnings,
    }))
}

fn save(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "actions.save";
    let skip = match p.get("skipUnsupported") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad(cmd, "`skipUnsupported` must be true or false")),
    };
    let set_param = match p.get("set") {
        None | Some(Value::Null) => None,
        Some(Value::String(n)) if !n.trim().is_empty() => Some(n.trim().to_string()),
        Some(_) => return Err(bad(cmd, "`set` must be a non-empty string")),
    };
    let (set, actions): (String, Vec<&Action>) = if p.get("action").is_some() {
        let idx = crate::actions_cmds::resolve(&s.actions.list, p, cmd)?;
        let a = &s.actions.list[idx];
        (set_param.or_else(|| a.set.clone()).unwrap_or_else(|| a.name.clone()), vec![a])
    } else if let Some(set) = set_param {
        let list: Vec<&Action> = s.actions.list.iter().filter(|a| a.set.as_deref() == Some(set.as_str())).collect();
        if list.is_empty() {
            return Err(bad(cmd, format!("no actions in a set named `{set}`")));
        }
        (set, list)
    } else {
        (DEFAULT_SET.to_string(), s.actions.list.iter().collect())
    };
    if actions.is_empty() {
        return Err(bad(cmd, "there are no actions to save"));
    }
    let names: Vec<String> = actions.iter().map(|a| a.name.clone()).collect();
    let keys: Vec<(String, String)> =
        s.prefs().shortcuts.iter().filter_map(|(k, v)| k.strip_prefix(crate::actions_cmds::PLAY_SHORTCUT_PREFIX).map(|n| (n.to_string(), v.clone()))).collect();
    let (bytes, skipped) = export(&set, &actions, &keys, skip).map_err(|e| bad(cmd, e))?;
    let steps: usize = actions.iter().map(|a| a.steps.len()).sum::<usize>().saturating_sub(skipped.len());
    let mut out = json!({"set": set, "actions": names, "steps": steps, "bytes": bytes.len(), "skipped": skipped});
    match p.get("path") {
        None | Some(Value::Null) => {
            out["data"] = json!(photocraft_paint::tile::b64_encode(&bytes));
        }
        Some(Value::String(path)) if !path.trim().is_empty() => {
            write_file(path, &bytes).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
            out["path"] = json!(path);
        }
        Some(_) => return Err(bad(cmd, "`path` must be a non-empty string")),
    }
    Ok(out)
}

#[cfg(not(target_arch = "wasm32"))]
fn write_file(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(target_arch = "wasm32")]
fn write_file(path: &str, _bytes: &[u8]) -> std::io::Result<()> {
    Err(std::io::Error::other(format!("cannot write paths on the web ({path}); omit `path` to get the bytes back as `data`")))
}

fn stop_message(_: &mut Session, p: &Value) -> Result<Value> {
    let message = jstr(p, "message").unwrap_or("Stop").to_string();
    if jbool(p, "allowContinue").unwrap_or(false) {
        Ok(json!({"message": message, "continued": true}))
    } else {
        Err(EngineError::Other(format!("stopped by the action: {message}")))
    }
}

fn photoshop_step_run(_: &mut Session, p: &Value) -> Result<Value> {
    let name = jstr(p, "name").filter(|n| !n.is_empty()).unwrap_or("this step");
    let event = jstr(p, "event").unwrap_or("?");
    Err(EngineError::Other(format!("Photoshop step \"{name}\" ({event}) has no PhotoCraft equivalent yet; switch it off in the Actions panel to skip it")))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "actions.load",
            label: "Load Actions…",
            menu: &[],
            shortcut: None,
            params: r##"{"path":".atn file"?,"data":base64 bytes?,"set":name? (override the file's set name),"replace":bool=true (replace the actions of a set of the same name)} → {set, actions:[names], steps, unsupported:[{action, step, event, name}], functionKeys:[{action, key}], warnings}. Photoshop steps PhotoCraft cannot play become actions.photoshopStep steps (kept for Save Actions; they fail at playback with the step's name); tool selections and other steps with no document effect are imported switched off. Layer references by name replay in any document."##,
            enabled: always,
            run: load,
            journal: false,
        },
        CommandSpec {
            id: "actions.save",
            label: "Save Actions…",
            menu: &[],
            shortcut: None,
            params: r##"{"path":".atn file"? (else the bytes come back as base64 `data`),"set":name? (that set's actions; default: every action, as the set "PhotoCraft Actions"),"action":name|index? (one action),"skipUnsupported":bool=false (leave out steps with no Photoshop equivalent instead of failing)} → {set, actions:[names], steps, bytes, skipped:[{action, step, id}], path|data}"##,
            enabled: always,
            run: save,
            journal: false,
        },
        CommandSpec {
            id: STOP_MESSAGE,
            label: "Stop",
            menu: &[],
            shortcut: None,
            params: r##"{"message":str,"allowContinue":bool=false} (Photoshop's Insert Stop: a step that shows the message; headless playback continues when allowContinue is set and otherwise stops the action with the message)"##,
            enabled: always,
            run: stop_message,
            journal: true,
        },
        CommandSpec {
            id: PHOTOSHOP_STEP,
            label: "Photoshop Step",
            menu: &[],
            shortcut: None,
            params: r##"{"event":id,"name":str,"descriptor":base64?} (a step from a .atn file PhotoCraft has no command for: fails at playback, written back by actions.save)"##,
            enabled: always,
            run: photoshop_step_run,
            journal: true,
        },
    ]
}

#[cfg(test)]
mod tests;
