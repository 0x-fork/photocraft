//! DICOM (medical imaging) reader, uncompressed transfer syntaxes only.
//!
//! Supports what a viewer needs from a single-frame image: MONOCHROME1/2 (gray) and RGB,
//! 8- and 16-bit samples, interleaved or planar, both byte orders and explicit/implicit VR.
//! Compressed transfer syntaxes, sequences, overlays and multi-frame pixel data are refused
//! with a clear error. Read-only in this crate.

use std::collections::HashMap;

use crate::error::CodecError;
use crate::format::Format;
use crate::image::{ChannelLayout, Image, SampleType};
use crate::options::Limits;

const F: Format = Format::Dicom;
const PREAMBLE: usize = 132; // 128 zero bytes + "DICM"

fn bad(msg: impl std::fmt::Display) -> CodecError {
    CodecError::malformed(F, msg)
}

/// A DICOM file begins with a 128-byte preamble and the `DICM` magic.
pub(crate) fn is_dicom(b: &[u8]) -> bool {
    b.get(128..132) == Some(b"DICM")
}

#[derive(Clone, Copy)]
struct Endian {
    big: bool,
}

/// The transfer syntax's byte order and whether value representations are explicit.
struct Syntax {
    endian: Endian,
    explicit: bool,
}

/// The value representations whose length field is 2 reserved bytes + `u32` in explicit VR.
fn long_vr(vr: &[u8]) -> bool {
    matches!(vr, b"OB" | b"OW" | b"OF" | b"SQ" | b"UT" | b"UN" | b"OD" | b"OL" | b"OV" | b"UC" | b"UR")
}

/// A cursor over the data set.
struct Cur<'a> {
    b: &'a [u8],
    pos: usize,
    big: bool,
}

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }
    fn u16(&mut self) -> Option<u16> {
        let c = self.take(2)?;
        Some(if self.big { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) })
    }
    fn u32(&mut self) -> Option<u32> {
        let c = self.take(4)?;
        Some(if self.big { u32::from_be_bytes([c[0], c[1], c[2], c[3]]) } else { u32::from_le_bytes([c[0], c[1], c[2], c[3]]) })
    }
}

/// A parsed element: its VR and the byte range of its value.
struct Elem {
    vr: [u8; 2],
    start: usize,
    len: usize,
}

/// Walk the elements from `start`, calling `f` with each `(group, element, value)`. Undefined
/// lengths (sequences) are refused, keeping the decoder single-frame and bounded.
fn elements(b: &[u8], start: usize, syn: &Syntax, mut f: impl FnMut(u16, u16, &Elem) -> Result<(), CodecError>) -> Result<usize, CodecError> {
    let mut c = Cur { b, pos: start, big: syn.endian.big };
    loop {
        let Some(group) = c.u16() else { return Ok(c.pos - start) };
        let elem = c.u16().ok_or_else(|| bad("truncated element tag"))?;
        let (vr, len) = if syn.explicit {
            let vr = c.take(2).ok_or_else(|| bad("truncated VR"))?;
            let vr2 = [vr[0], vr[1]];
            let len = if long_vr(&vr2) {
                c.take(2).ok_or_else(|| bad("truncated VR"))?; // reserved
                c.u32().ok_or_else(|| bad("truncated length"))?
            } else {
                u32::from(c.u16().ok_or_else(|| bad("truncated length"))?)
            };
            (vr2, len)
        } else {
            ([0, 0], c.u32().ok_or_else(|| bad("truncated length"))?)
        };
        if len == 0xFFFF_FFFF {
            return Err(CodecError::unsupported(F, "undefined-length (sequence) elements"));
        }
        let len = len as usize;
        let start = c.pos;
        if b.len() < start.saturating_add(len) {
            return Err(bad("element value runs past the end of the file"));
        }
        f(group, elem, &Elem { vr, start, len })?;
        c.pos = start + len;
    }
}

fn u16_val(b: &[u8], e: &Elem, big: bool) -> Option<u16> {
    let c = b.get(e.start..e.start + 2)?;
    Some(if big { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) })
}

/// The transfer syntax (byte order and explicit/implicit VR). The file meta group is always
/// explicit VR little-endian; the data set follows its `(0002,0010)` TransferSyntaxUID.
fn transfer_syntax(b: &[u8]) -> Result<(Syntax, usize), CodecError> {
    let meta = Syntax { endian: Endian { big: false }, explicit: true };
    let mut ts: Option<String> = None;
    let mut data_start = b.len();
    let mut c = Cur { b, pos: PREAMBLE, big: false };
    loop {
        let save = c.pos;
        let Some(group) = c.u16() else { break };
        let _ = c.u16().ok_or_else(|| bad("truncated element tag"))?;
        if group != 0x0002 {
            // A non-meta group begins the data set.
            data_start = save;
            break;
        }
        let vr = c.take(2).ok_or_else(|| bad("truncated VR"))?;
        let vr2 = [vr[0], vr[1]];
        let len = if long_vr(&vr2) {
            c.take(2).ok_or_else(|| bad("truncated VR"))?;
            c.u32().ok_or_else(|| bad("truncated length"))?
        } else {
            u32::from(c.u16().ok_or_else(|| bad("truncated length"))?)
        };
        if len == 0xFFFF_FFFF {
            return Err(CodecError::unsupported(F, "undefined-length meta element"));
        }
        let start = c.pos;
        if b.len() < start.saturating_add(len as usize) {
            return Err(bad("meta element runs past the end of the file"));
        }
        if (group, 0x0010) == (0x0002, 0x0010) {
            ts = Some(String::from_utf8_lossy(&b[start..start + len as usize]).trim_end_matches(['\0', ' ']).to_string());
        }
        c.pos = start + len as usize;
    }
    let _ = elements(b, PREAMBLE, &meta, |_, _, _| Ok(()))?; // validate the meta group parses
    let syn = match ts.as_deref() {
        Some("1.2.840.10008.1.2") => Syntax { endian: Endian { big: false }, explicit: false },
        Some("1.2.840.10008.1.2.1") => Syntax { endian: Endian { big: false }, explicit: true },
        Some("1.2.840.10008.1.2.2") => Syntax { endian: Endian { big: true }, explicit: true },
        Some(other) => return Err(CodecError::unsupported(F, format!("transfer syntax {other} (only uncompressed is read)"))),
        // No meta: the historical default is implicit VR little-endian.
        None => Syntax { endian: Endian { big: false }, explicit: false },
    };
    Ok((syn, data_start))
}

pub(crate) fn decode(b: &[u8], limits: &Limits) -> Result<Image, CodecError> {
    if !is_dicom(b) {
        return Err(bad("not a DICOM file (no DICM magic)"));
    }
    let (syn, start) = transfer_syntax(b)?;
    let mut tags: HashMap<(u16, u16), Elem> = HashMap::new();
    elements(b, start, &syn, |g, e, el| {
        // Later duplicates must not silently override the first (DICOM forbids duplicates).
        tags.entry((g, e)).or_insert_with(|| Elem { vr: el.vr, start: el.start, len: el.len });
        Ok(())
    })?;
    let big = syn.endian.big;
    let get_u16 = |tag: (u16, u16)| tags.get(&tag).and_then(|e| u16_val(b, e, big));
    let rows = get_u16((0x0028, 0x0010)).ok_or_else(|| bad("missing Rows (0028,0010)"))?;
    let cols = get_u16((0x0028, 0x0011)).ok_or_else(|| bad("missing Columns (0028,0011)"))?;
    let samples = get_u16((0x0028, 0x0002)).unwrap_or(1);
    let bits = get_u16((0x0028, 0x0100)).ok_or_else(|| bad("missing BitsAllocated (0028,0100)"))?;
    let bits_stored = get_u16((0x0028, 0x0101)).unwrap_or(bits);
    let high_bit = get_u16((0x0028, 0x0102)).unwrap_or(bits_stored.saturating_sub(1));
    let signed = get_u16((0x0028, 0x0103)).unwrap_or(0) == 1;
    let planar = get_u16((0x0028, 0x0006)).unwrap_or(0);
    let photometric = tags
        .get(&(0x0028, 0x0004))
        .and_then(|e| b.get(e.start..e.start + e.len))
        .map(|v| String::from_utf8_lossy(v).trim_end_matches(['\0', ' ']).to_string())
        .ok_or_else(|| bad("missing PhotometricInterpretation (0028,0004)"))?;
    let pixel = tags.get(&(0x7FE0, 0x0010)).ok_or_else(|| CodecError::unsupported(F, "no pixel data (0028,0004/7FE0,0010)"))?;
    if !matches!(bits, 8 | 16) || !matches!(bits_stored, 1..=16) {
        return Err(CodecError::unsupported(F, format!("{bits}-bit allocated / {bits_stored}-bit stored samples")));
    }
    let mono = matches!(photometric.as_str(), "MONOCHROME1" | "MONOCHROME2");
    let rgb = photometric == "RGB";
    let ybr = photometric == "YBR_FULL" || photometric == "YBR_FULL_422";
    if !(mono || rgb || ybr) {
        return Err(CodecError::unsupported(F, format!("photometric interpretation {photometric}")));
    }
    if mono && samples != 1 {
        return Err(CodecError::unsupported(F, format!("{samples} samples per pixel with {photometric}")));
    }
    if ybr && (bits != 8 || samples != 3) {
        return Err(CodecError::unsupported(F, format!("{bits}-bit {photometric} ({samples} samples)")));
    }
    let layout = if mono { ChannelLayout::Gray } else { ChannelLayout::Rgb };
    let out_sample = if bits == 8 { SampleType::U8 } else { SampleType::U16 };
    limits.check(u32::from(cols), u32::from(rows), layout, out_sample)?;
    let n = usize::from(cols) * usize::from(rows) * usize::from(samples);
    let bytes_per = if bits == 16 { 2 } else { 1 };
    let need = n.checked_mul(bytes_per).ok_or_else(|| bad("pixel size overflow"))?;
    let data = b.get(pixel.start..pixel.start + need).ok_or_else(|| bad("pixel data is truncated"))?;
    let out_channels = layout.channels();
    let mut out = Vec::new();
    out.try_reserve_exact(usize::from(cols) * usize::from(rows) * out_channels * bytes_per)
        .map_err(|_| CodecError::LimitExceeded("cannot allocate the image".into()))?;
    // Sample `i` of `samples` channels at pixel `p`, honouring the planar configuration.
    let sample_at = |p: usize, ch: usize, s: usize| -> u32 {
        let idx = if planar == 1 { ch * usize::from(cols) * usize::from(rows) + p } else { s + p * usize::from(samples) + ch };
        if bits == 16 {
            let off = idx * 2;
            let c = [data[off], data[off + 1]];
            u32::from(if big { u16::from_be_bytes(c) } else { u16::from_le_bytes(c) })
        } else {
            u32::from(data[idx])
        }
    };
    let stored_max = f64::from((1u32 << bits_stored) - 1);
    // Extract the `bits_stored` samples (the least significant one at `high_bit`) and scale to
    // [0, 1]; signed values are shifted up by half a range first. `high_bit` is 1-based here.
    let shift = (u32::from(high_bit) + 1).saturating_sub(u32::from(bits_stored));
    let mask = (1u32 << bits_stored) - 1;
    let half = 1u32 << (bits_stored - 1);
    let norm = move |raw: u32| -> f64 {
        let v = (raw >> shift) & mask;
        // Signed: flip the sign bit to map [-2^(b-1), 2^(b-1)-1] onto [0, 2^b-1].
        let v = if signed { v ^ half } else { v };
        (f64::from(v) / stored_max).clamp(0.0, 1.0)
    };
    let q = |x: f64| -> u16 { (x * 65535.0).round() as u16 };
    let push = |out: &mut Vec<u8>, x: f64| {
        if bits == 8 {
            out.push((x * 255.0).round() as u8);
        } else {
            out.extend_from_slice(&q(x).to_ne_bytes());
        }
    };
    let px = usize::from(cols) * usize::from(rows);
    for p in 0..px {
        if mono {
            let x = norm(sample_at(p, 0, 0));
            push(&mut out, if photometric == "MONOCHROME1" { 1.0 - x } else { x });
        } else if rgb {
            for ch in 0..3 {
                push(&mut out, norm(sample_at(p, ch, ch)));
            }
        } else {
            // YBR_FULL (8-bit): BT.601 full-range to RGB.
            let y = sample_at(p, 0, 0) as f64;
            let cb = sample_at(p, 1, 1) as f64 - 128.0;
            let cr = sample_at(p, 2, 2) as f64 - 128.0;
            let r = (y + 1.402 * cr).clamp(0.0, 255.0);
            let g = (y - 0.344_136 * cb - 0.714_136 * cr).clamp(0.0, 255.0);
            let bl = (y + 1.772 * cb).clamp(0.0, 255.0);
            for v in [r, g, bl] {
                push(&mut out, v / 255.0);
            }
        }
    }
    Image::from_raw(u32::from(cols), u32::from(rows), layout, out_sample, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A little-endian explicit-VR DICOM with meta group and one frame.
    fn synth(rows: u16, cols: u16, samples: u16, bits: u16, photometric: &str, pixels: &[u8]) -> Vec<u8> {
        fn us(b: &mut Vec<u8>, g: u16, e: u16, v: u16) {
            b.extend_from_slice(&g.to_le_bytes());
            b.extend_from_slice(&e.to_le_bytes());
            b.extend_from_slice(b"US");
            b.extend_from_slice(&2u16.to_le_bytes());
            b.extend_from_slice(&v.to_le_bytes());
        }
        fn cs(b: &mut Vec<u8>, g: u16, e: u16, v: &str) {
            let mut s = v.as_bytes().to_vec();
            if s.len() % 2 == 1 {
                s.push(0);
            }
            b.extend_from_slice(&g.to_le_bytes());
            b.extend_from_slice(&e.to_le_bytes());
            b.extend_from_slice(b"CS");
            b.extend_from_slice(&(s.len() as u16).to_le_bytes());
            b.extend_from_slice(&s);
        }
        let mut b = vec![0u8; 128];
        b.extend_from_slice(b"DICM");
        // File meta group: TransferSyntaxUID explicit VR LE => explicit VR little endian.
        let ts = b"1.2.840.10008.1.2.1\0";
        b.extend_from_slice(&0x0002u16.to_le_bytes());
        b.extend_from_slice(&0x0010u16.to_le_bytes());
        b.extend_from_slice(b"UI");
        b.extend_from_slice(&(ts.len() as u16).to_le_bytes());
        b.extend_from_slice(ts);
        // Data set.
        us(&mut b, 0x0028, 0x0010, rows);
        us(&mut b, 0x0028, 0x0011, cols);
        us(&mut b, 0x0028, 0x0002, samples);
        cs(&mut b, 0x0028, 0x0004, photometric);
        us(&mut b, 0x0028, 0x0100, bits);
        us(&mut b, 0x0028, 0x0101, bits);
        us(&mut b, 0x0028, 0x0102, bits - 1);
        us(&mut b, 0x0028, 0x0103, 0);
        // Pixel data (OB, long form).
        b.extend_from_slice(&0x7FE0u16.to_le_bytes());
        b.extend_from_slice(&0x0010u16.to_le_bytes());
        b.extend_from_slice(b"OB");
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
        b.extend_from_slice(pixels);
        b
    }

    #[test]
    fn decodes_8bit_monochrome() {
        let b = synth(2, 2, 1, 8, "MONOCHROME2", &[0, 51, 102, 255]);
        let img = decode(&b, &Limits::default()).unwrap();
        assert_eq!((img.width(), img.height(), img.layout(), img.sample_type()), (2, 2, ChannelLayout::Gray, SampleType::U8));
        assert_eq!(img.data(), &[0, 51, 102, 255]);
    }

    #[test]
    fn decodes_16bit_monochrome_and_scales() {
        let px: Vec<u8> = [0u16, 32768, 65535].iter().flat_map(|v| v.to_le_bytes()).collect();
        let b = synth(3, 1, 1, 16, "MONOCHROME2", &px);
        let img = decode(&b, &Limits::default()).unwrap();
        let v = img.to_u16_samples().unwrap();
        assert_eq!(v[0], 0);
        assert!((i32::from(v[1]) - 32768).abs() <= 1);
        assert_eq!(v[2], 65535);
    }

    #[test]
    fn monochrome1_is_inverted() {
        let b = synth(2, 1, 1, 8, "MONOCHROME1", &[0, 255]);
        let img = decode(&b, &Limits::default()).unwrap();
        assert_eq!(img.data(), &[255, 0]);
    }

    #[test]
    fn truncated_and_bad_is_error_not_panic() {
        let b = synth(2, 2, 1, 8, "MONOCHROME2", &[0, 1, 2, 3]);
        for cut in [0, 4, 128, 132, 140, b.len() - 1] {
            assert!(decode(&b[..cut.min(b.len())], &Limits::default()).is_err(), "cut {cut}");
        }
        assert!(decode(b"not dicom", &Limits::default()).is_err());
    }

    #[test]
    fn rejects_compressed_transfer_syntax() {
        let mut b = synth(1, 1, 1, 8, "MONOCHROME2", &[0]);
        let uncompressed = b"1.2.840.10008.1.2.1\0";
        let compressed = b"1.2.840.10008.1.2.4\0"; // same length, a compressed family
        let at = b.windows(uncompressed.len()).position(|w| w == uncompressed).expect("transfer syntax in the fixture");
        b[at..at + compressed.len()].copy_from_slice(compressed);
        assert!(matches!(decode(&b, &Limits::default()), Err(CodecError::Unsupported { .. })));
    }
}
