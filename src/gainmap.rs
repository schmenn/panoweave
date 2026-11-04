//! Smooth brightness correction across each photo ("gain maps").
//!
//! `stitch::compute_gains` finds ONE factor per photo and colour channel. The real brightness
//! difference between two photos differs from place to place (vignetting, sky gradient, automatic
//! exposure and local tone mapping of phones), so one factor leaves visible rectangles in smooth
//! areas such as the sky. Here every photo gets a coarse grid of correction values (bilinear
//! between the grid points) that is solved from point-wise comparisons in the overlaps.
//!
//! For a spot `p` seen by photos A and B (lightly blurred, so parallax and fine detail do not
//! matter) the corrected values must agree: `ln I_A + g_A(x_A) = ln I_B + g_B(x_B)`. Every spot
//! gives one linear equation in the grid values; smoothness between neighbouring grid points and
//! a weak pull towards "no change" keep the system well posed, Huber weights (IRLS) keep moving
//! objects out. Everything is deterministic (fixed order, no random numbers).

use crate::gainmap_solve::{solve_channel, Obs, SolveParams};
use crate::imgutil::sample_rgb_linear;
use crate::stitch::{clip_limits_linear, Mapper, PanoView, RenderPhoto};
use rayon::prelude::*;

/// Grid points along the long side of a photo (the short side follows its proportion).
pub const DEFAULT_GRID_LONG: usize = 5;
/// Weight of the smoothness between neighbouring grid points, relative to the data weight a
/// grid point gets on average (1 = as strong as the measurements around it).
pub const DEFAULT_SMOOTH: f64 = 0.1;
/// Weight of the pull of every grid point towards "no change", same unit as `smooth`.
pub const DEFAULT_ANCHOR: f64 = 0.02;
/// Largest deviation from the photo's global factor, as natural log (0.3 = about +35 % / -26 %).
pub const DEFAULT_MAX_DEV: f64 = 0.3;
/// A photo needs this many usable comparisons to get a map, else it keeps its global factor.
pub const DEFAULT_MIN_SAMPLES: usize = 60;
/// Width in pixels of the small panorama the comparisons are made on.
pub const DEFAULT_PREVIEW_WIDTH: usize = 480;
/// Spots nearer to a photo's border than this (centre weight, 0 = border, 1 = centre) are not used.
pub const DEFAULT_MIN_EDGE_WEIGHT: f32 = 0.1;
/// Radius in preview pixels of the box blur applied before comparing.
pub const DEFAULT_BLUR_RADIUS: usize = 2;
/// Distance in preview pixels of the two points whose brightness is compared to detect edges.
pub const DEFAULT_EDGE_STEP: usize = 3;
/// Spots where the blurred brightness changes by more than this (natural log, across the
/// distance `2 * edge_step`) are edges and not used.
pub const DEFAULT_MAX_GRADIENT: f32 = 0.2;
/// Only every `stride`-th preview pixel in x and y is used as a comparison spot.
pub const DEFAULT_STRIDE: usize = 2;
/// Upper limit of comparisons (more are thinned out evenly) – bounds the solving time.
pub const DEFAULT_MAX_OBS: usize = 60_000;
/// At most this many photos are compared with each other at one spot.
pub const DEFAULT_MAX_COVER: usize = 5;
/// Differences above this (natural log) count less and less (Huber).
pub const DEFAULT_HUBER: f64 = 0.04;
/// Re-weighting rounds of the robust solution.
pub const DEFAULT_IRLS_ITERS: usize = 4;
/// Conjugate gradient iterations per round, and the relative residual that stops them.
pub const DEFAULT_CG_ITERS: usize = 150;
pub const DEFAULT_CG_TOL: f64 = 1e-5;

/// Settings of the smooth brightness correction. `enabled = false` leaves everything as before.
#[derive(Clone, Debug, PartialEq)]
pub struct GainMapConfig {
    pub enabled: bool,
    pub grid_long: usize,
    pub smooth: f64,
    pub anchor: f64,
    pub max_dev: f64,
    pub min_samples: usize,
    pub preview_width: usize,
    pub min_edge_weight: f32,
    pub blur_radius: usize,
    pub edge_step: usize,
    pub max_gradient: f32,
    pub stride: usize,
    pub max_obs: usize,
    pub max_cover: usize,
    pub huber: f64,
    pub irls_iters: usize,
    pub cg_iters: usize,
    pub cg_tol: f64,
}

/// Experiments: `PW_GM_GRID`, `PW_GM_SMOOTH`, `PW_GM_MAXDEV`, `PW_GM_ANCHOR`, `PW_GM_EDGE`, `PW_GM_BLUR`, `PW_GM_GRAD`, `PW_GM_HUBER`, `PW_GM_COVER` replace the defaults of the same name.
fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

impl Default for GainMapConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            grid_long: env_or("PW_GM_GRID", DEFAULT_GRID_LONG),
            smooth: env_or("PW_GM_SMOOTH", DEFAULT_SMOOTH),
            anchor: env_or("PW_GM_ANCHOR", DEFAULT_ANCHOR),
            max_dev: env_or("PW_GM_MAXDEV", DEFAULT_MAX_DEV),
            min_samples: DEFAULT_MIN_SAMPLES,
            preview_width: DEFAULT_PREVIEW_WIDTH,
            min_edge_weight: env_or("PW_GM_EDGE", DEFAULT_MIN_EDGE_WEIGHT),
            blur_radius: env_or("PW_GM_BLUR", DEFAULT_BLUR_RADIUS),
            edge_step: DEFAULT_EDGE_STEP,
            max_gradient: env_or("PW_GM_GRAD", DEFAULT_MAX_GRADIENT),
            stride: DEFAULT_STRIDE,
            max_obs: DEFAULT_MAX_OBS,
            max_cover: env_or("PW_GM_COVER", DEFAULT_MAX_COVER),
            huber: env_or("PW_GM_HUBER", DEFAULT_HUBER),
            irls_iters: DEFAULT_IRLS_ITERS,
            cg_iters: DEFAULT_CG_ITERS,
            cg_tol: DEFAULT_CG_TOL,
        }
    }
}

impl GainMapConfig {
    /// Experiments: `PW_GAINMAP=0` / `1` forces the correction off / on, whatever the switch says.
    pub fn is_on(&self) -> bool {
        match std::env::var("PW_GAINMAP").ok().as_deref() {
            Some("0") => false,
            Some("1") => true,
            _ => self.enabled,
        }
    }
}

/// Correction factors (linear, per colour channel) on a grid over one photo. Positions are
/// fractions of the photo (x / width, y / height); between the grid points the factor is
/// interpolated bilinearly, outside it is clamped to the border.
#[derive(Clone, Debug, PartialEq)]
pub struct GainMap {
    pub gw: usize,
    pub gh: usize,
    pub d: Vec<[f32; 3]>,
}

/// The four grid points around the position and their bilinear weights. `gw`, `gh` >= 1.
pub fn cell(fx: f32, fy: f32, gw: usize, gh: usize) -> ([usize; 4], [f32; 4]) {
    let axis = |f: f32, n: usize| -> (usize, usize, f32) {
        if n < 2 {
            return (0, 0, 0.0);
        }
        let u = f.clamp(0.0, 1.0) * (n - 1) as f32;
        let i0 = (u.floor() as usize).min(n - 2);
        (i0, i0 + 1, u - i0 as f32)
    };
    let (x0, x1, tx) = axis(fx, gw);
    let (y0, y1, ty) = axis(fy, gh);
    ([y0 * gw + x0, y0 * gw + x1, y1 * gw + x0, y1 * gw + x1], [(1.0 - tx) * (1.0 - ty), tx * (1.0 - ty), (1.0 - tx) * ty, tx * ty])
}

impl GainMap {
    /// A map that changes nothing.
    #[cfg(test)]
    pub fn identity(gw: usize, gh: usize) -> Self {
        GainMap { gw, gh, d: vec![[1.0; 3]; gw.max(1) * gh.max(1)] }
    }

    /// Factor at the position (fractions of the photo).
    pub fn at(&self, fx: f32, fy: f32) -> [f32; 3] {
        if self.d.len() != self.gw * self.gh || self.d.is_empty() {
            return [1.0; 3];
        }
        let (n, w) = cell(fx, fy, self.gw, self.gh);
        let mut r = [0.0f32; 3];
        for j in 0..4 {
            for (k, v) in r.iter_mut().enumerate() {
                *v += self.d[n[j]][k] * w[j];
            }
        }
        r
    }
}

/// Grid size of a photo of `w` x `h` pixels (both >= 2).
pub fn grid_size(w: f64, h: f64, grid_long: usize) -> (usize, usize) {
    let long = grid_long.max(2);
    let short = ((long as f64 * w.min(h) / w.max(h)).round() as usize).max(2);
    if w >= h { (long, short) } else { (short, long) }
}

/// What one photo shows on the small panorama: lightly blurred linear colour (with the photo's
/// global factor), where it is usable for comparing, and where in the photo that spot is.
struct Layer {
    x0: usize,
    y0: usize,
    bw: usize,
    bh: usize,
    lin: Vec<[f32; 3]>,
    ok: Vec<bool>,
    pos: Vec<[f32; 2]>,
}

impl Layer {
    fn at(&self, x: usize, y: usize) -> Option<usize> {
        if x < self.x0 || y < self.y0 || x >= self.x0 + self.bw || y >= self.y0 + self.bh {
            return None;
        }
        let i = (y - self.y0) * self.bw + (x - self.x0);
        self.ok[i].then_some(i)
    }
}

/// Box blur of the valid pixels only (invalid ones count nothing), radius `r`.
fn blur_valid(src: &[[f32; 3]], valid: &[bool], w: usize, h: usize, r: usize) -> Vec<[f32; 3]> {
    let pass = |inp: &[[f32; 4]], horizontal: bool| -> Vec<[f32; 4]> {
        let mut out = vec![[0.0f32; 4]; w * h];
        for y in 0..h {
            for x in 0..w {
                let mut s = [0.0f32; 4];
                for t in 0..=2 * r {
                    let (xx, yy) = if horizontal { (x + t, y) } else { (x, y + t) };
                    let (xx, yy) = (xx.wrapping_sub(if horizontal { r } else { 0 }), yy.wrapping_sub(if horizontal { 0 } else { r }));
                    if xx < w && yy < h {
                        let v = inp[yy * w + xx];
                        for k in 0..4 {
                            s[k] += v[k];
                        }
                    }
                }
                out[y * w + x] = s;
            }
        }
        out
    };
    let a: Vec<[f32; 4]> = src.iter().zip(valid).map(|(c, &v)| if v { [c[0], c[1], c[2], 1.0] } else { [0.0; 4] }).collect();
    let b = pass(&pass(&a, true), false);
    b.iter().zip(valid).map(|(s, &v)| if v && s[3] > 0.0 { [s[0] / s[3], s[1] / s[3], s[2] / s[3]] } else { [0.0; 3] }).collect()
}

fn luma(c: [f32; 3]) -> f32 {
    (0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]).max(1e-6)
}

/// The usable, blurred view of photo `p` on the preview grid (None: nothing usable).
fn build_layer(p: &RenderPhoto, m: &Mapper, w: usize, h: usize, cfg: &GainMapConfig, lims: (f32, f32)) -> Option<Layer> {
    let mut raw = vec![[0.0f32; 3]; w * h];
    let mut valid = vec![false; w * h];
    let mut pos = vec![[0.0f32; 2]; w * h];
    for py in 0..h {
        for px in 0..w {
            let Some(d) = m.dir(px, py) else { continue };
            let Some((x, y, wt)) = p.lookup(d) else { continue };
            if wt <= cfg.min_edge_weight {
                continue;
            }
            let c = sample_rgb_linear(&p.img, x, y);
            if c.iter().all(|&v| v > lims.0 && v < lims.1) {
                let i = py * w + px;
                raw[i] = [c[0] * p.gain[0], c[1] * p.gain[1], c[2] * p.gain[2]];
                valid[i] = true;
                pos[i] = [(x + 0.5) / p.w as f32, (y + 0.5) / p.h as f32];
            }
        }
    }
    if !valid.iter().any(|&v| v) {
        return None;
    }
    let blur = blur_valid(&raw, &valid, w, h, cfg.blur_radius);
    // flat spots only: the blurred brightness hardly changes around them
    let s = cfg.edge_step.max(1);
    let flat: Vec<bool> = (0..w * h)
        .map(|i| {
            if !valid[i] {
                return false;
            }
            let (x, y) = (i % w, i / w);
            let grad = |a: Option<usize>, b: Option<usize>| match (a, b) {
                (Some(a), Some(b)) if valid[a] && valid[b] => (luma(blur[a]).ln() - luma(blur[b]).ln()).abs(),
                _ => 0.0,
            };
            let gx = grad((x + s < w).then(|| y * w + x + s), x.checked_sub(s).map(|xx| y * w + xx));
            let gy = grad((y + s < h).then(|| (y + s) * w + x), y.checked_sub(s).map(|yy| yy * w + x));
            gx.max(gy) <= cfg.max_gradient
        })
        .collect();
    if std::env::var("PW_GM_DEBUG").is_ok() {
        eprintln!("  layer: {} usable px, {} flat px", valid.iter().filter(|&&v| v).count(), flat.iter().filter(|&&v| v).count());
    }
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for i in (0..w * h).filter(|&i| flat[i]) {
        x0 = x0.min(i % w);
        x1 = x1.max(i % w + 1);
        y0 = y0.min(i / w);
        y1 = y1.max(i / w + 1);
    }
    if x0 >= x1 {
        return None;
    }
    let (bw, bh) = (x1 - x0, y1 - y0);
    let mut l = Layer { x0, y0, bw, bh, lin: vec![[0.0; 3]; bw * bh], ok: vec![false; bw * bh], pos: vec![[0.0; 2]; bw * bh] };
    for y in 0..bh {
        for x in 0..bw {
            let (j, i) = (y * bw + x, (y + y0) * w + x + x0);
            l.ok[j] = flat[i];
            l.lin[j] = blur[i];
            l.pos[j] = pos[i];
        }
    }
    Some(l)
}

/// One comparison before the grid points are numbered: photos, their cells and weights, and the
/// wanted difference `g_a - g_b` per channel.
struct RawObs {
    a: usize,
    b: usize,
    ca: [usize; 4],
    wa: [f32; 4],
    cb: [usize; 4],
    wb: [f32; 4],
    d: [f32; 3],
}

fn collect_obs(layers: &[Option<Layer>], photos: &[RenderPhoto], w: usize, h: usize, cfg: &GainMapConfig) -> Vec<RawObs> {
    let grids: Vec<(usize, usize)> = photos.iter().map(|p| grid_size(p.w, p.h, cfg.grid_long)).collect();
    let stride = cfg.stride.max(1);
    let mut obs = Vec::new();
    for py in (0..h).step_by(stride) {
        for px in (0..w).step_by(stride) {
            let cover: Vec<(usize, usize)> = layers.iter().enumerate().filter_map(|(k, l)| l.as_ref().and_then(|l| l.at(px, py)).map(|i| (k, i))).take(cfg.max_cover.max(2)).collect();
            for (ai, &(a, ia)) in cover.iter().enumerate() {
                for &(b, ib) in &cover[ai + 1..] {
                    let (la, lb) = (layers[a].as_ref().unwrap(), layers[b].as_ref().unwrap());
                    let (va, vb) = (la.lin[ia], lb.lin[ib]);
                    let d = [0, 1, 2].map(|k| vb[k].max(1e-6).ln() - va[k].max(1e-6).ln());
                    let (ca, wa) = cell(la.pos[ia][0], la.pos[ia][1], grids[a].0, grids[a].1);
                    let (cb, wb) = cell(lb.pos[ib][0], lb.pos[ib][1], grids[b].0, grids[b].1);
                    obs.push(RawObs { a, b, ca, wa, cb, wb, d });
                }
            }
        }
    }
    obs
}

/// Thins `obs` evenly (deterministic) down to at most `max` entries.
fn thin(obs: Vec<RawObs>, max: usize) -> Vec<RawObs> {
    if obs.len() <= max.max(1) {
        return obs;
    }
    let step = obs.len().div_ceil(max.max(1));
    obs.into_iter().enumerate().filter(|(i, _)| i % step == 0).map(|(_, o)| o).collect()
}

/// Correction maps for photos that carry their global factor in `gain`. `view` is the panorama
/// view (its bounds define the small panorama the photos are compared on). A photo that has
/// fewer than `cfg.min_samples` usable comparisons gets None (it keeps its global factor).
pub fn compute_gain_maps(photos: &[RenderPhoto], view: &PanoView, cfg: &GainMapConfig) -> Vec<Option<GainMap>> {
    let n = photos.len();
    let none = || vec![None; n];
    if n < 2 || !cfg.enabled {
        return none();
    }
    let aspect = view.aspect().clamp(0.05, 50.0);
    let w = cfg.preview_width.max(16);
    let h = ((w as f64 / aspect).round() as usize).clamp(8, 1600);
    let m = Mapper::new(view, view.bounds, w, h);
    let lims = clip_limits_linear();
    let layers: Vec<Option<Layer>> = photos.par_iter().map(|p| build_layer(p, &m, w, h, cfg, lims)).collect();
    if crate::engine::cancelled() {
        return none();
    }
    if std::env::var("PW_GM_DEBUG").is_ok() {
        let ok: Vec<usize> = layers.iter().map(|l| l.as_ref().map_or(0, |l| l.ok.iter().filter(|&&v| v).count())).collect();
        eprintln!("gain maps: flat usable pixels per photo {ok:?}");
    }
    let mut obs = thin(collect_obs(&layers, photos, w, h, cfg), cfg.max_obs);
    drop(layers);
    // photos with too few comparisons drop out (and with them the comparisons they take part in)
    let mut active = vec![true; n];
    loop {
        let mut count = vec![0usize; n];
        for o in &obs {
            count[o.a] += 1;
            count[o.b] += 1;
        }
        let mut changed = false;
        for k in 0..n {
            if active[k] && count[k] < cfg.min_samples {
                active[k] = false;
                changed = true;
            }
        }
        if !changed {
            break;
        }
        obs.retain(|o| active[o.a] && active[o.b]);
    }
    if obs.is_empty() {
        return none();
    }
    // number the grid points of the photos that take part
    let mut first = vec![0usize; n];
    let mut total = 0;
    let mut links: Vec<(u32, u32)> = Vec::new();
    for k in (0..n).filter(|&k| active[k]) {
        let (gw, gh) = grid_size(photos[k].w, photos[k].h, cfg.grid_long);
        first[k] = total;
        for y in 0..gh {
            for x in 0..gw {
                let i = (total + y * gw + x) as u32;
                if x + 1 < gw {
                    links.push((i, i + 1));
                }
                if y + 1 < gh {
                    links.push((i, i + gw as u32));
                }
            }
        }
        total += gw * gh;
    }
    let sobs: Vec<Obs> = obs
        .iter()
        .map(|o| {
            let mut s = Obs { nodes: [0; 8], coef: [0.0; 8], d: o.d };
            for j in 0..4 {
                s.nodes[j] = (first[o.a] + o.ca[j]) as u32;
                s.coef[j] = o.wa[j];
                s.nodes[4 + j] = (first[o.b] + o.cb[j]) as u32;
                s.coef[4 + j] = -o.wb[j];
            }
            s
        })
        .collect();
    // data weight a grid point gets on average sets the unit of the smoothness
    let unit = (2 * sobs.len()) as f64 / total.max(1) as f64;
    let params = SolveParams { n: total, lam: cfg.smooth * unit, anchor: cfg.anchor * unit, huber: cfg.huber, irls_iters: cfg.irls_iters, cg_iters: cfg.cg_iters, cg_tol: cfg.cg_tol };
    let sol: Vec<Option<Vec<f64>>> = (0..3).into_par_iter().map(|ch| solve_channel(&sobs, &links, &params, ch)).collect();
    let (Some(x0), Some(x1), Some(x2)) = (sol[0].as_ref(), sol[1].as_ref(), sol[2].as_ref()) else { return none() };
    if std::env::var("PW_GM_DEBUG").is_ok() {
        report_pairs(&obs, &first, [x0, x1, x2], n);
    }
    let lim = cfg.max_dev;
    (0..n)
        .map(|k| {
            if !active[k] {
                return None;
            }
            let (gw, gh) = grid_size(photos[k].w, photos[k].h, cfg.grid_long);
            let d = (0..gw * gh).map(|i| [x0[first[k] + i], x1[first[k] + i], x2[first[k] + i]].map(|v| v.clamp(-lim, lim).exp() as f32)).collect();
            Some(GainMap { gw, gh, d })
        })
        .collect()
}

/// Experiments (`PW_GM_DEBUG=1`): per pair of photos, how well they agree in the overlap before and after the correction.
fn report_pairs(obs: &[RawObs], first: &[usize], x: [&Vec<f64>; 3], n: usize) {
    use std::collections::BTreeMap;
    // (count, sum of d, sum of d - prediction) per pair and channel
    let mut pairs: BTreeMap<(usize, usize), (usize, [f64; 3], [f64; 3])> = BTreeMap::new();
    let mut per_photo = vec![0usize; n];
    for o in obs {
        let e = pairs.entry((o.a.min(o.b), o.a.max(o.b))).or_default();
        let sign = if o.a < o.b { 1.0 } else { -1.0 };
        e.0 += 1;
        per_photo[o.a] += 1;
        per_photo[o.b] += 1;
        for c in 0..3 {
            let pred: f64 = (0..4).map(|j| o.wa[j] as f64 * x[c][first[o.a] + o.ca[j]] - o.wb[j] as f64 * x[c][first[o.b] + o.cb[j]]).sum();
            e.1[c] += sign * o.d[c] as f64;
            e.2[c] += sign * (o.d[c] as f64 - pred);
        }
    }
    let mut rows: Vec<_> = pairs.iter().map(|(k, v)| (*k, v.0, v.1.map(|s| s / v.0 as f64), v.2.map(|s| s / v.0 as f64))).collect();
    rows.sort_by(|a, b| b.3.iter().map(|v| v.abs()).fold(0.0, f64::max).partial_cmp(&a.3.iter().map(|v| v.abs()).fold(0.0, f64::max)).unwrap_or(std::cmp::Ordering::Equal));
    eprintln!("gain maps: comparisons per photo {per_photo:?}");
    eprintln!("gain maps: pair (a,b) n | mean ln(Ib/Ia) before RGB | after RGB   (worst first)");
    for ((a, b), cnt, before, after) in rows.iter().take(12) {
        eprintln!("  ({a:2},{b:2}) {cnt:5} | {:+.3} {:+.3} {:+.3} | {:+.3} {:+.3} {:+.3}", before[0], before[1], before[2], after[0], after[1], after[2]);
    }
}

#[cfg(test)]
#[path = "gainmap_tests.rs"]
mod tests;
