//! Near objects (a car 3 m in front of the camera, a railing, a passer-by): a rotation-only camera cannot explain
//! how they shift between two shots. Points and patches on them pull the whole alignment towards the near object
//! and away from the background (which is what has to line up). Like foliage they get a smaller weight instead of
//! being deleted: the background decides.
//!
//! A *layer* is found per photo pair, in two ways:
//!  - while matching (`features::parallax_group`): a coherent group of matches that the main model rejects, and
//!  - after aligning (`detect_after`): points and patches whose remaining error is large, points the same way
//!    (similar error vector) and lie together in the picture. A pair whose points are *all* off (a wrong lens, a wrong
//!    start) is not a near object and is left alone: the group must leave enough points that fit.
//!
//! A region is a coarse mask (16 x 16 cells) in both photos. A control point whose two ends both lie in the mask gets
//! `ControlPoint::depth_w = DepthConfig::min_weight`; the optimiser multiplies it in (`ControlPoint::opt_weight`).
//! Protected points and points placed by hand are never touched (the same rule as for foliage). `enabled = false`
//! sets every factor to 1 and finds nothing: exactly the earlier behaviour.

use crate::align::{self, AlignInput, ControlPoint, Lens};
use crate::geometry::M3;
use serde::{Deserialize, Serialize};

/// Cells per side of a region mask.
pub const GRID: usize = 16;

/// Settings of the near-object rating.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DepthConfig {
    /// off = no layers are searched, no factor is set (all 1)
    pub enabled: bool,
    /// weight factor of a point or patch on a near object; 1 = no effect
    pub min_weight: f32,
    /// after aligning: smallest remaining error of a point that may belong to a near object, in working-image
    /// pixels at 1600 px (scaled with the picture size)
    pub min_error_px: f32,
    /// after aligning: smallest number of points and patches of a pair in one group
    pub min_points: usize,
    /// after aligning: smallest share of the pair's points a group must make up
    pub min_share: f32,
    /// a group may make up at most this share of the pair; more means the whole pair is off, not an object
    pub max_share: f32,
    /// a point belongs to the group when its error vector differs from the group's median by less than this share of
    /// the median (but at least 2 px)
    pub vector_tolerance: f32,
    /// share of the group that must lie in one connected region of the picture
    pub min_coherence: f32,
    /// a cell belongs to a region when at least this many points of the group lie in it (strays do not count)
    pub min_cell_points: usize,
    /// rings of cells added around the cells with group points (the object is larger than its points)
    pub dilate: usize,
    /// most layers searched per pair after aligning
    pub max_layers: usize,
}

impl Default for DepthConfig {
    fn default() -> Self {
        DepthConfig { enabled: true, min_weight: 0.25, min_error_px: 8.0, min_points: 8, min_share: 0.1, max_share: 0.6, vector_tolerance: 0.4, min_coherence: 0.7, min_cell_points: 2, dilate: 1, max_layers: 2 }
    }
}

impl DepthConfig {
    /// Default settings with the rating switched on or off. Environment `PW_DEPTH=0` switches it off (for comparing),
    /// `PW_DEPTH_W=<factor>` sets the factor, `PW_DEPTH_ERR=<px>` and `PW_DEPTH_POINTS=<n>` the limits `min_error_px` and `min_points` (experiments).
    pub fn with_env(self) -> Self {
        let mut c = self;
        if std::env::var("PW_DEPTH").is_ok_and(|v| v == "0") {
            c.enabled = false;
        }
        if let Some(w) = std::env::var("PW_DEPTH_W").ok().and_then(|v| v.parse().ok()) {
            c.min_weight = w;
        }
        if let Some(e) = std::env::var("PW_DEPTH_ERR").ok().and_then(|v| v.parse().ok()) {
            c.min_error_px = e;
        }
        if let Some(n) = std::env::var("PW_DEPTH_POINTS").ok().and_then(|v| v.parse().ok()) {
            c.min_points = n;
        }
        c
    }
}

/// Cells of a photo (`GRID` x `GRID`) as a bit field, row by row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoarseMask {
    bits: [u64; GRID * GRID / 64],
}

impl CoarseMask {
    pub fn set(&mut self, cx: usize, cy: usize) {
        let i = cy * GRID + cx;
        self.bits[i / 64] |= 1 << (i % 64);
    }
    pub fn get(&self, cx: usize, cy: usize) -> bool {
        let i = cy * GRID + cx;
        self.bits[i / 64] >> (i % 64) & 1 == 1
    }
    /// Number of cells set in both masks.
    pub fn overlap(&self, other: &CoarseMask) -> usize {
        self.bits.iter().zip(&other.bits).map(|(a, b)| (a & b).count_ones() as usize).sum()
    }
    /// Number of cells set.
    pub fn count(&self) -> usize {
        self.bits.iter().map(|b| b.count_ones() as usize).sum()
    }
    /// Cell of a pixel of a `w` x `h` picture (outside values are clamped to the edge).
    pub fn cell_of_px(x: f64, y: f64, w: usize, h: usize) -> (usize, usize) {
        let f = |v: f64, n: usize| ((v / n.max(1) as f64 * GRID as f64).floor() as isize).clamp(0, GRID as isize - 1) as usize;
        (f(x, w), f(y, h))
    }
    /// Cell of a normalised coordinate (long side = 1, see `engine::norm_coord`).
    /// Place of a normalised coordinate as shares of the picture (0..1 across, 0..1 down).
    pub fn frac_of_norm(p: [f64; 2], dims: (usize, usize)) -> [f32; 2] {
        let l = dims.0.max(dims.1) as f64;
        [((p[0] * l + dims.0 as f64 * 0.5) / dims.0.max(1) as f64) as f32, ((p[1] * l + dims.1 as f64 * 0.5) / dims.1.max(1) as f64) as f32]
    }
    pub fn cell_of_norm(p: [f64; 2], dims: (usize, usize)) -> (usize, usize) {
        let l = dims.0.max(dims.1) as f64;
        Self::cell_of_px(p[0] * l + dims.0 as f64 * 0.5, p[1] * l + dims.1 as f64 * 0.5, dims.0, dims.1)
    }
    #[cfg(test)]
    pub fn set_norm(&mut self, p: [f64; 2], dims: (usize, usize)) {
        let (cx, cy) = Self::cell_of_norm(p, dims);
        self.set(cx, cy);
    }
    pub fn contains_norm(&self, p: [f64; 2], dims: (usize, usize)) -> bool {
        let (cx, cy) = Self::cell_of_norm(p, dims);
        self.get(cx, cy)
    }
    /// Cells set, as (cx, cy).
    pub fn cells(&self) -> Vec<(usize, usize)> {
        (0..GRID * GRID).map(|i| (i % GRID, i / GRID)).filter(|&(x, y)| self.get(x, y)).collect()
    }
    /// Bounding box of the set cells as shares of the picture (x0, y0, x1, y1); None when empty.
    pub fn bbox(&self) -> Option<[f32; 4]> {
        let c = self.cells();
        let (x0, x1) = (c.iter().map(|p| p.0).min()?, c.iter().map(|p| p.0).max()?);
        let (y0, y1) = (c.iter().map(|p| p.1).min()?, c.iter().map(|p| p.1).max()?);
        let g = GRID as f32;
        Some([x0 as f32 / g, y0 as f32 / g, (x1 + 1) as f32 / g, (y1 + 1) as f32 / g])
    }
    /// Whether a place given as shares of the picture (0..1 across, 0..1 down) lies in a cell that is set.
    pub fn contains_frac(&self, fx: f64, fy: f64) -> bool {
        let f = |v: f64| ((v * GRID as f64).floor() as isize).clamp(0, GRID as isize - 1) as usize;
        self.get(f(fx), f(fy))
    }
    /// The mask with `rings` rings of cells added around it (8 neighbours per ring).
    pub fn dilated(&self, rings: usize) -> Self {
        let mut m = *self;
        for _ in 0..rings {
            let src = m;
            for cy in 0..GRID {
                for cx in 0..GRID {
                    if src.get(cx, cy) {
                        for (dx, dy) in (-1isize..=1).flat_map(|dy| (-1isize..=1).map(move |dx| (dx, dy))) {
                            let (nx, ny) = (cx as isize + dx, cy as isize + dy);
                            if (0..GRID as isize).contains(&nx) && (0..GRID as isize).contains(&ny) {
                                m.set(nx as usize, ny as usize);
                            }
                        }
                    }
                }
            }
        }
        m
    }
}

/// How many points of a group lie in each cell; `mask` keeps the cells with enough of them, so a few strays far from
/// the object do not stretch the region over half the picture.
#[derive(Clone)]
pub struct CellCounts([u16; GRID * GRID]);

impl Default for CellCounts {
    fn default() -> Self {
        CellCounts([0; GRID * GRID])
    }
}

impl CellCounts {
    pub fn add_px(&mut self, x: f64, y: f64, dims: (usize, usize)) {
        let (cx, cy) = CoarseMask::cell_of_px(x, y, dims.0, dims.1);
        self.0[cy * GRID + cx] += 1;
    }
    pub fn add_norm(&mut self, p: [f64; 2], dims: (usize, usize)) {
        let (cx, cy) = CoarseMask::cell_of_norm(p, dims);
        self.0[cy * GRID + cx] += 1;
    }
    /// The cells with at least `min` points; when the group is so small that none has that many, the cells with any.
    pub fn mask(&self, min: usize) -> CoarseMask {
        let strict = self.0.iter().any(|&c| c as usize >= min);
        let need = if strict { min.max(1) } else { 1 };
        let mut m = CoarseMask::default();
        for (i, &c) in self.0.iter().enumerate() {
            if c as usize >= need {
                m.set(i % GRID, i / GRID);
            }
        }
        m
    }
}

/// Size of the largest 8-connected region of occupied cells of a count grid (sum of the counts).
pub fn largest_component<const N: usize>(cnt: &[[usize; N]; N]) -> usize {
    let mut seen = [[false; N]; N];
    let mut best = 0usize;
    for y0 in 0..N {
        for x0 in 0..N {
            if seen[y0][x0] || cnt[y0][x0] == 0 {
                continue;
            }
            let (mut sum, mut stack) = (0usize, vec![(x0, y0)]);
            seen[y0][x0] = true;
            while let Some((x, y)) = stack.pop() {
                sum += cnt[y][x];
                for dy in -1isize..=1 {
                    for dx in -1isize..=1 {
                        let (nx, ny) = (x as isize + dx, y as isize + dy);
                        if nx < 0 || ny < 0 || nx >= N as isize || ny >= N as isize {
                            continue;
                        }
                        let (nx, ny) = (nx as usize, ny as usize);
                        if !seen[ny][nx] && cnt[ny][nx] > 0 {
                            seen[ny][nx] = true;
                            stack.push((nx, ny));
                        }
                    }
                }
            }
            best = best.max(sum);
        }
    }
    best
}

/// Where a region came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    /// matches the rotation model rejected (point finder)
    Matches,
    /// errors left after aligning
    Residuals,
}

/// A near object in one photo pair: its cells in both photos.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NearRegion {
    pub a: usize,
    pub b: usize,
    pub mask_a: CoarseMask,
    pub mask_b: CoarseMask,
    /// size of the working images of a and b
    pub dims_a: (usize, usize),
    pub dims_b: (usize, usize),
    /// typical distance from where the background model puts it, working-image pixels at 1600 px
    pub shift_px: f32,
    /// matches or points in the group
    pub count: usize,
    pub source: Source,
    /// the points of the group in photo a and b as shares of the picture (only regions found from errors have them;
    /// they give the tight outline of `near_seams.rs`)
    #[serde(default)]
    pub pts_a: Vec<[f32; 2]>,
    #[serde(default)]
    pub pts_b: Vec<[f32; 2]>,
}

impl NearRegion {
    /// Whether both ends of the pair lie on the object.
    pub fn covers(&self, cp: &ControlPoint) -> bool {
        if (cp.a, cp.b) == (self.a, self.b) {
            self.mask_a.contains_norm(cp.pa, self.dims_a) && self.mask_b.contains_norm(cp.pb, self.dims_b)
        } else if (cp.a, cp.b) == (self.b, self.a) {
            self.mask_b.contains_norm(cp.pa, self.dims_b) && self.mask_a.contains_norm(cp.pb, self.dims_a)
        } else {
            false
        }
    }
    /// Whether a spot of photo `photo` (given as shares of the picture, so any resolution works) lies on the object.
    pub fn covers_frac(&self, photo: usize, fx: f64, fy: f64) -> bool {
        if photo == self.a {
            self.mask_a.contains_frac(fx, fy)
        } else if photo == self.b {
            self.mask_b.contains_frac(fx, fy)
        } else {
            false
        }
    }
    pub fn text(&self) -> String {
        format!("photos {} and {}: {} matches or points about {:.0} px off the background", self.a + 1, self.b + 1, self.count, self.shift_px)
    }
}

/// All near objects found so far.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layers {
    pub regions: Vec<NearRegion>,
}

impl Layers {
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }
    /// The region that holds both ends of the control point.
    pub fn covering(&self, cp: &ControlPoint) -> Option<&NearRegion> {
        self.regions.iter().find(|r| r.covers(cp))
    }
    /// Replaces the regions of one source by new ones (the other source stays).
    pub fn replace_source(&mut self, source: Source, new: Vec<NearRegion>) {
        self.regions.retain(|r| r.source != source);
        self.regions.extend(new);
    }
    /// Whether a spot of photo `photo` lies on a near object in any pair.
    pub fn covers_spot(&self, photo: usize, fx: f64, fy: f64) -> bool {
        self.regions.iter().any(|r| r.covers_frac(photo, fx, fy))
    }
    /// Layers of the matches the point finder reported (see `engine::ParallaxHint`).
    pub fn from_hints(hints: &[crate::engine::ParallaxHint], cfg: &DepthConfig) -> Self {
        if !cfg.enabled {
            return Layers::default();
        }
        Layers {
            regions: hints
                .iter()
                .map(|h| NearRegion {
                    a: h.a,
                    b: h.b,
                    mask_a: h.group.mask_a.dilated(cfg.dilate),
                    mask_b: h.group.mask_b.dilated(cfg.dilate),
                    dims_a: h.dims,
                    dims_b: h.dims_b,
                    shift_px: h.group.shift_px as f32,
                    count: h.group.count,
                    source: Source::Matches,
                    pts_a: Vec::new(),
                    pts_b: Vec::new(),
                })
                .collect(),
        }
    }
}

/// Sets `depth_w` of every point and patch: `min_weight` where a region of its pair holds both ends, 1 elsewhere.
/// Switched off: every factor is put back to 1. Returns how many got a smaller factor (for logs).
pub fn rate(cps: &mut [ControlPoint], layers: &Layers, cfg: &DepthConfig) -> usize {
    let on = cfg.enabled && !layers.is_empty();
    let mut n = 0;
    for c in cps.iter_mut() {
        c.depth_w = 1.0;
        if on && crate::foliage::rateable(c) && layers.covering(c).is_some() {
            c.depth_w = cfg.min_weight.clamp(0.0, 1.0);
            if c.depth_w < 0.999 {
                n += 1;
            }
        }
    }
    n
}

/// Finds near objects from the errors an alignment leaves. `resid` are the residual vectors of `cps`
/// (`align::cp_residuals`), `dims` the size of every photo's working image. Protected and hand-placed points count as
/// evidence (a person put them there) but are never weighted down by `rate`.
pub fn detect_after(cps: &[ControlPoint], resid: &[[f64; 4]], dims: &[(usize, usize)], cfg: &DepthConfig) -> Vec<NearRegion> {
    let mut out = Vec::new();
    if !cfg.enabled || cps.len() != resid.len() {
        return out;
    }
    let mut by_pair: std::collections::BTreeMap<(usize, usize), Vec<usize>> = Default::default();
    for (i, c) in cps.iter().enumerate() {
        by_pair.entry((c.a, c.b)).or_default().push(i);
    }
    // vectors in 1600 px units, so the limits do not depend on the picture size
    let to_px = |v: f64| v * 1600.0;
    for ((a, b), idx) in by_pair {
        if crate::engine::cancelled() {
            break;
        }
        let total = idx.len();
        if total < cfg.min_points {
            continue;
        }
        let vec_of = |i: usize| [to_px(resid[i][0]), to_px(resid[i][1])];
        let mag = |v: [f64; 2]| (v[0] * v[0] + v[1] * v[1]).sqrt();
        let mut bad: Vec<usize> = idx.iter().copied().filter(|&i| mag(vec_of(i)) >= cfg.min_error_px as f64).collect();
        let fits = total - bad.len();
        for _ in 0..cfg.max_layers {
            if bad.len() < cfg.min_points {
                break;
            }
            // the most common error vector: the point with the most neighbours within the tolerance
            let near = |i: usize, j: usize| {
                let (u, v) = (vec_of(i), vec_of(j));
                mag([u[0] - v[0], u[1] - v[1]]) <= (cfg.vector_tolerance as f64 * mag(u)).max(2.0)
            };
            let Some(&seed) = bad.iter().max_by_key(|&&i| bad.iter().filter(|&&j| near(i, j)).count()) else { break };
            let group: Vec<usize> = bad.iter().copied().filter(|&j| near(seed, j)).collect();
            let share = group.len() as f32 / total as f32;
            if group.len() < cfg.min_points || share < cfg.min_share {
                break;
            }
            // coherence: most of the group lies in one connected region of the first photo
            let mut cnt = [[0usize; 8]; 8];
            for &i in &group {
                let (cx, cy) = CoarseMask::cell_of_norm(cps[i].pa, dims[a]);
                cnt[cy / 2][cx / 2] += 1;
            }
            let coherent = largest_component(&cnt) as f32 >= cfg.min_coherence * group.len() as f32;
            // a pair that is off as a whole (wrong lens, wrong start) is not a near object: enough points must fit
            let background = total - group.len();
            let enough_fit = share <= cfg.max_share && fits as f32 >= 0.2 * total as f32 && background >= cfg.min_points;
            if coherent && enough_fit {
                let (mut ma, mut mb) = (CellCounts::default(), CellCounts::default());
                for &i in &group {
                    ma.add_norm(cps[i].pa, dims[a]);
                    mb.add_norm(cps[i].pb, dims[b]);
                }
                let (ma, mb) = (ma.mask(cfg.min_cell_points), mb.mask(cfg.min_cell_points));
                let mut mags: Vec<f64> = group.iter().map(|&i| mag(vec_of(i))).collect();
                mags.sort_by(f64::total_cmp);
                out.push(NearRegion {
                    a,
                    b,
                    mask_a: ma.dilated(cfg.dilate),
                    mask_b: mb.dilated(cfg.dilate),
                    dims_a: dims[a],
                    dims_b: dims[b],
                    shift_px: mags[mags.len() / 2] as f32,
                    count: group.len(),
                    source: Source::Residuals,
                    pts_a: group.iter().map(|&i| CoarseMask::frac_of_norm(cps[i].pa, dims[a])).collect(),
                    pts_b: group.iter().map(|&i| CoarseMask::frac_of_norm(cps[i].pb, dims[b])).collect(),
                });
            }
            bad.retain(|i| !group.contains(i));
        }
    }
    out
}

/// `detect_after` for a finished alignment: computes the residuals and replaces the regions found by residuals
/// in `layers`. Returns how many regions there are now (for logs).
#[allow(clippy::too_many_arguments)]
pub fn refresh(layers: &mut Layers, lens_of: &[usize], long_side: &[f64], lenses: &[Lens], rots: &[M3], cps: &[ControlPoint], dims: &[(usize, usize)], cfg: &DepthConfig) -> usize {
    if !cfg.enabled {
        layers.replace_source(Source::Residuals, Vec::new());
        return layers.regions.len();
    }
    let inp = AlignInput { n: lens_of.len(), lens_of, long_side };
    let resid = align::cp_residuals(&inp, rots, lenses, cps);
    let found = detect_after(cps, &resid, dims, cfg);
    layers.replace_source(Source::Residuals, found);
    layers.regions.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::align::{project, Lens, OptimizeOptions};
    use crate::geometry::{camera_from_ypr, mm, mtv, mv, normalize, transpose};

    const DIMS: (usize, usize) = (1600, 1200);

    /// Points of a pair of cameras (rotation only): `n_bg` on the background and `n_obj` on a near object in the
    /// middle of the picture whose second end is shifted by `shift` (normalised units) the way parallax shifts it.
    fn scene(n_bg: usize, n_obj: usize, shift: [f64; 2]) -> (Vec<M3>, Lens, Vec<ControlPoint>) {
        let lens = Lens::from_hfov_deg(50.0);
        let truth = vec![camera_from_ypr(0.0, 0.0, 0.0), camera_from_ypr(0.35, 0.01, 0.0)];
        let mut cps = Vec::new();
        let mut s = 7u64;
        let mut rnd = move || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 33) as f64) / (1u64 << 31) as f64
        };
        let mut n = 0;
        while n < n_bg + n_obj {
            let ca = mtv(&truth[0], [0.0, 0.0, 1.0]);
            let cb = mtv(&truth[1], [0.0, 0.0, 1.0]);
            let mid = normalize([ca[0] + cb[0], ca[1] + cb[1], ca[2] + cb[2]]);
            // the background spreads over the picture, the object sits in the middle
            let spread = if n < n_bg { 0.6 } else { 0.3 };
            let d = normalize([mid[0] + spread * (rnd() - 0.5), mid[1] + spread * (rnd() - 0.5), mid[2] + 0.1 * (rnd() - 0.5)]);
            let (Some(pa), Some(mut pb)) = (project(&lens, mv(&truth[0], d)), project(&lens, mv(&truth[1], d))) else { continue };
            let inside = |p: [f64; 2]| p[0].abs() < 0.45 && p[1].abs() < 0.3;
            if !inside(pa) || !inside(pb) {
                continue;
            }
            // the object: a block in the middle of photo a (cells of the grid, so the mask can find it)
            let on_obj = pa[0].abs() < 0.12 && pa[1].abs() < 0.12;
            if n < n_bg && on_obj || n >= n_bg && !on_obj {
                continue;
            }
            if n >= n_bg {
                pb = [pb[0] + shift[0], pb[1] + shift[1]];
            }
            cps.push(ControlPoint::new(0, 1, pa, pb, false));
            n += 1;
        }
        (truth, lens, cps)
    }

    fn solve(cps: &[ControlPoint], lens: Lens) -> (Vec<M3>, Vec<f64>) {
        let lens_of = [0usize, 0];
        let long = [1600.0, 1600.0];
        let inp = AlignInput { n: 2, lens_of: &lens_of, long_side: &long };
        let mut ls = vec![lens];
        let mut rots = align::initial_rotations(&inp, &ls, cps, 0);
        align::optimize(&inp, &mut rots, &mut ls, cps, 0, &OptimizeOptions::default(), 40);
        let resid = align::cp_residuals(&inp, &rots, &ls, cps).iter().map(|r| (r[0] * r[0] + r[1] * r[1]).sqrt() * 1600.0).collect();
        (rots, resid)
    }

    fn rot_error(rots: &[M3], truth: &[M3]) -> f64 {
        // relative rotation between the two photos against the true one
        let rel = |r: &[M3]| mm(&r[1], &transpose(&r[0]));
        let (x, y) = (rel(rots), rel(truth));
        (0..3).flat_map(|i| (0..3).map(move |j| (i, j))).map(|(i, j)| (x[i][j] - y[i][j]).abs()).fold(0.0, f64::max)
    }

    fn find(cps: &[ControlPoint], lens: Lens, cfg: &DepthConfig) -> (Vec<M3>, Vec<NearRegion>) {
        let (rots, _) = solve(cps, lens);
        let lens_of = [0usize, 0];
        let long = [1600.0, 1600.0];
        let inp = AlignInput { n: 2, lens_of: &lens_of, long_side: &long };
        let resid = align::cp_residuals(&inp, &rots, &[lens], cps);
        let regions = detect_after(cps, &resid, &[DIMS, DIMS], cfg);
        (rots, regions)
    }

    #[test]
    fn near_object_is_found_and_the_background_wins() {
        let (truth, lens, cps) = scene(60, 20, [0.03, 0.0]);
        let cfg = DepthConfig::default();
        let (rots0, regions) = find(&cps, lens, &cfg);
        assert_eq!(regions.len(), 1, "{regions:?}");
        let layers = Layers { regions };
        let mut rated = cps.clone();
        let n = rate(&mut rated, &layers, &cfg);
        // the points on the object (the last 20) are down-weighted, the background is not
        assert!(n >= 15, "{n}");
        // a ring of cells around the object is generous: some background next to it is rated too, most is not
        assert!(rated[..60].iter().filter(|c| c.depth_w < 1.0).count() <= 20, "background points must keep their weight: {}", rated[..60].iter().filter(|c| c.depth_w < 1.0).count());
        assert!(rated[60..].iter().filter(|c| c.depth_w < 1.0).count() >= 15);
        let (rots1, _) = solve(&rated, lens);
        let (e0, e1) = (rot_error(&rots0, &truth), rot_error(&rots1, &truth));
        assert!(e1 < 0.6 * e0, "weighted {e1} against unweighted {e0}");
    }

    #[test]
    fn without_an_object_nothing_changes() {
        let (_, lens, cps) = scene(80, 0, [0.0, 0.0]);
        let cfg = DepthConfig::default();
        let (rots0, regions) = find(&cps, lens, &cfg);
        assert!(regions.is_empty());
        let mut rated = cps.clone();
        assert_eq!(rate(&mut rated, &Layers { regions }, &cfg), 0);
        assert!(rated.iter().all(|c| c.depth_w == 1.0));
        let (rots1, _) = solve(&rated, lens);
        assert_eq!(rots0, rots1);
    }

    #[test]
    fn switched_off_finds_nothing_and_resets_the_factors() {
        let (_, lens, cps) = scene(60, 20, [0.03, 0.0]);
        let off = DepthConfig { enabled: false, ..Default::default() };
        let (_, regions) = find(&cps, lens, &off);
        assert!(regions.is_empty());
        // factors left over from an earlier run are put back
        let (_, regions) = find(&cps, lens, &DepthConfig::default());
        let mut rated = cps.clone();
        rate(&mut rated, &Layers { regions: regions.clone() }, &DepthConfig::default());
        assert!(rated.iter().any(|c| c.depth_w < 1.0));
        assert_eq!(rate(&mut rated, &Layers { regions }, &off), 0);
        assert!(rated.iter().all(|c| c.depth_w == 1.0));
    }

    #[test]
    fn protected_and_hand_placed_points_keep_their_weight() {
        let (_, lens, mut cps) = scene(60, 20, [0.03, 0.0]);
        let (_, regions) = find(&cps, lens, &DepthConfig::default());
        let layers = Layers { regions };
        // the first three points of the object that the region holds: protected, by hand, ordinary
        let on: Vec<usize> = (60..80).filter(|&i| layers.covering(&cps[i]).is_some()).collect();
        assert!(on.len() >= 3, "{on:?}");
        cps[on[0]].locked = true;
        cps[on[1]].manual = true;
        rate(&mut cps, &layers, &DepthConfig::default());
        assert_eq!((cps[on[0]].depth_w, cps[on[1]].depth_w), (1.0, 1.0));
        assert!(cps[on[2]].depth_w < 1.0);
    }

    #[test]
    fn a_pair_that_is_off_as_a_whole_is_not_a_near_object() {
        // every point is shifted the same way (a wrong start): no background is left, so no object
        let (_, lens, mut cps) = scene(0, 60, [0.0, 0.0]);
        let (rots, _) = solve(&cps, lens);
        let lens_of = [0usize, 0];
        let long = [1600.0, 1600.0];
        let inp = AlignInput { n: 2, lens_of: &lens_of, long_side: &long };
        for c in cps.iter_mut() {
            c.pb[0] += 0.02;
        }
        let resid = align::cp_residuals(&inp, &rots, &[lens], &cps);
        assert!(detect_after(&cps, &resid, &[DIMS, DIMS], &DepthConfig::default()).is_empty());
    }

    #[test]
    fn mask_cells_and_components() {
        let mut m = CoarseMask::default();
        m.set_norm([0.0, 0.0], DIMS);
        assert_eq!(m.count(), 1);
        assert!(m.contains_norm([0.001, 0.001], DIMS));
        assert!(!m.contains_norm([0.4, 0.3], DIMS));
        assert_eq!(m.dilated(1).count(), 9);
        let mut cnt = [[0usize; 8]; 8];
        cnt[0][0] = 3;
        cnt[1][1] = 2;
        cnt[5][5] = 4;
        assert_eq!(largest_component(&cnt), 5);
    }

    #[test]
    fn region_follows_the_pair_in_either_order() {
        let mut ma = CoarseMask::default();
        ma.set_norm([0.0, 0.0], DIMS);
        let mut mb = CoarseMask::default();
        mb.set_norm([0.3, 0.0], DIMS);
        let r = NearRegion { a: 0, b: 1, mask_a: ma, mask_b: mb, dims_a: DIMS, dims_b: DIMS, shift_px: 10.0, count: 9, source: Source::Residuals, pts_a: Vec::new(), pts_b: Vec::new() };
        let fwd = ControlPoint::new(0, 1, [0.0, 0.0], [0.3, 0.0], false);
        let rev = ControlPoint::new(1, 0, [0.3, 0.0], [0.0, 0.0], false);
        let other = ControlPoint::new(0, 2, [0.0, 0.0], [0.3, 0.0], false);
        assert!(r.covers(&fwd) && r.covers(&rev) && !r.covers(&other));
        // both ends must lie on the object
        assert!(!r.covers(&ControlPoint::new(0, 1, [0.0, 0.0], [-0.3, 0.0], false)));
    }
}
