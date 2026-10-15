//! Area type: text that wraps inside a closed path, like Photoshop's area type.
//!
//! The path is flattened once into edges. For a line's band (ascent to descent),
//! [`AreaShape::spans`] gives the intervals that are inside the shape for the whole band, left
//! to right. A shape with dents or holes gives several spans on a row, and the layout fills
//! them in order at the same baseline, as Photoshop does.

use std::ops::Range;

use photocraft_doc::vector::{FillRule, Path, PathOp};
use photocraft_geom::Point;

/// Pieces each curved segment is cut into (about 0.1 px off a 300 px bubble's curve).
const CURVE_PIECES: usize = 16;
/// Largest number of edges an area keeps, so a hostile path can't make every line scan millions.
pub const MAX_AREA_EDGES: usize = 65_536;
/// Most rows sampled inside one band; past that, evenly spread vertex rows stand in.
const MAX_BAND_SAMPLES: usize = 256;
/// Most baselines `next_row` tries before giving up.
const MAX_ROW_STEPS: usize = 1024;

#[derive(Clone, Copy, Debug)]
struct Edge {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

#[derive(Clone, Debug)]
struct Component {
    op: PathOp,
    edges: Range<usize>,
}

/// A closed path ready for line fitting, in area space (text space, or `(y, -x)` for vertical
/// type so its rows are columns).
#[derive(Clone, Debug)]
pub struct AreaShape {
    edges: Vec<Edge>,
    comps: Vec<Component>,
    rule: FillRule,
    /// Extent of the edges: `[x0, y0, x1, y1]`.
    pub bounds: [f32; 4],
}

fn cubic(s: &[Point; 4], t: f64) -> (f64, f64) {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    (a * s[0].x + b * s[1].x + c * s[2].x + d * s[3].x, a * s[0].y + b * s[1].y + c * s[2].y + d * s[3].y)
}

fn same(a: Point, b: Point) -> bool {
    (a.x - b.x).abs() < 1e-9 && (a.y - b.y).abs() < 1e-9
}

/// f64 → f32, rejecting values that are not finite as f32.
fn small(v: f64) -> Option<f32> {
    let f = v as f32;
    f.is_finite().then_some(f)
}

impl AreaShape {
    /// `None` when the path encloses nothing: no closed subpath, no height or width, an
    /// inverted (unbounded) path, coordinates that aren't finite, or more than
    /// [`MAX_AREA_EDGES`] edges.
    pub fn new(path: &Path, vertical: bool) -> Option<AreaShape> {
        if path.inverted {
            return None;
        }
        let map = |x: f64, y: f64| -> Option<(f32, f32)> { if vertical { Some((small(y)?, small(-x)?)) } else { Some((small(x)?, small(y)?)) } };
        let mut edges: Vec<Edge> = Vec::new();
        let mut comps: Vec<Component> = Vec::new();
        for range in path.components() {
            let op = path.effective_op(range.start);
            let start = edges.len();
            for sub in path.subpaths.get(range).unwrap_or(&[]) {
                if !sub.closed {
                    continue;
                }
                let mut pts: Vec<(f32, f32)> = Vec::new();
                for seg in sub.segments() {
                    if pts.is_empty() {
                        pts.push(map(seg[0].x, seg[0].y)?);
                    }
                    if same(seg[1], seg[0]) && same(seg[2], seg[3]) {
                        pts.push(map(seg[3].x, seg[3].y)?);
                    } else {
                        for j in 1..=CURVE_PIECES {
                            let (x, y) = cubic(&seg, j as f64 / CURVE_PIECES as f64);
                            pts.push(map(x, y)?);
                        }
                    }
                    if edges.len().saturating_add(pts.len()) > MAX_AREA_EDGES {
                        return None;
                    }
                }
                // Closing edge back to the first point.
                if let Some(&first) = pts.first() {
                    pts.push(first);
                }
                for w in pts.windows(2) {
                    if let [(x0, y0), (x1, y1)] = *w {
                        edges.push(Edge { x0, y0, x1, y1 });
                    }
                }
                if edges.len() > MAX_AREA_EDGES {
                    return None;
                }
            }
            comps.push(Component { op, edges: start..edges.len() });
        }
        let mut b = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
        for e in &edges {
            b = [b[0].min(e.x0).min(e.x1), b[1].min(e.y0).min(e.y1), b[2].max(e.x0).max(e.x1), b[3].max(e.y0).max(e.y1)];
        }
        if edges.is_empty() || !(b[2] - b[0] >= 1.0 && b[3] - b[1] >= 1.0) {
            return None;
        }
        Some(AreaShape { edges, comps, rule: path.fill_rule, bounds: b })
    }

    fn filled(&self, winding: i32) -> bool {
        match self.rule {
            FillRule::NonZero => winding != 0,
            FillRule::EvenOdd => winding & 1 != 0,
        }
    }

    /// Inside intervals of one component on row `y` (half-open in y, so a vertex is counted once).
    fn component_intervals(&self, c: &Component, y: f32) -> Vec<(f32, f32)> {
        let mut xs: Vec<(f32, i32)> = Vec::new();
        for e in self.edges.get(c.edges.clone()).unwrap_or(&[]) {
            let (lo, hi, dir) = if e.y0 < e.y1 { (e.y0, e.y1, 1) } else { (e.y1, e.y0, -1) };
            if lo <= y && y < hi {
                let t = (y - e.y0) / (e.y1 - e.y0);
                xs.push((e.x0 + t * (e.x1 - e.x0), dir));
            }
        }
        xs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut out = Vec::new();
        let (mut winding, mut start) = (0i32, 0.0f32);
        for (x, dir) in xs {
            let was = self.filled(winding);
            winding = winding.saturating_add(dir);
            let now = self.filled(winding);
            if !was && now {
                start = x;
            } else if was && !now && x > start {
                out.push((start, x));
            }
        }
        out
    }

    /// Inside intervals of the whole path on row `y`, components folded in with their ops.
    fn intervals_at(&self, y: f32) -> Vec<(f32, f32)> {
        let mut acc: Vec<(f32, f32)> = Vec::new();
        for c in &self.comps {
            let v = self.component_intervals(c, y);
            acc = match c.op {
                PathOp::Combine | PathOp::Join => boolean(&acc, &v, |a, b| a || b),
                PathOp::Subtract => boolean(&acc, &v, |a, b| a && !b),
                PathOp::Intersect => boolean(&acc, &v, |a, b| a && b),
                PathOp::Exclude => boolean(&acc, &v, |a, b| a != b),
            };
        }
        acc
    }

    /// Intervals inside the shape on every row of `[y0, y1]`, left to right, each at least
    /// `min_width` wide. Edges are straight, so checking the band's ends and every vertex row
    /// between them is exact.
    pub fn spans(&self, y0: f32, y1: f32, min_width: f32) -> Vec<(f32, f32)> {
        if !(y0.is_finite() && y1.is_finite()) || y1 < y0 {
            return Vec::new();
        }
        let mut ys: Vec<f32> = self.edges.iter().flat_map(|e| [e.y0, e.y1]).filter(|&y| y > y0 && y < y1).collect();
        ys.sort_by(f32::total_cmp);
        ys.dedup();
        if ys.len() > MAX_BAND_SAMPLES {
            let step = ys.len() / MAX_BAND_SAMPLES + 1;
            ys = ys.into_iter().step_by(step).collect();
        }
        ys.push(y0);
        ys.push(y1);
        let mut acc: Option<Vec<(f32, f32)>> = None;
        for y in ys {
            let v = self.intervals_at(y);
            acc = Some(match acc {
                None => v,
                Some(a) => boolean(&a, &v, |p, q| p && q),
            });
        }
        acc.unwrap_or_default().into_iter().filter(|(l, r)| r - l >= min_width).collect()
    }

    /// The first baseline at or below `from` whose band `[b - ascent, b + descent]` has spans
    /// at least `min_width` wide, with those spans. `None` once the band leaves the shape.
    pub fn next_row(&self, from: f32, ascent: f32, descent: f32, min_width: f32) -> Option<(f32, Vec<(f32, f32)>)> {
        if !(from.is_finite() && ascent.is_finite() && descent.is_finite()) {
            return None;
        }
        let step = ((self.bounds[3] - self.bounds[1]) / 512.0).max(0.5);
        let mut b = from;
        for _ in 0..MAX_ROW_STEPS {
            if b + descent > self.bounds[3] {
                return None;
            }
            let s = self.spans(b - ascent, b + descent, min_width);
            if !s.is_empty() {
                return Some((b, s));
            }
            b += step;
        }
        None
    }

    /// Is `(x, y)` (area space) inside the shape?
    pub fn contains(&self, x: f32, y: f32) -> bool {
        self.intervals_at(y).iter().any(|&(l, r)| l <= x && x < r)
    }
}

/// Boolean combination of two sorted, disjoint interval lists.
fn boolean(a: &[(f32, f32)], b: &[(f32, f32)], f: impl Fn(bool, bool) -> bool) -> Vec<(f32, f32)> {
    let mut xs: Vec<f32> = a.iter().chain(b).flat_map(|&(l, r)| [l, r]).collect();
    xs.sort_by(f32::total_cmp);
    xs.dedup();
    let inside = |set: &[(f32, f32)], x: f32| set.iter().any(|&(l, r)| l <= x && x < r);
    let mut out: Vec<(f32, f32)> = Vec::new();
    for w in xs.windows(2) {
        let &[l, r] = w else { continue };
        let mid = (l + r) * 0.5;
        if f(inside(a, mid), inside(b, mid)) {
            match out.last_mut() {
                Some(last) if last.1 == l => last.1 = r,
                _ => out.push((l, r)),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_doc::vector::{FillRule, Knot, Path, PathOp, Subpath};

    fn poly(pts: &[(f64, f64)]) -> Path {
        Path::new(vec![Subpath::polygon(pts)])
    }

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 0.01 && (a.1 - b.1).abs() < 0.01
    }

    #[test]
    fn rectangle_band_is_its_width() {
        let a = AreaShape::new(&poly(&[(0.0, 0.0), (100.0, 0.0), (100.0, 50.0), (0.0, 50.0)]), false).unwrap();
        assert_eq!(a.bounds, [0.0, 0.0, 100.0, 50.0]);
        let s = a.spans(10.0, 20.0, 1.0);
        assert_eq!(s.len(), 1);
        assert!(close(s[0], (0.0, 100.0)), "{s:?}");
        // A band leaving the shape has no span.
        assert!(a.spans(40.0, 60.0, 1.0).is_empty());
    }

    #[test]
    fn triangle_band_is_its_narrowest_row() {
        // Apex at the top: the width at y is y.
        let a = AreaShape::new(&poly(&[(50.0, 0.0), (100.0, 100.0), (0.0, 100.0)]), false).unwrap();
        let s = a.spans(40.0, 60.0, 1.0);
        assert_eq!(s.len(), 1);
        assert!(close(s[0], (30.0, 70.0)), "{s:?}");
        // Narrower than min_width: dropped.
        assert!(a.spans(5.0, 10.0, 20.0).is_empty());
    }

    #[test]
    fn hole_splits_the_row() {
        let outer = Subpath::polygon(&[(0.0, 0.0), (100.0, 0.0), (100.0, 100.0), (0.0, 100.0)]);
        let hole = Subpath::polygon(&[(40.0, 40.0), (60.0, 40.0), (60.0, 60.0), (40.0, 60.0)]).with_op(PathOp::Subtract);
        let a = AreaShape::new(&Path::new(vec![outer.clone(), hole]), false).unwrap();
        let s = a.spans(45.0, 55.0, 1.0);
        assert_eq!(s.len(), 2, "{s:?}");
        assert!(close(s[0], (0.0, 40.0)) && close(s[1], (60.0, 100.0)), "{s:?}");
        // The same hole as a joined subpath under even-odd (a custom shape's counter).
        let joined = Subpath::polygon(&[(40.0, 40.0), (60.0, 40.0), (60.0, 60.0), (40.0, 60.0)]).with_op(PathOp::Join);
        let mut p = Path::new(vec![outer, joined]);
        p.fill_rule = FillRule::EvenOdd;
        assert_eq!(AreaShape::new(&p, false).unwrap().spans(45.0, 55.0, 1.0).len(), 2);
    }

    #[test]
    fn intersect_and_exclude_follow_photoshop_ops() {
        let a = Subpath::polygon(&[(0.0, 0.0), (60.0, 0.0), (60.0, 10.0), (0.0, 10.0)]);
        let b = |op| Subpath::polygon(&[(40.0, 0.0), (100.0, 0.0), (100.0, 10.0), (40.0, 10.0)]).with_op(op);
        let i = AreaShape::new(&Path::new(vec![a.clone(), b(PathOp::Intersect)]), false).unwrap().spans(2.0, 8.0, 1.0);
        assert!(i.len() == 1 && close(i[0], (40.0, 60.0)), "{i:?}");
        let x = AreaShape::new(&Path::new(vec![a, b(PathOp::Exclude)]), false).unwrap().spans(2.0, 8.0, 1.0);
        assert!(x.len() == 2 && close(x[0], (0.0, 40.0)) && close(x[1], (60.0, 100.0)), "{x:?}");
    }

    #[test]
    fn curves_are_flattened() {
        // A circle of radius 50 centred on (50, 50) from four kappa curves.
        let k = 0.552_284_75 * 50.0;
        let c = |x: f64, y: f64, ix: f64, iy: f64, ox: f64, oy: f64| Knot::smooth(
            photocraft_geom::Point::new(x, y),
            photocraft_geom::Point::new(ix, iy),
            photocraft_geom::Point::new(ox, oy),
        );
        let circle = Subpath {
            closed: true,
            knots: vec![
                c(50.0, 0.0, 50.0 - k, 0.0, 50.0 + k, 0.0),
                c(100.0, 50.0, 100.0, 50.0 - k, 100.0, 50.0 + k),
                c(50.0, 100.0, 50.0 + k, 100.0, 50.0 - k, 100.0),
                c(0.0, 50.0, 0.0, 50.0 + k, 0.0, 50.0 - k),
            ],
            op: PathOp::Combine,
        };
        let a = AreaShape::new(&Path::new(vec![circle]), false).unwrap();
        let s = a.spans(49.0, 51.0, 1.0);
        assert!(s.len() == 1 && s[0].0 < 1.0 && s[0].1 > 99.0, "{s:?}");
        assert!(a.contains(50.0, 50.0) && !a.contains(3.0, 3.0));
    }

    #[test]
    fn next_row_skips_the_narrow_top() {
        let a = AreaShape::new(&poly(&[(50.0, 0.0), (100.0, 100.0), (0.0, 100.0)]), false).unwrap();
        // Ascent 8: the band's top (b - 8) needs width ≥ 30, so b ≥ 38.
        let (b, s) = a.next_row(8.0, 8.0, 2.0, 30.0).unwrap();
        assert!((b - 38.0).abs() <= 0.51, "{b}");
        assert!(s[0].1 - s[0].0 >= 30.0);
        // Past the bottom: no row.
        assert!(a.next_row(99.0, 8.0, 2.0, 1.0).is_none());
    }

    #[test]
    fn vertical_maps_columns_right_to_left() {
        // Text space 100 wide, 50 tall; area space is (y, -x): x in 0..50, y in -100..0.
        let a = AreaShape::new(&poly(&[(0.0, 0.0), (100.0, 0.0), (100.0, 50.0), (0.0, 50.0)]), true).unwrap();
        assert_eq!(a.bounds, [0.0, -100.0, 50.0, 0.0]);
        let s = a.spans(-60.0, -40.0, 1.0);
        assert!(s.len() == 1 && close(s[0], (0.0, 50.0)), "{s:?}");
    }

    #[test]
    fn degenerate_and_hostile_paths_give_no_area() {
        assert!(AreaShape::new(&Path::default(), false).is_none());
        assert!(AreaShape::new(&Path::new(vec![Subpath::polyline(&[(0.0, 0.0), (10.0, 10.0), (0.0, 10.0)])]), false).is_none(), "open only");
        assert!(AreaShape::new(&poly(&[(0.0, 0.0), (10.0, 0.0), (20.0, 0.0)]), false).is_none(), "no height");
        assert!(AreaShape::new(&poly(&[(0.0, 0.0), (f64::NAN, 0.0), (5.0, 5.0)]), false).is_none());
        assert!(AreaShape::new(&poly(&[(0.0, 0.0), (1e300, 0.0), (5.0, 5.0)]), false).is_none(), "beyond f32");
        let mut inv = poly(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)]);
        inv.inverted = true;
        assert!(AreaShape::new(&inv, false).is_none(), "an inverted path is unbounded");
        let many: Vec<(f64, f64)> = (0..MAX_AREA_EDGES + 10).map(|i| ((i as f64).cos() * 50.0, (i as f64).sin() * 50.0)).collect();
        assert!(AreaShape::new(&poly(&many), false).is_none());
        // Bad bands never panic.
        let a = AreaShape::new(&poly(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)]), false).unwrap();
        assert!(a.spans(f32::NAN, 5.0, 1.0).is_empty());
        assert!(a.spans(5.0, 1.0, 1.0).is_empty());
        assert!(a.next_row(f32::INFINITY, 1.0, 1.0, 1.0).is_none());
    }
}
