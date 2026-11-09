use super::*;

impl PanoApp {
    pub(super) fn ui_points(&mut self, ui: &mut egui::Ui) {
        let n = self.photos.len();
        if n < 2 {
            ui.label("Add at least two photos first.");
            return;
        }
        self.pe.a = self.pe.a.min(n - 1);
        self.pe.b = self.pe.b.min(n - 1);
        if self.pe.a == self.pe.b {
            self.pe.b = if self.pe.a + 1 < n { self.pe.a + 1 } else { 0 };
        }
        if self.click.pair != (self.pe.a, self.pe.b) {
            // other photos on screen (switch, swap sides, photo removed): the clicks were made in the old ones
            self.click = ClickState { pair: (self.pe.a, self.pe.b), ..Default::default() };
        }
        let ctx = ui.ctx().clone();
        self.points_toolbar_all(ui);
        let (a, b) = (self.pe.a, self.pe.b);
        self.points_toolbar_pair(ui, a, b);
        self.points_hint(ui);

        self.ensure_tex(&ctx, a);
        self.ensure_tex(&ctx, b);
        self.request_full(a);
        self.request_full(b);
        // drop big textures of photos that are no longer shown
        self.full_tex.retain(|k, _| *k == a || *k == b);


        let avail = ui.available_size();
        let pane_w = ((avail.x - 10.0) / 2.0).max(100.0);
        let header_h = 30.0;
        let pane_h = (avail.y - header_h - 8.0).max(100.0);
        let mut events: Vec<PaneEvent> = Vec::new();
        let mut geo = [PaneGeo::default(); 2];
        let pair_idx: Vec<usize> = (0..self.cps.len()).filter(|&k| (self.cps[k].a == a && self.cps[k].b == b) || (self.cps[k].a == b && self.cps[k].b == a)).collect();
        // positions of the pair's points per pane (work px)
        let pts: Vec<(usize, P2, P2)> = pair_idx
            .iter()
            .map(|&k| {
                let c = &self.cps[k];
                let (pa, pb) = if c.a == a { (c.pa, c.pb) } else { (c.pb, c.pa) };
                (k, self.to_px(a, pa), self.to_px(b, pb))
            })
            .collect();
        self.sync_selection();
        let frame = self.selection_frame();
        let mut new_a = a;
        let mut new_b = b;
        ui.horizontal_top(|ui| {
            for pane in [Pane::A, Pane::B] {
                let lay = PaneLayout { a, b, n, pane_w, pane_h, frame };
                ui.allocate_ui_with_layout(Vec2::new(pane_w, header_h + pane_h + 8.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    self.draw_pane(ui, pane, &lay, &pts, &mut events, &mut geo, &mut new_a, &mut new_b);
                });
                if pane == Pane::A {
                    ui.add_space(6.0);
                }
            }
        });
        if new_a != a || new_b != b {
            if new_a != a && new_a == b {
                new_b = a; // keep two different photos
            }
            self.pe.a = new_a;
            self.pe.b = new_b;
            self.pe.selected = None;
            self.pe.sel.clear();
            self.pe.pending = None;
            return;
        }
        self.handle_pane_events(events, &geo, &pts);
        if ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)) {
            self.delete_selected();
        }
        if ui.input(|i| i.key_pressed(egui::Key::L) && !i.modifiers.any()) && !ui.ctx().egui_wants_keyboard_input() {
            self.toggle_lock_selected();
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            // Esc cancels a half-placed point first, otherwise it leaves the tool
            self.pe.sel_drag = None;
            if self.pe.pending.take().is_none() {
                self.pe.tool = PointTool::Edit;
            }
        }
    }

    /// Toolbar row for all photos: find points, detail, sky filter.
    fn points_toolbar_all(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if primary_button(ui, if self.matched { "🔍  Find points in all photos again" } else { "🔍  Find points in all photos" }, !self.busy()).clicked() {
                self.run_match(false);
            }
            ui.label("Detail:");
            egui::ComboBox::from_id_salt("density")
                .selected_text(match self.match_settings.points_per_photo {
                    0..=2000 => "Fast",
                    2001..=4000 => "Balanced",
                    _ => "Thorough",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.match_settings.points_per_photo, 1500, "Fast");
                    ui.selectable_value(&mut self.match_settings.points_per_photo, 3500, "Balanced");
                    ui.selectable_value(&mut self.match_settings.points_per_photo, 8000, "Thorough (difficult scenes)");
                });
            ui.checkbox(&mut self.match_settings.ignore_sky, "Ignore sky & clouds")
                .on_hover_text("Clouds move between shots, so points on them pull the photos apart. Turn off only if the sky is the only overlap.");
            ui.label("Points per pair:");
            ui.add(egui::DragValue::new(&mut self.match_settings.points_per_pair).range(8..=300));
            if let Some((k, d, m, pairs)) = self.last_match {
                ui.label(RichText::new(format!("last run: {k} features, {pairs} pairs, {}", fmt_ms(d + m))).small().color(MUTED));
            }
        });
    }

    /// Toolbar rows for the pair on screen: find/add/undo/delete/clean up.
    fn points_toolbar_pair(&mut self, ui: &mut egui::Ui, a: usize, b: usize) {
        let (cnt_ab, _, _) = self.pair_stats(a, b);
        ui.horizontal_wrapped(|ui| {
            let label = if cnt_ab == 0 { "🔍  Find points for these two photos" } else { "🔍  Find more points for these two photos" };
            if primary_button(ui, label, !self.busy())
                .on_hover_text("Searches only this pair. Without points it works fully automatically (also for photos with different zoom). With your points it starts from them and fills the overlap.")
                .clicked()
            {
                self.run_pair_points();
            }
        });
        // Row 2: tools and history. Every row below holds only normal-height widgets, so wrapped lines keep one height.
        ui.horizontal_wrapped(|ui| {
            let selecting = self.pe.tool == PointTool::Edit || self.pe.tool.is_select();
            if ui
                .add(egui::Button::selectable(selecting, "↖ Select / edit"))
                .on_hover_text("Click a point to select it, drag it to move it, Delete removes it. Dragging empty space moves the photo, the wheel zooms.")
                .clicked()
            {
                self.pe.tool = PointTool::Edit;
                self.pe.pending = None;
                self.pe.sel_drag = None;
            }
            // the selection shapes belong to Select / edit: shown right after it, only while it is on
            if selecting {
                for (tool, text, tip) in [
                    (PointTool::Edit, "Click", "Click a point to select it (Shift/Ctrl+click adds or takes it out), drag it to move it."),
                    (PointTool::SelectRect, "▭ Rectangle", "Drag a rectangle on empty space to select the points inside. Shift adds to the selection, Ctrl takes points out. Points can still be dragged."),
                    (PointTool::SelectLasso, "➰ Lasso", "Drag a free outline on empty space to select the points inside. Shift adds to the selection, Ctrl takes points out. Points can still be dragged."),
                    (PointTool::SelectBrush, "🖌 Brush", "Click or paint over points to select them. Ctrl takes points out again."),
                ] {
                    if ui.add(egui::Button::selectable(self.pe.tool == tool, text)).on_hover_text(tip).clicked() {
                        self.pe.tool = tool;
                        self.pe.pending = None;
                        self.pe.sel_drag = None;
                    }
                }
                if self.pe.tool == PointTool::SelectBrush {
                    if self.pe.brush_px < 1.0 {
                        self.pe.brush_px = select_shapes::BRUSH_PX_DEFAULT;
                    }
                    ui.add(egui::DragValue::new(&mut self.pe.brush_px).range(select_shapes::BRUSH_PX_RANGE).speed(0.5).suffix(" px")).on_hover_text("Brush radius");
                }
            }
            ui.separator();
            let adding = self.pe.tool == PointTool::Add;
            let add_txt = if adding { "✔ Done adding" } else { "➕ Add point" };
            if ui
                .add(egui::Button::selectable(adding, add_txt))
                .on_hover_text("Click a spot in one photo, then the same spot in the other. Points snap to the exact spot automatically.")
                .clicked()
            {
                self.pe.tool = PointTool::toggle(self.pe.tool, PointTool::Add);
                self.pe.pending = None;
            }
            if self.pe.tool == PointTool::Add {
                ui.checkbox(&mut self.auto_grow, "then find more automatically");
            }
            if ui
                .add(egui::Button::selectable(self.pe.tool == PointTool::PatchClick, "🎯 Patch click"))
                .on_hover_text("Click roughly the same spot left and right: the spot is refined by patch matching (also with different zoom) and more matching patches around it are proposed.")
                .clicked()
            {
                self.pe.tool = PointTool::toggle(self.pe.tool, PointTool::PatchClick);
                self.pe.pending = None;
            }
            if ui.add_enabled(self.undo.can_undo(), egui::Button::new("↶ Undo")).on_hover_text(self.undo.undo_label().map(|w| format!("Undo {w} (Ctrl+Z)")).unwrap_or_default()).clicked() {
                self.undo_last();
            }
            if ui.add_enabled(self.undo.can_redo(), egui::Button::new("↷ Redo")).on_hover_text(self.undo.redo_label().map(|w| format!("Redo {w} (Ctrl+Y)")).unwrap_or_default()).clicked() {
                self.redo_last();
            }
            if ui.button("🕸 Connections").on_hover_text("Which photos are joined by points, which overlapping pairs have too few").clicked() {
                self.show_graph = !self.show_graph;
            }
        });
        // Row 3: matching parameters.
        ui.horizontal_wrapped(|ui| {
            ui.label("Match window").on_hover_text("Size of the patch around a point that is compared between the two photos (red frame).");
            if ui.add(egui::DragValue::new(&mut self.tpl_size).range(7..=81).speed(0.5).suffix(" px")).changed() {
                self.tpl_size |= 1;
            }
            ui.label("Search area").on_hover_text("How far around a clicked spot the matching spot is searched (dashed frame), in % of the photo width, at most 500 px.");
            ui.add(egui::DragValue::new(&mut self.search_pct).range(1.0..=50.0).speed(0.2).suffix(" %"));
        });
        // Row 4: actions on points and views.
        ui.horizontal_wrapped(|ui| {
            let n_sel = self.selection_indices().len();
            let del_txt = if n_sel > 1 { format!("🗑 Delete selected ({n_sel})") } else { "🗑 Delete point".to_string() };
            if ui
                .add_enabled(n_sel > 0, egui::Button::new(del_txt))
                .on_hover_text("Shift/Ctrl+click adds points to the selection. Protected points are never deleted.")
                .clicked()
            {
                self.delete_selected();
            }
            if ui
                .add_enabled(n_sel > 0 && !self.busy(), egui::Button::new(format!("🧲 Snap selected ({n_sel})")))
                .on_hover_text("Refine the selected points like after dragging one: the right photo's side is searched again (match >= 80 %), the left one stays. Protected points are skipped. Runs in the background, one undo step.")
                .clicked()
            {
                self.snap_selected();
            }
            let sel_locked = self.pe.selected.and_then(|k| self.cps.get(k)).map(|c| c.locked);
            if ui
                .add_enabled(sel_locked.is_some(), egui::Button::selectable(sel_locked == Some(true), "🔒 Protect"))
                .on_hover_text("Protect the selected point (L): no bulk action deletes or changes it, and it can't be deleted or moved until unlocked.")
                .clicked()
            {
                self.toggle_lock_selected();
            }
            if ui.button("Delete all in this pair").clicked() {
                let n = self.cps.iter().filter(|c| !c.locked && ((c.a == a && c.b == b) || (c.a == b && c.b == a))).count();
                let mut kept = 0;
                self.edit_cps(&format!("deleting {n} points of photos {}/{}", a + 1, b + 1), |s| {
                    kept = crate::align::remove_unprotected(&mut s.cps, |c| (c.a == a && c.b == b) || (c.a == b && c.b == a));
                });
                self.pe.selected = None;
                if kept > 0 {
                    self.set_status(format!("Deleted {n} points.{}", crate::align::protected_note(kept)), MUTED);
                }
            }
            if ui.button("🔍 Check overlap").on_hover_text("Lay the two photos over each other: blend, difference, checkerboard, shift arrows").clicked() {
                self.show_overlap = !self.show_overlap;
            }
            if ui.button("📋 All points (F3)").on_hover_text("List of all control points, sortable by accuracy").clicked() {
                self.show_cp_list = !self.show_cp_list;
            }
            if ui.button("⇄ Swap sides").clicked() {
                self.pe.a = b;
                self.pe.b = a;
                self.pe.views.swap(0, 1);
                self.pe.pending = None;
            }
        });
        // Row 5: clean up by alignment error.
        ui.horizontal_wrapped(|ui| {
            {
                let on = self.aligned;
                ui.add_enabled(on, egui::Label::new("Remove points worse than"));
                ui.add_enabled(on, egui::DragValue::new(&mut self.err_threshold).range(0.5..=100.0).speed(0.1).suffix(" px"));
                if ui.add_enabled(on, egui::Button::new("Clean up")).clicked() {
                    let before = self.cps.len();
                    let t = self.err_threshold;
                    let n = self.cps.iter().filter(|c| c.err > t && !c.locked).count();
                    let mut kept = 0;
                    self.edit_cps(&format!("cleaning up {n} points"), |s| kept = crate::align::remove_unprotected(&mut s.cps, |c| c.err > t));
                    self.pe.selected = None;
                    let removed = before - self.cps.len();
                    self.set_status(format!("Removed {removed} points.{} Re-align in step 3 to update.", crate::align::protected_note(kept)), MUTED);
                }
            }
            ui.separator();
            self.sky_ui(ui, false);
            self.foliage_ui(ui);
        });
    }

    /// Hint line under the toolbar (or the strip of the patch click mode).
    fn points_hint(&mut self, ui: &mut egui::Ui) {
        let adding = self.pe.tool == PointTool::Add;
        let hint = if self.pe.tool.is_select() {
            "Drag in a photo to select points (the right mouse button still moves the photo). Shift adds, Ctrl takes out. Delete removes the selection; protected points stay. Esc leaves the tool."
        } else if adding {
            match self.pe.pending {
                Some((Pane::A, _)) => "Now click the same spot in the RIGHT photo – it snaps to the exact spot.",
                Some((Pane::B, _)) => "Now click the same spot in the LEFT photo – it snaps to the exact spot.",
                None => "Click a distinctive spot (a corner, a sharp detail) in either photo. Esc cancels.",
            }
        } else {
            "Scroll to zoom, drag the photo to move it, double-click to fit. Click a point to select it: red = match window, dashed white = search area. Drag it to move it (edges are shown, it snaps to the exact spot), Delete removes it."
        };
        if self.pe.tool == PointTool::PatchClick {
            self.click_panel(ui);
        } else {
            ui.label(RichText::new(hint).small().color(if adding { ACCENT } else { MUTED }));
        }
    }
}
