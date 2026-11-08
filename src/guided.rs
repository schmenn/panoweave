//! Point finding for one photo pair: fine-tuning single points to sub-pixel accuracy,
//! growing many points from a few hand-placed ones, and fully automatic matching of a pair
//! even when the two photos were taken at very different zoom levels.

use crate::features::{self, Features};
use crate::geometry::{fit_homography, h_apply, inv33, solve};
use crate::imgutil::{blur, resize_gray, Gray};
use image::RgbImage;
use rayon::prelude::*;

pub type P2 = (f64, f64);

/// Correlation template size, from the original: default 21 px.
pub const TEMPLATE_SIZE: i32 = 21;
pub const TPL_R: i32 = TEMPLATE_SIZE / 2;
/// Search area for placing a point: 10 % of the photo
/// width by default, at most 500 px.
pub const SEARCH_AREA_PERCENT: f64 = 10.0;
pub const SEARCH_AREA_MAX_PX: f64 = 500.0;
/// Local search when snapping a moved point: default width 14, window
/// = local search width + template size.
pub const LOCAL_SEARCH_WIDTH: f64 = 14.0;
/// Minimum contrast (standard deviation of grey 0..1) inside the match window.
pub const MIN_PATCH_STD: f32 = 0.02;
/// How much better the best match must be than any separate second match.
pub const MIN_PEAK_MARGIN: f32 = 0.05;
/// Correlation needed to accept a grown point.
pub const GROW_MIN_NCC: f32 = 0.8;
/// Correlation needed to accept a fine-tuned point: default 0.8.
pub const FINETUNE_MIN_NCC: f32 = 0.8;

/// Search radius (photo pixels) for a search window of `window` pixels, as the original
/// centres the template inside the window.
pub fn window_radius(window: f64) -> f64 {
    ((window - TEMPLATE_SIZE as f64) / 2.0).max(1.0)
}

/// Search window when placing a point in a photo `width` pixels wide (full size).
pub fn placing_window(width: f64, percent: f64) -> f64 {
    (width * percent / 100.0).min(SEARCH_AREA_MAX_PX)
}

fn sane(h: &[f64; 9], w: f64) -> bool {
    let det = h[0] * h[4] - h[1] * h[3];
    det.is_finite() && det > 1e-3 && det < 1e3 && (h[6] * w).abs() < 0.6 && (h[7] * w).abs() < 0.6
}

/// Similarity transform (scale, rotation, shift) from >= 2 pairs, as a 3x3 matrix.
fn similarity(pairs: &[(P2, P2)]) -> Option<[f64; 9]> {
    let mut ata = vec![vec![0.0; 4]; 4];
    let mut atb = vec![0.0; 4];
    for &((x, y), (u, v)) in pairs {
        for (row, rhs) in [([x, -y, 1.0, 0.0], u), ([y, x, 0.0, 1.0], v)] {
            for i in 0..4 {
                atb[i] += row[i] * rhs;
                for j in 0..4 {
                    ata[i][j] += row[i] * row[j];
                }
            }
        }
    }
    let s = solve(ata, atb)?;
    Some([s[0], -s[1], s[2], s[1], s[0], s[3], 0.0, 0.0, 1.0])
}

/// Transform A -> B (work pixels) from seed pairs. With one pair the zoom ratio of the lenses is used.
pub fn estimate(seeds: &[(P2, P2)], scale_prior: f64, w: f64) -> Option<[f64; 9]> {
    match seeds.len() {
        0 => None,
        1 => {
            let ((x, y), (u, v)) = seeds[0];
            let s = scale_prior;
            Some([s, 0.0, u - s * x, 0.0, s, v - s * y, 0.0, 0.0, 1.0])
        }
        2..=5 => similarity(seeds),
        _ => {
            let (src, dst): (Vec<P2>, Vec<P2>) = seeds.iter().copied().unzip();
            fit_homography(&src, &dst).filter(|h| sane(h, w)).or_else(|| similarity(seeds))
        }
    }
}

/// Local linear part (2x2, row-major) of a 3x3 transform at a point.
pub fn jacobian(h: &[f64; 9], p: P2) -> [f64; 4] {
    let e = 0.5;
    let c = h_apply(h, p.0, p.1);
    let x = h_apply(h, p.0 + e, p.1);
    let y = h_apply(h, p.0, p.1 + e);
    [(x.0 - c.0) / e, (y.0 - c.0) / e, (x.1 - c.1) / e, (y.1 - c.1) / e]
}

/// Images for correlation, with smoothed copies for sampling at coarser steps.
pub struct Corr<'a> {
    pub fixed: &'a Gray,
    pub fixed_soft: Option<&'a Gray>,
    pub moving: &'a Gray,
    pub moving_soft: Option<&'a Gray>,
    /// half size of the matching window in steps (window = 2 * tpl_r + 1)
    pub tpl_r: i32,
}

pub fn step_sizes(j: &[f64; 4]) -> (f64, f64) {
    let s = (j[0] * j[3] - j[1] * j[2]).abs().sqrt().max(1e-6);
    // spacing in the fixed photo, so that one step is >= 1 pixel in both photos
    let sf = (1.0 / s).max(1.0);
    (sf, s * sf)
}

/// How much smoothing to use for sampling with a given step.
pub fn soft_sigma(step: f64) -> f32 {
    (0.5 * step) as f32
}

/// Find the spot in the moving photo matching `pf` in the fixed photo, near `guess`.
/// `j` maps small shifts in the fixed photo to shifts in the moving photo (handles zoom and tilt).
/// Returns (x, y, correlation) with sub-pixel accuracy.
pub fn refine(c: &Corr, pf: P2, j: [f64; 4], guess: P2, search: f64) -> Option<(f64, f64, f32)> {
    let (sf, sm) = step_sizes(&j);
    let fimg = if sf > 1.5 { c.fixed_soft.unwrap_or(c.fixed) } else { c.fixed };
    let mimg = if sm > 1.5 { c.moving_soft.unwrap_or(c.moving) } else { c.moving };
    let tr = c.tpl_r.clamp(2, 60);
    let n = ((2 * tr + 1) * (2 * tr + 1)) as usize;
    let mut tpl = Vec::with_capacity(n);
    let mut offs = Vec::with_capacity(n);
    for dy in -tr..=tr {
        for dx in -tr..=tr {
            let fx = dx as f64 * sf;
            let fy = dy as f64 * sf;
            let x = pf.0 + fx;
            let y = pf.1 + fy;
            if x < 0.0 || y < 0.0 || x > (fimg.w - 1) as f64 || y > (fimg.h - 1) as f64 {
                return None;
            }
            tpl.push(fimg.sample(x as f32, y as f32));
            offs.push((j[0] * fx + j[1] * fy, j[2] * fx + j[3] * fy));
        }
    }
    let mt = tpl.iter().sum::<f32>() / n as f32;
    let vt: f32 = tpl.iter().map(|v| (v - mt) * (v - mt)).sum();
    // flat area (wall, sky, plain surface): nothing reliable to lock onto
    if vt / (n as f32) < MIN_PATCH_STD * MIN_PATCH_STD {
        return None;
    }
    let ncc = |cx: f64, cy: f64| -> f32 {
        let mut buf = vec![0.0f32; n];
        let mut sum = 0.0;
        for (k, o) in offs.iter().enumerate() {
            let v = mimg.sample((cx + o.0) as f32, (cy + o.1) as f32);
            buf[k] = v;
            sum += v;
        }
        let m = sum / n as f32;
        let mut cov = 0.0;
        let mut vm = 0.0;
        for k in 0..n {
            cov += (tpl[k] - mt) * (buf[k] - m);
            vm += (buf[k] - m) * (buf[k] - m);
        }
        if vm < 1e-6 { -1.0 } else { cov / (vt * vm).sqrt() }
    };
    let inside = |x: f64, y: f64| x >= 0.0 && y >= 0.0 && x <= (mimg.w - 1) as f64 && y <= (mimg.h - 1) as f64;
    // coarse grid, then 1 px, then sub-pixel
    let coarse = if search > 6.0 { 2.0 } else { 1.0 };
    let steps = (search / coarse).ceil() as i32;
    let mut best = (-2.0f32, guess.0, guess.1);
    let mut all: Vec<(f32, f64, f64)> = Vec::new();
    for sy in -steps..=steps {
        for sx in -steps..=steps {
            let x = guess.0 + sx as f64 * coarse;
            let y = guess.1 + sy as f64 * coarse;
            if !inside(x, y) {
                continue;
            }
            let v = ncc(x, y);
            all.push((v, x, y));
            if v > best.0 {
                best = (v, x, y);
            }
        }
    }
    // the best match must stand out: a second, separate peak almost as good means the spot is
    // ambiguous (plain surface, repeated pattern) and would pull the alignment off
    if search > 4.0 && best.0 > -1.0 {
        let (sf2, sm2) = step_sizes(&j);
        let _ = sf2;
        let apart = (tr as f64 * sm2 * 0.6).max(3.0);
        let second = all
            .iter()
            .filter(|c| (c.1 - best.1).hypot(c.2 - best.2) > apart)
            .map(|c| c.0)
            .fold(-2.0f32, f32::max);
        if second > best.0 - MIN_PEAK_MARGIN {
            return None;
        }
    }
    if coarse > 1.0 {
        let (_, bx, by) = best;
        for sy in -1..=1 {
            for sx in -1..=1 {
                let x = bx + sx as f64;
                let y = by + sy as f64;
                if inside(x, y) {
                    let v = ncc(x, y);
                    if v > best.0 {
                        best = (v, x, y);
                    }
                }
            }
        }
    }
    let (s, bx, by) = best;
    if s < -1.0 {
        return None;
    }
    // sub-pixel: parabola through neighbours, twice for a finer fit
    let mut x = bx;
    let mut y = by;
    let mut s0 = s;
    for h in [1.0, 0.5] {
        let l = ncc(x - h, y);
        let r = ncc(x + h, y);
        let u = ncc(x, y - h);
        let d = ncc(x, y + h);
        let den_x = 2.0 * s0 - l - r;
        let den_y = 2.0 * s0 - u - d;
        let ox = if den_x > 1e-6 { (0.5 * (r - l) / den_x).clamp(-0.5, 0.5) as f64 * h } else { 0.0 };
        let oy = if den_y > 1e-6 { (0.5 * (d - u) / den_y).clamp(-0.5, 0.5) as f64 * h } else { 0.0 };
        let nx = x + ox;
        let ny = y + oy;
        let ns = ncc(nx, ny);
        if ns >= s0 {
            x = nx;
            y = ny;
            s0 = ns;
        }
    }
    Some((x, y, s0))
}

/// With only the zoom ratio known, find the rotation between the photos at one point pair by
/// trying angles in 15° steps. Returns the local mapping and its correlation.
pub fn best_rotation(c: &Corr, pf: P2, s: f64, guess: P2) -> ([f64; 4], f32) {
    let mut best = ([s, 0.0, 0.0, s], -2.0f32);
    for k in 0..24 {
        let a = (k as f64 * 15.0).to_radians();
        let (sn, cs) = a.sin_cos();
        let j = [s * cs, -s * sn, s * sn, s * cs];
        if let Some((_, _, v)) = refine(c, pf, j, guess, 2.0)
            && v > best.1 {
                best = (j, v);
            }
    }
    // fine angle around the best
    let base = best.0[2].atan2(best.0[0]);
    for d in [-7.5f64, -5.0, -2.5, 2.5, 5.0, 7.5] {
        let a = base + d.to_radians();
        let (sn, cs) = a.sin_cos();
        let j = [s * cs, -s * sn, s * sn, s * cs];
        if let Some((_, _, v)) = refine(c, pf, j, guess, 2.0)
            && v > best.1 {
                best = (j, v);
            }
    }
    best
}


fn scale_of(h: &[f64; 9], p: P2) -> f64 {
    let j = jacobian(h, p);
    (j[0] * j[3] - j[1] * j[2]).abs().sqrt()
}

/// Spread-out corners of a photo (work pixels), optionally skipping sky and clouds.
fn corners(g: &Gray, rgb: Option<&RgbImage>, n: usize) -> Vec<P2> {
    let mut f: Features = features::detect(g, n);
    if let Some(img) = rgb {
        let mask = features::sky_mask(img, g);
        features::remove_masked(&mut f, &mask);
    }
    f.kps.iter().map(|k| (k.x as f64, k.y as f64)).collect()
}

/// Grow many points from a few seed pairs (A -> B, work pixels).
/// Returns (point in A, point in B, correlation).
pub fn grow(
    ga: &Gray,
    gb: &Gray,
    rgb_a: Option<&RgbImage>,
    rgb_b: Option<&RgbImage>,
    seeds: &[(P2, P2)],
    scale_prior: f64,
    max_points: usize,
) -> Vec<(P2, P2, f32)> {
    let wa = ga.w.max(ga.h) as f64;
    let Some(mut t) = estimate(seeds, scale_prior, wa) else { return vec![] };
    if seeds.len() == 1 {
        // a single point says nothing about rotation: measure it there
        let ((x, y), (u, v)) = seeds[0];
        let s = scale_prior;
        let soft_a = (s < 1.0 / 1.5).then(|| blur(ga, soft_sigma(1.0 / s)));
        let soft_b = (s > 1.5).then(|| blur(gb, soft_sigma(s)));
        let c = Corr { fixed: ga, fixed_soft: soft_a.as_ref(), moving: gb, moving_soft: soft_b.as_ref(), tpl_r: TPL_R };
        let (j, _) = best_rotation(&c, (x, y), s, (u, v));
        t = [j[0], j[1], u - j[0] * x - j[1] * y, j[2], j[3], v - j[2] * x - j[3] * y, 0.0, 0.0, 1.0];
    }
    let cen = seeds.iter().fold((0.0, 0.0), |acc, s| (acc.0 + s.0 .0, acc.1 + s.0 .1));
    let cen = (cen.0 / seeds.len() as f64, cen.1 / seeds.len() as f64);
    let s = scale_of(&t, cen).clamp(0.05, 20.0);
    // work in the photo where the shared content is smaller (coarser); compare against the other
    let flip = s < 1.0;
    let (gf, gm) = if flip { (gb, ga) } else { (ga, gb) };
    let rgb_f = if flip { rgb_b } else { rgb_a };
    let mut t_fm = if flip { match inv33(&t) { Some(i) => i, None => return vec![] } } else { t };
    let seeds_fm: Vec<(P2, P2)> = seeds.iter().map(|&(a, b)| if flip { (b, a) } else { (a, b) }).collect();
    let s_fm = if flip { 1.0 / s } else { s };
    let soft_m = if s_fm > 1.5 { Some(blur(gm, soft_sigma(s_fm))) } else { None };
    let c = Corr { fixed: gf, fixed_soft: None, moving: gm, moving_soft: soft_m.as_ref(), tpl_r: TPL_R };

    let margin = 12.0 * s_fm;
    let in_moving = |p: P2| p.0 > margin && p.1 > margin && p.0 < gm.w as f64 - margin && p.1 < gm.h as f64 - margin;
    let mut cands: Vec<P2> = corners(gf, rgb_f, 4000);
    let mut accepted: Vec<(P2, P2, f32)> = seeds_fm.iter().map(|&(f, m)| (f, m, 1.0)).collect();
    let n_seeds = accepted.len();
    let diag = ((gf.w * gf.w + gf.h * gf.h) as f64).sqrt();
    let mut radius = (0.12 * diag).max(60.0);
    for _round in 0..8 {
        // refresh the model from everything accepted so far
        let pairs: Vec<(P2, P2)> = accepted.iter().map(|a| (a.0, a.1)).collect();
        if pairs.len() >= 2
            && let Some(m) = estimate(&pairs, s_fm, gf.w.max(gf.h) as f64) {
                t_fm = m;
            }
        let search = if accepted.len() < 6 { 14.0 } else { 6.0 } * s_fm.max(1.0);
        let near: Vec<P2>;
        (near, cands) = cands.into_iter().partition(|c| accepted.iter().any(|a| (a.0 .0 - c.0).hypot(a.0 .1 - c.1) < radius));
        let found: Vec<(P2, P2, f32)> = near
            .par_iter()
            .filter_map(|&pf| {
                let guess = h_apply(&t_fm, pf.0, pf.1);
                if !in_moving(guess) {
                    return None;
                }
                let j = jacobian(&t_fm, pf);
                let (x, y, v) = refine(&c, pf, j, guess, search)?;
                (v >= GROW_MIN_NCC).then_some((pf, (x, y), v))
            })
            .collect();
        accepted.extend(found);
        if cands.is_empty() || radius > diag {
            break;
        }
        radius *= 1.8;
    }
    // final check against one consistent model
    let mut grown: Vec<(P2, P2, f32)> = accepted.split_off(n_seeds);
    let thr = (1.0 * s_fm).max(2.0);
    for _ in 0..3 {
        if grown.len() < 8 {
            break;
        }
        let (src, dst): (Vec<P2>, Vec<P2>) = grown.iter().map(|a| (a.0, a.1)).unzip();
        let Some(h) = fit_homography(&src, &dst) else { break };
        let before = grown.len();
        grown.retain(|g| {
            let p = h_apply(&h, g.0 .0, g.0 .1);
            (p.0 - g.1 .0).hypot(p.1 - g.1 .1) < thr
        });
        if grown.len() == before {
            break;
        }
    }
    // keep a well spread selection, best correlation first
    grown.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
    let cell = (diag / 12.0).max(8.0);
    let mut used = std::collections::HashMap::new();
    let mut out = Vec::new();
    let mut rest = Vec::new();
    for g in grown {
        let key = ((g.0 .0 / cell) as i64, (g.0 .1 / cell) as i64);
        let c = used.entry(key).or_insert(0);
        if *c < 2 {
            *c += 1;
            out.push(g);
        } else {
            rest.push(g);
        }
    }
    out.extend(rest);
    out.truncate(max_points);
    if flip {
        out.into_iter().map(|(f, m, v)| (m, f, v)).collect()
    } else {
        out
    }
}

/// Fully automatic matching of one pair. The photo showing the shared content larger is shrunk
/// first (using the lens zoom ratio), so photos taken at different zoom levels still match.
pub fn auto_pair(
    ga: &Gray,
    gb: &Gray,
    rgb_a: Option<&RgbImage>,
    rgb_b: Option<&RgbImage>,
    scale_prior: f64,
    max_points: usize,
) -> Vec<(P2, P2, f32)> {
    let try_scale = |s: f64| -> Option<Vec<(P2, P2)>> {
        // bring both to the same scale
        let (sa, sb) = if s > 1.25 { (1.0, 1.0 / s) } else if s < 0.8 { (s, 1.0) } else { (1.0, 1.0) };
        let ia = if sa < 0.999 { resize_gray(ga, sa as f32) } else { ga.clone() };
        let ib = if sb < 0.999 { resize_gray(gb, sb as f32) } else { gb.clone() };
        let (mut fa, mut fb) = rayon::join(|| features::detect(&ia, 6000), || features::detect(&ib, 6000));
        if let Some(img) = rgb_a {
            let small = if sa < 0.999 { image::imageops::resize(img, ia.w as u32, ia.h as u32, image::imageops::FilterType::Triangle) } else { img.clone() };
            features::remove_masked(&mut fa, &features::sky_mask(&small, &ia));
        }
        if let Some(img) = rgb_b {
            let small = if sb < 0.999 { image::imageops::resize(img, ib.w as u32, ib.h as u32, image::imageops::FilterType::Triangle) } else { img.clone() };
            features::remove_masked(&mut fb, &features::sky_mask(&small, &ib));
        }
        let m = features::match_pair(&fa, &fb, 200, 99)?;
        Some(m.corrs.iter().map(|c| ((c.ax as f64 / sa, c.ay as f64 / sa), (c.bx as f64 / sb, c.by as f64 / sb))).collect())
    };
    let mut seeds = try_scale(scale_prior);
    if seeds.is_none() && (scale_prior - 1.0).abs() > 0.2 {
        seeds = try_scale(1.0);
    }
    let Some(seeds) = seeds else { return vec![] };
    if seeds.len() < 6 {
        return vec![];
    }
    // use the matches as seeds: refines them to sub-pixel and fills the overlap evenly
    let grown = grow(ga, gb, rgb_a, rgb_b, &seeds, scale_prior, max_points);
    if grown.len() >= 8 {
        grown
    } else {
        seeds.into_iter().take(max_points).map(|(a, b)| (a, b, 0.0)).collect()
    }
}
