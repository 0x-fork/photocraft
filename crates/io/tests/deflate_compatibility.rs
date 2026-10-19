//! Backend compatibility, including native-format tile data rather than ZIP assembly alone.
mod common;

use std::{hint::black_box, io::Read, time::Instant};

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

fn old_and_new_inflate(bytes: &[u8]) {
    let old = miniz_oxide::inflate::decompress_to_vec_zlib(bytes)
        .unwrap_or_else(|e| panic!("old decoder: {:?}, header {:?}", e.status, &bytes[..bytes.len().min(8)]));
    let mut new = Vec::new();
    flate2::read::ZlibDecoder::new(bytes).read_to_end(&mut new).unwrap();
    assert_eq!(new, old);
}

fn check_png(bytes: &[u8], image: &Image) {
    assert_eq!(photocraft_codecs::decode(bytes).unwrap().data(), image.data());
    let mut idat = Vec::new();
    let mut offset = 8;
    while offset < bytes.len() {
        let len = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        if &bytes[offset + 4..offset + 8] == b"IDAT" {
            idat.extend_from_slice(&bytes[offset + 8..offset + 8 + len]);
        }
        offset += 12 + len;
    }
    old_and_new_inflate(&idat);
}

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
        for compression in [Compression::Zip, Compression::ZipPrediction] {
            let opts = PsdExportOptions { merged_compression: compression, merged_matte: false, ..Default::default() };
            let mut file = document_to_psd_with(&doc, &opts).0;
            // The IO exporter normally chooses Raw for float composites; also cover ZIP here.
            if depth == SampleType::F32 {
                file.image_data = photocraft_psd::ImageData::encode(compression, &file.image_data.decode(&file.header).unwrap(), &file.header).unwrap();
            }
            let bytes = file.to_bytes().unwrap();
            let parsed = photocraft_psd::PsdFile::from_bytes(&bytes).unwrap();
            assert_eq!(parsed.image_data.compression, compression);
            old_and_new_inflate(&parsed.image_data.data);
            let back = import("compatibility.psd", &bytes).unwrap().document;
            assert_eq!(back.layers[0].surface().unwrap().read_region(doc.bounds()), pixels);
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

#[test]
#[ignore = "manual 24 MP release benchmark; run on main and the backend branch"]
fn benchmark_deflate_formats() {
    for pattern in ["ramp", "noise"] {
        let mut doc = document(SampleType::U8, Size::new(6000, 4000));
        if pattern == "noise" {
            let mut seed = 0x9e3779b9u32;
            let raw: Vec<u8> = (0..96_000_000usize)
                .map(|i| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    if i % 4 == 3 { 255 } else { (seed >> 24) as u8 }
                })
                .collect();
            doc.layers[0].content = LayerContent::Raster(photocraft_raster::Surface::from_interleaved(doc.pixel_format(), doc.bounds(), &raw));
        }
        let pixels = doc.layers[0].surface().unwrap().read_region(doc.bounds());
        let image = Image::from_normalized(6000, 4000, ChannelLayout::Rgba, CodecSample::U8, &pixels).unwrap();
        drop(pixels);
        let psd_opts = PsdExportOptions { merged_compression: Compression::Zip, merged_matte: false, ..Default::default() };
        let save_opts = SaveOptions { thumbnail: Some(Rgba8Image::new(256, 170)), ..Default::default() };
        let png = photocraft_codecs::encode(&image, Format::Png, &Default::default()).unwrap();
        let psd = document_to_psd_with(&doc, &psd_opts).0.to_bytes().unwrap();
        let pcraft = save_to_bytes(&doc, &save_opts).unwrap();
        for trial in 0..6 {
            for op in ["png-encode", "png-decode", "psd-encode", "psd-decode", "pcraft-save", "pcraft-load"] {
                let start = Instant::now();
                let size = match op {
                    "png-encode" => black_box(photocraft_codecs::encode(black_box(&image), Format::Png, &Default::default()).unwrap()).len(),
                    "png-decode" => black_box(photocraft_codecs::decode(black_box(&png)).unwrap()).data().len(),
                    "psd-encode" => black_box(document_to_psd_with(black_box(&doc), &psd_opts).0.to_bytes().unwrap()).len(),
                    "psd-decode" => {
                        black_box(import("bench.psd", black_box(&psd)).unwrap());
                        psd.len()
                    }
                    "pcraft-save" => black_box(save_to_bytes(black_box(&doc), &save_opts).unwrap()).len(),
                    _ => {
                        black_box(load_from_bytes(black_box(&pcraft)).unwrap());
                        pcraft.len()
                    }
                };
                println!("deflate,{pattern}-{op},{trial},{:.3},{size}", start.elapsed().as_secs_f64() * 1000.0);
            }
        }
    }
}
