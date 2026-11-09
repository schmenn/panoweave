//! Fine alignment with the PatchMatcher.
//!
//! After the normal alignment, the photos are compared patch by patch in the shared panorama
//! space (on a small flat plane tangent to the viewing direction, sampled once from each
//! working image through the current camera model):
//!
//! 1. existing control points are refined (the matching spot in photo B is re-measured),
//! 2. a grid of "control areas" is laid over every overlap (denser near the expected seam),
//! 3. all pairs go into the existing optimiser as weighted control points (robust loss),
//! 4. this repeats with a shrinking search radius until the remaining shift is tiny; a round
//!    that makes things worse is undone.
//!
//! Everything is controlled by [`FineConfig`]; with `enabled = false` nothing changes.
//! Assumptions (conservative): points placed by hand are never moved; the matcher works on the
//! working images (long side 1600 px), the same images the point finder uses.

use crate::align::{self, AlignInput, ControlPoint, CpKind, Lens, OptimizeOptions};
use crate::foliage::{self, FoliageConfig, FoliageMap};
use crate::geometry::*;
use crate::imgutil::{blur, Gray};
use crate::patch_matcher::{self, MatchConfig, MatchResult, Region, SearchTrace};
use crate::patch_store::{self, Key, Override, Status, STORE};
use image::RgbImage;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum RobustLoss {
    Huber,
    Cauchy,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FineConfig {
    /// master switch (patch_match_enabled)
    pub enabled: bool,
    /// keep every measured patch (pixels, score field) for the "Patch comparison" window; off = faster, less memory,
    /// the window then shows nothing for that round
    #[serde(default = "record_patches_default")]
    pub record_patches: bool,
    pub matcher: MatchConfig,
    /// refine existing control points
    pub refine_points: bool,
    /// remove points whose patch match fails (otherwise they keep a low weight)
    pub drop_invalid_points: bool,
    /// weight of a point whose patch match failed
    pub invalid_point_weight: f64,
    /// automatic control areas on a grid over each overlap
    pub areas: bool,
    pub grid_cell: usize,
    pub max_areas_per_pair: usize,
    /// band around the expected seam that is sampled densely (pixels)
    pub seam_band: f32,
    pub manual_area_weight: f64,
    /// sum of area weights per pair ≤ this × sum of point weights
    pub area_weight_scale: f64,
    pub robust_loss: RobustLoss,
    pub refine_iterations: usize,
    /// stop when the mean remaining shift is below this (pixels)
    pub converge_threshold: f32,
    /// a round is undone when the remaining shift grows by more than this factor
    pub worse_factor: f32,
    /// smallest search radius in later rounds
    pub min_search_radius: f32,
    /// Refining existing points follows the original's "fine-tune all points"
    /// (template size 21 px,
    /// search window = template size + local search width (14) -> +-7 px,
    /// accepted when the correlation is >= 0.8).
    #[serde(default = "point_template_default")]
    pub point_template: usize,
    #[serde(default = "point_search_default")]
    pub point_search: f32,
    #[serde(default = "point_min_score_default")]
    pub point_min_score: f32,
    /// Protected points are never moved, re-weighted or dropped; when the measured shift of one is larger than this
    /// (pixels of the working image) it is counted and reported, so the person can look at it.
    #[serde(default = "locked_warn_default")]
    pub locked_warn_px: f32,
    /// Place each control area where the photo has structure instead of on the bare grid: inside every grid cell
    /// the spot with the strongest two-direction structure is taken, cells without any are left out (and counted
    /// as "no structure in the grid cell"). Off = the plain grid of earlier versions.
    #[serde(default = "pick_textured_default")]
    pub pick_textured: bool,
    /// How far from the grid point a better spot is looked for, as a share of the grid step (0.5 = up to half a step).
    #[serde(default = "pick_reach_default")]
    pub pick_reach: f32,
    /// Distance of an automatic area from the photo border: half the patch plus this share of the search
    /// radius (1.0 = half the patch plus the whole search radius, as in earlier versions). Lower values give areas
    /// closer to the border, where the lens distortion shows most.
    #[serde(default = "edge_search_share_default")]
    pub edge_search_share: f32,
    /// Foliage (trees, bushes) moves between the shots: points and patches on it get a smaller weight in the
    /// optimiser and areas are not preferably placed on it. `enabled = false` = exactly as before.
    #[serde(default)]
    pub foliage: FoliageConfig,
}

fn pick_textured_default() -> bool {
    true
}
fn pick_reach_default() -> f32 {
    0.5
}
fn edge_search_share_default() -> f32 {
    0.5
}
fn point_template_default() -> usize {
    21
}
fn point_search_default() -> f32 {
    7.0
}
fn locked_warn_default() -> f32 {
    3.0
}
fn point_min_score_default() -> f32 {
    0.8
}
/// Environment `PW_RECORD=0` switches the recording off (experiments); on by default.
fn record_patches_default() -> bool {
    std::env::var("PW_RECORD").map(|v| v != "0").unwrap_or(true)
}

impl Default for FineConfig {
    fn default() -> Self {
        FineConfig {
            enabled: true,
            record_patches: record_patches_default(),
            locked_warn_px: locked_warn_default(),
            matcher: MatchConfig::default(),
            refine_points: true,
            drop_invalid_points: false,
            invalid_point_weight: 0.3,
            areas: true,
            grid_cell: 64,
            max_areas_per_pair: 200,
            seam_band: 100.0,
            manual_area_weight: 2.0,
            area_weight_scale: 1.0,
            robust_loss: RobustLoss::Huber,
            refine_iterations: 4,
            converge_threshold: 0.1,
            worse_factor: 1.05,
            min_search_radius: 3.0,
            point_template: point_template_default(),
            point_search: point_search_default(),
            point_min_score: point_min_score_default(),
            pick_textured: pick_textured_default(),
            pick_reach: pick_reach_default(),
            edge_search_share: edge_search_share_default(),
            foliage: FoliageConfig::default().with_env(),
        }
    }
}

/// Foliage maps of the photos.
pub fn foliage_maps(photos: &[FaPhoto]) -> Vec<FoliageMap> {
    foliage::maps_of(&photos.iter().map(|p| (&*p.rgb, &*p.gray)).collect::<Vec<_>>())
}

/// What the fine alignment needs from each photo.
#[derive(Clone)]
pub struct FaPhoto {
    pub gray: Arc<Gray>,
    pub rgb: Arc<RgbImage>,
}

#[derive(Clone, Debug, Default)]
pub struct FineStats {
    pub rounds: usize,
    pub patches_total: usize,
    pub patches_valid: usize,
    pub reasons: Vec<(String, usize)>,
    /// mean remaining shift of valid patches (working-image pixels), first and last measurement
    pub shift_before: f32,
    pub shift_after: f32,
    /// control point error (original pixels): mean, median, 95th percentile
    pub err_before: [f64; 3],
    pub err_after: [f64; 3],
    /// mean absolute grey difference in the matched patches (0..255)
    pub diff_before: f32,
    pub diff_after: f32,
    pub areas: usize,
    /// grid cells of the first round where the photo had no structure (no control area was tried there)
    pub cells_skipped: usize,
    pub points_refined: usize,
    /// points whose fine-tuning failed (correlation too low or no clear peak)
    pub points_not_updated: usize,
    /// protected points whose measured shift is above `locked_warn_px` (left as they are), and the largest shift
    pub locked_far: usize,
    pub locked_far_max: f32,
    /// the limit used for `locked_far` (from the configuration)
    pub locked_warn_px: f32,
    /// protected points without a usable match (further off than the search radius, or too little texture)
    pub locked_unmatched: usize,
    /// no control area could be measured, so no round could be judged and nothing was changed
    pub no_test_spots: bool,
    /// the last measuring round (the list shown in the patch comparison window):
    /// patches measured, valid matches, control areas kept
    pub list_total: usize,
    pub list_valid: usize,
    pub list_areas: usize,
    pub rolled_back: bool,
    pub ms: u128,
    pub log: Vec<String>,
}

/// Camera model of one photo in working-image pixels.
#[derive(Clone, Copy)]
pub struct Geo {
    pub lens: Lens,
    pub rot: M3,
    pub w: f64,
    pub h: f64,
}

impl Geo {
    pub fn long(&self) -> f64 {
        self.w.max(self.h)
    }
    /// radians per working-image pixel
    pub fn pixel_angle(&self) -> f64 {
        1.0 / (self.lens.f * self.long())
    }
    pub fn px_to_dir(&self, p: (f64, f64)) -> V3 {
        let l = self.long();
        let n = [(p.0 + 0.5 - self.w * 0.5) / l, (p.1 + 0.5 - self.h * 0.5) / l];
        mtv(&self.rot, align::ray(&self.lens, n))
    }
    pub fn dir_to_px(&self, d: V3) -> Option<(f64, f64)> {
        let n = align::project(&self.lens, mv(&self.rot, d))?;
        let l = self.long();
        Some((n[0] * l + self.w * 0.5 - 0.5, n[1] * l + self.h * 0.5 - 0.5))
    }
    pub fn inside(&self, p: (f64, f64), margin: f64) -> bool {
        p.0 >= margin && p.1 >= margin && p.0 <= self.w - 1.0 - margin && p.1 <= self.h - 1.0 - margin
    }
    /// centre weight (1 in the middle, 0 at the border)
    pub fn centre_weight(&self, p: (f64, f64)) -> f64 {
        let ex = 1.0 - (2.0 * (p.0 + 0.5) / self.w - 1.0).abs();
        let ey = 1.0 - (2.0 * (p.1 + 0.5) / self.h - 1.0).abs();
        (ex * ey).max(0.0)
    }
}

/// A flat sampling plane tangent to direction `d`, oriented like photo `a`'s image.
#[derive(Clone, Copy)]
pub struct Plane {
    pub d: V3,
    pub ex: V3,
    pub ey: V3,
    /// radians per plane pixel
    pub scale: f64,
}

impl Plane {
    pub fn new(d: V3, oriented_like: &M3, scale: f64) -> Self {
        let d = normalize(d);
        let x0 = oriented_like[0];
        let ex = normalize([x0[0] - dot(x0, d) * d[0], x0[1] - dot(x0, d) * d[1], x0[2] - dot(x0, d) * d[2]]);
        let ey = cross(d, ex);
        Plane { d, ex, ey, scale }
    }
    pub fn dir(&self, u: f64, v: f64) -> V3 {
        let s = self.scale;
        normalize([
            self.d[0] + (u * self.ex[0] + v * self.ey[0]) * s,
            self.d[1] + (u * self.ex[1] + v * self.ey[1]) * s,
            self.d[2] + (u * self.ex[2] + v * self.ey[2]) * s,
        ])
    }
    /// plane coordinates of a direction (None if behind)
    pub fn coords(&self, w: V3) -> Option<(f64, f64)> {
        let z = dot(w, self.d);
        if z <= 1e-6 {
            return None;
        }
        Some((dot(w, self.ex) / z / self.scale, dot(w, self.ey) / z / self.scale))
    }
}

/// Sample a square region of one photo on a plane (one resampling from the working image).
/// Pixels outside the photo or blown out / black (clip limits) are marked invalid.
pub fn render_region(g: &Geo, gray: &Gray, rgb: &RgbImage, plane: &Plane, size: usize) -> Region {
    let mut r = Region::new(size);
    let c = (size as f64 - 1.0) * 0.5;
    let (lo, hi) = (crate::stitch::CLIP_LOWER, crate::stitch::CLIP_UPPER);
    let sx = rgb.width() as f64 / g.w;
    let sy = rgb.height() as f64 / g.h;
    for y in 0..size {
        for x in 0..size {
            let d = plane.dir(x as f64 - c, y as f64 - c);
            let Some(p) = g.dir_to_px(d) else { continue };
            if !g.inside(p, 0.0) {
                continue;
            }
            let i = y * size + x;
            r.data[i] = gray.sample(p.0 as f32 * (gray.w as f32 / g.w as f32), p.1 as f32 * (gray.h as f32 / g.h as f32));
            let px = rgb.get_pixel(((p.0 * sx).round() as u32).min(rgb.width() - 1), ((p.1 * sy).round() as u32).min(rgb.height() - 1));
            let mx = px[0].max(px[1]).max(px[2]);
            r.mask[i] = mx >= lo && mx <= hi;
        }
    }
    r
}

/// One measured patch.
#[derive(Clone)]
pub struct PatchMeasure {
    /// record id in the patch comparison store (0 = not recorded)
    pub id: u32,
    pub result: MatchResult,
    /// direction of the matched spot in photo b
    pub dir_b: V3,
    /// mean absolute grey difference before matching (0..255)
    pub diff: f32,
}

/// Match the patch around `d` (seen in photo a) inside photo b.
/// Patches measured per round at most (all pairs together); with many photos the number of
/// areas per pair is lowered so a round stays within a few seconds to a minute.
pub const PATCH_BUDGET: usize = 40_000;
/// Fewest candidates per pair, even with very many pairs.
pub const MIN_CANDIDATES_PER_PAIR: usize = 24;

pub fn measure(
    ga: &Geo,
    gb: &Geo,
    ia: (&Gray, &RgbImage),
    ib: (&Gray, &RgbImage),
    d: V3,
    cfg: &MatchConfig,
) -> Option<PatchMeasure> {
    measure_ex(ga, gb, ia, ib, d, cfg, None)
}

/// What the patch comparison window needs to know about a measured patch.
pub struct RecCtx {
    pub a: usize,
    pub b: usize,
    pub pa: [f64; 2],
    pub kind: CpKind,
    pub hand_point: bool,
    /// disabled by hand in the window (measured, shown, but not used)
    pub disabled: bool,
}

/// `measure`, and with `rec` the patch is also stored for the patch comparison window. The
/// matching result is exactly the same either way.
pub fn measure_ex(
    ga: &Geo,
    gb: &Geo,
    ia: (&Gray, &RgbImage),
    ib: (&Gray, &RgbImage),
    d: V3,
    cfg: &MatchConfig,
    rec: Option<&RecCtx>,
) -> Option<PatchMeasure> {
    let scale = ga.pixel_angle().max(gb.pixel_angle());
    let plane = Plane::new(d, &ga.rot, scale);
    if crate::engine::cancelled() {
        return None;
    }
    let size = Region::size_for(cfg);
    let ra = render_region(ga, ia.0, ia.1, &plane, size);
    let (id, focused) = match rec {
        Some(r) => STORE.start_patch(if cfg.use_pyramid { cfg.pyramid_levels } else { 0 }, r.a, r.b),
        None => (0, false),
    };
    let store = |result: &MatchResult, detail: patch_matcher::MatchDetail, rb: Option<&Region>, tr: &SearchTrace, diff: f32| {
        if let Some(r) = rec {
            let (data, mut state) = patch_store::build_record(patch_store::RecordInput {
                id,
                a: r.a,
                b: r.b,
                pa: r.pa,
                kind: r.kind,
                hand_point: r.hand_point,
                dir: d,
                cfg,
                ra: &ra,
                rb,
                trace: tr,
                result,
                detail,
                diff_before: diff,
                plane,
                geo_a: *ga,
                geo_b: *gb,
            });
            if r.disabled {
                state.status = Status::DisabledManual;
            }
            STORE.finish_patch(data, state, focused);
        }
    };
    if let Some(why) = patch_matcher::texture_reason(&ra, cfg) {
        // plain area: no need to look at photo b at all
        let (texture, min_eig) = patch_matcher::structure(&ra, cfg);
        let result = MatchResult { reason: why, texture, min_eig, ..Default::default() };
        if rec.is_some() {
            let detail = patch_matcher::MatchDetail { uniqueness: 0.0, texture, min_eig };
            store(&result, detail, None, &SearchTrace::default(), f32::NAN);
        }
        return Some(PatchMeasure { id, result, dir_b: d, diff: f32::NAN });
    }
    let rb = render_region(gb, ib.0, ib.1, &plane, size);
    // grey difference of the central patch as it lies now
    let (mut sd, mut n) = (0.0f32, 0usize);
    let c = size / 2;
    let h = cfg.patch_size / 2;
    for y in c - h..c + h {
        for x in c - h..c + h {
            let i = y * size + x;
            if ra.mask[i] && rb.mask[i] {
                sd += (ra.data[i] - rb.data[i]).abs();
                n += 1;
            }
        }
    }
    let diff = if n > 0 { 255.0 * sd / n as f32 } else { 0.0 };
    let result = if rec.is_some() {
        let mut tr = SearchTrace { want_images: focused && STORE.wants_frames(), ..Default::default() };
        let (result, detail) = patch_matcher::match_patch_detailed(&ra, &rb, cfg, Some(&mut tr));
        if focused
            && let Some(r) = rec {
                STORE.play_trace(id, r.a, r.b, &tr, &result);
            }
        store(&result, detail, Some(&rb), &tr, diff);
        result
    } else {
        patch_matcher::match_patch(&ra, &rb, cfg)
    };
    let dir_b = plane.dir(result.dx as f64, result.dy as f64);
    Some(PatchMeasure { id, result, dir_b, diff })
}

/// Smoothed copies for photos that are much finer than their partner (avoids aliasing when a
/// fine photo is sampled at the coarser photo's pixel size).
struct Images<'a> {
    photos: &'a [FaPhoto],
    soft: HashMap<(usize, u32), Arc<Gray>>,
    /// foliage map per photo (empty when the foliage rating is switched off)
    fol: Vec<FoliageMap>,
}

impl<'a> Images<'a> {
    fn sigma_key(sigma: f64) -> u32 {
        (sigma * 10.0).round() as u32
    }
    fn prepare(photos: &'a [FaPhoto], geos: &[Geo], pairs: &[(usize, usize)], foliage: bool) -> Self {
        let mut need: Vec<(usize, u32)> = Vec::new();
        for &(a, b) in pairs {
            let s = geos[a].pixel_angle().max(geos[b].pixel_angle());
            for i in [a, b] {
                let ratio = s / geos[i].pixel_angle();
                if ratio > 1.5 {
                    let k = Self::sigma_key(0.5 * ratio);
                    if !need.contains(&(i, k)) {
                        need.push((i, k));
                    }
                }
            }
        }
        let soft = need
            .par_iter()
            .map(|&(i, k)| ((i, k), Arc::new(blur(&photos[i].gray, k as f32 / 10.0))))
            .collect();
        Images { photos, soft, fol: if foliage { foliage_maps(photos) } else { Vec::new() } }
    }
    /// Foliage maps in the shape `foliage::rate` takes (all None when switched off).
    fn fol_maps(&self) -> Vec<Option<&FoliageMap>> {
        (0..self.photos.len()).map(|i| self.fol.get(i)).collect()
    }
    /// Static foliage value around a spot of photo `i` (working-image pixels); 0 when switched off.
    fn fol_at(&self, i: usize, p: (f64, f64)) -> f32 {
        self.fol.get(i).map_or(0.0, |m| m.around(p.0 as f32, p.1 as f32))
    }
    fn get(&self, i: usize, geos: &[Geo], partner: usize) -> (&Gray, &RgbImage) {
        let s = geos[i].pixel_angle().max(geos[partner].pixel_angle());
        let ratio = s / geos[i].pixel_angle();
        let g: &Gray = if ratio > 1.5 {
            self.soft.get(&(i, Self::sigma_key(0.5 * ratio))).map(|a| a.as_ref()).unwrap_or(&self.photos[i].gray)
        } else {
            &self.photos[i].gray
        };
        (g, &self.photos[i].rgb)
    }
}

fn err_stats(cps: &[ControlPoint]) -> [f64; 3] {
    let mut e: Vec<f64> = cps.iter().filter(|c| c.kind == CpKind::Point).map(|c| c.err).collect();
    if e.is_empty() {
        return [0.0; 3];
    }
    e.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = e.iter().sum::<f64>() / e.len() as f64;
    [mean, e[e.len() / 2], e[((e.len() as f64 - 1.0) * 0.95) as usize]]
}

/// Normalised image coordinates of a working-image pixel.
pub fn norm(g: &Geo, p: (f64, f64)) -> [f64; 2] {
    let l = g.long();
    [(p.0 + 0.5 - g.w * 0.5) / l, (p.1 + 0.5 - g.h * 0.5) / l]
}

fn denorm(g: &Geo, n: [f64; 2]) -> (f64, f64) {
    let l = g.long();
    (n[0] * l + g.w * 0.5 - 0.5, n[1] * l + g.h * 0.5 - 0.5)
}

struct RoundOutcome {
    cps: Vec<ControlPoint>,
    mean_shift: f32,
    mean_diff: f32,
    total: usize,
    valid: usize,
    reasons: HashMap<&'static str, usize>,
    areas: usize,
    refined: usize,
    failed: usize,
    locked_far: usize,
    locked_far_max: f32,
    locked_unmatched: usize,
    skipped_cells: usize,
}

/// One measuring round: refine points, re-measure manual areas, rebuild automatic areas.
/// Every measured patch goes into the patch comparison store (this round replaces the list).
/// `ov` holds what the person changed in the window (empty in a normal run): disabled patches
/// are measured and listed but not used; patches shifted by hand are kept as they are.
fn measure_round(
    geos: &[Geo],
    imgs: &Images,
    cps: &[ControlPoint],
    cfg: &FineConfig,
    mcfg: &MatchConfig,
    pairs: &[(usize, usize)],
    ov: &HashMap<Key, Override>,
    label: &str,
) -> RoundOutcome {
    let mut reasons: HashMap<&'static str, usize> = HashMap::new();
    let (mut total, mut valid) = (0usize, 0usize);
    let mut shifts: Vec<f32> = Vec::new();
    let mut diffs: Vec<f32> = Vec::new();
    let is_disabled = |k: &Key| matches!(ov.get(k), Some(Override::Disabled));
    let manual_of = |k: &Key| match ov.get(k) {
        Some(Override::Manual(d, s)) => Some((d.clone(), s.clone())),
        _ => None,
    };
    let point_measured = |c: &ControlPoint| match c.kind {
        CpKind::Point => cfg.refine_points,
        CpKind::AreaManual => true,
        CpKind::Area => false,
    };
    // area candidates first, so the number of patches of this round is known up front
    let size = Region::size_for(mcfg);
    let half_plane = size as f64 * 0.5;
    // (candidates per pair, grid cells left out because the photo has no structure there)
    let cand_all: Vec<(Vec<((f64, f64), bool, (usize, usize))>, usize)> = if cfg.areas {
        pairs
            .par_iter()
            .map(|&(a, b)| {
                let (ga, gb) = (&geos[a], &geos[b]);
                let scale = ga.pixel_angle().max(gb.pixel_angle());
                // grid in photo a, spacing in plane pixels converted to a's pixels
                let to_a = scale / ga.pixel_angle();
                let fine_step = (cfg.grid_cell as f64 * 0.5 * to_a).max(4.0);
                // distance from the border: half the patch plus a share of the search radius (the rest of the
                // search area may reach beyond the photo, the matcher then only searches inwards)
                let edge = (mcfg.patch_size as f64 * 0.5 + cfg.edge_search_share as f64 * mcfg.search_radius as f64).min(half_plane);
                let margin_a = edge * to_a;
                let margin_b = edge * scale / gb.pixel_angle();
                let smap = cfg.pick_textured.then(|| patch_matcher::StructMap::new(imgs.get(a, geos, b).0, mcfg.blur_sigma));
                let half_win = mcfg.patch_size as f64 * 0.5 * to_a;
                let mut skipped = 0usize;
                let mut picked: Vec<(f64, usize, (f64, f64))> = Vec::new(); // (structure, cell index, position)
                let coarse_w = ga.w.max(gb.w / (scale / gb.pixel_angle()));
                let band = (2.0 * cfg.seam_band as f64 / coarse_w).clamp(0.0, 1.0);
                let in_b = |p: (f64, f64)| ga.inside(p, margin_a) && gb.dir_to_px(ga.px_to_dir(p)).is_some_and(|q| gb.inside(q, margin_b));
                let band_at = |p: (f64, f64)| {
                    let q = gb.dir_to_px(ga.px_to_dir(p)).unwrap_or(p);
                    let (wa, wb) = (ga.centre_weight(p), gb.centre_weight(q));
                    (wa - wb).abs() / (wa + wb + 1e-9) < band
                };
                let mut cands: Vec<((f64, f64), bool, (usize, usize))> = Vec::new();
                let (nx, ny) = ((ga.w / fine_step) as usize, (ga.h / fine_step) as usize);
                for gy in 0..=ny {
                    for gx in 0..=nx {
                        let p0 = (gx as f64 * fine_step, gy as f64 * fine_step);
                        if !ga.inside(p0, margin_a) {
                            continue;
                        }
                        if !in_b(p0) {
                            continue;
                        }
                        let in_band0 = band_at(p0);
                        // outside the seam band only every second grid point (no overlap)
                        if !in_band0 && (gx % 2 == 1 || gy % 2 == 1) {
                            continue;
                        }
                        if let Some(m) = &smap {
                            // the spot with the most two-direction structure within reach of the grid point
                            let reach = (cfg.pick_reach as f64 * fine_step).floor() as i32;
                            let step = (reach / 4).max(2);
                            // a spot on foliage counts less (moves between the shots): its structure is multiplied
                            // by a factor below 1; 1 everywhere when the foliage rating is off
                            let pen = |q: (f64, f64)| 1.0 - cfg.foliage.pick_penalty * imgs.fol_at(a, q);
                            let mut best = (m.at(p0.0, p0.1, half_win).1, p0);
                            let mut best_adj = best.0 * pen(p0);
                            let mut oy = -reach;
                            while oy <= reach {
                                let mut ox = -reach;
                                while ox <= reach {
                                    let q = (p0.0 + ox as f64, p0.1 + oy as f64);
                                    let sc = m.at(q.0, q.1, half_win).1;
                                    let adj = sc * pen(q);
                                    if adj > best_adj && in_b(q) {
                                        best = (sc, q);
                                        best_adj = adj;
                                    }
                                    ox += step;
                                }
                                oy += step;
                            }
                            if best.0 < mcfg.min_structure.max(f32::MIN_POSITIVE) {
                                skipped += 1;
                                continue;
                            }
                            picked.push((best_adj as f64, gy * (nx + 1) + gx, best.1));
                            continue;
                        }
                        let big = (((p0.0 / ga.w) * 4.0) as usize).min(3) + 4 * (((p0.1 / ga.h) * 4.0) as usize).min(3);
                        cands.push((p0, in_band0, (big, 0)));
                    }
                }
                if smap.is_some() {
                    // strongest first; a spot within 0.45 grid steps of a better one is dropped (neighbouring cells
                    // would otherwise take the same structure twice); back in grid order for the even thinning below
                    picked.sort_by(|x, y| y.0.total_cmp(&x.0));
                    let min_d = 0.45 * fine_step;
                    let mut kept: Vec<(usize, (f64, f64))> = Vec::new();
                    for (_, cell, p) in picked {
                        if kept.iter().all(|k| (k.1 .0 - p.0).hypot(k.1 .1 - p.1) >= min_d) {
                            kept.push((cell, p));
                        }
                    }
                    kept.sort_by_key(|k| k.0);
                    for (_, p) in kept {
                        let big = (((p.0 / ga.w) * 4.0) as usize).min(3) + 4 * (((p.1 / ga.h) * 4.0) as usize).min(3);
                        cands.push((p, band_at(p), (big, 0)));
                    }
                }
                // keep at most a few times the area limit, spread evenly, seam band first
                let cap = (cfg.max_areas_per_pair * CANDIDATE_FACTOR).min((PATCH_BUDGET / pairs.len().max(1)).max(MIN_CANDIDATES_PER_PAIR));
                if cands.len() > cap {
                    let (band, rest): (Vec<_>, Vec<_>) = cands.iter().copied().partition(|c| c.1);
                    let take = |v: &Vec<((f64, f64), bool, (usize, usize))>, k: usize| -> Vec<((f64, f64), bool, (usize, usize))> {
                        if v.len() <= k || k == 0 {
                            return v.iter().copied().take(k).collect();
                        }
                        let step = v.len() as f64 / k as f64;
                        (0..k).map(|i| v[(i as f64 * step) as usize]).collect()
                    };
                    let nb = (cap * 2 / 3).min(band.len());
                    let mut c = take(&band, nb);
                    c.extend(take(&rest, cap - nb));
                    cands = c;
                }
                // patches shifted by hand are not matched again
                cands.retain(|&(p, _, _)| manual_of(&patch_store::key_of(a, b, norm(ga, p))).is_none());
                (cands, skipped)
            })
            .collect()
    } else {
        Vec::new()
    };
    let skipped_cells: usize = cand_all.iter().map(|c| c.1).sum();
    let cand_lists: Vec<Vec<((f64, f64), bool, (usize, usize))>> = cand_all.into_iter().map(|c| c.0).collect();
    let n_points = cps.iter().filter(|c| point_measured(c)).count();
    STORE.begin_round(n_points + cand_lists.iter().map(|v| v.len()).sum::<usize>(), label, cfg.record_patches);
    // 1) points and hand-drawn areas
    let refined: Vec<(ControlPoint, Option<PatchMeasure>, bool)> = cps
        .par_iter()
        .map(|c| {
            if !point_measured(c) {
                return (*c, None, false);
            }
            let key = patch_store::key_of(c.a, c.b, c.pa);
            if let Some((d, st)) = manual_of(&key) {
                // shifted by hand: kept as it is, not matched again
                STORE.keep_manual(&d, &st);
                return (st.cp.unwrap_or(*c), None, true);
            }
            let (ga, gb) = (&geos[c.a], &geos[c.b]);
            let d = ga.px_to_dir(denorm(ga, c.pa));
            let pcfg;
            let use_cfg = if c.kind == CpKind::Point {
                pcfg = MatchConfig {
                    patch_size: cfg.point_template.max(9),
                    search_radius: cfg.point_search.min(mcfg.search_radius.max(cfg.min_search_radius)),
                    min_score: cfg.point_min_score,
                    ..mcfg.clone()
                };
                &pcfg
            } else {
                mcfg
            };
            let rc = RecCtx { a: c.a, b: c.b, pa: c.pa, kind: c.kind, hand_point: c.manual, disabled: is_disabled(&key) };
            let m = measure_ex(ga, gb, imgs.get(c.a, geos, c.b), imgs.get(c.b, geos, c.a), d, use_cfg, cfg.record_patches.then_some(&rc));
            (*c, m, false)
        })
        .collect();
    let mut out: Vec<ControlPoint> = Vec::with_capacity(cps.len());
    let mut n_refined = 0;
    let mut n_failed = 0;
    let (mut locked_far, mut locked_far_max, mut locked_unmatched) = (0usize, 0.0f32, 0usize);
    for (mut c, m, kept_manual) in refined {
        if c.kind == CpKind::Area {
            if c.locked {
                out.push(c); // a protected area stays, even though the others are rebuilt
            }
            continue; // automatic areas are rebuilt below
        }
        if kept_manual {
            out.push(c);
            continue;
        }
        if let Some(m) = m {
            total += 1;
            *reasons.entry(m.result.reason).or_insert(0) += 1;
            // disabled by hand: measured and listed (with the pair it would give), not used
            let disabled = is_disabled(&patch_store::key_of(c.a, c.b, c.pa));
            c.conf = m.result.confidence;
            // like the original, the point keeps the correlation of its last fine-tuning
            c.score = m.result.score;
            if !m.result.valid && c.kind == CpKind::Point {
                n_failed += 1;
            }
            if m.result.valid {
                valid += 1;
                shifts.push((m.result.dx.powi(2) + m.result.dy.powi(2)).sqrt());
                if m.diff.is_finite() {
                    diffs.push(m.diff);
                }
                c.shift = [m.result.dx, m.result.dy];
                if c.locked {
                    // protected: only measured. How far the spot found in photo B is from where the point is
                    // (pixels of the working image); a large distance is reported, the point stays as it is.
                    if let Some(p) = geos[c.b].dir_to_px(m.dir_b) {
                        let (x, y) = denorm(&geos[c.b], c.pb);
                        let s = ((p.0 - x).hypot(p.1 - y)) as f32;
                        if s > cfg.locked_warn_px {
                            locked_far += 1;
                            locked_far_max = locked_far_max.max(s);
                        }
                    }
                } else {
                    // points placed by hand stay where the person put them
                    if (!c.manual || c.kind == CpKind::AreaManual)
                        && let Some(p) = geos[c.b].dir_to_px(m.dir_b) {
                            c.pb = norm(&geos[c.b], p);
                            n_refined += 1;
                        }
                    c.weight = match c.kind {
                        CpKind::AreaManual => cfg.manual_area_weight * c.conf.max(0.05) as f64,
                        _ => 1.0,
                    };
                }
            } else if c.locked {
                locked_unmatched += 1;
            } else if c.kind == CpKind::Point && !c.manual {
                if cfg.drop_invalid_points {
                    STORE.finalize(m.id, None, None);
                    continue;
                }
                c.weight = cfg.invalid_point_weight;
            }
            STORE.finalize(m.id, Some(c), None);
            if disabled {
                continue;
            }
        }
        out.push(c);
    }
    // 2) automatic control areas
    let mut n_areas = 0;
    if cfg.areas {
        let results: Vec<Vec<(ControlPoint, f64, (usize, usize), u32)>> = pairs
            .par_iter()
            .zip(cand_lists.par_iter())
            .map(|(&(a, b), cands)| {
                let (ga, gb) = (&geos[a], &geos[b]);
                cands
                    .par_iter()
                    .map(|&(p, in_band, cell)| {
                        let d = ga.px_to_dir(p);
                        let pa = norm(ga, p);
                        let rc = RecCtx { a, b, pa, kind: CpKind::Area, hand_point: false, disabled: is_disabled(&patch_store::key_of(a, b, pa)) };
                        let m = measure_ex(ga, gb, imgs.get(a, geos, b), imgs.get(b, geos, a), d, mcfg, cfg.record_patches.then_some(&rc));
                        (p, in_band, cell, m)
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .filter_map(|(p, in_band, cell, m)| {
                        let m = m?;
                        Some((p, in_band, cell, m))
                    })
                    .map(|(p, in_band, cell, m)| {
                        let mut c = ControlPoint::new(a, b, norm(ga, p), [0.0; 2], false);
                        c.kind = CpKind::Area;
                        c.conf = if m.result.valid { m.result.confidence } else { -1.0 };
                        c.shift = [m.result.dx, m.result.dy];
                        if let Some(q) = gb.dir_to_px(m.dir_b) {
                            c.pb = norm(gb, q);
                        } else {
                            c.conf = -1.0;
                        }
                        // carry the reason and the difference in the weight slot until selection
                        let prio = if in_band { 2.0 } else { 1.0 } * m.result.confidence as f64;
                        c.err = m.diff as f64;
                        (c, if m.result.valid { prio } else { -(reason_code(m.result.reason) as f64) }, cell, m.id)
                    })
                    .collect()
            })
            .collect();
        let max_areas = cfg.max_areas_per_pair.min(((PATCH_BUDGET / pairs.len().max(1)).max(MIN_CANDIDATES_PER_PAIR) / CANDIDATE_FACTOR).max(1));
        for (pi, list) in results.into_iter().enumerate() {
            let (a, b) = pairs[pi];
            let mut ok: Vec<(ControlPoint, f64, (usize, usize), u32)> = Vec::new();
            for (c, prio, cell, id) in list {
                total += 1;
                if prio < 0.0 || c.conf < 0.0 {
                    *reasons.entry(reason_name(-prio as i32)).or_insert(0) += 1;
                    continue;
                }
                *reasons.entry("ok").or_insert(0) += 1;
                valid += 1;
                shifts.push((c.shift[0].powi(2) + c.shift[1].powi(2)).sqrt());
                if c.err.is_finite() {
                    diffs.push(c.err as f32);
                }
                if is_disabled(&patch_store::key_of(c.a, c.b, c.pa)) {
                    // disabled by hand: listed (with the pair it would give), not used
                    let mut c = c;
                    c.weight = (c.conf as f64).max(1e-3);
                    c.err = 0.0;
                    STORE.finalize(id, Some(c), None);
                    continue;
                }
                ok.push((c, prio, cell, id));
            }
            // spatial spread: best per big cell first (round robin), up to the limit
            ok.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap());
            let mut by_cell: HashMap<usize, Vec<(ControlPoint, u32)>> = HashMap::new();
            for (c, _, cell, id) in ok {
                by_cell.entry(cell.0).or_default().push((c, id));
            }
            let mut chosen: Vec<(ControlPoint, u32)> = Vec::new();
            let mut round = 0;
            loop {
                let mut any = false;
                for list in by_cell.values() {
                    if round < list.len() && chosen.len() < max_areas {
                        chosen.push(list[round]);
                        any = true;
                    }
                }
                if !any || chosen.len() >= max_areas {
                    break;
                }
                round += 1;
            }
            let chosen_ids: std::collections::HashSet<u32> = chosen.iter().map(|x| x.1).collect();
            for list in by_cell.values() {
                for &(_, id) in list {
                    if !chosen_ids.contains(&id) {
                        STORE.finalize(id, None, Some(patch_store::REASON_AREA_LIMIT));
                    }
                }
            }
            // weights: confidence, balanced against the pair's points
            let point_w: f64 = out
                .iter()
                .filter(|c| c.kind != CpKind::Area && ((c.a == a && c.b == b) || (c.a == b && c.b == a)))
                .map(|c| c.weight)
                .sum::<f64>()
                .max(10.0);
            let area_w: f64 = chosen.iter().map(|c| c.0.conf as f64).sum();
            let k = if area_w > cfg.area_weight_scale * point_w { cfg.area_weight_scale * point_w / area_w } else { 1.0 };
            for (mut c, id) in chosen {
                c.weight = (c.conf as f64 * k).max(1e-3);
                c.err = 0.0;
                STORE.finalize(id, Some(c), None);
                out.push(c);
                n_areas += 1;
            }
            // control areas shifted by hand: kept as they are
            for o in ov.values() {
                if let Override::Manual(d, st) = o
                    && d.kind == CpKind::Area && d.image_a == a && d.image_b == b {
                        STORE.keep_manual(d, st);
                        if let Some(c) = st.cp {
                            out.push(c);
                        }
                    }
            }
        }
    }
    STORE.end_round(crate::engine::cancelled());
    foliage::rate(&mut out, &imgs.fol_maps(), &cfg.foliage);
    let mean = |v: &Vec<f32>| if v.is_empty() { 0.0 } else { v.iter().sum::<f32>() / v.len() as f32 };
    RoundOutcome {
        cps: out,
        mean_shift: mean(&shifts),
        mean_diff: mean(&diffs),
        total,
        valid,
        reasons,
        areas: n_areas,
        refined: n_refined,
        failed: n_failed,
        locked_far,
        locked_far_max,
        locked_unmatched,
        skipped_cells,
    }
}

/// How many candidates per pair are measured, as a multiple of `max_areas_per_pair`.
const CANDIDATE_FACTOR: usize = 2;

const REASONS: [&str; 10] = [
    "ok",
    "region smaller than the patch",
    "patch outside the photo",
    "too little texture",
    "too few valid pixels",
    "best match at the edge of the search area",
    "similarity too low",
    "ambiguous (repeating pattern)",
    "forward-backward check failed",
    "structure in one direction only",
];

fn reason_code(r: &str) -> i32 {
    REASONS.iter().position(|x| *x == r).map(|i| i as i32 + 1).unwrap_or(1)
}

fn reason_name(code: i32) -> &'static str {
    REASONS.get((code - 1).max(0) as usize).copied().unwrap_or("other")
}

/// Mean remaining shift (valid patches) and mean grey difference on a fixed set of test spots.
fn evaluate(geos: &[Geo], imgs: &Images, test: &[(usize, usize, (f64, f64))], mcfg: &MatchConfig) -> (f32, f32, usize) {
    let r: Vec<Option<PatchMeasure>> = test
        .par_iter()
        .map(|&(a, b, p)| {
            let d = geos[a].px_to_dir(p);
            measure(&geos[a], &geos[b], imgs.get(a, geos, b), imgs.get(b, geos, a), d, mcfg)
        })
        .collect();
    let (mut s, mut ns, mut df, mut nd) = (0.0f32, 0usize, 0.0f32, 0usize);
    for m in r.into_iter().flatten() {
        if m.diff.is_finite() {
            df += m.diff;
            nd += 1;
        }
        if m.result.valid {
            s += (m.result.dx.powi(2) + m.result.dy.powi(2)).sqrt();
            ns += 1;
        }
    }
    (if ns > 0 { s / ns as f32 } else { f32::NAN }, if nd > 0 { df / nd as f32 } else { 0.0 }, ns)
}

/// Independent check of an alignment: how far the photos are still apart, measured by patch
/// matching on spots that are neither control points nor control areas (a grid that is offset
/// against the area grid). Removing points cannot make this number better, so it is the measure
/// for comparing alignment methods. Used by the headless `--accuracy` driver.
#[derive(Clone, Debug, Default)]
pub struct Quality {
    /// spots measured / spots with a valid match
    pub spots: usize,
    pub valid: usize,
    /// remaining shift of the valid spots in working-image pixels
    pub mean: f32,
    pub median: f32,
    pub p90: f32,
    /// mean absolute grey difference before matching (0..255)
    pub diff: f32,
    /// (valid spots, median shift) by the larger distance of the spot from the photo centres
    /// (share of the long side): below 0.25, 0.25..0.4, above 0.4
    pub by_radius: [(usize, f32); 3],
}

/// Spots for `quality`: a regular grid in photo a of every pair that falls into photo b, offset
/// by an odd fraction of a cell so it never lands on the control-area grid, at most `per_pair`
/// per pair (evenly thinned). Computed once from one alignment and reused for all variants.
pub fn quality_spots(photos: &[FaPhoto], lens_of: &[usize], lenses: &[Lens], rots: &[M3], pairs: &[(usize, usize)], cell: f64, per_pair: usize) -> Vec<(usize, usize, (f64, f64))> {
    let geos: Vec<Geo> = (0..photos.len()).map(|i| Geo { lens: lenses[lens_of[i]], rot: rots[i], w: photos[i].gray.w as f64, h: photos[i].gray.h as f64 }).collect();
    let margin = 40.0;
    let mut out = Vec::new();
    for &(a, b) in pairs {
        let (ga, gb) = (&geos[a], &geos[b]);
        let mut v = Vec::new();
        let (nx, ny) = ((ga.w / cell) as usize, (ga.h / cell) as usize);
        for gy in 0..ny {
            for gx in 0..nx {
                let p = ((gx as f64 + 0.37) * cell, (gy as f64 + 0.61) * cell);
                if !ga.inside(p, margin) {
                    continue;
                }
                if let Some(q) = gb.dir_to_px(ga.px_to_dir(p))
                    && gb.inside(q, margin) {
                        v.push((a, b, p));
                    }
            }
        }
        if v.len() > per_pair {
            let step = v.len() as f64 / per_pair as f64;
            v = (0..per_pair).map(|i| v[(i as f64 * step) as usize]).collect();
        }
        out.extend(v);
    }
    out
}

/// Measure the remaining shift on fixed spots (see `Quality`).
pub fn quality(photos: &[FaPhoto], lens_of: &[usize], lenses: &[Lens], rots: &[M3], spots: &[(usize, usize, (f64, f64))], mcfg: &MatchConfig) -> Quality {
    let geos: Vec<Geo> = (0..photos.len()).map(|i| Geo { lens: lenses[lens_of[i]], rot: rots[i], w: photos[i].gray.w as f64, h: photos[i].gray.h as f64 }).collect();
    let mut pairs: Vec<(usize, usize)> = spots.iter().map(|s| (s.0, s.1)).collect();
    pairs.sort();
    pairs.dedup();
    let imgs = Images::prepare(photos, &geos, &pairs, false);
    let r: Vec<Option<PatchMeasure>> = spots.par_iter().map(|&(a, b, p)| measure(&geos[a], &geos[b], imgs.get(a, &geos, b), imgs.get(b, &geos, a), geos[a].px_to_dir(p), mcfg)).collect();
    let mut shifts: Vec<f32> = Vec::new();
    let mut bins: [Vec<f32>; 3] = Default::default();
    let (mut df, mut nd) = (0.0f32, 0usize);
    for (m, &(a, b, p)) in r.into_iter().zip(spots) {
        let Some(m) = m else { continue };
        if m.diff.is_finite() {
            df += m.diff;
            nd += 1;
        }
        if m.result.valid {
            let v = (m.result.dx.powi(2) + m.result.dy.powi(2)).sqrt();
            shifts.push(v);
            let rad = |g: &Geo, q: (f64, f64)| ((q.0 + 0.5 - g.w * 0.5).hypot(q.1 + 0.5 - g.h * 0.5)) / g.long();
            let ra = rad(&geos[a], p);
            let rb = geos[b].dir_to_px(geos[a].px_to_dir(p)).map(|q| rad(&geos[b], q)).unwrap_or(0.0);
            let r = ra.max(rb);
            bins[if r < 0.25 { 0 } else if r < 0.4 { 1 } else { 2 }].push(v);
        }
    }
    shifts.sort_by(f32::total_cmp);
    let by_radius = bins.map(|mut v| {
        v.sort_by(f32::total_cmp);
        (v.len(), v.get(v.len() / 2).copied().unwrap_or(f32::NAN))
    });
    let at = |q: f64| shifts.get(((shifts.len() as f64 - 1.0) * q).round().max(0.0) as usize).copied().unwrap_or(f32::NAN);
    Quality {
        spots: spots.len(),
        valid: shifts.len(),
        mean: if shifts.is_empty() { f32::NAN } else { shifts.iter().sum::<f32>() / shifts.len() as f32 },
        median: at(0.5),
        p90: at(0.9),
        diff: if nd > 0 { df / nd as f32 } else { 0.0 },
        by_radius,
    }
}

/// Largest number of fixed test spots used to judge each round.
const TEST_SPOTS: usize = 600;

/// Run the fine alignment. Returns new rotations, lenses, control points and statistics.
#[allow(clippy::too_many_arguments)]
pub fn run(
    photos: &[FaPhoto],
    lens_of: &[usize],
    long_side: &[f64],
    lenses_in: &[Lens],
    rots_in: &[M3],
    cps_in: &[ControlPoint],
    opts_focal: bool,
    opts_distortion: bool,
    cfg: &FineConfig,
    report: &(dyn Fn(f32, &str) + Sync),
) -> (Vec<M3>, Vec<Lens>, Vec<ControlPoint>, FineStats) {
    let t0 = std::time::Instant::now();
    let n = photos.len();
    let mut stats = FineStats::default();
    let mut rots = rots_in.to_vec();
    let mut lenses = lenses_in.to_vec();
    let mut cps = cps_in.to_vec();
    if !cfg.enabled || n < 2 || cps.is_empty() {
        return (rots, lenses, cps, stats);
    }
    let inp = AlignInput { n, lens_of, long_side };
    let mut cnt = vec![0usize; n];
    for c in &cps {
        cnt[c.a] += 1;
        cnt[c.b] += 1;
    }
    let anchor = (0..n).max_by_key(|&i| cnt[i]).unwrap_or(0);
    let opts = OptimizeOptions { focal: opts_focal, distortion: opts_distortion, cauchy: cfg.robust_loss == RobustLoss::Cauchy };
    let mut pairs: Vec<(usize, usize)> = cps.iter().map(|c| (c.a.min(c.b), c.a.max(c.b))).collect();
    pairs.sort();
    pairs.dedup();
    let geos_of = |rots: &[M3], lenses: &[Lens]| -> Vec<Geo> {
        (0..n)
            .map(|i| Geo { lens: lenses[lens_of[i]], rot: rots[i], w: photos[i].gray.w as f64, h: photos[i].gray.h as f64 })
            .collect()
    };
    align::cp_errors(&inp, &rots, &lenses, &mut cps);
    stats.err_before = err_stats(&cps);
    let geos = geos_of(&rots, &lenses);
    let imgs = Images::prepare(photos, &geos, &pairs, cfg.foliage.enabled);
    let fol = imgs.fol_maps();
    let rated = foliage::rate(&mut cps, &fol, &cfg.foliage);
    if rated > 0 {
        stats.log.push(format!("{rated} points and patches look like foliage and get a smaller weight"));
    }
    let rounds = cfg.refine_iterations.max(1);
    report(0.0, "Fine alignment: comparing patches");
    let none = HashMap::new();
    let o = measure_round(&geos, &imgs, &cps, cfg, &cfg.matcher, &pairs, &none, "Fine alignment, round 1");
    stats.list_total = o.total;
    stats.list_valid = o.valid;
    stats.list_areas = o.areas;
    stats.patches_total = o.total;
    stats.patches_valid = o.valid;
    let mut rs: Vec<(String, usize)> = o.reasons.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    rs.sort_by_key(|a| std::cmp::Reverse(a.1));
    stats.reasons = rs;
    stats.cells_skipped = o.skipped_cells;
    stats.log.push(format!("measured {} patches, {} valid, {} control areas kept", o.total, o.valid, o.areas));
    if o.skipped_cells > 0 {
        stats.log.push(format!("{} grid cells left out: no structure in two directions (plain surface, single edge)", o.skipped_cells));
    }
    stats.locked_far = o.locked_far;
    stats.locked_far_max = o.locked_far_max;
    stats.locked_warn_px = cfg.locked_warn_px;
    stats.locked_unmatched = o.locked_unmatched;
    if o.locked_far > 0 {
        stats.log.push(format!("{} protected points are off by more than {:.1} px (up to {:.1} px) - left as they are", o.locked_far, cfg.locked_warn_px, o.locked_far_max));
    }
    if o.locked_unmatched > 0 {
        stats.log.push(format!("{} protected points could not be confirmed (no match within the search radius) - left as they are", o.locked_unmatched));
    }
    if o.valid == 0 {
        stats.log.push("no valid patches - keeping the alignment as it is (texture-poor overlaps?)".into());
        stats.err_after = stats.err_before;
        stats.ms = t0.elapsed().as_millis();
        return (rots, lenses, cps, stats);
    }
    // fixed test spots: the accepted control areas of the first round
    let all_test: Vec<(usize, usize, (f64, f64))> = o.cps.iter().filter(|c| c.kind == CpKind::Area).map(|c| (c.a, c.b, denorm(&geos[c.a], c.pa))).collect();
    let step = (all_test.len() as f64 / TEST_SPOTS as f64).max(1.0);
    let test: Vec<(usize, usize, (f64, f64))> = (0..all_test.len().min(TEST_SPOTS)).map(|i| all_test[(i as f64 * step) as usize]).collect();
    let eval_cfg = MatchConfig { search_radius: (cfg.matcher.search_radius * 0.5).max(cfg.min_search_radius * 2.0), ..cfg.matcher.clone() };
    if test.is_empty() {
        // the rounds are judged on the control areas; without any there is nothing to tell better from worse
        stats.log.push("no control areas to judge the rounds with - alignment kept as it is (switch on \"Add control areas\" to fine-align)".into());
        stats.no_test_spots = true;
        stats.err_after = stats.err_before;
        stats.ms = t0.elapsed().as_millis();
        return (rots, lenses, cps, stats);
    }
    let (s0, d0, _) = evaluate(&geos, &imgs, &test, &eval_cfg);
    stats.shift_before = s0;
    stats.diff_before = d0;
    stats.shift_after = s0;
    stats.diff_after = d0;
    stats.log.push(format!("before: mean remaining shift {:.3} px, mean difference {:.2} ({} test spots)", s0, d0, test.len()));
    let mut prev = s0;
    let mut next_cps = o.cps;
    stats.areas = o.areas;
    stats.points_refined = o.refined;
    stats.points_not_updated = o.failed;
    for round in 0..rounds {
        if crate::engine::cancelled() {
            stats.log.push("cancelled".into());
            break;
        }
        if !(prev >= cfg.converge_threshold) {
            stats.log.push("remaining shift below the threshold - done".into());
            break;
        }
        let saved = (rots.clone(), lenses.clone(), cps.clone());
        cps = next_cps.clone();
        report((round as f32 + 0.5) / (rounds as f32 + 1.0), &format!("Fine alignment: optimising (round {} of {})", round + 1, rounds));
        align::optimize(&inp, &mut rots, &mut lenses, &cps, anchor, &opts, 40);
        align::cp_errors(&inp, &rots, &lenses, &mut cps);
        foliage::rate(&mut cps, &fol, &cfg.foliage);
        let geos = geos_of(&rots, &lenses);
        let (s, d, nv) = evaluate(&geos, &imgs, &test, &eval_cfg);
        stats.log.push(format!("round {}: mean remaining shift {:.3} px, mean difference {:.2} ({} valid)", round + 1, s, d, nv));
        if !(s <= prev * cfg.worse_factor) {
            rots = saved.0;
            lenses = saved.1;
            cps = saved.2;
            stats.rolled_back = true;
            stats.log.push("this round made it worse - undone".into());
            break;
        }
        stats.rounds = round + 1;
        stats.shift_after = s;
        stats.diff_after = d;
        if prev - s < cfg.converge_threshold * 0.1 {
            stats.log.push("no further improvement - done".into());
            break;
        }
        prev = s;
        if round + 1 < rounds {
            let radius = (cfg.matcher.search_radius / (2u32 << round.min(8)) as f32).max(cfg.min_search_radius);
            let mcfg = MatchConfig { search_radius: radius, ..cfg.matcher.clone() };
            report((round as f32 + 1.0) / (rounds as f32 + 1.0), &format!("Fine alignment: comparing patches (search {radius:.0} px)"));
            let o = measure_round(&geos, &imgs, &cps, cfg, &mcfg, &pairs, &none, &format!("Fine alignment, round {}", round + 2));
            stats.list_total = o.total;
            stats.list_valid = o.valid;
            stats.list_areas = o.areas;
            stats.areas = o.areas;
            stats.points_refined = o.refined;
            stats.points_not_updated = o.failed;
            stats.locked_far = o.locked_far;
            stats.locked_far_max = o.locked_far_max;
            stats.locked_unmatched = o.locked_unmatched;
            next_cps = o.cps;
        }
    }
    align::cp_errors(&inp, &rots, &lenses, &mut cps);
    foliage::rate(&mut cps, &fol, &cfg.foliage);
    stats.err_after = err_stats(&cps);
    stats.ms = t0.elapsed().as_millis();
    (rots, lenses, cps, stats)
}

/// Control pair for a patch shifted by hand: photo B's spot follows the shift (dx, dy) on the
/// patch's sampling plane, weight = the confidence given to manual patches.
pub fn manual_cp(d: &patch_store::PatchData, st: &patch_store::PatchState, dx: f32, dy: f32, conf: f32) -> Option<ControlPoint> {
    let p = d.geo_b.dir_to_px(d.plane.dir(dx as f64, dy as f64))?;
    let pb = norm(&d.geo_b, p);
    let mut c = st.cp.unwrap_or_else(|| {
        let mut c = ControlPoint::new(d.image_a, d.image_b, d.pa, pb, d.hand_point);
        c.kind = d.kind;
        c
    });
    c.pb = pb;
    c.conf = conf;
    c.shift = [dx, dy];
    c.weight = match c.kind {
        CpKind::AreaManual => 2.0 * conf as f64,
        _ => conf as f64,
    };
    Some(c)
}

/// "Restart optimisation" from the patch comparison window.
///
/// Uses every active patch of the list with its current dx/dy (patches disabled by hand are
/// left out, patches shifted by hand use the shift), optimises once, then measures a new round
/// for the list. Patches shifted by hand keep their values and are not matched again
/// (otherwise the correction would be lost); disabled ones stay disabled.
#[allow(clippy::too_many_arguments)]
pub fn reoptimize(
    photos: &[FaPhoto],
    lens_of: &[usize],
    long_side: &[f64],
    lenses_in: &[Lens],
    rots_in: &[M3],
    cps_in: &[ControlPoint],
    opts_focal: bool,
    opts_distortion: bool,
    cfg: &FineConfig,
    report: &(dyn Fn(f32, &str) + Sync),
) -> (Vec<M3>, Vec<Lens>, Vec<ControlPoint>, FineStats) {
    let t0 = std::time::Instant::now();
    let n = photos.len();
    let mut stats = FineStats::default();
    let mut rots = rots_in.to_vec();
    let mut lenses = lenses_in.to_vec();
    let (_, data, states) = STORE.snapshot();
    let listed: std::collections::HashSet<Key> = data.iter().map(|d| d.key).collect();
    // pairs not in the list stay as they are; listed ones come from the list
    let mut cps: Vec<ControlPoint> = cps_in.iter().filter(|c| !listed.contains(&patch_store::key_of(c.a, c.b, c.pa))).copied().collect();
    let mut used = 0;
    for st in &states {
        if st.status == Status::DisabledManual {
            continue;
        }
        if let Some(c) = st.cp {
            cps.push(c);
            used += 1;
        }
    }
    stats.log.push(format!("restart: {} patches from the list used, {} pairs kept from before", used, cps.len() - used));
    if n < 2 || cps.is_empty() {
        return (rots, lenses, cps, stats);
    }
    let inp = AlignInput { n, lens_of, long_side };
    let mut cnt = vec![0usize; n];
    for c in &cps {
        cnt[c.a] += 1;
        cnt[c.b] += 1;
    }
    let anchor = (0..n).max_by_key(|&i| cnt[i]).unwrap_or(0);
    let opts = OptimizeOptions { focal: opts_focal, distortion: opts_distortion, cauchy: cfg.robust_loss == RobustLoss::Cauchy };
    align::cp_errors(&inp, &rots, &lenses, &mut cps);
    stats.err_before = err_stats(&cps);
    // the list holds the patches as they were measured; the foliage weights are set again here
    let fol_v = if cfg.foliage.enabled { foliage_maps(photos) } else { Vec::new() };
    let fol: Vec<Option<&FoliageMap>> = (0..n).map(|i| fol_v.get(i)).collect();
    foliage::rate(&mut cps, &fol, &cfg.foliage);
    report(0.1, "Optimising with the patches from the list");
    align::optimize(&inp, &mut rots, &mut lenses, &cps, anchor, &opts, 40);
    align::cp_errors(&inp, &rots, &lenses, &mut cps);
    foliage::rate(&mut cps, &fol, &cfg.foliage);
    stats.err_after = err_stats(&cps);
    stats.rounds = 1;
    if crate::engine::cancelled() {
        return (rots, lenses, cps, stats);
    }
    // a new round for the list, with the search radius of the patches in the list
    let mut pairs: Vec<(usize, usize)> = cps.iter().map(|c| (c.a.min(c.b), c.a.max(c.b))).collect();
    pairs.sort();
    pairs.dedup();
    let geos: Vec<Geo> =
        (0..n).map(|i| Geo { lens: lenses[lens_of[i]], rot: rots[i], w: photos[i].gray.w as f64, h: photos[i].gray.h as f64 }).collect();
    let imgs = Images::prepare(photos, &geos, &pairs, cfg.foliage.enabled);
    let radius = data.iter().find(|d| d.kind == CpKind::Area).map(|d| d.cfg.search_radius).unwrap_or(cfg.matcher.search_radius);
    let mcfg = MatchConfig { search_radius: radius, ..cfg.matcher.clone() };
    report(0.5, "Comparing the patches again");
    let ov = STORE.overrides();
    // points disabled by hand are measured and listed again (still disabled), not used
    let mut to_measure = cps.clone();
    to_measure.extend(states.iter().filter(|st| st.status == Status::DisabledManual).filter_map(|st| st.cp).filter(|c| c.kind != CpKind::Area));
    let o = measure_round(&geos, &imgs, &to_measure, cfg, &mcfg, &pairs, &ov, "After restarting the optimisation");
    stats.list_total = o.total;
    stats.list_valid = o.valid;
    stats.list_areas = o.areas;
    stats.patches_total = o.total;
    stats.patches_valid = o.valid;
    stats.areas = cps.iter().filter(|c| c.kind == CpKind::Area).count();
    stats.shift_before = f32::NAN;
    stats.shift_after = o.mean_shift;
    stats.diff_after = o.mean_diff;
    let mut rs: Vec<(String, usize)> = o.reasons.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    rs.sort_by_key(|a| std::cmp::Reverse(a.1));
    stats.reasons = rs;
    stats.ms = t0.elapsed().as_millis();
    (rots, lenses, cps, stats)
}

// ---------------------------------------------------------------------------------------
// Parallax mode: local correction field per photo

/// Smooth displacement field over one photo (normalised units = fraction of the long side).
/// It is added to the photo's sampling position, so the correction costs no extra resampling.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DisplacementField {
    pub gw: usize,
    pub gh: usize,
    pub d: Vec<[f32; 2]>,
}

impl DisplacementField {
    /// Displacement at a position given as fractions of the photo width/height (0..1).
    #[inline]
    pub fn at(&self, fx: f64, fy: f64) -> [f64; 2] {
        let x = (fx * self.gw as f64 - 0.5).clamp(0.0, (self.gw - 1) as f64);
        let y = (fy * self.gh as f64 - 0.5).clamp(0.0, (self.gh - 1) as f64);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.gw - 1), (y0 + 1).min(self.gh - 1));
        let (tx, ty) = (x - x0 as f64, y - y0 as f64);
        let g = |xx: usize, yy: usize| self.d[yy * self.gw + xx];
        let mut out = [0.0; 2];
        for k in 0..2 {
            let a = g(x0, y0)[k] as f64 * (1.0 - tx) + g(x1, y0)[k] as f64 * tx;
            let b = g(x0, y1)[k] as f64 * (1.0 - tx) + g(x1, y1)[k] as f64 * tx;
            out[k] = a * (1.0 - ty) + b * ty;
        }
        out
    }
}

/// Settings of the parallax correction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ParallaxConfig {
    pub enabled: bool,
    /// smoothing of the field (Gaussian sigma, in control-area grid cells)
    pub smooth_cells: f64,
    /// largest correction (working-image pixels)
    pub max_shift: f64,
    /// samples deviating more than this from their neighbourhood are dropped (pixels)
    pub outlier_px: f64,
    /// grid resolution of the field (cells along the long side)
    pub grid: usize,
}

impl Default for ParallaxConfig {
    fn default() -> Self {
        ParallaxConfig { enabled: false, smooth_cells: 1.5, max_shift: 6.0, outlier_px: 1.0, grid: 64 }
    }
}

struct FieldSample {
    fx: f64,
    fy: f64,
    d: [f64; 2],
    w: f64,
}

/// Kernel regression with a fade: d = sum(w k d) / (sum(w k) + eps); far from samples it goes to 0.
fn build_field(samples: &[FieldSample], gw: usize, gh: usize, sigma_f: f64, aspect: f64) -> DisplacementField {
    let eps = 0.3;
    let s2 = 2.0 * sigma_f * sigma_f;
    let cut = 9.0 * sigma_f * sigma_f;
    let (kx, ky) = if aspect >= 1.0 { (1.0, 1.0 / aspect) } else { (aspect, 1.0) };
    let d: Vec<[f32; 2]> = (0..gw * gh)
        .into_par_iter()
        .map(|i| {
            let fx = ((i % gw) as f64 + 0.5) / gw as f64;
            let fy = ((i / gw) as f64 + 0.5) / gh as f64;
            let (mut sx, mut sy, mut sw) = (0.0, 0.0, 0.0);
            for s in samples {
                // distances in fractions of the long side
                let dx = (s.fx - fx) * kx;
                let dy = (s.fy - fy) * ky;
                let r2 = dx * dx + dy * dy;
                if r2 > cut {
                    continue;
                }
                let k = s.w * (-r2 / s2).exp();
                sx += k * s.d[0];
                sy += k * s.d[1];
                sw += k;
            }
            [(sx / (sw + eps)) as f32, (sy / (sw + eps)) as f32]
        })
        .collect();
    DisplacementField { gw, gh, d }
}

/// Measure the remaining shift at every control area and turn it into one correction field
/// per photo (both photos of a pair move half-way towards each other).
pub fn parallax_fields(
    photos: &[FaPhoto],
    lens_of: &[usize],
    lenses: &[Lens],
    rots: &[M3],
    cps: &[ControlPoint],
    fine: &FineConfig,
    pc: &ParallaxConfig,
) -> Vec<Option<Arc<DisplacementField>>> {
    let n = photos.len();
    if !pc.enabled || n < 2 {
        return vec![None; n];
    }
    let geos: Vec<Geo> = (0..n)
        .map(|i| Geo { lens: lenses[lens_of[i]], rot: rots[i], w: photos[i].gray.w as f64, h: photos[i].gray.h as f64 })
        .collect();
    let mut pairs: Vec<(usize, usize)> = cps.iter().map(|c| (c.a.min(c.b), c.a.max(c.b))).collect();
    pairs.sort();
    pairs.dedup();
    let imgs = Images::prepare(photos, &geos, &pairs, false);
    let mcfg = MatchConfig { search_radius: (pc.max_shift as f32).max(fine.min_search_radius), ..fine.matcher.clone() };
    let areas: Vec<&ControlPoint> = cps.iter().filter(|c| c.kind != CpKind::Point).collect();
    type Sample = (usize, usize, [f64; 2], [f64; 2], [f64; 2], [f64; 2], f64);
    let measured: Vec<Option<Sample>> = areas
        .par_iter()
        .map(|c| {
            let (ga, gb) = (&geos[c.a], &geos[c.b]);
            let d = ga.px_to_dir(denorm(ga, c.pa));
            let m = measure(ga, gb, imgs.get(c.a, &geos, c.b), imgs.get(c.b, &geos, c.a), d, &mcfg)?;
            if !m.result.valid {
                return None;
            }
            let scale = ga.pixel_angle().max(gb.pixel_angle());
            let plane = Plane::new(d, &ga.rot, scale);
            let (dx, dy) = (m.result.dx as f64, m.result.dy as f64);
            // meet half-way: both photos show this spot at `mid`
            let mid = plane.dir(dx * 0.5, dy * 0.5);
            let pa_now = ga.dir_to_px(mid)?;
            let pb_now = gb.dir_to_px(mid)?;
            let pa_target = ga.dir_to_px(d)?;
            let pb_target = gb.dir_to_px(plane.dir(dx, dy))?;
            let (la, lb) = (ga.long(), gb.long());
            Some((
                c.a,
                c.b,
                [pa_now.0 / ga.w, pa_now.1 / ga.h],
                [(pa_target.0 - pa_now.0) / la, (pa_target.1 - pa_now.1) / la],
                [pb_now.0 / gb.w, pb_now.1 / gb.h],
                [(pb_target.0 - pb_now.0) / lb, (pb_target.1 - pb_now.1) / lb],
                m.result.confidence.max(0.05) as f64,
            ))
        })
        .collect();
    let mut per: Vec<Vec<FieldSample>> = (0..n).map(|_| Vec::new()).collect();
    for (a, b, pa, da, pb, db, w) in measured.into_iter().flatten() {
        let ok_a = da[0].hypot(da[1]) * geos[a].long() <= pc.max_shift;
        let ok_b = db[0].hypot(db[1]) * geos[b].long() <= pc.max_shift;
        if ok_a && ok_b {
            per[a].push(FieldSample { fx: pa[0], fy: pa[1], d: da, w });
            per[b].push(FieldSample { fx: pb[0], fy: pb[1], d: db, w });
        }
    }
    (0..n)
        .map(|i| {
            let s = &per[i];
            if s.len() < 3 {
                return None;
            }
            let g = &geos[i];
            let aspect = g.w / g.h;
            let (gw, gh) = if aspect >= 1.0 {
                (pc.grid, ((pc.grid as f64 / aspect).round() as usize).max(2))
            } else {
                (((pc.grid as f64 * aspect).round() as usize).max(2), pc.grid)
            };
            let sigma_f = fine.grid_cell as f64 * pc.smooth_cells / g.long();
            let first = build_field(s, gw, gh, sigma_f, aspect);
            // drop samples that disagree with their neighbourhood
            let keep: Vec<FieldSample> = s
                .iter()
                .filter(|p| {
                    let f = first.at(p.fx, p.fy);
                    (p.d[0] - f[0]).hypot(p.d[1] - f[1]) * g.long() <= pc.outlier_px
                })
                .map(|p| FieldSample { fx: p.fx, fy: p.fy, d: p.d, w: p.w })
                .collect();
            if keep.len() < 3 {
                return None;
            }
            Some(Arc::new(build_field(&keep, gw, gh, sigma_f, aspect)))
        })
        .collect()
}

#[cfg(test)]
mod parallax_tests {
    use super::*;

    fn sample(fx: f64, fy: f64, d: [f64; 2]) -> FieldSample {
        FieldSample { fx, fy, d, w: 1.0 }
    }

    #[test]
    fn the_field_follows_the_samples_and_fades_out_far_from_them() {
        // a dense block of samples that all want to move by (+0.004, -0.002) in the middle of the photo
        let mut samples = Vec::new();
        for gx in 0..6 {
            for gy in 0..6 {
                samples.push(sample(0.4 + gx as f64 * 0.04, 0.4 + gy as f64 * 0.04, [0.004, -0.002]));
            }
        }
        let f = build_field(&samples, 32, 32, 0.03, 1.0);
        let mid = f.at(0.5, 0.5);
        assert!((mid[0] - 0.004).abs() < 0.0008 && (mid[1] + 0.002).abs() < 0.0006, "middle: {mid:?}");
        let corner = f.at(0.02, 0.02);
        assert!(corner[0].abs() < 1e-6 && corner[1].abs() < 1e-6, "corner: {corner:?}");
    }

    #[test]
    fn without_samples_there_is_no_correction() {
        let f = build_field(&[], 8, 6, 0.05, 1.5);
        assert!(f.d.iter().all(|d| *d == [0.0, 0.0]));
        assert_eq!((f.gw, f.gh), (8, 6));
    }

    #[test]
    fn the_field_is_interpolated_smoothly_and_clamped_at_the_edge() {
        let f = DisplacementField { gw: 2, gh: 1, d: vec![[0.0, 0.0], [0.01, 0.02]] };
        let mid = f.at(0.5, 0.5);
        assert!((mid[0] - 0.005).abs() < 1e-9 && (mid[1] - 0.01).abs() < 1e-9, "{mid:?}");
        // outside the grid the nearest value is used
        assert_eq!(f.at(-1.0, 0.5), [0.0, 0.0]);
        assert!((f.at(2.0, 0.5)[0] - 0.01).abs() < 1e-9);
    }
}

#[cfg(test)]
mod quality_tests {
    use super::*;

    /// Two copies of the same textured picture: aligned they have no shift; one camera turned by a
    /// known angle shows that angle as shift (in pixels) on the independent check.
    #[test]
    fn the_quality_check_sees_a_known_misalignment() {
        let (w, h) = (480u32, 360u32);
        let img = RgbImage::from_fn(w, h, |x, y| {
            let (x, y) = (x as f32, y as f32);
            let v = 128.0 + 50.0 * (x * 0.21).sin() * (y * 0.17).cos() + 40.0 * ((x + 2.0 * y) * 0.093).sin() + 30.0 * ((3.0 * x - y) * 0.051).cos();
            image::Rgb([v as u8; 3])
        });
        let gray = Arc::new(crate::imgutil::to_gray(&img));
        let rgb = Arc::new(img);
        let photos = vec![FaPhoto { gray: gray.clone(), rgb: rgb.clone() }, FaPhoto { gray, rgb }];
        let lenses = [Lens::from_hfov_deg(60.0)];
        let lens_of = [0, 0];
        let aligned = [IDENTITY, IDENTITY];
        let spots = quality_spots(&photos, &lens_of, &lenses, &aligned, &[(0, 1)], 40.0, 60);
        assert!(spots.len() >= 30, "{} spots", spots.len());
        let mcfg = MatchConfig { search_radius: 6.0, ..MatchConfig::default() };
        let q = quality(&photos, &lens_of, &lenses, &aligned, &spots, &mcfg);
        assert!(q.valid * 2 > q.spots && q.median < 0.05, "{q:?}");
        // turn photo b sideways by 1.5 pixels (at the centre)
        let g = Geo { lens: lenses[0], rot: IDENTITY, w: w as f64, h: h as f64 };
        let turned = [IDENTITY, rodrigues([0.0, 1.5 * g.pixel_angle(), 0.0])];
        let q = quality(&photos, &lens_of, &lenses, &turned, &spots, &mcfg);
        assert!(q.valid * 2 > q.spots && (q.median - 1.5).abs() < 0.2, "{q:?}");
    }
}

#[cfg(test)]
mod placement_tests {
    use super::*;

    /// Two identical photos: textured on the left, a plain surface with a faint ramp and noise on the right.
    fn pair(half_plain: bool) -> (Vec<FaPhoto>, [f64; 2]) {
        let (w, h) = (480u32, 360u32);
        let mut rng = crate::geometry::Rng(7);
        let img = RgbImage::from_fn(w, h, |x, y| {
            let (xf, yf) = (x as f32, y as f32);
            let v = if half_plain && x >= w / 2 {
                120.0 + 0.01 * xf + rng.unit() as f32 * 1.5
            } else {
                128.0 + 50.0 * (xf * 0.21).sin() * (yf * 0.17).cos() + 40.0 * ((xf + 2.0 * yf) * 0.093).sin() + 30.0 * ((3.0 * xf - yf) * 0.051).cos()
            };
            image::Rgb([v as u8; 3])
        });
        let gray = Arc::new(crate::imgutil::to_gray(&img));
        let rgb = Arc::new(img);
        let photos = vec![FaPhoto { gray: gray.clone(), rgb: rgb.clone() }, FaPhoto { gray, rgb }];
        (photos, [-0.25, 0.0])
    }

    fn areas(photos: &[FaPhoto], cfg: &FineConfig) -> (Vec<ControlPoint>, FineStats) {
        let lenses = [Lens::from_hfov_deg(60.0)];
        let p = [-0.25, 0.0];
        let seed = ControlPoint::new(0, 1, p, p, false);
        // photo b is turned by two pixels, so the first round has something to correct and its areas are kept
        let g = Geo { lens: lenses[0], rot: IDENTITY, w: 480.0, h: 360.0 };
        let rots = [IDENTITY, rodrigues([0.0, 2.0 * g.pixel_angle(), 0.0])];
        let (_, _, cps, st) = run(photos, &[0, 0], &[480.0, 480.0], &lenses, &rots, &[seed], false, false, cfg, &|_, _| {});
        (cps.into_iter().filter(|c| c.kind == CpKind::Area).collect(), st)
    }

    #[test]
    fn areas_sit_on_structure_and_plain_cells_are_left_out_and_reported() {
        let (photos, _) = pair(true);
        let on = FineConfig { record_patches: false, ..FineConfig::default() };
        let (a, st) = areas(&photos, &on);
        assert!(a.len() >= 10, "{} areas", a.len());
        // no area whose patch (half 32 px = 0.067 of the long side) lies wholly on the plain right half, and the
        // skipped cells are counted and logged
        assert!(a.iter().all(|c| c.pa[0] < 0.08), "an area lies on the plain surface");
        assert!(st.cells_skipped > 0, "plain cells must be reported as left out");
        assert!(st.log.iter().any(|l| l.contains("grid cells left out")));
        // switched off: the plain grid, nothing is skipped (plain patches are then rejected by the matcher)
        let off = FineConfig { record_patches: false, pick_textured: false, ..FineConfig::default() };
        let (_, st) = areas(&photos, &off);
        assert_eq!(st.cells_skipped, 0);
    }

    #[test]
    fn foliage_rating_changes_nothing_on_photos_without_foliage_and_off_is_the_old_behaviour() {
        // grey photos: no colour, so no foliage; areas and weights come out the same with the rating on or off
        let (photos, _) = pair(true);
        let on = FineConfig { record_patches: false, ..FineConfig::default() };
        let off = FineConfig { foliage: FoliageConfig::enabled(false), ..on.clone() };
        let sorted = |mut v: Vec<ControlPoint>| {
            v.sort_by(|x, y| x.pa[0].total_cmp(&y.pa[0]).then(x.pa[1].total_cmp(&y.pa[1])));
            v
        };
        let (a, _) = areas(&photos, &on);
        let (b, _) = areas(&photos, &off);
        let (a, b) = (sorted(a), sorted(b));
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert!(x.pa == y.pa && x.weight == y.weight && x.fol_w == 1.0 && y.fol_w == 1.0, "{:?} {:?} {} {} {} {}", x.pa, y.pa, x.weight, y.weight, x.fol_w, y.fol_w);
        }
    }

    #[test]
    fn areas_may_come_closer_to_the_border_than_the_whole_search_radius() {
        let (photos, _) = pair(false);
        let min_dist = |share: f32| {
            let cfg = FineConfig { record_patches: false, edge_search_share: share, pick_textured: false, grid_cell: 24, max_areas_per_pair: 2000, ..FineConfig::default() };
            let (a, _) = areas(&photos, &cfg);
            assert!(a.len() >= 100, "{} areas", a.len());
            a.iter().map(|c| ((c.pa[0] * 480.0 + 240.0 - 0.5).min(479.0 - (c.pa[0] * 480.0 + 240.0 - 0.5))).min((c.pa[1] * 480.0 + 180.0 - 0.5).min(359.0 - (c.pa[1] * 480.0 + 180.0 - 0.5)))).fold(f64::MAX, f64::min)
        };
        let old = min_dist(1.0);
        let new = min_dist(0.5);
        // half the patch (32) plus the search radius (24) before; plus half of it now
        assert!(old >= 55.0, "old limit {old}");
        assert!(new < old - 5.0 && new >= 43.0, "new limit {new} against {old}");
    }
}
