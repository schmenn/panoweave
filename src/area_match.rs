//! Area mode of the point editor: the person draws a rough rectangle round the same surface
//! (a sign, a number plate, a wall) in both photos. The two areas are then compared exactly:
//!
//! 1. Start guess from the two rectangles (shift and zoom), with the rotation of nearby points or
//!    of the alignment as extra guesses (like the patch click mode); the best guess is picked by
//!    matching a few strong spots, widening the search when none fits.
//! 2. Spots with real structure (corners and edges in both directions: the smaller eigenvalue of the
//!    structure tensor, not a single straight edge and not a plain surface) are chosen in the left
//!    area, spread over it.
//! 3. Every spot is matched with the patch matcher (ZNCC, pyramid, sub-pixel) through the current
//!    local affine map of the area; the map is fitted again from the matches (robust least squares)
//!    and the spots are matched again with a smaller search, three rounds.
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
/// Window radius for the structure measure never exceeds this (pixels).
const TEXTURE_WINDOW_MAX: usize = 15;

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
    /// how far off the right rectangle may be, as a share of its longer side
    pub search_frac: f32,
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
            search_frac: 0.15,
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
    /// the fitted map of the area, left to right: x' = m0 x + m1 y + t0, y' = m2 x + m3 y + t1
    pub map: [f64; 6],
    pub map_source: String,
    pub points: Vec<AreaSuggestion>,
    pub error: Option<String>,
    /// remark that is not an error (few points, weak ones)
    pub note: Option<String>,
    pub log: Vec<String>,
}

impl AreaResult {
    fn empty(a: usize, b: usize) -> Self {
        AreaResult { a, b, map: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0], map_source: String::new(), points: Vec::new(), error: None, note: None, log: Vec::new() }
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
    fn apply(&self, p: P2) -> P2 {
        (self.m[0] * p.0 + self.m[1] * p.1 + self.t.0, self.m[2] * p.0 + self.m[3] * p.1 + self.t.1)
    }
    fn scale(&self) -> f64 {
        (self.m[0] * self.m[3] - self.m[1] * self.m[2]).abs().sqrt()
    }
    fn as_array(&self) -> [f64; 6] {
        [self.m[0], self.m[1], self.m[2], self.m[3], self.t.0, self.t.1]
    }
}

fn center(r: &Rect) -> P2 {
    ((r.0.0 + r.1.0) * 0.5, (r.0.1 + r.1.1) * 0.5)
}

fn size(r: &Rect) -> P2 {
    (r.1.0 - r.0.0, r.1.1 - r.0.1)
}

/// Brings the corners in order and inside an image of `w` x `h` pixels.
pub fn clamp_rect(r: Rect, w: usize, h: usize) -> Rect {
    let (x0, x1) = (r.0.0.min(r.1.0), r.0.0.max(r.1.0));
    let (y0, y1) = (r.0.1.min(r.1.1), r.0.1.max(r.1.1));
    let (mx, my) = (w as f64 - 1.0, h as f64 - 1.0);
    ((x0.clamp(0.0, mx), y0.clamp(0.0, my)), (x1.clamp(0.0, mx), y1.clamp(0.0, my)))
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

/// Spots in `rect` of `g` with real structure, best first, at least `spacing` pixels apart.
/// Returns (position, structure measure); empty when the area has nothing to hold on to.
fn pick_spots(g: &Gray, rect: &Rect, patch: usize, spacing: f64, max: usize, min_texture: f32) -> Vec<(P2, f32)> {
    let blurred = imgutil::blur(g, 1.0);
    let t = Tensor::new(&blurred);
    let r = (patch / 2).min(TEXTURE_WINDOW_MAX);
    // the patch must lie inside the area as far as it fits
    let m = (patch as f64 / 2.0).min((size(rect).0.min(size(rect).1)) * 0.5 - 1.0).max(0.0);
    let (x0, x1) = ((rect.0.0 + m).ceil() as usize, (rect.1.0 - m).floor() as usize);
    let (y0, y1) = ((rect.0.1 + m).ceil() as usize, (rect.1.1 - m).floor() as usize);
    let step = 2usize;
    let mut cands: Vec<(P2, f32)> = Vec::new();
    let mut y = y0;
    while y <= y1.min(g.h.saturating_sub(1)) {
        let mut x = x0;
        while x <= x1.min(g.w.saturating_sub(1)) {
            let v = t.min_eig(x, y, r, g.h);
            if v >= min_texture {
                cands.push(((x as f64, y as f64), v));
            }
            x += step;
        }
        y += step;
    }
    cands.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<(P2, f32)> = Vec::new();
    for c in cands {
        if out.len() >= max {
            break;
        }
        if out.iter().all(|o| (o.0.0 - c.0.0).hypot(o.0.1 - c.0.1) >= spacing) {
            out.push(c);
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

/// Matches the patch round `pa` in A against B where `aff` predicts it; the found position is
/// `pred + m (dx, dy)` because the B region is sampled through the map.
fn match_spot(c: &Ctx, aff: &Aff, pa: P2, texture: f32, cfg: &MatchConfig) -> Result<Hit, &'static str> {
    if engine::cancelled() {
        return Err("cancelled");
    }
    let sz = Region::size_for(cfg);
    let id = [1.0, 0.0, 0.0, 1.0];
    let pred = aff.apply(pa);
    let ra = click_match::sample_region(c.sa, pa, id, sz);
    let rb = click_match::sample_region(c.sb, pred, aff.m, sz);
    let m = patch_matcher::match_patch(&ra, &rb, cfg);
    if !m.valid {
        return Err(m.reason);
    }
    let (dx, dy) = (m.dx as f64, m.dy as f64);
    let pb = (pred.0 + aff.m[0] * dx + aff.m[1] * dy, pred.1 + aff.m[2] * dx + aff.m[3] * dy);
    Ok(Hit { pa, pb, score: m.score, conf: m.confidence, texture })
}

fn solve3(mut a: [[f64; 4]; 3]) -> Option<[f64; 3]> {
    for i in 0..3 {
        let p = (i..3).max_by(|&x, &y| a[x][i].abs().partial_cmp(&a[y][i].abs()).unwrap_or(std::cmp::Ordering::Equal))?;
        if a[p][i].abs() < 1e-12 {
            return None;
        }
        a.swap(i, p);
        for r in 0..3 {
            if r != i {
                let f = a[r][i] / a[i][i];
                for k in i..4 {
                    a[r][k] -= f * a[i][k];
                }
            }
        }
    }
    Some([a[0][3] / a[0][0], a[1][3] / a[1][1], a[2][3] / a[2][2]])
}

/// Least-squares affine map through the pairs (A position, B position), weighted.
fn fit_affine(pairs: &[(P2, P2, f64)]) -> Option<Aff> {
    if pairs.len() < 3 {
        return None;
    }
    let wsum: f64 = pairs.iter().map(|p| p.2).sum();
    let (ma, mb) = pairs.iter().fold(((0.0, 0.0), (0.0, 0.0)), |s, p| ((s.0.0 + p.2 * p.0.0 / wsum, s.0.1 + p.2 * p.0.1 / wsum), (s.1.0 + p.2 * p.1.0 / wsum, s.1.1 + p.2 * p.1.1 / wsum)));
    let mut n = [[0.0f64; 4]; 3];
    let mut ny = [[0.0f64; 4]; 3];
    for (a, b, w) in pairs {
        let (x, y) = (a.0 - ma.0, a.1 - ma.1);
        let row = [x, y, 1.0];
        for i in 0..3 {
            for j in 0..3 {
                n[i][j] += w * row[i] * row[j];
                ny[i][j] += w * row[i] * row[j];
            }
            n[i][3] += w * row[i] * (b.0 - mb.0);
            ny[i][3] += w * row[i] * (b.1 - mb.1);
        }
    }
    let sx = solve3(n)?;
    let sy = solve3(ny)?;
    let m = [sx[0], sx[1], sy[0], sy[1]];
    // x' = m (x - ma) + mb + (sx[2], sy[2]) (the constant is ~0 after centring)
    let t = (mb.0 + sx[2] - m[0] * ma.0 - m[1] * ma.1, mb.1 + sy[2] - m[2] * ma.0 - m[3] * ma.1);
    Some(Aff { m, t })
}

/// Robust fit: refits on the pairs within 3 x the median residual (at least 1 px), three times.
/// Fewer than 4 pairs only give a shift (the zoom and rotation of `base` stay).
fn fit_robust(pairs: &[(P2, P2, f64)], base: &Aff) -> Option<Aff> {
    if pairs.is_empty() {
        return None;
    }
    let shift_only = || {
        let mut dx: Vec<f64> = pairs.iter().map(|p| p.1.0 - base.apply(p.0).0).collect();
        let mut dy: Vec<f64> = pairs.iter().map(|p| p.1.1 - base.apply(p.0).1).collect();
        dx.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        dy.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Aff { m: base.m, t: (base.t.0 + dx[dx.len() / 2], base.t.1 + dy[dy.len() / 2]) }
    };
    if pairs.len() < 4 {
        return Some(shift_only());
    }
    let mut active: Vec<(P2, P2, f64)> = pairs.to_vec();
    let mut aff = fit_affine(&active)?;
    for _ in 0..3 {
        let res = |p: &(P2, P2, f64), a: &Aff| {
            let q = a.apply(p.0);
            (q.0 - p.1.0).hypot(q.1 - p.1.1)
        };
        let mut r: Vec<f64> = active.iter().map(|p| res(p, &aff)).collect();
        r.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let limit = (3.0 * r[r.len() / 2]).max(1.0);
        let keep: Vec<(P2, P2, f64)> = pairs.iter().filter(|p| res(p, &aff) <= limit).copied().collect();
        if keep.len() < 4 {
            break;
        }
        active = keep;
        aff = fit_affine(&active)?;
    }
    // a fit that turns the area inside out or changes its size a lot is noise
    let (s, s0) = (aff.scale(), base.scale());
    let det = aff.m[0] * aff.m[3] - aff.m[1] * aff.m[2];
    if det <= 0.0 || s < 0.7 * s0 || s > 1.4 * s0 {
        return Some(shift_only());
    }
    Some(aff)
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

/// The map that carries the centre of `ra` to the centre of `rb` with matrix `m`.
fn map_between(ra: &Rect, rb: &Rect, m: [f64; 4]) -> Aff {
    let (ca, cb) = (center(ra), center(rb));
    Aff { m, t: (cb.0 - m[0] * ca.0 - m[1] * ca.1, cb.1 - m[2] * ca.0 - m[3] * ca.1) }
}

/// Compare the area `rect_a` of photo A with the area `rect_b` of photo B. The zoom comes from the
/// two rectangles;
/// `priors` are the existing points of the pair and the alignment. `None` = cancelled.
#[allow(clippy::too_many_arguments)]
pub fn run(a: usize, b: usize, ga: &Gray, gb: &Gray, rect_a: Rect, rect_b: Rect, priors: &ClickPriors, cfg: &AreaConfig) -> Option<AreaResult> {
    let rect_a = clamp_rect(rect_a, ga.w, ga.h);
    let rect_b = clamp_rect(rect_b, gb.w, gb.h);
    let mut res = AreaResult::empty(a, b);
    let (sa_, sb_) = (size(&rect_a), size(&rect_b));
    if sa_.0.min(sa_.1) < cfg.min_side_px || sb_.0.min(sb_.1) < cfg.min_side_px {
        res.error = Some(format!("An area is too small (at least {:.0} px on each side, the working image is ~1600 px wide) – draw a bigger rectangle.", cfg.min_side_px));
        return Some(res);
    }
    // patch size: as configured, smaller for small areas, odd
    let patch = (cfg.patch.min((sa_.0.min(sa_.1) - 2.0).max(11.0) as usize) | 1).max(11);
    let area = sa_.0 * sa_.1;
    let max_points = cfg.max_points.max(1);
    let spacing = (patch as f64 * 0.5).max((area / (2.0 * max_points as f64)).sqrt());
    let spots = pick_spots(ga, &rect_a, patch, spacing, max_points * 3, cfg.min_texture);
    res.log.push(format!("area A {:.0}x{:.0} px, B {:.0}x{:.0} px, patch {patch} px, {} spots with structure", sa_.0, sa_.1, sb_.0, sb_.1, spots.len()));
    if spots.is_empty() {
        res.error = Some("The area has too little structure (a plain surface, soft shades or only one straight edge) – there is no corner to hold on to. Draw the rectangles round something with detail, e.g. a sign, a number plate or a window frame.".into());
        return Some(res);
    }
    // start guesses: zoom from the rectangles, rotation 0 / of nearby points / of the alignment
    let s0 = ((sb_.0 * sb_.1) / area).sqrt().clamp(0.05, 20.0);
    let ca = center(&rect_a);
    let mut rots: Vec<(f64, String)> = vec![(0.0, "from the two rectangles".into())];
    if let Some(j) = click_match::similarity_weighted(&priors.seeds, Some(ca), SEED_RADIUS_PX) {
        rots.push((j[2].atan2(j[0]), format!("rotation from the {} points of this pair", priors.seeds.len())));
    }
    if let Some(j) = priors.align {
        rots.push((j[2].atan2(j[0]), "rotation from the alignment".into()));
    }
    // anti-aliasing: the finer photo is smoothed to the coarser one's pixel size
    let soft_a = (s0 < 1.0 / 1.5).then(|| imgutil::blur(ga, crate::guided::soft_sigma(1.0 / s0)));
    let soft_b = (s0 > 1.5).then(|| imgutil::blur(gb, crate::guided::soft_sigma(s0)));
    let ctx = Ctx { sa: soft_a.as_ref().unwrap_or(ga), sb: soft_b.as_ref().unwrap_or(gb) };
    let r0 = ((sb_.0.max(sb_.1) as f32) * cfg.search_frac).clamp(8.0, 48.0);
    let anchors: Vec<(P2, f32)> = spots.iter().take(ANCHORS).copied().collect();
    // how well a guess matches the strong spots: (spots found, their mean score, shift to apply)
    let judge = |aff: &Aff| -> (usize, f32, Aff) {
        let hits: Vec<Hit> = anchors.iter().filter_map(|(p, t)| match_spot(&ctx, aff, *p, *t, &mcfg(patch, r0, START_MIN_SCORE)).ok()).collect();
        if hits.is_empty() {
            return (0, 0.0, *aff);
        }
        let mean = hits.iter().map(|h| h.score).sum::<f32>() / hits.len() as f32;
        let pairs: Vec<(P2, P2, f64)> = hits.iter().map(|h| (h.pa, h.pb, 1.0)).collect();
        (hits.len(), mean, fit_robust(&pairs, aff).unwrap_or(*aff))
    };
    let better = |x: &(usize, f32, Aff), y: &(usize, f32, Aff)| (x.0, x.1) > (y.0, y.1);
    let mut start: Option<((usize, f32, Aff), String)> = None;
    for (rot, why) in &rots {
        let j = judge(&map_between(&rect_a, &rect_b, rotation_map(s0, *rot)));
        if start.as_ref().is_none_or(|(s, _)| better(&j, s)) {
            start = Some((j, why.clone()));
        }
    }
    if engine::cancelled() {
        return None;
    }
    let good = |s: &(usize, f32, Aff)| s.0 >= 2.min(anchors.len()) && s.1 >= cfg.min_zncc;
    if !start.as_ref().is_some_and(|(s, _)| good(s)) {
        // weak start: search zoom and rotation around the best guess and around "upright"
        let base_rot = start.as_ref().map(|(s, _)| s.2.m[2].atan2(s.2.m[0])).unwrap_or(0.0);
        let mut sweep: Vec<Aff> = Vec::new();
        for r0_ in [base_rot, 0.0] {
            for z in SWEEP_ZOOMS {
                for k in -SWEEP_ROT_STEPS..=SWEEP_ROT_STEPS {
                    sweep.push(map_between(&rect_a, &rect_b, rotation_map(s0 * z, r0_ + (k as f64 * SWEEP_ROT_STEP_DEG).to_radians())));
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
    let ((n_start, mean_start, mut aff), why) = start.expect("there is a guess");
    res.map_source = why;
    res.log.push(format!("start: {n_start} of {} strong spots matched, mean score {mean_start:.2}, zoom x{:.3}, rotation {:.1} deg ({})", anchors.len(), aff.scale(), aff.m[2].atan2(aff.m[0]).to_degrees(), res.map_source));
    // rounds: all spots through the current map, then the map again from the matches
    let mut radii: Vec<f32> = vec![if n_start > 0 { (r0 * 0.5).clamp(4.0, 16.0) } else { r0 }];
    radii.extend(FINE_RADII);
    let mut hits: Vec<Hit> = Vec::new();
    let mut reasons: HashMap<&str, usize> = HashMap::new();
    for (round, radius) in radii.iter().enumerate() {
        let cfg_m = mcfg(patch, *radius, cfg.min_zncc);
        let out: Vec<Result<Hit, &'static str>> = spots.par_iter().map(|(p, t)| match_spot(&ctx, &aff, *p, *t, &cfg_m)).collect();
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
        if let Some(f) = fit_robust(&pairs, &aff) {
            aff = f;
        }
    }
    res.map = aff.as_array();
    // drop what does not agree with the fitted map
    let dev: Vec<(f64, f64)> = hits
        .iter()
        .map(|h| {
            let q = aff.apply(h.pa);
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
            "No match: none of the {} spots in the left area was found in the right one ({}). The two rectangles may not show the same surface, or they are too far apart – redraw them closer round the same thing.",
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

    /// B shows A through the affine map `truth` (B(q) = A(truth^-1 q)), size `w` x `h`.
    fn warp(a: &Gray, truth: &Aff, w: usize, h: usize) -> Gray {
        let det = truth.m[0] * truth.m[3] - truth.m[1] * truth.m[2];
        let inv = [truth.m[3] / det, -truth.m[1] / det, -truth.m[2] / det, truth.m[0] / det];
        let mut g = Gray::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let (dx, dy) = (x as f64 - truth.t.0, y as f64 - truth.t.1);
                let (px, py) = (inv[0] * dx + inv[1] * dy, inv[2] * dx + inv[3] * dy);
                g.data[y * w + x] = a.sample(px as f32, py as f32);
            }
        }
        g
    }

    fn scene() -> Gray {
        imgutil::to_gray(&testutil::texture(480, 360, 7))
    }

    fn rect_through(truth: &Aff, r: &Rect, jitter: P2, grow: f64) -> Rect {
        let (p, q) = (truth.apply(r.0), truth.apply(r.1));
        let c = ((p.0 + q.0) * 0.5 + jitter.0, (p.1 + q.1) * 0.5 + jitter.1);
        let (hw, hh) = ((q.0 - p.0).abs() * 0.5 * grow, (q.1 - p.1).abs() * 0.5 * grow);
        ((c.0 - hw, c.1 - hh), (c.0 + hw, c.1 + hh))
    }

    fn check(truth: Aff, tol: f64) {
        let ga = scene();
        let gb = warp(&ga, &truth, 560, 420);
        let ra: Rect = ((120.0, 90.0), (300.0, 210.0));
        // drawn by hand: the right rectangle is off by a few pixels and a few per cent too big
        let rb = rect_through(&truth, &ra, (7.0, -6.0), 1.08);
        let r = run(0, 1, &ga, &gb, ra, rb, &ClickPriors::default(), &AreaConfig::default()).expect("not cancelled");
        assert!(r.error.is_none(), "{:?} {:?}", r.error, r.log);
        assert!(r.points.len() >= 6, "{} points, {:?}", r.points.len(), r.log);
        for p in &r.points {
            let t = truth.apply(p.pa);
            let e = (p.pb.0 - t.0).hypot(p.pb.1 - t.1);
            assert!(e < tol, "point {:?} is {e:.3} px off ({:?} vs {:?}), score {}", p.pa, p.pb, t, p.score);
            // inside the drawn area, and real structure
            assert!(p.pa.0 >= ra.0.0 && p.pa.0 <= ra.1.0 && p.pa.1 >= ra.0.1 && p.pa.1 <= ra.1.1);
            assert!(p.texture >= AreaConfig::default().min_texture);
        }
    }

    #[test]
    fn a_shifted_area_gives_sub_pixel_exact_points() {
        check(Aff { m: [1.0, 0.0, 0.0, 1.0], t: (23.4, -11.7) }, 0.3);
    }

    #[test]
    fn a_zoomed_and_turned_area_gives_exact_points() {
        let (s, rot) = (1.25, 3f64.to_radians());
        check(Aff { m: rotation_map(s, rot), t: (-40.0, 17.0) }, 0.5);
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
        let r = run(0, 1, &ga, &ga, ((50.0, 40.0), (200.0, 150.0)), ((50.0, 40.0), (200.0, 150.0)), &ClickPriors::default(), &AreaConfig::default()).unwrap();
        assert!(r.points.is_empty());
        assert!(r.error.as_deref().unwrap_or("").contains("too little structure"), "{:?}", r.error);
    }

    #[test]
    fn areas_that_do_not_show_the_same_thing_say_so() {
        let ga = scene();
        let gb = imgutil::to_gray(&testutil::texture(480, 360, 99));
        let ra: Rect = ((120.0, 90.0), (300.0, 210.0));
        let r = run(0, 1, &ga, &gb, ra, ra, &ClickPriors::default(), &AreaConfig::default()).unwrap();
        assert!(r.points.is_empty(), "{} points", r.points.len());
        assert!(r.error.as_deref().unwrap_or("").starts_with("No match"), "{:?}", r.error);
    }

    #[test]
    fn a_tiny_rectangle_is_refused() {
        let ga = scene();
        let r = run(0, 1, &ga, &ga, ((10.0, 10.0), (20.0, 20.0)), ((10.0, 10.0), (20.0, 20.0)), &ClickPriors::default(), &AreaConfig::default()).unwrap();
        assert!(r.error.unwrap().contains("too small"));
    }

    #[test]
    fn the_spots_have_two_directions_of_structure() {
        // a single vertical edge is not a spot, a corner is
        let mut g = Gray::new(200, 200);
        for y in 0..200 {
            for x in 0..200 {
                g.data[y * 200 + x] = if x > 100 { 0.8 } else { 0.2 };
            }
        }
        let edge = pick_spots(&g, &((20.0, 20.0), (180.0, 180.0)), 31, 20.0, 5, 3.0e-5);
        assert!(edge.is_empty(), "{edge:?}");
        for y in 100..200 {
            for x in 0..100 {
                g.data[y * 200 + x] = 0.8;
            }
        }
        let corner = pick_spots(&g, &((20.0, 20.0), (180.0, 180.0)), 31, 20.0, 5, 3.0e-5);
        assert!(!corner.is_empty());
        assert!(corner.iter().all(|c| (c.0.0 - 100.0).abs() <= 16.0 && (c.0.1 - 100.0).abs() <= 16.0), "{corner:?}");
    }

    #[test]
    fn the_map_is_fitted_exactly() {
        let t = Aff { m: [1.1, 0.07, -0.05, 0.95], t: (13.0, -4.0) };
        let pairs: Vec<(P2, P2, f64)> = [(10.0, 10.0), (200.0, 20.0), (30.0, 150.0), (180.0, 170.0), (90.0, 80.0)].iter().map(|&p| (p, t.apply(p), 1.0)).collect();
        let f = fit_robust(&pairs, &Aff { m: [1.0, 0.0, 0.0, 1.0], t: (0.0, 0.0) }).unwrap();
        for (k, v) in t.as_array().iter().enumerate() {
            assert!((f.as_array()[k] - v).abs() < 1e-9, "{k}");
        }
    }
}
