use super::*;

impl PanoApp {
    pub(super) fn work_dims(&self, i: usize) -> (usize, usize) {
        let (w, h) = self.photos[i].work.dimensions();
        (w as usize, h as usize)
    }

    pub(super) fn to_px(&self, i: usize, p: [f64; 2]) -> P2 {
        let (w, h) = self.work_dims(i);
        let (x, y) = engine::denorm_coord(p, w, h);
        (x as f64, y as f64)
    }

    pub(super) fn to_norm(&self, i: usize, p: P2) -> [f64; 2] {
        let (w, h) = self.work_dims(i);
        engine::norm_coord(p.0 as f32, p.1 as f32, w, h)
    }

    /// (number of points, mean error, max error) between two photos.
    pub(super) fn pair_stats(&self, a: usize, b: usize) -> (usize, f64, f64) {
        let mut n = 0;
        let mut s = 0.0;
        let mut m = 0.0f64;
        for c in &self.cps {
            if (c.a == a && c.b == b) || (c.a == b && c.b == a) {
                n += 1;
                s += c.err;
                m = m.max(c.err);
            }
        }
        (n, if n > 0 { s / n as f64 } else { 0.0 }, m)
    }

    /// How much larger things appear in photo `to` than in photo `from` (work pixels), from the lenses.
    pub(super) fn scale_prior(&self, from: usize, to: usize) -> f64 {
        let lf = self.work_dims(from);
        let lt = self.work_dims(to);
        let pf = self.lenses[self.photos[from].lens].f * lf.0.max(lf.1) as f64;
        let pt = self.lenses[self.photos[to].lens].f * lt.0.max(lt.1) as f64;
        (pt / pf).clamp(0.05, 20.0)
    }

    /// Point pairs between two photos, in work pixels, oriented from -> to.
    pub(super) fn pair_seeds(&self, from: usize, to: usize) -> Vec<(P2, P2)> {
        self.cps
            .iter()
            .filter_map(|c| {
                if c.a == from && c.b == to {
                    Some((self.to_px(from, c.pa), self.to_px(to, c.pb)))
                } else if c.a == to && c.b == from {
                    Some((self.to_px(from, c.pb), self.to_px(to, c.pa)))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Where a spot of photo `from` should be in photo `to`, and the local zoom/tilt between them.
    pub(super) fn predict(&self, from: usize, to: usize, p: P2) -> Option<(P2, [f64; 4])> {
        let seeds = self.pair_seeds(from, to);
        if seeds.len() == 1 {
            // one point: zoom from the lenses, rotation measured at that point
            let ((x, y), (u, v)) = seeds[0];
            let j = self.rotation_guess(from, (x, y), to, (u, v));
            let t = [j[0], j[1], u - j[0] * x - j[1] * y, j[2], j[3], v - j[2] * x - j[3] * y, 0.0, 0.0, 1.0];
            return Some((h_apply(&t, p.0, p.1), j));
        }
        if !seeds.is_empty() {
            let (w, h) = self.work_dims(from);
            let t = guided::estimate(&seeds, self.scale_prior(from, to), w.max(h) as f64)?;
            return Some((h_apply(&t, p.0, p.1), guided::jacobian(&t, p)));
        }
        if self.aligned && !self.needs_align {
            return self.predict_from_alignment(from, to, p);
        }
        None
    }

    /// Where a spot of photo `from` lies in photo `to` and the local zoom/rotation, from the alignment alone.
    pub(super) fn predict_from_alignment(&self, from: usize, to: usize, p: P2) -> Option<(P2, [f64; 4])> {
        let lf = self.lenses[self.photos[from].lens];
        let lt = self.lenses[self.photos[to].lens];
        let rf = self.photos[from].rot;
        let rt = self.photos[to].rot;
        let map = |q: P2| -> Option<P2> {
            let w = mtv(&rf, crate::align::ray(&lf, self.to_norm(from, q)));
            crate::align::project(&lt, mv(&rt, w)).map(|n| self.to_px(to, n))
        };
        let c = map(p)?;
        let x = map((p.0 + 1.0, p.1))?;
        let y = map((p.0, p.1 + 1.0))?;
        let (w, h) = self.work_dims(to);
        if c.0 < 0.0 || c.1 < 0.0 || c.0 >= w as f64 || c.1 >= h as f64 {
            return None;
        }
        Some((c, [x.0 - c.0, y.0 - c.0, x.1 - c.1, y.1 - c.1]))
    }

    /// Work pixels per full-size pixel of a photo.
    pub(super) fn work_per_full(&self, i: usize) -> f64 {
        self.photos[i].work.width() as f64 / self.photos[i].w.max(1) as f64
    }

    /// Search radius (work px) when placing a point: 10 % of the photo width, at most 500 px.
    pub(super) fn placing_radius(&self, i: usize) -> f64 {
        let win_work = guided::placing_window(self.photos[i].w as f64, self.search_pct) * self.work_per_full(i);
        ((win_work - self.tpl_size as f64) / 2.0).max(2.0)
    }

    /// Matching window of the selected point (cached; kept while the point is being dragged).
    pub(super) fn selection_frame(&mut self) -> Option<SelFrame> {
        let k = self.pe.selected?;
        if k >= self.cps.len() {
            return None;
        }
        let (a, b) = (self.pe.a, self.pe.b);
        let c = self.cps[k];
        if !((c.a == a && c.b == b) || (c.a == b && c.b == a)) {
            return None;
        }
        let moving = self.pe.moving.unwrap_or(Pane::B);
        let fixed = moving.other();
        let (pa, pb) = if c.a == a { (c.pa, c.pb) } else { (c.pb, c.pa) };
        let (fi, mi, fp, mp) = if fixed == Pane::A { (a, b, pa, pb) } else { (b, a, pb, pa) };
        let key = (
            k,
            fixed == Pane::A,
            (fp[0] * 1e4) as i64,
            (fp[1] * 1e4) as i64,
            (mp[0] * 1e4) as i64,
            (mp[1] * 1e4) as i64,
            self.cps.len(),
        );
        if let Some(f) = self.sel_frame
            && (f.key == key || (self.pe.dragging.is_some() && f.key.0 == k && f.fixed == fixed)) {
                return Some(f);
            }
        let pf = self.to_px(fi, fp);
        let pm = self.to_px(mi, mp);
        let j = self.predict(fi, mi, pf).map(|x| x.1).unwrap_or_else(|| self.rotation_guess(fi, pf, mi, pm));
        let f = SelFrame { key, fixed, j };
        self.sel_frame = Some(f);
        Some(f)
    }

    /// Search radius (work px) when snapping a moved point (local search).
    pub(super) fn local_radius(&self, i: usize) -> f64 {
        let win = guided::LOCAL_SEARCH_WIDTH + guided::TEMPLATE_SIZE as f64;
        (guided::window_radius(win) * self.work_per_full(i)).max(1.5)
    }

    /// Zoom from the lenses plus a measured rotation, for a point pair without any other help.
    pub(super) fn rotation_guess(&self, fixed: usize, pf: P2, moving: usize, guess: P2) -> [f64; 4] {
        let s = self.scale_prior(fixed, moving);
        let gf = &self.photos[fixed].gray;
        let gm = &self.photos[moving].gray;
        let fs = (s < 1.0 / 1.5).then(|| imgutil::blur(gf, guided::soft_sigma(1.0 / s)));
        let ms = (s > 1.5).then(|| imgutil::blur(gm, guided::soft_sigma(s)));
        let c = guided::Corr { fixed: gf, fixed_soft: fs.as_ref(), moving: gm, moving_soft: ms.as_ref(), tpl_r: self.tpl_size / 2 };
        rotation_guess_with(&c, s, pf, guess)
    }

    /// Correlation search for the spot in `moving` that matches `pf` in `fixed`.
    pub(super) fn finetune(&self, fixed: usize, pf: P2, moving: usize, guess: P2, j: [f64; 4], search: f64) -> Option<(P2, f32)> {
        let s = (j[0] * j[3] - j[1] * j[2]).abs().sqrt();
        let gf = &self.photos[fixed].gray;
        let gm = &self.photos[moving].gray;
        let fs = (s < 1.0 / 1.5).then(|| imgutil::blur(gf, guided::soft_sigma(1.0 / s)));
        let ms = (s > 1.5).then(|| imgutil::blur(gm, guided::soft_sigma(s)));
        let c = guided::Corr { fixed: gf, fixed_soft: fs.as_ref(), moving: gm, moving_soft: ms.as_ref(), tpl_r: self.tpl_size / 2 };
        guided::refine(&c, pf, j, guess, search).map(|(x, y, v)| ((x, y), v))
    }
}

/// The rotation part of `PanoApp::rotation_guess` for ready-made correlation images (also used by
/// the worker thread of "Snap selected"): zoom `s` plus the best rotation found around `guess`.
pub(super) fn rotation_guess_with(c: &guided::Corr, s: f64, pf: P2, guess: P2) -> [f64; 4] {
    // the click can be a few pixels off: look around it
    let mut best = ([s, 0.0, 0.0, s], -2.0f32);
    for (dx, dy) in [(0.0, 0.0), (-4.0, 0.0), (4.0, 0.0), (0.0, -4.0), (0.0, 4.0)] {
        let r = guided::best_rotation(c, pf, s, (guess.0 + dx, guess.1 + dy));
        if r.1 > best.1 {
            best = r;
        }
    }
    best.0
}
