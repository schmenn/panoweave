//! Photometric model per lens ("lens correction"): response curve and vignetting.
//!
//! One factor per photo and colour channel (`stitch::compute_gains`) cannot explain differences
//! that depend on the brightness (another tone curve of a second camera/lens) or on the distance
//! from the photo centre (vignetting). Both are properties of the lens, so they are solved once
//! for all photos of a lens, from the comparisons in the overlaps of all photos together:
//!
//! corrected = value^γ_c / v(r),   v(r) = 1 + c1·r² + c2·r⁴
//!
//! `value` is the linear light value of the standard sRGB decoding, γ_c a small exponent per
//! colour channel (relative to sRGB, near 1), `r` the distance from the photo centre divided by
//! half the diagonal. The fit is a robust (Huber) Levenberg-Marquardt in log space together with
//! one log factor per photo and channel, in three stages: factors only, then vignetting, then the
//! response curve. Parameters outside plausible limits are dropped (back to "no change"). The
//! smooth gain maps (`gainmap`) then only solve what is left.
//!
//! The response curve is only relative between lenses: the lens with the most comparisons keeps
//! γ = 1 (with matching linear photos the exponent could otherwise trade freely with the factors).

use crate::gainmap::{pair_samples, GainMapConfig};
use crate::stitch::{PanoView, RenderPhoto, MAX_GAIN};

/// Fewer usable comparisons than this for a lens: the lens gets no model.
pub const DEFAULT_MIN_SAMPLES: usize = 300;
/// Comparisons are weighted so that every one of this many radius bins counts the same.
pub const DEFAULT_RADIUS_BINS: usize = 10;
/// Differences above this (natural log) count less and less (Huber).
pub const DEFAULT_HUBER: f64 = 0.04;
/// Plausible range of the response exponent.
pub const DEFAULT_GAMMA_RANGE: (f32, f32) = (0.8, 1.25);
/// Plausible range of the vignetting factor anywhere between centre and corner.
pub const DEFAULT_VIG_RANGE: (f32, f32) = (0.2, 1.1);
/// Pull of γ towards 1 and of the vignetting coefficients towards 0, relative to the summed data
/// weight of the lens.
pub const DEFAULT_GAMMA_PRIOR: f64 = 0.02;
pub const DEFAULT_VIG_PRIOR: f64 = 0.002;
/// Pull of the photo factors towards 1, relative to the average data weight of a photo.
pub const DEFAULT_GAIN_PRIOR: f64 = 0.01;
/// Levenberg-Marquardt iterations per stage.
pub const DEFAULT_ITERS: usize = 15;
/// A radius bin may count at most this much more (or less) than its share of comparisons.
const MAX_BIN_WEIGHT: f64 = 5.0;
/// Vignetting factors below this are treated as this value (keeps the logarithm finite).
const MIN_VIG: f32 = 0.05;

/// Settings of the lens correction. `enabled = false` leaves everything as before.
#[derive(Clone, Debug, PartialEq)]
pub struct LensPhotoConfig {
    pub enabled: bool,
    pub min_samples: usize,
    pub radius_bins: usize,
    pub huber: f64,
    pub gamma_range: (f32, f32),
    pub vig_range: (f32, f32),
    pub gamma_prior: f64,
    pub vig_prior: f64,
    pub gain_prior: f64,
    pub iters: usize,
}

impl Default for LensPhotoConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_ENABLED,
            min_samples: DEFAULT_MIN_SAMPLES,
            radius_bins: DEFAULT_RADIUS_BINS,
            huber: DEFAULT_HUBER,
            gamma_range: DEFAULT_GAMMA_RANGE,
            vig_range: DEFAULT_VIG_RANGE,
            gamma_prior: DEFAULT_GAMMA_PRIOR,
            vig_prior: DEFAULT_VIG_PRIOR,
            gain_prior: DEFAULT_GAIN_PRIOR,
            iters: DEFAULT_ITERS,
        }
    }
}

/// Whether the lens correction is on by default.
pub const DEFAULT_ENABLED: bool = false;

impl LensPhotoConfig {
    /// Experiments: `PW_LENS=0` / `1` forces the correction off / on, whatever the switch says.
    pub fn is_on(&self) -> bool {
        match std::env::var("PW_LENS").ok().as_deref() {
            Some("0") => false,
            Some("1") => true,
            _ => self.enabled,
        }
    }
}

/// Response curve and vignetting of one lens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LensModel {
    /// exponent per colour channel applied to the linear value
    pub gamma: [f32; 3],
    /// c1, c2 of v(r) = 1 + c1·r² + c2·r⁴
    pub vig: [f32; 2],
}

/// Squared distance of the pixel position (`x`, `y`, pixel centres at +0.5 like `RenderPhoto::lookup`)
/// from the centre of a `w` x `h` photo, divided by the squared half diagonal.
#[inline]
pub fn radius2(x: f32, y: f32, w: f32, h: f32) -> f32 {
    let dx = x + 0.5 - 0.5 * w;
    let dy = y + 0.5 - 0.5 * h;
    (dx * dx + dy * dy) / (0.25 * (w * w + h * h)).max(1e-12)
}

impl LensModel {
    pub const IDENTITY: LensModel = LensModel { gamma: [1.0; 3], vig: [0.0; 2] };

    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }

    /// Vignetting factor at the squared normalised radius.
    #[inline]
    pub fn vignetting(&self, r2: f32) -> f32 {
        1.0 + self.vig[0] * r2 + self.vig[1] * r2 * r2
    }

    /// Corrected linear value of `c` seen at the photo position (`x`, `y`) of a `w` x `h` photo.
    /// Depends on nothing but its arguments (the same value in every strip of the export).
    #[inline]
    pub fn apply(&self, c: [f32; 3], x: f32, y: f32, w: f32, h: f32) -> [f32; 3] {
        let inv = 1.0 / self.vignetting(radius2(x, y, w, h)).max(MIN_VIG);
        let mut r = [0.0f32; 3];
        for k in 0..3 {
            let v = if self.gamma[k] == 1.0 { c[k] } else { c[k].max(0.0).powf(self.gamma[k]) };
            r[k] = v * inv;
        }
        r
    }

    /// Inside the plausible limits (vignetting between centre and corner, exponents)?
    pub fn plausible_vig(&self, range: (f32, f32)) -> bool {
        (0..=20).all(|i| {
            let r = i as f32 / 20.0;
            let v = self.vignetting(r * r);
            v.is_finite() && v >= range.0 && v <= range.1
        })
    }

    pub fn plausible_gamma(&self, range: (f32, f32)) -> bool {
        self.gamma.iter().all(|&g| g.is_finite() && g >= range.0 && g <= range.1)
    }
}

/// Result of the fit: a model per lens (None: not enough comparisons, or nothing plausible) and
/// the factor per photo that goes with it (to be used instead of `stitch::compute_gains`).
#[derive(Clone, Debug, PartialEq)]
pub struct LensFit {
    pub models: Vec<Option<LensModel>>,
    pub gains: Vec<[f32; 3]>,
    /// robust cost (mean per comparison) with factors only and with the whole model
    pub cost_gains_only: f64,
    pub cost_final: f64,
}

/// One comparison prepared for the fit.
struct FObs {
    a: usize,
    b: usize,
    la: usize,
    lb: usize,
    ra2: f64,
    rb2: f64,
    lva: [f64; 3],
    lvb: [f64; 3],
    w: f64,
}

/// Layout of the unknowns: 3 log factors per photo, 3 exponents and 2 vignetting coefficients per lens.
struct Layout {
    n: usize,
    nl: usize,
}

impl Layout {
    fn g(&self, p: usize, c: usize) -> usize {
        p * 3 + c
    }
    fn gamma(&self, l: usize, c: usize) -> usize {
        self.n * 3 + l * 3 + c
    }
    fn vig(&self, l: usize, k: usize) -> usize {
        self.n * 3 + self.nl * 3 + l * 2 + k
    }
    fn len(&self) -> usize {
        self.n * 3 + self.nl * 5
    }
}

fn vig_of(x: &[f64], lay: &Layout, l: usize, r2: f64) -> f64 {
    1.0 + x[lay.vig(l, 0)] * r2 + x[lay.vig(l, 1)] * r2 * r2
}

/// Log of the corrected value of one end of a comparison, and the derivatives (index, value).
fn side(x: &[f64], lay: &Layout, p: usize, l: usize, r2: f64, lv: f64, c: usize, out: &mut Vec<(usize, f64)>, sign: f64) -> f64 {
    let v = vig_of(x, lay, l, r2);
    let vc = v.max(MIN_VIG as f64);
    out.push((lay.g(p, c), sign));
    out.push((lay.gamma(l, c), sign * lv));
    if v > MIN_VIG as f64 {
        out.push((lay.vig(l, 0), -sign * r2 / vc));
        out.push((lay.vig(l, 1), -sign * r2 * r2 / vc));
    }
    x[lay.gamma(l, c)] * lv - vc.ln() + x[lay.g(p, c)]
}

fn huber(e: f64, k: f64) -> f64 {
    let a = e.abs();
    if a <= k { 0.5 * e * e } else { k * a - 0.5 * k * k }
}

/// Prior terms: (parameter, target, weight).
fn priors(lay: &Layout, active: &[bool], wl: &[f64], unit_g: f64, cfg: &LensPhotoConfig) -> Vec<(usize, f64, f64)> {
    let mut v = Vec::new();
    for p in (0..lay.n).filter(|&p| active[p]) {
        for c in 0..3 {
            v.push((lay.g(p, c), 0.0, cfg.gain_prior * unit_g));
        }
    }
    for l in 0..lay.nl {
        for c in 0..3 {
            v.push((lay.gamma(l, c), 1.0, cfg.gamma_prior * wl[l].max(1.0)));
        }
        for k in 0..2 {
            v.push((lay.vig(l, k), 0.0, cfg.vig_prior * wl[l].max(1.0)));
        }
    }
    v
}

fn cost(obs: &[FObs], x: &[f64], lay: &Layout, pri: &[(usize, f64, f64)], k: f64) -> f64 {
    let mut s = 0.0;
    let mut tmp = Vec::with_capacity(8);
    for o in obs {
        for c in 0..3 {
            tmp.clear();
            let e = side(x, lay, o.a, o.la, o.ra2, o.lva[c], c, &mut tmp, 1.0) - side(x, lay, o.b, o.lb, o.rb2, o.lvb[c], c, &mut tmp, -1.0);
            s += o.w * huber(e, k);
        }
    }
    s + pri.iter().map(|&(i, t, w)| 0.5 * w * (x[i] - t).powi(2)).sum::<f64>()
}

/// Levenberg-Marquardt on the free parameters (IRLS Huber weights from the current residuals).
fn lm(obs: &[FObs], x: &mut [f64], free: &[bool], lay: &Layout, pri: &[(usize, f64, f64)], cfg: &LensPhotoConfig) -> Option<f64> {
    let idx: Vec<usize> = (0..lay.len()).filter(|&i| free[i]).collect();
    let mut pos = vec![usize::MAX; lay.len()];
    for (j, &i) in idx.iter().enumerate() {
        pos[i] = j;
    }
    let m = idx.len();
    let k = cfg.huber;
    let mut cur = cost(obs, x, lay, pri, k);
    if m == 0 {
        return Some(cur);
    }
    let mut mu = 1e-3;
    let mut tmp = Vec::with_capacity(8);
    for _ in 0..cfg.iters {
        if crate::engine::cancelled() {
            return None;
        }
        let mut h = vec![vec![0.0f64; m]; m];
        let mut g = vec![0.0f64; m];
        for o in obs {
            for c in 0..3 {
                tmp.clear();
                let e = side(x, lay, o.a, o.la, o.ra2, o.lva[c], c, &mut tmp, 1.0) - side(x, lay, o.b, o.lb, o.rb2, o.lvb[c], c, &mut tmp, -1.0);
                let hw = if e.abs() <= k { 1.0 } else { k / e.abs() };
                let w = o.w * hw;
                for &(i, d) in &tmp {
                    let pi = pos[i];
                    if pi == usize::MAX {
                        continue;
                    }
                    g[pi] += w * d * e;
                    for &(j, d2) in &tmp {
                        let pj = pos[j];
                        if pj != usize::MAX {
                            h[pi][pj] += w * d * d2;
                        }
                    }
                }
            }
        }
        for &(i, t, w) in pri {
            let pi = pos[i];
            if pi != usize::MAX {
                g[pi] += w * (x[i] - t);
                h[pi][pi] += w;
            }
        }
        let mut accepted = false;
        for _ in 0..8 {
            let mut a = h.clone();
            for (j, row) in a.iter_mut().enumerate() {
                row[j] += mu * h[j][j].max(1e-9);
            }
            let Some(delta) = crate::geometry::solve(a, g.iter().map(|v| -v).collect()) else {
                mu *= 10.0;
                continue;
            };
            let mut nx = x.to_vec();
            for (j, &i) in idx.iter().enumerate() {
                nx[i] += delta[j];
            }
            let nc = cost(obs, &nx, lay, pri, k);
            if nc.is_finite() && nc < cur {
                let gain = cur - nc;
                x.copy_from_slice(&nx);
                cur = nc;
                mu = (mu / 3.0).max(1e-9);
                accepted = true;
                if gain < 1e-9 * cur.max(1e-12) {
                    return Some(cur);
                }
                break;
            }
            mu *= 4.0;
        }
        if !accepted {
            break;
        }
    }
    Some(cur)
}

/// Fits the model of every lens together with the factors of the photos. `photos` are the photos
/// as the renderer gets them (their factors, maps and models are ignored), `lens_of[i]` the lens
/// of photo `i`, `gm` the settings of the comparisons (the same as for the gain maps).
/// None: not enough comparisons at all (or cancelled) – then nothing should change.
pub fn fit_lens_models(photos: &[RenderPhoto], lens_of: &[usize], view: &PanoView, cfg: &LensPhotoConfig, gm: &GainMapConfig) -> Option<LensFit> {
    let n = photos.len();
    if n < 2 || lens_of.len() != n || !cfg.enabled {
        return None;
    }
    let nl = lens_of.iter().copied().max().unwrap_or(0) + 1;
    let raw: Vec<RenderPhoto> = photos.iter().map(|p| RenderPhoto { gain: [1.0; 3], gain_map: None, photometry: None, ..p.clone() }).collect();
    let samples = pair_samples(&raw, view, gm);
    if crate::engine::cancelled() || samples.is_empty() {
        return None;
    }
    let mut obs: Vec<FObs> = samples
        .iter()
        .map(|s| {
            let r2 = |p: usize, f: [f32; 2]| {
                let (w, h) = (photos[p].w, photos[p].h);
                radius2(f[0] * w as f32 - 0.5, f[1] * h as f32 - 0.5, w as f32, h as f32) as f64
            };
            FObs {
                a: s.a,
                b: s.b,
                la: lens_of[s.a],
                lb: lens_of[s.b],
                ra2: r2(s.a, s.pa),
                rb2: r2(s.b, s.pb),
                lva: s.va.map(|v| (v.max(1e-6) as f64).ln()),
                lvb: s.vb.map(|v| (v.max(1e-6) as f64).ln()),
                w: 1.0,
            }
        })
        .collect();
    // every radius bin of a lens counts the same (centre and border alike)
    let bins = cfg.radius_bins.max(1);
    let bin = |r2: f64| ((r2.max(0.0).sqrt() * bins as f64) as usize).min(bins - 1);
    let mut hist = vec![vec![0usize; bins]; nl];
    let mut ends = vec![0usize; nl];
    for o in &obs {
        hist[o.la][bin(o.ra2)] += 1;
        hist[o.lb][bin(o.rb2)] += 1;
        ends[o.la] += 1;
        ends[o.lb] += 1;
    }
    let end_w = |l: usize, r2: f64| {
        let nonempty = hist[l].iter().filter(|&&c| c > 0).count().max(1);
        let share = ends[l] as f64 / nonempty as f64;
        (share / hist[l][bin(r2)].max(1) as f64).clamp(1.0 / MAX_BIN_WEIGHT, MAX_BIN_WEIGHT)
    };
    for o in obs.iter_mut() {
        o.w = 0.5 * (end_w(o.la, o.ra2) + end_w(o.lb, o.rb2));
    }
    let mean_w = obs.iter().map(|o| o.w).sum::<f64>() / obs.len() as f64;
    for o in obs.iter_mut() {
        o.w /= mean_w;
    }
    let lay = Layout { n, nl };
    let mut active = vec![false; n];
    let mut wl = vec![0.0f64; nl];
    for o in &obs {
        active[o.a] = true;
        active[o.b] = true;
        wl[o.la] += o.w;
        wl[o.lb] += o.w;
    }
    let n_active = active.iter().filter(|&&a| a).count().max(1);
    let unit_g = 2.0 * obs.iter().map(|o| o.w).sum::<f64>() / n_active as f64;
    let pri = priors(&lay, &active, &wl, unit_g, cfg);
    // lenses with enough comparisons; the one with the most keeps γ = 1
    let modelled: Vec<bool> = (0..nl).map(|l| ends[l] >= cfg.min_samples).collect();
    let reference = (0..nl).filter(|&l| modelled[l]).max_by_key(|&l| (ends[l], std::cmp::Reverse(l)));
    let debug = std::env::var("PW_GM_DEBUG").is_ok() || std::env::var("PW_LENS_DEBUG").is_ok();
    if debug {
        eprintln!("lens fit: {} comparisons, per lens {:?}, modelled {:?}, reference {:?}", obs.len(), ends, modelled, reference);
    }
    let mut x = vec![0.0f64; lay.len()];
    for l in 0..nl {
        for c in 0..3 {
            x[lay.gamma(l, c)] = 1.0;
        }
    }
    let mut free = vec![false; lay.len()];
    for p in (0..n).filter(|&p| active[p]) {
        for c in 0..3 {
            free[lay.g(p, c)] = true;
        }
    }
    let cost_gains_only = lm(&obs, &mut x, &free, &lay, &pri, cfg)?;
    let x_gains = x.clone();
    let mut use_vig: Vec<bool> = modelled.clone();
    let mut use_gamma: Vec<bool> = (0..nl).map(|l| modelled[l] && Some(l) != reference).collect();
    let mut cost_final = cost_gains_only;
    // every repeated round drops at least one part, so this ends with a plausible fit
    for round in 0..=2 * nl {
        x.copy_from_slice(&x_gains);
        let mut f = free.clone();
        for l in (0..nl).filter(|&l| use_vig[l]) {
            f[lay.vig(l, 0)] = true;
            f[lay.vig(l, 1)] = true;
        }
        let c2 = lm(&obs, &mut x, &f, &lay, &pri, cfg)?;
        for l in (0..nl).filter(|&l| use_gamma[l]) {
            for c in 0..3 {
                f[lay.gamma(l, c)] = true;
            }
        }
        let c3 = lm(&obs, &mut x, &f, &lay, &pri, cfg)?;
        cost_final = c3;
        if debug {
            eprintln!("lens fit round {round}: cost factors only {:.5}, + vignetting {:.5}, + response {:.5} (per comparison)", cost_gains_only / obs.len() as f64, c2 / obs.len() as f64, c3 / obs.len() as f64);
        }
        // implausible parts go back to "no change" and the fit runs again without them
        let mut changed = false;
        for l in 0..nl {
            let m = model_of(&x, &lay, l);
            if use_vig[l] && !m.plausible_vig(cfg.vig_range) {
                eprintln!("lens correction: vignetting of lens {} is implausible ({:?}), not used", l + 1, m.vig);
                use_vig[l] = false;
                changed = true;
            }
            if use_gamma[l] && !m.plausible_gamma(cfg.gamma_range) {
                eprintln!("lens correction: response curve of lens {} is implausible ({:?}), not used", l + 1, m.gamma);
                use_gamma[l] = false;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut models: Vec<Option<LensModel>> = (0..nl)
        .map(|l| {
            let mut m = model_of(&x, &lay, l);
            if !use_vig[l] {
                m.vig = [0.0; 2];
            }
            if !use_gamma[l] {
                m.gamma = [1.0; 3];
            }
            (modelled[l] && !m.is_identity()).then_some(m)
        })
        .collect();
    // safety net (the rounds above already leave only plausible parts)
    for m in models.iter_mut() {
        if m.is_some_and(|m| !m.plausible_vig(cfg.vig_range) || !m.plausible_gamma(cfg.gamma_range)) {
            *m = None;
        }
    }
    // no silent drops: say why a lens got no model
    for (l, m) in models.iter().enumerate() {
        if m.is_none() && ends[l] > 0 {
            if !modelled[l] {
                eprintln!("lens correction: lens {} has only {} usable comparisons (at least {} needed), no model", l + 1, ends[l], cfg.min_samples);
            } else {
                eprintln!("lens correction: lens {} - the fit found nothing worth correcting (or nothing plausible), no model", l + 1);
            }
        }
    }
    // factors: centred on the photos that take part (overall brightness stays), limited like the global ones
    let mut gains = vec![[1.0f32; 3]; n];
    for c in 0..3 {
        let mean = (0..n).filter(|&p| active[p]).map(|p| x[lay.g(p, c)]).sum::<f64>() / n_active as f64;
        for p in (0..n).filter(|&p| active[p]) {
            gains[p][c] = ((x[lay.g(p, c)] - mean).exp() as f32).clamp(1.0 / MAX_GAIN, MAX_GAIN);
        }
    }
    if debug {
        for (l, m) in models.iter().enumerate() {
            eprintln!("lens correction: lens {} {:?}", l + 1, m);
        }
    }
    let per = obs.len() as f64;
    Some(LensFit { models, gains, cost_gains_only: cost_gains_only / per, cost_final: cost_final / per })
}

fn model_of(x: &[f64], lay: &Layout, l: usize) -> LensModel {
    LensModel { gamma: [0, 1, 2].map(|c| x[lay.gamma(l, c)] as f32), vig: [x[lay.vig(l, 0)] as f32, x[lay.vig(l, 1)] as f32] }
}

#[cfg(test)]
#[path = "lensphoto_tests.rs"]
mod tests;
