use photocraft_color::blend::BlendMode;
use photocraft_color::{ColorMode, PixelFormat, SampleType};
use photocraft_doc::{Adjustment, Document, Knockout, Layer, LayerContent, LayerMask};
use photocraft_geom::{Rect, Size};
use photocraft_raster::Surface;
use proptest::prelude::*;

use super::*;

const CMYKA8: PixelFormat = PixelFormat { mode: ColorMode::Cmyk, sample: SampleType::U8, alpha: true };
const CMYK8: PixelFormat = PixelFormat { mode: ColorMode::Cmyk, sample: SampleType::U8, alpha: false };

fn doc(w: u32, h: u32) -> Document {
    Document::new("c", Size::new(w, h), ColorMode::Cmyk, SampleType::U8)
}

fn layer(rects: &[(Rect, Cmyka)]) -> Layer {
    let mut l = Layer::raster("l", CMYKA8);
    for (r, c) in rects {
        if let Some(s) = l.surface_mut() {
            s.fill_rect(*r, c);
        }
    }
    l
}

/// An opaque layer without alpha (as a flattened Background is).
fn paper(w: i32, h: i32, c: [f32; 4]) -> Layer {
    let mut l = Layer::raster("bg", CMYK8);
    if let Some(s) = l.surface_mut() {
        s.fill_rect(Rect::new(0, 0, w, h), &c);
    }
    l
}

fn mask(rects: &[(Rect, f32)], default: f32, density: f32, enabled: bool) -> LayerMask {
    let mut surface = Surface::with_default(PixelFormat::GRAY8, &[default]);
    for (r, v) in rects {
        surface.fill_rect(*r, &[*v]);
    }
    LayerMask { surface, enabled, linked: true, density, feather: 0.0 }
}

/// An 8-bit sample as stored.
fn q(v: f32) -> f32 {
    (v.clamp(0.0, 1.0) * 255.0).round() / 255.0
}

// ---- an independent per-pixel reference -------------------------------------------------------

/// Porter-Duff source-over with straight alpha, written out separately from the compositor.
fn over(b: Cmyka, s: Cmyka, opacity: f32) -> Cmyka {
    let sa = s[4] * opacity;
    let a = sa + b[4] * (1.0 - sa);
    if sa <= 0.0 {
        return b;
    }
    if a <= 0.0 {
        return [0.0; 5];
    }
    let mut o = [0.0; 5];
    for i in 0..4 {
        o[i] = (s[i] * sa + b[i] * b[4] * (1.0 - sa)) / a;
    }
    o[4] = a;
    o
}

fn ref_mask(l: &Layer, x: i32, y: i32) -> f32 {
    l.mask.as_ref().map_or(1.0, |m| if m.enabled { 1.0 - m.density * (1.0 - m.surface.sample_channel(x, y, 0)) } else { 1.0 })
}

fn ref_own(l: &Layer, x: i32, y: i32) -> Cmyka {
    let s = match &l.content {
        LayerContent::Fill(f) => match &l.fill_cache {
            Some(c) if c.fill == *f => Some(&c.surface),
            _ => {
                // A solid colour's inks and alpha, stored at the (8-bit) document's levels.
                let photocraft_doc::Fill::Solid(color) = f else { return [0.0; 5] };
                let c = color.in_mode(ColorMode::Cmyk);
                return [q(c.c[0]), q(c.c[1]), q(c.c[2]), q(c.c[3]), q(color.alpha) * ref_mask(l, x, y)];
            }
        },
        _ => l.surface(),
    };
    let Some(s) = s else { return [0.0; 5] };
    let p = s.pixel(x, y);
    let a = if s.format().alpha { p[4] } else { 1.0 };
    [p[0], p[1], p[2], p[3], a * ref_mask(l, x, y)]
}

/// Source-over with coverage mixed in the type gamma, on brightness (1 − ink), worked out from
/// the colour transfer functions directly.
fn over_text(b: Cmyka, s: Cmyka, opacity: f32) -> Cmyka {
    let gamma = crate::psblend::text_gamma();
    let sa = s[4] * opacity;
    if sa <= 0.0 {
        return b;
    }
    let a = sa + b[4] * (1.0 - sa);
    if a <= 0.0 {
        return [0.0; 5];
    }
    let enc = |ink: f32| photocraft_color::convert::srgb_to_linear((1.0 - ink).max(0.0)).powf(1.0 / gamma);
    let dec = |v: f32| 1.0 - photocraft_color::convert::linear_to_srgb(v.max(0.0).powf(gamma));
    let mut o = [0.0; 5];
    for i in 0..4 {
        // Mixing an ink with itself (or with nothing) leaves it exactly as it is.
        o[i] = if b[4] <= 0.0 || b[i] == s[i] { s[i] } else { dec((1.0 - sa) * b[4] / a * enc(b[i]) + sa / a * enc(s[i])) };
    }
    o[4] = a;
    o
}

/// The blended ink of one channel: the blend function on brightness, back to ink.
fn ref_blend(mode: BlendMode, b: f32, s: f32) -> f32 {
    1.0 - crate::psblend::blend_rgb(mode, [1.0 - b; 3], [1.0 - s; 3])[0]
}

/// The general compositing formula (W3C / PDF), written out on each ink.
fn over_mode(mode: BlendMode, b: Cmyka, s: Cmyka, opacity: f32, text: bool) -> Cmyka {
    if mode == BlendMode::Normal {
        return if text { over_text(b, s, opacity) } else { over(b, s, opacity) };
    }
    let sa = s[4] * opacity;
    if sa <= 0.0 {
        return b;
    }
    let a = sa + b[4] * (1.0 - sa);
    if a <= 0.0 {
        return [0.0; 5];
    }
    let gamma = if text { crate::psblend::text_gamma() } else { 1.0 };
    let enc = |ink: f32| photocraft_color::convert::srgb_to_linear((1.0 - ink).max(0.0)).powf(1.0 / gamma);
    let dec = |v: f32| 1.0 - photocraft_color::convert::linear_to_srgb(v.max(0.0).powf(gamma));
    let (wb, ws, wbs) = ((1.0 - sa) * b[4] / a, (1.0 - b[4]) * sa / a, sa * b[4] / a);
    let mut o = [0.0; 5];
    for i in 0..4 {
        let m = ref_blend(mode, b[i], s[i]);
        o[i] = if text { dec(wb * enc(b[i]) + ws * enc(s[i]) + wbs * enc(m)) } else { wb * b[i] + ws * s[i] + wbs * m };
    }
    o[4] = a;
    o
}

/// `l` onto the backdrop in its blend mode (Dissolve: every pixel shown fully or not at all).
fn over_layer(l: &Layer, b: Cmyka, s: Cmyka, opacity: f32, at: Option<(i32, i32)>) -> Cmyka {
    if let (BlendMode::Dissolve, Some((x, y))) = (l.blend, at) {
        let shown = crate::dissolve_noise(x, y) < s[4] * opacity;
        return over(b, [s[0], s[1], s[2], s[3], if shown { 1.0 } else { 0.0 }], 1.0);
    }
    over_mode(l.blend, b, s, opacity, matches!(l.content, LayerContent::Text(_)))
}

/// Photoshop's special eight below 100% Fill, written out on each ink: the source pulled toward
/// the mode's neutral colour is blended at full strength (`psblend::blend_rgb_fill`, on
/// brightness), Opacity is its coverage over the backdrop, Fill × Opacity over nothing.
fn over_special_fill(l: &Layer, b: Cmyka, s: Cmyka) -> Cmyka {
    let f = l.fill_opacity.clamp(0.0, 1.0);
    let over = s[4] * l.opacity;
    if over <= 0.0 {
        return b;
    }
    let a = b[4] + (1.0 - b[4]) * over * f;
    if a <= 0.0 {
        return [0.0; 5];
    }
    let text = matches!(l.content, LayerContent::Text(_));
    let gamma = crate::psblend::text_gamma();
    let enc = |v: f32| if text { photocraft_color::convert::srgb_to_linear(v.max(0.0)).powf(1.0 / gamma) } else { v };
    let dec = |v: f32| if text { photocraft_color::convert::linear_to_srgb(v.max(0.0).powf(gamma)) } else { v };
    let (wb, wbs, ws) = (b[4] * (1.0 - over) / a, b[4] * over / a, (1.0 - b[4]) * over * f / a);
    let mut o = [0.0; 5];
    for i in 0..4 {
        let (bb, sb) = (1.0 - b[i], 1.0 - s[i]);
        let m = crate::psblend::blend_rgb_fill(l.blend, [bb; 3], [sb; 3], f)[0];
        o[i] = 1.0 - dec(wb * enc(bb) + wbs * enc(m) + ws * enc(sb));
    }
    o[4] = a;
    o
}

/// An adjustment on one pixel: each ink's brightness through the channel's table (Levels and
/// Curves: the compositor's tables for an 8-bit document, interpolated here), Invert and
/// Posterize written out.
fn ref_adjust(adj: &Adjustment, p: Cmyka) -> Cmyka {
    if p[4] <= 0.0 {
        return p;
    }
    let luts = crate::adjust::tone_luts_depth(adj, Some(SampleType::U8));
    let mut o = p;
    for c in 0..4 {
        let v = 1.0 - p[c];
        let r = match adj {
            Adjustment::Invert => 1.0 - v,
            Adjustment::Posterize { levels } => {
                let n = (*levels).clamp(2, 255) as f32;
                let bin = ((v * 255.0).round() * n / 256.0).floor().min(n - 1.0);
                (bin * 255.0 / (n - 1.0)).floor() / 255.0
            }
            _ => {
                let t = &luts[c];
                let x = v.clamp(0.0, 1.0) * (t.len() - 1) as f32;
                let i = x.floor() as usize;
                let j = (i + 1).min(t.len() - 1);
                t[i] + (t[j] - t[i]) * (x - i as f32)
            }
        };
        o[c] = 1.0 - r;
    }
    o
}

/// An adjustment layer `l` (adjusting `p`, then `clipped` onto that) mixed into `p` in its blend
/// mode by its opacity and mask; coverage stays.
fn ref_adjusted(l: &Layer, adj: &Adjustment, clipped: &[Layer], p: Cmyka, x: i32, y: i32) -> Cmyka {
    let mut a = ref_adjust(adj, p);
    if p[4] > 0.0 {
        a = ref_clip(clipped, a, x, y);
    }
    let k = l.opacity * l.fill_opacity * ref_mask(l, x, y);
    let mut o = p;
    for c in 0..4 {
        o[c] = p[c] + (ref_blend(l.blend, p[c], a[c]) - p[c]) * k;
    }
    o
}

/// `clipped` onto `content` (blending as if it were opaque; its coverage stays).
fn ref_clip(clipped: &[Layer], mut content: Cmyka, x: i32, y: i32) -> Cmyka {
    for c in clipped.iter().filter(|c| c.visible) {
        if let LayerContent::Adjustment(adj) = &c.content {
            content = ref_adjusted(c, adj, &[], content, x, y);
        } else if content[4] > 0.0 {
            let a = content[4];
            let r = over_layer(c, [content[0], content[1], content[2], content[3], 1.0], ref_own(c, x, y), c.opacity * c.fill_opacity, None);
            content = [r[0], r[1], r[2], r[3], a];
        }
    }
    content
}

/// Rounding to an 8-bit document's levels: stored channel brightness, and coverage.
fn ref_round(p: Cmyka) -> Cmyka {
    let r = |v: f32| (v * 255.0 + 0.5).floor() / 255.0;
    [1.0 - r(1.0 - p[0]), 1.0 - r(1.0 - p[1]), 1.0 - r(1.0 - p[2]), 1.0 - r(1.0 - p[3]), r(p[4])]
}

fn ref_stack(layers: &[Layer], x: i32, y: i32, mut b: Cmyka) -> Cmyka {
    let mut i = 0;
    while i < layers.len() {
        let base = &layers[i];
        let mut j = i + 1;
        while j < layers.len() && layers[j].clipped && !base.clipped {
            j += 1;
        }
        if base.visible {
            b = ref_layer(base, &layers[i + 1..j], x, y, b);
            if matches!(base.content, LayerContent::Adjustment(_)) {
                b = ref_round(b);
            }
        }
        i = j;
    }
    b
}

fn ref_layer(l: &Layer, clipped: &[Layer], x: i32, y: i32, b: Cmyka) -> Cmyka {
    let op = l.opacity * l.fill_opacity;
    let mut content = match &l.content {
        LayerContent::Group(g) if l.blend == BlendMode::PassThrough && l.fill_opacity >= 1.0 => {
            let r = ref_stack(&g.children, x, y, b);
            if op >= 1.0 && l.mask.is_none() {
                return r;
            }
            let k = op * ref_mask(l, x, y);
            let (wa, wb) = (b[4] * (1.0 - k), r[4] * k);
            let a = wa + wb;
            if a <= 0.0 {
                return [0.0; 5];
            }
            let mut o = [0.0; 5];
            for c in 0..4 {
                o[c] = (b[c] * wa + r[c] * wb) / a;
            }
            o[4] = a;
            return o;
        }
        LayerContent::Adjustment(adj) => return ref_adjusted(l, adj, clipped, b, x, y),
        LayerContent::Group(g) => {
            let mut c = ref_stack(&g.children, x, y, [0.0; 5]);
            c[4] *= ref_mask(l, x, y);
            c
        }
        _ => ref_own(l, x, y),
    };
    content = ref_clip(clipped, content, x, y);
    // A layer's own blend (not a group's) for the special eight below 100% Fill.
    if l.fill_opacity < 1.0 && crate::psblend::fill_is_special(l.blend) && !matches!(l.content, LayerContent::Group(_)) {
        return over_special_fill(l, b, content);
    }
    over_layer(l, b, content, op, Some((x, y)))
}

fn assert_matches_reference(d: &Document) {
    let rect = Rect::new(-1, -1, d.size.width as i32 + 1, d.size.height as i32 + 1);
    let Some(got) = render_cmyk(d, rect) else { panic!("supported") };
    let rgb = crate::render(d, rect);
    for y in rect.y0..rect.y1 {
        for x in rect.x0..rect.x1 {
            let i = ((y - rect.y0) * rect.width() as i32 + (x - rect.x0)) as usize;
            let (g, want) = (got.px[i], ref_stack(&d.layers, x, y, [0.0; 5]));
            assert!(g.iter().all(|v| v.is_finite()), "({x},{y}) {g:?}");
            // Ink only means something where there is coverage.
            let close = |a: f32, b: f32| (a - b).abs() <= 1e-5;
            assert!(close(g[4], want[4]), "alpha ({x},{y}): {g:?} vs reference {want:?}");
            if want[4] > 1e-4 {
                assert!((0..4).all(|c| close(g[c], want[c])), "ink ({x},{y}): {g:?} vs reference {want:?}");
            }
            // Coverage doesn't depend on the colour model: the RGB compositor agrees.
            assert!((g[4] - rgb.px[i][3]).abs() <= 1e-5, "alpha vs RGB ({x},{y}): {g:?} vs {:?}", rgb.px[i]);
        }
    }
}

// ---- directed tests ---------------------------------------------------------------------------

#[test]
fn opaque_pixels_keep_their_exact_inks() {
    let mut d = doc(6, 4);
    d.layers.push(paper(6, 4, [0.0, 0.0, 0.0, 0.0]));
    // K-only black "type", a rich four-colour patch, then a half-covered patch on top.
    d.layers.push(layer(&[(Rect::new(0, 0, 3, 4), [0.0, 0.0, 0.0, 1.0, 1.0])]));
    d.layers.push(layer(&[(Rect::new(3, 0, 6, 2), [0.2, 0.4, 0.6, 0.1, 1.0])]));
    let got = render_cmyk(&d, d.bounds()).unwrap_or_else(|| panic!("supported"));
    let at = |x: i32, y: i32| got.px[(y * 6 + x) as usize];
    assert_eq!(at(1, 1), [0.0, 0.0, 0.0, 1.0, 1.0], "K stays K-only");
    assert_eq!(at(4, 1), [q(0.2), q(0.4), q(0.6), q(0.1), 1.0], "every ink kept as stored");
    assert_eq!(at(4, 3), [0.0, 0.0, 0.0, 0.0, 1.0], "paper: no ink");
}

#[test]
fn partial_coverage_mixes_ink_values() {
    let mut d = doc(2, 1);
    d.layers.push(paper(2, 1, [0.0, 0.0, 0.0, 0.0]));
    let mut l = layer(&[(Rect::new(0, 0, 2, 1), [0.0, 0.0, 0.0, 1.0, 1.0])]);
    l.opacity = 0.5;
    l.fill_opacity = 0.5;
    d.layers.push(l);
    let got = render_cmyk(&d, d.bounds()).unwrap_or_else(|| panic!("supported"));
    // 25% coverage of K100 over paper: K25 and no other ink.
    assert!((got.px[0][3] - 0.25).abs() < 1e-6 && got.px[0][..3] == [0.0; 3], "{:?}", got.px[0]);
}

#[test]
fn masks_groups_and_clipping_match_the_reference() {
    let mut d = doc(8, 6);
    d.layers.push(paper(8, 6, [0.1, 0.0, 0.0, 0.0]));
    let mut base = layer(&[(Rect::new(1, 1, 7, 5), [0.0, 0.5, 0.0, 0.0, 0.8])]);
    base.mask = Some(mask(&[(Rect::new(0, 0, 4, 6), 0.0)], 1.0, 0.75, true));
    let mut clip = layer(&[(Rect::new(0, 0, 8, 3), [0.0, 0.0, 1.0, 0.2, 1.0])]);
    clip.clipped = true;
    clip.opacity = 0.6;
    let mut iso = Layer::group("iso", vec![layer(&[(Rect::new(2, 0, 5, 6), [0.3, 0.3, 0.3, 0.3, 0.5])])]);
    iso.blend = BlendMode::Normal;
    iso.opacity = 0.7;
    iso.mask = Some(mask(&[(Rect::new(0, 3, 8, 6), 0.25)], 1.0, 1.0, true));
    let mut pass = Layer::group("pass", vec![layer(&[(Rect::new(5, 2, 8, 6), [1.0, 0.0, 0.0, 0.0, 1.0])])]);
    pass.opacity = 0.5;
    pass.mask = Some(mask(&[(Rect::new(6, 0, 8, 6), 0.0)], 1.0, 1.0, true));
    d.layers.extend([base, clip, iso, pass]);
    assert_matches_reference(&d);
}

#[test]
fn unsupported_documents_fall_back() {
    let simple = || {
        let mut d = doc(2, 2);
        d.layers.push(layer(&[(Rect::new(0, 0, 2, 2), [0.0, 0.0, 0.0, 1.0, 1.0])]));
        d
    };
    assert!(supports(&simple()));
    let check = |what: &str, f: &dyn Fn(&mut Layer)| {
        let mut d = simple();
        let mut l = layer(&[(Rect::new(0, 0, 1, 1), [0.5; 5])]);
        f(&mut l);
        d.layers.push(l);
        assert!(!supports(&d), "{what}");
        assert!(render_cmyk(&d, d.bounds()).is_none(), "{what}");
        // An invisible layer doesn't matter.
        if let Some(l) = d.layers.last_mut() {
            l.visible = false;
        }
        assert!(supports(&d), "hidden {what}");
    };
    check("hue", &|l| l.blend = BlendMode::Hue);
    check("luminosity", &|l| l.blend = BlendMode::Luminosity);
    check("lighter colour", &|l| l.blend = BlendMode::LighterColor);
    check("knockout", &|l| l.advanced.knockout = Knockout::Shallow);
    check("channels", &|l| l.excluded_channels = 1);
    check("nan opacity", &|l| l.opacity = f32::NAN);
    // Adjustments that weigh the channels together, or whose records name RGB channels.
    check("threshold", &|l| l.content = LayerContent::Adjustment(Adjustment::Threshold { level: 0.5 }));
    check("rgb levels", &|l| l.content = LayerContent::Adjustment(rgb_levels()));
    check("invert in colour mode", &|l| {
        l.content = LayerContent::Adjustment(Adjustment::Invert);
        l.blend = BlendMode::Color;
    });
    check("rgb pixels", &|l| l.content = LayerContent::Raster(Surface::new(PixelFormat::RGBA8)));
    // Not a CMYK document.
    let mut rgb = simple();
    rgb.mode = ColorMode::Rgb;
    assert!(!supports(&rgb));
    // Too deep: refused without recursing through every level.
    let mut deep = layer(&[(Rect::new(0, 0, 1, 1), [0.5; 5])]);
    for _ in 0..(MAX_DEPTH + 8) {
        deep = Layer::group("g", vec![deep]);
    }
    let mut d = simple();
    d.layers.push(deep);
    assert!(!supports(&d));
}

#[test]
fn non_finite_samples_never_reach_the_output() {
    let fmt = PixelFormat { mode: ColorMode::Cmyk, sample: SampleType::F32, alpha: true };
    let mut d = Document::new("f", Size::new(4, 1), ColorMode::Cmyk, SampleType::F32);
    let mut l = Layer::raster("bad", fmt);
    if let Some(s) = l.surface_mut() {
        s.write_region(
            Rect::new(0, 0, 4, 1),
            &[
                f32::NAN,
                0.0,
                0.0,
                0.0,
                1.0, //
                0.0,
                f32::INFINITY,
                0.0,
                0.0,
                1.0, //
                0.0,
                0.0,
                f32::NEG_INFINITY,
                0.0,
                f32::NAN, //
                0.5,
                0.5,
                0.5,
                0.5,
                0.5,
            ],
        );
    }
    d.layers.push(l);
    let got = render_cmyk(&d, Rect::new(-2, -2, 6, 3)).unwrap_or_else(|| panic!("supported"));
    assert!(got.px.iter().flatten().all(|v| v.is_finite()), "{:?}", got.px);
}

#[test]
fn empty_and_off_canvas_rects() {
    let mut d = doc(4, 4);
    d.layers.push(layer(&[(Rect::new(0, 0, 4, 4), [0.0, 0.0, 0.0, 1.0, 1.0])]));
    assert!(render_cmyk(&d, Rect::new(2, 2, 2, 2)).is_some_and(|b| b.px.is_empty()));
    let far = render_cmyk(&d, Rect::new(1000, 1000, 1003, 1002)).unwrap_or_else(|| panic!("supported"));
    assert_eq!(far.px, vec![[0.0; 5]; 6]);
}

#[test]
fn bands_equal_the_whole_render() {
    let mut d = doc(300, 700);
    d.layers.push(paper(300, 700, [0.0, 0.1, 0.0, 0.0]));
    d.layers.push(layer(&[(Rect::new(10, 20, 290, 690), [0.4, 0.0, 0.2, 0.9, 0.6])]));
    let whole = render_cmyk(&d, d.bounds()).unwrap_or_else(|| panic!("supported"));
    let mut joined = Vec::new();
    let r = render_cmyk_bands(&d, d.bounds(), 256, |b: CmykBuffer| -> Result<(), ()> {
        joined.extend(b.px);
        Ok(())
    });
    assert_eq!(r, Some(Ok(())));
    assert_eq!(joined, whole.px);
}

// ---- random documents -------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Spec {
    w: i32,
    h: i32,
    layers: Vec<LayerSpec>,
}

#[derive(Debug, Clone)]
enum LayerSpec {
    Pixels { kind: Kind, blend: BlendMode, fills: Vec<(Rect, Cmyka)>, opacity: f32, fill: f32, clipped: bool, visible: bool, mask: Option<MaskSpec> },
    Group { children: Vec<LayerSpec>, pass_through: bool, blend: BlendMode, opacity: f32, fill: f32, mask: Option<MaskSpec> },
}

/// What kind of layer a [`LayerSpec::Pixels`] becomes: its pixels as a pixel layer, as a type or
/// shape layer's rendered pixels, or a solid colour fill in the first fill's colour.
#[derive(Debug, Clone)]
enum Kind {
    Raster,
    Text,
    Shape,
    Solid,
    /// An adjustment layer (its fills are unused).
    Adjust(AdjSpec),
}

/// The adjustments this path applies on ink values; `channel` 0..=3 is C, M, Y, K and 4 the
/// composite record.
#[derive(Debug, Clone)]
enum AdjSpec {
    Invert,
    Posterize(u32),
    Levels { channel: usize, levels: [f32; 5] },
    Curves { channel: usize, points: Vec<(f32, f32)> },
}

#[derive(Debug, Clone)]
struct MaskSpec {
    rects: Vec<(Rect, f32)>,
    default: f32,
    density: f32,
    enabled: bool,
}

/// Every blend mode this path composites natively, Normal most often.
fn blend_mode() -> impl Strategy<Value = BlendMode> {
    prop_oneof![
        6 => Just(BlendMode::Normal),
        1 => prop::sample::select(SEPARABLE.to_vec()),
    ]
}

fn adj_spec() -> impl Strategy<Value = AdjSpec> {
    prop_oneof![
        Just(AdjSpec::Invert),
        (2u32..9).prop_map(AdjSpec::Posterize),
        (0usize..5, 0.0f32..0.4, 0.6f32..=1.0, 0.3f32..3.0, 0.0f32..0.3, 0.7f32..=1.0)
            .prop_map(|(channel, ib, iw, g, ob, ow)| AdjSpec::Levels { channel, levels: [ib, iw, g, ob, ow] }),
        (0usize..5, prop::collection::vec((unit(), unit()), 2..5)).prop_map(|(channel, mut points)| {
            points.sort_by(|a, b| a.0.total_cmp(&b.0));
            points.dedup_by(|a, b| (a.0 - b.0).abs() < 0.02);
            AdjSpec::Curves { channel, points }
        }),
    ]
}

fn make_adjustment(a: &AdjSpec) -> Adjustment {
    use photocraft_doc::adjust::{CurvePoint, LevelsChannel, ToneSpace};
    match a {
        AdjSpec::Invert => Adjustment::Invert,
        AdjSpec::Posterize(levels) => Adjustment::Posterize { levels: *levels },
        AdjSpec::Levels { channel, levels: [in_black, in_white, gamma, out_black, out_white] } => {
            let rec = LevelsChannel { in_black: *in_black, in_white: *in_white, gamma: *gamma, out_black: *out_black, out_white: *out_white };
            let pick = |c: usize| if c == *channel { rec.clone() } else { LevelsChannel::default() };
            Adjustment::Levels { master: pick(4), per_channel: [pick(0), pick(1), pick(2)], space: ToneSpace::Cmyk, black: pick(3) }
        }
        AdjSpec::Curves { channel, points } => {
            let curve: Vec<CurvePoint> = points.iter().map(|(input, output)| CurvePoint { input: *input, output: *output }).collect();
            let pick = |c: usize| if c == *channel { curve.clone() } else { Vec::new() };
            Adjustment::Curves { master: pick(4), per_channel: [pick(0), pick(1), pick(2)], space: ToneSpace::Cmyk, black: pick(3) }
        }
    }
}

fn unit() -> impl Strategy<Value = f32> {
    prop_oneof![Just(0.0f32), Just(1.0f32), 0.0f32..=1.0]
}

fn rect(w: i32, h: i32) -> impl Strategy<Value = Rect> {
    (-1..w, -1..h, 1..w + 2, 1..h + 2).prop_map(|(x, y, rw, rh)| Rect::new(x, y, x + rw, y + rh))
}

fn mask_spec(w: i32, h: i32) -> impl Strategy<Value = Option<MaskSpec>> {
    prop::option::of(
        (prop::collection::vec((rect(w, h), unit()), 0..3), unit(), unit(), any::<bool>()).prop_map(|(rects, default, density, enabled)| MaskSpec {
            rects,
            default,
            density,
            enabled,
        }),
    )
}

fn pixels(w: i32, h: i32) -> impl Strategy<Value = LayerSpec> {
    (
        prop::collection::vec((rect(w, h), (unit(), unit(), unit(), unit(), unit())), 1..4),
        unit(),
        unit(),
        any::<bool>(),
        prop::bool::weighted(0.9),
        mask_spec(w, h),
        prop_oneof![
            3 => prop_oneof![Just(Kind::Raster), Just(Kind::Text), Just(Kind::Shape), Just(Kind::Solid)],
            1 => adj_spec().prop_map(Kind::Adjust),
        ],
        blend_mode(),
    )
        .prop_map(|(fills, opacity, fill, clipped, visible, mask, kind, blend)| LayerSpec::Pixels {
            kind,
            blend,
            fills: fills.into_iter().map(|(r, (c, m, y, k, a))| (r, [c, m, y, k, a])).collect(),
            opacity,
            fill,
            clipped,
            visible,
            mask,
        })
}

fn layer_spec(w: i32, h: i32) -> impl Strategy<Value = LayerSpec> {
    pixels(w, h).prop_recursive(3, 12, 4, move |inner| {
        (prop::collection::vec(inner, 1..4), any::<bool>(), unit(), prop_oneof![Just(1.0f32), unit()], mask_spec(w, h), blend_mode())
            .prop_map(move |(children, pass_through, opacity, fill, mask, blend)| LayerSpec::Group { children, pass_through, blend, opacity, fill, mask })
    })
}

fn spec() -> impl Strategy<Value = Spec> {
    (2i32..9, 2i32..7).prop_flat_map(|(w, h)| prop::collection::vec(layer_spec(w, h), 1..6).prop_map(move |layers| Spec { w, h, layers }))
}

fn build(s: &LayerSpec) -> Layer {
    let make_mask = |m: &MaskSpec| mask(&m.rects, m.default, m.density, m.enabled);
    match s {
        LayerSpec::Pixels { kind, blend, fills, opacity, fill, clipped, visible, mask } => {
            let pixels = layer(fills);
            let mut l = match kind {
                Kind::Raster => pixels,
                Kind::Text => Layer::new("t", LayerContent::Text(photocraft_doc::TextLayer { cache: pixels.surface().cloned(), ..Default::default() })),
                Kind::Shape => Layer::new("s", LayerContent::Shape(photocraft_doc::ShapeLayer { cache: pixels.surface().cloned(), ..Default::default() })),
                Kind::Solid => {
                    let [c, m, y, k, a] = fills.first().map_or([0.0; 5], |f| f.1);
                    let color = photocraft_color::Color { mode: ColorMode::Cmyk, c: [c, m, y, k], alpha: a };
                    Layer::new("f", LayerContent::Fill(photocraft_doc::Fill::Solid(color)))
                }
                Kind::Adjust(a) => Layer::new("a", LayerContent::Adjustment(make_adjustment(a))),
            };
            (l.opacity, l.fill_opacity, l.clipped, l.visible) = (*opacity, *fill, *clipped, *visible);
            l.blend = *blend;
            l.mask = mask.as_ref().map(make_mask);
            l
        }
        LayerSpec::Group { children, pass_through, blend, opacity, fill, mask } => {
            let mut g = Layer::group("g", children.iter().map(build).collect());
            g.blend = if *pass_through { BlendMode::PassThrough } else { *blend };
            (g.opacity, g.fill_opacity) = (*opacity, *fill);
            g.mask = mask.as_ref().map(make_mask);
            g
        }
    }
}

/// Clipped layers onto a pass-through group take the RGB compositor's isolated-rendering path,
/// which this path leaves alone: such specs are filtered out.
fn clips_onto_pass_through(layers: &[LayerSpec]) -> bool {
    layers.windows(2).any(|w| {
        matches!(w[0], LayerSpec::Group { pass_through: true, fill, .. } if fill >= 1.0)
            && matches!(w[1], LayerSpec::Pixels { clipped: true, visible: true, .. })
    }) || layers.iter().any(|l| matches!(l, LayerSpec::Group { children, .. } if clips_onto_pass_through(children)))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    /// Random stacks of pixel layers, masks, isolated and pass-through groups and clipping
    /// groups: the native composite equals the independent per-pixel reference, and its
    /// coverage equals the RGB compositor's.
    #[test]
    fn random_documents_match_the_reference(s in spec()) {
        prop_assume!(!clips_onto_pass_through(&s.layers));
        let mut d = doc(s.w as u32, s.h as u32);
        d.layers = s.layers.iter().map(build).collect();
        prop_assume!(supports(&d));
        assert_matches_reference(&d);
    }

    /// Tiling never changes the result (tiles render independently, in any order).
    #[test]
    fn tiling_is_invisible(s in spec(), tile in 1i32..9) {
        prop_assume!(!clips_onto_pass_through(&s.layers));
        let mut d = doc(s.w as u32, s.h as u32);
        d.layers = s.layers.iter().map(build).collect();
        prop_assume!(supports(&d));
        let whole = render_cmyk_tiled(&d, d.bounds(), 1024);
        prop_assert_eq!(render_cmyk_tiled(&d, d.bounds(), tile), whole);
    }
}

/// Where the composite is a single opaque layer's pixel (opaque, full opacity, binary masks),
/// the native ink, shown through the document's profile, is the colour the RGB compositor shows.
#[test]
fn opaque_results_look_the_same_as_the_rgb_compositor() {
    let mut d = doc(8, 4);
    d.layers.push(paper(8, 4, [0.0, 0.0, 0.0, 0.0]));
    d.layers.push(layer(&[(Rect::new(0, 0, 4, 4), [0.0, 0.0, 0.0, 1.0, 1.0]), (Rect::new(4, 0, 8, 2), [0.7, 0.1, 0.0, 0.0, 1.0])]));
    let mut top = layer(&[(Rect::new(2, 1, 7, 4), [0.0, 0.9, 0.9, 0.0, 1.0])]);
    top.mask = Some(mask(&[(Rect::new(5, 0, 8, 4), 0.0)], 1.0, 1.0, true));
    d.layers.push(top);
    let native = render_cmyk(&d, d.bounds()).unwrap_or_else(|| panic!("supported"));
    let rgb = crate::render(&d, d.bounds());
    let space = crate::cmyk_space(&d);
    let fmt = PixelFormat { mode: ColorMode::Cmyk, sample: SampleType::F32, alpha: true };
    photocraft_color::convert::with_cmyk_space(space.as_ref(), || {
        for (n, r) in native.px.iter().zip(&rgb.px) {
            let shown = photocraft_raster::to_rgba(&fmt, n);
            assert!((0..4).all(|c| (shown[c] - r[c]).abs() <= 1e-4), "native {n:?} shows {shown:?}, RGB compositor {r:?}");
        }
    });
}

// ---- stage 2: type, shape, smart object and fill layers ----------------------------------------

fn text_layer(rects: &[(Rect, Cmyka)]) -> Layer {
    let cache = layer(rects).surface().cloned();
    Layer::new("t", LayerContent::Text(photocraft_doc::TextLayer { cache, ..Default::default() }))
}

fn cmyk_color(c: [f32; 4], alpha: f32) -> photocraft_color::Color {
    photocraft_color::Color { mode: ColorMode::Cmyk, c, alpha }
}

/// K-only type keeps K-only glyphs: fully covered pixels are exactly K100, and anti-aliased edges
/// mix on brightness in the type gamma, adding no cyan, magenta or yellow.
#[test]
fn type_interiors_are_exact_and_edges_add_no_other_ink() {
    let mut d = doc(3, 1);
    d.layers.push(paper(3, 1, [0.0; 4]));
    d.layers.push(text_layer(&[(Rect::new(0, 0, 1, 1), [0.0, 0.0, 0.0, 1.0, 1.0]), (Rect::new(1, 0, 2, 1), [0.0, 0.0, 0.0, 1.0, 0.5])]));
    let got = render_cmyk(&d, d.bounds()).unwrap_or_else(|| panic!("supported"));
    assert_eq!(got.px[0], [0.0, 0.0, 0.0, 1.0, 1.0], "glyph interior");
    let edge = got.px[1];
    assert_eq!(edge[..3], [0.0; 3], "an edge adds no CMY: {edge:?}");
    let want = over_text([0.0, 0.0, 0.0, 0.0, 1.0], [0.0, 0.0, 0.0, 1.0, q(0.5)], 1.0);
    assert!((edge[3] - want[3]).abs() < 1e-6, "{edge:?} vs {want:?}");
    // The gamma lightens dark edges compared with plain mixing.
    assert!(edge[3] < q(0.5), "{edge:?}");
    assert_eq!(got.px[2], [0.0, 0.0, 0.0, 0.0, 1.0], "paper");
}

#[test]
fn solid_and_rendered_fills_keep_their_inks() {
    let mut d = doc(4, 1);
    d.layers.push(paper(4, 1, [0.0; 4]));
    let mut solid = Layer::new("f", LayerContent::Fill(photocraft_doc::Fill::Solid(cmyk_color([0.1, 0.2, 0.3, 0.4], 1.0))));
    solid.mask = Some(mask(&[(Rect::new(2, 0, 4, 1), 0.0)], 1.0, 1.0, true));
    d.layers.push(solid);
    // A gradient fill carrying the pixels it was rendered to (as a PSD supplies them).
    let gradient = photocraft_doc::Fill::gradient(
        vec![(0.0, cmyk_color([1.0, 0.0, 0.0, 0.0], 1.0)), (1.0, cmyk_color([0.0, 0.0, 0.0, 1.0], 1.0))],
        0.0,
        1.0,
        Default::default(),
        false,
    );
    let rendered = layer(&[(Rect::new(3, 0, 4, 1), [0.9, 0.0, 0.5, 0.0, 1.0])]);
    let mut cached = Layer::new("g", LayerContent::Fill(gradient.clone()));
    cached.fill_cache = rendered.surface().map(|s| photocraft_doc::FillCache { fill: gradient, surface: s.clone() });
    d.layers.push(cached);
    let got = render_cmyk(&d, d.bounds()).unwrap_or_else(|| panic!("supported"));
    assert_eq!(got.px[0], [q(0.1), q(0.2), q(0.3), q(0.4), 1.0], "the colour's own inks, at the document's levels");
    assert_eq!(got.px[2], [0.0, 0.0, 0.0, 0.0, 1.0], "masked out: paper");
    assert_eq!(got.px[3], [q(0.9), 0.0, q(0.5), 0.0, 1.0], "the fill's rendered pixels");
    assert_matches_reference(&d);
}

#[test]
fn stage_two_layers_that_stay_on_the_rgb_compositor() {
    let base = || {
        let mut d = doc(2, 2);
        d.layers.push(paper(2, 2, [0.0; 4]));
        d
    };
    // A pattern fill without rendered pixels.
    let mut d = base();
    let p = photocraft_doc::Fill::Pattern { name: "p".into(), scale: 1.0, id: "p".into(), angle: 0.0, link: true, phase: (0.0, 0.0) };
    d.layers.push(Layer::new("p", LayerContent::Fill(p)));
    assert!(!supports(&d), "uncached pattern fill");
    // Type rendered in RGB (not the document's model).
    let mut d = base();
    let mut rgb = Layer::raster("x", PixelFormat::RGBA8);
    if let Some(s) = rgb.surface_mut() {
        s.fill_rect(Rect::new(0, 0, 1, 1), &[0.0, 0.0, 0.0, 1.0]);
    }
    d.layers.push(Layer::new("t", LayerContent::Text(photocraft_doc::TextLayer { cache: rgb.surface().cloned(), ..Default::default() })));
    assert!(!supports(&d), "RGB type pixels");
    // A stroked shape with a clipped layer (its stroke goes above the clipped layers).
    let mut d = base();
    let shape = photocraft_doc::ShapeLayer { stroke: Some(Default::default()), cache: layer(&[]).surface().cloned(), ..Default::default() };
    d.layers.push(Layer::new("s", LayerContent::Shape(shape)));
    let mut clip = layer(&[(Rect::new(0, 0, 2, 2), [0.5; 5])]);
    clip.clipped = true;
    d.layers.push(clip);
    assert!(!supports(&d), "stroked shape with clipped layers");
    if let Some(c) = d.layers.last_mut() {
        c.visible = false;
    }
    assert!(supports(&d), "without a visible clipped layer the stroked shape is plain pixels");
}

/// A gradient fill without rendered pixels is the compositor's own CMYK ramp: between cyan and
/// black it holds only those two inks, pixel for pixel as the compositor renders it.
#[test]
fn uncached_gradient_fills_are_cmyk_ramps() {
    let mut d = doc(8, 2);
    let g = photocraft_doc::Fill::gradient(
        vec![(0.0, cmyk_color([1.0, 0.0, 0.0, 0.0], 1.0)), (1.0, cmyk_color([0.0, 0.0, 0.0, 1.0], 1.0))],
        0.0,
        1.0,
        Default::default(),
        false,
    );
    d.layers.push(Layer::new("g", LayerContent::Fill(g.clone())));
    assert!(supports(&d));
    let got = render_cmyk(&d, d.bounds()).unwrap_or_else(|| panic!("supported"));
    let frame = crate::fill_frame(&d.layers[0], d.bounds());
    let want = crate::gradient_fill::render_cmyk_fill(&g, d.bounds(), frame, SampleType::U8).unwrap_or_else(|| panic!("ramp"));
    for (i, p) in got.px.iter().enumerate() {
        let (x, y) = ((i % 8) as i32, (i / 8) as i32);
        assert_eq!(p[1..3], [0.0, 0.0], "only cyan and black: {p:?}");
        assert_eq!(p.to_vec(), want.pixel(x, y), "({x},{y})");
    }
    assert!(got.px[0][0] > got.px[7][0] && got.px[0][3] < got.px[7][3], "cyan to black");
}

// ---- stage 3: blend modes on ink values --------------------------------------------------------

const ALL_MODES: [BlendMode; 34] = [
    BlendMode::PassThrough,
    BlendMode::Normal,
    BlendMode::Dissolve,
    BlendMode::Darken,
    BlendMode::Multiply,
    BlendMode::ColorBurn,
    BlendMode::LinearBurn,
    BlendMode::DarkerColor,
    BlendMode::Lighten,
    BlendMode::Screen,
    BlendMode::ColorDodge,
    BlendMode::LinearDodge,
    BlendMode::LighterColor,
    BlendMode::Overlay,
    BlendMode::SoftLight,
    BlendMode::HardLight,
    BlendMode::VividLight,
    BlendMode::LinearLight,
    BlendMode::PinLight,
    BlendMode::HardMix,
    BlendMode::Difference,
    BlendMode::Exclusion,
    BlendMode::Subtract,
    BlendMode::Divide,
    BlendMode::Hue,
    BlendMode::Saturation,
    BlendMode::Color,
    BlendMode::Luminosity,
    BlendMode::Reflect,
    BlendMode::Glow,
    BlendMode::Negation,
    BlendMode::Xor,
    BlendMode::PaintNetColorBurn,
    BlendMode::PaintNetColorDodge,
];

/// The modes [`separable`] accepts, besides Normal.
const SEPARABLE: [BlendMode; 27] = [
    BlendMode::Dissolve,
    BlendMode::Darken,
    BlendMode::Multiply,
    BlendMode::ColorBurn,
    BlendMode::LinearBurn,
    BlendMode::Lighten,
    BlendMode::Screen,
    BlendMode::ColorDodge,
    BlendMode::LinearDodge,
    BlendMode::Overlay,
    BlendMode::SoftLight,
    BlendMode::HardLight,
    BlendMode::VividLight,
    BlendMode::LinearLight,
    BlendMode::PinLight,
    BlendMode::HardMix,
    BlendMode::Difference,
    BlendMode::Exclusion,
    BlendMode::Subtract,
    BlendMode::Divide,
    BlendMode::Reflect,
    BlendMode::Glow,
    BlendMode::Negation,
    BlendMode::Xor,
    BlendMode::PaintNetColorBurn,
    BlendMode::PaintNetColorDodge,
    BlendMode::Normal,
];

/// Every mode treated as per-channel really is: a channel's result depends only on that
/// channel's values (checked on many inputs), so applying it to each ink is exact. The modes left
/// to the RGB compositor really do mix channels.
#[test]
fn separable_modes_are_channel_by_channel() {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut r = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 40) as f32 / (1u64 << 24) as f32
    };
    for mode in ALL_MODES {
        let mut mixes = false;
        for _ in 0..2000 {
            let (b, s) = ([r(), r(), r()], [r(), r(), r()]);
            let (b2, s2) = ([b[0], r(), r()], [s[0], r(), r()]);
            let (x, y) = (crate::psblend::blend_rgb(mode, b, s)[0], crate::psblend::blend_rgb(mode, b2, s2)[0]);
            mixes |= (x - y).abs() > 1e-6 || x.is_nan() != y.is_nan();
        }
        // Pass Through is a group behaviour (its pixel formula is Normal's); it's checked apart.
        if mode != BlendMode::PassThrough {
            assert_eq!(separable(mode), !mixes, "{mode:?}");
        }
        assert_eq!(SEPARABLE.contains(&mode), separable(mode), "{mode:?} list");
    }
}

/// Opaque layers over opaque paper in common modes give the inks worked out by hand.
#[test]
fn blend_modes_give_the_expected_inks() {
    type Ink = fn(f32, f32) -> f32;
    let cases: [(BlendMode, Ink); 7] = [
        (BlendMode::Multiply, |b, s| b + s - b * s), // darker: inks add up
        (BlendMode::Screen, |b, s| b * s),           // lighter: inks thin out
        (BlendMode::Darken, f32::max),
        (BlendMode::Lighten, f32::min),
        (BlendMode::Difference, |b, s| 1.0 - (b - s).abs()),
        (BlendMode::LinearBurn, |b, s| (b + s).min(1.0)),
        (BlendMode::LinearDodge, |b, s| (b + s - 1.0).max(0.0)),
    ];
    let inks = [0.0, 0.2, 0.5, 0.8, 1.0];
    for (mode, want) in cases {
        for (bi, si) in inks.iter().flat_map(|b| inks.iter().map(move |s| (*b, *s))) {
            let mut d = doc(1, 1);
            d.layers.push(paper(1, 1, [bi, 0.0, bi, si]));
            let mut l = layer(&[(Rect::new(0, 0, 1, 1), [si, si, 0.0, bi, 1.0])]);
            l.blend = mode;
            d.layers.push(l);
            let got = render_cmyk(&d, d.bounds()).unwrap_or_else(|| panic!("supported"))[0];
            let (b, s) = (q(bi), q(si));
            let exp = [want(b, s), want(0.0, s), want(b, 0.0), want(s, b), 1.0];
            assert!(got.iter().zip(exp).all(|(g, e)| (g - e).abs() < 1e-5), "{mode:?} b={b} s={s}: {got:?} vs {exp:?}");
        }
    }
}

impl std::ops::Index<usize> for CmykBuffer {
    type Output = Cmyka;
    fn index(&self, i: usize) -> &Cmyka {
        &self.px[i]
    }
}

/// Dissolve shows a pixel fully or not at all, with the RGB compositor's noise: coverage is
/// either the layer's full ink or untouched paper, and agrees pixel for pixel with the RGB one.
#[test]
fn dissolve_matches_the_rgb_compositors_pixels() {
    let mut d = doc(16, 16);
    d.layers.push(paper(16, 16, [0.0; 4]));
    let mut l = layer(&[(Rect::new(0, 0, 16, 16), [0.0, 0.0, 0.0, 1.0, 0.4])]);
    l.blend = BlendMode::Dissolve;
    d.layers.push(l);
    let got = render_cmyk(&d, d.bounds()).unwrap_or_else(|| panic!("supported"));
    let rgb = crate::render(&d, d.bounds());
    let mut shown = 0;
    for (c, r) in got.px.iter().zip(&rgb.px) {
        assert!(*c == [0.0, 0.0, 0.0, 1.0, 1.0] || *c == [0.0, 0.0, 0.0, 0.0, 1.0], "{c:?}");
        // Paper is white on screen; a shown pixel is the K100's dark colour.
        assert_eq!(c[3] == 1.0, r[0] < 0.5, "{c:?} vs {r:?}");
        shown += usize::from(c[3] == 1.0);
    }
    assert!((60..150).contains(&shown), "about 40% of 256 shown: {shown}");
}

/// Photoshop's special eight: Fill below 100% is part of the blend, not coverage. Difference at
/// 50% Fill subtracts half the source's brightness, where 50% coverage would mix the full
/// difference in by half (the RGB compositor does the same per channel). An ink neither side
/// has comes out at 50%: the half-pulled source differs from the paper by half.
#[test]
fn fill_is_part_of_the_blend_for_the_special_eight() {
    let mut d = doc(1, 1);
    d.layers.push(paper(1, 1, [0.7, 0.0, 0.0, 0.0]));
    let mut l = layer(&[(Rect::new(0, 0, 1, 1), [0.1, 0.0, 0.0, 0.0, 1.0])]);
    l.blend = BlendMode::Difference;
    l.fill_opacity = 0.5;
    d.layers.push(l);
    let got = render_cmyk(&d, d.bounds()).unwrap_or_else(|| panic!("supported"))[0];
    let inside = |bi: f32, si: f32| 1.0 - ((1.0 - bi) - 0.5 * (1.0 - si)).abs();
    let want = [inside(q(0.7), q(0.1)), inside(0.0, 0.0), inside(0.0, 0.0), inside(0.0, 0.0), 1.0];
    assert!(got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-5), "{got:?} vs {want:?}");
    let (b, s) = (1.0 - q(0.7), 1.0 - q(0.1));
    let as_coverage = 1.0 - (0.5 * b + 0.5 * (b - s).abs());
    assert!((want[0] - as_coverage).abs() > 0.2, "the two readings differ");
}

// ---- stage 4: adjustment layers on ink values --------------------------------------------------

fn rgb_levels() -> Adjustment {
    use photocraft_doc::adjust::{LevelsChannel, ToneSpace};
    let d = LevelsChannel::default;
    Adjustment::Levels { master: LevelsChannel { in_white: 0.5, ..d() }, per_channel: [d(), d(), d()], space: ToneSpace::Rgb, black: d() }
}

fn cmyk_levels(master: photocraft_doc::adjust::LevelsChannel, black: photocraft_doc::adjust::LevelsChannel) -> Adjustment {
    use photocraft_doc::adjust::{LevelsChannel, ToneSpace};
    let d = LevelsChannel::default;
    Adjustment::Levels { master, per_channel: [d(), d(), d()], space: ToneSpace::Cmyk, black }
}

fn adjustment(a: Adjustment) -> Layer {
    Layer::new("a", LayerContent::Adjustment(a))
}

fn render1(d: &Document, x: i32, y: i32) -> Cmyka {
    let got = render_cmyk(d, d.bounds()).unwrap_or_else(|| panic!("supported"));
    got.px[(y * d.size.width as i32 + x) as usize]
}

fn near(a: Cmyka, b: Cmyka, tol: f32) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tol)
}

/// Invert turns every ink into its opposite, black included (blank paper becomes four-colour
/// black); Posterize steps each ink on its own.
#[test]
fn invert_and_posterize_work_ink_by_ink() {
    let mut d = doc(2, 1);
    d.layers.push(paper(2, 1, [0.0; 4]));
    d.layers.push(layer(&[(Rect::new(1, 0, 2, 1), [0.0, 0.0, 0.0, 1.0, 1.0])]));
    d.layers.push(adjustment(Adjustment::Invert));
    assert_eq!(render1(&d, 0, 0), [1.0, 1.0, 1.0, 1.0, 1.0], "paper");
    assert_eq!(render1(&d, 1, 0), [1.0, 1.0, 1.0, 0.0, 1.0], "K-only black");
    let mut d = doc(1, 1);
    d.layers.push(paper(1, 1, [0.2, 0.45, 0.7, 0.9]));
    d.layers.push(adjustment(Adjustment::Posterize { levels: 2 }));
    // Two levels: brightness below half goes dark (full ink), above half light (no ink).
    assert_eq!(render1(&d, 0, 0), [0.0, 0.0, 1.0, 1.0, 1.0]);
}

/// Levels' composite record changes every ink, the black record only K; channels a record
/// leaves alone keep their exact inks.
#[test]
fn levels_records_address_their_inks() {
    use photocraft_doc::adjust::LevelsChannel;
    let d0 = LevelsChannel::default;
    // Composite: input white at 50% doubles each ink's brightness.
    let mut d = doc(1, 1);
    d.layers.push(paper(1, 1, [0.2, 0.4, 0.6, 0.1]));
    d.layers.push(adjustment(cmyk_levels(LevelsChannel { in_white: 0.5, ..d0() }, d0())));
    assert!(near(render1(&d, 0, 0), [0.0, 0.0, 0.2, 0.0, 1.0], 1.0 / 255.0), "{:?}", render1(&d, 0, 0));
    // Black: output white at 50% leaves at least half K, and no other ink.
    let mut d = doc(2, 1);
    d.layers.push(paper(2, 1, [0.0; 4]));
    d.layers.push(layer(&[(Rect::new(1, 0, 2, 1), [0.4, 0.0, 0.0, 0.0, 1.0])]));
    d.layers.push(adjustment(cmyk_levels(d0(), LevelsChannel { out_white: 0.5, ..d0() })));
    let (blank, cyan) = (render1(&d, 0, 0), render1(&d, 1, 0));
    assert_eq!(&blank[..3], &[0.0; 3], "{blank:?}");
    assert!((blank[3] - 0.5).abs() <= 1.0 / 255.0, "{blank:?}");
    assert_eq!(cyan[0], q(0.4), "cyan untouched: {cyan:?}");
}

/// Opacity, masks, blend modes, clipping onto adjustments and adjustments clipped onto a layer,
/// inside isolated and pass-through groups, against the reference.
#[test]
fn adjustment_stacks_match_the_reference() {
    use photocraft_doc::adjust::LevelsChannel;
    let d0 = LevelsChannel::default;
    let mut d = doc(8, 6);
    d.layers.push(paper(8, 6, [0.05, 0.1, 0.0, 0.0]));
    d.layers.push(layer(&[(Rect::new(0, 0, 5, 6), [0.0, 0.0, 0.0, 1.0, 1.0]), (Rect::new(5, 0, 8, 3), [0.6, 0.2, 0.1, 0.3, 0.7])]));
    let mut inv = adjustment(Adjustment::Invert);
    inv.opacity = 0.4;
    inv.mask = Some(mask(&[(Rect::new(0, 0, 3, 6), 0.0)], 1.0, 0.8, true));
    let mut onto = layer(&[(Rect::new(2, 2, 8, 6), [0.0, 0.7, 0.0, 0.0, 1.0])]);
    onto.clipped = true;
    onto.blend = BlendMode::Multiply;
    let mut base = layer(&[(Rect::new(1, 1, 7, 5), [0.1, 0.1, 0.5, 0.0, 0.9])]);
    base.blend = BlendMode::Screen;
    let mut lv = adjustment(cmyk_levels(LevelsChannel { gamma: 1.8, ..d0() }, LevelsChannel { in_black: 0.2, ..d0() }));
    lv.clipped = true;
    lv.blend = BlendMode::Overlay;
    let mut pass = Layer::group("pass", vec![adjustment(Adjustment::Posterize { levels: 4 })]);
    pass.opacity = 0.6;
    let mut iso = Layer::group("iso", vec![layer(&[(Rect::new(0, 3, 8, 6), [0.5, 0.0, 0.5, 0.0, 0.5])]), adjustment(Adjustment::Invert)]);
    iso.blend = BlendMode::Normal;
    d.layers.extend([inv, onto, base, lv, pass, iso]);
    assert_matches_reference(&d);
}

/// In a 32-bit document (no rounding to levels), inks an adjustment leaves alone come out
/// bit for bit, at full strength and through blend modes that pick an input.
#[test]
fn untouched_inks_stay_bit_exact() {
    use photocraft_doc::adjust::LevelsChannel;
    let d0 = LevelsChannel::default;
    let fmt = PixelFormat { mode: ColorMode::Cmyk, sample: SampleType::F32, alpha: false };
    let inks = [0.123_456_7, 0.456_789_1, 0.789_123_4, 0.0];
    for blend in [BlendMode::Normal, BlendMode::Darken, BlendMode::Lighten] {
        let mut d = Document::new("f", Size::new(1, 1), ColorMode::Cmyk, SampleType::F32);
        let mut bg = Layer::raster("bg", fmt);
        if let Some(s) = bg.surface_mut() {
            s.fill_rect(Rect::new(0, 0, 1, 1), &inks);
        }
        d.layers.push(bg);
        let mut a = adjustment(cmyk_levels(d0(), LevelsChannel { out_white: 0.5, ..d0() }));
        a.blend = blend;
        d.layers.push(a);
        let got = render1(&d, 0, 0);
        assert_eq!(&got[..3], &inks[..3], "{blend:?}");
        let want_k = if blend == BlendMode::Lighten { 0.0 } else { 0.5 };
        assert!((got[3] - want_k).abs() < 1e-6, "{blend:?}: {got:?}");
    }
}
