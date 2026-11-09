//! Point detection & matching.
//!
//! Multi-scale corner detector (minimum-eigenvalue response, sub-pixel refined) with an even
//! spread over the photo, rotation-aware binary descriptors, mutual ratio-test matching,
//! RANSAC geometric check and guided re-matching to recover extra good points.

use crate::geometry::{fit_homography, h_apply, Rng};
use crate::imgutil::{blur, resize_gray, Gray};
use rayon::prelude::*;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug)]
pub struct Keypoint {
    pub x: f32,
    pub y: f32,
    pub score: f32,
    pub scale: f32,
}

#[derive(Clone)]
pub struct Features {
    pub w: usize,
    pub h: usize,
    pub kps: Vec<Keypoint>,
    pub desc: Vec<[u64; 4]>,
}

const PATCH_R: f32 = 13.0;
/// Minimum corner strength (smaller eigenvalue of the gradient structure, grey 0..1).
/// Below this the "corner" is mostly image noise on a plain surface.
pub const MIN_CORNER_RESPONSE: f32 = 1.5e-3;
/// Smaller / larger eigenvalue: below this the spot is an edge (slides along it) rather than a corner.
pub const MIN_CORNER_RATIO: f32 = 0.15;
const MARGIN: usize = 21;
/// Move every keypoint onto the vertex of the edges around it (see `snap_to_corner`). The peak of the
/// blurred corner response lies a little inside the corner (about 0.9 px at full size, more on the
/// coarser levels), so points sat beside the feature instead of on it. `PW_CORNER_SNAP=0` = earlier behaviour.
fn corner_snap() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("PW_CORNER_SNAP").map(|v| v != "0").unwrap_or(true))
}
/// Window of the snapping (radius in level pixels) and the largest move it may make.
const SNAP_RADIUS: i32 = 6;
const SNAP_MAX_MOVE: f32 = 3.0;

/// Least-squares intersection of the edge lines around (x, y): the point p that minimises
/// sum of (g . (q - p))^2 over the window, g being the gradient at pixel q (Gaussian weights).
/// For a corner this is its vertex; for an isolated blob its centre. Falls back to the input when the
/// window has structure in one direction only or the move would be larger than `SNAP_MAX_MOVE`.
pub fn snap_to_corner(img: &Gray, x: f32, y: f32) -> (f32, f32) {
    let (cx, cy) = (x.round() as i32, y.round() as i32);
    let r = SNAP_RADIUS;
    if cx - r - 1 < 0 || cy - r - 1 < 0 || cx + r + 1 >= img.w as i32 || cy + r + 1 >= img.h as i32 {
        return (x, y);
    }
    let sig2 = 2.0f32 * (SNAP_RADIUS as f32 / 2.5).powi(2);
    let (mut a, mut b, mut c) = (0.0f64, 0.0f64, 0.0f64);
    let (mut bx, mut by) = (0.0f64, 0.0f64);
    for dy in -r..=r {
        for dx in -r..=r {
            let (px, py) = ((cx + dx) as usize, (cy + dy) as usize);
            let gx = 0.5 * (img.at(px + 1, py) - img.at(px - 1, py));
            let gy = 0.5 * (img.at(px, py + 1) - img.at(px, py - 1));
            // weight around the detected point, not around the pixel
            let (ex, ey) = (px as f32 - x, py as f32 - y);
            let w = (-(ex * ex + ey * ey) / sig2).exp() as f64;
            let (gxx, gyy, gxy) = (w * (gx * gx) as f64, w * (gy * gy) as f64, w * (gx * gy) as f64);
            a += gxx;
            b += gxy;
            c += gyy;
            let (ox, oy) = (ex as f64, ey as f64);
            bx += gxx * ox + gxy * oy;
            by += gxy * ox + gyy * oy;
        }
    }
    let det = a * c - b * b;
    // both directions must carry structure (det relative to the squared trace)
    if !(det > 0.02 * (a + c) * (a + c) * 0.25) {
        return (x, y);
    }
    let (mx, my) = ((c * bx - b * by) / det, (a * by - b * bx) / det);
    let (mx, my) = (mx as f32, my as f32);
    if mx.hypot(my) > SNAP_MAX_MOVE {
        return (x, y);
    }
    (x + mx, y + my)
}

fn pattern() -> &'static Vec<[f32; 4]> {
    static P: OnceLock<Vec<[f32; 4]>> = OnceLock::new();
    P.get_or_init(|| {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let sigma = 2.0 * PATCH_R as f64 / 5.0;
        let mut v = Vec::with_capacity(256);
        while v.len() < 256 {
            let p: Vec<f32> = (0..4).map(|_| (rng.gauss() * sigma) as f32).collect();
            if p.iter().all(|c| c.abs() <= PATCH_R) && ((p[0] - p[2]).abs() + (p[1] - p[3]).abs()) > 1.5 {
                v.push([p[0], p[1], p[2], p[3]]);
            }
        }
        v
    })
}

/// Detect up to `max_points` well-spread keypoints with descriptors.
pub fn detect(gray: &Gray, max_points: usize) -> Features {
    let levels = 5usize;
    let factor = 1.0f32 / 1.32;
    let scales: Vec<f32> = (0..levels).map(|l| factor.powi(l as i32)).collect();
    let total: f32 = scales.iter().map(|s| s * s).sum();

    let per_level: Vec<(Vec<Keypoint>, Vec<[u64; 4]>)> = scales
        .par_iter()
        .map(|&s| {
            let quota = ((max_points as f32) * s * s / total).round() as usize;
            let img = if s >= 0.999 { gray.clone() } else { resize_gray(gray, s) };
            detect_level(&img, s, quota.max(10), (gray.w, gray.h))
        })
        .collect();

    let mut kps = Vec::new();
    let mut desc = Vec::new();
    for (k, d) in per_level {
        kps.extend(k);
        desc.extend(d);
    }
    Features { w: gray.w, h: gray.h, kps, desc }
}

fn detect_level(img: &Gray, s: f32, quota: usize, full: (usize, usize)) -> (Vec<Keypoint>, Vec<[u64; 4]>) {
    let (w, h) = (img.w, img.h);
    if w < 2 * MARGIN + 4 || h < 2 * MARGIN + 4 {
        return (vec![], vec![]);
    }
    // gradients
    let mut ixx = Gray::new(w, h);
    let mut iyy = Gray::new(w, h);
    let mut ixy = Gray::new(w, h);
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let gx = 0.5 * (img.at(x + 1, y) - img.at(x - 1, y));
            let gy = 0.5 * (img.at(x, y + 1) - img.at(x, y - 1));
            let i = y * w + x;
            ixx.data[i] = gx * gx;
            iyy.data[i] = gy * gy;
            ixy.data[i] = gx * gy;
        }
    }
    let ixx = blur(&ixx, 1.5);
    let iyy = blur(&iyy, 1.5);
    let ixy = blur(&ixy, 1.5);
    let mut resp = Gray::new(w, h);
    let mut maxr = 0.0f32;
    for i in 0..w * h {
        let a = ixx.data[i];
        let c = iyy.data[i];
        let b = ixy.data[i];
        let r = 0.5 * (a + c) - (0.25 * (a - c) * (a - c) + b * b).sqrt();
        resp.data[i] = r;
        maxr = maxr.max(r);
    }
    // weak corners are noise on plain surfaces: keep only clear ones
    let thr = (maxr * 0.0005).max(MIN_CORNER_RESPONSE);

    // non-maximum suppression (5x5) + sub-pixel refinement
    let mut cands: Vec<(f32, f32, f32)> = Vec::new();
    for y in MARGIN..h - MARGIN {
        for x in MARGIN..w - MARGIN {
            let r = resp.at(x, y);
            if r <= thr {
                continue;
            }
            let mut is_max = true;
            'n: for dy in -2isize..=2 {
                for dx in -2isize..=2 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let o = resp.at((x as isize + dx) as usize, (y as isize + dy) as usize);
                    if o > r || (o == r && (dy < 0 || (dy == 0 && dx < 0))) {
                        is_max = false;
                        break 'n;
                    }
                }
            }
            if !is_max {
                continue;
            }
            let l = resp.at(x - 1, y);
            let rr = resp.at(x + 1, y);
            let u = resp.at(x, y - 1);
            let d = resp.at(x, y + 1);
            let denx = 2.0 * r - l - rr;
            let deny = 2.0 * r - u - d;
            let ox = if denx > 1e-12 { (0.5 * (rr - l) / denx).clamp(-0.5, 0.5) } else { 0.0 };
            let oy = if deny > 1e-12 { (0.5 * (d - u) / deny).clamp(-0.5, 0.5) } else { 0.0 };
            // a real corner, not just an edge: both directions must have structure
            let i = y * w + x;
            let (a2, c2, b2) = (ixx.data[i], iyy.data[i], ixy.data[i]);
            let lmax = 0.5 * (a2 + c2) + (0.25 * (a2 - c2) * (a2 - c2) + b2 * b2).sqrt();
            if r < MIN_CORNER_RATIO * lmax {
                continue;
            }
            cands.push((x as f32 + ox, y as f32 + oy, r));
        }
    }
    cands.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());

    // even spread: grid buckets first, then fill with the strongest remaining
    let per_cell = 5usize;
    let ncells = (quota / per_cell).max(1) as f32;
    let cell = ((w * h) as f32 / ncells).sqrt().max(8.0);
    let gw = (w as f32 / cell).ceil() as usize;
    let gh = (h as f32 / cell).ceil() as usize;
    let mut counts = vec![0usize; gw * gh];
    let mut taken = vec![false; cands.len()];
    let mut chosen: Vec<(f32, f32, f32)> = Vec::with_capacity(quota);
    for (i, c) in cands.iter().enumerate() {
        if chosen.len() >= quota {
            break;
        }
        let gi = ((c.1 / cell) as usize).min(gh - 1) * gw + ((c.0 / cell) as usize).min(gw - 1);
        if counts[gi] < per_cell {
            counts[gi] += 1;
            taken[i] = true;
            chosen.push(*c);
        }
    }
    for (i, c) in cands.iter().enumerate() {
        if chosen.len() >= quota {
            break;
        }
        if !taken[i] {
            chosen.push(*c);
        }
    }

    // descriptors
    let smooth = blur(img, 1.6);
    let pat = pattern();
    let mut kps = Vec::with_capacity(chosen.len());
    let mut descs = Vec::with_capacity(chosen.len());
    let rad = 12i32;
    // level pixel -> full-size pixel with the real size ratio of this level (its size is rounded,
    // so the ratio is not exactly 1/s; with 1/s a point at the far edge would be off by a pixel or more)
    let snap_on = corner_snap();
    let (rx, ry) = (full.0 as f32 / w as f32, full.1 as f32 / h as f32);
    for (x, y, r) in chosen {
        let (x, y) = if snap_on { snap_to_corner(img, x, y) } else { (x, y) };
        // intensity centroid orientation
        let (cx, cy) = (x.round() as i32, y.round() as i32);
        let mut m10 = 0.0f32;
        let mut m01 = 0.0f32;
        for dy in -rad..=rad {
            for dx in -rad..=rad {
                if dx * dx + dy * dy > rad * rad {
                    continue;
                }
                let v = smooth.at((cx + dx) as usize, (cy + dy) as usize);
                m10 += dx as f32 * v;
                m01 += dy as f32 * v;
            }
        }
        let ang = m01.atan2(m10);
        let (sa, ca) = ang.sin_cos();
        let mut d = [0u64; 4];
        for (bit, p) in pat.iter().enumerate() {
            let x1 = x + ca * p[0] - sa * p[1];
            let y1 = y + sa * p[0] + ca * p[1];
            let x2 = x + ca * p[2] - sa * p[3];
            let y2 = y + sa * p[2] + ca * p[3];
            if smooth.sample(x1, y1) < smooth.sample(x2, y2) {
                d[bit / 64] |= 1u64 << (bit % 64);
            }
        }
        kps.push(Keypoint {
            x: (x + 0.5) * rx - 0.5,
            y: (y + 0.5) * ry - 0.5,
            score: r,
            scale: 1.0 / s,
        });
        descs.push(d);
    }
    (kps, descs)
}

#[inline]
fn hamming(a: &[u64; 4], b: &[u64; 4]) -> u32 {
    (a[0] ^ b[0]).count_ones() + (a[1] ^ b[1]).count_ones() + (a[2] ^ b[2]).count_ones() + (a[3] ^ b[3]).count_ones()
}

/// Mutual nearest neighbours passing the ratio test. Returns (index a, index b, distance).
pub fn match_descriptors(a: &Features, b: &Features, ratio: f32) -> Vec<(usize, usize, u32)> {
    if a.desc.is_empty() || b.desc.is_empty() {
        return vec![];
    }
    let best = |from: &Vec<[u64; 4]>, to: &Vec<[u64; 4]>| -> Vec<(usize, u32, u32)> {
        from.par_iter()
            .map(|d| {
                let mut b1 = u32::MAX;
                let mut b2 = u32::MAX;
                let mut bi = 0;
                for (j, e) in to.iter().enumerate() {
                    let h = hamming(d, e);
                    if h < b1 {
                        b2 = b1;
                        b1 = h;
                        bi = j;
                    } else if h < b2 {
                        b2 = h;
                    }
                }
                (bi, b1, b2)
            })
            .collect()
    };
    let ab = best(&a.desc, &b.desc);
    let ba = best(&b.desc, &a.desc);
    let mut out = Vec::new();
    for (i, &(j, d1, d2)) in ab.iter().enumerate() {
        if d1 > 90 {
            continue;
        }
        if (d1 as f32) > ratio * d2 as f32 {
            continue;
        }
        if ba[j].0 != i {
            continue;
        }
        out.push((i, j, d1));
    }
    out
}

/// One verified correspondence in working-image pixel coordinates.
#[derive(Clone, Copy, Debug)]
pub struct Corr {
    pub ax: f32,
    pub ay: f32,
    pub bx: f32,
    pub by: f32,
    pub dist: u32,
}

pub struct PairMatch {
    pub h: [f64; 9],
    pub corrs: Vec<Corr>,
}

/// Full pair pipeline: descriptor matching, RANSAC, guided matching, thinning.
pub fn match_pair(a: &Features, b: &Features, max_points: usize, seed: u64) -> Option<PairMatch> {
    let m = match_descriptors(a, b, 0.85);
    if m.len() < 10 {
        return None;
    }
    let long = a.w.max(a.h) as f64;
    let thr = (0.0035 * long).max(2.5);
    let src: Vec<(f64, f64)> = m.iter().map(|&(i, _, _)| (a.kps[i].x as f64, a.kps[i].y as f64)).collect();
    let dst: Vec<(f64, f64)> = m.iter().map(|&(_, j, _)| (b.kps[j].x as f64, b.kps[j].y as f64)).collect();

    let (h, inl) = ransac_h(&src, &dst, thr, seed)?;
    if !sane_h(&h) || inl.len() < 10 {
        return None;
    }
    // guided matching: look for more correspondences consistent with the model
    let guided = guided(a, b, &h, thr * 1.2);
    let gsrc: Vec<(f64, f64)> = guided.iter().map(|c| (c.ax as f64, c.ay as f64)).collect();
    let gdst: Vec<(f64, f64)> = guided.iter().map(|c| (c.bx as f64, c.by as f64)).collect();
    let h2 = fit_homography(&gsrc, &gdst).filter(sane_h).unwrap_or(h);
    let mut final_c: Vec<Corr> = guided
        .into_iter()
        .filter(|c| {
            let (x, y) = h_apply(&h2, c.ax as f64, c.ay as f64);
            ((x - c.bx as f64).powi(2) + (y - c.by as f64).powi(2)).sqrt() < thr
        })
        .collect();
    // acceptance test (probabilistic model of Brown & Lowe)
    let n_in = final_c.len().max(inl.len());
    if (n_in as f64) < 12.0f64.max(6.0 + 0.2 * m.len() as f64) {
        return None;
    }
    if final_c.len() < inl.len() {
        final_c = inl
            .iter()
            .map(|&k| {
                let (i, j, d) = m[k];
                Corr { ax: a.kps[i].x, ay: a.kps[i].y, bx: b.kps[j].x, by: b.kps[j].y, dist: d }
            })
            .collect();
    }
    let thinned = thin(final_c, a.w, a.h, max_points);
    Some(PairMatch { h: h2, corrs: thinned })
}

fn sane_h(h: &[f64; 9]) -> bool {
    let det = h[0] * h[4] - h[1] * h[3];
    det > 0.1 && det < 10.0 && h[6].abs() < 0.01 && h[7].abs() < 0.01
}

fn ransac_h(src: &[(f64, f64)], dst: &[(f64, f64)], thr: f64, seed: u64) -> Option<([f64; 9], Vec<usize>)> {
    let n = src.len();
    let mut rng = Rng(seed | 1);
    let mut best: Vec<usize> = Vec::new();
    let mut iters = 3000usize;
    let mut it = 0;
    let thr2 = thr * thr;
    while it < iters {
        it += 1;
        let mut idx = [0usize; 4];
        let mut k = 0;
        let mut guard = 0;
        while k < 4 && guard < 50 {
            guard += 1;
            let c = rng.below(n);
            if idx[..k].contains(&c) {
                continue;
            }
            idx[k] = c;
            k += 1;
        }
        if k < 4 {
            continue;
        }
        let s: Vec<(f64, f64)> = idx.iter().map(|&i| src[i]).collect();
        let d: Vec<(f64, f64)> = idx.iter().map(|&i| dst[i]).collect();
        let Some(h) = fit_homography(&s, &d) else { continue };
        if !sane_h(&h) {
            continue;
        }
        let inl: Vec<usize> = (0..n)
            .filter(|&i| {
                let (x, y) = h_apply(&h, src[i].0, src[i].1);
                (x - dst[i].0).powi(2) + (y - dst[i].1).powi(2) < thr2
            })
            .collect();
        if inl.len() > best.len() {
            best = inl;
            let w = best.len() as f64 / n as f64;
            let p = 1.0 - w.powi(4);
            if p <= 1e-12 {
                iters = it;
            } else {
                iters = iters.min(((1.0 - 0.999f64).ln() / p.ln()).ceil() as usize + 10);
            }
        }
    }
    if best.len() < 8 {
        return None;
    }
    // refine on inliers twice
    let mut h = fit_homography(
        &best.iter().map(|&i| src[i]).collect::<Vec<_>>(),
        &best.iter().map(|&i| dst[i]).collect::<Vec<_>>(),
    )?;
    for _ in 0..2 {
        let inl: Vec<usize> = (0..n)
            .filter(|&i| {
                let (x, y) = h_apply(&h, src[i].0, src[i].1);
                (x - dst[i].0).powi(2) + (y - dst[i].1).powi(2) < thr2
            })
            .collect();
        if inl.len() < 8 {
            break;
        }
        if let Some(h2) = fit_homography(
            &inl.iter().map(|&i| src[i]).collect::<Vec<_>>(),
            &inl.iter().map(|&i| dst[i]).collect::<Vec<_>>(),
        ) {
            h = h2;
            best = inl;
        }
    }
    Some((h, best))
}

fn guided(a: &Features, b: &Features, h: &[f64; 9], radius: f64) -> Vec<Corr> {
    let cell = radius.max(4.0) as f32;
    let gw = (b.w as f32 / cell).ceil() as usize + 1;
    let gh = (b.h as f32 / cell).ceil() as usize + 1;
    let mut grid: Vec<Vec<usize>> = vec![Vec::new(); gw * gh];
    for (j, k) in b.kps.iter().enumerate() {
        let gx = ((k.x.max(0.0) / cell) as usize).min(gw - 1);
        let gy = ((k.y.max(0.0) / cell) as usize).min(gh - 1);
        grid[gy * gw + gx].push(j);
    }
    let r2 = (radius * radius) as f32;
    let mut cand: Vec<(usize, usize, u32)> = a
        .kps
        .par_iter()
        .enumerate()
        .filter_map(|(i, k)| {
            let (px, py) = h_apply(h, k.x as f64, k.y as f64);
            let (px, py) = (px as f32, py as f32);
            if px < 0.0 || py < 0.0 || px >= b.w as f32 || py >= b.h as f32 {
                return None;
            }
            let gx = (px / cell) as isize;
            let gy = (py / cell) as isize;
            let mut b1 = u32::MAX;
            let mut b2 = u32::MAX;
            let mut bj = usize::MAX;
            for yy in gy - 1..=gy + 1 {
                for xx in gx - 1..=gx + 1 {
                    if xx < 0 || yy < 0 || xx >= gw as isize || yy >= gh as isize {
                        continue;
                    }
                    for &j in &grid[yy as usize * gw + xx as usize] {
                        let kb = &b.kps[j];
                        if (kb.x - px).powi(2) + (kb.y - py).powi(2) > r2 {
                            continue;
                        }
                        // scales must be compatible
                        let sr = kb.scale / k.scale;
                        if !(0.55..=1.8).contains(&sr) {
                            continue;
                        }
                        let d = hamming(&a.desc[i], &b.desc[j]);
                        if d < b1 {
                            b2 = b1;
                            b1 = d;
                            bj = j;
                        } else if d < b2 {
                            b2 = d;
                        }
                    }
                }
            }
            if bj == usize::MAX || b1 > 85 || (b2 != u32::MAX && b1 as f32 > 0.92 * b2 as f32) {
                return None;
            }
            Some((i, bj, b1))
        })
        .collect();
    // keep each b-point once (best distance)
    cand.sort_by_key(|c| c.2);
    let mut used_b = vec![false; b.kps.len()];
    let mut out = Vec::new();
    for (i, j, d) in cand {
        if used_b[j] {
            continue;
        }
        used_b[j] = true;
        out.push(Corr { ax: a.kps[i].x, ay: a.kps[i].y, bx: b.kps[j].x, by: b.kps[j].y, dist: d });
    }
    out
}

/// Side of the grid `thin` spreads the points over.
const THIN_GRID: usize = 8;

/// Order in which the grid cells are visited: farthest-point order (each next cell is the one farthest from all
/// cells taken so far, ties by index), deterministic, so every prefix of the order covers the whole picture.
fn scattered_cells(g: usize) -> Vec<usize> {
    let mut order: Vec<usize> = vec![(g / 2) * g + g / 2];
    let mut dist = vec![usize::MAX; g * g];
    while order.len() < g * g {
        let last = *order.last().unwrap_or(&0);
        let (lx, ly) = ((last % g) as isize, (last / g) as isize);
        let mut best = (0usize, usize::MAX);
        for c in 0..g * g {
            if order.contains(&c) {
                continue;
            }
            let (dx, dy) = ((c % g) as isize - lx, (c / g) as isize - ly);
            dist[c] = dist[c].min((dx * dx + dy * dy) as usize);
            if best.1 == usize::MAX || dist[c] > dist[best.0] {
                best = (c, dist[c]);
            }
        }
        order.push(best.0);
    }
    order
}

/// Keep at most `max` correspondences, spread evenly over the overlap.
pub fn thin(c: Vec<Corr>, w: usize, h: usize, max: usize) -> Vec<Corr> {
    thin_mode(c, w, h, max, true)
}

/// `thin`, with the choice of order. `even = true` (the default) takes the best match of each grid cell
/// round by round, visiting the cells in a scattered order, so a limit below the cell count still covers the
/// whole picture. `even = false` restores the former row by row order (top rows first), which crowded the
/// points into the upper part of the picture.
pub fn thin_mode(mut c: Vec<Corr>, w: usize, h: usize, max: usize, even: bool) -> Vec<Corr> {
    if c.len() <= max {
        return c;
    }
    c.sort_by_key(|k| k.dist);
    let g = THIN_GRID;
    let mut cells: Vec<Vec<Corr>> = vec![Vec::new(); g * g];
    for k in c {
        let gx = ((k.ax / w as f32 * g as f32) as usize).min(g - 1);
        let gy = ((k.ay / h as f32 * g as f32) as usize).min(g - 1);
        cells[gy * g + gx].push(k);
    }
    let order: Vec<usize> = if even { scattered_cells(g) } else { (0..g * g).collect() };
    let mut out = Vec::with_capacity(max);
    let mut round = 0;
    while out.len() < max {
        let mut any = false;
        for &ci in &order {
            let cell = &cells[ci];
            if round < cell.len() {
                out.push(cell[round]);
                any = true;
                if out.len() >= max {
                    break;
                }
            }
        }
        if !any {
            break;
        }
        round += 1;
    }
    out
}

/// Coarse map of sky and clouds (smooth, blue or grey/white areas connected to the top edge).
/// Clouds move between shots, so points there would pull the alignment off.
pub struct SkyMask {
    pub cell: f32,
    pub gw: usize,
    pub gh: usize,
    pub sky: Vec<bool>,
}

impl SkyMask {
    #[inline]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        if x < 0.0 || y < 0.0 {
            return false;
        }
        let gx = (x / self.cell) as usize;
        let gy = (y / self.cell) as usize;
        gx < self.gw && gy < self.gh && self.sky[gy * self.gw + gx]
    }
    pub fn fraction(&self) -> f32 {
        self.sky.iter().filter(|v| **v).count() as f32 / self.sky.len().max(1) as f32
    }
}

pub const SKY_TEXTURE_LIMIT: f32 = 0.006;

pub fn sky_mask(img: &image::RgbImage, gray: &Gray) -> SkyMask {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let cell = (w.max(h) as f32 / 96.0).max(4.0);
    let gw = (w as f32 / cell).ceil() as usize;
    let gh = (h as f32 / cell).ceil() as usize;
    let smooth = blur(gray, 1.0);
    let mut acc = vec![[0.0f32; 5]; gw * gh]; // r, g, b, |laplacian|, count
    let raw = img.as_raw();
    for y in 1..h.saturating_sub(1) {
        let gy = ((y as f32 / cell) as usize).min(gh - 1);
        for x in 1..w.saturating_sub(1) {
            let gx = ((x as f32 / cell) as usize).min(gw - 1);
            let o = (y * w + x) * 3;
            let lap = (4.0 * smooth.at(x, y) - smooth.at(x - 1, y) - smooth.at(x + 1, y) - smooth.at(x, y - 1) - smooth.at(x, y + 1)).abs();
            let a = &mut acc[gy * gw + gx];
            a[0] += raw[o] as f32 / 255.0;
            a[1] += raw[o + 1] as f32 / 255.0;
            a[2] += raw[o + 2] as f32 / 255.0;
            a[3] += lap;
            a[4] += 1.0;
        }
    }
    let tex: Vec<f32> = acc.iter().map(|a| a[3] / a[4].max(1.0)).collect();
    let like: Vec<bool> = acc
        .iter()
        .zip(&tex)
        .map(|(a, t)| {
            let n = a[4].max(1.0);
            let (r, g, b) = (a[0] / n, a[1] / n, a[2] / n);
            let mx = r.max(g).max(b);
            let mn = r.min(g).min(b);
            let sat = if mx > 1e-3 { (mx - mn) / mx } else { 0.0 };
            let blue = b > r + 0.04 && b >= g - 0.02 && mx > 0.25;
            let cloud = sat < 0.22 && mx > 0.35;
            (blue || cloud) && *t < SKY_TEXTURE_LIMIT
        })
        .collect();
    // flood fill from the top edge
    let mut sky = vec![false; gw * gh];
    let mut stack: Vec<usize> = (0..gw).filter(|&x| like[x]).collect();
    for &i in &stack {
        sky[i] = true;
    }
    while let Some(i) = stack.pop() {
        let (x, y) = (i % gw, i / gw);
        let mut push = |j: usize| {
            if !sky[j] && like[j] {
                sky[j] = true;
                stack.push(j);
            }
        };
        if x > 0 {
            push(i - 1);
        }
        if x + 1 < gw {
            push(i + 1);
        }
        if y > 0 {
            push(i - gw);
        }
        if y + 1 < gh {
            push(i + gw);
        }
    }
    // grow one cell into soft cloud edges (but never into sharp detail such as ridges or roofs)
    let mut grown = sky.clone();
    for y in 0..gh {
        for x in 0..gw {
            let i = y * gw + x;
            if sky[i] || tex[i] > 2.0 * SKY_TEXTURE_LIMIT {
                continue;
            }
            let near = (x > 0 && sky[i - 1]) || (x + 1 < gw && sky[i + 1]) || (y > 0 && sky[i - gw]) || (y + 1 < gh && sky[i + gw]);
            if near {
                grown[i] = true;
            }
        }
    }
    SkyMask { cell, gw, gh, sky: grown }
}

/// Cloud check around a point, following the original's rule:
/// the photo is judged at 800 px (long side), in a circle of radius 20 px around the point,
/// and the point is dropped when the cloud score is above 0.5.
pub const CLOUD_CHECK_SIZE: f32 = 800.0;
pub const CLOUD_CHECK_RADIUS: f32 = 20.0;
pub const CLOUD_THRESHOLD: f32 = 0.5;

/// Mean of `f` over a circle of radius `r` around (x, y), sampled on a regular grid (13 x 13, the part inside
/// the circle). Shared by the sky check and the foliage check.
pub fn circle_mean(x: f32, y: f32, r: f32, f: impl Fn(f32, f32) -> f32) -> f32 {
    let steps = 6i32;
    let mut sum = 0.0f32;
    let mut total = 0usize;
    for dy in -steps..=steps {
        for dx in -steps..=steps {
            let fx = dx as f32 / steps as f32;
            let fy = dy as f32 / steps as f32;
            if fx * fx + fy * fy > 1.0 {
                continue;
            }
            total += 1;
            sum += f(x + fx * r, y + fy * r);
        }
    }
    sum / total.max(1) as f32
}

/// Share of the circle around (x, y) (work-image pixels) that is sky or cloud, 0..1.
pub fn cloud_score(mask: &SkyMask, x: f32, y: f32, work_long: f32) -> f32 {
    let r = CLOUD_CHECK_RADIUS * work_long / CLOUD_CHECK_SIZE;
    circle_mean(x, y, r, |px, py| if mask.contains(px, py) { 1.0 } else { 0.0 })
}

/// Drop keypoints whose surroundings are mostly sky or cloud.
pub fn remove_masked(f: &mut Features, mask: &SkyMask) -> usize {
    let before = f.kps.len();
    let long = f.w.max(f.h) as f32;
    let keep: Vec<bool> = f.kps.iter().map(|k| cloud_score(mask, k.x, k.y, long) <= CLOUD_THRESHOLD).collect();
    let mut i = 0;
    f.kps.retain(|_| {
        let k = keep[i];
        i += 1;
        k
    });
    let mut i = 0;
    f.desc.retain(|_| {
        let k = keep[i];
        i += 1;
        k
    });
    before - f.kps.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imgutil::to_gray;
    use crate::testutil::{solid, texture};

    fn corr(x: f32, y: f32, dist: u32) -> Corr {
        Corr { ax: x, ay: y, bx: x, by: y, dist }
    }

    #[test]
    fn hamming_counts_differing_bits() {
        let a = [0u64, 0, 0, 0];
        assert_eq!(hamming(&a, &a), 0);
        assert_eq!(hamming(&a, &[1, 0, 0, 0]), 1);
        assert_eq!(hamming(&[u64::MAX; 4], &a), 256);
        assert_eq!(hamming(&[0b1010, 0, 0, 1 << 63], &[0b0110, 0, 0, 0]), 3);
    }

    /// Keypoints of a bright rectangle on a dark ground must sit on its corners (pixel-edge coordinates,
    /// pixel centres at integers), at every pyramid level, not beside them.
    #[test]
    fn keypoints_sit_on_corners() {
        let (w, h) = (400usize, 300usize);
        let mut g = crate::imgutil::Gray::new(w, h);
        for y in 100..200 {
            for x in 120..300 {
                g.data[y * w + x] = 0.9;
            }
        }
        // the rectangle covers pixels 120..=299 x 100..=199, so its corners lie at 119.5/299.5 and 99.5/199.5
        let corners = [(119.5f32, 99.5f32), (299.5, 99.5), (119.5, 199.5), (299.5, 199.5)];
        let f = detect(&g, 400);
        let mut worst = 0.0f32;
        let mut n = 0;
        for k in &f.kps {
            let d = corners.iter().map(|c| (k.x - c.0).hypot(k.y - c.1)).fold(f32::MAX, f32::min);
            if d < 8.0 * k.scale {
                // allow one pixel of the level the point was found on
                worst = worst.max(d / k.scale);
                n += 1;
            }
        }
        assert!(n >= 4, "only {n} keypoints near the corners");
        assert!(worst < 0.5, "a keypoint is {worst:.2} level pixels from the corner");
    }

    #[test]
    fn plain_surface_has_no_keypoints() {
        let g = to_gray(&solid(160, 120, [90, 90, 90]));
        let f = detect(&g, 500);
        assert!(f.kps.is_empty());
        assert_eq!(f.kps.len(), f.desc.len());
    }

    #[test]
    fn texture_gives_consistent_keypoints() {
        let g = to_gray(&texture(240, 180, 21));
        let f = detect(&g, 300);
        assert!(f.kps.len() >= 20, "only {} keypoints", f.kps.len());
        assert!(f.kps.len() <= 300);
        assert_eq!(f.kps.len(), f.desc.len());
        assert_eq!((f.w, f.h), (240, 180));
        assert!(f.kps.iter().all(|k| k.x >= 0.0 && k.y >= 0.0 && k.x < 240.0 && k.y < 180.0 && k.scale > 0.0));
    }

    #[test]
    fn detection_is_deterministic() {
        let g = to_gray(&texture(200, 150, 22));
        let a = detect(&g, 200);
        let b = detect(&g, 200);
        assert_eq!(a.kps.len(), b.kps.len());
        assert_eq!(a.desc, b.desc);
    }

    #[test]
    fn identical_images_match_themselves() {
        let g = to_gray(&texture(240, 180, 23));
        let f = detect(&g, 300);
        let m = match_descriptors(&f, &f, 0.9);
        assert!(m.len() >= f.kps.len() / 2, "{} of {}", m.len(), f.kps.len());
        assert!(m.iter().all(|&(i, j, d)| i == j && d == 0));
    }

    #[test]
    fn match_pair_finds_known_translation() {
        let big = texture(360, 200, 24);
        let a = crate::testutil::crop(&big, 0, 0, 260, 200);
        let b = crate::testutil::crop(&big, 60, 0, 260, 200);
        let fa = detect(&to_gray(&a), 600);
        let fb = detect(&to_gray(&b), 600);
        let pm = match_pair(&fa, &fb, 100, 1).expect("pair should match");
        // point (x, y) in a is at (x - 60, y) in b
        let (x, y) = h_apply(&pm.h, 150.0, 100.0);
        assert!((x - 90.0).abs() < 2.0 && (y - 100.0).abs() < 2.0, "({x}, {y})");
        assert!(pm.corrs.len() >= 12 && pm.corrs.len() <= 100);
        let good = pm.corrs.iter().filter(|c| ((c.bx - (c.ax - 60.0)).powi(2) + (c.by - c.ay).powi(2)).sqrt() < 3.0).count();
        assert!(good as f32 >= 0.9 * pm.corrs.len() as f32, "{good} of {}", pm.corrs.len());
    }

    #[test]
    fn unrelated_images_do_not_match() {
        let fa = detect(&to_gray(&texture(240, 180, 31)), 300);
        let fb = detect(&to_gray(&texture(240, 180, 32)), 300);
        assert!(match_pair(&fa, &fb, 100, 1).is_none());
    }

    #[test]
    fn thin_limits_and_spreads_points() {
        let few = vec![corr(1.0, 1.0, 5), corr(2.0, 2.0, 5)];
        assert_eq!(thin(few, 100, 100, 10).len(), 2);
        // 100 points crowded in one corner and 4 in the others: all corners must stay represented
        let mut many: Vec<Corr> = (0..100).map(|i| corr(1.0 + (i % 10) as f32 * 0.1, 1.0 + (i / 10) as f32 * 0.1, 1)).collect();
        many.extend([corr(95.0, 5.0, 50), corr(5.0, 95.0, 50), corr(95.0, 95.0, 50), corr(50.0, 50.0, 50)]);
        let t = thin(many, 100, 100, 8);
        assert_eq!(t.len(), 8);
        assert!(t.iter().any(|c| c.ax > 90.0 && c.ay > 90.0));
        assert!(t.iter().any(|c| c.ax > 90.0 && c.ay < 10.0));
    }

    #[test]
    fn thin_covers_all_rows_and_columns() {
        // one match in the middle of each of the 8x8 cells, limit 20 (below the cell count)
        let all: Vec<Corr> = (0..64).map(|i| corr((i % 8) as f32 * 12.5 + 6.0, (i / 8) as f32 * 12.5 + 6.0, 10)).collect();
        let rows = |t: &[Corr]| {
            let mut r = [false; 8];
            t.iter().for_each(|c| r[((c.ay / 12.5) as usize).min(7)] = true);
            r.iter().filter(|b| **b).count()
        };
        let cols = |t: &[Corr]| {
            let mut r = [false; 8];
            t.iter().for_each(|c| r[((c.ax / 12.5) as usize).min(7)] = true);
            r.iter().filter(|b| **b).count()
        };
        let t = thin(all.clone(), 100, 100, 20);
        assert_eq!(t.len(), 20);
        assert_eq!(rows(&t), 8, "all rows used");
        assert_eq!(cols(&t), 8, "all columns used");
        // the former order only reached the upper rows
        let old = thin_mode(all, 100, 100, 20, false);
        assert!(rows(&old) < 8);
    }

    #[test]
    fn blue_sky_is_detected_as_sky() {
        let img = solid(192, 128, [90, 140, 220]);
        let m = sky_mask(&img, &to_gray(&img));
        assert!(m.fraction() > 0.95, "fraction {}", m.fraction());
        assert!(m.contains(10.0, 10.0));
        assert!(m.contains(190.0, 126.0));
        // outside the picture
        assert!(!m.contains(-1.0, 5.0));
        assert!(!m.contains(5.0, -1.0));
        assert!(!m.contains(1e6, 5.0));
    }

    #[test]
    fn grey_cloud_is_detected_as_sky() {
        let img = solid(192, 128, [200, 200, 205]);
        let m = sky_mask(&img, &to_gray(&img));
        assert!(m.fraction() > 0.95);
    }

    #[test]
    fn textured_ground_is_not_sky() {
        let img = texture(192, 128, 41);
        let m = sky_mask(&img, &to_gray(&img));
        assert!(m.fraction() < 0.1, "fraction {}", m.fraction());
    }

    #[test]
    fn dark_surface_is_not_sky() {
        let img = solid(192, 128, [30, 25, 20]);
        let m = sky_mask(&img, &to_gray(&img));
        assert_eq!(m.fraction(), 0.0);
    }

    #[test]
    fn sky_below_the_ground_is_not_connected() {
        // top half: textured ground, bottom half: blue. Sky must touch the top edge, so none is found.
        let mut img = texture(192, 128, 42);
        for y in 64..128 {
            for x in 0..192 {
                img.put_pixel(x, y, image::Rgb([90, 140, 220]));
            }
        }
        let m = sky_mask(&img, &to_gray(&img));
        assert!(m.fraction() < 0.1);
    }

    #[test]
    fn remove_masked_drops_points_in_sky_only() {
        let kp = |x: f32, y: f32| Keypoint { x, y, score: 1.0, scale: 1.0 };
        let mut f = Features { w: 100, h: 100, kps: vec![kp(10.0, 10.0), kp(60.0, 60.0)], desc: vec![[1; 4], [2; 4]] };
        // grid of 10 x 10 cells of 10 px; the top-left quarter is sky
        let mut sky = vec![false; 100];
        for y in 0..5 {
            for x in 0..5 {
                sky[y * 10 + x] = true;
            }
        }
        let mask = SkyMask { cell: 10.0, gw: 10, gh: 10, sky };
        let removed = remove_masked(&mut f, &mask);
        assert_eq!(removed, 1);
        assert_eq!(f.kps.len(), 1);
        assert_eq!(f.desc, vec![[2; 4]]);
        assert_eq!(f.kps[0].x, 60.0);
        // nothing masked, nothing removed
        let none = SkyMask { cell: 10.0, gw: 10, gh: 10, sky: vec![false; 100] };
        assert_eq!(remove_masked(&mut f, &none), 0);
    }
}
