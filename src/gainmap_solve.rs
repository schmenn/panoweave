//! Robust least squares for the gain maps: conjugate gradients without building a matrix
//! (a dense solver would be far too slow with 100+ photos), repeated with Huber weights.

/// One comparison: `sum(coef * g[nodes]) = d[channel]` (four grid points of photo A with their
/// bilinear weights, four of photo B with negated weights).
pub struct Obs {
    pub nodes: [u32; 8],
    pub coef: [f32; 8],
    pub d: [f32; 3],
}

pub struct SolveParams {
    /// number of unknowns (grid points of all photos)
    pub n: usize,
    /// weight of the squared difference of neighbouring grid points
    pub lam: f64,
    /// weight of the squared grid values (pull towards 0)
    pub anchor: f64,
    /// differences above this get less weight (Huber)
    pub huber: f64,
    pub irls_iters: usize,
    pub cg_iters: usize,
    pub cg_tol: f64,
}

/// y = (sum w a aT + lam * L + anchor * I) x
fn apply(obs: &[Obs], w: &[f32], links: &[(u32, u32)], p: &SolveParams, x: &[f64], y: &mut [f64]) {
    y.iter_mut().zip(x).for_each(|(y, x)| *y = p.anchor * x);
    for (o, &wk) in obs.iter().zip(w) {
        let mut r = 0.0;
        for j in 0..8 {
            r += o.coef[j] as f64 * x[o.nodes[j] as usize];
        }
        let s = wk as f64 * r;
        for j in 0..8 {
            y[o.nodes[j] as usize] += o.coef[j] as f64 * s;
        }
    }
    for &(a, b) in links {
        let d = p.lam * (x[a as usize] - x[b as usize]);
        y[a as usize] += d;
        y[b as usize] -= d;
    }
}

/// Jacobi preconditioner (inverse of the diagonal).
fn inv_diag(obs: &[Obs], w: &[f32], links: &[(u32, u32)], p: &SolveParams) -> Vec<f64> {
    let mut d = vec![p.anchor; p.n];
    for (o, &wk) in obs.iter().zip(w) {
        for j in 0..8 {
            d[o.nodes[j] as usize] += wk as f64 * (o.coef[j] as f64).powi(2);
        }
    }
    for &(a, b) in links {
        d[a as usize] += p.lam;
        d[b as usize] += p.lam;
    }
    d.iter().map(|&v| if v > 1e-12 { 1.0 / v } else { 1.0 }).collect()
}

/// Preconditioned conjugate gradients from the start value `x`. False when cancelled.
fn cg(obs: &[Obs], w: &[f32], links: &[(u32, u32)], p: &SolveParams, b: &[f64], x: &mut [f64]) -> bool {
    let m = inv_diag(obs, w, links, p);
    let mut ax = vec![0.0; p.n];
    apply(obs, w, links, p, x, &mut ax);
    let mut r: Vec<f64> = b.iter().zip(&ax).map(|(b, a)| b - a).collect();
    let mut z: Vec<f64> = r.iter().zip(&m).map(|(r, m)| r * m).collect();
    let mut dir = z.clone();
    let mut rz: f64 = r.iter().zip(&z).map(|(a, b)| a * b).sum();
    let bnorm = b.iter().map(|v| v * v).sum::<f64>().sqrt().max(1e-30);
    let mut q = vec![0.0; p.n];
    for it in 0..p.cg_iters {
        if it % 16 == 0 && crate::engine::cancelled() {
            return false;
        }
        apply(obs, w, links, p, &dir, &mut q);
        let dq: f64 = dir.iter().zip(&q).map(|(a, b)| a * b).sum();
        if dq <= 0.0 {
            break;
        }
        let alpha = rz / dq;
        for i in 0..p.n {
            x[i] += alpha * dir[i];
            r[i] -= alpha * q[i];
        }
        if r.iter().map(|v| v * v).sum::<f64>().sqrt() < p.cg_tol * bnorm {
            break;
        }
        for i in 0..p.n {
            z[i] = r[i] * m[i];
        }
        let rz_new: f64 = r.iter().zip(&z).map(|(a, b)| a * b).sum();
        let beta = rz_new / rz;
        rz = rz_new;
        for i in 0..p.n {
            dir[i] = z[i] + beta * dir[i];
        }
    }
    true
}

/// Robust solution for one colour channel (None when cancelled).
pub fn solve_channel(obs: &[Obs], links: &[(u32, u32)], p: &SolveParams, ch: usize) -> Option<Vec<f64>> {
    let mut x = vec![0.0f64; p.n];
    let mut w = vec![1.0f32; obs.len()];
    for round in 0..p.irls_iters.max(1) {
        if round > 0 {
            for (o, wk) in obs.iter().zip(w.iter_mut()) {
                let r: f64 = (0..8).map(|j| o.coef[j] as f64 * x[o.nodes[j] as usize]).sum::<f64>() - o.d[ch] as f64;
                *wk = if r.abs() <= p.huber { 1.0 } else { (p.huber / r.abs()) as f32 };
            }
        }
        let mut b = vec![0.0f64; p.n];
        for (o, &wk) in obs.iter().zip(&w) {
            for j in 0..8 {
                b[o.nodes[j] as usize] += wk as f64 * o.coef[j] as f64 * o.d[ch] as f64;
            }
        }
        if !cg(obs, &w, links, p, &b, &mut x) {
            return None;
        }
    }
    Some(x)
}
