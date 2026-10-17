use photocraft_io::ExportOptions;
use serde_json::Value;
use std::io::Write;

use crate::{CliError, Result, args::ExportArgs};

pub(crate) fn export_opts(a: &ExportArgs) -> Result<ExportOptions> {
    let bad = |flag, value: &str, expected| CliError::BadValue { flag, value: value.to_owned(), expected };
    // TIFF output is flat unless asked: scripted conversions keep predictable sizes.
    let mut o = ExportOptions { tiff_layers: a.tiff_layers, ..Default::default() };
    if let Some(q) = &a.quality {
        let q = q.parse().ok().filter(|q| (1..=100).contains(q)).ok_or_else(|| bad("--quality", q, "a whole number from 1 to 100"))?;
        o.encode.jpeg_quality = q;
        // Asking for a quality asks for a lossy WebP; the default WebP stays lossless.
        o.encode.webp_quality = q;
        o.encode.webp_lossless = false;
    }
    if let Some(b) = &a.tga_bits {
        o.tga_bits = Some(b.parse().ok().and_then(photocraft_io::TgaBits::from_bits).ok_or_else(|| bad("--tga-bits", b, "24 or 32"))?);
    }
    if let Some(b) = &a.dpx_bits {
        o.encode.dpx_bits = Some(b.parse().ok().and_then(photocraft_io::DpxBits::from_bits).ok_or_else(|| bad("--dpx-bits", b, "8, 10, 12 or 16"))?);
    }
    if let Some(c) = &a.tiff_compression {
        o.encode.tiff_compression = match c.as_str() {
            "none" => photocraft_codecs::TiffCompression::None,
            "lzw" => photocraft_codecs::TiffCompression::Lzw,
            "zip" => photocraft_codecs::TiffCompression::Deflate,
            _ => return Err(bad("--tiff-compression", c, "none, lzw or zip")),
        };
    }
    if let Some(f) = &a.jpeg_format {
        o.encode.jpeg_format = photocraft_codecs::JpegFormat::parse(f).ok_or_else(|| bad("--jpeg-format", f, "baseline, optimized or progressive"))?;
    }
    if let Some(c) = &a.dds_compression {
        o.encode.dds.compression = photocraft_io::DdsCompression::from_name(c).ok_or_else(|| bad("--dds-compression", c, "none, bc1, bc2 or bc3"))?;
    }
    o.encode.dds.mipmaps = a.dds_mipmaps;
    Ok(o)
}

pub(crate) fn warn_all(err: &mut dyn Write, ws: &[String]) {
    for w in ws {
        let _ = writeln!(err, "warning: {w}");
    }
}

/// How [`print_json`] lays the JSON out.
pub(crate) enum JsonStyle {
    Pretty,
    Compact,
}

pub(crate) fn print_json(out: &mut dyn Write, v: &Value, style: JsonStyle) -> Result {
    let s = match style {
        JsonStyle::Pretty => serde_json::to_string_pretty(v),
        JsonStyle::Compact => serde_json::to_string(v),
    }?;
    Ok(writeln!(out, "{s}")?)
}
