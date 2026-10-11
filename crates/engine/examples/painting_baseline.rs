//! CPU baseline, one case per process: `painting_baseline <dimension> <diameter> <slow|rapid> <stamp|mixer>`.
//! Run against the pinned upstream checkout too. Timings exclude fixture construction.
use photocraft_engine::{Session, brush_cmds::LiveStroke};
use photocraft_geom::Rect;
use photocraft_paint::{BrushSettings, Stroke, StrokePoint};
use serde_json::json;
use std::time::Instant;

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn stats(mut times: Vec<f64>) -> serde_json::Value {
    if times.is_empty() {
        return serde_json::Value::Null;
    }
    times.sort_by(f64::total_cmp);
    let len = times.len();
    json!({"mean_ms":times.iter().sum::<f64>() / len as f64,
        "p50_ms":times.get((len as f64 * 0.5).ceil() as usize - 1),
        "p95_ms":times.get((len as f64 * 0.95).ceil() as usize - 1),
        "p99_ms":times.get((len as f64 * 0.99).ceil() as usize - 1),"max_ms":times.last(),"samples":len})
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let dim: i32 = args.get(1).ok_or("missing dimension")?.parse()?;
    let size: f32 = args.get(2).ok_or("missing diameter")?.parse()?;
    let speed = args.get(3).ok_or("missing speed")?;
    let mode = args.get(4).ok_or("missing mode")?;
    if ![2048, 4096, 8192].contains(&dim)
        || ![16.0, 64.0, 256.0, 1024.0].contains(&size)
        || !["slow", "rapid"].contains(&speed.as_str())
        || !["stamp", "mixer"].contains(&mode.as_str())
    {
        return Err("unsupported benchmark case".into());
    }
    let mut s = Session::new();
    s.execute("file.new", json!({"width":dim,"height":dim,"background":"#808080"}))?;
    s.execute("layer.new.layer", json!({"name":"Dense synthetic source"}))?;
    s.edit("Fixture", |doc, id| {
        if let Some(surface) = id.and_then(|id| doc.layer_mut(id)).and_then(|l| l.surface_mut()) {
            let mut pixels = vec![0.0; 256 * 256 * 4];
            for ty in (0..dim).step_by(256) {
                for tx in (0..dim).step_by(256) {
                    for (i, p) in pixels.chunks_exact_mut(4).enumerate() {
                        let x = tx + (i % 256) as i32;
                        let y = ty + (i / 256) as i32;
                        p.copy_from_slice(&[(x % 257) as f32 / 256.0, (y % 251) as f32 / 250.0, ((x ^ y) % 239) as f32 / 238.0, 0.85]);
                    }
                    surface.write_region(Rect::new(tx, ty, tx + 256, ty + 256), &pixels);
                }
            }
        }
        Ok(())
    })?;
    s.execute("layer.new.layer", json!({"name":"Paint"}))?;
    let source = s.active().ok_or("no document")?.doc.clone();
    let count = 96;
    let step = if speed == "slow" { 4.0 } else { 48.0 };
    // A bounded out-and-back path exercises many interpolated dabs without a huge bounding box.
    let points: Vec<_> = (0..=count)
        .map(|i| {
            let distance = i as f64 * step;
            let phase = distance % 1600.0;
            let x = if phase < 800.0 { phase } else { 1600.0 - phase };
            StrokePoint::new(dim as f64 / 2.0 - 400.0 + x, dim as f64 / 2.0 + (i as f64 * 0.17).sin() * 24.0, 1.0)
        })
        .collect();
    let brush = BrushSettings { size, pressure_size: false, spacing: 0.25, seed: 42, ..Default::default() };
    let dab_count = photocraft_paint::dabs(&Stroke { brush: brush.clone(), points: points.clone() }).len();
    let mut params = json!({"points":[[points[0].x,points[0].y,1.0]],"brush":brush,"seed":42,"sampleAllLayers":true});
    let mut moves = Vec::new();
    let mut begin_ms = None;
    let render_start = Instant::now();
    if mode == "stamp" {
        let start = Instant::now();
        let mut live = LiveStroke::begin(&s, &params)?;
        begin_ms = Some(ms(start));
        for point in points.iter().skip(1) {
            let start = Instant::now();
            live.push(&[*point])?;
            moves.push(ms(start));
        }
        std::hint::black_box(&live.doc);
    }
    let live_ms = ms(render_start);
    params["points"] = json!(points.iter().map(|p| [p.x, p.y, 1.0]).collect::<Vec<_>>());
    let start = Instant::now();
    s.execute(if mode == "stamp" { "paint.stroke" } else { "paint.mixerBrush" }, params)?;
    let commit_ms = ms(start);
    let painted = s.active().ok_or("no document")?.doc.clone();
    let start = Instant::now();
    s.execute("edit.undo", json!({}))?;
    let undo_ms = ms(start);
    let undo_exact = *s.active().ok_or("no document")?.doc == *source;
    let start = Instant::now();
    s.execute("edit.redo", json!({}))?;
    let redo_ms = ms(start);
    let redo_exact = *s.active().ok_or("no document")?.doc == *painted;
    println!(
        "{}",
        json!({"dimension":dim,"diameter":size,"speed":speed,"mode":mode,"input":"synthetic pressure=1, 97 samples",
        "source":"dense synthetic RGBA8, three composited layers","begin_ms":begin_ms,"engine_updates":stats(moves),
        "commit_ms":commit_ms,"undo_ms":undo_ms,"redo_ms":redo_ms,"undo_exact":undo_exact,"redo_exact":redo_exact,
        "dabs":dab_count,"dabs_per_second":dab_count as f64 / (if mode == "stamp" {live_ms} else {commit_ms} / 1000.0),
        "pointer_to_visible_ms":null,"display_frame_times":null,"gpu_peak_percent":null,
        "scope":"CPU engine only; baseline stamp commit replays the recorded stroke; mixer has no live preview"})
    );
    Ok(())
}
