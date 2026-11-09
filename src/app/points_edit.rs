use super::*;

impl PanoApp {
    /// The control points and the open suggestions as they are now.
    pub(super) fn cp_snapshot(&self) -> CpSnapshot {
        CpSnapshot { cps: self.cps.clone(), result: self.click.result.clone() }
    }

    /// Records the state before a change that is about to happen for sure. Prefer `edit_cps`,
    /// which also skips the step when nothing changed.
    pub(super) fn push_undo(&mut self, what: &str) {
        let before = self.cp_snapshot();
        self.undo.push(what, before);
    }

    /// Records `before` as an undo step if the points (or the open suggestions) differ from now.
    /// Returns whether a step was recorded; an unchanged list never makes an empty step.
    pub(super) fn commit_undo(&mut self, what: &str, before: CpSnapshot) -> bool {
        if same_points(&before.cps, &self.cps) && before.result.is_some() == self.click.result.is_some() {
            return false;
        }
        self.undo.push(what, before);
        true
    }

    /// Runs a change of the control points as exactly one undo step (none if it changed nothing).
    pub(super) fn edit_cps(&mut self, what: &str, change: impl FnOnce(&mut Self)) -> bool {
        let before = self.cp_snapshot();
        change(self);
        let done = self.commit_undo(what, before);
        if done {
            self.needs_align = true;
        }
        done
    }

    fn restore_snapshot(&mut self, s: CpSnapshot) {
        self.cps = s.cps;
        self.click.result = s.result;
        self.pe.selected = None;
        self.pe.sel.clear();
        self.pe.pending = None;
        self.pe.fresh = None;
        self.pe.dragging = None;
        self.pe.drag_before = None;
        // the lists in the point window are cached by a fingerprint: build them again
        self.cp_rows_cache = None;
        self.dup_cache = None;
        self.needs_align = true;
    }

    pub(super) fn undo_last(&mut self) {
        if self.busy() {
            self.set_status("Please wait until the current task has finished, then undo.", WARN);
            return;
        }
        let now = self.cp_snapshot();
        match self.undo.undo(now) {
            Some((what, prev)) => {
                self.restore_snapshot(prev);
                self.set_status(format!("Undone: {what}. Ctrl+Y brings it back."), MUTED);
            }
            None => self.set_status("Nothing to undo.", MUTED),
        }
    }

    pub(super) fn redo_last(&mut self) {
        if self.busy() {
            self.set_status("Please wait until the current task has finished, then redo.", WARN);
            return;
        }
        let now = self.cp_snapshot();
        match self.undo.redo(now) {
            Some((what, next)) => {
                self.restore_snapshot(next);
                self.set_status(format!("Redone: {what}."), MUTED);
            }
            None => self.set_status("Nothing to redo.", MUTED),
        }
    }

    /// The one place where Ctrl+Z / Ctrl+Y act. Called once per frame at the end of the main
    /// window; the secondary windows leave their key in `vp.undo_request` / `patch_win.undo_request`.
    /// One key press makes one step, even if several windows saw it.
    pub(super) fn handle_undo_keys(&mut self, ctx: &egui::Context) {
        let mut key = std::mem::take(&mut self.vp.undo_request);
        if let Some(k) = self.patch_win.undo_request.take() {
            key = key.or(Some(k));
        }
        if self.mode == Mode::Panorama {
            // the masks window keeps its own history and consumes its keys before this runs
            key = key.or(super::history::read_undo_key(ctx));
        }
        match key {
            // while a lasso is being clicked, Ctrl+Z takes its last corner back (as in the mask editor)
            Some(super::history::UndoKey::Undo) if self.pe.lasso.pop_corner() => {}
            Some(super::history::UndoKey::Undo) => self.undo_last(),
            Some(super::history::UndoKey::Redo) => self.redo_last(),
            None => {}
        }
    }

    pub(super) fn delete_selected(&mut self) {
        if self.selection_indices().len() > 1 {
            self.delete_selection();
            return;
        }
        if let Some(k) = self.pe.selected
            && self.cps.get(k).is_some_and(|c| c.locked) {
                self.set_status("This point is protected: press L (or the lock button) to unlock it first.", WARN);
                return;
            }
        if let Some(k) = self.pe.selected.take()
            && k < self.cps.len() {
                self.edit_cps("deleting a point", |s| {
                    s.cps.remove(k);
                });
            }
    }

    /// Removes the points at `idx` (any order, duplicates allowed) as one undo step. Protected
    /// points are never removed; returns how many of them were kept.
    pub(super) fn delete_points(&mut self, mut idx: Vec<usize>, what: &str) -> usize {
        idx.sort_unstable();
        idx.dedup();
        idx.retain(|&k| k < self.cps.len());
        let (idx, kept) = crate::align::split_protected(&self.cps, idx);
        if idx.is_empty() {
            return kept;
        }
        self.edit_cps(what, |s| {
            for k in idx.into_iter().rev() {
                s.cps.remove(k);
            }
        });
        self.pe.selected = None;
        self.pe.sel.clear();
        kept
    }

    /// Locks or unlocks the points at `idx` as one undo step; points that already are in that
    /// state are left out. Returns how many were changed. Used by the editor key, the lock
    /// buttons and the list of all points.
    pub(super) fn lock_points(&mut self, mut idx: Vec<usize>, lock: bool) -> usize {
        idx.sort_unstable();
        idx.dedup();
        idx.retain(|&k| self.cps.get(k).is_some_and(|c| c.locked != lock));
        let n = idx.len();
        if n == 0 {
            return 0;
        }
        let what = match (n, lock) {
            (1, true) => "protecting a point".to_string(),
            (1, false) => "unlocking a point".to_string(),
            (_, true) => format!("protecting {n} points"),
            (_, false) => format!("unlocking {n} points"),
        };
        self.edit_cps(&what, |s| {
            for k in idx {
                s.cps[k].locked = lock;
            }
        });
        n
    }

    /// Status text after `lock_points` (`n` = points changed).
    pub(super) fn lock_status(&mut self, n: usize, lock: bool) {
        let msg = match (n, lock) {
            (0, true) => "Nothing to protect: the points are protected already.".to_string(),
            (0, false) => "Nothing to unlock: none of the points is protected.".to_string(),
            (1, true) => "Point protected: no bulk action touches it, and it can't be deleted or moved until unlocked.".to_string(),
            (1, false) => "Point unlocked.".to_string(),
            (n, true) => format!("{n} points protected. Ctrl+Z undoes it."),
            (n, false) => format!("{n} points unlocked, they can be deleted or moved now. Ctrl+Z undoes it."),
        };
        self.set_status(msg, MUTED);
    }

    /// Locks or unlocks the selected point (one undo step).
    pub(super) fn toggle_lock_selected(&mut self) {
        let Some(k) = self.pe.selected.filter(|&k| k < self.cps.len()) else { return };
        let lock = !self.cps[k].locked;
        let n = self.lock_points(vec![k], lock);
        self.lock_status(n, lock);
    }

    /// A click in the add tool while the point that was just placed is still selected: moves
    /// exactly the clicked half of that point instead of starting a second point. The match
    /// search only corrects within a small radius around the click (`local_radius`) and keeps
    /// the click spot when the match is weak or `snap` is off (Alt). Returns false when there is
    /// no such point (the click then starts a new one).
    pub(super) fn replace_fresh_side(&mut self, pane: Pane, p: P2, snap: bool) -> bool {
        let Some(k) = self.pe.fresh.filter(|&k| self.pe.selected == Some(k) && k < self.cps.len()) else { return false };
        let (a, b) = (self.pe.a, self.pe.b);
        let c = self.cps[k];
        if !((c.a == a && c.b == b) || (c.a == b && c.b == a)) {
            return false;
        }
        if c.locked {
            self.set_status("This point is protected: press L (or the lock button) to unlock it before moving it.", WARN);
            return true;
        }
        let (this, other) = if pane == Pane::A { (a, b) } else { (b, a) };
        let fixed_n = if c.a == other { c.pa } else { c.pb };
        let mut final_p = p;
        let mut snapped = false;
        if snap {
            let pf = self.to_px(other, fixed_n);
            let j = self.local_jacobian(other, this, pf, p);
            let r = self.local_radius(this);
            if let Some((q, _)) = self.finetune(other, pf, this, p, j, r).filter(|r| r.1 >= guided::FINETUNE_MIN_NCC) {
                final_p = q;
                snapped = true;
            }
        }
        let n = self.to_norm(this, final_p);
        self.edit_cps("moving a point", |s| {
            let c = &mut s.cps[k];
            if c.a == this {
                c.pa = n;
            } else {
                c.pb = n;
            }
            c.manual = true;
        });
        self.pe.moving = Some(pane);
        self.set_status(
            if snapped {
                "Point moved to where you clicked (corrected slightly to the exact spot). Esc or Ctrl+click starts a new point."
            } else {
                "Point moved exactly to where you clicked. Esc or Ctrl+click starts a new point."
            },
            if snapped { GOOD } else { MUTED },
        );
        true
    }

    pub(super) fn add_point_click(&mut self, pane: Pane, p: P2, mods: egui::Modifiers) {
        // the point that was just placed is still selected: correct it (Ctrl+click: new point)
        if self.pe.pending.is_none() && !mods.command && self.replace_fresh_side(pane, p, !mods.alt) {
            return;
        }
        let snap = !mods.alt;
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
                let snapped = if snap { self.finetune(fixed, pf, this, p, j, r).filter(|r| r.1 >= guided::FINETUNE_MIN_NCC) } else { None };
                let final_p = snapped.map(|r| r.0).unwrap_or(p);
                self.pe.pending = None;
                let n_this = self.to_norm(this, final_p);
                if snapped.is_some() {
                    self.set_status("Point added and snapped to the exact spot.", GOOD);
                } else {
                    self.set_status(if snap { "Point added where you clicked (no clear detail to snap to)." } else { "Point added exactly where you clicked (Alt: no snapping)." }, WARN);
                }
                self.push_manual(q, qn, n_this);
            }
            _ => {
                let pn = self.to_norm(this, p);
                if let Some((guess, j)) = self.predict(this, other, p).filter(|_| snap) {
                    let r = self.placing_radius(other);
                    if let Some((pos, v)) = self.finetune(this, p, other, guess, j, r)
                        && v >= guided::FINETUNE_MIN_NCC {
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
                self.pe.pending = Some((pane, pn));
            }
        }
    }

    /// Brush radius in screen pixels.
    pub(super) fn brush_radius(&self) -> f32 {
        if self.pe.brush_px < 1.0 { select_shapes::BRUSH_PX_DEFAULT } else { self.pe.brush_px }
    }

    /// Applies the selection shape being dragged to the selection. The brush acts on every step
    /// (only the newest piece of the stroke), rectangle and lasso when the drag ends (`finish`).
    /// `pts` are the points of the dragged pane in its photo pixels.
    pub(super) fn apply_select(&mut self, pts: &[(usize, P2)], r_px: f64, finish: bool) {
        use select_shapes::{points_in, SelOp, Shape};
        let Some(sd) = self.pe.sel_drag.as_ref() else { return };
        let tool = self.pe.tool;
        let hits = match tool {
            PointTool::SelectBrush => points_in(&Shape::Brush(&sd.path[sd.path.len().saturating_sub(2)..], r_px), pts),
            PointTool::SelectRect if finish => match (sd.path.first(), sd.path.last()) {
                (Some(a), Some(b)) => points_in(&Shape::Rect(*a, *b), pts),
                _ => return,
            },
            _ => return, // the lasso has its own builder, see `select_shapes::Lasso`
        };
        let op = SelOp::for_tool(tool, sd.mods.shift, sd.mods.command);
        self.apply_hits(&hits, op);
    }

    /// Store a hand-placed pair; `n1` belongs to pane `p1`, `n2` to the other pane.
    pub(super) fn push_manual(&mut self, p1: Pane, n1: [f64; 2], n2: [f64; 2]) {
        let (a, b) = (self.pe.a, self.pe.b);
        let (pa, pb) = if p1 == Pane::A { (n1, n2) } else { (n2, n1) };
        self.push_undo("adding a point");
        self.cps.push(ControlPoint::new(a, b, pa, pb, true));
        self.pe.selected = Some(self.cps.len() - 1);
        self.pe.fresh = self.pe.selected;
        // the half placed second is the one that followed, whichever photo it is in
        self.pe.moving = Some(p1.other());
        self.needs_align = true;
        if self.auto_grow {
            let auto = self.cps.iter().filter(|c| !c.manual && ((c.a == a && c.b == b) || (c.a == b && c.b == a))).count();
            if auto < 10 {
                self.run_pair_points();
            }
        }
    }

    /// Local zoom/tilt between `fixed` and `moving` at `pf` for a snap, from the other points of
    /// the pair (the point at `pf` itself is left out), else from the lenses and a measured
    /// rotation around the guess `pm`. Works the same whichever of the two photos is on the left.
    fn local_jacobian(&self, fixed: usize, moving: usize, pf: P2, pm: P2) -> [f64; 4] {
        let seeds: Vec<(P2, P2)> = self.pair_seeds(fixed, moving).into_iter().filter(|s| (s.0 .0 - pf.0).hypot(s.0 .1 - pf.1) > 0.01).collect();
        let (w, h) = self.work_dims(fixed);
        if seeds.len() >= 2 {
            guided::estimate(&seeds, self.scale_prior(fixed, moving), w.max(h) as f64).map(|t| guided::jacobian(&t, pf))
        } else {
            None
        }
        .unwrap_or_else(|| self.rotation_guess(fixed, pf, moving, pm))
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
        let j = self.local_jacobian(fixed, moving, pf, pm);
        let r = self.local_radius(moving);
        if let Some((q, v)) = self.finetune(fixed, pf, moving, pm, j, r)
            && v >= guided::FINETUNE_MIN_NCC {
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
        let pane_pts = |pane: Pane| -> Vec<(usize, P2)> { pts.iter().map(|(k, pa, pb)| (*k, if pane == Pane::A { *pa } else { *pb })).collect() };
        let brush_screen = self.brush_radius();
        let brush_r = |pane: Pane| -> f64 { (brush_screen / geo[pane.i()].scale) as f64 };
        for ev in events {
            // the lasso tool works like the one of the mask editor (`select_shapes::Lasso`)
            if self.lasso_event(&ev, geo, &px_of, &|pane, pos| nearest(pane, pos).is_some(), pts) {
                continue;
            }
            match ev {
                PaneEvent::LassoClose => {}
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
                PaneEvent::Click(pane, pos, mods) => {
                    if self.pe.tool == PointTool::PatchClick {
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
                    } else if self.pe.tool == PointTool::Add {
                        let p = px_of(pane, pos);
                        let (w, h) = (geo[pane.i()].w as f64, geo[pane.i()].h as f64);
                        if p.0 >= -0.5 && p.1 >= -0.5 && p.0 <= w - 0.5 && p.1 <= h - 0.5 {
                            self.add_point_click(pane, p, mods);
                        }
                    } else if self.pe.tool == PointTool::SelectBrush {
                        // a single click with the brush already selects the points under it
                        self.pe.sel_drag = Some(SelDrag { pane, path: vec![px_of(pane, pos)], mods });
                        self.apply_select(&pane_pts(pane), brush_r(pane), true);
                        self.pe.sel_drag = None;
                        let n = self.pe.sel.len();
                        self.set_status(format!("{n} point{} selected.", if n == 1 { "" } else { "s" }), MUTED);
                    } else if mods.shift || mods.command {
                        // Shift/Ctrl+click: add the point to the selection or take it out again
                        if let Some(k) = nearest(pane, pos) {
                            self.toggle_in_selection(k);
                        }
                    } else {
                        self.pe.sel.clear();
                        self.pe.fresh = None;
                        self.pe.selected = nearest(pane, pos);
                        if self.pe.selected.is_some() {
                            self.pe.moving = Some(pane);
                        }
                        // show the partner in the other photo
                        if let Some(k) = self.pe.selected
                            && let Some((_, pa, pb)) = pts.iter().find(|t| t.0 == k) {
                                let other = pane.other();
                                if self.pe.views[other.i()].zoom > 1.2 {
                                    let q = if other == Pane::A { *pa } else { *pb };
                                    self.pe.views[other.i()].center = Vec2::new(q.0 as f32 + 0.5, q.1 as f32 + 0.5);
                                }
                            }
                    }
                }
                PaneEvent::DragStart(pane, pos, primary, mods) => {
                    // rectangle starts on empty space; on a point the point is dragged as usual
                    let on_point = self.pe.tool != PointTool::SelectBrush && !mods.shift && !mods.command && nearest(pane, pos).is_some();
                    if primary && self.pe.tool.is_select() && !on_point {
                        // a selection shape starts here (the secondary button still moves the photo)
                        self.pe.dragging = None;
                        self.pe.drag_before = None;
                        self.pe.panning = None;
                        self.pe.sel_drag = Some(SelDrag { pane, path: vec![px_of(pane, pos)], mods });
                        self.apply_select(&pane_pts(pane), brush_r(pane), false);
                        continue;
                    }
                    self.pe.dragging = None;
                    self.pe.drag_before = None;
                    self.pe.panning = None;
                    if primary
                        && let Some(k) = nearest(pane, pos) {
                            self.pe.selected = Some(k);
                            if !self.pe.sel.contains(&k) {
                                self.pe.sel.clear();
                            }
                            if self.cps[k].locked {
                                self.set_status("This point is protected: press L (or the lock button) to unlock it before moving it.", WARN);
                                self.pe.panning = Some(pane);
                                continue;
                            }
                            self.pe.dragging = Some((k, pane));
                            self.pe.drag_no_snap = mods.alt;
                            self.pe.drag_before = Some(self.cp_snapshot());
                            self.pe.moving = Some(pane);
                            continue;
                        }
                    self.pe.panning = Some(pane);
                }
                PaneEvent::Drag(pane, pos, delta) => {
                    if self.pe.sel_drag.is_some() {
                        let same = self.pe.sel_drag.as_ref().is_some_and(|s| s.pane == pane);
                        if same {
                            if let Some(sd) = self.pe.sel_drag.as_mut() {
                                sd.path.push(px_of(pane, pos));
                            }
                            self.apply_select(&pane_pts(pane), brush_r(pane), false);
                        }
                    } else if let Some((k, dp)) = self.pe.dragging {
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
                    if let Some(pane) = self.pe.sel_drag.as_ref().map(|s| s.pane) {
                        self.apply_select(&pane_pts(pane), brush_r(pane), true);
                        self.pe.sel_drag = None;
                        self.pe.panning = None;
                        let n = self.pe.sel.len();
                        self.set_status(format!("{n} point{} selected.", if n == 1 { "" } else { "s" }), MUTED);
                        continue;
                    }
                    if let Some((k, pane)) = self.pe.dragging.take() {
                        // Alt held at the start of the drag: the point stays exactly where it was dropped
                        if !self.pe.drag_no_snap {
                            self.snap_point(k, pane);
                        }
                    }
                    // the whole drag (and the snap) is one step, none if the point did not move
                    if let Some(before) = self.pe.drag_before.take() {
                        self.commit_undo("moving a point", before);
                    }
                    self.pe.panning = None;
                }
            }
        }
    }
}

/// Same points in the same order (position, photos, kind, weight, hand-placed). The alignment
/// error and the patch-match results are derived values and do not count as a change.
pub(super) fn same_points(a: &[ControlPoint], b: &[ControlPoint]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.a == y.a && x.b == y.b && x.pa == y.pa && x.pb == y.pb && x.manual == y.manual && x.locked == y.locked && x.weight == y.weight && x.kind == y.kind)
}

#[cfg(test)]
mod undo_tests {
    use super::super::masks_tests::app_with_photos;
    use super::*;

    fn pts(n: usize) -> Vec<ControlPoint> {
        (0..n)
            .map(|i| {
                let mut c = ControlPoint::new(0, 1, [i as f64 * 0.01, 0.1], [0.2, i as f64 * 0.02], i % 3 == 0);
                c.err = i as f64;
                c.weight = 1.0 + i as f64;
                c
            })
            .collect()
    }

    fn setup() -> PanoApp {
        let mut app = app_with_photos(2);
        app.cps = pts(8);
        app.needs_align = false;
        app
    }

    #[test]
    fn delete_undo_restores_exactly_and_redo_deletes_again() {
        let mut app = setup();
        let orig = app.cps.clone();
        app.delete_points(vec![5, 1, 1, 7], "deleting 3 points");
        assert_eq!(app.cps.len(), 5);
        assert!(app.needs_align);
        app.needs_align = false;
        app.undo_last();
        assert_eq!(app.cps.len(), 8);
        for (x, y) in app.cps.iter().zip(&orig) {
            assert!(x.a == y.a && x.b == y.b && x.pa == y.pa && x.pb == y.pb && x.err == y.err && x.manual == y.manual && x.weight == y.weight);
        }
        assert!(app.needs_align && app.cp_rows_cache.is_none() && app.dup_cache.is_none());
        app.redo_last();
        assert_eq!(app.cps.len(), 5);
        assert_eq!(app.cps[1].pa, orig[2].pa);
    }

    #[test]
    fn protected_points_survive_list_delete_and_locking_is_one_undo_step() {
        let mut app = setup();
        app.cps[2].locked = true;
        let kept = app.delete_points(vec![1, 2, 3], "deleting 3 points");
        assert_eq!(kept, 1);
        assert_eq!(app.cps.len(), 6);
        assert!(app.cps.iter().any(|c| c.locked));
        // single delete of a protected point is refused
        let k = app.cps.iter().position(|c| c.locked).unwrap();
        app.pe.selected = Some(k);
        app.delete_selected();
        assert_eq!(app.cps.len(), 6);
        // unlock = one undo step, undo protects again
        app.toggle_lock_selected();
        assert!(!app.cps[k].locked);
        app.undo_last();
        assert!(app.cps[k].locked);
        app.redo_last();
        assert!(!app.cps[k].locked);
        app.pe.selected = Some(k);
        app.delete_selected();
        assert_eq!(app.cps.len(), 5);
    }

    #[test]
    fn lock_points_unlocks_a_whole_set_as_one_undo_step() {
        let mut app = setup();
        for k in [1, 2, 5] {
            app.cps[k].locked = true;
        }
        // unlocking 1, 2 and 4 (4 is not locked): two change, one step, then the delete works
        assert_eq!(app.lock_points(vec![4, 2, 1, 2, 99], false), 2);
        assert!(!app.cps[1].locked && !app.cps[2].locked && app.cps[5].locked);
        assert_eq!(app.lock_points(vec![1, 2], false), 0, "nothing to do is no undo step");
        assert_eq!(app.delete_points(vec![1, 2, 5], "deleting 3 points"), 1);
        assert_eq!(app.cps.len(), 6);
        app.undo_last();
        app.undo_last();
        assert!(app.cps[1].locked && app.cps[2].locked && app.cps[5].locked, "one undo brings the protection back");
        assert_eq!(app.lock_points(vec![0, 1], true), 1);
        assert!(app.cps[0].locked);
    }

    fn app_with_fresh_point() -> PanoApp {
        let mut app = setup();
        app.cps.clear();
        app.auto_grow = false;
        app.pe.a = 0;
        app.pe.b = 1;
        app.pe.tool = PointTool::Add;
        app.push_manual(Pane::A, [0.0, 0.0], [0.1, 0.1]);
        assert_eq!(app.pe.fresh, app.pe.selected);
        app
    }

    #[test]
    fn a_click_after_adding_moves_that_side_instead_of_adding_a_second_point() {
        let mut app = app_with_fresh_point();
        let old_a = app.cps[0].pa;
        // Alt: no snapping at all, the point lands exactly on the click
        app.add_point_click(Pane::B, (30.0, 20.0), egui::Modifiers::ALT);
        assert_eq!(app.cps.len(), 1);
        assert_eq!(app.cps[0].pb, app.to_norm(1, (30.0, 20.0)));
        assert_eq!(app.cps[0].pa, old_a, "the other side stays");
        assert!(app.cps[0].manual);
        // again, on the other side, with snapping: stays near the click (small search radius only)
        app.add_point_click(Pane::A, (50.0, 40.0), egui::Modifiers::NONE);
        assert_eq!(app.cps.len(), 1);
        let (x, y) = app.to_px(0, app.cps[0].pa);
        assert!((x - 50.0).hypot(y - 40.0) <= app.local_radius(0) + 1.0, "moved to ({x}, {y})");
        // each correction is one undo step
        app.undo_last();
        assert_eq!(app.cps[0].pb, app.to_norm(1, (30.0, 20.0)));
        assert_eq!(app.cps[0].pa, old_a);
    }

    /// The point's position in photo `idx` (whichever of its two sides that is).
    fn side_of(c: &ControlPoint, idx: usize) -> [f64; 2] {
        if c.a == idx { c.pa } else { c.pb }
    }

    #[test]
    fn both_photos_behave_the_same_whichever_is_on_the_left() {
        // the pair shown as (0, 1) or (1, 0), the click in the left or in the right pane
        for (a, b) in [(0usize, 1usize), (1, 0)] {
            for pane in [Pane::A, Pane::B] {
                let mut app = setup();
                app.cps.clear();
                app.auto_grow = false;
                app.pe.a = a;
                app.pe.b = b;
                app.pe.tool = PointTool::Add;
                let idx = if pane == Pane::A { a } else { b };
                let other = if pane == Pane::A { b } else { a };
                // start in `pane` (Alt: no snapping, no partner search), finish in the other pane
                app.add_point_click(pane, (10.0, 12.0), egui::Modifiers::ALT);
                assert!(app.pe.pending.is_some(), "no partner is searched with Alt");
                app.add_point_click(pane.other(), (40.0, 30.0), egui::Modifiers::ALT);
                assert_eq!(app.cps.len(), 1);
                assert_eq!(side_of(&app.cps[0], idx), app.to_norm(idx, (10.0, 12.0)), "pair ({a},{b}) first click in {pane:?}");
                assert_eq!(side_of(&app.cps[0], other), app.to_norm(other, (40.0, 30.0)));
                // the frame of the new point shows the first-placed half as the fixed one
                assert_eq!(app.pe.moving, Some(pane.other()));
                // a click in either pane moves exactly that side, the other stays
                for click in [pane, pane.other()] {
                    let ci = if click == Pane::A { a } else { b };
                    let oi = if click == Pane::A { b } else { a };
                    let keep = side_of(&app.cps[0], oi);
                    app.add_point_click(click, (25.0, 33.0), egui::Modifiers::ALT);
                    assert_eq!(app.cps.len(), 1);
                    assert_eq!(side_of(&app.cps[0], ci), app.to_norm(ci, (25.0, 33.0)));
                    assert_eq!(side_of(&app.cps[0], oi), keep);
                    assert_eq!(app.pe.moving, Some(click));
                    // snapping on: stays within the small local radius of the click
                    app.add_point_click(click, (50.0, 40.0), egui::Modifiers::NONE);
                    let (x, y) = app.to_px(ci, side_of(&app.cps[0], ci));
                    assert!((x - 50.0).hypot(y - 40.0) <= app.local_radius(ci) + 1.0);
                    assert_eq!(side_of(&app.cps[0], oi), keep);
                }
            }
        }
    }

    #[test]
    fn ctrl_click_or_esc_state_starts_a_new_point_and_a_protected_point_is_not_moved() {
        let mut app = app_with_fresh_point();
        // Ctrl+click (Alt: no automatic partner search, so the test does not depend on the picture): first half of a second point, nothing moved
        app.add_point_click(Pane::A, (10.0, 10.0), egui::Modifiers { alt: true, ..egui::Modifiers::COMMAND });
        assert_eq!(app.cps.len(), 1);
        assert!(app.pe.pending.is_some());
        app.pe.pending = None;
        // after "Esc" (fresh cleared) the next click starts a new point as well
        app.pe.fresh = None;
        app.add_point_click(Pane::A, (10.0, 10.0), egui::Modifiers::ALT);
        assert!(app.pe.pending.is_some());
        // a protected fresh point is not moved by a click
        let mut app = app_with_fresh_point();
        app.cps[0].locked = true;
        let before = app.cps[0];
        app.add_point_click(Pane::B, (30.0, 20.0), egui::Modifiers::ALT);
        assert!(app.cps[0].pa == before.pa && app.cps[0].pb == before.pb && app.cps.len() == 1);
    }

    #[test]
    fn new_change_after_undo_drops_redo_and_nothing_changed_is_no_step() {
        let mut app = setup();
        app.delete_points(vec![0], "deleting a point");
        app.undo_last();
        assert!(app.undo.can_redo());
        // nothing to delete: no step, redo stays
        app.delete_points(vec![99], "deleting a point");
        assert!(app.undo.can_redo() && !app.undo.can_undo());
        assert!(!app.edit_cps("noop", |_| {}));
        app.edit_cps("adding a point", |s| s.cps.push(ControlPoint::new(0, 1, [0.0; 2], [0.0; 2], true)));
        assert!(!app.undo.can_redo() && app.undo.can_undo());
        assert_eq!(app.cps.len(), 9);
        app.undo_last();
        assert_eq!(app.cps.len(), 8);
    }

    #[test]
    fn only_derived_values_changing_is_no_step() {
        let mut app = setup();
        assert!(!app.edit_cps("aligning", |s| s.cps.iter_mut().for_each(|c| c.err += 1.0)));
        assert!(!app.undo.can_undo());
    }
}
