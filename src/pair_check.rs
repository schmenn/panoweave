//! Geometric pair check: after an alignment, every photo pair whose pictures overlap (from the
//! current geometry) is checked for control points. Pairs with too few points get, in this
//! order: (1) the normal point finder again, but only inside the overlap; (2) a targeted search
//! predicted through a third photo that is connected to both; (3) patch matching on a grid in
//! the overlap. Points that disagree with the prediction are rejected. Pairs that still have too
//! few points are reported as weak - no points are made up.
//!
//! Where the pairs were chosen before: `engine::candidate_pairs` (all pairs up to 30 photos,
//! otherwise a quick descriptor check per photo) - purely by image features, without geometry.
//! A pair was dropped when fewer than `engine::MIN_VERIFIED` matches survived the check.
//! This module adds to that; the finder itself is unchanged. With `enabled = false` nothing runs.

use crate::align::{self, ControlPoint, CpKind, Lens};
use crate::engine::{self, MatchSettings};
use crate::features;
use crate::fine_align::{self, FaPhoto, Geo};
use crate::geometry::*;
use crate::imgutil::Gray;
use crate::patch_matcher::MatchConfig;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Allowed range of an overlap threshold: the original accepts a minimum overlap only when
/// 0.001 < value <= 1.0 ("The minimum overlap has to be greater than 0 and smaller than 1.").
pub const MIN_OVERLAP_RANGE: (f32, f32) = (0.001, 1.0);

/// Overlap from which two photos "obviously" overlap (shown as a hint in the settings).
pub const OBVIOUS_OVERLAP: f32 = 0.30;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PairCheckConfig {
    /// master switch
    pub enabled: bool,
    /// a pair is a candidate from this overlap on (share of the smaller photo)
    pub min_overlap: f32,
    /// fewer valid points than this: the pair is searched again
    pub min_points: usize,
    /// safety margin around the estimated overlap (share of the photo's long side)
    pub margin: f32,
    /// step 1: the normal finder, only inside the overlap
    pub restrict_search: bool,
    /// step 2: prediction through a third photo
    pub predict_via_third: bool,
    /// step 3: patch matching on a grid in the overlap
    pub patch_fallback: bool,
    /// found points further than this many standard deviations from the prediction are outliers
    pub outlier_sigma: f64,
    /// smallest standard deviation used in that test (working-image pixels)
    pub sigma_floor_px: f64,
    /// align again and repeat the check at most this often
    pub max_rounds: usize,
    /// samples per side when estimating the overlap
    pub overlap_grid: usize,
    /// a third photo needs at least this many points to each of the two
    pub third_min_points: usize,
    /// step 2: corners looked at, template size, search radius and minimum score
    pub predict_corners: usize,
    pub predict_template: usize,
    pub predict_search: f32,
    pub predict_min_score: f32,
    /// step 3: grid step (working-image pixels) and most patches per pair
    pub patch_step: f64,
    pub patch_max: usize,
    /// connection view: a pair with at least this many points is "well connected" (green)
    pub enough_points: usize,
}

impl Default for PairCheckConfig {
    fn default() -> Self {
        PairCheckConfig {
            enabled: true,
            min_overlap: 0.10,
            min_points: 8,
            margin: 0.10,
            restrict_search: true,
            predict_via_third: true,
            patch_fallback: true,
            outlier_sigma: 3.0,
            sigma_floor_px: 1.0,
            max_rounds: 3,
            overlap_grid: 24,
            third_min_points: 3,
            predict_corners: 400,
            predict_template: 21,
            predict_search: 6.0,
            predict_min_score: 0.8,
            patch_step: 48.0,
            patch_max: 80,
            enough_points: 8,
        }
    }
}

/// Estimated overlap of two photos.
#[derive(Clone, Debug)]
pub struct PairOverlap {
    pub a: usize,
    pub b: usize,
    /// overlapping area / area of the smaller photo (0..1)
    pub share: f32,
    /// bounding box of the overlap in each photo (working-image pixels: x0, y0, x1, y1)
    pub box_a: [f64; 4],
    pub box_b: [f64; 4],
}

/// One step tried for a pair.
#[derive(Clone, Debug)]
pub struct StepLog {
    pub step: &'static str,
    /// points of the pair after this step
    pub points: usize,
    pub note: String,
}

#[derive(Clone, Debug)]
pub struct PairReport {
    pub a: usize,
    pub b: usize,
    pub overlap: f32,
    pub before: usize,
    pub steps: Vec<StepLog>,
    pub added: usize,
    /// points of the pair after the last alignment (outlier clean-up included)
    pub after: usize,
    /// still too few points after every step
    pub weak: bool,
    /// the step that brought enough points
    pub success: Option<&'static str>,
}

/// Everything the check found (kept by the main window for the connection view).
#[derive(Clone, Debug, Default)]
pub struct PairSummary {
    pub overlaps: Vec<PairOverlap>,
    pub reports: Vec<PairReport>,
    pub log: Vec<String>,
    pub rounds: usize,
    /// pairs with at least one control point before / after
    pub pairs_before: usize,
    pub pairs_after: usize,
    pub ran: bool,
}

pub const STEP_RESTRICTED: &str = "finder inside the overlap";
pub const STEP_PREDICT: &str = "prediction through a third photo";
pub const STEP_PATCH: &str = "patch matching in the overlap";

pub fn geos(photos: &[FaPhoto], lens_of: &[usize], lenses: &[Lens], rots: &[M3]) -> Vec<Geo> {
    (0..photos.len()).map(|i| Geo { lens: lenses[lens_of[i]], rot: rots[i], w: photos[i].gray.w as f64, h: photos[i].gray.h as f64 }).collect()
}

/// Share of `from`'s samples that land inside `to`, and their bounding box in `from`.
fn sample_overlap(from: &Geo, to: &Geo, n: usize) -> (f64, [f64; 4]) {
    let n = n.max(4);
    let (cw, ch) = (from.w / n as f64, from.h / n as f64);
    let mut cnt = 0usize;
    let mut bx = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for gy in 0..n {
        for gx in 0..n {
            let p = ((gx as f64 + 0.5) * cw, (gy as f64 + 0.5) * ch);
            let d = from.px_to_dir(p);
            if let Some(q) = to.dir_to_px(d)
                && to.inside(q, 0.0) {
                    cnt += 1;
                    bx = [bx[0].min(p.0 - cw * 0.5), bx[1].min(p.1 - ch * 0.5), bx[2].max(p.0 + cw * 0.5), bx[3].max(p.1 + ch * 0.5)];
                }
        }
    }
    (cnt as f64 / (n * n) as f64, bx)
}

/// Overlap of two photos from their geometry (both directions sampled, area in steradians).
pub fn estimate_overlap(a: usize, b: usize, ga: &Geo, gb: &Geo, grid: usize) -> Option<PairOverlap> {
    let (fa, box_a) = sample_overlap(ga, gb, grid);
    let (fb, box_b) = sample_overlap(gb, ga, grid);
    if fa <= 0.0 || fb <= 0.0 {
        return None;
    }
    let area = |g: &Geo| g.w * g.h * g.pixel_angle() * g.pixel_angle();
    let (aa, ab) = (area(ga), area(gb));
    let ov = (fa * aa + fb * ab) * 0.5;
    let share = (ov / aa.min(ab)).clamp(0.0, 1.0) as f32;
    Some(PairOverlap { a, b, share, box_a, box_b })
}

/// Angular radius of a photo (centre to the farthest corner).
fn radius(g: &Geo) -> f64 {
    let c = g.px_to_dir((g.w * 0.5, g.h * 0.5));
    [(0.0, 0.0), (g.w, 0.0), (0.0, g.h), (g.w, g.h)].iter().map(|&p| dot(c, g.px_to_dir(p)).clamp(-1.0, 1.0).acos()).fold(0.0, f64::max)
}

/// Overlaps of all photo pairs that can overlap. Not all n² pairs are measured: the photos are
/// sorted by the polar angle of their centre, and a pair is only looked at when the angle
/// difference is below the sum of both radii (a necessary condition on the sphere).
pub fn overlaps(geos: &[Geo], used: &[bool], grid: usize, min_share: f32) -> Vec<PairOverlap> {
    let n = geos.len();
    let info: Vec<(usize, V3, f64, f64)> = (0..n)
        .filter(|&i| used[i])
        .map(|i| {
            let g = &geos[i];
            let c = g.px_to_dir((g.w * 0.5, g.h * 0.5));
            (i, c, c[2].clamp(-1.0, 1.0).acos(), radius(g))
        })
        .collect();
    let mut order: Vec<usize> = (0..info.len()).collect();
    order.sort_by(|&x, &y| info[x].2.partial_cmp(&info[y].2).unwrap_or(std::cmp::Ordering::Equal));
    let rmax = info.iter().map(|x| x.3).fold(0.0, f64::max);
    let mut cand: Vec<(usize, usize)> = Vec::new();
    for (k, &x) in order.iter().enumerate() {
        for &y in order[k + 1..].iter() {
            if info[y].2 - info[x].2 > info[x].3 + rmax {
                break;
            }
            let ang = dot(info[x].1, info[y].1).clamp(-1.0, 1.0).acos();
            if ang < info[x].3 + info[y].3 {
                let (i, j) = (info[x].0.min(info[y].0), info[x].0.max(info[y].0));
                cand.push((i, j));
            }
        }
    }
    let mut out: Vec<PairOverlap> = cand.par_iter().filter_map(|&(i, j)| estimate_overlap(i, j, &geos[i], &geos[j], grid)).filter(|o| o.share >= min_share).collect();
    out.sort_by_key(|o| (o.a, o.b));
    out
}

/// Points between two photos (any kind, any direction).
pub fn pair_counts(cps: &[ControlPoint]) -> HashMap<(usize, usize), usize> {
    let mut m = HashMap::new();
    for c in cps {
        *m.entry((c.a.min(c.b), c.a.max(c.b))).or_insert(0) += 1;
    }
    m
}

/// Rotation R with ray_to ≈ R · ray_from (Horn's quaternion method, power iteration).
fn rotation_between(pairs: &[(V3, V3)]) -> Option<M3> {
    if pairs.len() < 2 {
        return None;
    }
    let mut s = [[0.0f64; 3]; 3];
    for (a, b) in pairs {
        for i in 0..3 {
            for j in 0..3 {
                s[i][j] += a[i] * b[j];
            }
        }
    }
    let (sxx, sxy, sxz, syx, syy, syz, szx, szy, szz) = (s[0][0], s[0][1], s[0][2], s[1][0], s[1][1], s[1][2], s[2][0], s[2][1], s[2][2]);
    let nm = [
        [sxx + syy + szz, syz - szy, szx - sxz, sxy - syx],
        [syz - szy, sxx - syy - szz, sxy + syx, szx + sxz],
        [szx - sxz, sxy + syx, -sxx + syy - szz, syz + szy],
        [sxy - syx, szx + sxz, syz + szy, -sxx - syy + szz],
    ];
    // shift so the largest eigenvalue is also the largest in size
    let shift: f64 = nm.iter().map(|r| r.iter().map(|v| v.abs()).sum::<f64>()).fold(0.0, f64::max);
    let mut q = [1.0, 0.0, 0.0, 0.0];
    for _ in 0..200 {
        let mut nq = [0.0; 4];
        for i in 0..4 {
            nq[i] = shift * q[i] + (0..4).map(|j| nm[i][j] * q[j]).sum::<f64>();
        }
        let l = nq.iter().map(|v| v * v).sum::<f64>().sqrt();
        if l < 1e-12 {
            return None;
        }
        q = [nq[0] / l, nq[1] / l, nq[2] / l, nq[3] / l];
    }
    let [w, x, y, z] = q;
    Some([
        [w * w + x * x - y * y - z * z, 2.0 * (x * y - w * z), 2.0 * (x * z + w * y)],
        [2.0 * (x * y + w * z), w * w - x * x + y * y - z * z, 2.0 * (y * z - w * x)],
        [2.0 * (x * z - w * y), 2.0 * (y * z + w * x), w * w - x * x - y * y + z * z],
    ])
}

/// Rotation from photo `from`'s camera to photo `to`'s camera, from their control points.
fn relative_rotation(cps: &[ControlPoint], from: usize, to: usize, lf: &Lens, lt: &Lens) -> Option<(M3, usize)> {
    let pairs: Vec<(V3, V3)> = cps
        .iter()
        .filter_map(|c| {
            if c.a == from && c.b == to {
                Some((align::ray(lf, c.pa), align::ray(lt, c.pb)))
            } else if c.a == to && c.b == from {
                Some((align::ray(lf, c.pb), align::ray(lt, c.pa)))
            } else {
                None
            }
        })
        .collect();
    let n = pairs.len();
    rotation_between(&pairs).map(|r| (r, n))
}

/// Keeps the deviations that agree with the rest: |d - median| <= k · max(sigma, floor), with a
/// robust sigma (1.4826 · median absolute deviation). Returns the kept indices.
pub fn consistent(dev: &[(f64, f64)], k: f64, floor: f64) -> Vec<bool> {
    if dev.len() < 3 {
        return vec![true; dev.len()];
    }
    let med = |mut v: Vec<f64>| -> f64 {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[v.len() / 2]
    };
    let mx = med(dev.iter().map(|d| d.0).collect());
    let my = med(dev.iter().map(|d| d.1).collect());
    let r: Vec<f64> = dev.iter().map(|d| (d.0 - mx).hypot(d.1 - my)).collect();
    let sigma = (1.4826 * med(r.clone())).max(floor);
    r.iter().map(|&x| x <= k * sigma).collect()
}

fn crop(g: &Gray, bx: [f64; 4], margin: f64) -> (Gray, usize, usize) {
    let x0 = (bx[0] - margin).floor().max(0.0) as usize;
    let y0 = (bx[1] - margin).floor().max(0.0) as usize;
    let x1 = ((bx[2] + margin).ceil() as usize).min(g.w);
    let y1 = ((bx[3] + margin).ceil() as usize).min(g.h);
    let (w, h) = (x1.saturating_sub(x0).max(1), y1.saturating_sub(y0).max(1));
    let mut out = Gray::new(w, h);
    for y in 0..h {
        let src = (y0 + y).min(g.h - 1) * g.w + x0;
        out.data[y * w..y * w + w].copy_from_slice(&g.data[src..src + w]);
    }
    (out, x0, y0)
}

fn norm_px(g: &Geo, p: (f64, f64)) -> [f64; 2] {
    fine_align::norm(g, p)
}

struct Ctx<'a> {
    photos: &'a [FaPhoto],
    geos: &'a [Geo],
    lens_of: &'a [usize],
    lenses: &'a [Lens],
    cps: &'a [ControlPoint],
    counts: &'a HashMap<(usize, usize), usize>,
    used: &'a [bool],
    cfg: &'a PairCheckConfig,
    matcher: &'a MatchConfig,
    settings: MatchSettings,
    skies: Mutex<HashMap<usize, Arc<features::SkyMask>>>,
}

impl Ctx<'_> {
    fn sky(&self, i: usize) -> Arc<features::SkyMask> {
        if let Some(m) = self.skies.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&i) {
            return m.clone();
        }
        let m = Arc::new(features::sky_mask(&self.photos[i].rgb, &self.photos[i].gray));
        self.skies.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(i, m.clone());
        m
    }
    fn count(&self, a: usize, b: usize) -> usize {
        self.counts.get(&(a.min(b), a.max(b))).copied().unwrap_or(0)
    }

    /// Corners of a photo inside a box (working-image pixels), sky removed when asked.
    fn features_in(&self, i: usize, bx: [f64; 4], max: usize) -> features::Features {
        let g = &self.photos[i].gray;
        let margin = self.cfg.margin as f64 * g.w.max(g.h) as f64;
        let (c, x0, y0) = crop(g, bx, margin);
        let mut f = features::detect(&c, max);
        for k in f.kps.iter_mut() {
            k.x += x0 as f32;
            k.y += y0 as f32;
        }
        f.w = g.w;
        f.h = g.h;
        if self.settings.ignore_sky {
            features::remove_masked(&mut f, &self.sky(i));
        }
        f
    }

    /// Step 1: the normal finder, only inside the overlap of both photos.
    fn restricted(&self, o: &PairOverlap, note: &mut String) -> Vec<ControlPoint> {
        let (a, b) = (o.a, o.b);
        let fa = self.features_in(a, o.box_a, self.settings.points_per_photo);
        let fb = self.features_in(b, o.box_b, self.settings.points_per_photo);
        let Some(pm) = features::match_pair(&fa, &fb, engine::VERIFY_POOL, (a * 7919 + b * 104729) as u64 + 7) else {
            *note = format!("{} / {} corners in the overlap, no consistent matches", fa.kps.len(), fb.kps.len());
            return Vec::new();
        };
        let (gra, grb) = (&self.photos[a].gray, &self.photos[b].gray);
        let checked = engine::verify_matches(gra, grb, &pm.h, &pm.corrs);
        let (ga, gb) = (&self.geos[a], &self.geos[b]);
        // agreement with the current geometry
        let dev: Vec<(f64, f64)> = checked
            .iter()
            .map(|c| match gb.dir_to_px(ga.px_to_dir((c.ax as f64, c.ay as f64))) {
                Some(q) => (c.bx as f64 - q.0, c.by as f64 - q.1),
                None => (f64::MAX, f64::MAX),
            })
            .collect();
        let keep = consistent(&dev, self.cfg.outlier_sigma, self.cfg.sigma_floor_px);
        let good: Vec<features::Corr> = checked.iter().zip(&keep).filter(|(_, k)| **k).map(|(c, _)| *c).collect();
        let thinned = features::thin_mode(good.clone(), gra.w, gra.h, self.settings.points_per_pair, self.settings.even_thinning);
        *note = format!(
            "{} matches, {} passed the correlation check, {} rejected as outliers against the geometry, {} kept",
            pm.corrs.len(),
            checked.len(),
            checked.len() - good.len(),
            thinned.len()
        );
        thinned
            .iter()
            .map(|c| ControlPoint::new(a, b, engine::norm_coord(c.ax, c.ay, gra.w, gra.h), engine::norm_coord(c.bx, c.by, grb.w, grb.h), false))
            .collect()
    }

    /// Step 2: the relation A -> B chained through the best third photo C, then a targeted
    /// search with a small radius around the predicted spots.
    fn predicted(&self, o: &PairOverlap, note: &mut String) -> Vec<ControlPoint> {
        let (a, b) = (o.a, o.b);
        let n = self.photos.len();
        let best = (0..n)
            .filter(|&c| c != a && c != b && self.used[c])
            .map(|c| (c, self.count(a, c).min(self.count(c, b))))
            .filter(|x| x.1 >= self.cfg.third_min_points)
            .max_by_key(|x| x.1);
        let Some((c, _)) = best else {
            *note = "no third photo with points to both".into();
            return Vec::new();
        };
        let (la, lb, lc) = (self.lenses[self.lens_of[a]], self.lenses[self.lens_of[b]], self.lenses[self.lens_of[c]]);
        let (Some((rac, nac)), Some((rcb, ncb))) = (relative_rotation(self.cps, a, c, &la, &lc), relative_rotation(self.cps, c, b, &lc, &lb)) else {
            *note = format!("photo {}: the relations could not be estimated", c + 1);
            return Vec::new();
        };
        let rab = mm(&rcb, &rac);
        let ga = self.geos[a];
        let gbp = Geo { rot: mm(&rab, &ga.rot), ..self.geos[b] };
        let Some(op) = estimate_overlap(a, b, &ga, &gbp, self.cfg.overlap_grid) else {
            *note = format!("through photo {} ({nac} + {ncb} points) the photos would not overlap", c + 1);
            return Vec::new();
        };
        let f = self.features_in(a, op.box_a, self.cfg.predict_corners);
        let pcfg = MatchConfig {
            patch_size: self.cfg.predict_template,
            search_radius: self.cfg.predict_search,
            min_score: self.cfg.predict_min_score,
            ..self.matcher.clone()
        };
        let (pa_img, pb_img) = (&self.photos[a], &self.photos[b]);
        let found: Vec<(features::Corr, (f64, f64), &'static str)> = f
            .kps
            .par_iter()
            .filter_map(|k| {
                let p = (k.x as f64, k.y as f64);
                let d = ga.px_to_dir(p);
                let q0 = gbp.dir_to_px(d).filter(|q| gbp.inside(*q, pcfg.patch_size as f64))?;
                let m = fine_align::measure(&ga, &gbp, (&pa_img.gray, &pa_img.rgb), (&pb_img.gray, &pb_img.rgb), d, &pcfg)?;
                let q = gbp.dir_to_px(m.dir_b)?;
                let c = features::Corr { ax: k.x, ay: k.y, bx: q.0 as f32, by: q.1 as f32, dist: 0 };
                Some((c, (q.0 - q0.0, q.1 - q0.1), if m.result.valid { "" } else { m.result.reason }))
            })
            .collect();
        let mut reasons: HashMap<&str, usize> = HashMap::new();
        let ok: Vec<(features::Corr, (f64, f64))> = found
            .iter()
            .filter(|x| {
                if !x.2.is_empty() {
                    *reasons.entry(x.2).or_insert(0) += 1;
                }
                x.2.is_empty()
            })
            .map(|x| (x.0, x.1))
            .collect();
        let keep = consistent(&ok.iter().map(|x| x.1).collect::<Vec<_>>(), self.cfg.outlier_sigma, self.cfg.sigma_floor_px);
        let good: Vec<features::Corr> = ok.iter().zip(&keep).filter(|(_, k)| **k).map(|(x, _)| x.0).collect();
        let (gra, grb) = (&pa_img.gray, &pb_img.gray);
        let thinned = features::thin_mode(good.clone(), gra.w, gra.h, self.settings.points_per_pair, self.settings.even_thinning);
        let mut rs: Vec<String> = reasons.iter().map(|(k, v)| format!("{v} {k}")).collect();
        rs.sort();
        *note = format!(
            "through photo {} ({nac} + {ncb} points): {} corners, {} matched, {} outliers against the prediction, {} kept{}",
            c + 1,
            f.kps.len(),
            ok.len(),
            ok.len() - good.len(),
            thinned.len(),
            if rs.is_empty() { String::new() } else { format!(" (rejected: {})", rs.join(", ")) }
        );
        thinned
            .iter()
            .map(|c| ControlPoint::new(a, b, engine::norm_coord(c.ax, c.ay, gra.w, gra.h), engine::norm_coord(c.bx, c.by, grb.w, grb.h), false))
            .collect()
    }

    /// Step 3: patch matching on a grid in the overlap (the control-area matcher).
    fn patches(&self, o: &PairOverlap, note: &mut String) -> Vec<ControlPoint> {
        let (a, b) = (o.a, o.b);
        let (ga, gb) = (&self.geos[a], &self.geos[b]);
        let step = self.cfg.patch_step.max(4.0);
        let half = self.matcher.patch_size as f64 * 0.5;
        let mut spots: Vec<(f64, f64)> = Vec::new();
        let mut y = o.box_a[1] + step * 0.5;
        while y < o.box_a[3] {
            let mut x = o.box_a[0] + step * 0.5;
            while x < o.box_a[2] {
                if ga.inside((x, y), half) {
                    spots.push((x, y));
                }
                x += step;
            }
            y += step;
        }
        if spots.len() > self.cfg.patch_max {
            let k = spots.len() as f64 / self.cfg.patch_max as f64;
            spots = (0..self.cfg.patch_max).map(|i| spots[(i as f64 * k) as usize]).collect();
        }
        let (pa_img, pb_img) = (&self.photos[a], &self.photos[b]);
        let res: Vec<(ControlPoint, (f64, f64), &'static str)> = spots
            .par_iter()
            .filter_map(|&p| {
                let d = ga.px_to_dir(p);
                let m = fine_align::measure(ga, gb, (&pa_img.gray, &pa_img.rgb), (&pb_img.gray, &pb_img.rgb), d, self.matcher)?;
                let q = gb.dir_to_px(m.dir_b)?;
                let mut c = ControlPoint::new(a, b, norm_px(ga, p), norm_px(gb, q), false);
                c.kind = CpKind::Point;
                c.conf = m.result.confidence;
                c.score = m.result.score;
                c.shift = [m.result.dx, m.result.dy];
                // confidence as weight
                c.weight = (m.result.confidence as f64).max(1e-3);
                Some((c, (m.result.dx as f64, m.result.dy as f64), if m.result.valid { "" } else { m.result.reason }))
            })
            .collect();
        let mut reasons: HashMap<&str, usize> = HashMap::new();
        let ok: Vec<(ControlPoint, (f64, f64))> = res
            .into_iter()
            .filter(|x| {
                if !x.2.is_empty() {
                    *reasons.entry(x.2).or_insert(0) += 1;
                }
                x.2.is_empty()
            })
            .map(|x| (x.0, x.1))
            .collect();
        let keep = consistent(&ok.iter().map(|x| x.1).collect::<Vec<_>>(), self.cfg.outlier_sigma, self.cfg.sigma_floor_px);
        let good: Vec<ControlPoint> = ok.iter().zip(&keep).filter(|(_, k)| **k).map(|(x, _)| x.0).collect();
        let mut rs: Vec<String> = reasons.iter().map(|(k, v)| format!("{v} {k}")).collect();
        rs.sort();
        *note = format!(
            "{} patches, {} valid, {} outliers, {} kept{}",
            spots.len(),
            ok.len(),
            ok.len() - good.len(),
            good.len(),
            if rs.is_empty() { String::new() } else { format!(" (rejected: {})", rs.join(", ")) }
        );
        good
    }

    fn check(&self, o: &PairOverlap) -> (PairReport, Vec<ControlPoint>) {
        let before = self.count(o.a, o.b);
        let mut rep = PairReport { a: o.a, b: o.b, overlap: o.share, before, steps: Vec::new(), added: 0, after: before, weak: false, success: None };
        let mut have = before;
        let mut add: Vec<ControlPoint> = Vec::new();
        let steps: [(&'static str, bool); 3] = [(STEP_RESTRICTED, self.cfg.restrict_search), (STEP_PREDICT, self.cfg.predict_via_third), (STEP_PATCH, self.cfg.patch_fallback)];
        for (name, on) in steps {
            if have >= self.cfg.min_points || !on || engine::cancelled() {
                continue;
            }
            let mut note = String::new();
            let pts = match name {
                STEP_RESTRICTED => self.restricted(o, &mut note),
                STEP_PREDICT => self.predicted(o, &mut note),
                _ => self.patches(o, &mut note),
            };
            // points the pair already has (or an earlier step just found) are no help: before, they
            // counted as new and could make a weak pair look solved
            let pair_cps: Vec<ControlPoint> = self.cps.iter().filter(|c| (c.a, c.b) == (o.a, o.b) || (c.a, c.b) == (o.b, o.a)).copied().chain(add.iter().copied()).collect();
            let (pts, dup) = crate::duplicate_points::new_spots(&pair_cps, pts, &crate::duplicate_points::DupConfig::default());
            if dup > 0 {
                note.push_str(&format!(", {dup} already there"));
                eprintln!("pair check {}-{} {name}: {dup} points already there, {} new", o.a, o.b, pts.len());
            }
            have += pts.len();
            add.extend(pts);
            rep.steps.push(StepLog { step: name, points: have, note });
            if have >= self.cfg.min_points {
                rep.success = Some(name);
            }
        }
        rep.added = add.len();
        rep.after = have;
        rep.weak = have < self.cfg.min_points;
        (rep, add)
    }
}

/// Check all overlapping pairs (or only `only`) with the current geometry. Returns the new
/// control points, one report per pair that was searched, and all overlaps.
#[allow(clippy::too_many_arguments)]
pub fn check_pairs(
    photos: &[FaPhoto],
    lens_of: &[usize],
    lenses: &[Lens],
    rots: &[M3],
    cps: &[ControlPoint],
    used: &[bool],
    only: Option<(usize, usize)>,
    cfg: &PairCheckConfig,
    matcher: &MatchConfig,
    settings: MatchSettings,
    report: &(dyn Fn(f32, &str) + Sync),
) -> (Vec<ControlPoint>, Vec<PairReport>, Vec<PairOverlap>) {
    let g = geos(photos, lens_of, lenses, rots);
    // geometry is only meaningful inside the group of photos joined by points
    let comp = align::components(photos.len(), cps);
    let counts = pair_counts(cps);
    let has_pts: Vec<bool> = (0..photos.len()).map(|i| used[i] && cps.iter().any(|c| c.a == i || c.b == i)).collect();
    let ovs = overlaps(&g, &has_pts, cfg.overlap_grid, cfg.min_overlap);
    let todo: Vec<PairOverlap> = ovs
        .iter()
        .filter(|o| comp[o.a] == comp[o.b])
        .filter(|o| only.map(|p| (p.0.min(p.1), p.0.max(p.1)) == (o.a, o.b)).unwrap_or(true))
        .filter(|o| counts.get(&(o.a, o.b)).copied().unwrap_or(0) < cfg.min_points)
        .cloned()
        .collect();
    let ctx = Ctx { photos, geos: &g, lens_of, lenses, cps, counts: &counts, used: &has_pts, cfg, matcher, settings, skies: Mutex::new(HashMap::new()) };
    let total = todo.len().max(1);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let res: Vec<(PairReport, Vec<ControlPoint>)> = todo
        .par_iter()
        .map(|o| {
            let r = ctx.check(o);
            let k = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            report(k as f32 / total as f32, &format!("Checking overlapping pairs without enough points ({k} of {total})"));
            r
        })
        .collect();
    let mut added = Vec::new();
    let mut reports = Vec::new();
    for (r, c) in res {
        reports.push(r);
        added.extend(c);
    }
    (added, reports, ovs)
}

/// Overlaps only (for the connection view when the check is switched off).
pub fn summary_only(photos: &[FaPhoto], lens_of: &[usize], lenses: &[Lens], rots: &[M3], cps: &[ControlPoint], used: &[bool], cfg: &PairCheckConfig) -> PairSummary {
    let g = geos(photos, lens_of, lenses, rots);
    let has_pts: Vec<bool> = (0..photos.len()).map(|i| used[i] && cps.iter().any(|c| c.a == i || c.b == i)).collect();
    let n = pair_counts(cps).len();
    PairSummary { overlaps: overlaps(&g, &has_pts, cfg.overlap_grid, cfg.min_overlap), pairs_before: n, pairs_after: n, ..Default::default() }
}

/// The check in rounds: check, add the points, align again with the better geometry, repeat
/// (at most `max_rounds`, stops as soon as a round adds nothing).
#[allow(clippy::too_many_arguments)]
pub fn run_rounds(
    photos: &[FaPhoto],
    lens_of: &[usize],
    lenses_start: &[Lens],
    long: &[f64],
    first: engine::AlignResult,
    s: &engine::AlignSettings,
    used: &[bool],
    cfg: &PairCheckConfig,
    matcher: &MatchConfig,
    settings: MatchSettings,
    report: &(dyn Fn(f32, &str) + Sync),
) -> (engine::AlignResult, PairSummary) {
    let mut r = first;
    let mut sum = PairSummary { ran: true, pairs_before: pair_counts(&r.cps).len(), ..Default::default() };
    let mut merged: Vec<PairReport> = Vec::new();
    for round in 0..cfg.max_rounds.max(1) {
        if engine::cancelled() {
            sum.log.push("cancelled".into());
            break;
        }
        let (add, reps, ovs) = check_pairs(photos, lens_of, &r.lenses, &r.rots, &r.cps, used, None, cfg, matcher, settings, report);
        sum.rounds = round + 1;
        sum.log.push(format!("round {}: {} overlapping pairs, {} with fewer than {} points, {} new points", round + 1, ovs.len(), reps.len(), cfg.min_points, add.len()));
        for l in report_lines(&reps) {
            sum.log.push(format!("  {l}"));
        }
        for rep in reps {
            match merged.iter_mut().find(|m| m.a == rep.a && m.b == rep.b) {
                Some(m) => {
                    m.steps.extend(rep.steps);
                    m.added += rep.added;
                    m.success = m.success.or(rep.success);
                }
                None => merged.push(rep),
            }
        }
        if add.is_empty() {
            sum.log.push(format!("round {}: nothing new - done", round + 1));
            break;
        }
        let mut all = r.cps.clone();
        all.extend(add);
        report(0.0, &format!("Aligning again with the new points (round {})", round + 1));
        let removed = r.removed;
        let mut r2 = engine::align_all(lens_of, lenses_start, long, &all, s, report);
        r2.removed += removed;
        r2.removal_passes += r.removal_passes;
        r2.ms += r.ms;
        r = r2;
    }
    let counts = pair_counts(&r.cps);
    for m in merged.iter_mut() {
        m.after = counts.get(&(m.a, m.b)).copied().unwrap_or(0);
        m.weak = m.after < cfg.min_points;
        if m.weak {
            m.success = None;
            sum.log.push(format!("photos {}–{}: still weak ({} points) – reported, no points made up", m.a + 1, m.b + 1, m.after));
        }
    }
    let g = geos(photos, lens_of, &r.lenses, &r.rots);
    let has_pts: Vec<bool> = (0..photos.len()).map(|i| used[i] && r.cps.iter().any(|c| c.a == i || c.b == i)).collect();
    sum.overlaps = overlaps(&g, &has_pts, cfg.overlap_grid, cfg.min_overlap);
    sum.pairs_after = counts.len();
    merged.sort_by_key(|m| (m.a, m.b));
    sum.reports = merged;
    (r, sum)
}

/// Text lines for the log (one per pair).
pub fn report_lines(reports: &[PairReport]) -> Vec<String> {
    reports
        .iter()
        .map(|r| {
            let steps: Vec<String> = r.steps.iter().map(|s| format!("{} → {} points ({})", s.step, s.points, s.note)).collect();
            format!(
                "photos {}–{}: overlap {:.0} %, {} points before; {}; {}",
                r.a + 1,
                r.b + 1,
                r.overlap * 100.0,
                r.before,
                if steps.is_empty() { "no step switched on".to_string() } else { steps.join("; ") },
                match (r.success, r.weak) {
                    (Some(s), _) => format!("solved by: {s}"),
                    (None, true) => "still weak".to_string(),
                    _ => String::new(),
                }
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(yaw_deg: f64) -> Geo {
        Geo { lens: Lens::from_hfov_deg(60.0), rot: rot_y(yaw_deg.to_radians()), w: 1600.0, h: 1067.0 }
    }

    #[test]
    fn overlap_of_shifted_photos() {
        // 60° wide photos 30° apart overlap by about half
        let (a, b) = (geo(0.0), geo(30.0));
        let o = estimate_overlap(0, 1, &a, &b, 24).unwrap();
        assert!(o.share > 0.4 && o.share < 0.6, "share {}", o.share);
        assert!(estimate_overlap(0, 1, &a, &geo(90.0), 24).is_none());
        // the sweep finds the same pairs as testing all of them
        let gs: Vec<Geo> = (0..12).map(|i| geo(i as f64 * 25.0)).collect();
        let fast = overlaps(&gs, &[true; 12], 16, 0.05);
        let mut all = 0;
        for i in 0..12 {
            for j in i + 1..12 {
                if estimate_overlap(i, j, &gs[i], &gs[j], 16).map(|o| o.share >= 0.05).unwrap_or(false) {
                    all += 1;
                }
            }
        }
        assert_eq!(fast.len(), all);
    }

    #[test]
    fn chained_rotation() {
        let r = rot_y(0.3);
        let pairs: Vec<(V3, V3)> = [[0.1, 0.2, 1.0], [-0.3, 0.1, 1.0], [0.2, -0.25, 1.0], [0.0, 0.0, 1.0]]
            .iter()
            .map(|v| {
                let v = normalize(*v);
                (v, mv(&r, v))
            })
            .collect();
        let e = rotation_between(&pairs).unwrap();
        for i in 0..3 {
            for j in 0..3 {
                assert!((e[i][j] - r[i][j]).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn outliers_against_prediction() {
        let mut d: Vec<(f64, f64)> = (0..20).map(|i| (0.3 * (i % 3) as f64, -0.2 * (i % 2) as f64)).collect();
        d.push((15.0, -9.0));
        let k = consistent(&d, 3.0, 1.0);
        assert!(!k[20] && k[..20].iter().all(|x| *x));
    }
}
