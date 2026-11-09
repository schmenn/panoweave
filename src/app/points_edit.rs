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

    /// Locks or unlocks the selected point (one undo step).
    pub(super) fn toggle_lock_selected(&mut self) {
        let Some(k) = self.pe.selected.filter(|&k| k < self.cps.len()) else { return };
        let lock = !self.cps[k].locked;
        self.edit_cps(if lock { "protecting a point" } else { "unlocking a point" }, |s| s.cps[k].locked = lock);
        self.set_status(if lock { "Point protected: no bulk action touches it, and it can't be deleted or moved until unlocked." } else { "Point unlocked." }, MUTED);
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
            if self.pe.tool == PointTool::Area && self.area_event(&ev, &px_of, &|pane| geo[pane.i()].scale) {
                continue;
            }
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
                            self.add_point_click(pane, p);
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
                        self.snap_point(k, pane);
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
