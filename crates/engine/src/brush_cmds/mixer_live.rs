//! Incremental Mixer Brush transaction. Source pixels are composed before each dab;
//! only the selected target changes. Dropping the transaction cancels it.
use super::*;
use photocraft_color::{PixelFormat, SampleType};
use photocraft_doc::{Document, LayerId};
use photocraft_paint::dynamics::DabGenerator;
use photocraft_paint::mixer::{MixerState, apply_mixer_dab_region};
use photocraft_paint::retouch::{Region, mix_back};
use photocraft_paint::{BrushContext, Dab};
use std::collections::BTreeSet;
use std::sync::Arc;

const CMD: &str = "paint.mixerBrush";
const MAX_TILES: usize = 256;
const TILE: i32 = 256;

fn valid_color(color: &[f32; 4]) -> bool {
    color.iter().all(|v| v.is_finite() && v.abs() <= 1_000_000.0)
}

fn valid_state(state: &MixerState) -> bool {
    state.amount.is_finite()
        && state.reservoir.as_ref().is_none_or(valid_color)
        && state.pickup.as_ref().is_none_or(valid_color)
        && state.color.as_ref().is_none_or(valid_color)
        && state.spatial.as_ref().is_none_or(|colors| colors.len() == 4096 && colors.iter().all(valid_color))
}

#[derive(Clone)]
struct Core {
    doc: Arc<Document>,
    generator: DabGenerator,
    ctx: BrushContext,
    pre: Surface,
    work: Surface,
    tiles: BTreeSet<(i32, i32)>,
    preview_limits: bool,
    state: MixerState,
    settings: MixerSettings,
    layer: Option<LayerId>,
    params: Value,
    selection: Option<Surface>,
    lock: bool,
    color_restrict: Option<usize>,
    count: usize,
    last: Option<StrokePoint>,
    bounds: Rect,
}

impl Core {
    fn apply(&mut self, dabs: &[Dab]) -> Result<Rect> {
        if self.count.saturating_add(dabs.len()) > 250_000 {
            return Err(bad(CMD, "stroke exceeds 250000 dabs; start another stroke"));
        }
        let mut damage = Rect::EMPTY;
        for dab in dabs {
            let index = self.count;
            self.count += 1;
            let r = self.ctx.dab_rect(dab, false);
            if r.is_empty() {
                continue;
            }
            let full = self.ctx.dab_rect(dab, false);
            if u64::from(full.width()).saturating_mul(u64::from(full.height())) > 4_194_304 {
                return Err(bad(CMD, "Mixer Brush footprint exceeds 4194304 pixels"));
            }
            // Float working tiles avoid repeated integer-depth quantization. Initialize only
            // touched tiles, keeping unchanged source tiles shared by the document snapshots.
            for ty in r.y0.div_euclid(TILE)..=(r.y1 - 1).div_euclid(TILE) {
                for tx in r.x0.div_euclid(TILE)..=(r.x1 - 1).div_euclid(TILE) {
                    if !self.tiles.contains(&(tx, ty)) {
                        if self.preview_limits && self.tiles.len() >= MAX_TILES {
                            return Err(bad(CMD, "Mixer Brush stroke working set exceeds 256 tiles"));
                        }
                        let tile = Rect::new(tx * TILE, ty * TILE, (tx + 1) * TILE, (ty + 1) * TILE);
                        self.work.write_region(tile, &self.pre.read_region(tile));
                        self.tiles.insert((tx, ty));
                    }
                }
            }
            // Render the visible document including the active layer exactly once, before
            // deposition. Masks, transforms, blend modes and adjustments use the compositor.
            let sample = self.settings.sample_all_layers.then(|| {
                let buf = photocraft_compose::render(&self.doc, r);
                let fmt = self.pre.format();
                let mut sample = Region::new(r, fmt.channels());
                for (src, dst) in buf.px.iter().zip(sample.data.chunks_exact_mut(sample.ch)) {
                    photocraft_raster::from_rgba_into(&fmt, *src, dst);
                }
                sample
            });
            let fp = self.ctx.footprint(dab, index);
            let mut region = Region::read(&self.work, r);
            apply_mixer_dab_region(&mut region, &fp, self.pre.format(), sample.as_ref(), &self.settings, &mut self.state, self.lock);
            region.write(&mut self.work);
            let original = Region::read(&self.pre, r);
            let (target, _) = crate::channel_cmds::target_surface(Arc::make_mut(&mut self.doc), self.layer, &self.params)?;
            mix_back(target, &original, &region, r, 1.0, self.selection.as_ref(), self.lock);
            if let Some(k) = self.color_restrict {
                let n = target.channels();
                let mut values = target.read_region(r);
                let before = self.pre.read_region(r);
                for (new, old) in values.chunks_exact_mut(n).zip(before.chunks_exact(n)) {
                    for (c, (new, old)) in new.iter_mut().zip(old).enumerate() {
                        if c != k {
                            *new = *old;
                        }
                    }
                }
                target.write_region(r, &values);
            }
            damage = damage.union(&r);
        }
        self.bounds = self.bounds.union(&damage);
        Ok(damage)
    }
}

/// A real, cancellable paint result, displayed by the existing damage-aware canvas.
pub struct LiveMixer {
    pub doc: Arc<Document>,
    pub seed: u64,
    core: Core,
    state: MixerState,
    tail: Rect,
    cmyk: Option<Arc<photocraft_color::convert::CmykSpace>>,
    original: Arc<Document>,
    points: Vec<StrokePoint>,
    original_state: MixerState,
}

impl LiveMixer {
    pub fn begin(s: &Session, p: &Value) -> Result<Self> {
        Self::begin_with_limits(s, p, true)
    }

    fn begin_with_limits(s: &Session, p: &Value, preview_limits: bool) -> Result<Self> {
        has_paintable(s).map_err(EngineError::Other)?;
        if p.get("points").and_then(Value::as_array).is_some_and(|v| v.len() > 65_536) {
            return Err(bad(CMD, "stroke exceeds 65536 recorded input samples"));
        }
        let points = parse_points(p, CMD)?;
        let brush = resolve_brush(s, p, CMD)?;
        // Reject unsupported sizes before allocating a dense brush footprint.
        if brush.size > 1024.0 {
            return Err(bad(CMD, "live Mixer Brush size is limited to 1024 pixels; choose Brush for larger tips"));
        }
        let bm = &brush.mixer;
        let settings = MixerSettings {
            wet: pct(p, "wet", bm.wet * 100.0),
            load: pct(p, "load", bm.load * 100.0),
            mix: pct(p, "mix", bm.mix * 100.0),
            flow: pct(p, "flow", bm.flow * 100.0),
            sample_all_layers: flag(p, "sampleAllLayers", bm.sample_all_layers) && crate::channel_cmds::target_of(p) == crate::channel_cmds::Target::Pixels,
            clean_after_stroke: flag(p, "cleanAfterStroke", bm.clean_after_stroke),
            load_after_stroke: flag(p, "loadAfterStroke", bm.load_after_stroke),
        };
        let initial_state = if let Some(value) = p.get("mixerState") {
            if value.get("spatial").and_then(Value::as_array).is_some_and(|v| v.len() != 4096) {
                return Err(bad(CMD, "spatial reservoir must contain exactly 4096 colors"));
            }
            serde_json::from_value::<MixerState>(value.clone()).map_err(|e| bad(CMD, e.to_string()))?
        } else {
            s.tools.mixer.clone()
        };
        let mut state = initial_state;
        if !valid_state(&state) {
            return Err(bad(CMD, "loaded paint must be finite and color magnitudes at most 1000000"));
        }
        let paint = color(p.get("color"), s.tools.foreground);
        if !valid_color(&paint) {
            return Err(bad(CMD, "loaded color must be finite and magnitudes at most 1000000"));
        }
        // Earlier captured replay states had no foreground marker. Keep their loaded paint.
        if p.get("mixerState").is_some_and(|v| v.get("color").is_none()) && (state.reservoir.is_some() || state.spatial.is_some()) {
            state.color = Some(paint);
        }
        state.begin_stroke(paint);
        let layer = if crate::channel_cmds::is_channel_target(p) { None } else { Some(layer_id(s, p)?) };
        let mut doc = (*s.active().ok_or(EngineError::NoDocument)?.doc).clone();
        let selection = doc.selection.clone();
        let (target, lock) = crate::channel_cmds::target_surface(&mut doc, layer, p)?;
        let pre = target.clone();
        let work = Surface::new(PixelFormat { sample: SampleType::F32, ..pre.format() });
        let core = Core {
            doc: Arc::new(doc),
            generator: DabGenerator::new(&brush, num(p, "zoom").unwrap_or(1.0)),
            ctx: BrushContext::new(&brush),
            pre,
            work,
            tiles: BTreeSet::new(),
            preview_limits,
            state: state.clone(),
            settings,
            layer,
            params: p.clone(),
            selection,
            lock,
            color_restrict: crate::channel_cmds::color_restriction(s, CMD, p),
            count: 0,
            last: None,
            bounds: Rect::EMPTY,
        };
        let mut live = Self {
            doc: core.doc.clone(),
            seed: brush.seed,
            core,
            state,
            tail: Rect::EMPTY,
            cmyk: doc_cmyk_space(s),
            original: s.active().ok_or(EngineError::NoDocument)?.doc.clone(),
            points: Vec::new(),
            original_state: s.tools.mixer.clone(),
        };
        live.push(&points)?;
        Ok(live)
    }

    pub fn bounds(&self) -> Rect {
        self.core.bounds.union(&self.tail)
    }
    pub fn wants_time(&self) -> bool {
        self.core.generator.wants_time()
    }

    /// Batches and individual input samples produce identical results. The speculative end
    /// dab / smoothing tail is displayed but never fed into the next accepted input.
    pub fn push(&mut self, points: &[StrokePoint]) -> Result<Rect> {
        check_coords(points, CMD)?;
        if self.points.len().saturating_add(points.len()) > 65_536 {
            return Err(bad(CMD, "stroke exceeds 65536 recorded input samples"));
        }
        let cmyk = self.cmyk.clone();
        photocraft_color::convert::with_cmyk_space(cmyk.as_ref(), || {
            let old_tail = self.tail;
            let mut damage = Rect::EMPTY;
            for point in points {
                // Bound each temporary dab batch without rejecting sparse mouse events or
                // long Shift-click lines. Live and recorded replay use identical interpolation.
                let start = self.core.last.unwrap_or(*point);
                let steps = ((point.x - start.x).hypot(point.y - start.y) / 2048.0).ceil().max(1.0) as usize;
                for step in 1..=steps {
                    let f = step as f64 / steps as f64;
                    let ff = f as f32;
                    let q = if step == steps {
                        *point
                    } else {
                        StrokePoint {
                            x: start.x + (point.x - start.x) * f,
                            y: start.y + (point.y - start.y) * f,
                            pressure: start.pressure + (point.pressure - start.pressure) * ff,
                            tilt_x: start.tilt_x + (point.tilt_x - start.tilt_x) * ff,
                            tilt_y: start.tilt_y + (point.tilt_y - start.tilt_y) * ff,
                            rotation: start.rotation + (point.rotation - start.rotation) * ff,
                            wheel: start.wheel + (point.wheel - start.wheel) * ff,
                            time: start.time + (point.time - start.time) * f,
                        }
                    };
                    let (mut dabs, mut dual) = (Vec::new(), Vec::new());
                    self.core.generator.push(&[q], &mut dabs, &mut dual);
                    damage = damage.union(&self.core.apply(&dabs)?);
                }
                self.core.last = Some(*point);
            }
            let mut finished = self.core.clone();
            let (mut dabs, mut dual) = (Vec::new(), Vec::new());
            finished.generator.finish(&mut dabs, &mut dual);
            self.tail = finished.apply(&dabs)?;
            self.doc = finished.doc;
            self.state = finished.state;
            self.points.extend_from_slice(points);
            Ok(damage.union(&old_tail).union(&self.tail))
        })
    }

    fn commit_params(&self) -> Result<Value> {
        let mut params = self.core.params.clone();
        params["points"] = serde_json::to_value(&self.points).map_err(|e| bad(CMD, e.to_string()))?;
        params["brush"] = serde_json::to_value(&self.core.ctx.brush).map_err(|e| bad(CMD, e.to_string()))?;
        params["seed"] = json!(self.seed);
        params["color"] = json!(self.core.ctx.brush.color);
        if params.get("mixerState").is_none() {
            params["mixerState"] = serde_json::to_value(&self.original_state).map_err(|e| bad(CMD, e.to_string()))?;
        }
        Ok(params)
    }

    /// Stage the already rendered pixels for the ordinary journaled command. Refuse stale
    /// document/tool state. The command consumes this single-use cache only for identical params.
    pub fn prepare_commit(self, session: &mut Session) -> Result<Value> {
        if !session.active().is_some_and(|st| Arc::ptr_eq(&st.doc, &self.original) && (self.core.layer.is_none() || st.active_layer == self.core.layer))
            || session.tools.mixer != self.original_state
        {
            return Err(bad(CMD, "document or loaded paint changed during the stroke"));
        }
        let params = self.commit_params()?;
        session.prepared_mixer = Some(self);
        Ok(params)
    }
}

pub(super) fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let cached = s.prepared_mixer.take().filter(|live| {
        s.active().is_some_and(|st| Arc::ptr_eq(&st.doc, &live.original) && (live.core.layer.is_none() || st.active_layer == live.core.layer))
            && s.tools.mixer == live.original_state
            && live.commit_params().is_ok_and(|params| params == *p)
    });
    let live = match cached {
        Some(live) => live,
        None => LiveMixer::begin_with_limits(s, p, false)?,
    };
    let damage = live.bounds();
    s.edit("Mixer Brush", |doc, _| {
        *doc = (*live.doc).clone();
        Ok(())
    })?;
    let mut state = live.state;
    let paint = live.core.ctx.brush.color;
    state.end_stroke(paint, live.core.settings.clean_after_stroke, live.core.settings.load_after_stroke);
    s.tools.mixer = state;
    Ok(damage_json(s, damage))
}

/// Sampling changes only session tool state, never document pixels or document Undo.
pub(super) fn sample(s: &mut Session, p: &Value) -> Result<Value> {
    if p.get("points").and_then(Value::as_array).is_some_and(|v| v.len() != 1) {
        return Err(bad("brush.mixer.sample", "exactly one sample point is required"));
    }
    let points = parse_points(p, "brush.mixer.sample")?;
    if points.len() != 1 {
        return Err(bad("brush.mixer.sample", "exactly one sample point is required"));
    }
    let mut brush = resolve_brush(s, p, "brush.mixer.sample")?;
    if brush.size > 1024.0 {
        return Err(bad("brush.mixer.sample", "sampling size is limited to 1024 pixels"));
    }
    brush.flow = 1.0;
    let dabs = photocraft_paint::dabs(&Stroke { brush: brush.clone(), points });
    let dab = dabs.first().ok_or_else(|| bad("brush.mixer.sample", "brush has no contact"))?;
    let ctx = BrushContext::new(&brush);
    let rect = ctx.dab_rect(dab, false);
    if u64::from(rect.width()).saturating_mul(u64::from(rect.height())) > 4_194_304 {
        return Err(bad("brush.mixer.sample", "sampling footprint is too large"));
    }
    let fp = ctx.footprint(dab, 0);
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let mut doc = crate::float_cmds::displayed(st, (0, 0)).unwrap_or_else(|| (*st.doc).clone());
    let id = if crate::channel_cmds::is_channel_target(p) { None } else { Some(layer_id(s, p)?) };
    let cmyk = doc_cmyk_space(s);
    let load = photocraft_color::convert::with_cmyk_space(cmyk.as_ref(), || -> Result<Vec<[f32; 4]>> {
        let surface = if flag(p, "sampleAllLayers", brush.mixer.sample_all_layers) && crate::channel_cmds::target_of(p) == crate::channel_cmds::Target::Pixels {
            let buf = photocraft_compose::render(&doc, rect);
            let mut surface = Surface::new(PixelFormat::RGBA32F);
            surface.write_region(rect, &buf.px.iter().flatten().copied().collect::<Vec<_>>());
            surface
        } else if crate::channel_cmds::target_of(p) == crate::channel_cmds::Target::Pixels {
            id.and_then(|id| doc.layer(id))
                .and_then(|layer| layer.surface())
                .cloned()
                .ok_or_else(|| bad("brush.mixer.sample", "current layer has no raster pixels to sample"))?
        } else {
            crate::channel_cmds::target_surface(&mut doc, id, p)?.0.clone()
        };
        let mut load = Vec::with_capacity(4096);
        let (sin, cos) = dab.angle.sin_cos();
        for row in 0..64 {
            for col in 0..64 {
                let mut u = ((col as f32 + 0.5) / 32.0 - 1.0) * dab.radius;
                let mut v = ((row as f32 + 0.5) / 32.0 - 1.0) * (dab.radius * dab.roundness).max(0.5);
                if dab.flip_x {
                    u = -u;
                }
                if dab.flip_y {
                    v = -v;
                }
                let (mut dx, mut up) = (u * cos + v * sin, u * sin - v * cos);
                if dab.proj_scale < 0.999 {
                    let (sin, cos) = dab.proj_angle.sin_cos();
                    let along = (dx * cos + up * sin) * (dab.proj_scale.max(0.05) - 1.0);
                    dx += along * cos;
                    up += along * sin;
                }
                let x = (dab.center.x as f32 + dx).floor() as i32;
                let y = (dab.center.y as f32 - up).floor() as i32;
                let mut color = surface.rgba(x, y);
                color[3] *= fp.at(x, y).clamp(0.0, 1.0);
                load.push(color);
            }
        }
        Ok(load)
    })?;
    let mut average = [0.0f32; 4];
    for c in &load {
        for k in 0..3 {
            average[k] += c[k] * c[3];
        }
        average[3] += c[3];
    }
    if average[3] > 0.0 {
        for k in 0..3 {
            average[k] /= average[3];
        }
    }
    average[3] = 1.0;
    s.tools.mixer.load(average);
    s.tools.mixer.spatial = Some(load);
    s.tools.mixer.pickup = None;
    s.tools.mixer.color = Some(s.tools.foreground);
    Ok(json!({"width":64,"height":64,"color":average}))
}
