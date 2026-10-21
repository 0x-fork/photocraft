//! Reproducible CPU-engine latency probe; not a hardware pointer-to-display qualification.
use photocraft_engine::{Session, brush_cmds::mixer_live::LiveMixer};
use photocraft_geom::Rect;
use photocraft_paint::{
    BrushSettings, Stroke, StrokePoint,
    mixer::{MixerState, apply_mixer_stroke},
};
use photocraft_raster::Surface;
use serde_json::json;
use std::time::Instant;

/// Measure live updates and prepared commits on a synthetic 24 MP document against a frozen-source reference.
/// These CPU timings do not measure hardware input or display latency.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut results = Vec::new();
    for size in [16.0, 64.0, 256.0] {
        let mut s = Session::new();
        s.execute("file.new", json!({"width":6000,"height":4000,"background":"#808080"}))?;
        s.execute("layer.new.layer", json!({"name":"Source"}))?;
        s.edit("fixture", |doc, id| {
            if let Some(layer) = id.and_then(|id| doc.layer_mut(id))
                && let Some(surface) = layer.surface_mut()
            {
                surface.fill_rect(Rect::new(0, 0, 500, 800), &[1.0, 0.0, 0.0, 1.0]);
                surface.fill_rect(Rect::new(500, 0, 1000, 800), &[0.0, 1.0, 0.0, 1.0]);
            }
            Ok(())
        })?;
        s.execute("layer.new.layer", json!({"name":"Paint"}))?;
        let baseline_doc = s.active().ok_or("no document")?.doc.clone();
        let params = json!({"points":[[200,400]],"size":size,"seed":42,"wet":100,"mix":85,"brush":{"pressureSize":false,"spacing":0.25}});
        let start = Instant::now();
        let mut live = LiveMixer::begin(&s, &params)?;
        let begin_ms = start.elapsed().as_secs_f64() * 1000.0;
        let mut times = Vec::new();
        let mut points = vec![StrokePoint::new(200.0, 400.0, 1.0)];
        for i in 1..=60 {
            let point = StrokePoint::new(200.0 + 10.0 * f64::from(i), 400.0 + 20.0 * (f64::from(i) / 10.0).sin(), 1.0);
            let start = Instant::now();
            live.push(&[point])?;
            times.push(start.elapsed().as_secs_f64() * 1000.0);
            points.push(point);
        }
        let area = live.bounds();
        let commit = live.prepare_commit(&mut s)?;
        let start = Instant::now();
        s.execute("paint.mixerBrush", commit)?;
        let commit_ms = start.elapsed().as_secs_f64() * 1000.0;
        let mut baseline = Surface::new(photocraft_color::PixelFormat::RGBA8);
        let mut state = MixerState::default();
        state.load(s.tools.foreground);
        let stroke = Stroke { brush: BrushSettings { size, pressure_size: false, spacing: 0.25, ..Default::default() }, points };
        let settings = photocraft_paint::MixerSettings { wet: 1.0, mix: 0.85, ..Default::default() };
        // Baseline's historical frozen-composite path, including its one source composition.
        let start = Instant::now();
        let buffer = photocraft_compose::render(&baseline_doc, area);
        let mut source = Surface::new(photocraft_color::PixelFormat::RGBA32F);
        source.write_region(area, &buffer.px.iter().flatten().copied().collect::<Vec<_>>());
        apply_mixer_stroke(&mut baseline, Some(&source), &stroke, &settings, &mut state, None, false);
        let baseline_ms = start.elapsed().as_secs_f64() * 1000.0;
        times.sort_by(f64::total_cmp);
        results.push(json!({"size":size,"document":[6000,4000],"layers":3,"moves":60,"begin_ms":begin_ms,
            "move_median_ms":times.get(30),"move_p95_ms":times.get(57),"move_max_ms":times.last(),
            "prepared_commit_ms":commit_ms,"baseline_frozen_composite_full_stroke_ms":baseline_ms}));
    }
    println!("{}", serde_json::to_string_pretty(&json!({"scope":"CPU engine only; no OS, tablet, display, or GPU latency claim","results":results}))?);
    Ok(())
}
