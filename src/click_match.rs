//! Patch click mode: the person clicks roughly the same spot in both photos; the local zoom and
//! rotation between the photos is estimated (from existing points, the alignment, or the lenses
//! plus a rotation search), the right spot is refined by patch matching (pyramid, sub-pixel),
//! and more patch pairs are searched around it with the same local transform. Everything comes
//! back as suggestions; nothing is added until the person accepts it. Runs in a worker thread.

use crate::guided::{self, P2};
use crate::imgutil::{self, Gray};
use crate::pair_check::consistent;
use crate::patch_matcher::{self, MatchConfig, Region};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Without points or alignment the zoom comes from the lenses and the rotation from a coarse
/// search; both are then refined on this grid (zoom ± 2 × 6 %, rotation ± 5 × 1.5°).
const REFINE_ZOOM_STEPS: i32 = 2;
const REFINE_ZOOM_STEP: f64 = 0.06;
const REFINE_ROT_STEPS: i32 = 5;
const REFINE_ROT_STEP_DEG: f64 = 1.5;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ClickConfig {
    /// patch around the clicked spot (pixels of the left photo's working image)
    pub patch_size: usize,
    /// how far the right click may be off (pixels, left photo's scale)
    pub search_radius: f32,
    pub min_score: f32,
    pub pyramid_levels: usize,
    /// search more patch pairs around the click
    pub more: bool,
    /// area around the click searched for more pairs (radius, pixels of the left photo)
    pub more_radius: f64,
    pub more_step: f64,
    pub more_patch: usize,
    pub more_search: f32,
    pub more_min_score: f32,
    pub more_max: usize,
    /// weight of the clicked (refined) point, times its confidence (like hand-drawn areas)
    pub manual_weight: f64,
    pub outlier_sigma: f64,
    pub sigma_floor_px: f64,
}

impl Default for ClickConfig {
    fn default() -> Self {
        ClickConfig {
            patch_size: 41,
            search_radius: 40.0,
            min_score: 0.7,
            pyramid_levels: 3,
            more: true,
            more_radius: 200.0,
            more_step: 40.0,
            more_patch: 31,
            more_search: 6.0,
            more_min_score: 0.8,
            more_max: 80,
            manual_weight: 2.0,
            outlier_sigma: 3.0,
            sigma_floor_px: 1.0,
        }
    }
}

/// One proposed point pair (working-image pixels of both photos).
#[derive(Clone, Debug)]
pub struct Suggestion {
    pub pa: P2,
    pub pb: P2,
    pub score: f32,
    pub conf: f32,
    pub accept: bool,
    /// the refined clicked point
    pub main: bool,
}

#[derive(Clone, Debug)]
pub struct ClickResult {
    pub a: usize,
    pub b: usize,
    /// local transform left -> right (pixels): [dx/dx, dx/dy, dy/dx, dy/dy]
    pub j: [f64; 4],
    pub j_source: String,
    pub main: Option<Suggestion>,
    pub error: Option<String>,
    pub more: Vec<Suggestion>,
    pub log: Vec<String>,
}

impl ClickResult {
    pub fn zoom(&self) -> f64 {
        (self.j[0] * self.j[3] - self.j[1] * self.j[2]).abs().sqrt()
    }
    pub fn rotation_deg(&self) -> f64 {
        self.j[2].atan2(self.j[0]).to_degrees()
    }
}

/// Similarity (zoom + rotation) from point pairs, least squares: z_b = alpha z_a + beta.
pub fn similarity_from(seeds: &[(P2, P2)]) -> Option<[f64; 4]> {
    if seeds.len() < 2 {
        return None;
    }
    let n = seeds.len() as f64;
    let (ma, mb) = seeds.iter().fold(((0.0, 0.0), (0.0, 0.0)), |acc, (a, b)| ((acc.0.0 + a.0 / n, acc.0.1 + a.1 / n), (acc.1.0 + b.0 / n, acc.1.1 + b.1 / n)));
    let (mut num_re, mut num_im, mut den) = (0.0, 0.0, 0.0);
    for (a, b) in seeds {
        let (ax, ay) = (a.0 - ma.0, a.1 - ma.1);
        let (bx, by) = (b.0 - mb.0, b.1 - mb.1);
        // conj(za) * zb
        num_re += ax * bx + ay * by;
        num_im += ax * by - ay * bx;
        den += ax * ax + ay * ay;
    }
    if den < 1e-6 {
        return None;
    }
    let (re, im) = (num_re / den, num_im / den);
    Some([re, -im, im, re])
}

/// Sample a square region around `c` of image `g` through the 2x2 transform `j`
/// (region pixel offsets -> image pixel offsets). Pixels outside the image are invalid.
fn sample_region(g: &Gray, c: P2, j: [f64; 4], size: usize) -> Region {
    let mut r = Region::new(size);
    let h = (size as f64 - 1.0) * 0.5;
    for y in 0..size {
        for x in 0..size {
            let (u, v) = (x as f64 - h, y as f64 - h);
            let px = c.0 + j[0] * u + j[1] * v;
            let py = c.1 + j[2] * u + j[3] * v;
            if px < 0.0 || py < 0.0 || px > g.w as f64 - 1.0 || py > g.h as f64 - 1.0 {
                continue;
            }
            let i = y * size + x;
            r.data[i] = g.sample(px as f32, py as f32);
            r.mask[i] = true;
        }
    }
    r
}

fn reason_text(r: &str) -> String {
    match r {
        "too little texture" => "too little texture at the clicked spot (plain surface) – click a corner or a sharp detail".into(),
        "ambiguous (repeating pattern)" => "the spot is ambiguous (repeating pattern) – click something unique".into(),
        "similarity too low" => "the two spots do not look alike – click the same spot in both photos".into(),
        "best match at the edge of the search area" => "the right click is too far off – click closer to the same spot".into(),
        "patch outside the photo" => "the spot is too close to the edge of the photo".into(),
        "forward-backward check failed" => "the match is not stable (forward-backward check) – try another spot".into(),
        other => other.to_string(),
    }
}

/// Run the click mode for one pair of clicks.
#[allow(clippy::too_many_arguments)]
pub fn run(a: usize, b: usize, ga: &Gray, gb: &Gray, click_a: P2, click_b: P2, hint: Option<([f64; 4], String)>, scale_prior: f64, cfg: &ClickConfig) -> ClickResult {
    let mut res = ClickResult { a, b, j: [1.0, 0.0, 0.0, 1.0], j_source: String::new(), main: None, error: None, more: Vec::new(), log: Vec::new() };
    // local zoom / rotation
    let s0 = scale_prior.max(1e-3);
    let fs0 = (s0 < 1.0 / 1.5).then(|| imgutil::blur(ga, guided::soft_sigma(1.0 / s0)));
    let ms0 = (s0 > 1.5).then(|| imgutil::blur(gb, guided::soft_sigma(s0)));
    let searched = hint.is_none();
    let (mut j, src) = match hint {
        Some(h) => h,
        None => {
            let c = guided::Corr { fixed: ga, fixed_soft: fs0.as_ref(), moving: gb, moving_soft: ms0.as_ref(), tpl_r: guided::TPL_R };
            let (j, v) = guided::best_rotation(&c, click_a, s0, click_b);
            (j, format!("zoom from the lenses, rotation searched (correlation {v:.2})"))
        }
    };
    let s = (j[0] * j[3] - j[1] * j[2]).abs().sqrt();
    // anti-aliasing: the finer photo is smoothed to the coarser one's pixel size
    let soft_a = (s < 1.0 / 1.5).then(|| imgutil::blur(ga, guided::soft_sigma(1.0 / s)));
    let soft_b = (s > 1.5).then(|| imgutil::blur(gb, guided::soft_sigma(s)));
    let (sa, sb) = (soft_a.as_ref().unwrap_or(ga), soft_b.as_ref().unwrap_or(gb));
    let id = [1.0, 0.0, 0.0, 1.0];
    let mcfg = MatchConfig { patch_size: cfg.patch_size, search_radius: cfg.search_radius, min_score: cfg.min_score, pyramid_levels: cfg.pyramid_levels, ..MatchConfig::default() };
    let size = Region::size_for(&mcfg);
    let ra = sample_region(sa, click_a, id, size);
    let try_j = |j: [f64; 4]| patch_matcher::match_patch(&ra, &sample_region(sb, click_b, j, size), &mcfg);
    let mut m = try_j(j);
    if searched {
        // the coarse rotation search has steps of 15°: refine zoom and rotation around it
        let mut cands: Vec<[f64; 4]> = Vec::new();
        // around the searched rotation and around "upright" (photos of a panorama are usually
        // held the same way; the coarse search can confuse symmetric details by 180°)
        for base in [j, [s0, 0.0, 0.0, s0]] {
        let j = base;
        for kz in -REFINE_ZOOM_STEPS..=REFINE_ZOOM_STEPS {
            for kr in -REFINE_ROT_STEPS..=REFINE_ROT_STEPS {
                let z = 1.0 + kz as f64 * REFINE_ZOOM_STEP;
                let (c, sn) = ((kr as f64 * REFINE_ROT_STEP_DEG).to_radians().cos(), (kr as f64 * REFINE_ROT_STEP_DEG).to_radians().sin());
                // (z R(d)) * j
                let r = [z * c, -z * sn, z * sn, z * c];
                cands.push([r[0] * j[0] + r[1] * j[2], r[0] * j[1] + r[1] * j[3], r[2] * j[0] + r[3] * j[2], r[2] * j[1] + r[3] * j[3]]);
            }
        }
        }
        let best = cands.par_iter().map(|&cj| (cj, try_j(cj))).max_by(|x, y| (x.1.valid, x.1.score).partial_cmp(&(y.1.valid, y.1.score)).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((bj, bm)) = best {
            if (bm.valid, bm.score) > (m.valid, m.score) {
                j = bj;
                m = bm;
            }
        }
    }
    res.j = j;
    res.j_source = src;
    res.log.push(format!("local transform: zoom ×{:.3}, rotation {:.1}° ({})", res.zoom(), res.rotation_deg(), res.j_source));
    if !m.valid {
        res.error = Some(format!("No match: {} (score {:.2}). The clicks stay – click again.", reason_text(m.reason), m.score));
        res.log.push(format!("clicked spot rejected: {}", m.reason));
        return res;
    }
    let off = (j[0] * m.dx as f64 + j[1] * m.dy as f64, j[2] * m.dx as f64 + j[3] * m.dy as f64);
    let pos_b = (click_b.0 + off.0, click_b.1 + off.1);
    res.log.push(format!("refined: right spot moved by ({:.2}, {:.2}) px, score {:.3}, confidence {:.3}", off.0, off.1, m.score, m.confidence));
    res.main = Some(Suggestion { pa: click_a, pb: pos_b, score: m.score, conf: m.confidence, accept: true, main: true });
    if !cfg.more {
        return res;
    }
    // more pairs around the click, with the same local transform
    let step = cfg.more_step.max(4.0);
    let r = cfg.more_radius.max(step);
    let k = (r / step).floor() as i64;
    let mut offs: Vec<P2> = Vec::new();
    for gy in -k..=k {
        for gx in -k..=k {
            let o = (gx as f64 * step, gy as f64 * step);
            if (gx, gy) != (0, 0) && o.0.hypot(o.1) <= r {
                offs.push(o);
            }
        }
    }
    offs.sort_by(|x, y| x.0.hypot(x.1).partial_cmp(&y.0.hypot(y.1)).unwrap_or(std::cmp::Ordering::Equal));
    offs.truncate(cfg.more_max);
    let pcfg = MatchConfig { patch_size: cfg.more_patch, search_radius: cfg.more_search, min_score: cfg.more_min_score, pyramid_levels: cfg.pyramid_levels, ..MatchConfig::default() };
    let psize = Region::size_for(&pcfg);
    let found: Vec<Result<(Suggestion, (f64, f64)), &'static str>> = offs
        .par_iter()
        .map(|&o| {
            let pa = (click_a.0 + o.0, click_a.1 + o.1);
            let pred = (pos_b.0 + j[0] * o.0 + j[1] * o.1, pos_b.1 + j[2] * o.0 + j[3] * o.1);
            let ra = sample_region(sa, pa, id, psize);
            let rb = sample_region(sb, pred, j, psize);
            let m = patch_matcher::match_patch(&ra, &rb, &pcfg);
            if !m.valid {
                return Err(m.reason);
            }
            let d = (j[0] * m.dx as f64 + j[1] * m.dy as f64, j[2] * m.dx as f64 + j[3] * m.dy as f64);
            Ok((Suggestion { pa, pb: (pred.0 + d.0, pred.1 + d.1), score: m.score, conf: m.confidence, accept: true, main: false }, d))
        })
        .collect();
    let mut reasons: HashMap<&str, usize> = HashMap::new();
    let mut ok: Vec<(Suggestion, (f64, f64))> = Vec::new();
    for f in found {
        match f {
            Ok(x) => ok.push(x),
            Err(e) => *reasons.entry(e).or_insert(0) += 1,
        }
    }
    let keep = consistent(&ok.iter().map(|x| x.1).collect::<Vec<_>>(), cfg.outlier_sigma, cfg.sigma_floor_px);
    let outliers = keep.iter().filter(|k| !**k).count();
    res.more = ok.into_iter().zip(keep).filter(|(_, k)| *k).map(|(x, _)| x.0).collect();
    let mut rs: Vec<String> = reasons.iter().map(|(k, v)| format!("{v} {k}")).collect();
    rs.sort();
    res.log.push(format!(
        "around the click: {} patches, {} found, {} outliers against the local transform{}",
        offs.len(),
        res.more.len(),
        outliers,
        if rs.is_empty() { String::new() } else { format!(", rejected: {}", rs.join(", ")) }
    ));
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(x: f64, y: f64) -> f32 {
        let mut v = 0.0;
        for (fx, fy, ph) in [(0.11, 0.07, 1.3), (0.05, 0.13, 0.4), (0.23, 0.19, 2.1), (0.17, 0.04, 0.9), (0.031, 0.027, 2.7)] {
            v += ((x * fx + y * fy) * 1.7 + ph).sin() * ((x * fy - y * fx) * 1.3 + ph * 0.5).cos();
        }
        (0.5 + 0.08 * v) as f32
    }

    /// photo of the scene: pixel p shows scene point zoom * R(rot) * p + t
    fn photo(zoom: f64, rot: f64, t: P2, w: usize) -> Gray {
        let mut g = Gray::new(w, w);
        let (c, s) = (rot.cos(), rot.sin());
        for y in 0..w {
            for x in 0..w {
                let (u, v) = (x as f64, y as f64);
                g.data[y * w + x] = scene(zoom * (c * u - s * v) + t.0, zoom * (s * u + c * v) + t.1);
            }
        }
        g
    }

    #[test]
    fn works_with_different_zoom_and_rotation() {
        // B is zoomed in 1.6x and turned by 8 degrees against A
        let ga = photo(1.0, 0.0, (0.0, 0.0), 600);
        let (z, rot) = (1.0 / 1.6, 8f64.to_radians());
        let gb = photo(z, rot, (40.0, 25.0), 900);
        // true position in B of A's pixel p: solve z R q + t = p
        let truth = |p: P2| -> P2 {
            let (x, y) = (p.0 - 40.0, p.1 - 25.0);
            let (c, s) = (rot.cos(), rot.sin());
            ((c * x + s * y) / z, (-s * x + c * y) / z)
        };
        let ca = (300.0, 280.0);
        let tb = truth(ca);
        let rough = (tb.0 + 9.0, tb.1 - 7.0);
        let r = run(0, 1, &ga, &gb, ca, rough, None, 1.6, &ClickConfig::default());
        let (c8, s8) = (rot.cos(), rot.sin());
        let jt = [c8 / z, s8 / z, -s8 / z, c8 / z];
        let rt = run(0, 1, &ga, &gb, ca, rough, Some((jt, "truth".into())), 1.6, &ClickConfig::default());
        assert!(rt.main.is_some(), "with the true transform: {:?}", rt.error);
        let m = r.main.as_ref().unwrap_or_else(|| panic!("{:?} {:?}", r.error, r.log));
        assert!((m.pb.0 - tb.0).hypot(m.pb.1 - tb.1) < 0.6, "main off: {:?} vs {:?}", m.pb, tb);
        assert!((r.zoom() - 1.6).abs() < 0.1, "zoom {}", r.zoom());
        assert!(r.more.len() >= 10, "{} more", r.more.len());
        for s in &r.more {
            let t = truth(s.pa);
            assert!((s.pb.0 - t.0).hypot(s.pb.1 - t.1) < 1.0);
        }
    }

    #[test]
    fn plain_surface_gives_a_clear_message() {
        let mut ga = Gray::new(300, 300);
        ga.data.iter_mut().for_each(|v| *v = 0.5);
        let gb = ga.clone();
        let r = run(0, 1, &ga, &gb, (150.0, 150.0), (150.0, 150.0), Some(([1.0, 0.0, 0.0, 1.0], "test".into())), 1.0, &ClickConfig::default());
        assert!(r.main.is_none());
        assert!(r.error.unwrap().contains("too little texture"));
    }
}
