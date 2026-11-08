//! Camera model and panorama alignment (rotation-only cameras, shared lenses).
//!
//! Image coordinates are normalised: u = (x + 0.5 - w/2) / L, v = (y + 0.5 - h/2) / L with
//! L = the long side in pixels, so the numbers do not depend on the image resolution.

use crate::geometry::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Lens {
    /// focal length / long side
    pub f: f64,
    /// radial distortion
    pub k1: f64,
    /// field of view came from the photo's information
    pub from_exif: bool,
}

impl Lens {
    pub fn from_hfov_deg(deg: f64) -> Self {
        Lens { f: 0.5 / (deg.to_radians() * 0.5).tan(), k1: 0.0, from_exif: false }
    }
    pub fn hfov_deg(&self) -> f64 {
        (2.0 * (0.5 / self.f).atan()).to_degrees()
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ControlPoint {
    pub a: usize,
    pub b: usize,
    pub pa: [f64; 2],
    pub pb: [f64; 2],
    /// last alignment error in pixels (of the original photo)
    #[serde(default)]
    pub err: f64,
    /// placed by hand
    #[serde(default)]
    pub manual: bool,
    /// weight in the optimisation (1 = normal point)
    #[serde(default = "one")]
    pub weight: f64,
    /// point or control area
    #[serde(default)]
    pub kind: CpKind,
    /// confidence of the last patch match (0..1), 0 = not checked
    #[serde(default)]
    pub conf: f32,
    /// shift found by the last patch match (pixels of the working image)
    #[serde(default)]
    pub shift: [f32; 2],
    /// correlation of the last fine-tuning (0 = not fine-tuned), as the original keeps it
    #[serde(default)]
    pub score: f32,
    /// protected: no bulk action (clean-up, outlier or duplicate removal, delete all, new search)
    /// removes or changes it; deleting or dragging it one by one needs unlocking first
    #[serde(default)]
    pub locked: bool,
}

fn one() -> f64 {
    1.0
}

/// What a control pair stands for.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum CpKind {
    /// a point from the point finder or placed by hand
    #[default]
    Point,
    /// automatic control area (grid patch in the overlap)
    Area,
    /// control area drawn by hand
    AreaManual,
}

impl ControlPoint {
    pub fn new(a: usize, b: usize, pa: [f64; 2], pb: [f64; 2], manual: bool) -> Self {
        ControlPoint { a, b, pa, pb, err: 0.0, manual, weight: 1.0, kind: CpKind::Point, conf: 0.0, shift: [0.0; 2], score: 0.0, locked: false }
    }
}

/// Bulk removal that spares protected points: removes every point for which `remove` is true
/// unless it is locked. Returns how many protected points were kept although `remove` hit them.
pub fn remove_unprotected(cps: &mut Vec<ControlPoint>, remove: impl Fn(&ControlPoint) -> bool) -> usize {
    let kept = cps.iter().filter(|c| c.locked && remove(c)).count();
    cps.retain(|c| c.locked || !remove(c));
    kept
}

/// Takes the protected points out of an index list; returns the rest and how many were dropped.
pub fn split_protected(cps: &[ControlPoint], idx: Vec<usize>) -> (Vec<usize>, usize) {
    let n = idx.len();
    let rest: Vec<usize> = idx.into_iter().filter(|&k| !cps.get(k).is_some_and(|c| c.locked)).collect();
    let dropped = n - rest.len();
    (rest, dropped)
}

/// Status text for bulk actions that left protected points alone ("" when there were none).
pub fn protected_note(n: usize) -> String {
    match n {
        0 => String::new(),
        1 => " 1 protected point kept.".into(),
        n => format!(" {n} protected points kept."),
    }
}

/// Normalised image point -> camera ray.
#[inline]
pub fn ray(lens: &Lens, p: [f64; 2]) -> V3 {
    let (mut u, mut v) = (p[0], p[1]);
    if lens.k1 != 0.0 {
        for _ in 0..5 {
            let s = 1.0 + lens.k1 * (u * u + v * v);
            u = p[0] / s;
            v = p[1] / s;
        }
    }
    normalize([u, v, lens.f])
}

/// Camera-space direction -> normalised image point.
#[inline]
pub fn project(lens: &Lens, c: V3) -> Option<[f64; 2]> {
    if c[2] <= 1e-6 {
        return None;
    }
    let u = lens.f * c[0] / c[2];
    let v = lens.f * c[1] / c[2];
    let r2 = u * u + v * v;
    // outside the range where the lens model is valid (avoids folding far-away rays into the photo)
    if r2 > 4.0 || (lens.k1 < 0.0 && r2 * -3.0 * lens.k1 > 0.9) {
        return None;
    }
    let s = 1.0 + lens.k1 * r2;
    Some([u * s, v * s])
}

pub struct AlignInput<'a> {
    pub n: usize,
    pub lens_of: &'a [usize],
    pub long_side: &'a [f64],
}

/// Connected groups (by control points). Returns group id per image.
pub fn components(n: usize, cps: &[ControlPoint]) -> Vec<usize> {
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], x: usize) -> usize {
        let mut r = x;
        while p[r] != r {
            r = p[r];
        }
        let mut c = x;
        while p[c] != r {
            let nx = p[c];
            p[c] = r;
            c = nx;
        }
        r
    }
    for cp in cps {
        if cp.a < n && cp.b < n {
            let (ra, rb) = (find(&mut parent, cp.a), find(&mut parent, cp.b));
            if ra != rb {
                parent[ra] = rb;
            }
        }
    }
    let roots: Vec<usize> = (0..n).map(|i| find(&mut parent, i)).collect();
    let mut ids = std::collections::HashMap::new();
    roots
        .iter()
        .map(|r| {
            let k = ids.len();
            *ids.entry(*r).or_insert(k)
        })
        .collect()
}

fn pair_counts(n: usize, cps: &[ControlPoint]) -> Vec<Vec<usize>> {
    let mut c = vec![vec![0usize; n]; n];
    for cp in cps {
        c[cp.a][cp.b] += 1;
        c[cp.b][cp.a] += 1;
    }
    c
}

/// Initial rotations: maximum spanning tree over the match graph, each edge solved in closed form.
pub fn initial_rotations(inp: &AlignInput, lenses: &[Lens], cps: &[ControlPoint], anchor: usize) -> Vec<M3> {
    let n = inp.n;
    let counts = pair_counts(n, cps);
    let mut rots = vec![IDENTITY; n];
    let mut done = vec![false; n];
    if n == 0 {
        return rots;
    }
    done[anchor] = true;
    loop {
        // best edge from a done to a not-done image
        let mut best: Option<(usize, usize, usize)> = None;
        for i in 0..n {
            if !done[i] {
                continue;
            }
            for j in 0..n {
                if done[j] || counts[i][j] == 0 {
                    continue;
                }
                if best.map(|b| counts[i][j] > b.2).unwrap_or(true) {
                    best = Some((i, j, counts[i][j]));
                }
            }
        }
        let Some((i, j, _)) = best else { break };
        // pairs (ray in i, ray in j)
        let mut pairs = Vec::new();
        for cp in cps {
            if cp.a == i && cp.b == j {
                pairs.push((ray(&lenses[inp.lens_of[i]], cp.pa), ray(&lenses[inp.lens_of[j]], cp.pb)));
            } else if cp.a == j && cp.b == i {
                pairs.push((ray(&lenses[inp.lens_of[i]], cp.pb), ray(&lenses[inp.lens_of[j]], cp.pa)));
            }
        }
        let rel = horn_rotation(&pairs); // c_j = rel * c_i
        rots[j] = orthonormalize(&mm(&rel, &rots[i]));
        done[j] = true;
    }
    // unconnected images: place to the right of the last one so they are at least visible
    let mut yaw = 0.0;
    for i in 0..n {
        if !done[i] {
            yaw += 0.6;
            rots[i] = mm(&rots[anchor], &camera_from_ypr(yaw, 0.0, 0.0));
        }
    }
    rots
}

/// Residual of one control point (both directions), in normalised units of the target image.
#[inline]
fn cp_residual(cp: &ControlPoint, ra: &M3, rb: &M3, la: &Lens, lb: &Lens) -> [f64; 4] {
    let wa = mtv(ra, ray(la, cp.pa));
    let wb = mtv(rb, ray(lb, cp.pb));
    let pb = project(lb, mv(rb, wa));
    let pa = project(la, mv(ra, wb));
    let big = 0.5;
    let r1 = match pb {
        Some(p) => [p[0] - cp.pb[0], p[1] - cp.pb[1]],
        None => [big, big],
    };
    let r2 = match pa {
        Some(p) => [p[0] - cp.pa[0], p[1] - cp.pa[1]],
        None => [big, big],
    };
    [r1[0], r1[1], r2[0], r2[1]]
}

pub fn total_cost(rots: &[M3], lens_of: &[usize], lenses: &[Lens], cps: &[ControlPoint]) -> f64 {
    cps.iter()
        .map(|cp| {
            let r = cp_residual(cp, &rots[cp.a], &rots[cp.b], &lenses[lens_of[cp.a]], &lenses[lens_of[cp.b]]);
            cp.weight * r.iter().map(|v| v * v).sum::<f64>()
        })
        .sum()
}

pub struct OptimizeOptions {
    pub focal: bool,
    pub distortion: bool,
    /// Cauchy instead of Huber robust loss
    pub cauchy: bool,
}

/// Levenberg-Marquardt bundle adjustment over rotations (+ optional lens parameters).
pub fn optimize(
    inp: &AlignInput,
    rots: &mut [M3],
    lenses: &mut [Lens],
    cps: &[ControlPoint],
    anchor: usize,
    opts: &OptimizeOptions,
    iterations: usize,
) -> f64 {
    let n = inp.n;
    let nl = lenses.len();
    // parameter layout
    let mut rot_idx = vec![usize::MAX; n];
    let mut np = 0;
    let used: Vec<bool> = {
        let mut u = vec![false; n];
        for cp in cps {
            u[cp.a] = true;
            u[cp.b] = true;
        }
        u
    };
    for i in 0..n {
        if i != anchor && used[i] {
            rot_idx[i] = np;
            np += 3;
        }
    }
    let mut f_idx = vec![usize::MAX; nl];
    let mut k_idx = vec![usize::MAX; nl];
    let lens_used: Vec<bool> = (0..nl).map(|l| (0..n).any(|i| used[i] && inp.lens_of[i] == l)).collect();
    for l in 0..nl {
        if !lens_used[l] || lenses[l].from_exif && !opts.focal {
            continue;
        }
        if opts.focal {
            f_idx[l] = np;
            np += 1;
        }
    }
    if opts.distortion {
        for l in 0..nl {
            if lens_used[l] {
                k_idx[l] = np;
                np += 1;
            }
        }
    }
    if np == 0 || cps.is_empty() {
        return total_cost(rots, inp.lens_of, lenses, cps);
    }

    let mut lambda = 1e-3;
    let mut cost = total_cost(rots, inp.lens_of, lenses, cps);
    for _it in 0..iterations {
        if crate::engine::cancelled() {
            break;
        }
        // build normal equations with local numeric derivatives
        let mut jtj = vec![vec![0.0; np]; np];
        let mut jtr = vec![0.0; np];
        let eps = 1e-6;
        for cp in cps {
            let la_i = inp.lens_of[cp.a];
            let lb_i = inp.lens_of[cp.b];
            let base = cp_residual(cp, &rots[cp.a], &rots[cp.b], &lenses[la_i], &lenses[lb_i]);
            // robust weight (Huber, ~3 px on a 3000 px photo)
            let rn = (base[0] * base[0] + base[1] * base[1] + base[2] * base[2] + base[3] * base[3]).sqrt();
            let k = 0.001;
            let robust = if opts.cauchy { 1.0 / (1.0 + (rn / k) * (rn / k)) } else if rn < k { 1.0 } else { k / rn };
            let wgt = robust * cp.weight;
            let mut cols: Vec<(usize, [f64; 4])> = Vec::with_capacity(10);
            for (img, which) in [(cp.a, 0), (cp.b, 1)] {
                let pi = rot_idx[img];
                if pi == usize::MAX {
                    continue;
                }
                for d in 0..3 {
                    let mut w = [0.0; 3];
                    w[d] = eps;
                    let pert = mm(&rodrigues(w), &rots[img]);
                    let r = if which == 0 {
                        cp_residual(cp, &pert, &rots[cp.b], &lenses[la_i], &lenses[lb_i])
                    } else {
                        cp_residual(cp, &rots[cp.a], &pert, &lenses[la_i], &lenses[lb_i])
                    };
                    cols.push((pi + d, [(r[0] - base[0]) / eps, (r[1] - base[1]) / eps, (r[2] - base[2]) / eps, (r[3] - base[3]) / eps]));
                }
            }
            let mut lens_params: Vec<(usize, usize, bool)> = Vec::new();
            for l in [la_i, lb_i] {
                if f_idx[l] != usize::MAX && !lens_params.iter().any(|p| p.0 == l && !p.2) {
                    lens_params.push((l, f_idx[l], false));
                }
                if k_idx[l] != usize::MAX && !lens_params.iter().any(|p| p.0 == l && p.2) {
                    lens_params.push((l, k_idx[l], true));
                }
            }
            for (l, pi, is_k) in lens_params {
                let mut lp = lenses.to_vec();
                if is_k {
                    lp[l].k1 += eps;
                } else {
                    lp[l].f *= 1.0 + eps;
                }
                let r = cp_residual(cp, &rots[cp.a], &rots[cp.b], &lp[la_i], &lp[lb_i]);
                cols.push((pi, [(r[0] - base[0]) / eps, (r[1] - base[1]) / eps, (r[2] - base[2]) / eps, (r[3] - base[3]) / eps]));
            }
            for (pi, ci) in &cols {
                for k in 0..4 {
                    jtr[*pi] += wgt * ci[k] * base[k];
                }
                for (pj, cj) in &cols {
                    let mut s = 0.0;
                    for k in 0..4 {
                        s += ci[k] * cj[k];
                    }
                    jtj[*pi][*pj] += wgt * s;
                }
            }
        }
        // try steps with increasing damping
        let mut improved = false;
        for _try in 0..8 {
            let mut a = jtj.clone();
            for i in 0..np {
                a[i][i] += lambda * (jtj[i][i] + 1e-9);
            }
            let b: Vec<f64> = jtr.iter().map(|v| -v).collect();
            let Some(delta) = solve(a, b) else {
                lambda *= 10.0;
                continue;
            };
            let mut nr = rots.to_vec();
            let mut nlz = lenses.to_vec();
            for i in 0..n {
                let pi = rot_idx[i];
                if pi != usize::MAX {
                    nr[i] = orthonormalize(&mm(&rodrigues([delta[pi], delta[pi + 1], delta[pi + 2]]), &rots[i]));
                }
            }
            for l in 0..nl {
                if f_idx[l] != usize::MAX {
                    nlz[l].f = (nlz[l].f * (1.0 + delta[f_idx[l]])).clamp(0.05, 20.0);
                }
                if k_idx[l] != usize::MAX {
                    nlz[l].k1 = (nlz[l].k1 + delta[k_idx[l]]).clamp(-0.6, 0.6);
                }
            }
            let nc = total_cost(&nr, inp.lens_of, &nlz, cps);
            if nc < cost {
                let rel = (cost - nc) / cost.max(1e-30);
                rots.copy_from_slice(&nr);
                lenses.copy_from_slice(&nlz);
                cost = nc;
                lambda = (lambda * 0.3).max(1e-9);
                improved = true;
                if rel < 1e-7 {
                    return cost;
                }
                break;
            } else {
                lambda *= 8.0;
            }
        }
        if !improved {
            break;
        }
    }
    cost
}

/// Error of each control point in pixels of the original photos.
pub fn cp_errors(inp: &AlignInput, rots: &[M3], lenses: &[Lens], cps: &mut [ControlPoint]) {
    for cp in cps.iter_mut() {
        let r = cp_residual(cp, &rots[cp.a], &rots[cp.b], &lenses[inp.lens_of[cp.a]], &lenses[inp.lens_of[cp.b]]);
        let eb = (r[0] * r[0] + r[1] * r[1]).sqrt() * inp.long_side[cp.b];
        let ea = (r[2] * r[2] + r[3] * r[3]).sqrt() * inp.long_side[cp.a];
        cp.err = 0.5 * (ea + eb);
    }
}

/// Try a range of fields of view for lenses without photo information and keep the best start.
pub fn guess_fov(inp: &AlignInput, lenses: &mut [Lens], cps: &[ControlPoint], anchor: usize) {
    let unknown: Vec<usize> = (0..lenses.len()).filter(|&l| !lenses[l].from_exif).collect();
    if unknown.is_empty() || cps.is_empty() {
        return;
    }
    let mut best = (f64::MAX, 50.0);
    for deg in [15.0, 22.0, 30.0, 38.0, 46.0, 55.0, 65.0, 75.0, 88.0, 100.0, 115.0] {
        if crate::engine::cancelled() {
            break;
        }
        let mut ls = lenses.to_vec();
        for &l in &unknown {
            ls[l] = Lens::from_hfov_deg(deg);
        }
        let mut rots = initial_rotations(inp, &ls, cps, anchor);
        optimize(inp, &mut rots, &mut ls, cps, anchor, &OptimizeOptions { focal: false, distortion: false, cauchy: false }, 6);
        let c = total_cost(&rots, inp.lens_of, &ls, cps);
        if c < best.0 {
            best = (c, deg);
        }
    }
    for &l in &unknown {
        lenses[l] = Lens::from_hfov_deg(best.1);
    }
}

/// Rotate the whole panorama so the horizon is level and the middle of the photos looks forward.
pub fn straighten(rots: &mut [M3], use_mask: &[bool]) {
    let idx: Vec<usize> = (0..rots.len()).filter(|&i| use_mask[i]).collect();
    if idx.is_empty() {
        return;
    }
    let mut down_avg = [0.0; 3];
    let mut fwd_avg = [0.0; 3];
    let mut cov = vec![vec![0.0; 3]; 3];
    for &i in &idx {
        let r = &rots[i];
        let x = r[0];
        for a in 0..3 {
            for b in 0..3 {
                cov[a][b] += x[a] * x[b];
            }
            down_avg[a] += r[1][a];
            fwd_avg[a] += r[2][a];
        }
    }
    let down = if idx.len() >= 2 {
        let (vals, vecs) = jacobi_eigen(&cov);
        let mut k = 0;
        for i in 1..3 {
            if vals[i] < vals[k] {
                k = i;
            }
        }
        let mut d = normalize([vecs[0][k], vecs[1][k], vecs[2][k]]);
        if dot(d, down_avg) < 0.0 {
            d = [-d[0], -d[1], -d[2]];
        }
        d
    } else {
        normalize(down_avg)
    };
    let mut fwd = [
        fwd_avg[0] - dot(fwd_avg, down) * down[0],
        fwd_avg[1] - dot(fwd_avg, down) * down[1],
        fwd_avg[2] - dot(fwd_avg, down) * down[2],
    ];
    if norm(fwd) < 0.2 * idx.len() as f64 {
        // full 360: use first photo's direction
        let f0 = rots[idx[0]][2];
        fwd = [f0[0] - dot(f0, down) * down[0], f0[1] - dot(f0, down) * down[1], f0[2] - dot(f0, down) * down[2]];
    }
    let z = normalize(fwd);
    let y = down;
    let x = cross(y, z);
    let q: M3 = [x, y, z];
    let qt = transpose(&q);
    for r in rots.iter_mut() {
        *r = orthonormalize(&mm(r, &qt));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn old_project_points_load_unprotected() {
        let old = r#"{"a":0,"b":1,"pa":[0.1,0.2],"pb":[0.3,0.4]}"#;
        let c: ControlPoint = serde_json::from_str(old).unwrap();
        assert!(!c.locked);
        let mut c2 = c;
        c2.locked = true;
        let back: ControlPoint = serde_json::from_str(&serde_json::to_string(&c2).unwrap()).unwrap();
        assert!(back.locked);
    }

    #[test]
    fn bulk_removal_and_index_lists_spare_protected_points() {
        let mut cps: Vec<ControlPoint> = (0..6).map(|i| ControlPoint::new(0, 1, [i as f64 * 0.1, 0.0], [0.0; 2], false)).collect();
        cps[1].locked = true;
        cps[4].locked = true;
        let (rest, dropped) = split_protected(&cps, vec![0, 1, 2, 4]);
        assert_eq!((rest, dropped), (vec![0, 2], 2));
        // "delete all in pair" / clean up: everything matches, only the locked two stay
        let kept = remove_unprotected(&mut cps, |_| true);
        assert_eq!(kept, 2);
        assert_eq!(cps.len(), 2);
        assert!(cps.iter().all(|c| c.locked));
        assert_eq!(protected_note(0), "");
        assert_eq!(protected_note(2), " 2 protected points kept.");
    }

    use super::*;

    fn m_dist(a: &M3, b: &M3) -> f64 {
        (0..3).flat_map(|i| (0..3).map(move |j| (a[i][j] - b[i][j]).abs())).fold(0.0, f64::max)
    }

    /// Control points between cameras with known rotations (world -> camera), no noise.
    fn synth_cps(rots: &[M3], lens: &Lens, pairs: &[(usize, usize)], per_pair: usize, seed: u64) -> Vec<ControlPoint> {
        let mut rng = Rng(seed);
        let mut cps = Vec::new();
        for &(a, b) in pairs {
            let mut n = 0;
            let mut guard = 0;
            while n < per_pair && guard < 100_000 {
                guard += 1;
                // random world direction around the middle between both cameras
                let ca = mtv(&rots[a], [0.0, 0.0, 1.0]);
                let cb = mtv(&rots[b], [0.0, 0.0, 1.0]);
                let mid = normalize([ca[0] + cb[0], ca[1] + cb[1], ca[2] + cb[2]]);
                let d = normalize([mid[0] + 0.4 * rng.gauss(), mid[1] + 0.3 * rng.gauss(), mid[2] + 0.4 * rng.gauss()]);
                let (Some(pa), Some(pb)) = (project(lens, mv(&rots[a], d)), project(lens, mv(&rots[b], d))) else { continue };
                let inside = |p: [f64; 2]| p[0].abs() < 0.5 && p[1].abs() < 0.375;
                if inside(pa) && inside(pb) {
                    cps.push(ControlPoint::new(a, b, pa, pb, false));
                    n += 1;
                }
            }
            assert_eq!(n, per_pair, "could not generate points");
        }
        cps
    }

    #[test]
    fn lens_fov_roundtrip() {
        for deg in [20.0, 53.0, 90.0, 120.0] {
            let l = Lens::from_hfov_deg(deg);
            assert!((l.hfov_deg() - deg).abs() < 1e-9);
            assert_eq!(l.k1, 0.0);
        }
        // 90 degrees across the long side -> focal length is half the long side
        assert!((Lens::from_hfov_deg(90.0).f - 0.5).abs() < 1e-12);
    }

    #[test]
    fn ray_project_roundtrip() {
        for k1 in [0.0, -0.05, 0.03] {
            let lens = Lens { f: 0.8, k1, from_exif: true };
            for p in [[0.0, 0.0], [0.3, 0.2], [-0.45, 0.1], [0.1, -0.35]] {
                let r = ray(&lens, p);
                assert!((norm(r) - 1.0).abs() < 1e-12);
                let q = project(&lens, r).unwrap();
                assert!((q[0] - p[0]).abs() < 1e-6 && (q[1] - p[1]).abs() < 1e-6, "k1 {k1} {p:?} -> {q:?}");
            }
        }
        let lens = Lens::from_hfov_deg(60.0);
        assert_eq!(ray(&lens, [0.0, 0.0]), [0.0, 0.0, 1.0]);
        // behind the camera
        assert!(project(&lens, [0.0, 0.0, -1.0]).is_none());
    }

    #[test]
    fn control_point_defaults() {
        let cp = ControlPoint::new(1, 2, [0.1, 0.2], [0.3, 0.4], true);
        assert_eq!((cp.a, cp.b), (1, 2));
        assert!(cp.manual);
        assert_eq!(cp.weight, 1.0);
        assert_eq!(cp.kind, CpKind::Point);
        assert_eq!(cp.err, 0.0);
    }

    #[test]
    fn components_group_connected_photos() {
        let cps = vec![ControlPoint::new(0, 1, [0.0; 2], [0.0; 2], false), ControlPoint::new(2, 3, [0.0; 2], [0.0; 2], false)];
        let c = components(5, &cps);
        assert_eq!(c.len(), 5);
        assert_eq!(c[0], c[1]);
        assert_eq!(c[2], c[3]);
        assert_ne!(c[0], c[2]);
        assert_ne!(c[4], c[0]);
        assert_ne!(c[4], c[2]);
    }

    #[test]
    fn rotation_free_input_stays_identity() {
        // identical coordinates in both photos: no rotation between them
        let lens = Lens::from_hfov_deg(60.0);
        let cps: Vec<ControlPoint> = (0..12)
            .map(|i| {
                let p = [-0.4 + 0.07 * i as f64, -0.3 + 0.05 * i as f64];
                ControlPoint::new(0, 1, p, p, false)
            })
            .collect();
        let lens_of = [0usize, 0];
        let long = [1000.0, 1000.0];
        let inp = AlignInput { n: 2, lens_of: &lens_of, long_side: &long };
        let lenses = [lens];
        let mut rots = initial_rotations(&inp, &lenses, &cps, 0);
        assert!(m_dist(&rots[0], &IDENTITY) < 1e-9);
        assert!(m_dist(&rots[1], &IDENTITY) < 1e-6);
        let mut ls = lenses.to_vec();
        let cost = optimize(&inp, &mut rots, &mut ls, &cps, 0, &OptimizeOptions { focal: false, distortion: false, cauchy: false }, 10);
        assert!(cost < 1e-12, "cost {cost}");
        assert!(m_dist(&rots[1], &IDENTITY) < 1e-6);
        assert_eq!(ls[0], lens);
        assert!(total_cost(&rots, &lens_of, &ls, &cps) < 1e-12);
        let mut cps = cps;
        cp_errors(&inp, &rots, &ls, &mut cps);
        assert!(cps.iter().all(|c| c.err < 1e-6));
    }

    #[test]
    fn initial_rotations_recover_known_geometry() {
        let lens = Lens::from_hfov_deg(50.0);
        let truth = [camera_from_ypr(0.0, 0.0, 0.0), camera_from_ypr(0.35, 0.02, 0.0), camera_from_ypr(0.7, -0.03, 0.01)];
        let cps = synth_cps(&truth, &lens, &[(0, 1), (1, 2)], 25, 11);
        let lens_of = [0usize; 3];
        let long = [1000.0; 3];
        let inp = AlignInput { n: 3, lens_of: &lens_of, long_side: &long };
        let rots = initial_rotations(&inp, &[lens], &cps, 0);
        for i in 0..3 {
            assert!(m_dist(&rots[i], &truth[i]) < 1e-6, "photo {i}");
        }
    }

    #[test]
    fn optimize_fixes_perturbed_rotations() {
        let lens = Lens::from_hfov_deg(50.0);
        let truth = [camera_from_ypr(0.0, 0.0, 0.0), camera_from_ypr(0.35, 0.02, 0.0), camera_from_ypr(0.7, -0.03, 0.01)];
        let cps = synth_cps(&truth, &lens, &[(0, 1), (1, 2), (0, 2)], 20, 12);
        let lens_of = [0usize; 3];
        let long = [1000.0; 3];
        let inp = AlignInput { n: 3, lens_of: &lens_of, long_side: &long };
        let mut rots = vec![
            truth[0],
            mm(&truth[1], &rodrigues([0.03, -0.02, 0.01])),
            mm(&truth[2], &rodrigues([-0.02, 0.04, -0.02])),
        ];
        let mut ls = vec![lens];
        let before = total_cost(&rots, &lens_of, &ls, &cps);
        let after = optimize(&inp, &mut rots, &mut ls, &cps, 0, &OptimizeOptions { focal: false, distortion: false, cauchy: false }, 50);
        assert!(after < before * 1e-3 + 1e-12, "before {before} after {after}");
        for i in 0..3 {
            assert!(m_dist(&rots[i], &truth[i]) < 1e-3, "photo {i}");
        }
    }

    #[test]
    fn optimize_estimates_focal_length() {
        let lens = Lens::from_hfov_deg(50.0);
        let truth = [camera_from_ypr(0.0, 0.0, 0.0), camera_from_ypr(0.35, 0.0, 0.0), camera_from_ypr(0.7, 0.0, 0.0)];
        let cps = synth_cps(&truth, &lens, &[(0, 1), (1, 2), (0, 2)], 30, 13);
        let lens_of = [0usize; 3];
        let long = [1000.0; 3];
        let inp = AlignInput { n: 3, lens_of: &lens_of, long_side: &long };
        let mut ls = vec![Lens::from_hfov_deg(58.0)];
        let mut rots = initial_rotations(&inp, &ls, &cps, 0);
        optimize(&inp, &mut rots, &mut ls, &cps, 0, &OptimizeOptions { focal: true, distortion: false, cauchy: false }, 80);
        assert!((ls[0].f - lens.f).abs() / lens.f < 0.01, "f {} vs {}", ls[0].f, lens.f);
    }

    #[test]
    fn guess_fov_picks_roughly_right_lens() {
        let lens = Lens::from_hfov_deg(46.0);
        let truth = [camera_from_ypr(0.0, 0.0, 0.0), camera_from_ypr(0.3, 0.0, 0.0), camera_from_ypr(0.6, 0.0, 0.0)];
        let cps = synth_cps(&truth, &lens, &[(0, 1), (1, 2), (0, 2)], 25, 14);
        let lens_of = [0usize; 3];
        let long = [1000.0; 3];
        let inp = AlignInput { n: 3, lens_of: &lens_of, long_side: &long };
        let mut ls = vec![Lens::from_hfov_deg(100.0)];
        guess_fov(&inp, &mut ls, &cps, 0);
        assert!((ls[0].hfov_deg() - 46.0).abs() < 10.0, "got {}", ls[0].hfov_deg());
        // lenses with photo information are left alone
        let mut known = vec![Lens { from_exif: true, ..Lens::from_hfov_deg(100.0) }];
        guess_fov(&inp, &mut known, &cps, 0);
        assert!((known[0].hfov_deg() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn straighten_leaves_level_panorama_alone() {
        let rots0: Vec<M3> = [-0.3, 0.0, 0.3].iter().map(|&y| camera_from_ypr(y, 0.0, 0.0)).collect();
        let mut rots = rots0.clone();
        straighten(&mut rots, &[true, true, true]);
        for i in 0..3 {
            assert!(m_dist(&rots[i], &rots0[i]) < 1e-9);
        }
    }

    #[test]
    fn straighten_levels_a_tilted_panorama() {
        let tilt = rodrigues([0.0, 0.0, 0.15]);
        let mut rots: Vec<M3> = [-0.4, -0.2, 0.0, 0.2, 0.4].iter().map(|&y| mm(&camera_from_ypr(y, 0.0, 0.0), &tilt)).collect();
        straighten(&mut rots, &[true; 5]);
        for r in &rots {
            // all rotations remain proper rotations ...
            assert!(m_dist(&mm(r, &transpose(r)), &IDENTITY) < 1e-9);
            // ... and the camera's "down" axis points down in the world again
            assert!((r[1][1] - 1.0).abs() < 1e-6 && r[1][0].abs() < 1e-6 && r[1][2].abs() < 1e-6, "{r:?}");
        }
    }

    #[test]
    fn straighten_ignores_empty_selection() {
        let mut rots = vec![camera_from_ypr(0.2, 0.1, 0.0)];
        let copy = rots.clone();
        straighten(&mut rots, &[false]);
        assert_eq!(rots, copy);
    }
}
