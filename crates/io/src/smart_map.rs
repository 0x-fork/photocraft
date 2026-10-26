//! Smart objects in PSD: the placed-layer blocks (`SoLd`, `PlLd`) and the smart filter stack
//! (`filterFX` inside `SoLd`).
//!
//! Layouts follow the Adobe PSD spec ("Placed Layer", "Placed Layer Data") and Photoshop-authored
//! files (`corpus/photoshop/smart-filters`): a `soLD` descriptor holds the linked-file id (`Idnt`,
//! the uuid of the `lnk2` item), the instance id (`placed`, which keys the `FEid` filter cache),
//! the transform quad (`Trnf`), the source size (`Sz  `), the warp, and when filters are applied a
//! `filterFX` object:
//!
//! ```text
//! filterFX (filterFXStyle): enab, validAtPosition, filterMaskEnable, filterMaskLinked,
//!   filterMaskExtendWithWhite, filterFXList = [ filterFX: Nm, blendOptions {Opct, Md},
//!   enab, hasoptions, FrgC, BckC, Fltr (the filter's own descriptor), filterID ]
//! ```
//!
//! The list is in application order (first = bottom, applied first), like
//! [`SmartObject::smart_filters`](photocraft_doc::SmartObject). Filters PhotoCraft implements map
//! to their command id and dialog parameters; any other filter becomes a
//! [`UNSUPPORTED_FILTER`] entry that keeps its descriptor verbatim (hex in `params.psd`), so it is
//! listed, can be hidden, reordered or deleted, and is written back unchanged.
//!
//! Camera Raw and Displace keep their whole item as a template next to the editable settings
//! (Displace's carries the embedded displacement map, see `displace_map.rs`); so does Perspective
//! Warp (`perspective_warp_map.rs`), whose quads become the planes of its command. Hue/Saturation
//! (`HStr`) maps to its adjustment command in `hue_sat_filter_map.rs`.

use photocraft_color::{BlendMode, ColorMode, PixelFormat, SampleType};
use photocraft_doc::{LayerMask, SmartFilter};
use photocraft_geom::warp::{BezierMesh, Warp, WarpStyle};
use photocraft_geom::{Affine, Rect as GeomRect};
use photocraft_psd::Compression;
use photocraft_psd::Rect as PsdRect;
use photocraft_psd::descriptor::{Class, Descriptor, Id, ObjectArray, ReferenceItem, UnicodeString, Value, VersionedDescriptor};
use photocraft_psd::filter_effects::{EffectsPlane, FilterEffectsItem};
use photocraft_raster::Surface;
use serde_json::{Map, Value as J, json};

use crate::blocks::{enum_of, get_desc, num};

#[path = "camera_raw_map.rs"]
mod camera_raw;
#[path = "displace_map.rs"]
mod displace;
#[path = "hue_sat_filter_map.rs"]
mod hue_sat;
#[path = "perspective_warp_map.rs"]
mod perspective_warp;

pub use displace::{embed_map as displace_embed_map, embedded_map as displace_embedded_map, without_map as displace_without_map};

/// Command id of a Photoshop smart filter PhotoCraft does not implement. Its params hold the
/// filter's name, Photoshop filter id and descriptor (`psd`, hex); it renders as a pass-through.
pub const UNSUPPORTED_FILTER: &str = "psd.unsupportedFilter";

/// The smart filter stack of a placed layer, as stored in `filterFX`.
#[derive(Clone, Debug, PartialEq)]
pub struct FilterStack {
    /// `enab`: Layer › Smart Filter › Disable Smart Filters clears it.
    pub enabled: bool,
    pub filters: Vec<SmartFilter>,
    /// `filterMaskEnable`.
    pub mask_enabled: bool,
    /// `filterMaskLinked`.
    pub mask_linked: bool,
}

impl Default for FilterStack {
    fn default() -> Self {
        FilterStack { enabled: true, filters: Vec::new(), mask_enabled: true, mask_linked: false }
    }
}

/// What a `soLD` descriptor says about a smart object.
#[derive(Clone, Debug)]
pub struct Placed {
    pub descriptor: Descriptor,
    /// `Idnt`: uuid of the embedded or linked file (`lnk2` item).
    pub idnt: String,
    /// `placed`: id of this instance (keys the `FEid` filter cache).
    pub placed: String,
    /// Source pixels → document pixels (from `Trnf` and `Sz  `).
    pub transform: Affine,
    /// `filterFX`, if any filters were ever applied.
    pub stack: Option<FilterStack>,
}

fn text_of(d: &Descriptor, key: &str) -> Option<String> {
    match d.get(key)? {
        Value::Text(t) => Some(t.to_string_lossy()),
        _ => None,
    }
}

fn bool_or(d: &Descriptor, key: &str, default: bool) -> bool {
    match d.get(key) {
        Some(Value::Boolean(b)) => *b,
        _ => default,
    }
}

/// The `soLD` descriptor of `SoLd`/`SoLE` block data.
pub fn sold_descriptor(data: &[u8]) -> Option<Descriptor> {
    if data.get(..4)? != b"soLD" {
        return None;
    }
    crate::blocks::parse_prefix_versioned(data.get(8..)?)
}

/// Parses `SoLd`/`SoLE` block data (best effort: `None` when it is not a `soLD` structure).
pub fn parse_sold(data: &[u8]) -> Option<Placed> {
    let d = sold_descriptor(data)?;
    let (idnt, transform) = crate::blocks::parse_smart(b"SoLd", data);
    let placed = text_of(&d, "placed").unwrap_or_default();
    let stack = get_desc(&d, "filterFX").map(filter_stack);
    Some(Placed { descriptor: d, idnt, placed, transform, stack })
}

/// Reads a `filterFX` object.
pub fn filter_stack(fx: &Descriptor) -> FilterStack {
    let filters = match fx.get("filterFXList") {
        Some(Value::List(items)) => items
            .iter()
            .filter_map(|v| match v {
                Value::Descriptor(d) => Some(filter_from_item(d)),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    FilterStack {
        enabled: bool_or(fx, "enab", true),
        filters,
        mask_enabled: bool_or(fx, "filterMaskEnable", true),
        mask_linked: bool_or(fx, "filterMaskLinked", false),
    }
}

// ---------- the filters we model ----------

/// Photoshop filter → PhotoCraft command, keyed by the `Fltr` class (and `filterID`).
struct Known {
    class: &'static str,
    filter_id: i32,
    /// `Nm  ` as Photoshop writes it (the menu item, with its ellipsis and mnemonic).
    name: &'static str,
    command: &'static str,
}

/// A four-character PSD code as the big-endian integer Photoshop stores it as.
const fn code(c: &[u8; 4]) -> i32 {
    i32::from_be_bytes(*c)
}

const KNOWN: [Known; 53] = [
    Known { class: "GsnB", filter_id: code(b"GsnB"), name: "Gaussian Blur...", command: "filter.blur.gaussianBlur" },
    Known { class: "boxblur", filter_id: 840, name: "Box Blur...", command: "filter.blur.boxBlur" },
    Known { class: "MtnB", filter_id: code(b"MtnB"), name: "Motion Blur...", command: "filter.blur.motionBlur" },
    // Surface Blur, Smart Blur and Radial Blur: keys, value types and enums measured in Photoshop.
    Known { class: "surfaceBlur", filter_id: 1135, name: "Surface Blur...", command: "filter.blur.surfaceBlur" },
    Known { class: "SmrB", filter_id: code(b"SmrB"), name: "Smart Blur...", command: "filter.blur.smartBlur" },
    // Radial Blur at its defaults stores no `Fltr` (`hasoptions` false); that entry is kept verbatim.
    Known { class: "RdlB", filter_id: code(b"RdlB"), name: "Radial Blur...", command: "filter.blur.radialBlur" },
    Known { class: "UnsM", filter_id: code(b"UnsM"), name: "Unsharp Mask...", command: "filter.sharpen.unsharpMask" },
    Known { class: "AdNs", filter_id: code(b"AdNs"), name: "Add Noise...", command: "filter.noise.addNoise" },
    Known { class: "Mdn ", filter_id: code(b"Mdn "), name: "Median...", command: "filter.noise.median" },
    Known { class: "HghP", filter_id: code(b"HghP"), name: "High Pass...", command: "filter.other.highPass" },
    Known { class: "Mxm ", filter_id: code(b"Mxm "), name: "Maximum...", command: "filter.other.maximum" },
    Known { class: "Mnm ", filter_id: code(b"Mnm "), name: "Minimum...", command: "filter.other.minimum" },
    Known { class: "Ofst", filter_id: code(b"Ofst"), name: "Offset...", command: "filter.other.offset" },
    Known { class: "Cstm", filter_id: code(b"Cstm"), name: "Custom...", command: "filter.other.custom" },
    Known { class: "Msc ", filter_id: code(b"Msc "), name: "Mosaic...", command: "filter.pixelate.mosaic" },
    Known { class: "Mztn", filter_id: code(b"Mztn"), name: "Mezzotint...", command: "filter.pixelate.mezzotint" },
    Known { class: "ClrH", filter_id: code(b"ClrH"), name: "Color Halftone...", command: "filter.pixelate.colorHalftone" },
    Known { class: "Embs", filter_id: code(b"Embs"), name: "Emboss...", command: "filter.stylize.emboss" },
    Known { class: "Dfs ", filter_id: code(b"Dfs "), name: "Diffuse...", command: "filter.stylize.diffuse" },
    Known { class: "TrcC", filter_id: code(b"TrcC"), name: "Trace Contour...", command: "filter.stylize.traceContour" },
    Known { class: "Crst", filter_id: code(b"Crst"), name: "Crystallize...", command: "filter.pixelate.crystallize" },
    Known { class: "Pntl", filter_id: code(b"Pntl"), name: "Pointillize...", command: "filter.pixelate.pointillize" },
    Known { class: "Wnd ", filter_id: code(b"Wnd "), name: "Wind...", command: "filter.stylize.wind" },
    Known { class: "Extr", filter_id: code(b"Extr"), name: "Extrude...", command: "filter.stylize.extrude" },
    Known { class: "Tls ", filter_id: code(b"Tls "), name: "Tiles...", command: "filter.stylize.tiles" },
    Known { class: "Lvls", filter_id: code(b"Lvls"), name: "Levels...", command: "image.adjustments.levels" },
    Known { class: "Crvs", filter_id: code(b"Crvs"), name: "Curves...", command: "image.adjustments.curves" },
    // Shadows/Highlights at its defaults stores no `Fltr` (`hasoptions` false).
    Known { class: "adaptCorrect", filter_id: 781, name: "Shado&ws/Highlights...", command: "image.adjustments.shadowsHighlights" },
    // Filters without options (measured in Photoshop): `hasoptions` false, no `Fltr`, only the
    // four-char `filterID`. The class is that code; see `NO_OPTIONS`.
    Known { class: "Blr ", filter_id: code(b"Blr "), name: "Blur", command: "filter.blur.blur" },
    Known { class: "BlrM", filter_id: code(b"BlrM"), name: "Blur More", command: "filter.blur.blurMore" },
    Known { class: "Avrg", filter_id: code(b"Avrg"), name: "Average", command: "filter.blur.average" },
    Known { class: "Dspc", filter_id: code(b"Dspc"), name: "Despeckle", command: "filter.noise.despeckle" },
    Known { class: "FndE", filter_id: code(b"FndE"), name: "Find Edges", command: "filter.stylize.findEdges" },
    Known { class: "Fct ", filter_id: code(b"Fct "), name: "Facet", command: "filter.pixelate.facet" },
    Known { class: "Frgm", filter_id: code(b"Frgm"), name: "Fragment", command: "filter.pixelate.fragment" },
    Known { class: "Shrp", filter_id: code(b"Shrp"), name: "Sharpen", command: "filter.sharpen.sharpen" },
    Known { class: "ShrM", filter_id: code(b"ShrM"), name: "Sharpen More", command: "filter.sharpen.sharpenMore" },
    Known { class: "ShrE", filter_id: code(b"ShrE"), name: "Sharpen Edges", command: "filter.sharpen.sharpenEdges" },
    Known { class: "Slrz", filter_id: code(b"Slrz"), name: "Solarize", command: "filter.stylize.solarize" },
    // Smart Sharpen has a numeric filterID and a stringID class, like Box Blur.
    Known { class: SMART_SHARPEN, filter_id: 1131, name: "Smart Sharpen...", command: "filter.sharpen.smartSharpen" },
    Known { class: "DstS", filter_id: code(b"DstS"), name: "Dust & Scratches...", command: "filter.noise.dustAndScratches" },
    // Distort (Photoshop 27, `corpus`-style check: one smart object per filter saved from Photoshop
    // and read back). Shear isn't mapped yet: its curve lives on Photoshop's own grid.
    Known { class: "Pnch", filter_id: code(b"Pnch"), name: "Pinch...", command: "filter.distort.pinch" },
    Known { class: "Plr ", filter_id: code(b"Plr "), name: "Polar Coordinates...", command: "filter.distort.polarCoordinates" },
    Known { class: "Rple", filter_id: code(b"Rple"), name: "Ripple...", command: "filter.distort.ripple" },
    Known { class: "Sphr", filter_id: code(b"Sphr"), name: "Spherize...", command: "filter.distort.spherize" },
    Known { class: "Twrl", filter_id: code(b"Twrl"), name: "Twirl...", command: "filter.distort.twirl" },
    Known { class: "Wave", filter_id: code(b"Wave"), name: "Wave...", command: "filter.distort.wave" },
    Known { class: "ZgZg", filter_id: code(b"ZgZg"), name: "ZigZag...", command: "filter.distort.zigZag" },
    // Image › Adjustments › Invert on a smart object: no settings, so no `Fltr` (`hasoptions` false).
    Known { class: "Invr", filter_id: code(b"Invr"), name: "Invert", command: "image.adjustments.invert" },
    Known { class: "LnsF", filter_id: code(b"LnsF"), name: "Lens Flare...", command: "filter.render.lensFlare" },
    // These three also read the item's foreground and background colours (`coloured_params`).
    Known { class: "Fbrs", filter_id: code(b"Fbrs"), name: "Fibers...", command: "filter.render.fibers" },
    Known { class: "Clds", filter_id: code(b"Clds"), name: "Clouds", command: "filter.render.clouds" },
    Known { class: "DfrC", filter_id: code(b"DfrC"), name: "Difference Clouds", command: "filter.render.differenceClouds" },
];

/// Lens Flare's `Lns ` enum and our `lens` values. Photoshop's codes keep the old lens names:
/// Nikon 35 mm and 105 mm primes, a Panavision movie prime.
const LENSES: [(&[u8; 4], &str); 4] = [(b"Zm  ", "zoom"), (b"Nkn ", "prime35"), (b"Nkn1", "prime105"), (b"PnVs", "moviePrime")];

/// Classes of the filters that have no options: Photoshop stores them without a `Fltr`.
const NO_OPTIONS: [&str; 11] = ["Blr ", "BlrM", "Avrg", "Dspc", "FndE", "Fct ", "Frgm", "Shrp", "ShrM", "ShrE", "Slrz"];

// ---------- Smart Sharpen and Dust & Scratches ----------
//
// Measured in Photoshop (applied through Action Manager to a smart object, saved, `filterFX` read
// back):
//
// ```text
// Smart Sharpen (filterID 1131): smartSharpen { presetKind = presetKindType enum (only on the
//   default entry), useLegacy bool, Amnt UntF #Prc, Rds  UntF #Pxl, noiseReduction UntF #Prc,
//   blur = blurType enum (GsnB | lensBlur | MtnB), Angl long (Motion Blur only) }
// Dust & Scratches (filterID DstS): DstS { Rds  long, Thsh long }
// ```
//
// PhotoCraft's Smart Sharpen removes a Gaussian blur, non-legacy: Lens Blur, Motion Blur, legacy
// mode, Shadow/Highlight settings or any other key keep the whole entry verbatim. `presetKind`
// only names the dialog's preset: it is accepted on open but not kept (the engine takes no such
// parameter) and not written (Photoshop leaves it out of entries with explicit values).

const SMART_SHARPEN: &str = "smartSharpen";
const SMART_SHARPEN_KEYS: [&str; 6] = ["presetKind", "useLegacy", "Amnt", "Rds ", "noiseReduction", "blur"];

/// A `UntF` value in `unit`, finite and within `lo..=hi`.
fn unit_in(d: &Descriptor, key: &str, u: &[u8; 4], lo: f64, hi: f64) -> Option<f64> {
    match d.get(key)? {
        Value::UnitFloat { unit, value } if unit == u && value.is_finite() && (lo..=hi).contains(value) => Some(*value),
        _ => None,
    }
}

/// `filter.sharpen.smartSharpen` params; `None` for a setting PhotoCraft doesn't render.
fn smart_sharpen_params(f: &Descriptor) -> Option<J> {
    if f.items.iter().any(|(k, _)| !SMART_SHARPEN_KEYS.iter().any(|x| k.is(x))) {
        return None;
    }
    let Some(Value::Boolean(false)) = f.get("useLegacy") else { return None };
    let Some(Value::Enumerated { type_id, value }) = f.get("blur") else { return None };
    if !type_id.is("blurType") || !value.is("GsnB") {
        return None;
    }
    let mut out = Map::new();
    out.insert("amount".into(), json!(unit_in(f, "Amnt", b"#Prc", 1.0, 500.0)?));
    out.insert("radius".into(), json!(unit_in(f, "Rds ", b"#Pxl", 0.1, 64.0)?));
    out.insert("reduceNoise".into(), json!(unit_in(f, "noiseReduction", b"#Prc", 0.0, 100.0)?));
    match f.get("presetKind") {
        None => {}
        Some(Value::Enumerated { type_id, .. }) if type_id.is("presetKindType") => {}
        Some(_) => return None,
    }
    Some(J::Object(out))
}

/// Photoshop enum codes ↔ PhotoCraft parameter values, per filter setting.
const RIPPLE_SIZE: [(&[u8; 4], &str); 3] = [(b"Sml ", "small"), (b"Mdm ", "medium"), (b"Lrg ", "large")];
const SPHERIZE_MODE: [(&[u8; 4], &str); 3] = [(b"Nrml", "normal"), (b"HrzO", "horizontalOnly"), (b"VrtO", "verticalOnly")];
const POLAR_MODE: [(&[u8; 4], &str); 2] = [(b"RctP", "rectangularToPolar"), (b"PlrR", "polarToRectangular")];
const ZIGZAG_STYLE: [(&[u8; 4], &str); 3] = [(b"PndR", "pondRipples"), (b"OtFr", "outFromCenter"), (b"ArnC", "aroundCenter")];
const WAVE_TYPE: [(&[u8; 4], &str); 3] = [(b"WvSn", "sine"), (b"WvTr", "triangle"), (b"WvSq", "square")];
const UNDEFINED_AREAS: [(&[u8; 4], &str); 2] = [(b"WrpA", "wrap"), (b"RptE", "repeat")];

/// The PSD filter record that PhotoCraft's `command` is written as, if it has one.
fn known_by_command(command: &str) -> Option<&'static Known> {
    KNOWN.iter().find(|k| k.command == command)
}

/// RGB channel references in Levels/Curves (`Chnl` enum) and their parameter keys ("" = the
/// composite: top-level keys).
const CHANNELS: [(&[u8; 4], &str); 4] = [(b"Cmps", ""), (b"Rd  ", "red"), (b"Grn ", "green"), (b"Bl  ", "blue")];

/// Mezzotint's `MztT` enum codes and our `type` values (measured in Photoshop by rendering each).
const MEZZOTINT_TYPES: [(&[u8; 4], &str); 10] = [
    (b"FnDt", "fineDots"),
    (b"MdmD", "mediumDots"),
    (b"GrnD", "grainyDots"),
    (b"CrsD", "coarseDots"),
    (b"ShrL", "shortLines"),
    (b"MdmL", "mediumLines"),
    (b"LngL", "longLines"),
    (b"ShSt", "shortStrokes"),
    (b"MdmS", "mediumStrokes"),
    (b"LngS", "longStrokes"),
];

/// Color Halftone's `Rds ` (max radius) and `Ang1`..`Ang4` (screen angles) longs, read within
/// the dialog's ranges; anything else is kept verbatim.
fn color_halftone_params(f: &Descriptor) -> Option<J> {
    if !only_keys(f, &["Rds ", "Ang1", "Ang2", "Ang3", "Ang4"]) {
        return None;
    }
    let radius = int(f, "Rds ").filter(|r| (4..=127).contains(r))?;
    let angle = |k: &str| int(f, k).filter(|a| (-360..=360).contains(a));
    Some(json!({"maxRadius": radius, "channel1": angle("Ang1")?, "channel2": angle("Ang2")?, "channel3": angle("Ang3")?, "channel4": angle("Ang4")?}))
}

/// A numeric descriptor item as a finite number; `None` when it is absent, not a number, or not finite.
fn px(d: &Descriptor, key: &str) -> Option<f64> {
    num(d.get(key)).filter(|v| v.is_finite())
}

/// An integer descriptor item; `None` when it is absent or of another type.
fn int(d: &Descriptor, key: &str) -> Option<i32> {
    match d.get(key)? {
        Value::Integer(i) => Some(*i),
        _ => None,
    }
}

/// `v` when it lies in `lo..=hi` (the filter dialog's range); out-of-range settings are not
/// opened as editable filters, so a re-save keeps Photoshop's value instead of a clamped one.
fn in_range(v: f64, lo: f64, hi: f64) -> Option<f64> {
    (lo..=hi).contains(&v).then_some(v)
}

/// Integer `key` of `f` when it lies in `lo..=hi`.
fn int_in(f: &Descriptor, key: &str, lo: i32, hi: i32) -> Option<i32> {
    int(f, key).filter(|v| (lo..=hi).contains(v))
}

/// True when `f` holds only the given keys, each at most once. A filter with a setting we don't
/// know about (Radial Blur's off-centre centre, for one) or a repeated one is kept verbatim rather
/// than losing that setting.
fn only_keys(f: &Descriptor, keys: &[&str]) -> bool {
    f.items.iter().all(|(k, _)| keys.iter().any(|x| k.is(x))) && keys.iter().all(|key| f.items.iter().filter(|(k, _)| k.is(key)).count() <= 1)
}

/// Our name for enum `key`'s value in `f` (of enum type `type_id`), from `(PSD value, our name)`
/// pairs. An enum of another type is not one we know, so the filter stays verbatim.
fn enum_name(f: &Descriptor, key: &str, type_id: &str, table: &[(&[u8; 4], &'static str)]) -> Option<&'static str> {
    let Some(Value::Enumerated { type_id: t, value }) = f.get(key) else { return None };
    if t.as_bytes() != type_id.as_bytes() {
        return None;
    }
    table.iter().find(|(c, _)| value.as_bytes() == &c[..]).map(|(_, n)| *n)
}

/// True when `f` holds each of the given keys once and nothing else.
fn exact_keys(f: &Descriptor, keys: &[&str]) -> bool {
    only_keys(f, keys) && keys.iter().all(|x| f.items.iter().any(|(k, _)| k.is(x)))
}

/// Finite `key` of `f` stored as a plain double (the type Photoshop writes and we write back).
fn double(f: &Descriptor, key: &str) -> Option<f64> {
    match f.get(key)? {
        Value::Double(v) if v.is_finite() => Some(*v),
        _ => None,
    }
}

/// `p[key]` (`default` when absent) when it is a number in `lo..=hi`, the range the filter opens
/// with; anything else is an error, so a saved filter always opens as the same editable filter.
fn pf_range(p: &J, key: &str, default: f64, lo: f64, hi: f64) -> Result<f64, String> {
    match p.get(key) {
        None | Some(J::Null) => Ok(default),
        Some(v) => v.as_f64().filter(|x| (lo..=hi).contains(x)).ok_or_else(|| format!("setting `{key}` ({v}) is not a number in {lo}..={hi}")),
    }
}

/// Integer `p[key]` (rounded; `default` when absent) in `lo..=hi`, as a PSD long.
fn pi_range(p: &J, key: &str, default: f64, lo: i32, hi: i32) -> Result<Value, String> {
    let v = pf_range(p, key, default, f64::from(i32::MIN), f64::from(i32::MAX))?.round();
    let range = f64::from(lo)..=f64::from(hi);
    if !range.contains(&v) {
        return Err(format!("setting `{key}` ({v}) is not a number in {lo}..={hi}"));
    }
    Ok(Value::Integer(v as i32))
}

/// Enum `type_id` holding the PSD value of our name `p[key]` (`default` when absent).
fn enum_param(p: &J, key: &str, default: &str, type_id: &str, table: &[(&[u8; 4], &'static str)]) -> Result<Value, String> {
    let name = match p.get(key) {
        None | Some(J::Null) => default,
        Some(v) => v.as_str().ok_or_else(|| format!("setting `{key}` is not text"))?,
    };
    let code = table.iter().find(|(_, n)| *n == name).map(|(c, _)| **c).ok_or_else(|| format!("{key} `{name}` has no PSD equivalent"))?;
    Ok(Value::Enumerated { type_id: Id::new(type_id), value: Id::Code(code) })
}

/// Smart Blur `SmBQ` / `SmBM` and Radial Blur `BlrM` / `BlrQ` values (measured in Photoshop).
const SMART_BLUR_QUALITY: [(&[u8; 4], &str); 3] = [(b"SBQL", "low"), (b"SBQM", "medium"), (b"SBQH", "high")];
const SMART_BLUR_MODE: [(&[u8; 4], &str); 3] = [(b"SBMN", "normal"), (b"SBME", "edgeOnly"), (b"SBMO", "overlayEdge")];
const RADIAL_METHOD: [(&[u8; 4], &str); 2] = [(b"Spn ", "spin"), (b"Zm  ", "zoom")];
const RADIAL_QUALITY: [(&[u8; 4], &str); 3] = [(b"Drft", "draft"), (b"Gd  ", "good"), (b"Bst ", "best")];

fn channel_key(d: &Descriptor) -> Option<&'static str> {
    let Some(Value::Reference(items)) = d.get("Chnl") else { return None };
    let Some(ReferenceItem::Enumerated { value, .. }) = items.first() else { return None };
    CHANNELS.iter().find(|(c, _)| value.as_bytes() == &c[..]).map(|(_, k)| *k)
}

fn adjustment_list(d: &Descriptor) -> Option<Vec<&Descriptor>> {
    let Some(Value::List(items)) = d.get("Adjs") else { return None };
    items
        .iter()
        .map(|v| match v {
            Value::Descriptor(x) => Some(x),
            _ => None,
        })
        .collect()
}

fn levels_params(f: &Descriptor) -> Option<J> {
    let mut out = Map::new();
    for a in adjustment_list(f)? {
        let key = channel_key(a)?;
        let mut o = Map::new();
        let pair = |k: &str| match a.get(k) {
            Some(Value::List(v)) if v.len() == 2 => Some((int_value(&v[0])?, int_value(&v[1])?)),
            _ => None,
        };
        if let Some((lo, hi)) = pair("Inpt") {
            o.insert("inBlack".into(), json!(lo));
            o.insert("inWhite".into(), json!(hi));
        }
        if let Some(g) = px(a, "Gmm ") {
            o.insert("gamma".into(), json!(g));
        }
        if let Some((lo, hi)) = pair("Otpt") {
            o.insert("outBlack".into(), json!(lo));
            o.insert("outWhite".into(), json!(hi));
        }
        if key.is_empty() {
            out.extend(o);
        } else {
            out.insert(key.into(), J::Object(o));
        }
    }
    Some(J::Object(out))
}

fn int_value(v: &Value) -> Option<i32> {
    match v {
        Value::Integer(i) => Some(*i),
        _ => None,
    }
}

fn curves_params(f: &Descriptor) -> Option<J> {
    let mut out = Map::new();
    for a in adjustment_list(f)? {
        let key = channel_key(a)?;
        // A pencil-drawn curve is a mapping of 256 output levels (`Mpng`) instead of points.
        if let Some(Value::List(map)) = a.get("Mpng") {
            let map: Option<Vec<J>> = map.iter().map(|v| int_value(v).map(|o| json!(o.clamp(0, 255)))).collect();
            out.insert(if key.is_empty() { "points".into() } else { key.into() }, J::Array(map.filter(|m| m.len() == 256)?));
            out.insert("pencil".into(), J::Bool(true));
            continue;
        }
        let Some(Value::List(pts)) = a.get("Crv ") else { return None };
        let pts: Option<Vec<J>> = pts
            .iter()
            .map(|p| match p {
                Value::Descriptor(p) => Some(json!([px(p, "Hrzn")?, px(p, "Vrtc")?])),
                _ => None,
            })
            .collect();
        out.insert(if key.is_empty() { "points".into() } else { key.into() }, J::Array(pts?));
    }
    Some(J::Object(out))
}

/// Diffuse's `Md  ` (`DfsM` enum, measured in Photoshop) ↔ our `mode`.
const DIFFUSE_MODES: [(&str, &str); 4] = [("Nrml", "normal"), ("DrkO", "darkenOnly"), ("LghO", "lightenOnly"), ("anisotropic", "anisotropic")];

/// Trace Contour's `Edg ` (`CntE` enum, measured in Photoshop) ↔ our `edge`.
const TRACE_EDGES: [(&str, &str); 2] = [("Upr ", "upper"), ("Lwr ", "lower")];

/// Our name for the enum value of `key` (of type `type_id`) through `table` (Photoshop's value, ours).
fn enum_from(d: &Descriptor, key: &str, type_id: &str, table: &[(&str, &'static str)]) -> Option<&'static str> {
    let Some(Value::Enumerated { type_id: t, value }) = d.get(key) else { return None };
    if t.as_bytes() != type_id.as_bytes() {
        return None;
    }
    table.iter().find(|(psd, _)| value.as_bytes() == psd.as_bytes()).map(|(_, ours)| *ours)
}

// Wind, Extrude and Tiles, measured in Photoshop (each code changes the rendered result).

/// Wind's `WndM` (enum `WndM`) ↔ our `method`.
const WIND_METHODS: [(&str, &str); 3] = [("Wnd ", "wind"), ("Blst", "blast"), ("Stgr", "stagger")];
/// Wind's `Drct` (enum `Drct`) ↔ our `direction`. Photoshop's `Left` streaks extend to the left
/// of an edge, which is our wind from the right.
const WIND_DIRECTIONS: [(&str, &str); 2] = [("Left", "fromRight"), ("Rght", "fromLeft")];
/// Extrude's `ExtT` (enum `ExtT`) ↔ our `type`.
const EXTRUDE_TYPES: [(&str, &str); 2] = [("Blks", "blocks"), ("Pyrm", "pyramids")];
/// Extrude's `ExtR` (enum `ExtR`) ↔ our `depthMode`.
const EXTRUDE_DEPTHS: [(&str, &str); 2] = [("Rndm", "random"), ("LvlB", "levelBased")];
/// Tiles' `FlCl` (enum `FlCl`) ↔ our `fill`.
const TILE_FILLS: [(&str, &str); 4] = [("FlBc", "background"), ("FlFr", "foreground"), ("FlIn", "inverse"), ("FlSm", "unaltered")];
/// Extrude's size and depth (`ExtS`, `ExtD`) and Tiles' count and offset (`TlNm`, `TlOf`), all
/// `long`s, in the engine's (and Photoshop's dialog) ranges.
const EXTRUDE_SIZE: (i32, i32) = (2, 255);
const EXTRUDE_DEPTH: (i32, i32) = (1, 255);
const TILE_COUNT: (i32, i32) = (1, 99);
const TILE_OFFSET: (i32, i32) = (1, 99);

/// The `bool` at `key`.
fn bool_at(d: &Descriptor, key: &str) -> Option<bool> {
    match d.get(key)? {
        Value::Boolean(b) => Some(*b),
        _ => None,
    }
}

/// Tiles' params; background and foreground fills use the colours the entry carries (`FrgC`,
/// `BckC`). `None` when a setting or a colour that is there can't be read.
fn tiles_params(f: &Descriptor, item: &Descriptor) -> Option<J> {
    let mut out = Map::new();
    out.insert("count".into(), json!(int_in(f, "TlNm", TILE_COUNT.0, TILE_COUNT.1)?));
    out.insert("maxOffset".into(), json!(int_in(f, "TlOf", TILE_OFFSET.0, TILE_OFFSET.1)?));
    out.insert("fill".into(), json!(enum_from(f, "FlCl", "FlCl", &TILE_FILLS)?));
    for (psd, ours) in [("FrgC", "foreground"), ("BckC", "background")] {
        if item.get(psd).is_some() {
            out.insert(ours.into(), get_desc(item, psd).and_then(rgbc_colour)?);
        }
    }
    Some(J::Object(out))
}

/// Offset › Undefined Areas: the `FlMd` enum (`Fl  `) and our `undefinedAreas`. On a layer
/// Photoshop labels `Bckg` "Set to Transparent"; a smart object is always a layer.
const OFFSET_FILL: [(&str, &str); 3] = [("Wrp ", "wrap"), ("Rpt ", "repeat"), ("Bckg", "transparent")];

/// Custom's ranges in Photoshop's dialog: matrix cells, Scale, Offset.
const CUSTOM_CELL: (i32, i32) = (-999, 999);
const CUSTOM_SCALE: (i32, i32) = (1, 9999);
const CUSTOM_OFFSET: (i32, i32) = (-9999, 9999);

/// `v` when it lies in `lo..=hi`.
fn long_in_range(v: Option<i32>, (lo, hi): (i32, i32)) -> Option<i32> {
    v.filter(|v| (lo..=hi).contains(v))
}

/// `filter.other.custom` params for a `Cstm` filter: `Mtrx` is 25 longs, row-major 5 × 5. Values
/// outside Photoshop's ranges are not modelled (the filter stays verbatim).
fn custom_params(f: &Descriptor) -> Option<J> {
    if !only_keys(f, &["Mtrx", "Scl ", "Ofst"]) {
        return None;
    }
    let Some(Value::List(m)) = f.get("Mtrx") else { return None };
    if m.len() != 25 {
        return None;
    }
    let kernel: Option<Vec<J>> = m.iter().map(|v| long_in_range(int_value(v), CUSTOM_CELL).map(|v| json!(v))).collect();
    let scale = long_in_range(int(f, "Scl "), CUSTOM_SCALE)?;
    let offset = long_in_range(int(f, "Ofst"), CUSTOM_OFFSET)?;
    Some(json!({"kernel": kernel?, "scale": scale, "offset": offset}))
}

/// The settings of Photoshop's Wave, as it writes them (and we write them back).
const WAVE_KEYS: [&str; 10] = ["Wvtp", "NmbG", "WLMn", "WLMx", "AmMn", "AmMx", "SclH", "SclV", "UndA", "RndS"];

/// Our command and parameters for a known Photoshop filter; `None` when the filter (or one of its
/// settings) is not modelled. `item` is the whole `filterFX` entry (Pointillize reads its `BckC`,
/// Tiles its colours).
fn known_params(class: &str, f: Option<&Descriptor>, item: &Descriptor) -> Option<(&'static str, J)> {
    let k = KNOWN.iter().find(|k| k.class == class)?;
    let params = match (class, f) {
        ("adaptCorrect" | "Invr", None) => json!({}),
        (c, None) if NO_OPTIONS.contains(&c) => json!({}),
        (_, None) => return None,
        // Exactly the settings Photoshop writes (and we write back); an extra or repeated one keeps
        // the filter verbatim rather than lose it on save.
        ("Pnch", Some(f)) if exact_keys(f, &["Amnt"]) => json!({"amount": int(f, "Amnt")?}),
        ("Twrl", Some(f)) if exact_keys(f, &["Angl"]) => json!({"angle": int(f, "Angl")?}),
        ("Rple", Some(f)) if exact_keys(f, &["Amnt", "RplS"]) => json!({"amount": int(f, "Amnt")?, "size": enum_name(f, "RplS", "RplS", &RIPPLE_SIZE)?}),
        ("Sphr", Some(f)) if exact_keys(f, &["Amnt", "SphM"]) => json!({"amount": int(f, "Amnt")?, "mode": enum_name(f, "SphM", "SphM", &SPHERIZE_MODE)?}),
        ("Plr ", Some(f)) if exact_keys(f, &["Cnvr"]) => json!({"mode": enum_name(f, "Cnvr", "Cnvr", &POLAR_MODE)?}),
        ("ZgZg", Some(f)) if exact_keys(f, &["Amnt", "NmbR", "ZZTy"]) => {
            json!({"amount": int(f, "Amnt")?, "ridges": int(f, "NmbR")?, "style": enum_name(f, "ZZTy", "ZZTy", &ZIGZAG_STYLE)?})
        }
        ("Wave", Some(f)) if exact_keys(f, &WAVE_KEYS) => {
            // PhotoCraft's Wave has no Scale: a scaled wave stays verbatim rather than lose it. The
            // same for more generators than the renderer models, which would draw fewer than
            // Photoshop did (and than the file asks for, once its cached pixels are re-rendered).
            let generators = int(f, "NmbG")?;
            let modelled = u32::try_from(generators).is_ok_and(|g| (1..=photocraft_algo::MAX_WAVE_GENERATORS).contains(&g));
            if int(f, "SclH")? != 100 || int(f, "SclV")? != 100 || !modelled {
                return None;
            }
            json!({
                "type": enum_name(f, "Wvtp", "Wvtp", &WAVE_TYPE)?,
                "generators": generators,
                "wavelengthMin": int(f, "WLMn")?,
                "wavelengthMax": int(f, "WLMx")?,
                "amplitudeMin": int(f, "AmMn")?,
                "amplitudeMax": int(f, "AmMx")?,
                "undefinedAreas": enum_name(f, "UndA", "UndA", &UNDEFINED_AREAS)?,
                // `RndS` is a signed long; the seed is its bit pattern.
                "seed": (int(f, "RndS")? as u32),
            })
        }
        ("GsnB" | "boxblur" | "Mdn " | "HghP" | "Mxm " | "Mnm ", Some(f)) => json!({"radius": px(f, "Rds ")?}),
        ("MtnB", Some(f)) => json!({"angle": int(f, "Angl")?, "distance": px(f, "Dstn")?}),
        ("UnsM", Some(f)) => json!({"amount": px(f, "Amnt")?, "radius": px(f, "Rds ")?, "threshold": int(f, "Thsh")?}),
        ("AdNs", Some(f)) => {
            let dist = match enum_of(f, "Dstr")? {
                b"Unfr" => "uniform",
                b"Gsn " => "gaussian",
                _ => return None,
            };
            let mono = match f.get("Mnch") {
                Some(Value::Boolean(b)) => *b,
                _ => return None,
            };
            json!({"amount": px(f, "Nose")?, "distribution": dist, "monochromatic": mono, "seed": int(f, "FlRs").unwrap_or(0)})
        }
        ("surfaceBlur", Some(f)) if only_keys(f, &["Rds ", "Thsh"]) => json!({
            "radius": unit_in(f, "Rds ", b"#Pxl", 1.0, 100.0)?,
            "threshold": int_in(f, "Thsh", 2, 255)?,
        }),
        ("SmrB", Some(f)) if only_keys(f, &["Rds ", "Thsh", "SmBQ", "SmBM"]) => json!({
            "radius": in_range(double(f, "Rds ")?, 0.1, 100.0)?,
            "threshold": in_range(double(f, "Thsh")?, 0.1, 100.0)?,
            "quality": enum_name(f, "SmBQ", "SmBQ", &SMART_BLUR_QUALITY)?,
            "mode": enum_name(f, "SmBM", "SmBM", &SMART_BLUR_MODE)?,
        }),
        ("RdlB", Some(f)) if only_keys(f, &["Amnt", "BlrM", "BlrQ"]) => json!({
            "amount": int_in(f, "Amnt", 1, 100)?,
            "method": enum_name(f, "BlrM", "BlrM", &RADIAL_METHOD)?,
            "quality": enum_name(f, "BlrQ", "BlrQ", &RADIAL_QUALITY)?,
        }),
        ("Msc ", Some(f)) => json!({"cellSize": px(f, "ClSz")?}),
        ("Mztn", Some(f)) if only_keys(f, &["MztT"]) => json!({"type": enum_name(f, "MztT", "MztT", &MEZZOTINT_TYPES)?}),
        ("ClrH", Some(f)) => color_halftone_params(f)?,
        ("Embs", Some(f)) => json!({"angle": int(f, "Angl")?, "height": int(f, "Hght")?, "amount": int(f, "Amnt")?}),
        (SMART_SHARPEN, Some(f)) => smart_sharpen_params(f)?,
        ("DstS", Some(f)) => {
            if f.items.iter().any(|(k, _)| !k.is("Rds ") && !k.is("Thsh")) {
                return None;
            }
            json!({"radius": int_in(f, "Rds ", 1, 500)?, "threshold": int_in(f, "Thsh", 0, 255)?})
        }
        ("Dfs ", Some(f)) => json!({"mode": enum_from(f, "Md  ", "DfsM", &DIFFUSE_MODES)?}),
        ("TrcC", Some(f)) => json!({"level": int_in(f, "Lvl ", 0, 255)?, "edge": enum_from(f, "Edg ", "CntE", &TRACE_EDGES)?}),
        ("Crst", Some(f)) => json!({"cellSize": int_in(f, "ClSz", CELL_SIZE.0, CELL_SIZE.1)?}),
        ("Pntl", Some(f)) => {
            let mut p = json!({"cellSize": int_in(f, "ClSz", CELL_SIZE.0, CELL_SIZE.1)?});
            // Pointillize paints the gaps in the background colour the entry carries.
            if item.get("BckC").is_some() {
                p["background"] = get_desc(item, "BckC").and_then(rgbc_colour)?;
            }
            p
        }
        ("Wnd ", Some(f)) => json!({
            "method": enum_from(f, "WndM", "WndM", &WIND_METHODS)?,
            "direction": enum_from(f, "Drct", "Drct", &WIND_DIRECTIONS)?,
        }),
        ("Extr", Some(f)) => json!({
            "type": enum_from(f, "ExtT", "ExtT", &EXTRUDE_TYPES)?,
            "size": int_in(f, "ExtS", EXTRUDE_SIZE.0, EXTRUDE_SIZE.1)?,
            "depth": int_in(f, "ExtD", EXTRUDE_DEPTH.0, EXTRUDE_DEPTH.1)?,
            "depthMode": enum_from(f, "ExtR", "ExtR", &EXTRUDE_DEPTHS)?,
            "solidFrontFaces": bool_at(f, "ExtF")?,
            "maskIncompleteBlocks": bool_at(f, "ExtM")?,
        }),
        ("Tls ", Some(f)) => tiles_params(f, item)?,
        ("Ofst", Some(f)) if only_keys(f, &["Hrzn", "Vrtc", "Fl  "]) => {
            json!({"horizontal": int(f, "Hrzn")?, "vertical": int(f, "Vrtc")?, "undefinedAreas": enum_from(f, "Fl  ", "FlMd", &OFFSET_FILL)?})
        }
        ("Cstm", Some(f)) => custom_params(f)?,
        // Only the settings and value types Photoshop writes (and we write back); anything else
        // keeps the filter verbatim.
        ("LnsF", Some(f)) if exact_keys(f, &["Brgh", "FlrC", "Lns "]) => {
            let centre = get_desc(f, "FlrC").filter(|c| c.class_id.is("Pnt ") && exact_keys(c, &["Hrzn", "Vrtc"]))?;
            let lens = enum_name(f, "Lns ", "Lns ", &LENSES)?;
            json!({"brightness": int(f, "Brgh")?, "centerX": double(centre, "Hrzn")?, "centerY": double(centre, "Vrtc")?, "lens": lens})
        }
        ("Lvls", Some(f)) => levels_params(f)?,
        ("Crvs", Some(f)) => curves_params(f)?,
        _ => return None,
    };
    Some((k.command, params))
}

// ---------- Filter Gallery ----------
//
// Filter › Filter Gallery stores its effect stack in a `GEfc` filter (filterID `GEfc`). Photoshop
// writes a single effect inline; Photopea writes a list (also for one effect):
//
// ```text
// Fltr = GEfc { GEfk = GEft enum (the effect), <settings as long> }                          (Photoshop)
// Fltr = GEfc { GEfs = [ GEfc { <settings as long>, GEfk = GEft enum, GELv = visible } ] }   (Photopea)
// ```
//
// The list is in application order, like `filter.filterGallery`'s `effects`. The item's
// `FrgC`/`BckC` are the colours the Sketch effects read. Only effects whose settings we know are
// mapped; a stack with any other effect (or setting) stays an [`UNSUPPORTED_FILTER`].

const GALLERY_CLASS: &str = "GEfc";
const GALLERY_COMMAND: &str = "filter.filterGallery";
const GALLERY_NAME: &str = "Filter Gallery...";

/// One gallery effect: our filter key, Photoshop's `GEft` code and its settings (PSD key, our key).
struct GalleryCode {
    key: &'static str,
    code: &'static str,
    settings: &'static [(&'static str, &'static str)],
}

const GALLERY_EFFECTS: [GalleryCode; 1] = [GalleryCode { key: "chrome", code: "Chrm", settings: &[("Dtl ", "detail"), ("Smth", "smoothness")] }];

/// An `RGBC` descriptor (0–255 channels) as straight `[r, g, b, a]`, 0–1.
fn rgbc_colour(d: &Descriptor) -> Option<J> {
    let c = |k: &str| px(d, k).map(|v| (v / 255.0).clamp(0.0, 1.0));
    Some(json!([c("Rd  ")?, c("Grn ")?, c("Bl  ")?, 1.0]))
}

/// Fibers, Clouds and Difference Clouds draw with the item's foreground and background colours:
/// their params are the `Fltr` settings plus those colours.
fn coloured_params(class: &str, item: &Descriptor, f: &Descriptor) -> Option<(&'static str, J)> {
    let k = KNOWN.iter().find(|k| k.class == class)?;
    // An `RGBC` of three `doub`s in 0–255, as Photoshop writes it (and we write it back); any
    // other colour keeps the filter verbatim.
    let colour = |key: &str| {
        get_desc(item, key)
            .filter(|c| c.class_id.is("RGBC") && exact_keys(c, &["Rd  ", "Grn ", "Bl  "]))
            .filter(|c| ["Rd  ", "Grn ", "Bl  "].iter().all(|k| double(c, k).is_some_and(|v| (0.0..=255.0).contains(&v))))
            .and_then(rgbc_colour)
    };
    // Only the settings Photoshop writes; an extra or repeated one keeps the filter verbatim.
    let keys: &[&str] = if class == "Fbrs" { &["Vrnc", "Strg", "RndS"] } else { &["FlRs"] };
    if !exact_keys(f, keys) {
        return None;
    }
    // The seeds (`RndS`, `FlRs`) are signed longs; ours is their bit pattern.
    let mut p = match class {
        "Fbrs" => json!({"variance": int(f, "Vrnc")?, "strength": int(f, "Strg")?, "seed": int(f, "RndS")? as u32}),
        _ => json!({"seed": int(f, "FlRs")? as u32}),
    };
    let o = p.as_object_mut()?;
    o.insert("foreground".into(), colour("FrgC")?);
    o.insert("background".into(), colour("BckC")?);
    Some((k.command, p))
}

fn seed_param(p: &J) -> Value {
    Value::Integer(p.get("seed").and_then(J::as_u64).and_then(|n| u32::try_from(n).ok()).unwrap_or(0) as i32)
}

/// `filter.filterGallery` params for a `GEfc` filter item; `None` when an effect or setting is
/// not modelled.
fn gallery_params(item: &Descriptor, f: &Descriptor) -> Option<J> {
    let list: Vec<&Descriptor> = match f.get("GEfs") {
        Some(Value::List(list)) => list
            .iter()
            .map(|v| match v {
                Value::Descriptor(e) => Some(e),
                _ => None,
            })
            .collect::<Option<_>>()?,
        Some(_) => return None,
        None if f.get("GEfk").is_some() => vec![f],
        None => return None,
    };
    let mut effects = Vec::with_capacity(list.len());
    for e in list {
        let code = enum_of(e, "GEfk")?;
        let g = GALLERY_EFFECTS.iter().find(|g| g.code.as_bytes() == code)?;
        let mut params = Map::new();
        for (k, v) in &e.items {
            if k.is("GEfk") || k.is("GELv") {
                continue;
            }
            let (_, ours) = g.settings.iter().find(|(psd, _)| k.is(psd))?;
            params.insert((*ours).into(), json!(int_value(v)?));
        }
        effects.push(json!({"filter": g.key, "params": params, "visible": bool_or(e, "GELv", true)}));
    }
    if effects.is_empty() {
        return None;
    }
    let mut out = Map::new();
    out.insert("effects".into(), J::Array(effects));
    for (psd, ours) in [("FrgC", "foreground"), ("BckC", "background")] {
        if let Some(c) = get_desc(item, psd).and_then(rgbc_colour) {
            out.insert(ours.into(), c);
        }
    }
    Some(J::Object(out))
}

fn colour_rgb(v: &J) -> Option<[f64; 3]> {
    match v {
        J::Array(a) => Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?, a.get(2)?.as_f64()?]),
        J::String(h) if h.len() == 7 && h.starts_with('#') => {
            let ch = |j: usize| h.get(j..j + 2).and_then(|s| u8::from_str_radix(s, 16).ok()).map(|v| f64::from(v) / 255.0);
            Some([ch(1)?, ch(3)?, ch(5)?])
        }
        _ => None,
    }
}

/// A `[r, g, b(, a)]` (0–1) or `#rrggbb` colour param as an `RGBC` descriptor (0–255).
fn colour_rgbc(v: Option<&J>, default: f64) -> Result<Value, String> {
    let Some(v) = v else { return Ok(rgbc(default)) };
    let rgb = colour_rgb(v).filter(|c| c.iter().all(|v| v.is_finite())).ok_or("a colour is not [r, g, b] or #rrggbb")?;
    let c = |v: f64| Value::Double(v.clamp(0.0, 1.0) * 255.0);
    Ok(Value::Descriptor(Descriptor::new("RGBC").with("Rd  ", c(rgb[0])).with("Grn ", c(rgb[1])).with("Bl  ", c(rgb[2]))))
}

/// The `filterFXList` item for a `filter.filterGallery` smart filter.
fn gallery_item(f: &SmartFilter) -> Result<Descriptor, String> {
    let p = &f.params;
    if let Some(k) = p.as_object().and_then(|o| o.keys().find(|k| !is_meta(k) && !["effects", "foreground", "background"].contains(&k.as_str()))) {
        return Err(format!("setting `{k}` has no PSD equivalent"));
    }
    let list = p.get("effects").and_then(J::as_array).ok_or("it has no effects")?;
    let mut out = Vec::with_capacity(list.len());
    for e in list {
        let key = e.get("filter").and_then(J::as_str).ok_or("an effect has no `filter`")?;
        let g = GALLERY_EFFECTS.iter().find(|g| g.key == key).ok_or_else(|| format!("gallery effect `{key}` has no PSD equivalent"))?;
        let params = e.get("params").filter(|v| v.is_object()).unwrap_or(e);
        if let Some(k) = params.as_object().and_then(|o| {
            o.keys().find(|k| !is_meta(k) && !["filter", "params", "visible"].contains(&k.as_str()) && !g.settings.iter().any(|(_, ours)| ours == k))
        }) {
            return Err(format!("gallery setting `{key}.{k}` has no PSD equivalent"));
        }
        let defaults = photocraft_algo::GalleryFilter::from_key(key).map(photocraft_algo::GalleryEffect::new);
        let settings: Vec<(&str, Value)> =
            g.settings.iter().map(|(psd, ours)| (*psd, pi(params, ours, defaults.as_ref().map_or(0.0, |x| f64::from(x.get(ours)))))).collect();
        out.push((g.code, settings, e.get("visible").and_then(J::as_bool).unwrap_or(true)));
    }
    let gallery = || {
        let mut d = Descriptor::new(GALLERY_CLASS);
        d.name = UnicodeString::new_nul("Filter Gallery");
        d
    };
    // One visible effect: inline, as Photoshop writes it. Otherwise the effect list.
    let fltr = match out.as_slice() {
        [] => return Err("it has no effects".into()),
        [(code, settings, true)] => settings.iter().fold(gallery().with("GEfk", enumv("GEft", code)), |d, (k, v)| d.with(k, v.clone())),
        _ => {
            let list = out
                .into_iter()
                .map(|(code, settings, visible)| {
                    let d = settings.into_iter().fold(gallery(), |d, (k, v)| d.with(k, v));
                    Value::Descriptor(d.with("GEfk", enumv("GEft", code)).with("GELv", Value::Boolean(visible)))
                })
                .collect();
            gallery().with("GEfs", Value::List(list))
        }
    };
    Ok(Descriptor::new("filterFX")
        .with("Nm  ", Value::Text(UnicodeString::new_nul(GALLERY_NAME)))
        .with("blendOptions", blend_options(f))
        .with("enab", Value::Boolean(f.visible))
        .with("hasoptions", Value::Boolean(true))
        .with("FrgC", colour_rgbc(p.get("foreground"), 0.0)?)
        .with("BckC", colour_rgbc(p.get("background"), 255.0)?)
        .with("Fltr", Value::Descriptor(fltr))
        .with("filterID", Value::Integer(code(b"GEfc"))))
}

/// Lowercase hex of `bytes`.
fn to_hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(H[usize::from(b >> 4)]));
        s.push(char::from(H[usize::from(b & 15)]));
    }
    s
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    let d = |c: u8| (c as char).to_digit(16).map(|v| v as u8);
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    b.as_chunks::<2>().0.iter().map(|c| Some(d(c[0])? << 4 | d(c[1])?)).collect()
}

/// One `filterFXList` item → [`SmartFilter`] (an [`UNSUPPORTED_FILTER`] when not modelled).
pub fn filter_from_item(item: &Descriptor) -> SmartFilter {
    let opts = get_desc(item, "blendOptions");
    let opacity = opts.and_then(|o| px(o, "Opct")).map_or(1.0, |v| (v / 100.0).clamp(0.0, 1.0) as f32);
    let blend = opts.and_then(|o| enum_of(o, "Md  ")).and_then(crate::effects_map::blend_from_id).unwrap_or(BlendMode::Normal);
    let visible = bool_or(item, "enab", true);
    let fltr = get_desc(item, "Fltr");
    let class = fltr.map(|f| String::from_utf8_lossy(f.class_id.as_bytes()).into_owned());
    let filter_id = int(item, "filterID").unwrap_or(0);
    let class = class.or_else(|| KNOWN.iter().find(|k| k.filter_id == filter_id).map(|k| k.class.to_string()));
    let known = match class.as_deref() {
        Some(camera_raw::CLASS) => camera_raw::import_params(item).map(|p| (camera_raw::COMMAND, p)),
        Some(displace::CLASS) => displace::import_params(item).map(|p| (displace::COMMAND, p)),
        Some(hue_sat::CLASS) => fltr.and_then(hue_sat::import_params).map(|p| (hue_sat::COMMAND, p)),
        Some(perspective_warp::CLASS) => perspective_warp::import_params(item).map(|p| (perspective_warp::COMMAND, p)),
        Some(GALLERY_CLASS) => fltr.and_then(|f| gallery_params(item, f)).map(|p| (GALLERY_COMMAND, p)),
        Some(c @ ("Fbrs" | "Clds" | "DfrC")) => fltr.and_then(|f| coloured_params(c, item, f)),
        // A `Fltr` that is there but not a descriptor is settings we can't read: kept verbatim
        // rather than taken for a filter stored without options (which would drop it on save).
        _ if fltr.is_none() && item.get("Fltr").is_some() => None,
        c => c.and_then(|c| known_params(c, fltr, item)),
    };
    let (command, params) = match known {
        Some((c, p)) => (c.to_string(), p),
        None => {
            let name = text_of(item, "Nm  ").unwrap_or_default();
            // The stored descriptor carries neutral blending (the model's own fields replace
            // them on export), so hiding or re-blending the filter leaves its params alone.
            let mut neutral = item.clone();
            for (k, v) in &mut neutral.items {
                if k.is("blendOptions") {
                    *v = blend_options(&SmartFilter { command: String::new(), params: J::Null, blend: BlendMode::Normal, opacity: 1.0, visible: true });
                } else if k.is("enab") {
                    *v = Value::Boolean(true);
                }
            }
            (UNSUPPORTED_FILTER.to_string(), json!({"name": name.replace('&', ""), "filterID": filter_id, "psd": to_hex(&neutral.to_bytes())}))
        }
    };
    SmartFilter { command, params, blend, opacity, visible }
}

// ---------- writing ----------

fn unit(u: &[u8; 4], v: f64) -> Value {
    Value::UnitFloat { unit: *u, value: v }
}

fn rgbc(v: f64) -> Value {
    Value::Descriptor(Descriptor::new("RGBC").with("Rd  ", Value::Double(v)).with("Grn ", Value::Double(v)).with("Bl  ", Value::Double(v)))
}

fn blend_options(f: &SmartFilter) -> Value {
    let opacity = if f.opacity.is_finite() { f64::from(f.opacity.clamp(0.0, 1.0)) * 100.0 } else { 100.0 };
    Value::Descriptor(Descriptor::new("blendOptions").with("Opct", unit(b"#Prc", opacity)).with("Md  ", crate::effects_map::blend_long_value(f.blend)))
}

/// Parameter keys that are bookkeeping, not filter settings.
fn is_meta(k: &str) -> bool {
    k == "layer" || k.starts_with("__")
}

fn pf(p: &J, key: &str, default: f64) -> f64 {
    p.get(key).and_then(J::as_f64).filter(|v| v.is_finite()).unwrap_or(default)
}

fn pi(p: &J, key: &str, default: f64) -> Value {
    Value::Integer(pf(p, key, default).round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32)
}

/// A whole-number setting as a long, clamped to `range` like the engine clamps it; a fraction
/// has no PSD equivalent (Photoshop stores these as integers).
fn whole(v: f64, (lo, hi): (i32, i32), what: &str) -> Result<Value, String> {
    if v.fract() != 0.0 {
        return Err(format!("{what} ({v}) is not a whole number"));
    }
    Ok(Value::Integer(v.clamp(f64::from(lo), f64::from(hi)) as i32))
}

fn channel_ref(code: &[u8; 4]) -> Value {
    Value::Reference(vec![ReferenceItem::Enumerated {
        class: Class { name: UnicodeString::new_nul(""), class_id: Id::new("Chnl") },
        type_id: Id::new("Chnl"),
        value: Id::Code(*code),
    }])
}

/// The `Adjs` entries of a Levels or Curves filter: the composite first, then red, green, blue.
fn adjustments(p: &J, curves: bool) -> Result<Value, String> {
    let obj = p.as_object().ok_or("parameters are not an object")?;
    let allowed: &[&str] =
        if curves { &["points", "red", "green", "blue", "pencil"] } else { &["inBlack", "inWhite", "gamma", "outBlack", "outWhite", "red", "green", "blue"] };
    if let Some(k) = obj.keys().find(|k| !is_meta(k) && !allowed.contains(&k.as_str())) {
        return Err(format!("setting `{k}` has no PSD equivalent"));
    }
    let mut list = Vec::new();
    for (code, key) in CHANNELS {
        let o = if key.is_empty() { Some(p) } else { p.get(key) };
        let Some(o) = o else { continue };
        let mut a = Descriptor::new(if curves { "CrvA" } else { "LvlA" }).with("Chnl", channel_ref(code));
        if curves {
            let src = if key.is_empty() { p.get("points") } else { Some(o) };
            let Some(pts) = src.and_then(J::as_array) else { continue };
            if p.get("pencil").and_then(J::as_bool) == Some(true) {
                let map: Option<Vec<Value>> =
                    pts.iter().map(|o| o.as_f64().filter(|o| o.is_finite()).map(|o| Value::Integer(o.round().clamp(0.0, 255.0) as i32))).collect();
                a = a.with("Mpng", Value::List(map.filter(|m| m.len() == 256).ok_or("a pencil-drawn curve is not 256 output levels")?));
                list.push(Value::Descriptor(a));
                continue;
            }
            let pts: Option<Vec<Value>> = pts
                .iter()
                .map(|q| {
                    let x = q.get(0)?.as_f64().filter(|v| v.is_finite())?;
                    let y = q.get(1)?.as_f64().filter(|v| v.is_finite())?;
                    Some(Value::Descriptor(Descriptor::new("Pnt ").with("Hrzn", Value::Double(x)).with("Vrtc", Value::Double(y))))
                })
                .collect();
            a = a.with("Crv ", Value::List(pts.ok_or("a curve point is not [input, output]")?));
        } else {
            if key.is_empty() && !["inBlack", "inWhite", "gamma", "outBlack", "outWhite"].iter().any(|k| o.get(k).is_some()) {
                continue;
            }
            a = a.with("Inpt", Value::List(vec![pi(o, "inBlack", 0.0), pi(o, "inWhite", 255.0)])).with("Gmm ", Value::Double(pf(o, "gamma", 1.0)));
            if o.get("outBlack").is_some() || o.get("outWhite").is_some() {
                a = a.with("Otpt", Value::List(vec![pi(o, "outBlack", 0.0), pi(o, "outWhite", 255.0)]));
            }
        }
        list.push(Value::Descriptor(a));
    }
    Ok(Value::List(list))
}

/// Crystallize's and Pointillize's cell size range (`ClSz`, a `long`).
const CELL_SIZE: (i32, i32) = (3, 300);

/// Our `key` (a string) as Photoshop's enum value through `table` (Photoshop's value, ours).
fn enum_out(p: &J, key: &str, default: &str, type_id: &str, table: &[(&str, &str)]) -> Result<Value, String> {
    let ours = match p.get(key) {
        None => default,
        Some(v) => v.as_str().ok_or_else(|| format!("`{key}` is not a string"))?,
    };
    let (psd, _) = table.iter().find(|(_, o)| *o == ours).ok_or_else(|| format!("{key} `{ours}` has no PSD equivalent"))?;
    Ok(enumv(type_id, psd))
}

/// `key` rounded to a `long` in `lo..=hi`.
fn pi_in(p: &J, key: &str, default: f64, lo: i32, hi: i32) -> Value {
    Value::Integer(pf(p, key, default).round().clamp(f64::from(lo), f64::from(hi)) as i32)
}

/// Param `key` as a `bool` (false when absent or not a bool, as the engine reads it).
fn bool_param(p: &J, key: &str) -> Value {
    Value::Boolean(p.get(key).and_then(J::as_bool).unwrap_or(false))
}

/// The `Fltr` descriptor of a modelled filter (`None` = written without options).
fn fltr_for(k: &Known, p: &J) -> Result<Option<Descriptor>, String> {
    let d = Descriptor::new(k.class);
    Ok(Some(match k.class {
        "GsnB" | "boxblur" | "Mdn " | "HghP" | "Mxm " | "Mnm " => {
            if p.get("preserve").and_then(J::as_str).is_some_and(|s| s == "roundness") {
                return Err("“preserve roundness” has no PSD equivalent".into());
            }
            d.with("Rds ", unit(b"#Pxl", pf(p, "radius", 1.0)))
        }
        "MtnB" => d.with("Angl", pi(p, "angle", 0.0)).with("Dstn", unit(b"#Pxl", pf(p, "distance", 10.0))),
        "UnsM" => d.with("Amnt", unit(b"#Prc", pf(p, "amount", 50.0))).with("Rds ", unit(b"#Pxl", pf(p, "radius", 1.0))).with("Thsh", pi(p, "threshold", 0.0)),
        "AdNs" => {
            let gauss = p.get("distribution").and_then(J::as_str) == Some("gaussian");
            d.with("Dstr", Value::Enumerated { type_id: Id::new("Dstr"), value: Id::new(if gauss { "Gsn " } else { "Unfr" }) })
                .with("Nose", unit(b"#Prc", pf(p, "amount", 12.5)))
                .with("Mnch", Value::Boolean(p.get("monochromatic").and_then(J::as_bool).unwrap_or(false)))
                .with("FlRs", pi(p, "seed", 0.0))
        }
        "surfaceBlur" => d.with("Rds ", unit(b"#Pxl", pf_range(p, "radius", 5.0, 1.0, 100.0)?)).with("Thsh", pi_range(p, "threshold", 15.0, 2, 255)?),
        "SmrB" => d
            .with("Rds ", Value::Double(pf_range(p, "radius", 3.0, 0.1, 100.0)?))
            .with("Thsh", Value::Double(pf_range(p, "threshold", 25.0, 0.1, 100.0)?))
            .with("SmBQ", enum_param(p, "quality", "high", "SmBQ", &SMART_BLUR_QUALITY)?)
            .with("SmBM", enum_param(p, "mode", "normal", "SmBM", &SMART_BLUR_MODE)?),
        "RdlB" => {
            // Photoshop wrote no centre key for a centred blur; an off-centre one is unmeasured.
            for k in ["centerX", "centerY"] {
                if (pf_range(p, k, 0.5, 0.0, 1.0)? - 0.5).abs() > 1e-9 {
                    return Err("an off-centre Radial Blur centre is not written to PSD".into());
                }
            }
            d.with("Amnt", pi_range(p, "amount", 10.0, 1, 100)?)
                .with("BlrM", enum_param(p, "method", "spin", "BlrM", &RADIAL_METHOD)?)
                .with("BlrQ", enum_param(p, "quality", "good", "BlrQ", &RADIAL_QUALITY)?)
        }
        "Msc " => d.with("ClSz", unit(b"#Pxl", pf(p, "cellSize", 10.0))),
        // Photoshop's Mezzotint has no seed (it is random on every render), so `seed` is not saved.
        "Mztn" => {
            let t = p.get("type").and_then(J::as_str).unwrap_or("fineDots");
            let code = MEZZOTINT_TYPES.iter().find(|(_, k)| *k == t).map_or(b"FnDt", |(c, _)| *c);
            d.with("MztT", Value::Enumerated { type_id: Id::new("MztT"), value: Id::Code(*code) })
        }
        "ClrH" => {
            // Longs within the dialog's ranges (what the engine accepts: radius 4..127, angles ±360).
            let long = |key: &str, default: f64, lo: f64, hi: f64| Value::Integer(pf(p, key, default).round().clamp(lo, hi) as i32);
            d.with("Rds ", long("maxRadius", 8.0, 4.0, 127.0))
                .with("Ang1", long("channel1", 108.0, -360.0, 360.0))
                .with("Ang2", long("channel2", 162.0, -360.0, 360.0))
                .with("Ang3", long("channel3", 90.0, -360.0, 360.0))
                .with("Ang4", long("channel4", 45.0, -360.0, 360.0))
        }
        "Embs" => d.with("Angl", pi(p, "angle", 135.0)).with("Hght", pi(p, "height", 3.0)).with("Amnt", pi(p, "amount", 100.0)),
        SMART_SHARPEN => {
            // The engine's defaults and ranges (it clamps the same way when rendering).
            d.with("useLegacy", Value::Boolean(false))
                .with("Amnt", unit(b"#Prc", pf(p, "amount", 100.0).clamp(1.0, 500.0)))
                .with("Rds ", unit(b"#Pxl", pf(p, "radius", 1.0).clamp(0.1, 64.0)))
                .with("noiseReduction", unit(b"#Prc", pf(p, "reduceNoise", 10.0).clamp(0.0, 100.0)))
                .with("blur", enumv("blurType", "GsnB"))
        }
        // Photoshop stores whole pixels and levels.
        "DstS" => {
            let whole = |key: &str, default: f64, lo: f64, hi: f64| Value::Integer(pf(p, key, default).round().clamp(lo, hi) as i32);
            d.with("Rds ", whole("radius", 1.0, 1.0, 500.0)).with("Thsh", whole("threshold", 0.0, 0.0, 255.0))
        }
        // Photoshop stores no random seed for these: `seed` is ours only and is not written.
        "Dfs " => d.with("Md  ", enum_out(p, "mode", "normal", "DfsM", &DIFFUSE_MODES)?),
        "TrcC" => d.with("Lvl ", pi_in(p, "level", 128.0, 0, 255)).with("Edg ", enum_out(p, "edge", "lower", "CntE", &TRACE_EDGES)?),
        "Crst" => d.with("ClSz", pi_in(p, "cellSize", 10.0, CELL_SIZE.0, CELL_SIZE.1)),
        "Pntl" => d.with("ClSz", pi_in(p, "cellSize", 5.0, CELL_SIZE.0, CELL_SIZE.1)),
        // Photoshop stores no random seed for these: our `seed` is not written.
        "Wnd " => {
            d.with("WndM", enum_out(p, "method", "wind", "WndM", &WIND_METHODS)?).with("Drct", enum_out(p, "direction", "fromRight", "Drct", &WIND_DIRECTIONS)?)
        }
        "Extr" => d
            .with("ExtT", enum_out(p, "type", "blocks", "ExtT", &EXTRUDE_TYPES)?)
            .with("ExtS", pi_in(p, "size", 30.0, EXTRUDE_SIZE.0, EXTRUDE_SIZE.1))
            .with("ExtD", pi_in(p, "depth", 30.0, EXTRUDE_DEPTH.0, EXTRUDE_DEPTH.1))
            .with("ExtR", enum_out(p, "depthMode", "random", "ExtR", &EXTRUDE_DEPTHS)?)
            .with("ExtF", bool_param(p, "solidFrontFaces"))
            .with("ExtM", bool_param(p, "maskIncompleteBlocks")),
        "Tls " => d
            .with("TlNm", pi_in(p, "count", 10.0, TILE_COUNT.0, TILE_COUNT.1))
            .with("TlOf", pi_in(p, "maxOffset", 10.0, TILE_OFFSET.0, TILE_OFFSET.1))
            .with("FlCl", enum_out(p, "fill", "background", "FlCl", &TILE_FILLS)?),
        "Ofst" => {
            // As the engine reads it: anything but repeat / transparent wraps.
            let ours = p.get("undefinedAreas").and_then(J::as_str).unwrap_or("wrap");
            let fill = OFFSET_FILL.iter().find(|(_, o)| *o == ours).map_or("Wrp ", |(psd, _)| *psd);
            d.with("Hrzn", pi(p, "horizontal", 0.0)).with("Vrtc", pi(p, "vertical", 0.0)).with("Fl  ", enumv("FlMd", fill))
        }
        "Cstm" => {
            // The engine's identity kernel when none is given; missing cells are 0.
            let kernel = p.get("kernel").and_then(J::as_array);
            let cell = |i: usize| match kernel {
                Some(a) => a.get(i).and_then(J::as_f64).filter(|v| v.is_finite()).unwrap_or(0.0),
                None => f64::from(u8::from(i == 12)),
            };
            let m = (0..25).map(|i| whole(cell(i), CUSTOM_CELL, "a kernel value")).collect::<Result<Vec<_>, _>>()?;
            d.with("Scl ", whole(pf(p, "scale", 1.0), CUSTOM_SCALE, "Scale")?)
                .with("Ofst", whole(pf(p, "offset", 0.0), CUSTOM_OFFSET, "Offset")?)
                .with("Mtrx", Value::List(m))
        }
        "LnsF" => {
            let lens = p.get("lens").and_then(J::as_str).unwrap_or("zoom");
            let code = LENSES.iter().find(|(_, l)| *l == lens).ok_or_else(|| format!("lens `{lens}` has no PSD equivalent"))?.0;
            let mut centre = Descriptor::new("Pnt ").with("Hrzn", Value::Double(pf(p, "centerX", 0.5))).with("Vrtc", Value::Double(pf(p, "centerY", 0.5)));
            centre.name = UnicodeString::new_nul("");
            d.with("Brgh", pi(p, "brightness", 100.0))
                .with("FlrC", Value::Descriptor(centre))
                .with("Lns ", Value::Enumerated { type_id: Id::new("Lns "), value: Id::Code(*code) })
        }
        "Fbrs" => d.with("Vrnc", pi(p, "variance", 16.0)).with("Strg", pi(p, "strength", 4.0)).with("RndS", seed_param(p)),
        "Clds" | "DfrC" => d.with("FlRs", seed_param(p)),
        "Lvls" => d.with("Adjs", adjustments(p, false)?),
        "Crvs" => d.with("Adjs", adjustments(p, true)?),
        "adaptCorrect" => {
            if p.as_object().is_some_and(|o| o.keys().any(|k| !is_meta(k))) {
                return Err("only default Shadows/Highlights settings are written to PSD".into());
            }
            return Ok(None);
        }
        // No settings to write; any parameter keys an old record carries have no effect on the
        // filter either, so nothing is lost.
        c if NO_OPTIONS.contains(&c) => return Ok(None),
        // Keys and order as Photoshop writes them.
        "Pnch" => d.with("Amnt", pi(p, "amount", 50.0)),
        "Twrl" => d.with("Angl", pi(p, "angle", 50.0)),
        "Rple" => d.with("Amnt", pi(p, "amount", 100.0)).with("RplS", enum_param(p, "size", "medium", "RplS", &RIPPLE_SIZE)?),
        "Sphr" => d.with("Amnt", pi(p, "amount", 100.0)).with("SphM", enum_param(p, "mode", "normal", "SphM", &SPHERIZE_MODE)?),
        "Plr " => d.with("Cnvr", enum_param(p, "mode", "rectangularToPolar", "Cnvr", &POLAR_MODE)?),
        "ZgZg" => {
            d.with("Amnt", pi(p, "amount", 10.0)).with("NmbR", pi(p, "ridges", 5.0)).with("ZZTy", enum_param(p, "style", "pondRipples", "ZZTy", &ZIGZAG_STYLE)?)
        }
        "Wave" => {
            // The seed the renderer draws with (engine `filters::seed`): a u32, else a legacy signed
            // or fractional seed, rounded and saturated, as its bit pattern.
            let seed = match p.get("seed").and_then(J::as_u64).and_then(|s| u32::try_from(s).ok()) {
                Some(s) => Value::Integer(s as i32),
                None => pi(p, "seed", 0.0),
            };
            // The renderer draws at most MAX_WAVE_GENERATORS: write what PhotoCraft rendered, so
            // Photoshop doesn't draw more generators than the user saw.
            let generators = pf(p, "generators", 5.0).round().clamp(1.0, f64::from(photocraft_algo::MAX_WAVE_GENERATORS)) as i32;
            d.with("Wvtp", enum_param(p, "type", "sine", "Wvtp", &WAVE_TYPE)?)
                .with("NmbG", Value::Integer(generators))
                .with("WLMn", pi(p, "wavelengthMin", 10.0))
                .with("WLMx", pi(p, "wavelengthMax", 120.0))
                .with("AmMn", pi(p, "amplitudeMin", 5.0))
                .with("AmMx", pi(p, "amplitudeMax", 35.0))
                .with("SclH", Value::Integer(100))
                .with("SclV", Value::Integer(100))
                .with("UndA", enum_param(p, "undefinedAreas", "wrap", "UndA", &UNDEFINED_AREAS)?)
                .with("RndS", seed)
        }
        "Invr" => return Ok(None),
        _ => return Err("not a recognized filter".into()),
    }))
}

/// The `filterFXList` item for one smart filter, or why it can't be written.
pub fn item_for_filter(f: &SmartFilter) -> Result<Descriptor, String> {
    if f.command == camera_raw::COMMAND {
        let mut d = camera_raw::export_item(&f.params)?;
        set(&mut d, "blendOptions", blend_options(f));
        set(&mut d, "enab", Value::Boolean(f.visible));
        return Ok(d);
    }
    if f.command == displace::COMMAND {
        let mut d = displace::export_item(&f.params)?;
        set(&mut d, "blendOptions", blend_options(f));
        set(&mut d, "enab", Value::Boolean(f.visible));
        return Ok(d);
    }
    if f.command == GALLERY_COMMAND {
        return gallery_item(f);
    }
    if f.command == hue_sat::COMMAND {
        return hue_sat::export_item(f);
    }
    if perspective_warp::handles(&f.command) {
        return perspective_warp::export_item(f);
    }
    if f.command == UNSUPPORTED_FILTER {
        let raw = f.params.get("psd").and_then(J::as_str).and_then(from_hex).ok_or("its filter data is missing")?;
        let mut d = Descriptor::from_bytes(&raw).map_err(|e| format!("its filter data is unreadable ({e})"))?;
        set(&mut d, "blendOptions", blend_options(f));
        set(&mut d, "enab", Value::Boolean(f.visible));
        return Ok(d);
    }
    let k = known_by_command(&f.command).ok_or("this filter has no PSD equivalent")?;
    let fltr = fltr_for(k, &f.params)?;
    // Pointillize's background, Tiles' colours and the render filters' colours are the entry's;
    // the others store Photoshop's defaults.
    let (fg, bg) = match k.class {
        "Pntl" => (rgbc(0.0), colour_rgbc(f.params.get("background"), 255.0)?),
        "Tls " | "Fbrs" | "Clds" | "DfrC" => (colour_rgbc(f.params.get("foreground"), 0.0)?, colour_rgbc(f.params.get("background"), 255.0)?),
        _ => (rgbc(0.0), rgbc(255.0)),
    };
    // The render filters are written with the descriptor names Photoshop gives them: the filter's
    // own name on `Fltr` and empty ones on the colours.
    let render = matches!(k.class, "Fbrs" | "Clds" | "DfrC" | "LnsF");
    let (fltr, fg, bg) = if render { (fltr.map(|f| named(f, k.name.trim_end_matches("..."))), named_value(fg), named_value(bg)) } else { (fltr, fg, bg) };
    let mut d = Descriptor::new("filterFX")
        .with("Nm  ", Value::Text(UnicodeString::new_nul(k.name)))
        .with("blendOptions", blend_options(f))
        .with("enab", Value::Boolean(f.visible))
        .with("hasoptions", Value::Boolean(fltr.is_some()))
        .with("FrgC", fg)
        .with("BckC", bg);
    if let Some(x) = fltr {
        d = d.with("Fltr", Value::Descriptor(x));
    }
    Ok(d.with("filterID", Value::Integer(k.filter_id)))
}

fn named(mut d: Descriptor, name: &str) -> Descriptor {
    d.name = UnicodeString::new_nul(name);
    d
}

fn named_value(v: Value) -> Value {
    match v {
        Value::Descriptor(d) => Value::Descriptor(named(d, "")),
        other => other,
    }
}

/// Builds a `filterFX` object; filters that can't be written are left out, with a message each.
pub fn filter_fx(stack: &FilterStack, layer: &str, warnings: &mut Vec<String>) -> Descriptor {
    let mut list = Vec::new();
    for f in &stack.filters {
        match item_for_filter(f) {
            Ok(d) => list.push(Value::Descriptor(d)),
            Err(e) => warnings.push(format!("layer \"{layer}\": smart filter {} was not written to PSD: {e}", f.command)),
        }
    }
    Descriptor::new("filterFXStyle")
        .with("enab", Value::Boolean(stack.enabled))
        .with("validAtPosition", Value::Boolean(true))
        .with("filterMaskEnable", Value::Boolean(stack.mask_enabled))
        .with("filterMaskLinked", Value::Boolean(stack.mask_linked))
        .with("filterMaskExtendWithWhite", Value::Boolean(true))
        .with("filterFXList", Value::List(list))
}

/// Replaces item `key` in place, or appends it.
fn set(d: &mut Descriptor, key: &str, v: Value) {
    match d.items.iter_mut().find(|(k, _)| k.is(key)) {
        Some(e) => e.1 = v,
        None => d.items.push((Id::new(key), v)),
    }
}

/// Replaces item `key` in place, or inserts it before item `before` (appends it without one).
fn set_before(d: &mut Descriptor, key: &str, v: Value, before: &str) {
    if let Some(e) = d.items.iter_mut().find(|(k, _)| k.is(key)) {
        e.1 = v;
        return;
    }
    let at = d.items.iter().position(|(k, _)| k.is(before)).unwrap_or(d.items.len());
    d.items.insert(at, (Id::new(key), v));
}

/// Everything needed to write the placed-layer blocks of a smart object.
#[derive(Clone, Debug)]
pub struct PlacedSpec<'a> {
    pub idnt: &'a str,
    pub placed: &'a str,
    /// Source pixels → document pixels.
    pub transform: Affine,
    /// Distort / Perspective: the full projective map (row-major 3×3), overriding `transform`.
    pub perspective: Option<[f64; 9]>,
    /// Source size in pixels.
    pub size: (f64, f64),
    /// Source resolution.
    pub dpi: f64,
    pub warp: Option<&'a Warp>,
    /// `filterFX`, when the smart object has filters.
    pub filter_fx: Option<Descriptor>,
}

/// Where the frame corners land (top-left, top-right, bottom-right, bottom-left), as x, y pairs.
/// The frame is [`crate::blocks::placed_frame`] of `warp`, the warp the block stores, for an
/// affine placement (the inverse of `parse_smart`), [`crate::blocks::perspective_frame`] of it
/// for a projective one (as `parse_smart_perspective` reads it).
fn quad_points(s: &PlacedSpec<'_>, warp: Option<&Warp>) -> [f64; 8] {
    let (w, h) = s.size;
    let [a, b, c, d, e, f] = s.transform.m;
    let map = |x: f64, y: f64| match &s.perspective {
        Some(p) => photocraft_algo::transform::Homography(*p).apply(x, y),
        None => (a * x + c * y + e, b * x + d * y + f),
    };
    let [x0, y0, x1, y1] = if s.perspective.is_some() { crate::blocks::perspective_frame(warp, w, h) } else { crate::blocks::placed_frame(warp, w, h) };
    let mut out = [0.0; 8];
    for (i, (x, y)) in [(x0, y0), (x1, y0), (x1, y1), (x0, y1)].into_iter().enumerate() {
        (out[2 * i], out[2 * i + 1]) = map(x, y);
    }
    out
}

/// The transform quad: the source corners (top-left, top-right, bottom-right, bottom-left).
fn quad(s: &PlacedSpec<'_>, warp: Option<&Warp>) -> Value {
    Value::List(quad_points(s, warp).into_iter().map(Value::Double).collect())
}

/// Has the placement moved from the quad a template descriptor stores (`Trnf`)?
fn quad_moved(template: &Descriptor, s: &PlacedSpec<'_>, warp: Option<&Warp>) -> bool {
    let Some(Value::List(pts)) = template.get("Trnf") else { return true };
    let old: Vec<f64> = pts.iter().filter_map(|v| crate::blocks::num(Some(v))).collect();
    let new = quad_points(s, warp);
    old.len() != 8 || old.iter().zip(new).any(|(a, b)| (a - b).abs() > 1e-6 * (1.0 + a.abs()))
}

fn enumv(t: &str, v: &str) -> Value {
    Value::Enumerated { type_id: Id::new(t), value: Id::new(v) }
}

/// The mesh of a split warp: a custom warp of more than one patch, which `SoLd` stores as
/// `quiltWarp` and `PlLd` can't store.
fn split_mesh(w: Option<&Warp>) -> Option<&BezierMesh> {
    w.filter(|w| w.style == WarpStyle::Custom).and_then(|w| w.mesh.as_ref()).filter(|m| m.points.len() != 16 && m.is_valid())
}

/// The warp `SoLd` stores: none, with a message, for a split warp of more patches along a side
/// than a `quiltWarp` is read with, so its corners are framed by the source rect as on reopen.
fn sold_warp<'a>(w: Option<&'a Warp>, warnings: &mut Vec<String>) -> Option<&'a Warp> {
    let too_big = split_mesh(w).is_some_and(|m| m.us.len().max(m.vs.len()) > crate::blocks::QUILT_MAX_PATCHES + 1);
    if too_big {
        warnings.push(format!("a split warp of more than {} patches along a side was written to PSD without its warp", crate::blocks::QUILT_MAX_PATCHES));
        return None;
    }
    w
}

/// Style, bend, distortions, axis, bounds and order: the keys a `warp` and a `quiltWarp`
/// descriptor share, in Photoshop's order.
fn warp_head(class: &str, w: &Warp) -> Descriptor {
    let [x0, y0, x1, y1] = w.bounds;
    let bounds = Descriptor::new("classFloatRect")
        .with("Top ", Value::Double(y0))
        .with("Left", Value::Double(x0))
        .with("Btom", Value::Double(y1))
        .with("Rght", Value::Double(x1));
    let d = Descriptor::new(class).with("warpStyle", enumv("warpStyle", w.style.psd_name())).with("warpValue", Value::Double(w.bend));
    // A cylinder is stored as its seven `warpValues` (#4112).
    let d = if w.style == WarpStyle::Cylinder { d.with("warpValues", Value::List(w.values.iter().copied().map(Value::Double).collect())) } else { d };
    d.with("warpPerspective", Value::Double(w.h_distort))
        .with("warpPerspectiveOther", Value::Double(w.v_distort))
        .with("warpRotate", enumv("Ornt", if w.vertical { "Vrtc" } else { "Hrzn" }))
        .with("bounds", Value::Descriptor(bounds))
        .with("uOrder", Value::Integer(4))
        .with("vOrder", Value::Integer(4))
}

/// The control points of a custom warp: an object array of their x and y.
fn mesh_points(m: &BezierMesh) -> Value {
    let body = Descriptor::new("rationalPoint")
        .with("Hrzn", Value::UnitFloats { unit: *b"#Pxl", values: m.points.iter().map(|p| p[0]).collect() })
        .with("Vrtc", Value::UnitFloats { unit: *b"#Pxl", values: m.points.iter().map(|p| p[1]).collect() });
    Value::ObjectArray(ObjectArray { prefix: m.points.len() as u32, body })
}

/// The `warp` descriptor (also the `PlLd` warp). A split warp is written as no warp: `SoLd`
/// keeps it in `quiltWarp` ([`quilt_desc`]), next to this `warpNone`, as Photoshop does. A
/// custom warp without a usable mesh is written as no warp, with a message. A cylinder is
/// written as its seven `warpValues`.
fn warp_desc(w: Option<&Warp>, size: (f64, f64), warnings: &mut Vec<String>) -> Value {
    let none = Warp::none([0.0, 0.0, size.0, size.1]);
    let mut w = w.unwrap_or(&none).clone();
    let mesh = match (&w.style, &w.mesh) {
        (WarpStyle::Custom, Some(m)) if m.points.len() == 16 => Some(m.clone()),
        (WarpStyle::Custom, Some(m)) if m.is_valid() => {
            w = none.clone();
            None
        }
        (WarpStyle::Custom, _) => {
            warnings.push("a custom warp without a valid mesh was written to PSD without its warp".into());
            w = none.clone();
            None
        }
        _ => None,
    };
    let mut d = warp_head("warp", &w);
    if let Some(m) = mesh {
        d = d.with("customEnvelopeWarp", Value::Descriptor(Descriptor::new("customEnvelopeWarp").with("meshPoints", mesh_points(&m))));
    }
    Value::Descriptor(d)
}

/// The `quiltWarp` descriptor of a split warp, in Photoshop's layout: `deformNumCols` ×
/// `deformNumRows` control points, and the patch boundaries (`quiltSliceX` / `quiltSliceY`) at
/// the knots across the warp's box. The imported `quiltWarp` (`old`) gives its own slices where
/// they still bound the mesh's patches, so their units and padding stay Photoshop's.
fn quilt_desc(w: &Warp, mesh: &BezierMesh, old: Option<&Descriptor>) -> Value {
    let old_env = old.and_then(|q| get_desc(q, "customEnvelopeWarp"));
    let [x0, y0, x1, y1] = w.bounds;
    let slices = |key: &str, knots: &[f64], lo: f64, hi: f64| {
        if let Some(env) = old_env
            && crate::blocks::quilt_knots(env, key, knots.len() - 1).as_deref() == Some(knots)
            && let Some(v) = env.get(key)
        {
            return v.clone();
        }
        let (lo, hi) = if (hi - lo).is_finite() && hi > lo { (lo, hi) } else { (0.0, 1.0) };
        let values: Vec<f64> = knots.iter().map(|u| lo + u * (hi - lo)).collect();
        let prefix = values.len() as u32;
        Value::ObjectArray(ObjectArray { prefix, body: Descriptor::new("UntF").with(key, Value::UnitFloats { unit: *b"#Pxl", values }) })
    };
    let envelope = Descriptor::new("customEnvelopeWarp")
        .with("quiltSliceX", slices("quiltSliceX", &mesh.us, x0, x1))
        .with("quiltSliceY", slices("quiltSliceY", &mesh.vs, y0, y1))
        .with("meshPoints", mesh_points(mesh));
    Value::Descriptor(
        warp_head("quiltWarp", w)
            .with("deformNumRows", Value::Integer(mesh.ny() as i32))
            .with("deformNumCols", Value::Integer(mesh.nx() as i32))
            .with("customEnvelopeWarp", Value::Descriptor(envelope)),
    )
}

fn ratio(n: i32, d: i32) -> Value {
    Value::Descriptor(Descriptor::new("null").with("numerator", Value::Integer(n)).with("denominator", Value::Integer(d)))
}

fn pad4(mut v: Vec<u8>) -> Vec<u8> {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
    v
}

/// `SoLd` block data. With `template` (the imported descriptor) every key it has is kept and only
/// the placed id (for a duplicate), transform (when it moved), warp and filter stack are rewritten;
/// otherwise a fresh descriptor in Photoshop's key order. A split warp goes in `quiltWarp`.
pub fn sold_bytes(template: Option<&Descriptor>, s: &PlacedSpec<'_>, warnings: &mut Vec<String>) -> Vec<u8> {
    let stored = sold_warp(s.warp, warnings);
    let warp = warp_desc(stored, s.size, warnings);
    let split = stored.zip(split_mesh(stored));
    let d = match template {
        Some(t) => {
            let mut d = t.clone();
            if text_of(t, "placed").as_deref() != Some(s.placed) {
                set(&mut d, "placed", Value::Text(UnicodeString::new_nul(s.placed)));
            }
            if quad_moved(t, s, stored) {
                set(&mut d, "Trnf", quad(s, stored));
                set(&mut d, "nonAffineTransform", quad(s, stored));
            }
            set(&mut d, "warp", warp);
            // `quiltWarp` is read before `warp`, so the imported one stays only while it still
            // says what the layer's warp is (unchanged, or unreadable while the layer has none);
            // otherwise it is rewritten, or dropped for a warp that isn't split.
            let old = get_desc(t, "quiltWarp");
            let kept = old.is_some_and(|q| match crate::blocks::warp_of(q) {
                Some(w) => w.as_ref() == stored,
                None => s.warp.is_none(),
            });
            if !kept {
                match split {
                    Some((w, m)) => set_before(&mut d, "quiltWarp", quilt_desc(w, m, old), "warp"),
                    None => d.items.retain(|(k, _)| !k.is("quiltWarp")),
                }
            }
            match &s.filter_fx {
                Some(fx) => set(&mut d, "filterFX", Value::Descriptor(fx.clone())),
                None => d.items.retain(|(k, _)| !k.is("filterFX")),
            }
            d
        }
        None => {
            let mut d = Descriptor::new("null")
                .with("Idnt", Value::Text(UnicodeString::new_nul(s.idnt)))
                .with("placed", Value::Text(UnicodeString::new_nul(s.placed)))
                .with("PgNm", Value::Integer(1))
                .with("totalPages", Value::Integer(1))
                .with("Crop", Value::Integer(1))
                .with("frameStep", ratio(0, 600))
                .with("duration", ratio(0, 600))
                .with("frameCount", Value::Integer(1))
                .with("Annt", Value::Integer(16))
                .with("Type", Value::Integer(2))
                .with("Trnf", quad(s, stored))
                .with("nonAffineTransform", quad(s, stored));
            if let Some((w, m)) = split {
                d = d.with("quiltWarp", quilt_desc(w, m, None));
            }
            let mut d = d
                .with("warp", warp)
                .with("Sz  ", Value::Descriptor(Descriptor::new("Pnt ").with("Wdth", Value::Double(s.size.0)).with("Hght", Value::Double(s.size.1))))
                .with("Rslt", unit(b"#Rsl", s.dpi));
            if let Some(fx) = &s.filter_fx {
                d = d.with("filterFX", Value::Descriptor(fx.clone()));
            }
            d.with("comp", Value::Integer(-1))
                .with("compInfo", Value::Descriptor(Descriptor::new("null").with("compID", Value::Integer(-1)).with("originalCompID", Value::Integer(-1))))
        }
    };
    let mut out = b"soLD".to_vec();
    out.extend_from_slice(&4u32.to_be_bytes());
    out.extend(VersionedDescriptor::new(d).to_bytes());
    pad4(out)
}

/// `PlLd` block data (`plcL` version 3): the legacy placed-layer record Photoshop writes next to
/// `SoLd`. It has no split warp: it stores none, its corners framed by the source rect.
pub fn plld_bytes(s: &PlacedSpec<'_>, warnings: &mut Vec<String>) -> Vec<u8> {
    let warp = s.warp.filter(|_| split_mesh(s.warp).is_none());
    let mut out = b"plcL".to_vec();
    out.extend_from_slice(&3u32.to_be_bytes());
    let id = s.idnt.as_bytes();
    let n = id.len().min(255);
    out.push(n as u8);
    out.extend_from_slice(&id[..n]);
    for v in [1i32, 1, 16, 2] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    if let Value::List(q) = quad(s, warp) {
        for v in q {
            if let Value::Double(x) = v {
                out.extend_from_slice(&x.to_be_bytes());
            }
        }
    }
    out.extend_from_slice(&0u32.to_be_bytes());
    let Value::Descriptor(w) = warp_desc(warp, s.size, warnings) else { return pad4(out) };
    out.extend(VersionedDescriptor::new(w).to_bytes());
    pad4(out)
}

/// The source size a `soLD` descriptor stores (`Sz  `).
pub fn stored_size(d: &Descriptor) -> Option<(f64, f64)> {
    let sz = get_desc(d, "Sz  ")?;
    Some((num(sz.get("Wdth"))?, num(sz.get("Hght"))?)).filter(|(w, h)| *w > 0.0 && *h > 0.0)
}

// ---------- the filter mask (`FEid`) ----------

/// The filter mask stored in an `FEid` item, in document coordinates and white beyond its bounds
/// (`filterMaskExtendWithWhite`), as a grayscale mask of `sample` depth. `Ok(None)` when the
/// item has no mask or it is all white (no mask); `Err` when it can't be decoded.
pub fn mask_from_item(item: &FilterEffectsItem, sample: SampleType, stack: &FilterStack) -> Result<Option<LayerMask>, String> {
    if item.mask.is_none() {
        return Ok(None);
    }
    let (rect, plane) = item.decoded_mask().ok_or("the smart filter mask could not be decoded")?;
    if item.depth != u32::from(crate::pixels::psd_depth(sample)) {
        return Err(format!("the smart filter mask is {}-bit in a {:?} document", item.depth, sample));
    }
    let (w, h) = rect.size().map_err(|e| e.to_string())?;
    let fmt = PixelFormat::new(ColorMode::Grayscale, sample, false);
    let mut surface = Surface::with_default(fmt, &[1.0]);
    let n = w.checked_mul(h).ok_or("the smart filter mask is too large")?;
    let bytes = crate::pixels::interleave(&[Some(&plane)], &[crate::pixels::zero_sample(sample)], n, sample, &[false]);
    surface.write_interleaved(GeomRect::new(rect.left, rect.top, rect.right, rect.bottom), &bytes);
    surface.prune();
    if surface.content_bounds().is_empty() {
        return Ok(None);
    }
    Ok(Some(LayerMask { surface, enabled: stack.mask_enabled, linked: stack.mask_linked, density: 1.0, feather: 0.0 }))
}

/// The unfiltered placed pixels of an `FEid` item (the inverse of [`feid_item`]), in `fmt`: colour
/// channels from slot 0, transparency from the last slot (opaque without it). `None` when the
/// item has no colour planes, its depth isn't `fmt`'s, or a plane doesn't decode.
pub fn unfiltered_from_item(item: &FilterEffectsItem, fmt: PixelFormat) -> Option<Surface> {
    let depth = u16::try_from(item.depth).ok()?;
    if u32::from(crate::pixels::psd_depth(fmt.sample)) != item.depth || !fmt.alpha {
        return None;
    }
    let (w, h) = item.rect.size().ok()?;
    let n = w.checked_mul(h)?;
    let cc = fmt.mode.color_channels();
    let mut planes = Vec::with_capacity(cc + 1);
    for slot in (0..cc).chain(std::iter::once(item.slots.len().saturating_sub(1))) {
        planes.push(match item.slots.get(slot).and_then(Option::as_ref) {
            Some(p) => Some(p.decode(w, h, depth).ok()?),
            None if slot < cc => return None,
            None => None,
        });
    }
    let refs: Vec<Option<&[u8]>> = planes.iter().map(|p| p.as_deref()).collect();
    let mut fill = vec![crate::pixels::zero_sample(fmt.sample); cc];
    // Without a transparency plane the pixels are opaque (a native-endian full sample).
    fill.push(match fmt.sample {
        SampleType::U8 => vec![u8::MAX],
        SampleType::U16 => u16::MAX.to_ne_bytes().to_vec(),
        SampleType::F32 => 1.0f32.to_ne_bytes().to_vec(),
    });
    let mut invert = vec![fmt.mode == ColorMode::Cmyk; cc];
    invert.push(false);
    let mut bytes = crate::pixels::interleave(&refs, &fill, n, fmt.sample, &invert);
    if fmt.mode == ColorMode::Lab && fmt.sample == SampleType::U16 {
        crate::pixels::lab16_chroma(&mut bytes, cc + 1, true);
    }
    let mut surface = Surface::new(fmt);
    surface.write_interleaved(GeomRect::new(item.rect.left, item.rect.top, item.rect.right, item.rect.bottom), &bytes);
    surface.prune();
    Some(surface)
}

/// An `FEid` item for a smart object with filters: the unfiltered placed pixels Photoshop
/// filters from (colour channels from slot 0, transparency in the last slot; in the document's
/// pixel format, CMYK inverted as in layer channels) and the filter mask over `bounds` (white
/// when `mask` is `None`). Photoshop needs the planes: re-rendering the filters of an item
/// without them crashes it. `None` when there are no unfiltered pixels.
pub fn feid_item(placed: &str, unfiltered: &Surface, mask: Option<&LayerMask>, bounds: GeomRect, doc_fmt: PixelFormat) -> Option<FilterEffectsItem> {
    let sample = doc_fmt.sample;
    let px = if unfiltered.format() == doc_fmt { unfiltered.clone() } else { unfiltered.convert(doc_fmt) };
    let content = px.content_bounds();
    if content.is_empty() {
        return None;
    }
    // Over the whole area the filters may draw into: Photoshop clips the filtered result to these
    // planes, so the content bounds alone would cut blurs off at the source's edge.
    let r = content.union(&bounds);
    let cc = doc_fmt.mode.color_channels();
    let depth = crate::pixels::psd_depth(sample);
    let compression = if sample == SampleType::F32 { Compression::ZipPrediction } else { Compression::Rle };
    let encode = |plane: &[u8], w: usize, h: usize| {
        EffectsPlane::encode(compression, plane, w, h, depth)
            .or_else(|_| EffectsPlane::encode(Compression::Raw, plane, w, h, depth))
            .unwrap_or(EffectsPlane { compression: Compression::Raw, data: plane.to_vec() })
    };
    let mut invert = vec![doc_fmt.mode == ColorMode::Cmyk; cc];
    invert.push(false);
    // Fully transparent pixels carry white, as in Photoshop's own cache: filters that look at
    // colour regardless of alpha (Shadows/Highlights) read it.
    let mut vals = px.read_region(r);
    let white = photocraft_raster::from_rgba(&doc_fmt, [1.0, 1.0, 1.0, 1.0]);
    for p in vals.chunks_exact_mut(cc + 1) {
        if p[cc] <= 0.0 {
            p[..cc].copy_from_slice(&white[..cc]);
        }
    }
    let mut filled = Surface::new(doc_fmt);
    filled.write_region(r, &vals);
    drop(vals);
    let mut bytes = filled.to_interleaved(r);
    if doc_fmt.mode == ColorMode::Lab && sample == SampleType::U16 {
        // 16-bit Lab a*/b* in Photoshop's scale, as in layer channels.
        crate::pixels::lab16_chroma(&mut bytes, cc + 1, false);
    }
    let planes = crate::pixels::deinterleave(&bytes, cc + 1, sample, &invert);
    let (w, h) = (r.width() as usize, r.height() as usize);
    let mut slots = vec![None; 26];
    for (i, p) in planes.iter().enumerate() {
        let slot = if i == cc { 25 } else { i };
        if let Some(s) = slots.get_mut(slot) {
            *s = Some(encode(p, w, h));
        }
    }
    let mut item = filter_mask_item(placed, mask, bounds, sample);
    item.rect = PsdRect { top: r.y0, left: r.x0, bottom: r.y1, right: r.x1 };
    item.slots = slots;
    Some(item)
}

fn filter_mask_item(placed: &str, mask: Option<&LayerMask>, bounds: GeomRect, sample: SampleType) -> FilterEffectsItem {
    let fmt = PixelFormat::new(ColorMode::Grayscale, sample, false);
    let area = match mask {
        Some(m) => m.surface.content_bounds().union(&bounds),
        None => bounds,
    };
    let area = if area.is_empty() { GeomRect::new(0, 0, 1, 1) } else { area };
    let rect = PsdRect { top: area.y0, left: area.x0, bottom: area.y1, right: area.x1 };
    let (w, h) = (area.width() as usize, area.height() as usize);
    let plane = match mask {
        Some(m) => {
            let s = if m.surface.format() == fmt { m.surface.clone() } else { m.surface.convert(fmt) };
            crate::pixels::deinterleave(&s.to_interleaved(area), 1, sample, &[false]).into_iter().next().unwrap_or_default()
        }
        None => {
            let mut white = Vec::with_capacity(w * h * sample.bytes());
            for _ in 0..w * h {
                crate::pixels::encode_be(1.0, sample, &mut white);
            }
            white
        }
    };
    let depth = crate::pixels::psd_depth(sample);
    let compression = if sample == SampleType::F32 { Compression::ZipPrediction } else { Compression::Rle };
    let plane = EffectsPlane::encode(compression, &plane, w, h, depth)
        .or_else(|_| EffectsPlane::encode(Compression::Raw, &plane, w, h, depth))
        .unwrap_or(EffectsPlane { compression: Compression::Raw, data: plane });
    FilterEffectsItem { id: placed.to_string(), version: 1, rect, depth: u32::from(depth), max_channels: 24, slots: vec![None; 26], mask: Some((rect, plane)) }
}

/// A deterministic uuid-shaped id (8-4-4-4-12 hex digits) derived from `seed`.
pub fn uuid_from(seed: &[u8]) -> String {
    let h = blake3::hash(seed);
    let x = to_hex(&h.as_bytes()[..16]);
    format!("{}-{}-{}-{}-{}", &x[..8], &x[8..12], &x[12..16], &x[16..20], &x[20..32])
}

#[cfg(test)]
mod tests;
