//! Area mode of the point editor: the person draws a rough quadrilateral (a rectangle that can be
//! pulled into a perspective shape) round the same surface (a sign, a number plate of a car seen
//! from the side, a wall) in both photos. The two areas are then compared exactly:
//!
//! 1. Start guess: the projective map (homography) that carries the four corners of the left area
//!    onto the four corners of the right one, with the shift/zoom/rotation of nearby points or of the
//!    alignment as extra guesses (like the patch click mode); the best guess is picked by matching a
//!    few strong spots, widening the search when none fits. The corners may be off by a share of the
//!    area (`corner_tolerance`): the fit below corrects that.
//! 2. Spots are chosen in the left area where the structure is strong over the whole patch (the
//!    smaller eigenvalue of the structure tensor: not a single straight edge, not a plain surface)
//!    AND that are local maxima of the corner strength on a small scale, then moved onto the vertex
//!    of the edges around them (sub-pixel), so a point sits on a corner and not beside it.
//! 3. Every spot is matched with the patch matcher (ZNCC, pyramid, sub-pixel) through the local
//!    linearisation of the current homography; the homography (8 parameters) is fitted again from the
//!    matches (robust least squares, affine with few matches) and the spots are matched again with a
//!    smaller search, three rounds.
//! 4. Matches that do not agree with the fitted map are dropped; the rest come back as suggestions
//!    with their score. Weak ones are marked and not ticked.
//!
//! Everything is a suggestion: nothing is added until the person accepts it. Runs on a worker
//! thread and checks `engine::cancelled()`. Coordinates are pixels of the working images.

use crate::click_match::{self, ClickPriors};
use crate::engine;
use crate::guided::P2;
use crate::imgutil::{self, Gray};
use crate::pair_check::consistent;
use crate::patch_matcher::{self, MatchConfig, Region};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A rectangle (top left, bottom right) in working-image pixels.
pub type Rect = (P2, P2);
/// A quadrilateral in working-image pixels: top left, top right, bottom right, bottom left (as drawn).
pub type Quad = [P2; 4];

/// The corners of a rectangle as a quadrilateral.
pub fn quad_from_rect(r: Rect) -> Quad {
    let (x0, x1) = (r.0.0.min(r.1.0), r.0.0.max(r.1.0));
    let (y0, y1) = (r.0.1.min(r.1.1), r.0.1.max(r.1.1));
    [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
}

/// Existing points further than this many pixels from the area are not used for its rotation
/// (Gaussian weight with this sigma, see `click_match::similarity_weighted`).
const SEED_RADIUS_PX: f64 = 400.0;
/// With a weak start the zoom is tried at these factors and the rotation at these steps (degrees).
const SWEEP_ZOOMS: [f64; 3] = [0.9, 1.0, 1.1];
const SWEEP_ROT_STEP_DEG: f64 = 3.0;
const SWEEP_ROT_STEPS: i32 = 5;
/// Strong spots tried for each start guess.
const ANCHORS: usize = 4;
/// Matching rounds: search radius in pixels (the first one is set from the area size) and pyramid levels.
const FINE_RADII: [f32; 2] = [3.0, 1.5];
/// Minimum score for the start guess (only for choosing among the guesses, not for the result).
const START_MIN_SCORE: f32 = 0.55;
/// Window radius for the structure measure over the patch never exceeds this (pixels).
const TEXTURE_WINDOW_MAX: usize = 15;
/// Window radius of the small-scale corner strength that decides where a corner is (pixels).
const CORNER_WINDOW: usize = 3;
/// Spots are local maxima of the corner strength over this radius (pixels).
const CORNER_NMS: isize = 3;
/// Snapping a spot onto the vertex of the edges round it: window radius, largest move (pixels), rounds.
const SNAP_RADIUS: i32 = 7;
const SNAP_MAX_MOVE: f64 = 5.0;
/// A homography needs at least this many matches; fewer give an affine map (6 parameters).
const MIN_HOM_PAIRS: usize = 8;
/// A fitted map may move the corners of the area at most this share of the area's longer side
/// away from the map it started from (otherwise it is noise and a shift is used).
const MAX_CORNER_MOVE: f64 = 0.4;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AreaConfig {
    /// the Area tool in the point editor; off = the button is not there and everything is as before
    /// (also `PW_AREA=0` in the environment)
    pub enabled: bool,
    /// most points proposed for one pair of areas
    pub max_points: usize,
    /// least structure a spot needs: the smaller eigenvalue of the mean structure tensor of the
    /// grey values (0..1) in its patch. Plain surfaces and single straight edges are far below,
    /// corners and texture above.
    pub min_texture: f32,
    /// least ZNCC score (1 = perfect) of a proposed point
    pub min_zncc: f32,
    /// scores below this are proposed but marked weak and not ticked
    pub weak_zncc: f32,
    /// patch size around a spot (pixels of the left working image; smaller for small areas)
    pub patch: usize,
    /// how far off the corners of the drawn areas may be, as a share of the longer side of the area:
    /// sets how far the first search looks round the predicted place (and how far the fitted
    /// homography may correct the corners)
    pub corner_tolerance: f32,
    /// a spot must be a local maximum of the small-scale corner strength with at least this share
    /// of the strongest one in the area (so points sit on corners, not on smooth surface inside a bar)
    pub corner_rel: f32,
    /// show the corrected right quadrilateral (the left one carried over by the fitted map) after matching
    pub show_corrected: bool,
    /// rectangles shorter than this on either side (pixels) are refused
    pub min_side_px: f64,
    /// weight of a proposed point in the alignment, times its confidence
    pub weight: f64,
    /// matches further than this many sigma from the fitted map are dropped
    pub outlier_sigma: f64,
    pub sigma_floor_px: f64,
}

impl Default for AreaConfig {
    fn default() -> Self {
        let off = std::env::var("PW_AREA").is_ok_and(|v| matches!(v.trim(), "0" | "off" | "false"));
        AreaConfig {
            enabled: !off,
            max_points: 12,
            min_texture: 3.0e-5,
            min_zncc: 0.80,
            weak_zncc: 0.90,
            patch: 31,
            corner_tolerance: 0.15,
            corner_rel: 0.03,
            show_corrected: true,
            min_side_px: 16.0,
            weight: 1.5,
            outlier_sigma: 3.0,
            sigma_floor_px: 0.6,
        }
    }
}

/// One proposed point pair (working-image pixels of both photos).
#[derive(Clone, Debug)]
pub struct AreaSuggestion {
    pub pa: P2,
    pub pb: P2,
    pub score: f32,
    pub conf: f32,
    /// structure measure of the spot (smaller eigenvalue of the structure tensor)
    pub texture: f32,
    /// score below `weak_zncc`
    pub weak: bool,
    pub accept: bool,
}

#[derive(Clone, Debug)]
pub struct AreaResult {
    pub a: usize,
    pub b: usize,
    /// the fitted map at the centre of the left area, left to right, as its linearisation:
    /// x' = m0 x + m1 y + t0, y' = m2 x + m3 y + t1
    pub map: [f64; 6],
    /// the fitted homography, left to right (row by row, `h[8]` = 1)
    pub hom: [f64; 9],
    /// the left quadrilateral carried over by the fitted map = the corrected right quadrilateral
    pub fitted_b: Quad,
    pub map_source: String,
    pub points: Vec<AreaSuggestion>,
    pub error: Option<String>,
    /// remark that is not an error (few points, weak ones)
    pub note: Option<String>,
    pub log: Vec<String>,
}

impl AreaResult {
    pub(crate) fn empty(a: usize, b: usize) -> Self {
        AreaResult {
            a,
            b,
            map: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            hom: Hom::IDENTITY.0,
            fitted_b: [(0.0, 0.0); 4],
            map_source: String::new(),
            points: Vec::new(),
            error: None,
            note: None,
            log: Vec::new(),
        }
    }
    pub fn zoom(&self) -> f64 {
        (self.map[0] * self.map[3] - self.map[1] * self.map[2]).abs().sqrt()
    }
    pub fn rotation_deg(&self) -> f64 {
        self.map[2].atan2(self.map[0]).to_degrees()
    }
    pub fn mean_score(&self) -> f32 {
        if self.points.is_empty() { 0.0 } else { self.points.iter().map(|p| p.score).sum::<f32>() / self.points.len() as f32 }
    }
}

/// Affine map x' = m x + t.
#[derive(Clone, Copy, Debug)]
struct Aff {
    m: [f64; 4],
    t: P2,
}

impl Aff {
    #[cfg(test)]
    fn apply(&self, p: P2) -> P2 {
        (self.m[0] * p.0 + self.m[1] * p.1 + self.t.0, self.m[2] * p.0 + self.m[3] * p.1 + self.t.1)
    }
}

/// Projective map (homography), row by row: x' = (h0 x + h1 y + h2) / w, y' = (h3 x + h4 y + h5) / w,
/// w = h6 x + h7 y + h8.
#[derive(Clone, Copy, Debug)]
struct Hom([f64; 9]);

impl Hom {
    const IDENTITY: Hom = Hom([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);

    fn from_affine(a: &Aff) -> Hom {
        Hom([a.m[0], a.m[1], a.t.0, a.m[2], a.m[3], a.t.1, 0.0, 0.0, 1.0])
    }
    /// The denominator at `p` (positive in front of the camera).
    fn w(&self, p: P2) -> f64 {
        self.0[6] * p.0 + self.0[7] * p.1 + self.0[8]
    }
    fn apply(&self, p: P2) -> P2 {
        let (h, w) = (&self.0, self.w(p));
        ((h[0] * p.0 + h[1] * p.1 + h[2]) / w, (h[3] * p.0 + h[4] * p.1 + h[5]) / w)
    }
    /// The 2x2 Jacobian at `p`: how a small step in A turns into a step in B.
    fn jac(&self, p: P2) -> [f64; 4] {
        let (h, w) = (&self.0, self.w(p));
        let q = self.apply(p);
        [(h[0] - h[6] * q.0) / w, (h[1] - h[7] * q.0) / w, (h[3] - h[6] * q.1) / w, (h[4] - h[7] * q.1) / w]
    }
    /// The affine map that touches this one at `p`.
    fn linearised(&self, p: P2) -> Aff {
        let (m, q) = (self.jac(p), self.apply(p));
        Aff { m, t: (q.0 - m[0] * p.0 - m[1] * p.1, q.1 - m[2] * p.0 - m[3] * p.1) }
    }
    fn scale_at(&self, p: P2) -> f64 {
        let m = self.jac(p);
        (m[0] * m[3] - m[1] * m[2]).abs().sqrt()
    }
    /// The same map followed by a shift of the result.
    fn shifted(&self, d: P2) -> Hom {
        let h = &self.0;
        Hom([h[0] + d.0 * h[6], h[1] + d.0 * h[7], h[2] + d.0 * h[8], h[3] + d.1 * h[6], h[4] + d.1 * h[7], h[5] + d.1 * h[8], h[6], h[7], h[8]])
    }
}

fn mul3(a: &[f64; 9], b: &[f64; 9]) -> [f64; 9] {
    let mut c = [0.0; 9];
    for i in 0..3 {
        for j in 0..3 {
            c[i * 3 + j] = (0..3).map(|k| a[i * 3 + k] * b[k * 3 + j]).sum();
        }
    }
    c
}

fn quad_center(q: &Quad) -> P2 {
    (q.iter().map(|p| p.0).sum::<f64>() / 4.0, q.iter().map(|p| p.1).sum::<f64>() / 4.0)
}

/// Bounding box (min, max) of a quadrilateral.
fn quad_bounds(q: &Quad) -> Rect {
    let min = q.iter().fold((f64::MAX, f64::MAX), |m, p| (m.0.min(p.0), m.1.min(p.1)));
    let max = q.iter().fold((f64::MIN, f64::MIN), |m, p| (m.0.max(p.0), m.1.max(p.1)));
    (min, max)
}

/// Width and height of the bounding box.
pub fn quad_size(q: &Quad) -> P2 {
    let b = quad_bounds(q);
    (b.1.0 - b.0.0, b.1.1 - b.0.1)
}

/// Signed area (shoelace); the sign tells the winding.
fn quad_signed_area(q: &Quad) -> f64 {
    (0..4).map(|i| q[i].0 * q[(i + 1) % 4].1 - q[(i + 1) % 4].0 * q[i].1).sum::<f64>() * 0.5
}

fn cross(o: P2, a: P2, b: P2) -> f64 {
    (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
}

/// Convex, no crossing and not flat: all turns go the same way.
pub fn quad_is_convex(q: &Quad) -> bool {
    let area = quad_signed_area(q);
    area.abs() > 1.0 && (0..4).all(|i| cross(q[i], q[(i + 1) % 4], q[(i + 2) % 4]) * area > 0.0)
}

/// Is `p` inside the (convex) quadrilateral, or at most `margin` pixels outside it?
pub fn quad_contains(q: &Quad, p: P2, margin: f64) -> bool {
    let o = quad_signed_area(q).signum();
    (0..4).all(|i| {
        let (a, b) = (q[i], q[(i + 1) % 4]);
        let len = (b.0 - a.0).hypot(b.1 - a.1).max(1e-9);
        o * cross(a, b, p) / len >= -margin
    })
}

/// Clamps every corner into an image of `w` x `h` pixels. The order of the corners stays.
pub fn clamp_quad(q: Quad, w: usize, h: usize) -> Quad {
    let (mx, my) = (w as f64 - 1.0, h as f64 - 1.0);
    q.map(|p| (p.0.clamp(0.0, mx), p.1.clamp(0.0, my)))
}

// ---------------------------------------------------------------------------------------
// structure measure and choice of spots

/// Integral images of the structure tensor of a (pre-blurred) grey image.
struct Tensor {
    w: usize,
    sxx: Vec<f64>,
    syy: Vec<f64>,
    sxy: Vec<f64>,
}

impl Tensor {
    fn new(g: &Gray) -> Self {
        let (w, h) = (g.w, g.h);
        let n = (w + 1) * (h + 1);
        let (mut sxx, mut syy, mut sxy) = (vec![0.0; n], vec![0.0; n], vec![0.0; n]);
        for y in 0..h {
            let (mut rxx, mut ryy, mut rxy) = (0.0, 0.0, 0.0);
            for x in 0..w {
                let gx = (g.at_clamped(x as isize + 1, y as isize) - g.at_clamped(x as isize - 1, y as isize)) as f64 * 0.5;
                let gy = (g.at_clamped(x as isize, y as isize + 1) - g.at_clamped(x as isize, y as isize - 1)) as f64 * 0.5;
                rxx += gx * gx;
                ryy += gy * gy;
                rxy += gx * gy;
                let i = (y + 1) * (w + 1) + x + 1;
                sxx[i] = sxx[i - (w + 1)] + rxx;
                syy[i] = syy[i - (w + 1)] + ryy;
                sxy[i] = sxy[i - (w + 1)] + rxy;
            }
        }
        Tensor { w, sxx, syy, sxy }
    }

    /// Smaller eigenvalue of the mean tensor in the square of radius `r` round (x, y).
    fn min_eig(&self, x: usize, y: usize, r: usize, h: usize) -> f32 {
        let (x0, y0) = (x.saturating_sub(r), y.saturating_sub(r));
        let (x1, y1) = ((x + r + 1).min(self.w), (y + r + 1).min(h));
        let sum = |v: &[f64]| v[y1 * (self.w + 1) + x1] - v[y0 * (self.w + 1) + x1] - v[y1 * (self.w + 1) + x0] + v[y0 * (self.w + 1) + x0];
        let n = ((x1 - x0) * (y1 - y0)).max(1) as f64;
        let (a, c, b) = (sum(&self.sxx) / n, sum(&self.syy) / n, sum(&self.sxy) / n);
        let tr = a + c;
        let det = a * c - b * b;
        (0.5 * (tr - (tr * tr - 4.0 * det).max(0.0).sqrt())) as f32
    }
}

/// The vertex of the edges round `start`: the point p that minimises the sum over the window of
/// (g . (q - p))^2, g the gradient at pixel q (Gaussian weights round the current p). For a corner this
/// is its vertex, for a blob its centre. Sub-pixel; the start comes back when the window has structure
/// in one direction only, is too close to the border, or the move would exceed `SNAP_MAX_MOVE`.
/// (Same idea as `features::snap_to_corner`; kept here so the two files stay independent. When both are
/// merged, this can call that function.)
fn snap_to_vertex(g: &Gray, start: P2) -> P2 {
    let mut p = start;
    for _ in 0..2 {
        let (cx, cy) = (p.0.round() as i32, p.1.round() as i32);
        let r = SNAP_RADIUS;
        if cx - r - 1 < 0 || cy - r - 1 < 0 || cx + r + 1 >= g.w as i32 || cy + r + 1 >= g.h as i32 {
            return p;
        }
        let sig2 = 2.0 * (SNAP_RADIUS as f64 / 2.5).powi(2);
        let (mut a, mut b, mut c, mut bx, mut by) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for dy in -r..=r {
            for dx in -r..=r {
                let (px, py) = ((cx + dx) as usize, (cy + dy) as usize);
                let gx = 0.5 * (g.at(px + 1, py) - g.at(px - 1, py)) as f64;
                let gy = 0.5 * (g.at(px, py + 1) - g.at(px, py - 1)) as f64;
                let (ex, ey) = (px as f64 - p.0, py as f64 - p.1);
                let w = (-(ex * ex + ey * ey) / sig2).exp();
                let (gxx, gyy, gxy) = (w * gx * gx, w * gy * gy, w * gx * gy);
                a += gxx;
                b += gxy;
                c += gyy;
                bx += gxx * ex + gxy * ey;
                by += gxy * ex + gyy * ey;
            }
        }
        let det = a * c - b * b;
        // both directions must carry structure
        if det.is_nan() || det <= 0.02 * (a + c) * (a + c) * 0.25 {
            return p;
        }
        let (mx, my) = ((c * bx - b * by) / det, (a * by - b * bx) / det);
        if (p.0 + mx - start.0).hypot(p.1 + my - start.1) > SNAP_MAX_MOVE {
            return p;
        }
        p = (p.0 + mx, p.1 + my);
    }
    p
}

/// Spots in `quad` of `g` that sit on corners with real structure, best first, at least `spacing`
/// pixels apart: the structure over the whole patch must be strong (`min_texture`, not a plain surface
/// or one straight edge) and the spot must be a local maximum of the corner strength on a small scale
/// (at least `corner_rel` of the strongest one), then it is moved onto the vertex of the edges round it
/// (sub-pixel). Returns (position, structure measure of the patch); empty when the area has nothing to
/// hold on to.
fn pick_spots(g: &Gray, quad: &Quad, patch: usize, spacing: f64, max: usize, cfg: &AreaConfig) -> Vec<(P2, f32)> {
    let blurred = imgutil::blur(g, 1.0);
    let t = Tensor::new(&blurred);
    let r = (patch / 2).min(TEXTURE_WINDOW_MAX);
    // the spot may lie at the border of the drawn area: the corners of a sign are there
    let margin = patch as f64 * 0.25;
    let (lo, hi) = quad_bounds(quad);
    // the whole patch must lie in the photo (a spot nearer to the border cannot be matched)
    let half = (patch / 2) as f64 + 1.0;
    let (x0, y0) = ((lo.0 - margin).floor().max(half) as usize, (lo.1 - margin).floor().max(half) as usize);
    let (x1, y1) = (((hi.0 + margin).ceil().min(g.w as f64 - 1.0 - half)).max(0.0) as usize, ((hi.1 + margin).ceil().min(g.h as f64 - 1.0 - half)).max(0.0) as usize);
    if x1 <= x0 || y1 <= y0 {
        return Vec::new();
    }
    // small-scale corner strength over the box
    let bw = x1 - x0 + 1;
    let mut strength = vec![0.0f32; bw * (y1 - y0 + 1)];
    for y in y0..=y1 {
        for x in x0..=x1 {
            strength[(y - y0) * bw + (x - x0)] = t.min_eig(x, y, CORNER_WINDOW, g.h);
        }
    }
    let at = |x: isize, y: isize| -> f32 {
        if x < x0 as isize || y < y0 as isize || x > x1 as isize || y > y1 as isize { 0.0 } else { strength[(y as usize - y0) * bw + (x as usize - x0)] }
    };
    let inner: Vec<(usize, usize, f32)> = (y0..=y1)
        .flat_map(|y| (x0..=x1).map(move |x| (x, y)))
        .filter(|&(x, y)| quad_contains(quad, (x as f64, y as f64), margin))
        .map(|(x, y)| (x, y, at(x as isize, y as isize)))
        .collect();
    let vmax = inner.iter().map(|c| c.2).fold(0.0f32, f32::max);
    let floor = (cfg.corner_rel * vmax).max(1e-9);
    let mut cands: Vec<(P2, f32)> = Vec::new();
    for &(x, y, v) in &inner {
        if v < floor {
            continue;
        }
        // local maximum of the corner strength (ties: the one first in scan order)
        let is_max = (-CORNER_NMS..=CORNER_NMS).all(|dy| {
            (-CORNER_NMS..=CORNER_NMS).all(|dx| {
                let o = at(x as isize + dx, y as isize + dy);
                o < v || (o == v && (dy, dx) >= (0, 0))
            })
        });
        if !is_max {
            continue;
        }
        let texture = t.min_eig(x, y, r, g.h);
        if texture >= cfg.min_texture {
            cands.push(((x as f64, y as f64), texture.max(v)));
        }
    }
    cands.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<(P2, f32)> = Vec::new();
    for c in cands {
        if out.len() >= max {
            break;
        }
        let p = snap_to_vertex(&blurred, c.0);
        if out.iter().all(|o| (o.0.0 - p.0).hypot(o.0.1 - p.1) >= spacing) {
            out.push((p, c.1));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// matching through a map

struct Ctx<'a> {
    sa: &'a Gray,
    sb: &'a Gray,
}

fn mcfg(patch: usize, radius: f32, min_score: f32) -> MatchConfig {
    MatchConfig { patch_size: patch, search_radius: radius, min_score, pyramid_levels: if radius > 12.0 { 3 } else { 2 }, ..MatchConfig::default() }
}

/// What one spot gave.
struct Hit {
    pa: P2,
    pb: P2,
    score: f32,
    conf: f32,
    texture: f32,
}

/// Matches the patch round `pa` in A against B where `hom` predicts it; the B region is sampled
/// through the local linearisation of the map, so the found position is `pred + J (dx, dy)`.
fn match_spot(c: &Ctx, hom: &Hom, pa: P2, texture: f32, cfg: &MatchConfig) -> Result<Hit, &'static str> {
    if engine::cancelled() {
        return Err("cancelled");
    }
    let sz = Region::size_for(cfg);
    let id = [1.0, 0.0, 0.0, 1.0];
    if hom.w(pa).is_nan() || hom.w(pa) <= 1e-9 {
        return Err("patch outside the photo");
    }
    let (pred, j) = (hom.apply(pa), hom.jac(pa));
    let ra = click_match::sample_region(c.sa, pa, id, sz);
    let rb = click_match::sample_region(c.sb, pred, j, sz);
    let m = patch_matcher::match_patch(&ra, &rb, cfg);
    if !m.valid {
        return Err(m.reason);
    }
    let (dx, dy) = (m.dx as f64, m.dy as f64);
    let pb = (pred.0 + j[0] * dx + j[1] * dy, pred.1 + j[2] * dx + j[3] * dy);
    Ok(Hit { pa, pb, score: m.score, conf: m.confidence, texture })
}

/// Solves the n x n system given as n rows of n + 1 numbers (Gauss, partial pivoting).
fn gauss(mut a: Vec<Vec<f64>>) -> Option<Vec<f64>> {
    let n = a.len();
    for i in 0..n {
        let p = (i..n).max_by(|&x, &y| a[x][i].abs().partial_cmp(&a[y][i].abs()).unwrap_or(std::cmp::Ordering::Equal))?;
        if a[p][i].abs().is_nan() || a[p][i].abs() <= 1e-12 {
            return None;
        }
        a.swap(i, p);
        for r in 0..n {
            if r != i {
                let f = a[r][i] / a[i][i];
                for k in i..=n {
                    let v = a[i][k];
                    a[r][k] -= f * v;
                }
            }
        }
    }
    Some((0..n).map(|i| a[i][n] / a[i][i]).collect())
}

/// Least-squares affine map through the pairs (A position, B position), weighted.
fn fit_affine(pairs: &[(P2, P2, f64)]) -> Option<Aff> {
    if pairs.len() < 3 {
        return None;
    }
    let wsum: f64 = pairs.iter().map(|p| p.2).sum();
    let (ma, mb) = pairs.iter().fold(((0.0, 0.0), (0.0, 0.0)), |s, p| ((s.0.0 + p.2 * p.0.0 / wsum, s.0.1 + p.2 * p.0.1 / wsum), (s.1.0 + p.2 * p.1.0 / wsum, s.1.1 + p.2 * p.1.1 / wsum)));
    let mut nx = vec![vec![0.0f64; 4]; 3];
    let mut ny = vec![vec![0.0f64; 4]; 3];
    for (a, b, w) in pairs {
        let (x, y) = (a.0 - ma.0, a.1 - ma.1);
        let row = [x, y, 1.0];
        for i in 0..3 {
            for j in 0..3 {
                nx[i][j] += w * row[i] * row[j];
                ny[i][j] += w * row[i] * row[j];
            }
            nx[i][3] += w * row[i] * (b.0 - mb.0);
            ny[i][3] += w * row[i] * (b.1 - mb.1);
        }
    }
    let sx = gauss(nx)?;
    let sy = gauss(ny)?;
    let m = [sx[0], sx[1], sy[0], sy[1]];
    // x' = m (x - ma) + mb + (sx[2], sy[2]) (the constant is ~0 after centring)
    let t = (mb.0 + sx[2] - m[0] * ma.0 - m[1] * ma.1, mb.1 + sy[2] - m[2] * ma.0 - m[3] * ma.1);
    Some(Aff { m, t })
}

/// Least-squares homography through the pairs (A position, B position), weighted. The coordinates are
/// normalised first (centre 0, mean distance sqrt 2) so the 8 x 8 system stays well conditioned.
/// Four pairs give the exact map.
fn fit_hom(pairs: &[(P2, P2, f64)]) -> Option<Hom> {
    if pairs.len() < 4 {
        return None;
    }
    let n = pairs.len() as f64;
    let norm = |pts: &[P2]| -> (P2, f64) {
        let c = (pts.iter().map(|p| p.0).sum::<f64>() / n, pts.iter().map(|p| p.1).sum::<f64>() / n);
        let d = pts.iter().map(|p| (p.0 - c.0).hypot(p.1 - c.1)).sum::<f64>() / n;
        (c, if d > 1e-9 { std::f64::consts::SQRT_2 / d } else { 1.0 })
    };
    let (pa, pb): (Vec<P2>, Vec<P2>) = (pairs.iter().map(|p| p.0).collect(), pairs.iter().map(|p| p.1).collect());
    let ((ca, sa), (cb, sb)) = (norm(&pa), norm(&pb));
    let mut m = vec![vec![0.0f64; 9]; 8];
    for (a, b, w) in pairs {
        let (x, y) = ((a.0 - ca.0) * sa, (a.1 - ca.1) * sa);
        let (u, v) = ((b.0 - cb.0) * sb, (b.1 - cb.1) * sb);
        for (row, rhs) in [([x, y, 1.0, 0.0, 0.0, 0.0, -u * x, -u * y], u), ([0.0, 0.0, 0.0, x, y, 1.0, -v * x, -v * y], v)] {
            for i in 0..8 {
                for j in 0..8 {
                    m[i][j] += w * row[i] * row[j];
                }
                m[i][8] += w * row[i] * rhs;
            }
        }
    }
    let h = gauss(m)?;
    let hn = [h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], 1.0];
    let ta = [sa, 0.0, -sa * ca.0, 0.0, sa, -sa * ca.1, 0.0, 0.0, 1.0];
    let tb_inv = [1.0 / sb, 0.0, cb.0, 0.0, 1.0 / sb, cb.1, 0.0, 0.0, 1.0];
    let full = mul3(&tb_inv, &mul3(&hn, &ta));
    (full[8].abs() > 1e-12 && full.iter().all(|v| v.is_finite())).then(|| Hom(full.map(|v| v / full[8])))
}

/// The map that carries the four corners of `qa` onto those of `qb`.
fn hom_between_quads(qa: &Quad, qb: &Quad) -> Option<Hom> {
    let pairs: Vec<(P2, P2, f64)> = (0..4).map(|i| (qa[i], qb[i], 1.0)).collect();
    fit_hom(&pairs)
}

/// Is `h` a sane map for the quadrilateral `qa`: in front of the camera everywhere, the carried quad
/// still convex, and no corner further than `MAX_CORNER_MOVE` of its size from where `base` puts it?
fn plausible(h: &Hom, qa: &Quad, base: &Hom) -> bool {
    if qa.iter().any(|p| h.w(*p).is_nan() || h.w(*p) <= 1e-9 || base.w(*p).is_nan() || base.w(*p) <= 1e-9) {
        return false;
    }
    let (moved, was): (Quad, Quad) = (qa.map(|p| h.apply(p)), qa.map(|p| base.apply(p)));
    if !quad_is_convex(&moved) {
        return false;
    }
    let sz = quad_size(&was);
    let limit = MAX_CORNER_MOVE * sz.0.max(sz.1);
    (0..4).all(|i| (moved[i].0 - was[i].0).hypot(moved[i].1 - was[i].1) <= limit)
}

/// One robust fit: refits on the pairs within 3 x the median residual (at least 1 px), three times.
fn robust_loop(pairs: &[(P2, P2, f64)], use_hom: bool) -> Option<Hom> {
    let need = if use_hom { MIN_HOM_PAIRS } else { 4 };
    let fit = |p: &[(P2, P2, f64)]| -> Option<Hom> { if use_hom { fit_hom(p) } else { fit_affine(p).map(|a| Hom::from_affine(&a)) } };
    let mut active: Vec<(P2, P2, f64)> = pairs.to_vec();
    let mut h = fit(&active)?;
    for _ in 0..3 {
        let res = |p: &(P2, P2, f64), h: &Hom| {
            let q = h.apply(p.0);
            (q.0 - p.1.0).hypot(q.1 - p.1.1)
        };
        let mut r: Vec<f64> = active.iter().map(|p| res(p, &h)).collect();
        r.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let limit = (3.0 * r[r.len() / 2]).max(1.0);
        let keep: Vec<(P2, P2, f64)> = pairs.iter().filter(|p| res(p, &h) <= limit).copied().collect();
        if keep.len() < need {
            break;
        }
        active = keep;
        h = fit(&active)?;
    }
    Some(h)
}

/// The map from the matches: a homography when there are enough of them, else an affine map, else only
/// a shift of `base`. A fit that is not plausible for the area `qa` (see `plausible`) is noise and gives
/// the next simpler one.
fn fit_robust(pairs: &[(P2, P2, f64)], base: &Hom, qa: &Quad) -> Option<Hom> {
    if pairs.is_empty() {
        return None;
    }
    let shift_only = || {
        let mut dx: Vec<f64> = pairs.iter().map(|p| p.1.0 - base.apply(p.0).0).collect();
        let mut dy: Vec<f64> = pairs.iter().map(|p| p.1.1 - base.apply(p.0).1).collect();
        dx.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        dy.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        base.shifted((dx[dx.len() / 2], dy[dy.len() / 2]))
    };
    if pairs.len() >= 4 {
        for use_hom in [true, false] {
            if use_hom && pairs.len() < MIN_HOM_PAIRS {
                continue;
            }
            if let Some(h) = robust_loop(pairs, use_hom).filter(|h| plausible(h, qa, base)) {
                return Some(h);
            }
        }
    }
    Some(shift_only())
}

fn reason_text(r: &str) -> &'static str {
    match r {
        "too little texture" => "too little texture",
        "ambiguous (repeating pattern)" => "ambiguous (repeating pattern)",
        "similarity too low" => "too different between the photos",
        "best match at the edge of the search area" => "match at the edge of the search area",
        "patch outside the photo" => "outside the photo",
        "forward-backward check failed" => "not stable (forward-backward check)",
        _ => "not matched",
    }
}

fn rotation_map(s: f64, rot: f64) -> [f64; 4] {
    let (sn, c) = rot.sin_cos();
    [s * c, -s * sn, s * sn, s * c]
}

/// The map with matrix `m` that carries `ca` to `cb`.
fn map_between(ca: P2, cb: P2, m: [f64; 4]) -> Hom {
    Hom::from_affine(&Aff { m, t: (cb.0 - m[0] * ca.0 - m[1] * ca.1, cb.1 - m[2] * ca.0 - m[3] * ca.1) })
}

/// Compare the area `quad_a` of photo A with the area `quad_b` of photo B (four corners each, in the
/// same order). The start map comes from the four corners; they may be off by `corner_tolerance`, the
/// fit corrects that. `priors` are the existing points of the pair and the alignment. `None` = cancelled.
#[allow(clippy::too_many_arguments)]
pub fn run(a: usize, b: usize, ga: &Gray, gb: &Gray, quad_a: Quad, quad_b: Quad, priors: &ClickPriors, cfg: &AreaConfig) -> Option<AreaResult> {
    let quad_a = clamp_quad(quad_a, ga.w, ga.h);
    let quad_b = clamp_quad(quad_b, gb.w, gb.h);
    let mut res = AreaResult::empty(a, b);
    let (sa_, sb_) = (quad_size(&quad_a), quad_size(&quad_b));
    if sa_.0.min(sa_.1) < cfg.min_side_px || sb_.0.min(sb_.1) < cfg.min_side_px {
        res.error = Some(format!("An area is too small (at least {:.0} px on each side, the working image is ~1600 px wide) – draw a bigger area.", cfg.min_side_px));
        return Some(res);
    }
    if !quad_is_convex(&quad_a) || !quad_is_convex(&quad_b) {
        res.error = Some("An area has crossing or dented sides – pull its corners so that it is a plain four-sided shape (no corner behind another side).".into());
        return Some(res);
    }
    // patch size: as configured, smaller for small areas, odd
    let patch = (cfg.patch.min((sa_.0.min(sa_.1) - 2.0).max(11.0) as usize) | 1).max(11);
    let (area, area_b) = (quad_signed_area(&quad_a).abs(), quad_signed_area(&quad_b).abs());
    let max_points = cfg.max_points.max(1);
    let spacing = (patch as f64 * 0.5).max((area / (2.0 * max_points as f64)).sqrt());
    let spots = pick_spots(ga, &quad_a, patch, spacing, max_points * 3, cfg);
    res.log.push(format!("area A {:.0}x{:.0} px, B {:.0}x{:.0} px, patch {patch} px, {} corner spots with structure", sa_.0, sa_.1, sb_.0, sb_.1, spots.len()));
    if spots.is_empty() {
        res.error = Some("The area has too little structure (a plain surface, soft shades or only one straight edge) – there is no corner to hold on to. Draw the areas round something with detail, e.g. a sign, a number plate or a window frame.".into());
        return Some(res);
    }
    // start guesses: the four corners; shift and zoom with rotation 0 / of nearby points / of the alignment
    let s0 = (area_b / area).sqrt().clamp(0.05, 20.0);
    let (ca, cb) = (quad_center(&quad_a), quad_center(&quad_b));
    let plain = map_between(ca, cb, rotation_map(s0, 0.0));
    let mut starts: Vec<(Hom, String)> = Vec::new();
    if let Some(h) = hom_between_quads(&quad_a, &quad_b).filter(|h| plausible(h, &quad_a, &plain)) {
        starts.push((h, "from the four corners".into()));
    }
    starts.push((plain, "from the two areas (shift and zoom)".into()));
    if let Some(j) = click_match::similarity_weighted(&priors.seeds, Some(ca), SEED_RADIUS_PX) {
        starts.push((map_between(ca, cb, rotation_map(s0, j[2].atan2(j[0]))), format!("rotation from the {} points of this pair", priors.seeds.len())));
    }
    if let Some(j) = priors.align {
        starts.push((map_between(ca, cb, rotation_map(s0, j[2].atan2(j[0]))), "rotation from the alignment".into()));
    }
    // anti-aliasing: the finer photo is smoothed to the coarser one's pixel size
    let soft_a = (s0 < 1.0 / 1.5).then(|| imgutil::blur(ga, crate::guided::soft_sigma(1.0 / s0)));
    let soft_b = (s0 > 1.5).then(|| imgutil::blur(gb, crate::guided::soft_sigma(s0)));
    let ctx = Ctx { sa: soft_a.as_ref().unwrap_or(ga), sb: soft_b.as_ref().unwrap_or(gb) };
    // the corners of both drawings may be off: the first search looks that far round the prediction
    let r0 = ((sb_.0.max(sb_.1) as f32) * cfg.corner_tolerance * 1.5).clamp(8.0, 60.0);
    let anchors: Vec<(P2, f32)> = spots.iter().take(ANCHORS).copied().collect();
    // how well a guess matches the strong spots: (spots found, their mean score, map moved onto them)
    let judge = |h: &Hom| -> (usize, f32, Hom) {
        let hits: Vec<Hit> = anchors.iter().filter_map(|(p, t)| match_spot(&ctx, h, *p, *t, &mcfg(patch, r0, START_MIN_SCORE)).ok()).collect();
        if hits.is_empty() {
            return (0, 0.0, *h);
        }
        let mean = hits.iter().map(|h| h.score).sum::<f32>() / hits.len() as f32;
        let pairs: Vec<(P2, P2, f64)> = hits.iter().map(|h| (h.pa, h.pb, 1.0)).collect();
        (hits.len(), mean, fit_robust(&pairs, h, &quad_a).unwrap_or(*h))
    };
    let better = |x: &(usize, f32, Hom), y: &(usize, f32, Hom)| (x.0, x.1) > (y.0, y.1);
    let mut start: Option<((usize, f32, Hom), String)> = None;
    for (h, why) in &starts {
        let j = judge(h);
        if start.as_ref().is_none_or(|(s, _)| better(&j, s)) {
            start = Some((j, why.clone()));
        }
    }
    if engine::cancelled() {
        return None;
    }
    let good = |s: &(usize, f32, Hom)| s.0 >= 2.min(anchors.len()) && s.1 >= cfg.min_zncc;
    if !start.as_ref().is_some_and(|(s, _)| good(s)) {
        // weak start: search zoom and rotation around the best guess and around "upright"
        let base_rot = start.as_ref().map(|(s, _)| s.2.jac(ca)).map(|m| m[2].atan2(m[0])).unwrap_or(0.0);
        let mut sweep: Vec<Hom> = Vec::new();
        for r0_ in [base_rot, 0.0] {
            for z in SWEEP_ZOOMS {
                for k in -SWEEP_ROT_STEPS..=SWEEP_ROT_STEPS {
                    sweep.push(map_between(ca, cb, rotation_map(s0 * z, r0_ + (k as f64 * SWEEP_ROT_STEP_DEG).to_radians())));
                }
            }
        }
        let best = sweep.par_iter().map(&judge).max_by(|x, y| (x.0, x.1).partial_cmp(&(y.0, y.1)).unwrap_or(std::cmp::Ordering::Equal));
        if let Some(j) = best
            && start.as_ref().is_none_or(|(s, _)| better(&j, s)) {
                start = Some((j, "zoom and rotation searched".into()));
            }
    }
    if engine::cancelled() {
        return None;
    }
    let ((n_start, mean_start, mut hom), why) = start.expect("there is a guess");
    res.map_source = why;
    {
        let m = hom.jac(ca);
        res.log.push(format!("start: {n_start} of {} strong spots matched, mean score {mean_start:.2}, zoom x{:.3}, rotation {:.1} deg ({})", anchors.len(), hom.scale_at(ca), m[2].atan2(m[0]).to_degrees(), res.map_source));
    }
    // rounds: all spots through the current map, then the map again from the matches
    let mut radii: Vec<f32> = vec![if n_start > 0 { (r0 * 0.5).clamp(4.0, 24.0) } else { r0 }];
    radii.extend(FINE_RADII);
    let mut hits: Vec<Hit> = Vec::new();
    let mut reasons: HashMap<&str, usize> = HashMap::new();
    for (round, radius) in radii.iter().enumerate() {
        let cfg_m = mcfg(patch, *radius, cfg.min_zncc);
        let out: Vec<Result<Hit, &'static str>> = spots.par_iter().map(|(p, t)| match_spot(&ctx, &hom, *p, *t, &cfg_m)).collect();
        if engine::cancelled() {
            return None;
        }
        reasons.clear();
        hits.clear();
        for o in out {
            match o {
                Ok(h) => hits.push(h),
                Err(e) => *reasons.entry(e).or_insert(0) += 1,
            }
        }
        res.log.push(format!("round {} (search {radius} px): {} of {} spots matched", round + 1, hits.len(), spots.len()));
        let pairs: Vec<(P2, P2, f64)> = hits.iter().map(|h| (h.pa, h.pb, h.score as f64)).collect();
        if let Some(f) = fit_robust(&pairs, &hom, &quad_a) {
            hom = f;
        }
    }
    let lin = hom.linearised(ca);
    res.map = [lin.m[0], lin.m[1], lin.m[2], lin.m[3], lin.t.0, lin.t.1];
    res.hom = hom.0;
    res.fitted_b = quad_a.map(|p| hom.apply(p));
    // drop what does not agree with the fitted map
    let dev: Vec<(f64, f64)> = hits
        .iter()
        .map(|h| {
            let q = hom.apply(h.pa);
            (h.pb.0 - q.0, h.pb.1 - q.1)
        })
        .collect();
    let keep = consistent(&dev, cfg.outlier_sigma, cfg.sigma_floor_px);
    let outliers = keep.iter().filter(|k| !**k).count();
    if outliers > 0 {
        *reasons.entry("outlier").or_insert(0) += outliers;
    }
    let mut good_hits: Vec<Hit> = hits.into_iter().zip(keep).filter(|(_, k)| *k).map(|(h, _)| h).collect();
    good_hits.sort_by(|x, y| y.conf.partial_cmp(&x.conf).unwrap_or(std::cmp::Ordering::Equal));
    good_hits.truncate(max_points);
    let mut why_not: Vec<String> = reasons.iter().map(|(k, v)| format!("{v} {}", if *k == "outlier" { "did not fit the others" } else { reason_text(k) })).collect();
    why_not.sort();
    res.log.push(format!("{} points kept{}", good_hits.len(), if why_not.is_empty() { String::new() } else { format!("; dropped: {}", why_not.join(", ")) }));
    if good_hits.is_empty() {
        res.error = Some(format!(
            "No match: none of the {} spots in the left area was found in the right one ({}). The two areas may not show the same surface, or they are too far apart – redraw them closer round the same thing.",
            spots.len(),
            if why_not.is_empty() { "no reason given".to_string() } else { why_not.join(", ") }
        ));
        return Some(res);
    }
    res.points = good_hits
        .into_iter()
        .map(|h| {
            let weak = h.score < cfg.weak_zncc;
            AreaSuggestion { pa: h.pa, pb: h.pb, score: h.score, conf: h.conf, texture: h.texture, weak, accept: !weak }
        })
        .collect();
    let weak = res.points.iter().filter(|p| p.weak).count();
    if res.points.len() < 3 {
        res.note = Some(format!("Only {} point{} found – a few more would hold the photos better. Try a larger area with more detail.", res.points.len(), if res.points.len() == 1 { "" } else { "s" }));
    } else if weak > 0 {
        res.note = Some(format!("{weak} weak match{} (score below {:.2}) left unticked.", if weak == 1 { "" } else { "es" }, cfg.weak_zncc));
    }
    Some(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    /// The inverse of a homography (adjugate), for building test photos.
    fn inverse(h: &Hom) -> Hom {
        let m = &h.0;
        let adj = [
            m[4] * m[8] - m[5] * m[7],
            m[2] * m[7] - m[1] * m[8],
            m[1] * m[5] - m[2] * m[4],
            m[5] * m[6] - m[3] * m[8],
            m[0] * m[8] - m[2] * m[6],
            m[2] * m[3] - m[0] * m[5],
            m[3] * m[7] - m[4] * m[6],
            m[1] * m[6] - m[0] * m[7],
            m[0] * m[4] - m[1] * m[3],
        ];
        Hom(adj.map(|v| v / adj[8]))
    }

    /// B shows A through the map `truth` (B(q) = A(truth^-1 q)), size `w` x `h`.
    fn warp(a: &Gray, truth: &Hom, w: usize, h: usize) -> Gray {
        let inv = inverse(truth);
        let mut g = Gray::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let p = inv.apply((x as f64, y as f64));
                g.data[y * w + x] = a.sample(p.0 as f32, p.1 as f32);
            }
        }
        g
    }

    fn scene() -> Gray {
        imgutil::to_gray(&testutil::texture(480, 360, 7))
    }

    fn affine(m: [f64; 4], t: P2) -> Hom {
        Hom::from_affine(&Aff { m, t })
    }

    /// A rectangle drawn by hand in B: the corners of `r` carried over by `truth`, each off by `off`.
    fn quad_through(truth: &Hom, r: &Rect, off: [P2; 4]) -> Quad {
        let q = quad_from_rect(*r);
        [0, 1, 2, 3].map(|i| {
            let p = truth.apply(q[i]);
            (p.0 + off[i].0, p.1 + off[i].1)
        })
    }

    const SLOPPY: [P2; 4] = [(7.0, -6.0), (-6.0, 5.0), (8.0, 7.0), (-5.0, -8.0)];

    fn check(truth: Hom, off: [P2; 4], tol: f64, corner_tol: f64) {
        let ga = scene();
        let gb = warp(&ga, &truth, 560, 420);
        let ra: Rect = ((120.0, 90.0), (300.0, 210.0));
        let qb = quad_through(&truth, &ra, off);
        let r = run(0, 1, &ga, &gb, quad_from_rect(ra), qb, &ClickPriors::default(), &AreaConfig::default()).expect("not cancelled");
        assert!(r.error.is_none(), "{:?} {:?}", r.error, r.log);
        assert!(r.points.len() >= 6, "{} points, {:?}", r.points.len(), r.log);
        for p in &r.points {
            let t = truth.apply(p.pa);
            let e = (p.pb.0 - t.0).hypot(p.pb.1 - t.1);
            assert!(e < tol, "point {:?} is {e:.3} px off ({:?} vs {:?}), score {}", p.pa, p.pb, t, p.score);
            assert!(quad_contains(&quad_from_rect(ra), p.pa, 16.0));
            assert!(p.texture >= AreaConfig::default().min_texture);
        }
        // the corrected right quadrilateral lies on the true corners although the drawn one did not
        let want = quad_through(&truth, &ra, [(0.0, 0.0); 4]);
        for i in 0..4 {
            let e = (r.fitted_b[i].0 - want[i].0).hypot(r.fitted_b[i].1 - want[i].1);
            assert!(e < corner_tol, "corner {i} of the corrected area is {e:.2} px off, {:?}", r.log);
        }
    }

    #[test]
    fn a_shifted_area_gives_sub_pixel_exact_points() {
        check(affine([1.0, 0.0, 0.0, 1.0], (23.4, -11.7)), SLOPPY, 0.3, 2.0);
    }

    #[test]
    fn a_zoomed_and_turned_area_gives_exact_points() {
        let (s, rot) = (1.25, 3f64.to_radians());
        check(affine(rotation_map(s, rot), (-40.0, 17.0)), SLOPPY, 0.5, 2.0);
    }

    /// A surface seen at an angle (number plate of a car standing sideways): perspective, and the
    /// drawn corners are off by up to 18 px on a 180 x 120 px area (more than 10 % of the area).
    #[test]
    fn a_slanted_area_with_inexact_corners_gives_sub_pixel_points_and_corrected_corners() {
        let truth = Hom([1.1, 0.12, 20.0, 0.05, 0.95, -10.0, 0.0004, -0.0003, 1.0]);
        check(truth, [(18.0, -12.0), (-14.0, 15.0), (16.0, 18.0), (-17.0, -9.0)], 0.5, 3.0);
    }

    #[test]
    fn a_plain_area_gives_no_points_and_says_why() {
        let mut ga = Gray::new(300, 200);
        // a gentle brightness ramp: gradients in one direction only
        for y in 0..200 {
            for x in 0..300 {
                ga.data[y * 300 + x] = 0.4 + 0.0004 * x as f32;
            }
        }
        let q = quad_from_rect(((50.0, 40.0), (200.0, 150.0)));
        let r = run(0, 1, &ga, &ga, q, q, &ClickPriors::default(), &AreaConfig::default()).unwrap();
        assert!(r.points.is_empty());
        assert!(r.error.as_deref().unwrap_or("").contains("too little structure"), "{:?}", r.error);
    }

    #[test]
    fn areas_that_do_not_show_the_same_thing_say_so() {
        let ga = scene();
        let gb = imgutil::to_gray(&testutil::texture(480, 360, 99));
        let q = quad_from_rect(((120.0, 90.0), (300.0, 210.0)));
        let r = run(0, 1, &ga, &gb, q, q, &ClickPriors::default(), &AreaConfig::default()).unwrap();
        assert!(r.points.is_empty(), "{} points", r.points.len());
        assert!(r.error.as_deref().unwrap_or("").starts_with("No match"), "{:?}", r.error);
    }

    #[test]
    fn a_tiny_or_crossed_area_is_refused() {
        let ga = scene();
        let tiny = quad_from_rect(((10.0, 10.0), (20.0, 20.0)));
        let r = run(0, 1, &ga, &ga, tiny, tiny, &ClickPriors::default(), &AreaConfig::default()).unwrap();
        assert!(r.error.unwrap().contains("too small"));
        // two corners swapped: the sides cross
        let ok = quad_from_rect(((100.0, 80.0), (220.0, 180.0)));
        let crossed = [ok[0], ok[2], ok[1], ok[3]];
        assert!(!quad_is_convex(&crossed) && quad_is_convex(&ok));
        let r = run(0, 1, &ga, &ga, ok, crossed, &ClickPriors::default(), &AreaConfig::default()).unwrap();
        assert!(r.error.unwrap().contains("crossing"));
    }

    /// A bright rectangle on a dark ground (a sign): the spots lie on its corners, not inside the bar.
    #[test]
    fn the_spots_sit_on_the_corners_of_a_rectangle() {
        let (w, h) = (400usize, 300usize);
        let mut g = Gray::new(w, h);
        for y in 100..200 {
            for x in 120..300 {
                g.data[y * w + x] = 0.9;
            }
        }
        // the bar covers the pixels 120..=299 x 100..=199: its corners lie at 119.5 / 299.5 and 99.5 / 199.5
        let corners = [(119.5, 99.5), (299.5, 99.5), (119.5, 199.5), (299.5, 199.5)];
        let cfg = AreaConfig::default();
        // a rough area drawn round the bar
        let spots = pick_spots(&g, &quad_from_rect(((100.0, 80.0), (320.0, 220.0))), 31, 20.0, 8, &cfg);
        assert!(spots.len() >= 4, "{spots:?}");
        for (p, _) in &spots {
            let d = corners.iter().map(|c| (p.0 - c.0).hypot(p.1 - c.1)).fold(f64::MAX, f64::min);
            assert!(d < 0.5, "spot {p:?} is {d:.2} px from a corner ({spots:?})");
        }
        for c in corners {
            assert!(spots.iter().any(|(p, _)| (p.0 - c.0).hypot(p.1 - c.1) < 0.5), "no spot on the corner {c:?}");
        }
    }

    /// The same bar in two photos, the right one shifted: the proposed points are the corners in both.
    #[test]
    fn a_sign_gives_points_on_its_corners_in_both_photos() {
        let (w, h) = (400usize, 300usize);
        let (dx, dy) = (14usize, 9usize);
        let mut ga = Gray::new(w, h);
        let mut gb = Gray::new(w, h);
        for y in 0..h {
            for x in 0..w {
                // a dark wall with a soft pattern, and a bright bar in it
                let wall = 0.25 + 0.02 * ((x as f32 * 0.21).sin() + (y as f32 * 0.17).cos());
                let bar = |x: usize, y: usize| (120..300).contains(&x) && (100..200).contains(&y);
                ga.data[y * w + x] = if bar(x, y) { 0.9 } else { wall };
                gb.data[y * w + x] = if x >= dx && y >= dy && bar(x - dx, y - dy) { 0.9 } else { 0.25 + 0.02 * (((x - dx.min(x)) as f32 * 0.21).sin() + (((y - dy.min(y)) as f32) * 0.17).cos()) };
            }
        }
        let qa = quad_from_rect(((95.0, 75.0), (325.0, 225.0)));
        let qb = quad_from_rect(((100.0, 88.0), (338.0, 240.0)));
        let r = run(0, 1, &ga, &gb, qa, qb, &ClickPriors::default(), &AreaConfig::default()).unwrap();
        assert!(r.error.is_none(), "{:?} {:?}", r.error, r.log);
        assert!(r.points.len() >= 3, "{:?}", r.log);
        let corners = [(119.5, 99.5), (299.5, 99.5), (119.5, 199.5), (299.5, 199.5)];
        for p in &r.points {
            let d = corners.iter().map(|c| (p.pa.0 - c.0).hypot(p.pa.1 - c.1)).fold(f64::MAX, f64::min);
            assert!(d < 0.5, "point {:?} is {d:.2} px from a corner", p.pa);
            let e = (p.pb.0 - p.pa.0 - dx as f64).hypot(p.pb.1 - p.pa.1 - dy as f64);
            assert!(e < 0.5, "point {:?} -> {:?} is {e:.2} px off", p.pa, p.pb);
        }
    }

    #[test]
    fn a_single_edge_is_no_spot() {
        let mut g = Gray::new(200, 200);
        for y in 0..200 {
            for x in 0..200 {
                g.data[y * 200 + x] = if x > 100 { 0.8 } else { 0.2 };
            }
        }
        let edge = pick_spots(&g, &quad_from_rect(((20.0, 20.0), (180.0, 180.0))), 31, 20.0, 5, &AreaConfig::default());
        assert!(edge.is_empty(), "{edge:?}");
    }

    #[test]
    fn the_map_is_fitted_exactly() {
        let t = Aff { m: [1.1, 0.07, -0.05, 0.95], t: (13.0, -4.0) };
        let q = quad_from_rect(((0.0, 0.0), (210.0, 180.0)));
        let pairs: Vec<(P2, P2, f64)> = [(10.0, 10.0), (200.0, 20.0), (30.0, 150.0), (180.0, 170.0), (90.0, 80.0)].iter().map(|&p| (p, t.apply(p), 1.0)).collect();
        // five matches: an affine map
        let f = fit_robust(&pairs, &Hom::IDENTITY, &q).unwrap();
        for (k, v) in Hom::from_affine(&t).0.iter().enumerate() {
            assert!((f.0[k] - v).abs() < 1e-9, "{k}");
        }
    }

    #[test]
    fn a_homography_is_fitted_exactly_and_survives_a_wrong_match() {
        let t = Hom([1.1, 0.12, 20.0, 0.05, 0.95, -10.0, 0.0004, -0.0003, 1.0]);
        let q = quad_from_rect(((0.0, 0.0), (210.0, 180.0)));
        let mut pairs: Vec<(P2, P2, f64)> = (0..14).map(|i| ((15.0 + 13.0 * i as f64, 10.0 + 11.0 * ((i * 5) % 14) as f64), (0.0, 0.0), 1.0)).collect();
        for p in pairs.iter_mut() {
            p.1 = t.apply(p.0);
        }
        // four pairs are exact
        let h4 = hom_between_quads(&q, &q.map(|p| t.apply(p))).unwrap();
        for k in 0..9 {
            assert!((h4.0[k] - t.0[k]).abs() < 1e-8, "4 pairs, {k}");
        }
        let f = fit_robust(&pairs, &Hom::IDENTITY.shifted((20.0, -10.0)), &q).unwrap();
        for k in 0..9 {
            assert!((f.0[k] - t.0[k]).abs() < 1e-7, "{k}: {} vs {}", f.0[k], t.0[k]);
        }
        // one wrong match does not move it
        pairs[3].1 = (pairs[3].1.0 + 25.0, pairs[3].1.1 - 18.0);
        let f = fit_robust(&pairs, &Hom::IDENTITY.shifted((20.0, -10.0)), &q).unwrap();
        for p in &q {
            let (a, b) = (f.apply(*p), t.apply(*p));
            assert!((a.0 - b.0).hypot(a.1 - b.1) < 0.01, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn the_jacobian_and_the_linearisation_match_the_map() {
        let t = Hom([1.1, 0.12, 20.0, 0.05, 0.95, -10.0, 0.0004, -0.0003, 1.0]);
        let p = (100.0, 80.0);
        let lin = t.linearised(p);
        let d = (1.0, -0.5);
        let (exact, approx) = (t.apply((p.0 + d.0, p.1 + d.1)), lin.apply((p.0 + d.0, p.1 + d.1)));
        assert!((exact.0 - approx.0).hypot(exact.1 - approx.1) < 1e-3);
        let q = lin.apply(p);
        let e = t.apply(p);
        assert!((q.0 - e.0).hypot(q.1 - e.1) < 1e-9);
    }

    #[test]
    fn quad_tests() {
        let q = quad_from_rect(((10.0, 10.0), (60.0, 40.0)));
        assert!(quad_is_convex(&q));
        assert!(quad_contains(&q, (30.0, 20.0), 0.0) && !quad_contains(&q, (5.0, 20.0), 0.0) && quad_contains(&q, (8.0, 20.0), 3.0));
        // a dented quadrilateral is not convex
        assert!(!quad_is_convex(&[(0.0, 0.0), (100.0, 0.0), (10.0, 10.0), (0.0, 100.0)]));
    }
}
