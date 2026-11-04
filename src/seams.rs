//! Seam finder: decides which photo shows where in the overlaps, so that the seams run where the
//! photos agree (and around objects that differ because of parallax) instead of through the
//! middle of the overlap.
//!
//! The decision is made once for the whole panorama on a coarse grid of blocks (a fixed number of
//! blocks along the long side, never derived from the available memory), by graph cuts with
//! alpha expansion (Boykov/Veksler/Zabih) on the energy
//!
//!   E = sum_p D(p, label_p) + sum_{p~q, label_p != label_q} V(p, q, label_p, label_q)
//!
//! * `D` prefers the photo with the best score (distance from the photo border, detail) and
//!   forbids photos that do not cover the block (or are masked out there).
//! * `V` is the colour difference of the two photos at the seam, plus a small constant per cut edge
//!   (short, smooth seams).
//!
//! The result is a [`LabelMap`]. The renderer asks it for a preferred photo for every output pixel
//! ([`LabelMap::preferred`], a vote of the four nearest blocks, so seams are not blocky). Because
//! the map exists once and every pixel is decided on its own, the picture does not depend on how
//! the render is cut into strips. All arithmetic of the optimisation is integer, so the result is
//! the same on every computer.

use serde::{Deserialize, Serialize};

/// Flags of a [`Sample`].
pub const VALID: u8 = 1;
/// The person painted "use this photo here" over the block.
pub const POSITIVE: u8 = 2;
/// The person painted "do not use this photo here" over the block.
pub const NEGATIVE: u8 = 4;

/// Label of a block no photo covers.
pub const NONE: u16 = u16::MAX;

/// Capacities and costs are integers: cost * `SCALE`.
const SCALE: f32 = 1000.0;
/// "Never": larger than any sum of real costs.
const INF: i64 = 1 << 40;
/// Added to the data cost of a photo that is practically worthless where it is used (an unconnected photo,
/// see `stitch::UNTRUSTED_QUALITY`), so it only fills gaps.
const WORTHLESS_RATIO: f32 = 1e-6;
const WORTHLESS_PENALTY: f32 = 50.0;

/// Settings of the seam finder. `SeamConfig::default()` is off: the seams are then found as before
/// (the photo with the best score wins at every pixel).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SeamConfig {
    pub enabled: bool,
    /// the grid has at most this many blocks along the long side of the panorama
    pub max_side: usize,
    /// weight of the "best photo here" preference (per block, relative to the best photo)
    pub data_weight: f32,
    /// weight of the colour difference at the seam
    pub smooth_weight: f32,
    /// constant cost of every block edge a seam crosses (keeps seams short and smooth)
    pub cut_cost: f32,
    /// colour difference assumed where one of the two photos does not cover a seam block
    pub missing_diff: f32,
    /// passes over all photos (stops early when nothing changes)
    pub cycles: usize,
}

impl Default for SeamConfig {
    fn default() -> Self {
        SeamConfig { enabled: false, max_side: 1536, data_weight: 0.05, smooth_weight: 1.0, cut_cost: 0.01, missing_diff: 0.5, cycles: 2 }
    }
}

impl SeamConfig {
    /// For experiments: `PW_SEAMS=1` switches the seam finder on, `PW_SEAM_DATA`, `PW_SEAM_SMOOTH`,
    /// `PW_SEAM_CUT` and `PW_SEAM_SIDE` change its settings.
    pub fn experiment() -> Self {
        let mut c = SeamConfig::default();
        let get = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<f32>().ok());
        c.enabled = std::env::var("PW_SEAMS").is_ok_and(|v| v == "1");
        if let Some(v) = get("PW_SEAM_DATA") {
            c.data_weight = v;
        }
        if let Some(v) = get("PW_SEAM_SMOOTH") {
            c.smooth_weight = v;
        }
        if let Some(v) = get("PW_SEAM_CUT") {
            c.cut_cost = v;
        }
        if let Some(v) = get("PW_SEAM_SIDE") {
            c.max_side = v as usize;
        }
        c
    }
}

/// What one photo shows in one block.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    /// colour (gain corrected, sRGB)
    pub rgb: [u8; 3],
    pub flags: u8,
    /// how much this photo is wanted here: higher is better (border distance, detail, blown-out pixels count less)
    pub score: f32,
}

/// The blocks of one photo: block rows `by0 .. by0 + rows`, `bw` blocks wide.
#[derive(Clone, Debug, Default)]
pub struct PhotoBlocks {
    pub by0: usize,
    pub rows: usize,
    pub samples: Vec<Sample>,
}

impl PhotoBlocks {
    pub fn new(by0: usize, rows: usize, bw: usize) -> Self {
        PhotoBlocks { by0, rows, samples: vec![Sample::default(); rows * bw] }
    }
    #[inline]
    fn at(&self, bw: usize, bx: usize, by: usize) -> Option<&Sample> {
        if by < self.by0 || by >= self.by0 + self.rows {
            return None;
        }
        let s = &self.samples[(by - self.by0) * bw + bx];
        (s.flags & VALID != 0).then_some(s)
    }
}

/// Which photo shows where, on the block grid.
#[derive(Clone, Debug)]
pub struct LabelMap {
    pub bw: usize,
    pub bh: usize,
    /// output pixels per block (along each side)
    pub bsz: usize,
    pub labels: Vec<u16>,
}

impl LabelMap {
    /// The photo that should show output pixel (`px`, `py`): the label with the largest share among the four
    /// nearest block centres (bilinear weights), the lower photo number on a tie. `NONE` if no block around is covered.
    pub fn preferred(&self, px: usize, py: usize) -> u16 {
        let fx = (px as f32 + 0.5) / self.bsz as f32 - 0.5;
        let fy = (py as f32 + 0.5) / self.bsz as f32 - 0.5;
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = (fx - x0, fy - y0);
        let clamp = |v: f32, n: usize| (v.max(0.0) as usize).min(n - 1);
        let (xa, xb) = (clamp(x0, self.bw), clamp(x0 + 1.0, self.bw));
        let (ya, yb) = (clamp(y0, self.bh), clamp(y0 + 1.0, self.bh));
        let cand = [
            (self.labels[ya * self.bw + xa], (1.0 - tx) * (1.0 - ty)),
            (self.labels[ya * self.bw + xb], tx * (1.0 - ty)),
            (self.labels[yb * self.bw + xa], (1.0 - tx) * ty),
            (self.labels[yb * self.bw + xb], tx * ty),
        ];
        let mut best = (NONE, -1.0f32);
        for (i, &(l, _)) in cand.iter().enumerate() {
            if l == NONE {
                continue;
            }
            let wsum: f32 = cand.iter().filter(|c| c.0 == l).map(|c| c.1).sum();
            if wsum > best.1 || (wsum == best.1 && l < best.0) {
                best = (l, wsum);
            }
            let _ = i;
        }
        best.0
    }

    /// The preferred photos of the output rows `[ya, yb)` of a panorama `w` pixels wide.
    pub fn preferred_rows(&self, w: usize, (ya, yb): (usize, usize)) -> Vec<u16> {
        use rayon::prelude::*;
        let mut out = vec![NONE; w * (yb - ya)];
        out.par_chunks_mut(w).enumerate().for_each(|(ry, row)| {
            for (px, v) in row.iter_mut().enumerate() {
                *v = self.preferred(px, ya + ry);
            }
        });
        out
    }
}

/// Grid size for a panorama of `w` x `h` pixels: (block size in pixels, blocks wide, blocks high).
pub fn grid(w: usize, h: usize, max_side: usize) -> (usize, usize, usize) {
    let bsz = w.max(h).div_ceil(max_side.max(8)).max(1);
    (bsz, w.div_ceil(bsz), h.div_ceil(bsz))
}

// ---------------------------------------------------------------------------------------------
// costs

struct Problem<'a> {
    bw: usize,
    bh: usize,
    photos: &'a [PhotoBlocks],
    cfg: SeamConfig,
    /// per block: 0 = every covering photo may be used, 1 = except the masked-out ones, 2 = only the ones painted positive
    mode: Vec<u8>,
    /// per block: the best score among the photos that may be used
    best: Vec<f32>,
}

impl<'a> Problem<'a> {
    fn new(photos: &'a [PhotoBlocks], bw: usize, bh: usize, cfg: SeamConfig) -> Self {
        let n = bw * bh;
        let mut mode = vec![0u8; n];
        let mut best = vec![0.0f32; n];
        for b in 0..n {
            let (bx, by) = (b % bw, b / bw);
            let (mut any, mut any_ok, mut any_pos) = (false, false, false);
            for p in photos {
                if let Some(s) = p.at(bw, bx, by) {
                    any = true;
                    if s.flags & NEGATIVE == 0 {
                        any_ok = true;
                        any_pos |= s.flags & POSITIVE != 0;
                    }
                }
            }
            // the masks are ignored where they would leave no photo at all
            mode[b] = if !any || !any_ok { 0 } else if any_pos { 2 } else { 1 };
            for p in photos {
                if let Some(s) = p.at(bw, bx, by) {
                    if Self::allowed_in(mode[b], s) && s.score > best[b] {
                        best[b] = s.score;
                    }
                }
            }
        }
        Problem { bw, bh, photos, cfg, mode, best }
    }

    #[inline]
    fn allowed_in(mode: u8, s: &Sample) -> bool {
        match mode {
            0 => true,
            1 => s.flags & NEGATIVE == 0,
            _ => s.flags & NEGATIVE == 0 && s.flags & POSITIVE != 0,
        }
    }

    /// The sample of photo `i` in block `b` if the photo may be used there.
    #[inline]
    fn sample(&self, i: usize, b: usize) -> Option<&Sample> {
        let s = self.photos[i].at(self.bw, b % self.bw, b / self.bw)?;
        Self::allowed_in(self.mode[b], s).then_some(s)
    }

    /// Cost of showing photo `i` in block `b` (`INF` if it may not).
    fn data(&self, i: usize, b: usize) -> i64 {
        let Some(s) = self.sample(i, b) else { return INF };
        let r = (s.score / self.best[b].max(1e-30)).clamp(0.0, 1.0);
        let mut c = self.cfg.data_weight * (1.0 - r);
        if r < WORTHLESS_RATIO {
            c += self.cfg.data_weight * WORTHLESS_PENALTY;
        }
        (c * SCALE) as i64
    }

    /// Colour difference (0..1) of photos `a` and `b` in block `x`.
    #[inline]
    fn diff(&self, a: usize, b: usize, x: usize) -> f32 {
        let (bx, by) = (x % self.bw, x / self.bw);
        match (self.photos[a].at(self.bw, bx, by), self.photos[b].at(self.bw, bx, by)) {
            (Some(sa), Some(sb)) => (0..3).map(|k| (sa.rgb[k] as f32 - sb.rgb[k] as f32).abs()).sum::<f32>() / (3.0 * 255.0),
            _ => self.cfg.missing_diff,
        }
    }

    /// Cost of a seam between the neighbouring blocks `p` and `q` where photo `a` shows in `p` and photo `b` in `q`.
    /// (The colour difference of the two photos at both blocks; the same photo on both sides costs nothing.)
    fn seam(&self, a: usize, b: usize, p: usize, q: usize) -> i64 {
        if a == b {
            return 0;
        }
        let c = self.cfg.smooth_weight * 0.5 * (self.diff(a, b, p) + self.diff(a, b, q)) + self.cfg.cut_cost;
        (c * SCALE) as i64
    }
}

/// Total energy of a labelling (used by the tests and for checking).
fn energy(pr: &Problem, labels: &[u16]) -> i64 {
    let mut e = 0i64;
    for b in 0..pr.bw * pr.bh {
        if labels[b] == NONE {
            continue;
        }
        e += pr.data(labels[b] as usize, b);
        let (bx, by) = (b % pr.bw, b / pr.bw);
        for q in [(bx + 1 < pr.bw).then(|| b + 1), (by + 1 < pr.bh).then(|| b + pr.bw)].into_iter().flatten() {
            if labels[q] != NONE {
                e += pr.seam(labels[b] as usize, labels[q] as usize, b, q);
            }
        }
    }
    e
}

// ---------------------------------------------------------------------------------------------
// max flow (Dinic, integer capacities, iterative)

struct Graph {
    n: usize,
    /// edges as (from, to, capacity forward, capacity backward)
    pending: Vec<(u32, u32, i64, i64)>,
    start: Vec<u32>,
    to: Vec<u32>,
    cap: Vec<i64>,
    rev: Vec<u32>,
}

impl Graph {
    fn new(n: usize) -> Self {
        Graph { n, pending: Vec::new(), start: Vec::new(), to: Vec::new(), cap: Vec::new(), rev: Vec::new() }
    }
    fn add(&mut self, u: usize, v: usize, cap_uv: i64, cap_vu: i64) {
        if cap_uv > 0 || cap_vu > 0 {
            self.pending.push((u as u32, v as u32, cap_uv, cap_vu));
        }
    }
    fn finish(&mut self) {
        let mut deg = vec![0u32; self.n + 1];
        for &(u, v, _, _) in &self.pending {
            deg[u as usize + 1] += 1;
            deg[v as usize + 1] += 1;
        }
        for i in 0..self.n {
            deg[i + 1] += deg[i];
        }
        self.start = deg.clone();
        let m = self.pending.len() * 2;
        self.to = vec![0; m];
        self.cap = vec![0; m];
        self.rev = vec![0; m];
        let mut fill = deg;
        for &(u, v, cuv, cvu) in &self.pending {
            let (iu, iv) = (fill[u as usize], fill[v as usize]);
            fill[u as usize] += 1;
            fill[v as usize] += 1;
            self.to[iu as usize] = v;
            self.cap[iu as usize] = cuv;
            self.rev[iu as usize] = iv;
            self.to[iv as usize] = u;
            self.cap[iv as usize] = cvu;
            self.rev[iv as usize] = iu;
        }
        self.pending = Vec::new();
    }

    /// Maximum flow from `s` to `t`; afterwards `reachable` tells which nodes are on the source side of a minimum cut.
    fn max_flow(&mut self, s: usize, t: usize) -> i64 {
        let n = self.n;
        let mut flow = 0i64;
        let mut level = vec![-1i32; n];
        let mut it = vec![0u32; n];
        let mut queue = Vec::with_capacity(n);
        loop {
            level.iter_mut().for_each(|l| *l = -1);
            level[s] = 0;
            queue.clear();
            queue.push(s as u32);
            let mut head = 0;
            while head < queue.len() {
                let u = queue[head] as usize;
                head += 1;
                for e in self.start[u] as usize..self.start[u + 1] as usize {
                    let v = self.to[e] as usize;
                    if self.cap[e] > 0 && level[v] < 0 {
                        level[v] = level[u] + 1;
                        queue.push(v as u32);
                    }
                }
            }
            if level[t] < 0 {
                return flow;
            }
            it.copy_from_slice(&self.start[..n]);
            let mut path: Vec<u32> = Vec::new();
            let mut u = s;
            loop {
                if u == t {
                    let f = path.iter().map(|&e| self.cap[e as usize]).min().unwrap_or(0);
                    for &e in &path {
                        self.cap[e as usize] -= f;
                        let r = self.rev[e as usize] as usize;
                        self.cap[r] += f;
                    }
                    flow += f;
                    path.clear();
                    u = s;
                    continue;
                }
                let end = self.start[u + 1];
                let mut found = false;
                while it[u] < end {
                    let e = it[u] as usize;
                    let v = self.to[e] as usize;
                    if self.cap[e] > 0 && level[v] == level[u] + 1 {
                        path.push(e as u32);
                        u = v;
                        found = true;
                        break;
                    }
                    it[u] += 1;
                }
                if found {
                    continue;
                }
                // dead end: step back
                if u == s {
                    break;
                }
                level[u] = -1;
                let e = path.pop().expect("path to a dead end is not empty") as usize;
                u = self.to[self.rev[e] as usize] as usize;
                it[u] += 1;
            }
        }
    }

    /// Nodes reachable from `s` in the residual graph.
    fn reachable(&self, s: usize) -> Vec<bool> {
        let mut seen = vec![false; self.n];
        let mut stack = vec![s];
        seen[s] = true;
        while let Some(u) = stack.pop() {
            for e in self.start[u] as usize..self.start[u + 1] as usize {
                let v = self.to[e] as usize;
                if self.cap[e] > 0 && !seen[v] {
                    seen[v] = true;
                    stack.push(v);
                }
            }
        }
        seen
    }
}

// ---------------------------------------------------------------------------------------------
// alpha expansion

/// One expansion move: every block photo `alpha` may use decides between keeping its label and switching to `alpha`.
/// Returns whether any label changed.
fn expand(pr: &Problem, labels: &mut [u16], alpha: usize, node_of: &mut [u32]) -> bool {
    let (bw, bh) = (pr.bw, pr.bh);
    // the blocks that can switch (or already show alpha)
    let ph = &pr.photos[alpha];
    let mut vs: Vec<usize> = Vec::new();
    for by in ph.by0..(ph.by0 + ph.rows).min(bh) {
        for bx in 0..bw {
            let b = by * bw + bx;
            if labels[b] != NONE && pr.sample(alpha, b).is_some() {
                node_of[b] = vs.len() as u32;
                vs.push(b);
            }
        }
    }
    if vs.iter().all(|&b| labels[b] as usize == alpha) {
        for &b in &vs {
            node_of[b] = u32::MAX;
        }
        return false;
    }
    let n = vs.len();
    let (s, t) = (n, n + 1);
    // unary terms: cost of keeping (to the sink side) and of switching (source side)
    let mut keep = vec![0i64; n];
    let mut sw = vec![0i64; n];
    let mut g = Graph::new(n + 2);
    for (v, &b) in vs.iter().enumerate() {
        if labels[b] as usize != alpha {
            keep[v] = pr.data(labels[b] as usize, b);
            sw[v] = pr.data(alpha, b);
        }
    }
    let mut next_aux = n + 2;
    // neighbour terms (every pair of neighbouring blocks once)
    for b in 0..bw * bh {
        let (bx, by) = (b % bw, b / bw);
        for q in [(bx + 1 < bw).then(|| b + 1), (by + 1 < bh).then(|| b + bw)].into_iter().flatten() {
            let (cp, cq) = (labels[b], labels[q]);
            if cp == NONE || cq == NONE {
                continue;
            }
            let (np, nq) = (node_of[b], node_of[q]);
            let (ip, iq) = (np != u32::MAX, nq != u32::MAX);
            if !ip && !iq {
                continue;
            }
            let (cp, cq) = (cp as usize, cq as usize);
            match (ip, iq) {
                (true, true) => {
                    let (vp, vq) = (np as usize, nq as usize);
                    let (fp, fq) = (cp == alpha, cq == alpha);
                    if fp && fq {
                        continue;
                    }
                    if fp {
                        // p shows alpha for sure: q keeps (pays to meet alpha) or switches (free)
                        keep[vq] += pr.seam(alpha, cq, b, q);
                    } else if fq {
                        keep[vp] += pr.seam(cp, alpha, b, q);
                    } else if cp == cq {
                        let w = pr.seam(alpha, cp, b, q);
                        g.add(vp, vq, w, w);
                    } else {
                        // different labels: an auxiliary node (Boykov, Veksler, Zabih)
                        let m = next_aux;
                        next_aux += 1;
                        g.n = next_aux;
                        let a = pr.seam(cp, alpha, b, q);
                        let bb = pr.seam(alpha, cq, b, q);
                        let c = pr.seam(cp, cq, b, q);
                        g.add(s, m, c, 0);
                        g.add(vp, m, a, a);
                        g.add(m, vq, bb, bb);
                    }
                }
                (true, false) => {
                    let v = np as usize;
                    if cp != alpha {
                        sw[v] += pr.seam(alpha, cq, b, q);
                        keep[v] += pr.seam(cp, cq, b, q);
                    }
                }
                (false, true) => {
                    let v = nq as usize;
                    if cq != alpha {
                        sw[v] += pr.seam(cp, alpha, b, q);
                        keep[v] += pr.seam(cp, cq, b, q);
                    }
                }
                (false, false) => {}
            }
        }
    }
    g.n = next_aux;
    for (v, &b) in vs.iter().enumerate() {
        if labels[b] as usize == alpha {
            g.add(s, v, INF, 0);
        } else {
            g.add(s, v, keep[v], 0);
            g.add(v, t, sw[v], 0);
        }
    }
    g.finish();
    g.max_flow(s, t);
    let on_source = g.reachable(s);
    let mut changed = false;
    for (v, &b) in vs.iter().enumerate() {
        if on_source[v] && labels[b] as usize != alpha {
            labels[b] = alpha as u16;
            changed = true;
        }
        node_of[b] = u32::MAX;
    }
    changed
}

/// Finds the seams. `photos[i]` are the blocks of photo `i`, the grid is `bw` x `bh` blocks (see [`grid`]).
/// `cancelled` is checked between the expansion moves. Returns one label per block.
pub fn find(photos: &[PhotoBlocks], bw: usize, bh: usize, cfg: &SeamConfig, cancelled: &dyn Fn() -> bool) -> Result<Vec<u16>, String> {
    if photos.len() >= NONE as usize {
        return Err("too many photos for the seam finder".into());
    }
    let pr = Problem::new(photos, bw, bh, *cfg);
    // start: the photo with the best score in every block (the way seams were found before)
    let mut labels = vec![NONE; bw * bh];
    for b in 0..bw * bh {
        let mut best = (NONE, f32::MIN);
        for i in 0..photos.len() {
            if let Some(s) = pr.sample(i, b) {
                if s.score > best.1 {
                    best = (i as u16, s.score);
                }
            }
        }
        labels[b] = best.0;
    }
    let mut node_of = vec![u32::MAX; bw * bh];
    for _ in 0..cfg.cycles {
        let mut changed = false;
        for alpha in 0..photos.len() {
            if cancelled() {
                return Err(crate::stitch::CANCELLED.into());
            }
            changed |= expand(&pr, &mut labels, alpha, &mut node_of);
        }
        if !changed {
            break;
        }
    }
    Ok(labels)
}

/// Total energy of `labels` for the given blocks (for tests and tuning).
#[allow(dead_code)]
pub fn labelling_energy(photos: &[PhotoBlocks], bw: usize, bh: usize, cfg: &SeamConfig, labels: &[u16]) -> i64 {
    energy(&Problem::new(photos, bw, bh, *cfg), labels)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BW: usize = 40;
    const BH: usize = 20;

    /// A photo covering block columns `x0..x1` of the whole height; the score falls towards its left and
    /// right edge like the border weight of a real photo. `colour(bx, by)` is what it shows.
    fn photo(x0: usize, x1: usize, colour: impl Fn(usize, usize) -> [u8; 3]) -> PhotoBlocks {
        let mut p = PhotoBlocks::new(0, BH, BW);
        for by in 0..BH {
            for bx in x0..x1 {
                let t = (bx - x0) as f32 + 0.5;
                let edge = t.min((x1 - x0) as f32 - t) / ((x1 - x0) as f32 * 0.5);
                p.samples[by * BW + bx] = Sample { rgb: colour(bx, by), flags: VALID, score: edge.max(1e-6) };
            }
        }
        p
    }

    fn cfg() -> SeamConfig {
        SeamConfig { enabled: true, ..SeamConfig::default() }
    }

    fn never() -> bool {
        false
    }

    #[test]
    fn two_equal_photos_keep_the_seam_in_the_middle_of_the_overlap() {
        let grey = |_: usize, _: usize| [100, 100, 100];
        let photos = [photo(0, 26, grey), photo(14, 40, grey)];
        let l = find(&photos, BW, BH, &cfg(), &never).unwrap();
        for by in 0..BH {
            assert_eq!(l[by * BW + 2], 0);
            assert_eq!(l[by * BW + 38], 1);
            let first_b = (0..BW).find(|&bx| l[by * BW + bx] == 1).unwrap();
            assert!((19..=21).contains(&first_b), "row {by}: seam at {first_b}");
        }
    }

    #[test]
    fn the_seam_runs_around_an_object_that_only_one_photo_shows() {
        // photo 1 shows a bright object (a pole) in the middle of the overlap, photo 0 does not
        let object = |bx: usize, by: usize| if (19..=21).contains(&bx) && (6..=13).contains(&by) { [250, 250, 250] } else { [100, 100, 100] };
        let grey = |_: usize, _: usize| [100, 100, 100];
        let photos = [photo(0, 26, grey), photo(14, 40, object)];
        let l = find(&photos, BW, BH, &cfg(), &never).unwrap();
        // the seam must not cut through the object: at every row of the object, all object blocks belong to one photo
        for by in 6..=13 {
            let labels: Vec<u16> = (19..=21).map(|bx| l[by * BW + bx]).collect();
            assert!(labels.iter().all(|&x| x == labels[0]), "row {by}: the seam cuts the object: {labels:?}");
        }
        // and with the seam finder off the plain argmax would cut straight through the middle
        let off = find(&photos, BW, BH, &SeamConfig { cycles: 0, ..cfg() }, &never).unwrap();
        assert!((6..=13).any(|by| (19..=21).map(|bx| off[by * BW + bx]).any(|x| x != off[by * BW + 19])), "the start labelling cuts the object");
        // the energy went down
        assert!(labelling_energy(&photos, BW, BH, &cfg(), &l) < labelling_energy(&photos, BW, BH, &cfg(), &off));
    }

    #[test]
    fn the_result_is_valid_and_always_the_same() {
        let wavy = |bx: usize, by: usize| [(bx * 13 % 90 + 80) as u8, (by * 7 % 60 + 60) as u8, 120];
        let photos = [photo(0, 26, wavy), photo(14, 40, |bx, by| wavy(bx + 1, by))];
        let a = find(&photos, BW, BH, &cfg(), &never).unwrap();
        let b = find(&photos, BW, BH, &cfg(), &never).unwrap();
        assert_eq!(a, b);
        for by in 0..BH {
            for bx in 0..BW {
                let l = a[by * BW + bx];
                assert!(l != NONE && photos[l as usize].at(BW, bx, by).is_some(), "block ({bx},{by}) shows a photo that does not cover it");
            }
        }
    }

    #[test]
    fn blocks_nobody_covers_stay_empty_and_a_worthless_photo_only_fills_gaps() {
        let grey = |_: usize, _: usize| [100, 100, 100];
        let mut worthless = photo(10, 40, grey);
        worthless.samples.iter_mut().for_each(|s| s.score *= 1e-9);
        let photos = [photo(0, 20, grey), worthless];
        let l = find(&photos, BW, BH, &cfg(), &never).unwrap();
        for by in 0..BH {
            for bx in 0..20 {
                assert_eq!(l[by * BW + bx], 0, "({bx},{by}) must show the good photo");
            }
            for bx in 20..40 {
                assert_eq!(l[by * BW + bx], 1, "({bx},{by}) is only covered by the worthless photo");
            }
        }
        let empty = [photo(0, 10, grey)];
        let l = find(&empty, BW, BH, &cfg(), &never).unwrap();
        assert_eq!(l[20], NONE);
    }

    #[test]
    fn masks_decide_and_are_ignored_where_they_would_leave_nothing() {
        let grey = |_: usize, _: usize| [100, 100, 100];
        // photo 1 is painted "do not use" in the left half of the overlap, photo 0 "use" in the right half
        let mut p0 = photo(0, 26, grey);
        let mut p1 = photo(14, 40, grey);
        for by in 0..BH {
            for bx in 14..20 {
                p1.samples[by * BW + bx].flags |= NEGATIVE;
            }
            for bx in 20..26 {
                p0.samples[by * BW + bx].flags |= POSITIVE;
            }
        }
        let l = find(&[p0.clone(), p1.clone()], BW, BH, &cfg(), &never).unwrap();
        for by in 0..BH {
            for bx in 14..20 {
                assert_eq!(l[by * BW + bx], 0, "({bx},{by}): photo 1 is masked out");
            }
            for bx in 20..26 {
                assert_eq!(l[by * BW + bx], 0, "({bx},{by}): photo 0 is painted positive");
            }
            assert_eq!(l[by * BW + 30], 1);
        }
        // photo 1 is the only one covering columns 26.. and is masked out there: the mask is ignored
        for by in 0..BH {
            for bx in 26..30 {
                p1.samples[by * BW + bx].flags |= NEGATIVE;
            }
        }
        let l = find(&[p0, p1], BW, BH, &cfg(), &never).unwrap();
        assert_eq!(l[5 * BW + 28], 1);
    }

    #[test]
    fn the_preferred_photo_of_a_pixel_follows_the_labels_smoothly() {
        let mut labels = vec![0u16; 8 * 4];
        for by in 0..4 {
            for bx in 4..8 {
                labels[by * 8 + bx] = 1;
            }
        }
        let m = LabelMap { bw: 8, bh: 4, bsz: 10, labels };
        assert_eq!(m.preferred(5, 15), 0);
        assert_eq!(m.preferred(75, 15), 1);
        // the boundary lies between the centres of blocks 3 and 4, that is at x = 40 (pixel 39.5 is still 0)
        assert_eq!(m.preferred(39, 20), 0);
        assert_eq!(m.preferred(40, 20), 1);
        // a pixel next to an empty block still gets the label of the covered ones
        let mut e = m.clone();
        e.labels[0] = NONE;
        assert_eq!(e.preferred(9, 9), 0);
        assert_eq!(e.preferred(0, 0), NONE, "the corner pixel only sees the empty block");
        let all_empty = LabelMap { bw: 2, bh: 2, bsz: 4, labels: vec![NONE; 4] };
        assert_eq!(all_empty.preferred(3, 3), NONE);
        // rows come out the same as single pixels
        let rows = m.preferred_rows(80, (10, 13));
        assert_eq!(rows[2 * 80 + 41], m.preferred(41, 12));
    }

    #[test]
    fn the_grid_has_at_most_max_side_blocks_and_covers_the_panorama() {
        for (w, h, side) in [(12000usize, 3000usize, 1536usize), (500, 200, 1536), (60000, 20000, 1000), (7, 3, 8)] {
            let (bsz, bw, bh) = grid(w, h, side);
            assert!(bw.max(bh) <= side.max(8));
            assert!(bw * bsz >= w && bh * bsz >= h);
        }
    }

    #[test]
    fn cancelling_stops_the_search() {
        let grey = |_: usize, _: usize| [100, 100, 100];
        let photos = [photo(0, 26, grey), photo(14, 40, grey)];
        assert_eq!(find(&photos, BW, BH, &cfg(), &|| true).unwrap_err(), crate::stitch::CANCELLED);
    }

    #[test]
    fn the_max_flow_finds_the_minimum_cut() {
        // s -> a (3), s -> b (2), a -> b (1), a -> t (2), b -> t (3): flow 5
        let mut g = Graph::new(4);
        g.add(0, 1, 3, 0);
        g.add(0, 2, 2, 0);
        g.add(1, 2, 1, 0);
        g.add(1, 3, 2, 0);
        g.add(2, 3, 3, 0);
        g.finish();
        assert_eq!(g.max_flow(0, 3), 5);
        let r = g.reachable(0);
        assert!(r[0] && !r[3]);
    }
}
