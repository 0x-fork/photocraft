//! Indexed Color and Duotone documents in flat exports.

use photocraft_color::{Color, ColorMode, SampleType};
use photocraft_doc::{ColorTable, Document, Duotone, DuotoneInk, Size};

fn doc(mode: ColorMode) -> Document {
    let mut d = Document::with_background("m", Size::new(4, 2), ColorMode::Rgb, SampleType::U8, Color::rgba(1.0, 0.0, 0.0, 1.0));
    d.mode = mode;
    d
}

#[test]
fn indexed_png_is_palette_png() {
    let mut d = doc(ColorMode::Indexed);
    d.color_table = Some(ColorTable { colors: vec![[0, 0, 0], [255, 0, 0]], transparent: None });
    let r = photocraft_io::export(&d, "x.png", &Default::default()).unwrap();
    // IHDR colour type 3 = indexed; a PLTE chunk follows.
    assert_eq!(r.bytes[25], 3);
    assert!(r.bytes.windows(4).any(|w| w == b"PLTE"));
    let back = photocraft_io::import("x.png", &r.bytes).unwrap().document;
    let px = back.layers[0].surface().unwrap().rgba(1, 1);
    assert_eq!(&px[..3], &[1.0, 0.0, 0.0]);
    // Other formats keep the expanded pixels.
    let j = photocraft_io::export(&d, "x.tif", &Default::default()).unwrap();
    assert!(!j.bytes.is_empty());
}

#[test]
fn duotone_exports_the_inks_as_rgb() {
    // Mid gray background.
    let mut d = Document::with_background("m", Size::new(4, 2), ColorMode::Grayscale, SampleType::U8, Color::rgba(0.5, 0.5, 0.5, 1.0));
    d.mode = ColorMode::Duotone;
    d.duotone = Some(Duotone { inks: vec![DuotoneInk::new("Black", [0.0; 3]), DuotoneInk::new("Orange", [1.0, 0.5, 0.0])], psd_raw: None });
    let r = photocraft_io::export(&d, "x.png", &Default::default()).unwrap();
    assert!(r.warnings.iter().any(|w| w.contains("Duotone")), "{:?}", r.warnings);
    let back = photocraft_io::import("x.png", &r.bytes).unwrap().document;
    let px = back.layers[0].surface().unwrap().rgba(0, 0);
    assert!(px[0] > px[2] + 0.05, "warm: {px:?}");
}

#[test]
fn indexed_png_keeps_the_document_resolution() {
    // #3047: the palette PNG lost its pHYs chunk and reopened at 72 ppi.
    let mut d = doc(ColorMode::Indexed);
    d.resolution_dpi = 300.0;
    d.color_table = Some(ColorTable { colors: vec![[0, 0, 0], [255, 0, 0]], transparent: None });
    let r = photocraft_io::export(&d, "x.png", &Default::default()).unwrap();
    assert_eq!(r.bytes[25], 3, "still a palette PNG");
    let back = photocraft_io::import("x.png", &r.bytes).unwrap().document;
    assert!((back.resolution_dpi - 300.0).abs() < 0.01, "{}", back.resolution_dpi);
}

#[test]
fn indexed_bmp_is_palette_bmp() {
    // The BMP export wrote an Indexed Color document as 24-bit RGB, so reopening it lost the mode (#3869 follow-up).
    let mut d = doc(ColorMode::Indexed);
    d.color_table = Some(ColorTable { colors: vec![[0, 0, 0], [255, 0, 0], [0, 255, 0]], transparent: None });
    let r = photocraft_io::export(&d, "x.bmp", &Default::default()).unwrap();
    assert_eq!(u16::from_le_bytes([r.bytes[28], r.bytes[29]]), 8, "8 bits per pixel");
    assert!(r.warnings.iter().any(|w| w.contains("palette BMP")), "{:?}", r.warnings);
    let back = photocraft_io::import("x.bmp", &r.bytes).unwrap().document;
    assert_eq!(back.mode, ColorMode::Indexed);
    assert_eq!(back.color_table.as_ref().map(|t| t.colors.clone()), Some(vec![[0, 0, 0], [255, 0, 0], [0, 255, 0]]));
    let px = back.layers[0].surface().unwrap().rgba(1, 1);
    assert_eq!(&px[..3], &[1.0, 0.0, 0.0]);
}

#[test]
fn indexed_bmp_warns_that_it_has_no_transparency() {
    let mut d = doc(ColorMode::Indexed);
    d.color_table = Some(ColorTable { colors: vec![[0, 0, 0], [255, 0, 0]], transparent: Some(0) });
    let r = photocraft_io::export(&d, "x.bmp", &Default::default()).unwrap();
    assert!(r.warnings.iter().any(|w| w.contains("no transparency")), "{:?}", r.warnings);
}

#[test]
fn indexed_bmp_without_a_usable_table_falls_back_to_rgb() {
    // No table (or one past 256 entries) keeps the old 24-bit path rather than failing the save.
    for table in [None, Some(ColorTable { colors: vec![[0; 3]; 257], transparent: None })] {
        let mut d = doc(ColorMode::Indexed);
        d.color_table = table;
        let r = photocraft_io::export(&d, "x.bmp", &Default::default()).unwrap();
        assert_eq!(u16::from_le_bytes([r.bytes[28], r.bytes[29]]), 24);
    }
}
