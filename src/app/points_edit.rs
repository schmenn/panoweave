use super::*;

impl PanoApp {
    pub(super) fn push_undo(&mut self, what: &str) {
        self.undo.push(UndoEntry { what: what.to_string(), cps: self.cps.clone(), result: self.click.result.clone() });
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
    }

    pub(super) fn undo_last(&mut self) {
        if let Some(u) = self.undo.pop() {
            self.cps = u.cps;
            self.click.result = u.result;
            self.pe.selected = None;
            self.needs_align = true;
            self.set_status(format!("Undone: {}.", u.what), MUTED);
        }
    }

    pub(super) fn delete_selected(&mut self) {
        if let Some(k) = self.pe.selected.take() {
            if k < self.cps.len() {
                self.cps.remove(k);
                self.needs_align = true;
            }
        }
    }

    pub(super) fn add_point_click(&mut self, pane: Pane, p: P2) {
        let (a, b) = (self.pe.a, self.pe.b);
        let this = if pane == Pane::A { a } else { b };
        let other = if pane == Pane::A { b } else { a };
        match self.pe.pending {
            Some((q, qn)) if q != pane => {
                // second click: keep the first spot, snap this one onto it
                let fixed = other;
                let pf = self.to_px(fixed, qn);
                let j = match self.predict(fixed, this, pf) {
                    Some(x) => x.1,
                    None => self.rotation_guess(fixed, pf, this, p),
                };
                let r = self.placing_radius(this);
                let snapped = self.finetune(fixed, pf, this, p, j, r).filter(|r| r.1 >= guided::FINETUNE_MIN_NCC);
                let final_p = snapped.map(|r| r.0).unwrap_or(p);
                self.pe.pending = None;
                let n_this = self.to_norm(this, final_p);
                if snapped.is_some() {
                    self.set_status("Point added and snapped to the exact spot.", GOOD);
                } else {
                    self.set_status("Point added where you clicked (no clear detail to snap to).", WARN);
                }
                self.push_manual(q, qn, n_this);
            }
            _ => {
                let pn = self.to_norm(this, p);
                if let Some((guess, j)) = self.predict(this, other, p) {
                    let r = self.placing_radius(other);
                    if let Some((pos, v)) = self.finetune(this, p, other, guess, j, r) {
                        if v >= guided::FINETUNE_MIN_NCC {
                            let on = self.to_norm(other, pos);
                            let o = pane.other();
                            if self.pe.views[o.i()].zoom > 1.2 {
                                self.pe.views[o.i()].center = Vec2::new(pos.0 as f32 + 0.5, pos.1 as f32 + 0.5);
                            }
                            self.set_status("Point added – the matching spot was found and snapped automatically. Drag it if it is off.", GOOD);
                            self.push_manual(pane, pn, on);
                            return;
                        }
                    }
                }
                self.pe.pending = Some((pane, pn));
            }
        }
    }

    /// Store a hand-placed pair; `n1` belongs to pane `p1`, `n2` to the other pane.
    pub(super) fn push_manual(&mut self, p1: Pane, n1: [f64; 2], n2: [f64; 2]) {
        let (a, b) = (self.pe.a, self.pe.b);
        let (pa, pb) = if p1 == Pane::A { (n1, n2) } else { (n2, n1) };
        self.cps.push(ControlPoint::new(a, b, pa, pb, true));
        self.pe.selected = Some(self.cps.len() - 1);
        self.needs_align = true;
        if self.auto_grow {
            let auto = self.cps.iter().filter(|c| !c.manual && ((c.a == a && c.b == b) || (c.a == b && c.b == a))).count();
            if auto < 10 {
                self.run_pair_points();
            }
        }
    }

    /// After dragging a point: snap it onto its partner with sub-pixel accuracy.
    pub(super) fn snap_point(&mut self, k: usize, pane: Pane) {
        if k >= self.cps.len() {
            return;
        }
        let (a, b) = (self.pe.a, self.pe.b);
        let c = self.cps[k];
        let (pa, pb) = if c.a == a { (c.pa, c.pb) } else { (c.pb, c.pa) };
        let (fixed, moving, fixed_p, moving_p) = if pane == Pane::A { (b, a, pb, pa) } else { (a, b, pa, pb) };
        let pf = self.to_px(fixed, fixed_p);
        let pm = self.to_px(moving, moving_p);
        // local zoom/tilt from the other points of the pair (without this one)
        let seeds: Vec<(P2, P2)> = self.pair_seeds(fixed, moving).into_iter().filter(|s| (s.0 .0 - pf.0).hypot(s.0 .1 - pf.1) > 0.01).collect();
        let (w, h) = self.work_dims(fixed);
        let j = if seeds.len() >= 2 {
            guided::estimate(&seeds, self.scale_prior(fixed, moving), w.max(h) as f64).map(|t| guided::jacobian(&t, pf))
        } else {
            None
        }
        .unwrap_or_else(|| self.rotation_guess(fixed, pf, moving, pm));
        let r = self.local_radius(moving);
        if let Some((q, v)) = self.finetune(fixed, pf, moving, pm, j, r) {
            if v >= guided::FINETUNE_MIN_NCC {
                let n = self.to_norm(moving, q);
                let cp = &mut self.cps[k];
                if cp.a == moving {
                    cp.pa = n;
                } else {
                    cp.pb = n;
                }
                self.set_status(format!("Point snapped to the exact spot (match {:.0} %).", v * 100.0), GOOD);
            }
        }
    }

    pub(super) fn handle_pane_events(&mut self, events: Vec<PaneEvent>, geo: &[PaneGeo; 2], pts: &[(usize, P2, P2)]) {
        let (a, b) = (self.pe.a, self.pe.b);
        let px_of = |pane: Pane, pos: Pos2| -> P2 {
            let g = &geo[pane.i()];
            (((pos.x - g.origin.x) / g.scale - 0.5) as f64, ((pos.y - g.origin.y) / g.scale - 0.5) as f64)
        };
        let screen_of = |pane: Pane, p: P2| -> Pos2 {
            let g = &geo[pane.i()];
            g.origin + Vec2::new((p.0 as f32 + 0.5) * g.scale, (p.1 as f32 + 0.5) * g.scale)
        };
        let nearest = |pane: Pane, pos: Pos2| -> Option<usize> {
            pts.iter()
                .map(|(k, pa, pb)| (*k, screen_of(pane, if pane == Pane::A { *pa } else { *pb }).distance(pos)))
                .filter(|(_, d)| *d < 11.0)
                .min_by(|x, y| x.1.partial_cmp(&y.1).unwrap())
                .map(|(k, _)| k)
        };
        for ev in events {
            match ev {
                PaneEvent::Fit(pane) => {
                    let g = &geo[pane.i()];
                    self.pe.views[pane.i()].zoom = 1.0;
                    self.pe.views[pane.i()].center = Vec2::new(g.w / 2.0, g.h / 2.0);
                }
                PaneEvent::Zoom(pane, pos, f) => {
                    let g = &geo[pane.i()];
                    let v = &mut self.pe.views[pane.i()];
                    let p_img = (pos - g.origin) / g.scale;
                    v.zoom = (v.zoom * f).clamp(0.5, 80.0);
                    let ns = g.fit * v.zoom;
                    v.center = p_img - (pos - g.rect.center()) / ns;
                    v.center.x = v.center.x.clamp(0.0, g.w);
                    v.center.y = v.center.y.clamp(0.0, g.h);
                }
                PaneEvent::Click(pane, pos) => {
                    if self.click.on {
                        let p = px_of(pane, pos);
                        let (w, h) = (geo[pane.i()].w as f64, geo[pane.i()].h as f64);
                        if p.0 >= 0.0 && p.1 >= 0.0 && p.0 <= w - 1.0 && p.1 <= h - 1.0 {
                            if pane == Pane::A {
                                self.click.a = Some(p);
                            } else {
                                self.click.b = Some(p);
                            }
                            // new clicks replace open suggestions (undo keeps them)
                            if self.click.result.is_some() {
                                self.push_undo("replacing the patch click suggestions");
                                self.click.result = None;
                            }
                            self.run_click();
                        }
                    } else if self.pe.adding {
                        let p = px_of(pane, pos);
                        let (w, h) = (geo[pane.i()].w as f64, geo[pane.i()].h as f64);
                        if p.0 >= -0.5 && p.1 >= -0.5 && p.0 <= w - 0.5 && p.1 <= h - 0.5 {
                            self.add_point_click(pane, p);
                        }
                    } else {
                        self.pe.selected = nearest(pane, pos);
                        if self.pe.selected.is_some() {
                            self.pe.moving = Some(pane);
                        }
                        // show the partner in the other photo
                        if let Some(k) = self.pe.selected {
                            if let Some((_, pa, pb)) = pts.iter().find(|t| t.0 == k) {
                                let other = pane.other();
                                if self.pe.views[other.i()].zoom > 1.2 {
                                    let q = if other == Pane::A { *pa } else { *pb };
                                    self.pe.views[other.i()].center = Vec2::new(q.0 as f32 + 0.5, q.1 as f32 + 0.5);
                                }
                            }
                        }
                    }
                }
                PaneEvent::DragStart(pane, pos, primary) => {
                    self.pe.dragging = None;
                    self.pe.panning = None;
                    if primary {
                        if let Some(k) = nearest(pane, pos) {
                            self.pe.selected = Some(k);
                            self.pe.dragging = Some((k, pane));
                            self.pe.moving = Some(pane);
                            continue;
                        }
                    }
                    self.pe.panning = Some(pane);
                }
                PaneEvent::Drag(pane, pos, delta) => {
                    if let Some((k, dp)) = self.pe.dragging {
                        if dp == pane && k < self.cps.len() {
                            let idx = if pane == Pane::A { a } else { b };
                            let n = self.to_norm(idx, px_of(pane, pos));
                            let c = &mut self.cps[k];
                            if c.a == idx {
                                c.pa = n;
                            } else {
                                c.pb = n;
                            }
                            c.manual = true;
                            self.needs_align = true;
                        }
                    } else if self.pe.panning == Some(pane) {
                        let g = &geo[pane.i()];
                        let v = &mut self.pe.views[pane.i()];
                        v.center -= delta / g.scale;
                        v.center.x = v.center.x.clamp(0.0, g.w);
                        v.center.y = v.center.y.clamp(0.0, g.h);
                    }
                }
                PaneEvent::DragEnd => {
                    if let Some((k, pane)) = self.pe.dragging.take() {
                        self.snap_point(k, pane);
                    }
                    self.pe.panning = None;
                }
            }
        }
    }
}
