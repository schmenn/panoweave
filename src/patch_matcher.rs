//! PatchMatcher: finds the remaining shift (dx, dy) between two image regions that are already
//! warped into the same space, with sub-pixel accuracy and a confidence value.
//!
//! Like two overhead-projector sheets: a patch of A is slid over B inside a small search
//! window; at every position a similarity is computed; the best position is the shift.
//!
//! - metrics: ZNCC (default), SSD, SAD, gradient-ZNCC, phase correlation
//! - features: intensity, gradient magnitude, or both
//! - Gaussian pre-blur (matching only), masks for invalid pixels
//! - coarse-to-fine image pyramid, parabola sub-pixel fit
//! - validity checks: score, uniqueness, texture, forward-backward, mask coverage, search border
//!
//! All numbers come from [`MatchConfig`]; nothing is hard-coded in the algorithm.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Metric {
    Zncc,
    Ssd,
    Sad,
    GradientNcc,
    Phase,
}

impl Metric {
    pub const ALL: [Metric; 5] = [Metric::Zncc, Metric::Ssd, Metric::Sad, Metric::GradientNcc, Metric::Phase];
    pub fn name(&self) -> &'static str {
        match self {
            Metric::Zncc => "ZNCC (correlation)",
            Metric::Ssd => "SSD (squared differences)",
            Metric::Sad => "SAD (absolute differences)",
            Metric::GradientNcc => "Gradient ZNCC",
            Metric::Phase => "Phase correlation",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum FeatureSpace {
    Intensity,
    Gradient,
    Both,
}

impl FeatureSpace {
    pub const ALL: [FeatureSpace; 3] = [FeatureSpace::Intensity, FeatureSpace::Gradient, FeatureSpace::Both];
    pub fn name(&self) -> &'static str {
        match self {
            FeatureSpace::Intensity => "Brightness",
            FeatureSpace::Gradient => "Edges (gradient)",
            FeatureSpace::Both => "Both",
        }
    }
}

/// Texture threshold (mean squared gradient, grey 0..1 per pixel, after the pre-blur).
/// Determined on the person's photos and the synthetic tests: plain sky/walls stay below
/// ~5e-6, textured city/foliage patches are above ~1e-4.
pub const MIN_TEXTURE_DEFAULT: f32 = 2.0e-5;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MatchConfig {
    /// edge length of the patch in pixels
    pub patch_size: usize,
    /// largest shift searched in every direction (pixels, finest level)
    pub search_radius: f32,
    pub metric: Metric,
    pub feature_space: FeatureSpace,
    pub use_pyramid: bool,
    pub pyramid_levels: usize,
    /// minimum peak score (ZNCC-like, 1 = perfect)
    pub min_score: f32,
    /// best peak / second-best peak (outside a 5x5 neighbourhood)
    pub min_uniqueness: f32,
    pub min_texture: f32,
    /// forward-backward tolerance in pixels
    pub fb_tolerance: f32,
    pub forward_backward: bool,
    /// share of valid pixels a patch needs
    pub min_valid_fraction: f32,
    /// Gaussian pre-blur (matching only)
    pub blur_sigma: f32,
}

impl Default for MatchConfig {
    fn default() -> Self {
        MatchConfig {
            patch_size: 64,
            search_radius: 24.0,
            metric: Metric::Zncc,
            feature_space: FeatureSpace::Gradient,
            use_pyramid: true,
            pyramid_levels: 3,
            min_score: 0.6,
            min_uniqueness: 1.15,
            min_texture: MIN_TEXTURE_DEFAULT,
            fb_tolerance: 0.5,
            forward_backward: true,
            min_valid_fraction: 0.8,
            blur_sigma: 1.0,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MatchResult {
    pub dx: f32,
    pub dy: f32,
    pub score: f32,
    pub confidence: f32,
    pub valid: bool,
    pub reason: &'static str,
}

impl MatchResult {
    fn invalid(reason: &'static str) -> Self {
        MatchResult { reason, ..Default::default() }
    }
}

/// A square region (size x size) in the common space with a validity mask.
/// The patch centre is the middle of the region.
#[derive(Clone, Debug)]
pub struct Region {
    pub size: usize,
    pub data: Vec<f32>,
    pub mask: Vec<bool>,
}

impl Region {
    pub fn new(size: usize) -> Self {
        Region { size, data: vec![0.0; size * size], mask: vec![false; size * size] }
    }
    /// Region size needed for a configuration (patch + search + a small margin for sub-pixel).
    pub fn size_for(cfg: &MatchConfig) -> usize {
        let s = cfg.patch_size + 2 * cfg.search_radius.ceil() as usize + 6;
        s | 1
    }
}

// ---------------------------------------------------------------------------------------
// preprocessing

/// Feature planes of one region (one or two channels) plus mask.
#[derive(Clone)]
struct Feat {
    s: usize,
    ch: Vec<Vec<f32>>,
    mask: Vec<bool>,
}

fn masked_blur(v: &[f32], m: &[bool], s: usize, sigma: f32) -> Vec<f32> {
    if sigma <= 0.01 {
        return v.to_vec();
    }
    let r = (sigma * 3.0).ceil() as isize;
    let k: Vec<f32> = (-r..=r).map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let pass = |src: &[f32], wsrc: &[f32], horiz: bool| -> (Vec<f32>, Vec<f32>) {
        let mut o = vec![0.0; s * s];
        let mut ow = vec![0.0; s * s];
        for y in 0..s {
            for x in 0..s {
                let mut a = 0.0;
                let mut w = 0.0;
                for (t, kv) in k.iter().enumerate() {
                    let d = t as isize - r;
                    let (xx, yy) = if horiz { (x as isize + d, y as isize) } else { (x as isize, y as isize + d) };
                    if xx < 0 || yy < 0 || xx >= s as isize || yy >= s as isize {
                        continue;
                    }
                    let i = yy as usize * s + xx as usize;
                    a += kv * src[i];
                    w += kv * wsrc[i];
                }
                o[y * s + x] = a;
                ow[y * s + x] = w;
            }
        }
        (o, ow)
    };
    let wv: Vec<f32> = m.iter().map(|&b| if b { 1.0 } else { 0.0 }).collect();
    let vv: Vec<f32> = v.iter().zip(&wv).map(|(a, b)| a * b).collect();
    let (h, hw) = pass(&vv, &wv, true);
    let (o, ow) = pass(&h, &hw, false);
    o.iter().zip(&ow).map(|(a, w)| if *w > 1e-6 { a / w } else { 0.0 }).collect()
}

fn gradient(v: &[f32], m: &[bool], s: usize) -> (Vec<f32>, Vec<bool>) {
    let mut g = vec![0.0; s * s];
    let mut gm = vec![false; s * s];
    for y in 1..s.saturating_sub(1) {
        for x in 1..s - 1 {
            let i = y * s + x;
            let ok = m[i] && m[i - 1] && m[i + 1] && m[i - s] && m[i + s] && m[i - s - 1] && m[i - s + 1] && m[i + s - 1] && m[i + s + 1];
            if !ok {
                continue;
            }
            let p = |dx: isize, dy: isize| v[(y as isize + dy) as usize * s + (x as isize + dx) as usize];
            let gx = ((p(1, -1) + 2.0 * p(1, 0) + p(1, 1)) - (p(-1, -1) + 2.0 * p(-1, 0) + p(-1, 1))) / 8.0;
            let gy = ((p(-1, 1) + 2.0 * p(0, 1) + p(1, 1)) - (p(-1, -1) + 2.0 * p(0, -1) + p(1, -1))) / 8.0;
            g[i] = (gx * gx + gy * gy).sqrt();
            gm[i] = true;
        }
    }
    (g, gm)
}

fn features(r: &Region, cfg: &MatchConfig, force_gradient: bool) -> Feat {
    let s = r.size;
    let b = masked_blur(&r.data, &r.mask, s, cfg.blur_sigma);
    let fs = if force_gradient { FeatureSpace::Gradient } else { cfg.feature_space };
    match fs {
        FeatureSpace::Intensity => Feat { s, ch: vec![b], mask: r.mask.clone() },
        FeatureSpace::Gradient => {
            let (g, gm) = gradient(&b, &r.mask, s);
            Feat { s, ch: vec![g], mask: gm }
        }
        FeatureSpace::Both => {
            let (g, gm) = gradient(&b, &r.mask, s);
            Feat { s, ch: vec![b, g], mask: gm }
        }
    }
}

/// Quick check before the (more expensive) search: does the template have enough texture?
pub fn has_texture(r: &Region, cfg: &MatchConfig) -> bool {
    texture(r, cfg) >= cfg.min_texture
}

/// Mean squared gradient of the template (texture measure).
fn texture(r: &Region, cfg: &MatchConfig) -> f32 {
    let s = r.size;
    let b = masked_blur(&r.data, &r.mask, s, cfg.blur_sigma);
    let (g, gm) = gradient(&b, &r.mask, s);
    let c = s / 2;
    let h = cfg.patch_size / 2;
    let mut sum = 0.0;
    let mut n = 0;
    for y in c.saturating_sub(h)..(c + h).min(s) {
        for x in c.saturating_sub(h)..(c + h).min(s) {
            let i = y * s + x;
            if gm[i] {
                sum += g[i] * g[i];
                n += 1;
            }
        }
    }
    if n == 0 { 0.0 } else { sum / n as f32 }
}

fn down(f: &Feat) -> Feat {
    let s2 = f.s / 2;
    let mut ch = vec![vec![0.0; s2 * s2]; f.ch.len()];
    let mut mask = vec![false; s2 * s2];
    for y in 0..s2 {
        for x in 0..s2 {
            let idx = [(2 * y) * f.s + 2 * x, (2 * y) * f.s + 2 * x + 1, (2 * y + 1) * f.s + 2 * x, (2 * y + 1) * f.s + 2 * x + 1];
            let valid: Vec<usize> = idx.iter().copied().filter(|&i| f.mask[i]).collect();
            if valid.len() >= 3 {
                mask[y * s2 + x] = true;
                for c in 0..f.ch.len() {
                    ch[c][y * s2 + x] = valid.iter().map(|&i| f.ch[c][i]).sum::<f32>() / valid.len() as f32;
                }
            }
        }
    }
    Feat { s: s2, ch, mask }
}

// ---------------------------------------------------------------------------------------
// scores

/// Similarity of the template of `a` (centre `ca`, half size `h`) with `b` shifted by (ox, oy).
/// Higher is better for every metric. None if too few pixels are valid.
fn score_at(a: &Feat, b: &Feat, ca: isize, h: isize, ox: isize, oy: isize, metric: Metric, min_frac: f32) -> Option<f32> {
    let s = a.s as isize;
    let total = ((2 * h + 1) * (2 * h + 1)) as f32;
    // the window that lies inside both regions
    let x0 = (-h).max(-ca).max(-ca - ox);
    let x1 = h.min(s - 1 - ca).min(s - 1 - ca - ox);
    let y0 = (-h).max(-ca).max(-ca - oy);
    let y1 = h.min(s - 1 - ca).min(s - 1 - ca - oy);
    if x0 > x1 || y0 > y1 {
        return None;
    }
    let mut sum = 0.0;
    for c in 0..a.ch.len() {
        let (ach, bch) = (&a.ch[c], &b.ch[c]);
        // pass 1: sums
        let (mut n, mut sa, mut sb, mut saa, mut sbb, mut sab) = (0f32, 0f32, 0f32, 0f32, 0f32, 0f32);
        for dy in y0..=y1 {
            let ra = ((ca + dy) * s + ca) as usize;
            let rb = ((ca + dy + oy) * s + ca + ox) as usize;
            for dx in x0..=x1 {
                let ia = (ra as isize + dx) as usize;
                let ib = (rb as isize + dx) as usize;
                if !a.mask[ia] || !b.mask[ib] {
                    continue;
                }
                let (va, vb) = (ach[ia], bch[ib]);
                n += 1.0;
                sa += va;
                sb += vb;
                saa += va * va;
                sbb += vb * vb;
                sab += va * vb;
            }
        }
        if n < min_frac * total || n < 9.0 {
            return None;
        }
        let (ma, mb) = (sa / n, sb / n);
        let va = (saa - n * ma * ma).max(0.0);
        let vb = (sbb - n * mb * mb).max(0.0);
        let cov = sab - n * ma * mb;
        let v = match metric {
            Metric::Zncc | Metric::GradientNcc | Metric::Phase => cov / ((va * vb).sqrt() + 1e-12),
            Metric::Ssd => {
                // sum((x - y)^2) of the zero-mean values = va + vb - 2 cov
                1.0 - (va + vb - 2.0 * cov) / (va + vb + 1e-12)
            }
            Metric::Sad => {
                // pass 2 needed for absolute values
                let (mut d, mut e) = (0.0f32, 0.0f32);
                for dy in y0..=y1 {
                    let ra = ((ca + dy) * s + ca) as usize;
                    let rb = ((ca + dy + oy) * s + ca + ox) as usize;
                    for dx in x0..=x1 {
                        let ia = (ra as isize + dx) as usize;
                        let ib = (rb as isize + dx) as usize;
                        if !a.mask[ia] || !b.mask[ib] {
                            continue;
                        }
                        let (x, y) = (ach[ia] - ma, bch[ib] - mb);
                        d += (x - y).abs();
                        e += x.abs() + y.abs();
                    }
                }
                1.0 - d / (e + 1e-12)
            }
        };
        sum += v;
    }
    Some(sum / a.ch.len() as f32)
}

/// Like `score_at` but with a fractional shift (b sampled bilinearly).
fn score_at_f(a: &Feat, b: &Feat, ca: isize, h: isize, ox: f32, oy: f32, metric: Metric, min_frac: f32) -> Option<f32> {
    let s = b.s;
    let fx = ox.floor();
    let fy = oy.floor();
    let tx = ox - fx;
    let ty = oy - fy;
    // shifted copy of b by the fractional part, then integer scoring
    let mut nb = Feat { s, ch: vec![vec![0.0; s * s]; b.ch.len()], mask: vec![false; s * s] };
    for y in 0..s.saturating_sub(1) {
        for x in 0..s - 1 {
            let i = y * s + x;
            if !(b.mask[i] && b.mask[i + 1] && b.mask[i + s] && b.mask[i + s + 1]) {
                continue;
            }
            nb.mask[i] = true;
            for c in 0..b.ch.len() {
                let v = &b.ch[c];
                nb.ch[c][i] = (v[i] * (1.0 - tx) + v[i + 1] * tx) * (1.0 - ty) + (v[i + s] * (1.0 - tx) + v[i + s + 1] * tx) * ty;
            }
        }
    }
    score_at(a, &nb, ca, h, fx as isize, fy as isize, metric, min_frac)
}

fn parabola(l: f32, c: f32, r: f32) -> f32 {
    let den = 2.0 * c - l - r;
    if den > 1e-9 { (0.5 * (r - l) / den).clamp(-0.5, 0.5) } else { 0.0 }
}

// ---------------------------------------------------------------------------------------
// phase correlation (small radix-2 FFT)

fn fft(re: &mut [f32], im: &mut [f32], inverse: bool) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = 2.0 * std::f32::consts::PI / len as f32 * if inverse { 1.0 } else { -1.0 };
        let (wr, wi) = (ang.cos(), ang.sin());
        for i in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (ur, ui) = (re[i + k], im[i + k]);
                let (vr, vi) = (re[i + k + len / 2] * cr - im[i + k + len / 2] * ci, re[i + k + len / 2] * ci + im[i + k + len / 2] * cr);
                re[i + k] = ur + vr;
                im[i + k] = ui + vi;
                re[i + k + len / 2] = ur - vr;
                im[i + k + len / 2] = ui - vi;
                let nr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = nr;
            }
        }
        len <<= 1;
    }
    if inverse {
        for v in re.iter_mut() {
            *v /= n as f32;
        }
        for v in im.iter_mut() {
            *v /= n as f32;
        }
    }
}

fn fft2(re: &mut [f32], im: &mut [f32], n: usize, inverse: bool) {
    let mut rr = vec![0.0; n];
    let mut ii = vec![0.0; n];
    for y in 0..n {
        rr.copy_from_slice(&re[y * n..(y + 1) * n]);
        ii.copy_from_slice(&im[y * n..(y + 1) * n]);
        fft(&mut rr, &mut ii, inverse);
        re[y * n..(y + 1) * n].copy_from_slice(&rr);
        im[y * n..(y + 1) * n].copy_from_slice(&ii);
    }
    for x in 0..n {
        for y in 0..n {
            rr[y] = re[y * n + x];
            ii[y] = im[y * n + x];
        }
        fft(&mut rr, &mut ii, inverse);
        for y in 0..n {
            re[y * n + x] = rr[y];
            im[y * n + x] = ii[y];
        }
    }
}

/// Shift of b against a by phase correlation on the central n x n window (Hann window).
/// Returns (dx, dy, peak, second peak).
fn phase_shift(a: &Feat, b: &Feat, n: usize) -> Option<(f32, f32, f32, f32)> {
    let s = a.s;
    if n > s {
        return None;
    }
    let off = (s - n) / 2;
    let hann = |i: usize| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (n as f32 - 1.0)).cos();
    let take = |f: &Feat| -> (Vec<f32>, Vec<f32>) {
        let mut re = vec![0.0; n * n];
        let mut mean = 0.0;
        let mut cnt = 0.0;
        for y in 0..n {
            for x in 0..n {
                let i = (y + off) * s + x + off;
                if f.mask[i] {
                    mean += f.ch[0][i];
                    cnt += 1.0;
                }
            }
        }
        let mean = if cnt > 0.0 { mean / cnt } else { 0.0 };
        for y in 0..n {
            for x in 0..n {
                let i = (y + off) * s + x + off;
                let v = if f.mask[i] { f.ch[0][i] - mean } else { 0.0 };
                re[y * n + x] = v * hann(x) * hann(y);
            }
        }
        (re, vec![0.0; n * n])
    };
    let (mut ar, mut ai) = take(a);
    let (mut br, mut bi) = take(b);
    fft2(&mut ar, &mut ai, n, false);
    fft2(&mut br, &mut bi, n, false);
    let mut cr = vec![0.0; n * n];
    let mut ci = vec![0.0; n * n];
    for i in 0..n * n {
        // B * conj(A): peak at the shift of B relative to A
        let r = br[i] * ar[i] + bi[i] * ai[i];
        let im = bi[i] * ar[i] - br[i] * ai[i];
        let m = (r * r + im * im).sqrt() + 1e-12;
        cr[i] = r / m;
        ci[i] = im / m;
    }
    fft2(&mut cr, &mut ci, n, true);
    let at = |x: isize, y: isize| cr[(y.rem_euclid(n as isize) as usize) * n + x.rem_euclid(n as isize) as usize];
    let mut best = (f32::MIN, 0isize, 0isize);
    for y in 0..n as isize {
        for x in 0..n as isize {
            let v = at(x, y);
            if v > best.0 {
                best = (v, x, y);
            }
        }
    }
    let (p, mut bx, mut by) = best;
    if bx > n as isize / 2 {
        bx -= n as isize;
    }
    if by > n as isize / 2 {
        by -= n as isize;
    }
    let mut second = f32::MIN;
    for y in 0..n as isize {
        for x in 0..n as isize {
            let (mut sx, mut sy) = (x, y);
            if sx > n as isize / 2 {
                sx -= n as isize;
            }
            if sy > n as isize / 2 {
                sy -= n as isize;
            }
            if (sx - bx).abs() > 2 || (sy - by).abs() > 2 {
                second = second.max(at(x, y));
            }
        }
    }
    let ox = parabola(at(bx - 1, by), p, at(bx + 1, by));
    let oy = parabola(at(bx, by - 1), p, at(bx, by + 1));
    Some((bx as f32 + ox, by as f32 + oy, p, second))
}

// ---------------------------------------------------------------------------------------
// search

struct Search {
    dx: f32,
    dy: f32,
    score: f32,
    uniqueness: f32,
    at_border: bool,
}

/// What a search tested, for the patch comparison window. Only filled when a trace is passed;
/// collecting it never changes the result. `tested` holds every integer offset whose score was
/// computed, as (level, ox, oy, score) in level units (level l: 1 unit = 2^l pixels), in the
/// order the search computed them (coarse full scan, level-1 uniqueness scan, 5x5 refinement
/// windows per level).
#[derive(Default)]
pub struct SearchTrace {
    pub tested: Vec<(u8, i16, i16, f32)>,
    /// pyramid levels used below the finest (0 = no pyramid)
    pub levels: usize,
    /// search radius per level (level units), index = level
    pub radius: Vec<isize>,
    /// feature images of every level (only when `want_images`; for the live preview)
    pub want_images: bool,
    pub images: Vec<LevelImages>,
}

/// Feature images of one pyramid level (first channel), for the live preview.
#[derive(Clone)]
pub struct LevelImages {
    pub size: usize,
    /// half size of the template on this level
    pub half: isize,
    pub radius: isize,
    pub a: Vec<f32>,
    pub b: Vec<f32>,
    pub ma: Vec<bool>,
    pub mb: Vec<bool>,
}

/// Extra numbers of one match (for the patch comparison window).
#[derive(Clone, Copy, Debug, Default)]
pub struct MatchDetail {
    /// best / second-best peak (infinite when there is no second peak)
    pub uniqueness: f32,
    /// mean squared gradient of the template
    pub texture: f32,
}

fn search(a: &Region, b: &Region, cfg: &MatchConfig, mut tr: Option<&mut SearchTrace>) -> Result<Search, &'static str> {
    let force_grad = cfg.metric == Metric::GradientNcc;
    let fa0 = features(a, cfg, force_grad);
    let fb0 = features(b, cfg, force_grad);
    let r0 = cfg.search_radius.max(1.0);
    let h0 = (cfg.patch_size / 2) as isize;
    // pyramid
    let mut levels = if cfg.use_pyramid { cfg.pyramid_levels } else { 0 };
    let fits = |l: usize| {
        let sl = (a.size >> l) as isize;
        let hl = (h0 >> l).max(2);
        let rl = (r0 / (1 << l) as f32).ceil() as isize;
        (h0 >> l) >= 4 && sl / 2 >= hl + rl + 1
    };
    while levels > 0 && !fits(levels) {
        levels -= 1;
    }
    let mut pa = vec![fa0];
    let mut pb = vec![fb0];
    for _ in 0..levels {
        let na = down(pa.last().unwrap());
        let nb = down(pb.last().unwrap());
        pa.push(na);
        pb.push(nb);
    }
    let metric = if cfg.metric == Metric::Phase { Metric::Zncc } else { cfg.metric };
    let min_frac = cfg.min_valid_fraction;
    if let Some(t) = tr.as_deref_mut() {
        t.levels = levels;
        t.radius = (0..=levels).map(|l| (r0 / (1 << l) as f32).ceil() as isize).collect();
        if t.want_images {
            t.images = (0..=levels)
                .map(|l| LevelImages {
                    size: pa[l].s,
                    half: (h0 >> l).max(2),
                    radius: (r0 / (1 << l) as f32).ceil() as isize,
                    a: pa[l].ch.last().cloned().unwrap_or_default(),
                    b: pb[l].ch.last().cloned().unwrap_or_default(),
                    ma: pa[l].mask.clone(),
                    mb: pb[l].mask.clone(),
                })
                .collect();
        }
    }
    // coarsest level: full search (also gives the uniqueness)
    let lc = levels;
    let (fa, fb) = (&pa[lc], &pb[lc]);
    let ca = (fa.s / 2) as isize;
    let hc = (h0 >> lc).max(2);
    let rc = (r0 / (1 << lc) as f32).ceil() as isize;
    let mut map: Vec<(isize, isize, f32)> = Vec::new();
    let mut start: Option<(isize, isize)> = None;
    if cfg.metric == Metric::Phase {
        // phase correlation gives the start directly
        let n = (fa0_pow2(fa.s)).min(fa.s);
        if let Some((dx, dy, _, _)) = phase_shift(fa, fb, n) {
            if dx.abs() <= rc as f32 + 0.5 && dy.abs() <= rc as f32 + 0.5 {
                start = Some((dx.round() as isize, dy.round() as isize));
            }
        }
    }
    for oy in -rc..=rc {
        for ox in -rc..=rc {
            if let Some(v) = score_at(fa, fb, ca, hc, ox, oy, metric, min_frac) {
                map.push((ox, oy, v));
                if let Some(t) = tr.as_deref_mut() {
                    t.tested.push((lc as u8, ox as i16, oy as i16, v));
                }
            }
        }
    }
    if map.is_empty() {
        return Err("too few valid pixels");
    }
    let best_c = map.iter().cloned().fold((0, 0, f32::MIN), |m, v| if v.2 > m.2 { v } else { m });
    let second = map
        .iter()
        .filter(|v| (v.0 - best_c.0).abs() > 2 || (v.1 - best_c.1).abs() > 2)
        .map(|v| v.2)
        .fold(f32::MIN, f32::max);
    let mut uniqueness = if second <= 1e-3 { f32::INFINITY } else { best_c.2 / second };
    // a finer, more reliable uniqueness test on level 1 (or 0) over the whole search area
    {
        let lu = lc.min(1);
        if lu < lc {
            let (fa, fb) = (&pa[lu], &pb[lu]);
            let ca = (fa.s / 2) as isize;
            let h = (h0 >> lu).max(2);
            let r = ((r0 / (1 << lu) as f32).ceil() as isize).min(8);
            let mut m: Vec<(isize, isize, f32)> = Vec::new();
            for oy in -r..=r {
                for ox in -r..=r {
                    if let Some(v) = score_at(fa, fb, ca, h, ox, oy, metric, min_frac) {
                        m.push((ox, oy, v));
                        if let Some(t) = tr.as_deref_mut() {
                            t.tested.push((lu as u8, ox as i16, oy as i16, v));
                        }
                    }
                }
            }
            if let Some(b1) = m.iter().cloned().reduce(|x, y| if y.2 > x.2 { y } else { x }) {
                let s2 = m.iter().filter(|v| (v.0 - b1.0).abs() > 2 || (v.1 - b1.1).abs() > 2).map(|v| v.2).fold(f32::MIN, f32::max);
                uniqueness = if s2 <= 1e-3 { f32::INFINITY } else { b1.2 / s2 };
            }
        }
    }
    // the phase-correlation start is used only if it is about as good as the exhaustive best
    let start = start.filter(|&(x, y)| score_at(fa, fb, ca, hc, x, y, metric, min_frac).map(|v| v >= best_c.2 - 0.05).unwrap_or(false));
    let (mut bx, mut by) = start.unwrap_or((best_c.0, best_c.1));
    // refine level by level
    for l in (0..lc).rev() {
        bx *= 2;
        by *= 2;
        let (fa, fb) = (&pa[l], &pb[l]);
        let ca = (fa.s / 2) as isize;
        let h = (h0 >> l).max(2);
        let rl = (r0 / (1 << l) as f32).ceil() as isize;
        let mut best = (bx, by, f32::MIN);
        for oy in by - 2..=by + 2 {
            for ox in bx - 2..=bx + 2 {
                if ox.abs() > rl || oy.abs() > rl {
                    continue;
                }
                if let Some(v) = score_at(fa, fb, ca, h, ox, oy, metric, min_frac) {
                    if let Some(t) = tr.as_deref_mut() {
                        t.tested.push((l as u8, ox as i16, oy as i16, v));
                    }
                    if v > best.2 {
                        best = (ox, oy, v);
                    }
                }
            }
        }
        if best.2 == f32::MIN {
            return Err("too few valid pixels");
        }
        bx = best.0;
        by = best.1;
    }
    // level 0: sub-pixel
    let (fa, fb) = (&pa[0], &pb[0]);
    let ca = (fa.s / 2) as isize;
    let r0i = r0.ceil() as isize;
    let sc = |ox: isize, oy: isize| score_at(fa, fb, ca, h0, ox, oy, metric, min_frac);
    let c = sc(bx, by).ok_or("too few valid pixels")?;
    if let Some(t) = tr.as_deref_mut() {
        t.tested.push((0, bx as i16, by as i16, c));
    }
    let at_border = bx.abs() >= r0i || by.abs() >= r0i;
    let (mut dx, mut dy) = (bx as f32, by as f32);
    let mut best_score = c;
    if !at_border {
        if let (Some(l), Some(r)) = (sc(bx - 1, by), sc(bx + 1, by)) {
            dx += parabola(l, c, r);
        }
        if let (Some(u), Some(d)) = (sc(bx, by - 1), sc(bx, by + 1)) {
            dy += parabola(u, c, d);
        }
        // finer parabola fits with fractional shifts
        let scf = |x: f32, y: f32| score_at_f(fa, fb, ca, h0, x, y, metric, min_frac);
        for step in [0.25f32, 0.1] {
            let Some(cc) = scf(dx, dy) else { break };
            best_score = best_score.max(cc);
            if let (Some(l), Some(r)) = (scf(dx - step, dy), scf(dx + step, dy)) {
                dx += parabola(l, cc, r) * 2.0 * step;
            }
            let Some(cc) = scf(dx, dy) else { break };
            if let (Some(u), Some(d)) = (scf(dx, dy - step), scf(dx, dy + step)) {
                dy += parabola(u, cc, d) * 2.0 * step;
            }
        }
    }
    Ok(Search { dx, dy, score: best_score, uniqueness, at_border })
}

fn fa0_pow2(s: usize) -> usize {
    let mut n = 1;
    while n * 2 <= s {
        n *= 2;
    }
    n
}

/// Shift a region's content by an integer amount (for the backward check).
fn shifted(r: &Region, sx: isize, sy: isize) -> Region {
    let s = r.size;
    let mut o = Region::new(s);
    for y in 0..s as isize {
        for x in 0..s as isize {
            let (xx, yy) = (x + sx, y + sy);
            if xx < 0 || yy < 0 || xx >= s as isize || yy >= s as isize {
                continue;
            }
            let i = (y as usize) * s + x as usize;
            let j = (yy as usize) * s + xx as usize;
            o.data[i] = r.data[j];
            o.mask[i] = r.mask[j];
        }
    }
    o
}

/// Match the centre patch of `a` inside `b` (both regions of the same size, same space).
pub fn match_patch(a: &Region, b: &Region, cfg: &MatchConfig) -> MatchResult {
    match_patch_detailed(a, b, cfg, None).0
}

/// Like [`match_patch`], plus uniqueness and texture, and (optionally) what the forward search
/// tested. The result is exactly the same as `match_patch`.
pub fn match_patch_detailed(a: &Region, b: &Region, cfg: &MatchConfig, trace: Option<&mut SearchTrace>) -> (MatchResult, MatchDetail) {
    let mut detail = MatchDetail::default();
    let r = match_inner(a, b, cfg, trace, &mut detail);
    (r, detail)
}

fn match_inner(a: &Region, b: &Region, cfg: &MatchConfig, trace: Option<&mut SearchTrace>, detail: &mut MatchDetail) -> MatchResult {
    if a.size != b.size || a.size < cfg.patch_size + 2 {
        return MatchResult::invalid("region smaller than the patch");
    }
    // mask coverage of the template
    let s = a.size;
    let c = s / 2;
    let h = cfg.patch_size / 2;
    let (mut ok, mut tot) = (0usize, 0usize);
    for y in c - h..(c + h).min(s) {
        for x in c - h..(c + h).min(s) {
            tot += 1;
            if a.mask[y * s + x] {
                ok += 1;
            }
        }
    }
    if (ok as f32) < cfg.min_valid_fraction * tot as f32 {
        return MatchResult::invalid("patch outside the photo");
    }
    let tex = texture(a, cfg);
    detail.texture = tex;
    if tex < cfg.min_texture {
        return MatchResult::invalid("too little texture");
    }
    let fwd = match search(a, b, cfg, trace) {
        Ok(f) => f,
        Err(e) => return MatchResult::invalid(e),
    };
    detail.uniqueness = fwd.uniqueness;
    let mut res = MatchResult { dx: fwd.dx, dy: fwd.dy, score: fwd.score, ..Default::default() };
    if fwd.at_border {
        res.reason = "best match at the edge of the search area";
        return res;
    }
    if fwd.score < cfg.min_score {
        res.reason = "similarity too low";
        return res;
    }
    if fwd.uniqueness < cfg.min_uniqueness {
        res.reason = "ambiguous (repeating pattern)";
        return res;
    }
    if cfg.forward_backward {
        // B's patch at the found spot, searched back in A: must come back to the start
        let (rx, ry) = (fwd.dx.round() as isize, fwd.dy.round() as isize);
        let b_centered = shifted(b, rx, ry);
        match search(&b_centered, a, cfg, None) {
            Ok(back) => {
                // back shift in the frame centred on B's spot: expected -(fwd - round)
                let ex = fwd.dx - rx as f32;
                let ey = fwd.dy - ry as f32;
                let err = ((back.dx + ex).powi(2) + (back.dy + ey).powi(2)).sqrt();
                if err > cfg.fb_tolerance {
                    res.reason = "forward-backward check failed";
                    return res;
                }
            }
            Err(_) => {
                res.reason = "forward-backward check failed";
                return res;
            }
        }
    }
    let cs = ((fwd.score - cfg.min_score) / (1.0 - cfg.min_score).max(1e-3)).clamp(0.0, 1.0);
    let cu = if fwd.uniqueness.is_infinite() { 1.0 } else { ((fwd.uniqueness - 1.0) / (2.0 * (cfg.min_uniqueness - 1.0).max(1e-3))).clamp(0.0, 1.0) };
    let ct = (tex / (4.0 * cfg.min_texture.max(1e-12))).clamp(0.0, 1.0);
    res.confidence = (cs * (0.5 + 0.5 * cu) * (0.5 + 0.5 * ct)).clamp(0.0, 1.0);
    res.valid = true;
    res.reason = "ok";
    res
}

// ---------------------------------------------------------------------------------------
// helpers for the patch comparison window (display only, never used by the matching)

/// Texture measure of a region's centre patch (same as the matcher's check).
pub fn texture_of(r: &Region, cfg: &MatchConfig) -> f32 {
    texture(r, cfg)
}

/// The matching features of a region (after pre-blur): brightness or gradient magnitude.
/// With `FeatureSpace::Both` the gradient channel is returned.
pub fn feature_plane(r: &Region, cfg: &MatchConfig) -> (Vec<f32>, Vec<bool>) {
    let f = features(r, cfg, cfg.metric == Metric::GradientNcc);
    (f.ch.last().cloned().unwrap_or_default(), f.mask)
}

/// The full score field on the finest level for every integer shift within +-radius
/// (row-major, (2 radius + 1)^2 values, NaN where too few pixels are valid). Expensive
/// (one score per shift), so the window computes it on demand in a background thread.
pub fn full_score_map(a: &Region, b: &Region, cfg: &MatchConfig, radius: isize) -> Vec<f32> {
    let force_grad = cfg.metric == Metric::GradientNcc;
    let fa = features(a, cfg, force_grad);
    let fb = features(b, cfg, force_grad);
    let metric = if cfg.metric == Metric::Phase { Metric::Zncc } else { cfg.metric };
    let ca = (a.size / 2) as isize;
    let h = (cfg.patch_size / 2) as isize;
    let n = (2 * radius + 1) as usize;
    let mut out = vec![f32::NAN; n * n];
    for oy in -radius..=radius {
        for ox in -radius..=radius {
            if let Some(v) = score_at(&fa, &fb, ca, h, ox, oy, metric, cfg.min_valid_fraction) {
                out[(oy + radius) as usize * n + (ox + radius) as usize] = v;
            }
        }
    }
    out
}

/// Score of the template at a fractional shift on the finest level (for shifts made by hand).
pub fn score_at_shift(a: &Region, b: &Region, cfg: &MatchConfig, dx: f32, dy: f32) -> Option<f32> {
    let force_grad = cfg.metric == Metric::GradientNcc;
    let fa = features(a, cfg, force_grad);
    let fb = features(b, cfg, force_grad);
    let metric = if cfg.metric == Metric::Phase { Metric::Zncc } else { cfg.metric };
    let ca = (a.size / 2) as isize;
    let h = (cfg.patch_size / 2) as isize;
    score_at_f(&fa, &fb, ca, h, dx, dy, metric, cfg.min_valid_fraction)
}

// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// smooth random texture
    fn texture_value(x: f32, y: f32) -> f32 {
        let mut v = 0.0;
        let freqs = [(0.11, 0.07, 1.3), (0.05, 0.13, 0.4), (0.23, 0.19, 2.1), (0.17, 0.04, 0.9), (0.08, 0.21, 2.7), (0.31, 0.27, 0.2)];
        for (fx, fy, ph) in freqs {
            v += ((x * fx + y * fy) * 1.7 + ph).sin() * ((x * fy - y * fx) * 1.3 + ph * 0.5).cos();
        }
        0.5 + 0.08 * v
    }

    fn region(shift: (f32, f32), f: &dyn Fn(f32, f32) -> f32) -> Region {
        let cfg = MatchConfig::default();
        let s = Region::size_for(&cfg);
        let mut r = Region::new(s);
        for y in 0..s {
            for x in 0..s {
                // content of B at (x, y) equals A at (x - dx, y - dy): B is A shifted by +shift
                r.data[y * s + x] = f(x as f32 - shift.0, y as f32 - shift.1);
                r.mask[y * s + x] = true;
            }
        }
        r
    }

    #[test]
    fn detailed_match_is_identical() {
        let cfg = MatchConfig::default();
        let a = region((0.0, 0.0), &texture_value);
        let b = region((3.3, -2.6), &texture_value);
        let plain = match_patch(&a, &b, &cfg);
        let mut t = SearchTrace { want_images: true, ..Default::default() };
        let (det, d) = match_patch_detailed(&a, &b, &cfg, Some(&mut t));
        assert_eq!(plain, det);
        assert!(!t.tested.is_empty() && t.images.len() == t.levels + 1);
        assert!(d.texture > 0.0);
        let s = score_at_shift(&a, &b, &cfg, det.dx, det.dy).unwrap();
        assert!(s > 0.9);
    }

    #[test]
    fn known_subpixel_shift() {
        let a = region((0.0, 0.0), &texture_value);
        let b = region((3.4, -2.7), &texture_value);
        let r = match_patch(&a, &b, &MatchConfig::default());
        assert!(r.valid, "{:?}", r);
        assert!((r.dx - 3.4).abs() < 0.1 && (r.dy + 2.7).abs() < 0.1, "{:?}", r);
        for m in [Metric::Zncc, Metric::Ssd, Metric::Sad, Metric::GradientNcc, Metric::Phase] {
            let cfg = MatchConfig { metric: m, feature_space: FeatureSpace::Intensity, ..Default::default() };
            let r = match_patch(&a, &b, &cfg);
            assert!(r.valid, "{m:?} {:?}", r);
            assert!((r.dx - 3.4).abs() < 0.15 && (r.dy + 2.7).abs() < 0.15, "{m:?} {:?}", r);
        }
    }

    #[test]
    fn with_noise() {
        let a = region((0.0, 0.0), &texture_value);
        let mut b = region((-5.3, 1.8), &texture_value);
        let mut seed = 12345u64;
        for v in b.data.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let u1 = ((seed >> 11) as f32 / (1u64 << 53) as f32).max(1e-6);
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let u2 = (seed >> 11) as f32 / (1u64 << 53) as f32;
            *v += 0.01 * (-2.0 * u1.ln()).sqrt() * (6.2831 * u2).cos();
        }
        let r = match_patch(&a, &b, &MatchConfig::default());
        assert!(r.valid, "{:?}", r);
        assert!((r.dx + 5.3).abs() < 0.25 && (r.dy - 1.8).abs() < 0.25, "{:?}", r);
    }

    #[test]
    fn with_brightness_change() {
        let a = region((0.0, 0.0), &texture_value);
        let b = region((2.2, 4.6), &|x, y| texture_value(x, y) * 1.2 + 10.0 / 255.0);
        for (m, fs) in [(Metric::Zncc, FeatureSpace::Intensity), (Metric::GradientNcc, FeatureSpace::Gradient), (Metric::Zncc, FeatureSpace::Gradient)] {
            let cfg = MatchConfig { metric: m, feature_space: fs, ..Default::default() };
            let r = match_patch(&a, &b, &cfg);
            assert!(r.valid, "{m:?} {:?}", r);
            assert!((r.dx - 2.2).abs() < 0.15 && (r.dy - 4.6).abs() < 0.15, "{m:?} {:?}", r);
        }
    }

    #[test]
    fn flat_area_is_invalid() {
        let a = region((0.0, 0.0), &|_, _| 0.5);
        let b = region((1.0, 1.0), &|_, _| 0.5);
        let r = match_patch(&a, &b, &MatchConfig::default());
        assert!(!r.valid);
        assert_eq!(r.reason, "too little texture");
    }

    #[test]
    fn repeating_pattern_is_rejected() {
        // vertical stripes with a 9 px period: many equally good positions
        let f = |x: f32, _y: f32| 0.5 + 0.3 * (x * 2.0 * std::f32::consts::PI / 9.0).sin();
        let a = region((0.0, 0.0), &f);
        let b = region((2.0, 0.0), &f);
        let r = match_patch(&a, &b, &MatchConfig::default());
        assert!(!r.valid, "{:?}", r);
    }

    #[test]
    fn masked_pixels_are_ignored() {
        let a = region((0.0, 0.0), &texture_value);
        let mut b = region((1.5, -0.5), &texture_value);
        // a block of garbage that is marked invalid
        let s = b.size;
        for y in 0..s / 3 {
            for x in 0..s {
                b.data[y * s + x] = 1.0;
                b.mask[y * s + x] = false;
            }
        }
        let cfg = MatchConfig { min_valid_fraction: 0.5, ..Default::default() };
        let r = match_patch(&a, &b, &cfg);
        assert!(r.valid, "{:?}", r);
        assert!((r.dx - 1.5).abs() < 0.15 && (r.dy + 0.5).abs() < 0.15, "{:?}", r);
    }
}
