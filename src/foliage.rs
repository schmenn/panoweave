//! Foliage: trees, bushes and grass move between the shots (wind, a few seconds apart), so points and
//! patches on them pull the alignment off. Instead of deleting them they get a smaller weight in the optimiser.
//!
//! Three clues, in this order of trust:
//!  - colour (green to brown) and
//!  - very fine, irregular structure (many small edges in all directions, no dominant direction)
//!    give a *static* foliage value per spot (`FoliageMap`, one cell is 1/96 of the long side of the
//!    ~1600 px working image), which alone only lowers the weight a little: a green sign, a green wall
//!    (no structure) or a lawn that holds still must not lose its influence;
//!  - the *motion* clue decides: after aligning, the residual errors around a spot are restless (the
//!    neighbours disagree with each other and with the point). It is read from the errors the optimiser leaves
//!    (`rate`), so it needs an alignment first.
//!
//! `value = static * (static_share + (1 - static_share) * motion)`; `weight factor = 1 - (1 - min_weight) * value`.
//! The factor is stored in `ControlPoint::fol_w` and multiplied in by the optimiser. Protected points and
//! points placed by hand are never touched. `FoliageConfig::enabled = false` leaves every factor at 1
//! (exactly the earlier behaviour); that is also the switch for a run of the "maximum accuracy" mode:
//! `FoliageConfig::enabled(false)`.

use crate::align::{ControlPoint, CpKind};
use crate::engine;
use crate::features;
use crate::imgutil::{self, Gray};
use image::RgbImage;
use std::collections::HashMap;

/// Settings of the foliage rating.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FoliageConfig {
    /// switched off = no factor is set (all 1), no patch is steered away from foliage, nothing is offered for removal
    pub enabled: bool,
    /// weight factor of a spot that is certainly foliage (value 1); 1 = no effect
    pub min_weight: f32,
    /// how much of the value colour and structure alone give (the rest needs the motion clue)
    pub static_share: f32,
    /// control areas on foliage: the structure of a candidate spot is multiplied by `1 - pick_penalty * static value`,
    /// so a spot without foliage wins over a spot on foliage (0 = off)
    pub pick_penalty: f32,
    /// radius around a point within which its nearest neighbours are compared for the motion clue, as a share of the long side
    pub neighbour_radius: f32,
    /// at least this many neighbours are needed for the neighbour comparison
    pub min_neighbours: usize,
    /// "Remove points on foliage" offers points whose value is at least this
    pub remove_threshold: f32,
    /// also offer points placed by hand for removal (protected points stay in any case)
    pub include_manual: bool,
}

/// Lowest and highest value of the removal threshold slider.
pub const THRESHOLD_RANGE: (f32, f32) = (0.2, 0.9);

impl Default for FoliageConfig {
    fn default() -> Self {
        FoliageConfig { enabled: true, min_weight: 0.25, static_share: 0.3, pick_penalty: 0.5, neighbour_radius: 0.12, min_neighbours: 3, remove_threshold: 0.5, include_manual: false }
    }
}

impl FoliageConfig {
    /// The default settings with the filter switched on or off (hook for runs that decide per run, e.g. the
    /// "maximum accuracy" mode).
    #[allow(dead_code)] // hook for runs that decide per run (maximum accuracy mode); only tests call it here
    pub fn enabled(on: bool) -> Self {
        FoliageConfig { enabled: on, ..Self::default() }
    }

    /// Environment `PW_FOLIAGE=0` switches the filter off, `PW_FOLIAGE=1` on (experiments, `--accuracy`).
    pub fn with_env(mut self) -> Self {
        match std::env::var("PW_FOLIAGE").as_deref() {
            Ok("0") => self.enabled = false,
            Ok("1") => self.enabled = true,
            _ => {}
        }
        self
    }
}

/// Circle around a spot in which the static value is averaged: 12 px at 800 px photo width (as the sky check
/// judges at 800 px).
const CHECK_SIZE: f32 = 800.0;
const CHECK_RADIUS: f32 = 12.0;
/// A pixel counts as "edge" when its gradient (gray 0..1, after a light blur) exceeds this.
const EDGE_GRADIENT: f32 = 0.025;
/// Share of edge pixels in a cell: below `.0` no fine structure, above `.1` full.
const DENSITY_RAMP: (f32, f32) = (0.3, 0.65);
/// Coherence of the gradient directions in a cell (1 = all one direction, a straight edge): below `.0`
/// irregular, above `.1` ordered (wall, fence, building edge).
const COHERENCE_RAMP: (f32, f32) = (0.45, 0.8);
/// At most this many nearest neighbours are compared.
const NEIGHBOURS_MAX: usize = 6;
/// Ramp of the unrest (restless residual errors in units of the typical error) that counts as motion.
const UNREST_RAMP: (f32, f32) = (0.6, 2.5);

fn ramp(v: f32, lo: f32, hi: f32) -> f32 {
    ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
}

/// Foliage value (0..1) from colour and fine structure per grid cell of one photo.
pub struct FoliageMap {
    pub w: usize,
    pub h: usize,
    pub cell: f32,
    pub gw: usize,
    pub gh: usize,
    pub score: Vec<f32>,
}

/// Colour clue of a mean colour (r, g, b in 0..1): 1 for green to yellow-green, up to 0.5 for dark brown.
pub fn colour_score(r: f32, g: f32, b: f32) -> f32 {
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    if mx < 0.06 {
        return 0.0;
    }
    let sat = (mx - mn) / mx;
    let d = mx - mn;
    let hue = if d < 1e-6 {
        0.0
    } else if mx == r {
        (60.0 * ((g - b) / d)).rem_euclid(360.0)
    } else if mx == g {
        60.0 * ((b - r) / d) + 120.0
    } else {
        60.0 * ((r - g) / d) + 240.0
    };
    let value = ramp(mx, 0.05, 0.15);
    // yellow-green .. green .. grey-blue-green (eucalyptus, dark trees: little saturation); sky blue stays out
    // the grey-blue-green end (hue above 160) only for dark colours, so a grey sky stays out
    let dark = if hue > 160.0 { 1.0 - ramp(mx, 0.45, 0.6) } else { 1.0 };
    let green = ramp(hue, 40.0, 65.0).min(1.0 - ramp(hue, 195.0, 212.0)) * ramp(sat, 0.04, 0.12) * dark;
    // brown (dry leaves, shadowed branches): orange-yellow hue, saturated, not bright
    let brown = ramp(hue, 15.0, 25.0).min(1.0 - ramp(hue, 40.0, 50.0)) * ramp(sat, 0.3, 0.5) * (1.0 - ramp(mx, 0.5, 0.7));
    (green.max(0.5 * brown)) * value
}

/// Foliage map of a working image: colour (green to brown) times fine, irregular structure.
pub fn map_of(img: &RgbImage, gray: &Gray) -> FoliageMap {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let cell = (w.max(h) as f32 / 96.0).max(4.0);
    let gw = (w as f32 / cell).ceil() as usize;
    let gh = (h as f32 / cell).ceil() as usize;
    let smooth = imgutil::blur(gray, 0.8);
    // r, g, b, edge pixels, Jxx, Jyy, Jxy, count
    let mut acc = vec![[0.0f32; 8]; gw * gh];
    let raw = img.as_raw();
    for y in 1..h.saturating_sub(1) {
        let gy = ((y as f32 / cell) as usize).min(gh - 1);
        for x in 1..w.saturating_sub(1) {
            let gx = ((x as f32 / cell) as usize).min(gw - 1);
            let o = (y * w + x) * 3;
            let ix = 0.5 * (smooth.at(x + 1, y) - smooth.at(x - 1, y));
            let iy = 0.5 * (smooth.at(x, y + 1) - smooth.at(x, y - 1));
            let a = &mut acc[gy * gw + gx];
            a[0] += raw[o] as f32 / 255.0;
            a[1] += raw[o + 1] as f32 / 255.0;
            a[2] += raw[o + 2] as f32 / 255.0;
            if ix * ix + iy * iy > EDGE_GRADIENT * EDGE_GRADIENT {
                a[3] += 1.0;
            }
            a[4] += ix * ix;
            a[5] += iy * iy;
            a[6] += ix * iy;
            a[7] += 1.0;
        }
    }
    let score = acc
        .iter()
        .map(|a| {
            let n = a[7].max(1.0);
            let colour = colour_score(a[0] / n, a[1] / n, a[2] / n);
            if colour <= 0.0 {
                return 0.0;
            }
            let density = ramp(a[3] / n, DENSITY_RAMP.0, DENSITY_RAMP.1);
            let tr = a[4] + a[5];
            let coherence = if tr > 1e-9 { (((a[4] - a[5]).powi(2) + 4.0 * a[6] * a[6]).sqrt()) / tr } else { 1.0 };
            let irregular = 1.0 - ramp(coherence, COHERENCE_RAMP.0, COHERENCE_RAMP.1);
            colour * density * irregular
        })
        .collect();
    FoliageMap { w, h, cell, gw, gh, score }
}

impl FoliageMap {
    #[inline]
    pub fn at(&self, x: f32, y: f32) -> f32 {
        if x < 0.0 || y < 0.0 {
            return 0.0;
        }
        let (gx, gy) = ((x / self.cell) as usize, (y / self.cell) as usize);
        if gx < self.gw && gy < self.gh { self.score[gy * self.gw + gx] } else { 0.0 }
    }

    /// Mean static value in a circle around (x, y) (working-image pixels).
    pub fn around(&self, x: f32, y: f32) -> f32 {
        let r = CHECK_RADIUS * self.w.max(self.h) as f32 / CHECK_SIZE;
        features::circle_mean(x, y, r, |px, py| self.at(px, py))
    }

    /// Same for a point in normalised coordinates.
    pub fn around_norm(&self, p: [f64; 2]) -> f32 {
        let (x, y) = engine::denorm_coord(p, self.w, self.h);
        self.around(x, y)
    }

    /// Share of the photo with a value above one half (for reports).
    pub fn share(&self) -> f32 {
        self.score.iter().filter(|v| **v > 0.5).count() as f32 / self.score.len().max(1) as f32
    }
}

/// Foliage maps of all photos (None for a photo that is not given).
pub fn maps_of(photos: &[(&RgbImage, &Gray)]) -> Vec<FoliageMap> {
    use rayon::prelude::*;
    photos.par_iter().map(|(rgb, gray)| map_of(rgb, gray)).collect()
}

/// Static value of a pair spot: the higher of the two photos (None map = not judged).
fn static_value(maps: &[Option<&FoliageMap>], c: &ControlPoint) -> f32 {
    let a = maps.get(c.a).copied().flatten().map(|m| m.around_norm(c.pa));
    let b = maps.get(c.b).copied().flatten().map(|m| m.around_norm(c.pb));
    match (a, b) {
        (Some(x), Some(y)) => x.max(y),
        (Some(x), None) | (None, Some(x)) => x,
        (None, None) => 0.0,
    }
}

fn median(v: &mut [f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

/// Motion clue per control pair (0..1) from the residual errors `err` left by the last alignment: how far a
/// pair's error is from the neighbours' median plus how much the neighbours differ among each other, in
/// units of the typical error of all pairs. Neighbours are the other pairs of the same photo pair within
/// `neighbour_radius` (normalised units of the lower photo). Without enough neighbours the pair's own error
/// counts at half strength; pairs without any error (not aligned yet) get 0.
pub fn motion(cps: &[ControlPoint], cfg: &FoliageConfig) -> Vec<f32> {
    let mut out = vec![0.0f32; cps.len()];
    let mut all: Vec<f32> = cps.iter().filter(|c| c.err > 0.0).map(|c| c.err as f32).collect();
    if all.len() < 4 {
        return out;
    }
    let reference = median(&mut all).max(1e-6);
    // per photo pair: positions (in the lower photo) of the pairs, as (index, x, y)
    let mut groups: HashMap<(usize, usize), Vec<(usize, f64, f64)>> = HashMap::new();
    for (i, c) in cps.iter().enumerate() {
        let (key, p) = if c.a <= c.b { ((c.a, c.b), c.pa) } else { ((c.b, c.a), c.pb) };
        groups.entry(key).or_default().push((i, p[0], p[1]));
    }
    let r2 = (cfg.neighbour_radius as f64).powi(2);
    for list in groups.values() {
        for &(i, x, y) in list {
            // the nearest few others within the radius
            let mut near: Vec<(f64, f32)> = list
                .iter()
                .filter(|&&(j, ..)| j != i && cps[j].err > 0.0)
                .map(|&(j, xj, yj)| ((xj - x).powi(2) + (yj - y).powi(2), cps[j].err as f32))
                .filter(|(d2, _)| *d2 <= r2)
                .collect();
            near.sort_by(|p, q| p.0.total_cmp(&q.0));
            near.truncate(NEIGHBOURS_MAX);
            let mut nb: Vec<f32> = near.into_iter().map(|(_, e)| e).collect();
            let own = cps[i].err as f32;
            let unrest = if nb.len() >= cfg.min_neighbours {
                let m = median(&mut nb);
                let spread = nb.iter().map(|e| (e - m).abs()).sum::<f32>() / nb.len() as f32;
                let d = if own > 0.0 { (own - m).abs() } else { 0.0 };
                (d + spread) / reference
            } else if own > 0.0 {
                (own / reference - 1.0).max(0.0) * 0.5
            } else {
                0.0
            };
            out[i] = ramp(unrest, UNREST_RAMP.0, UNREST_RAMP.1);
        }
    }
    out
}

/// Foliage value (0..1) of every control pair: static value (colour, structure) with the motion clue deciding.
pub fn values(cps: &[ControlPoint], maps: &[Option<&FoliageMap>], cfg: &FoliageConfig) -> Vec<f32> {
    let mo = motion(cps, cfg);
    cps.iter().enumerate().map(|(i, c)| combine(static_value(maps, c), mo[i], cfg)).collect()
}

/// `static * (static_share + (1 - static_share) * motion)`.
pub fn combine(static_value: f32, motion: f32, cfg: &FoliageConfig) -> f32 {
    (static_value * (cfg.static_share + (1.0 - cfg.static_share) * motion)).clamp(0.0, 1.0)
}

/// Weight factor of a foliage value.
pub fn factor(value: f32, cfg: &FoliageConfig) -> f32 {
    (1.0 - (1.0 - cfg.min_weight) * value).clamp(cfg.min_weight.min(1.0), 1.0)
}

/// Whether the rating may change this pair: protected points and points placed by hand never.
fn rateable(c: &ControlPoint) -> bool {
    !c.locked && !c.manual && c.kind != CpKind::AreaManual
}

/// Sets `fol_w` of every pair. Switched off: every factor is put back to 1. Returns how many pairs got a
/// factor below 0.9 (for logs).
pub fn rate(cps: &mut [ControlPoint], maps: &[Option<&FoliageMap>], cfg: &FoliageConfig) -> usize {
    if !cfg.enabled {
        for c in cps.iter_mut() {
            c.fol_w = 1.0;
        }
        return 0;
    }
    let v = values(cps, maps, cfg);
    let mut n = 0;
    for (c, v) in cps.iter_mut().zip(v) {
        c.fol_w = if rateable(c) { factor(v, cfg) } else { 1.0 };
        if c.fol_w < 0.9 {
            n += 1;
        }
    }
    n
}

/// Picture of the foliage map over a photo (red = foliage value, stronger = higher) with the points on it:
/// white ring = normal point, red filled = counted as foliage (value >= 0.5), small yellow dot = control area.
/// For looking at the rating (debug tool `--accuracy` with `PW_FOLIAGE_OVERLAY`).
pub fn overlay(img: &RgbImage, map: &FoliageMap, spots: impl Iterator<Item = ([f64; 2], CpKind, f32)>) -> RgbImage {
    let mut out = img.clone();
    for (x, y, p) in img.enumerate_pixels() {
        let v = map.at(x as f32, y as f32);
        if v > 0.0 {
            let o = out.get_pixel_mut(x, y);
            let t = 0.55 * v;
            for (k, c) in [255.0f32, 30.0, 30.0].into_iter().enumerate() {
                o[k] = (p[k] as f32 * (1.0 - t) + c * t) as u8;
            }
        }
    }
    let (w, h) = (img.width() as i32, img.height() as i32);
    let r0 = (img.width().max(img.height()) as f32 / 200.0).round().max(2.0) as i32;
    let mut dot = |cx: i32, cy: i32, r: i32, fill: bool, col: [u8; 3]| {
        for dy in -r - 1..=r + 1 {
            for dx in -r - 1..=r + 1 {
                let d2 = dx * dx + dy * dy;
                let on = if fill { d2 <= r * r } else { d2 <= (r + 1) * (r + 1) && d2 >= (r - 1) * (r - 1) };
                let (x, y) = (cx + dx, cy + dy);
                if on && x >= 0 && y >= 0 && x < w && y < h {
                    out.put_pixel(x as u32, y as u32, image::Rgb(col));
                }
            }
        }
    };
    for (p, kind, v) in spots {
        let (x, y) = engine::denorm_coord(p, img.width() as usize, img.height() as usize);
        let (x, y) = (x.round() as i32, y.round() as i32);
        match kind {
            CpKind::Point if v >= 0.5 => dot(x, y, r0, true, [255, 0, 80]),
            CpKind::Point => dot(x, y, r0, false, [255, 255, 255]),
            _ => dot(x, y, 1, true, [255, 230, 0]),
        }
    }
    out
}

/// Points (not control areas) the filter counts as foliage, with their value; ordered by point index.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FoliagePoint {
    pub point: usize,
    pub score: f32,
}

pub fn find(cps: &[ControlPoint], maps: &[Option<&FoliageMap>], cfg: &FoliageConfig) -> Vec<FoliagePoint> {
    if !cfg.enabled {
        return Vec::new();
    }
    let v = values(cps, maps, cfg);
    cps.iter()
        .enumerate()
        .filter(|(i, c)| c.kind == CpKind::Point && v[*i] >= cfg.remove_threshold)
        .map(|(i, _)| FoliagePoint { point: i, score: v[i] })
        .collect()
}

/// Shares for reports: (points, patches) whose value is at least `threshold`, as (count, total) each.
pub fn shares(cps: &[ControlPoint], maps: &[Option<&FoliageMap>], cfg: &FoliageConfig, threshold: f32) -> [(usize, usize); 2] {
    let v = values(cps, maps, cfg);
    let mut out = [(0usize, 0usize); 2];
    for (c, v) in cps.iter().zip(v) {
        let k = usize::from(c.kind != CpKind::Point);
        out[k].1 += 1;
        if v >= threshold {
            out[k].0 += 1;
        }
    }
    out
}

/// Which of the found points a removal would take: protected never, by hand only when `include_manual`.
pub fn removable(cps: &[ControlPoint], found: &[FoliagePoint], include_manual: bool) -> crate::sky_points::Removal {
    crate::sky_points::removable_idx(cps, found.iter().map(|f| f.point), include_manual)
}

/// Text for the status line after a removal.
pub fn removal_note(r: &crate::sky_points::Removal) -> String {
    crate::sky_points::removal_note_for(r, "foliage")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rng;
    use image::Rgb;

    const W: u32 = 1200;
    const H: u32 = 800;

    /// Green with fine irregular small structure in every direction (leaves).
    fn leaves(seed: u64, shade: [i32; 3]) -> RgbImage {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        RgbImage::from_fn(W, H, |_, _| {
            let v = rng.unit() as f32;
            let k = (v - 0.5) * 120.0;
            Rgb([(shade[0] as f32 + k * 0.5).clamp(0.0, 255.0) as u8, (shade[1] as f32 + k).clamp(0.0, 255.0) as u8, (shade[2] as f32 + k * 0.4).clamp(0.0, 255.0) as u8])
        })
    }

    /// Smooth green sign with a few straight edges.
    fn sign() -> RgbImage {
        RgbImage::from_fn(W, H, |x, y| if (x / 300 + y / 200) % 2 == 0 { Rgb([40, 130, 60]) } else { Rgb([45, 140, 65]) })
    }

    fn cp(a: usize, b: usize, p: (f32, f32), err: f64) -> ControlPoint {
        let mut c = ControlPoint::new(a, b, engine::norm_coord(p.0, p.1, W as usize, H as usize), engine::norm_coord(p.0, p.1, W as usize, H as usize), false);
        c.err = err;
        c
    }

    fn map(img: &RgbImage) -> FoliageMap {
        map_of(img, &imgutil::to_gray(img))
    }

    #[test]
    fn colours_green_to_brown_count_grey_and_blue_do_not() {
        assert!(colour_score(0.2, 0.5, 0.2) > 0.9);
        assert!(colour_score(0.5, 0.55, 0.15) > 0.9, "yellow-green");
        assert!(colour_score(0.35, 0.22, 0.08) > 0.3, "brown");
        assert_eq!(colour_score(0.5, 0.5, 0.5), 0.0);
        assert_eq!(colour_score(0.3, 0.4, 0.9), 0.0);
        assert_eq!(colour_score(0.01, 0.02, 0.01), 0.0);
        assert!(colour_score(0.30, 0.34, 0.35) > 0.5, "dark grey-green (eucalyptus)");
        assert_eq!(colour_score(0.78, 0.84, 0.87), 0.0, "grey sky");
    }

    #[test]
    fn restless_green_pattern_is_foliage_smooth_green_sign_is_not() {
        let l = map(&leaves(3, [60, 130, 40]));
        let s = map(&sign());
        let grey = map(&leaves(4, [120, 120, 120]));
        let (vl, vs, vg) = (l.around(600.0, 400.0), s.around(600.0, 400.0), grey.around(600.0, 400.0));
        assert!(vl > 0.7, "leaves {vl}");
        assert!(vs < 0.05, "sign {vs}");
        assert!(vg < 0.05, "grey texture {vg}");
    }

    #[test]
    fn colour_and_structure_alone_only_lower_the_weight_a_little() {
        let cfg = FoliageConfig::default();
        let f = factor(combine(1.0, 0.0, &cfg), &cfg);
        assert!(f > 0.7 && f < 0.85, "{f}");
        let f = factor(combine(1.0, 1.0, &cfg), &cfg);
        assert!((f - cfg.min_weight).abs() < 1e-6, "{f}");
        assert_eq!(factor(combine(0.0, 1.0, &cfg), &cfg), 1.0, "no foliage look = no foliage, whatever the motion");
    }

    /// 12 points in a row on a photo pair; the errors of all are 1, except the listed ones.
    fn row(errs: &[f64]) -> Vec<ControlPoint> {
        errs.iter().enumerate().map(|(i, e)| cp(0, 1, (300.0 + 50.0 * i as f32, 400.0), *e)).collect()
    }

    /// Errors of 12 neighbours: most near 1, some far below and far above (moving leaves).
    fn restless_errs() -> Vec<f64> {
        (0..12).map(|i| match i % 4 {
            0 => 0.2,
            2 => 4.0,
            _ => 1.0,
        }).collect()
    }

    /// A strongly restless row (errors jump between 0.2 and 4) plus 12 calm points (error 1) far away that set
    /// the typical error. The first 12 are the row.
    fn row_with_calm() -> Vec<ControlPoint> {
        let errs: Vec<f64> = (0..12).map(|i| if i % 2 == 0 { 0.2 } else { 4.0 }).collect();
        let mut v = row(&errs);
        v.extend((0..12).map(|i| cp(0, 1, (300.0 + 50.0 * i as f32, 100.0), 1.0)));
        v
    }

    #[test]
    fn moving_leaves_restless_errors_get_motion_calm_errors_do_not() {
        let cfg = FoliageConfig::default();
        let calm = motion(&row(&[1.0; 12]), &cfg);
        assert!(calm.iter().all(|m| *m == 0.0), "{calm:?}");
        let restless = restless_errs();
        let m = motion(&row(&restless), &cfg);
        let mean = m.iter().sum::<f32>() / m.len() as f32;
        assert!(mean > 0.35 && m.iter().any(|v| *v > 0.5), "{m:?}");
        // no alignment yet (err 0): no motion
        assert!(motion(&row(&[0.0; 12]), &cfg).iter().all(|m| *m == 0.0));
    }

    #[test]
    fn motion_decides_between_foliage_and_a_green_wall_with_the_same_look() {
        let cfg = FoliageConfig::default();
        let img = leaves(3, [60, 130, 40]); // looks like foliage everywhere
        let m = map(&img);
        let maps = [Some(&m), Some(&m)];
        let restless = restless_errs();
        let mut moving = row(&restless);
        let mut still = row(&[1.0; 12]);
        rate(&mut moving, &maps, &cfg);
        rate(&mut still, &maps, &cfg);
        let (fm, fs) = (moving.iter().map(|c| c.fol_w).sum::<f32>() / 12.0, still.iter().map(|c| c.fol_w).sum::<f32>() / 12.0);
        assert!(fm < 0.65, "moving {fm}");
        assert!(fs > 0.7 && fs < 0.9, "still but foliage-like: weak {fs}");
        assert!(fm < fs - 0.15);
        // a smooth green sign with restless errors keeps its weight
        let s = map(&sign());
        let maps = [Some(&s), Some(&s)];
        let mut on_sign = row(&restless);
        rate(&mut on_sign, &maps, &cfg);
        assert!(on_sign.iter().all(|c| c.fol_w > 0.95), "{:?}", on_sign.iter().map(|c| c.fol_w).collect::<Vec<_>>());
    }

    #[test]
    fn protected_and_hand_placed_are_never_down_weighted_and_off_resets_everything() {
        let cfg = FoliageConfig::default();
        let m = map(&leaves(3, [60, 130, 40]));
        let maps = [Some(&m), Some(&m)];
        let restless = restless_errs();
        let mut v = row(&restless);
        v[1].locked = true;
        v[3].manual = true;
        v[5].kind = CpKind::AreaManual;
        let n = rate(&mut v, &maps, &cfg);
        assert!(n >= 6);
        for i in [1, 3, 5] {
            assert_eq!(v[i].fol_w, 1.0, "{i}");
        }
        assert!(v[0].fol_w < 0.9);
        // switched off: factors back to 1, nothing found
        let off = FoliageConfig::enabled(false);
        assert_eq!(rate(&mut v, &maps, &off), 0);
        assert!(v.iter().all(|c| c.fol_w == 1.0));
        assert!(find(&v, &maps, &off).is_empty());
    }

    #[test]
    fn finds_points_and_offers_only_automatic_unprotected_ones() {
        let cfg = FoliageConfig::default();
        let m = map(&leaves(3, [60, 130, 40]));
        let maps = [Some(&m), Some(&m)];
        let mut v = row_with_calm();
        v[1].locked = true;
        v[3].manual = true;
        v[5].kind = CpKind::Area;
        let found = find(&v, &maps, &cfg);
        assert!(found.iter().any(|f| f.point == 1) && found.iter().all(|f| f.point != 5), "areas are not offered");
        let r = removable(&v, &found, false);
        assert!(!r.remove.contains(&1) && !r.remove.contains(&3) && !r.remove.is_empty());
        assert_eq!((r.skipped_protected, r.skipped_manual), (1, 1));
        let r = removable(&v, &found, true);
        assert!(r.remove.contains(&3) && !r.remove.contains(&1));
        assert!(removal_note(&r).contains("foliage"));
    }

    #[test]
    fn shares_count_points_and_patches_apart() {
        let cfg = FoliageConfig::default();
        let m = map(&leaves(3, [60, 130, 40]));
        let maps = [Some(&m), Some(&m)];
        let mut v = row_with_calm();
        for c in v.iter_mut().take(12).skip(8) {
            c.kind = CpKind::Area;
        }
        let s = shares(&v, &maps, &cfg, 0.5);
        assert_eq!((s[0].1, s[1].1), (20, 4));
        assert!(s[0].0 > 0 && s[1].0 > 0);
    }

    #[test]
    fn config_reads_old_settings_and_env_switch() {
        let c: FoliageConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(c, FoliageConfig::default());
        assert!(FoliageConfig::default().enabled);
        assert!(!FoliageConfig::enabled(false).enabled);
    }
}
