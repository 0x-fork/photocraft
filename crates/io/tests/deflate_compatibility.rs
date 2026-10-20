//! Backend compatibility, including native-format tile data rather than ZIP assembly alone.
mod common;

use std::io::Read;

use photocraft_codecs::{ChannelLayout, Format, Image, SampleType as CodecSample};
use photocraft_color::{ColorMode, SampleType};
use photocraft_doc::{Document, Layer, LayerContent};
use photocraft_format::{SaveOptions, load_from_bytes, read_thumbnail, save_to_bytes};
use photocraft_geom::Size;
use photocraft_io::{PsdExportOptions, document_to_psd_with, import};
use photocraft_psd::Compression;
use photocraft_raster::Rgba8Image;

fn document(depth: SampleType, size: Size) -> Document {
    let mut doc = Document::new("compatibility", size, ColorMode::Rgb, depth);
    doc.layers.push(Layer::new("pixels", LayerContent::Raster(common::pattern(doc.pixel_format(), doc.bounds(), 3, false))));
    doc
}

/// Compare both decoders and generate a legacy stream without flate2 compression.
fn old_and_new_inflate(bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let old = miniz_oxide::inflate::decompress_to_vec_zlib(bytes)
        .unwrap_or_else(|e| panic!("old decoder: {:?}, header {:?}", e.status, &bytes[..bytes.len().min(8)]));
    let legacy = miniz_oxide::deflate::compress_to_vec_zlib(&old, 6);
    for stream in [bytes, legacy.as_slice()] {
        let mut new = Vec::new();
        flate2::read::ZlibDecoder::new(stream).read_to_end(&mut new).unwrap();
        assert_eq!(new, old);
    }
    (old, legacy)
}

/// Read new PNGs and PNGs with legacy-miniz IDAT data through the codec.
fn check_png(bytes: &[u8], image: &Image) {
    assert_eq!(photocraft_codecs::decode(bytes).unwrap().data(), image.data());
    let mut idat = Vec::new();
    let mut legacy_png = bytes[..8].to_vec();
    let mut idat_offset = None;
    let mut offset = 8;
    while offset < bytes.len() {
        let len = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let end = offset + 12 + len;
        if &bytes[offset + 4..offset + 8] == b"IDAT" {
            idat_offset.get_or_insert(legacy_png.len());
            idat.extend_from_slice(&bytes[offset + 8..offset + 8 + len]);
        } else {
            legacy_png.extend_from_slice(&bytes[offset..end]);
        }
        offset = end;
    }
    let (_, legacy) = old_and_new_inflate(&idat);
    let mut chunk = (legacy.len() as u32).to_be_bytes().to_vec();
    chunk.extend_from_slice(b"IDAT");
    chunk.extend_from_slice(&legacy);
    chunk.extend_from_slice(&crc32fast::hash(&chunk[4..]).to_be_bytes());
    let at = idat_offset.unwrap();
    legacy_png.splice(at..at, chunk);
    assert_eq!(photocraft_codecs::decode(&legacy_png).unwrap().data(), image.data());
}

/// New zlib-rs streams and legacy-miniz streams preserve the same document samples.
#[test]
fn png_psd_and_pcraft_decode_identically() {
    for depth in [SampleType::U8, SampleType::U16, SampleType::F32] {
        let doc = document(depth, Size::new(257, 131));
        let pixels = doc.layers[0].surface().unwrap().read_region(doc.bounds());
        if depth != SampleType::F32 {
            let sample = if depth == SampleType::U8 { CodecSample::U8 } else { CodecSample::U16 };
            let image = Image::from_normalized(257, 131, ChannelLayout::Rgba, sample, &pixels).unwrap();
            check_png(&photocraft_codecs::encode(&image, Format::Png, &Default::default()).unwrap(), &image);
        }
        let raw_opts = PsdExportOptions { merged_compression: Compression::Raw, merged_matte: false, ..Default::default() };
        let merged = document_to_psd_with(&doc, &raw_opts).0.image_data.data;
        for compression in [Compression::Zip, Compression::ZipPrediction] {
            let opts = PsdExportOptions { merged_compression: compression, merged_matte: false, ..Default::default() };
            let mut file = document_to_psd_with(&doc, &opts).0;
            // The IO exporter normally chooses Raw for float composites; also cover ZIP here.
            if depth == SampleType::F32 {
                file.image_data = photocraft_psd::ImageData::encode(compression, &merged, &file.header).unwrap();
            }
            let bytes = file.to_bytes().unwrap();
            let parsed = photocraft_psd::PsdFile::from_bytes(&bytes).unwrap();
            assert_eq!(parsed.image_data.compression, compression);
            let (inflated, legacy) = old_and_new_inflate(&parsed.image_data.data);
            // The PSD streaming helper uses a different flate2 reader from plane decoding.
            assert_eq!(photocraft_psd::compression::zip_decompress(&legacy, inflated.len()).unwrap(), inflated);
            file.image_data.data = legacy;
            for fixture in [bytes, file.to_bytes().unwrap()] {
                let parsed = photocraft_psd::PsdFile::from_bytes(&fixture).unwrap();
                assert_eq!(parsed.image_data.decode(&parsed.header).unwrap(), merged);
                let back = import("compatibility.psd", &fixture).unwrap().document;
                assert_eq!(back.layers[0].surface().unwrap().read_region(doc.bounds()), pixels);
            }
        }
        let mut thumbnail = Rgba8Image::new(7, 3);
        thumbnail.pixels = (0..84).map(|i| (i * 37) as u8).collect();
        let opts = SaveOptions { thumbnail: Some(thumbnail.clone()), ..Default::default() };
        let bytes = save_to_bytes(&doc, &opts).unwrap();
        assert!(load_from_bytes(&bytes).unwrap() == doc, "all tile bytes, {depth:?}");
        let image = Image::from_u8(7, 3, ChannelLayout::Rgba, thumbnail.pixels).unwrap();
        check_png(&read_thumbnail(&bytes).unwrap().unwrap(), &image);
    }
}
