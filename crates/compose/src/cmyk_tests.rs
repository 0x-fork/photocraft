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
        o[i] = dec((1.0 - sa) * b[4] / a * enc(b[i]) + sa / a * enc(s[i]));
    }
    o[4] = a;
    o
}

fn over_layer(l: &Layer, b: Cmyka, s: Cmyka, opacity: f32) -> Cmyka {
    if matches!(l.content, LayerContent::Text(_)) { over_text(b, s, opacity) } else { over(b, s, opacity) }
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
        LayerContent::Group(g) => {
            let mut c = ref_stack(&g.children, x, y, [0.0; 5]);
            c[4] *= ref_mask(l, x, y);
            c
        }
        _ => ref_own(l, x, y),
    };
    for c in clipped.iter().filter(|c| c.visible) {
        if content[4] > 0.0 {
            let a = content[4];
            let r = over_layer(c, [content[0], content[1], content[2], content[3], 1.0], ref_own(c, x, y), c.opacity * c.fill_opacity);
            content = [r[0], r[1], r[2], r[3], a];
        }
    }
    over_layer(l, b, content, op)
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
    check("multiply", &|l| l.blend = BlendMode::Multiply);
    check("dissolve", &|l| l.blend = BlendMode::Dissolve);
    check("knockout", &|l| l.advanced.knockout = Knockout::Shallow);
    check("channels", &|l| l.excluded_channels = 1);
    check("nan opacity", &|l| l.opacity = f32::NAN);
    check("adjustment", &|l| l.content = LayerContent::Adjustment(Adjustment::Invert));
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
    Pixels { kind: Kind, fills: Vec<(Rect, Cmyka)>, opacity: f32, fill: f32, clipped: bool, visible: bool, mask: Option<MaskSpec> },
    Group { children: Vec<LayerSpec>, pass_through: bool, opacity: f32, fill: f32, mask: Option<MaskSpec> },
}

/// What kind of layer a [`LayerSpec::Pixels`] becomes: its pixels as a pixel layer, as a type or
/// shape layer's rendered pixels, or a solid colour fill in the first fill's colour.
#[derive(Debug, Clone, Copy)]
enum Kind {
    Raster,
    Text,
    Shape,
    Solid,
}

#[derive(Debug, Clone)]
struct MaskSpec {
    rects: Vec<(Rect, f32)>,
    default: f32,
    density: f32,
    enabled: bool,
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
        prop_oneof![Just(Kind::Raster), Just(Kind::Text), Just(Kind::Shape), Just(Kind::Solid)],
    )
        .prop_map(|(fills, opacity, fill, clipped, visible, mask, kind)| LayerSpec::Pixels {
            kind,
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
        (prop::collection::vec(inner, 1..4), any::<bool>(), unit(), prop_oneof![Just(1.0f32), unit()], mask_spec(w, h))
            .prop_map(move |(children, pass_through, opacity, fill, mask)| LayerSpec::Group { children, pass_through, opacity, fill, mask })
    })
}

fn spec() -> impl Strategy<Value = Spec> {
    (2i32..9, 2i32..7).prop_flat_map(|(w, h)| prop::collection::vec(layer_spec(w, h), 1..6).prop_map(move |layers| Spec { w, h, layers }))
}

fn build(s: &LayerSpec) -> Layer {
    let make_mask = |m: &MaskSpec| mask(&m.rects, m.default, m.density, m.enabled);
    match s {
        LayerSpec::Pixels { kind, fills, opacity, fill, clipped, visible, mask } => {
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
            };
            (l.opacity, l.fill_opacity, l.clipped, l.visible) = (*opacity, *fill, *clipped, *visible);
            l.mask = mask.as_ref().map(make_mask);
            l
        }
        LayerSpec::Group { children, pass_through, opacity, fill, mask } => {
            let mut g = Layer::group("g", children.iter().map(build).collect());
            g.blend = if *pass_through { BlendMode::PassThrough } else { BlendMode::Normal };
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
