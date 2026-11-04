use super::*;

impl PanoApp {
    pub(super) fn run_match(&mut self, chain: bool) {
        if self.busy() || self.photos.len() < 2 {
            return;
        }
        self.auto_chain = chain;
        let grays: Vec<Arc<Gray>> = self.photos.iter().map(|p| p.gray.clone()).collect();
        let works: Vec<Arc<RgbImage>> = self.photos.iter().map(|p| p.work.clone()).collect();
        let manual: Vec<ControlPoint> = self.cps.iter().filter(|c| c.manual).copied().collect();
        let settings = self.match_settings;
        self.start_job("Finding matching points", move |report, _| {
            JobMsg::Matched(engine::find_points(&works, &grays, &manual, settings, report))
        });
    }

    /// "Search again" for one pair (connection view): part 1 for this pair only.
    pub(super) fn run_pair_recheck(&mut self, a: usize, b: usize) {
        if self.busy() {
            return;
        }
        if !self.aligned {
            self.set_status("Align the photos first – the search uses the estimated overlap.", WARN);
            return;
        }
        let lens_of: Vec<usize> = self.photos.iter().map(|p| p.lens).collect();
        let lenses = self.lenses.clone();
        let rots: Vec<M3> = self.photos.iter().map(|p| p.rot).collect();
        let active = self.active_indices();
        let used: Vec<bool> = (0..self.photos.len()).map(|i| active.contains(&i)).collect();
        let cps: Vec<ControlPoint> = self.cps.iter().filter(|c| active.contains(&c.a) && active.contains(&c.b)).copied().collect();
        let cfg = self.pair_cfg.clone();
        let matcher = self.fine_cfg.matcher.clone();
        let ms = self.match_settings;
        let fa: Vec<fine_align::FaPhoto> = self.photos.iter().map(|p| fine_align::FaPhoto { gray: p.gray.clone(), rgb: p.work.clone() }).collect();
        self.start_job(&format!("Searching photos {} and {} again", a + 1, b + 1), move |report, _| {
            let (cps, reports, _) = crate::pair_check::check_pairs(&fa, &lens_of, &lenses, &rots, &cps, &used, Some((a, b)), &cfg, &matcher, ms, report);
            for l in crate::pair_check::report_lines(&reports) {
                eprintln!("pair check: {l}");
            }
            JobMsg::PairChecked { a, b, cps, reports }
        });
    }

    pub(super) fn on_pair_checked(&mut self, a: usize, b: usize, cps: Vec<ControlPoint>, reports: Vec<crate::pair_check::PairReport>) {
        let n = cps.len();
        if let Some(sm) = &mut self.pair_summary {
            for r in &reports {
                sm.reports.retain(|x| !(x.a == r.a && x.b == r.b));
                sm.reports.push(r.clone());
            }
            sm.reports.sort_by_key(|r| (r.a, r.b));
        }
        let steps: Vec<String> = reports.iter().flat_map(|r| r.steps.iter().map(|s| format!("{}: {}", s.step, s.note))).collect();
        if reports.is_empty() {
            self.set_status(format!("Photos {} and {}: the estimated overlap is below {:.0} % or the pair already has enough points.", a + 1, b + 1, self.pair_cfg.min_overlap * 100.0), WARN);
            return;
        }
        self.cps.extend(cps);
        if n > 0 {
            self.set_status(format!("Photos {} and {}: {n} new points ({}). Aligning again…", a + 1, b + 1, steps.join("; ")), GOOD);
            self.needs_align = true;
            self.run_align(false);
        } else {
            self.set_status(format!("Photos {} and {}: no reliable points found ({}). Try “Manual”.", a + 1, b + 1, steps.join("; ")), WARN);
        }
    }

    /// Patch click mode: both rough clicks are there, refine them in a worker thread.
    pub(super) fn run_click(&mut self) {
        let (Some(ca), Some(cb)) = (self.click.a, self.click.b) else { return };
        if self.busy() {
            return;
        }
        let (a, b) = (self.pe.a, self.pe.b);
        // local transform: from points of this pair, else from the alignment, else searched
        let seeds = self.pair_seeds(a, b);
        let hint: Option<([f64; 4], String)> = if let Some(j) = crate::click_match::similarity_from(&seeds) {
            Some((j, format!("from the {} points of this pair", seeds.len())))
        } else if seeds.len() == 1 {
            None
        } else if self.aligned && !self.needs_align {
            self.predict(a, b, ca).map(|(_, j)| (j, "from the alignment (lens, zoom, rotation)".to_string()))
        } else {
            None
        };
        let s0 = self.scale_prior(a, b);
        let (ga, gb) = (self.photos[a].gray.clone(), self.photos[b].gray.clone());
        let cfg = self.click_cfg.clone();
        // what is known besides the clicks: the points of this pair and the alignment at the click
        let align = if self.aligned && !self.needs_align { self.predict_from_alignment(a, b, ca).map(|(_, j)| j) } else { None };
        let priors = crate::click_match::ClickPriors { seeds, align };
        self.start_job("Patch click: refining", move |_, _| JobMsg::ClickMatched(Box::new(crate::click_match::run(a, b, &ga, &gb, ca, cb, hint, &priors, s0, &cfg))));
    }

    pub(super) fn on_click_matched(&mut self, r: crate::click_match::ClickResult) {
        for l in &r.log {
            eprintln!("patch click: {l}");
        }
        match (&r.error, &r.main) {
            (Some(e), _) => self.set_status(e.clone(), BAD),
            (None, Some(m)) => self.set_status(
                format!(
                    "Patch click: refined (score {:.3}, confidence {:.2}, zoom ×{:.2}, rotation {:.1}°) and {} more pairs found – accept or discard them below.",
                    m.score,
                    m.conf,
                    r.zoom(),
                    r.rotation_deg(),
                    r.more.len()
                ),
                GOOD,
            ),
            _ => {}
        }
        if r.error.is_none() {
            // the refined spot replaces the rough right click
            if let Some(m) = &r.main {
                self.click.b = Some(m.pb);
            }
        }
        self.click.result = Some(r);
    }

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

    /// Accept the checked suggestions of the click mode as control points.
    pub(super) fn accept_click(&mut self, all: bool) {
        let Some(r) = self.click.result.clone() else { return };
        if r.error.is_some() || r.a != self.pe.a || r.b != self.pe.b {
            return;
        }
        self.push_undo("adding the patch click points");
        let mut n = 0;
        for s in r.main.iter().chain(r.more.iter()) {
            if !(all || s.accept) {
                continue;
            }
            let mut c = ControlPoint::new(r.a, r.b, self.to_norm(r.a, s.pa), self.to_norm(r.b, s.pb), s.main);
            c.conf = s.conf;
            c.score = s.score;
            // confidence as weight; the clicked point counts more (like hand-drawn areas)
            c.weight = if s.main { self.click_cfg.manual_weight * (s.conf as f64).max(0.05) } else { (s.conf as f64).max(1e-3) };
            self.cps.push(c);
            n += 1;
        }
        self.click.result = None;
        self.click.a = None;
        self.click.b = None;
        self.needs_align = true;
        self.set_status(format!("{n} points added from the patch click. Align again (step 3) to use them. Ctrl+Z undoes it."), GOOD);
    }

    pub(super) fn discard_click(&mut self) {
        if self.click.result.is_some() {
            self.push_undo("discarding the patch click suggestions");
        }
        self.click.result = None;
        self.click.a = None;
        self.click.b = None;
    }

    pub(super) fn on_matched(&mut self, m: MatchResult) {
        self.cps = m.cps;
        self.matched = true;
        self.needs_align = true;
        self.last_match = Some((m.keypoints, m.detect_ms, m.match_ms, m.pairs.len()));
        let n = self.photos.len();
        let comps = crate::align::components(n, &self.cps);
        let groups = comps.iter().max().map(|v| v + 1).unwrap_or(0);
        let msg = format!(
            "Found {} matching points between {} photo pairs in {}.",
            self.cps.len(),
            m.pairs.len(),
            fmt_ms(m.detect_ms + m.match_ms)
        );
        if groups > 1 {
            self.set_status(format!("{msg} Some photos have no overlap with the rest – see step 3."), WARN);
        } else {
            self.set_status(msg, GOOD);
        }
        if let Some(p) = self.cps.first() {
            self.pe.a = p.a;
            self.pe.b = p.b;
        }
        if self.auto_chain {
            self.run_align(true);
        } else if self.step == Step::Photos {
            self.step = Step::Points;
        }
    }

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
        let ctx = ui.ctx().clone();
        // --- toolbar: all photos ---
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
        let (a, b) = (self.pe.a, self.pe.b);
        let (cnt_ab, _, _) = self.pair_stats(a, b);
        // --- toolbar: this pair ---
        ui.horizontal_wrapped(|ui| {
            let label = if cnt_ab == 0 { "🔍  Find points for these two photos" } else { "🔍  Find more points for these two photos" };
            if primary_button(ui, label, !self.busy())
                .on_hover_text("Searches only this pair. Without points it works fully automatically (also for photos with different zoom). With your points it starts from them and fills the overlap.")
                .clicked()
            {
                self.run_pair_points();
            }
            let add_txt = if self.pe.adding { "✔ Done adding" } else { "➕ Add point" };
            if ui
                .add(egui::Button::selectable(self.pe.adding, add_txt))
                .on_hover_text("Click a spot in one photo, then the same spot in the other. Points snap to the exact spot automatically.")
                .clicked()
            {
                self.pe.adding = !self.pe.adding;
                self.pe.pending = None;
            }
            ui.checkbox(&mut self.auto_grow, "then find more automatically");
            if ui
                .add(egui::Button::selectable(self.click.on, "🎯 Patch click"))
                .on_hover_text("Click roughly the same spot left and right: the spot is refined by patch matching (also with different zoom) and more matching patches around it are proposed.")
                .clicked()
            {
                self.click.on = !self.click.on;
                self.pe.adding = false;
                self.pe.pending = None;
            }
            if ui.add_enabled(!self.undo.is_empty(), egui::Button::new("↶ Undo")).on_hover_text(self.undo.last().map(|u| format!("Undo {} (Ctrl+Z)", u.what)).unwrap_or_default()).clicked() {
                self.undo_last();
            }
            if ui.button("🕸 Connections").on_hover_text("Which photos are joined by points, which overlapping pairs have too few").clicked() {
                self.show_graph = !self.show_graph;
            }
            ui.separator();
            ui.label("Match window").on_hover_text("Size of the patch around a point that is compared between the two photos (red frame).");
            if ui.add(egui::DragValue::new(&mut self.tpl_size).range(7..=81).speed(0.5).suffix(" px")).changed() {
                self.tpl_size |= 1;
            }
            ui.label("Search area").on_hover_text("How far around a clicked spot the matching spot is searched (dashed frame), in % of the photo width, at most 500 px.");
            ui.add(egui::DragValue::new(&mut self.search_pct).range(1.0..=50.0).speed(0.2).suffix(" %"));
            if ui.add_enabled(self.pe.selected.is_some(), egui::Button::new("🗑 Delete point")).clicked() {
                self.delete_selected();
            }
            if ui.button("Delete all in this pair").clicked() {
                self.cps.retain(|c| !((c.a == a && c.b == b) || (c.a == b && c.b == a)));
                self.pe.selected = None;
                self.needs_align = true;
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
            ui.separator();
            {
                let on = self.aligned;
                ui.add_enabled(on, egui::Label::new("Remove points worse than"));
                ui.add_enabled(on, egui::DragValue::new(&mut self.err_threshold).range(0.5..=100.0).speed(0.1).suffix(" px"));
                if ui.add_enabled(on, egui::Button::new("Clean up")).clicked() {
                    let before = self.cps.len();
                    let t = self.err_threshold;
                    self.cps.retain(|c| c.err <= t);
                    self.needs_align = true;
                    self.pe.selected = None;
                    let removed = before - self.cps.len();
                    self.set_status(format!("Removed {removed} points. Re-align in step 3 to update."), MUTED);
                }
            }
        });
        let hint = if self.pe.adding {
            match self.pe.pending {
                Some((Pane::A, _)) => "Now click the same spot in the RIGHT photo – it snaps to the exact spot.",
                Some((Pane::B, _)) => "Now click the same spot in the LEFT photo – it snaps to the exact spot.",
                None => "Click a distinctive spot (a corner, a sharp detail) in either photo. Esc cancels.",
            }
        } else {
            "Scroll to zoom, drag the photo to move it, double-click to fit. Click a point to select it: red = match window, dashed white = search area. Drag it to move it (edges are shown, it snaps to the exact spot), Delete removes it."
        };
        if self.click.on {
            self.click_panel(ui);
        } else {
            ui.label(RichText::new(hint).small().color(if self.pe.adding { ACCENT } else { MUTED }));
        }

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
        let frame = self.selection_frame();
        let mut new_a = a;
        let mut new_b = b;
        ui.horizontal_top(|ui| {
            for pane in [Pane::A, Pane::B] {
                let idx = if pane == Pane::A { a } else { b };
                ui.allocate_ui_with_layout(Vec2::new(pane_w, header_h + pane_h + 8.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_max_width(pane_w);
                    // header with the photo choice
                    ui.horizontal(|ui| {
                        if pane == Pane::A {
                            let total = |i: usize| self.cps.iter().filter(|c| c.a == i || c.b == i).count();
                            egui::ComboBox::from_id_salt("left_photo")
                                .width((pane_w - 190.0).max(120.0))
                                .truncate()
                                .selected_text(format!("{}. {}  —  {} points", a + 1, self.photos[a].name, total(a)))
                                .show_ui(ui, |ui| {
                                    for i in 0..n {
                                        ui.selectable_value(&mut new_a, i, format!("{}. {}  —  {} points", i + 1, self.photos[i].name, total(i)));
                                    }
                                });
                        } else {
                            let label = |j: usize| -> RichText {
                                let (c, mean, mx) = self.pair_stats(a, j);
                                let t = if c == 0 {
                                    format!("{}. {}  —  no points", j + 1, self.photos[j].name)
                                } else if self.aligned {
                                    format!("{}. {}  —  {} points · ⌀ {:.1} px · max {:.1} px", j + 1, self.photos[j].name, c, mean, mx)
                                } else {
                                    format!("{}. {}  —  {} points", j + 1, self.photos[j].name, c)
                                };
                                let col = if c == 0 { MUTED } else if self.aligned { err_color(mx) } else { Color32::WHITE };
                                RichText::new(t).color(col)
                            };
                            egui::ComboBox::from_id_salt("right_photo").width((pane_w - 190.0).max(120.0)).truncate().selected_text(label(b)).show_ui(ui, |ui| {
                                ui.set_min_width(380.0);
                                for j in 0..n {
                                    if j != a {
                                        ui.selectable_value(&mut new_b, j, label(j));
                                    }
                                }
                            });
                        }
                        let v = self.pe.views[pane.i()];
                        if ui.small_button("⤢ Fit").clicked() {
                            events.push(PaneEvent::Fit(pane));
                        }
                        ui.label(RichText::new(format!("{:.0} %", v.zoom.max(0.01) * 100.0)).small().color(MUTED));
                    });
                    let (resp, painter) = ui.allocate_painter(Vec2::new(pane_w, pane_h), Sense::click_and_drag());
                    let rect = resp.rect;
                    let painter = painter.with_clip_rect(rect);
                    let (w, h) = self.work_dims(idx);
                    let key = idx + 1;
                    let fit = (rect.width() / w as f32).min(rect.height() / h as f32);
                    {
                        let v = &mut self.pe.views[pane.i()];
                        if v.key != key {
                            *v = PaneView { zoom: 1.0, center: Vec2::new(w as f32 / 2.0, h as f32 / 2.0), key };
                        }
                    }
                    let v = self.pe.views[pane.i()];
                    let scale = fit * v.zoom;
                    let origin = rect.center() - v.center * scale;
                    geo[pane.i()] = PaneGeo { rect, origin, scale, fit, w: w as f32, h: h as f32 };
                    painter.rect_filled(rect, 8.0, BG);
                    let img_rect = Rect::from_min_size(origin, Vec2::new(w as f32 * scale, h as f32 * scale));
                    let tex_id = self.full_tex.get(&idx).map(|t| t.id()).or_else(|| self.photos[idx].tex.as_ref().map(|t| t.id()));
                    if let Some(t) = tex_id {
                        painter.image(t, img_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    }
                    // edges around the point while it is being moved
                    if let (Some((dk, _)), Some(fr)) = (self.pe.dragging, frame) {
                        if let Some((_, pa, pb)) = pts.iter().find(|t| t.0 == dk) {
                            let p = if pane == Pane::A { *pa } else { *pb };
                            let (sf, sm) = guided::step_sizes(&fr.j);
                            let r = (self.tpl_size / 2) as f64;
                            let half = if pane == fr.fixed {
                                2.0 * r * sf
                            } else {
                                self.local_radius(idx) + r * sm
                            }
                            .clamp(16.0, 220.0)
                                + 6.0;
                            let (img, x0, y0, ew, eh) = edge_image(&self.photos[idx].gray, p, half);
                            let tid = match &mut self.edge_tex[pane.i()] {
                                Some(t) => {
                                    t.set(img, egui::TextureOptions::LINEAR);
                                    t.id()
                                }
                                None => {
                                    let t = ui.ctx().load_texture(format!("edges-{}", pane.i()), img, egui::TextureOptions::LINEAR);
                                    let id = t.id();
                                    self.edge_tex[pane.i()] = Some(t);
                                    id
                                }
                            };
                            let rmin = origin + Vec2::new(x0 as f32 * scale, y0 as f32 * scale);
                            painter.rect_filled(Rect::from_min_size(rmin, Vec2::new(ew as f32 * scale, eh as f32 * scale)), 0.0, Color32::from_black_alpha(70));
                            painter.image(tid, Rect::from_min_size(rmin, Vec2::new(ew as f32 * scale, eh as f32 * scale)), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                        }
                    }
                    // points
                    let to_screen = |p: P2| origin + Vec2::new((p.0 as f32 + 0.5) * scale, (p.1 as f32 + 0.5) * scale);
                    for (num, (k, pa, pb)) in pts.iter().enumerate() {
                        let p = if pane == Pane::A { *pa } else { *pb };
                        let s = to_screen(p);
                        if !rect.expand(10.0).contains(s) {
                            continue;
                        }
                        let c = &self.cps[*k];
                        let col = if self.aligned { err_color(c.err) } else { point_color(num) };
                        let sel = self.pe.selected == Some(*k);
                        if sel {
                            let r = 11.0;
                            painter.circle_stroke(s, r, Stroke::new(3.0, Color32::BLACK));
                            painter.circle_stroke(s, r, Stroke::new(1.6, Color32::WHITE));
                            for d in [Vec2::new(1.0, 0.0), Vec2::new(0.0, 1.0)] {
                                painter.line_segment([s - d * (r + 6.0), s - d * 3.0], Stroke::new(1.4, Color32::WHITE));
                                painter.line_segment([s + d * 3.0, s + d * (r + 6.0)], Stroke::new(1.4, Color32::WHITE));
                            }
                            let info = if self.aligned { format!("#{} · {:.2} px", num + 1, c.err) } else { format!("#{}", num + 1) };
                            painter.text(s + Vec2::new(14.0, -14.0), Align2::LEFT_BOTTOM, info, FontId::proportional(12.5), Color32::WHITE);
                        } else {
                            painter.circle_stroke(s, 6.0, Stroke::new(1.5, Color32::BLACK));
                            if c.kind == CpKind::Point {
                                painter.circle_stroke(s, 5.0, Stroke::new(if c.manual { 2.4 } else { 1.8 }, col));
                            } else {
                                // control area: small square
                                painter.rect_stroke(Rect::from_center_size(s, Vec2::splat(8.0)), 0.0, Stroke::new(1.4, col), StrokeKind::Middle);
                            }
                            if v.zoom > 2.5 {
                                painter.text(s + Vec2::new(7.0, -7.0), Align2::LEFT_BOTTOM, format!("{}", num + 1), FontId::proportional(11.0), col);
                            }
                        }
                    }
                    // matching window and search area of the selected point
                    if let (Some(fr), Some(k)) = (frame, self.pe.selected) {
                        if let Some((_, pa, pb)) = pts.iter().find(|t| t.0 == k) {
                            let p = if pane == Pane::A { *pa } else { *pb };
                            let (sf, sm) = guided::step_sizes(&fr.j);
                            // half size = TemplateSize / 2 (integer), as the original draws it (sub_14006cfe0)
                            let r = (self.tpl_size / 2) as f64;
                            let yellow = TEMPLATE_FRAME_COLOR;
                            let blue = SEARCH_FRAME_COLOR;
                            let quad = |m: &dyn Fn(f64, f64) -> P2| -> Vec<Pos2> {
                                [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0), (-1.0, -1.0)].iter().map(|&(x, y)| to_screen(m(x, y))).collect()
                            };
                            let window = if pane == fr.fixed {
                                let h = r * sf;
                                quad(&|x, y| (p.0 + x * h, p.1 + y * h))
                            } else {
                                // the same patch as it appears in this photo (zoomed / rotated)
                                let h = r * sf;
                                let j = fr.j;
                                quad(&|x, y| (p.0 + (j[0] * x + j[1] * y) * h, p.1 + (j[2] * x + j[3] * y) * h))
                            };
                            painter.line(window.clone(), Stroke::new(3.2, Color32::from_black_alpha(150)));
                            painter.line(window.clone(), Stroke::new(1.6, yellow));
                            let label_at = window.iter().fold(Pos2::new(f32::MAX, f32::MIN), |m, q| Pos2::new(m.x.min(q.x), m.y.max(q.y)));
                            if pane != fr.fixed {
                                let sr = if self.pe.dragging.is_some() {
                                    self.local_radius(idx) + r * sm
                                } else {
                                    // original: half = SearchAreaPercent * image width / 200 (sub_140072d30)
                                    (self.search_pct * self.photos[idx].w as f64 / 200.0).min(guided::SEARCH_AREA_MAX_PX / 2.0) * self.work_per_full(idx)
                                };
                                let outer = quad(&|x, y| (p.0 + x * sr, p.1 + y * sr));
                                painter.extend(egui::Shape::dashed_line(&outer, Stroke::new(3.0, Color32::from_black_alpha(130)), 7.0, 5.0));
                                painter.extend(egui::Shape::dashed_line(&outer, Stroke::new(1.4, blue), 7.0, 5.0));
                                if (outer[2].x - outer[0].x) > 70.0 {
                                    painter.text(outer[0] + Vec2::new(3.0, -2.0), Align2::LEFT_BOTTOM, "search area", FontId::proportional(11.0), blue);
                                }
                            }
                            if (window[2].x - window[0].x).abs() > 50.0 || (window[2].y - window[0].y).abs() > 50.0 {
                                painter.text(label_at + Vec2::new(0.0, 3.0), Align2::LEFT_TOP, "match window", FontId::proportional(11.0), yellow);
                            }
                        }
                    }
                    if self.click.on {
                        // patch click: rough clicks (orange), refined spot and suggestions
                        let orange = Color32::from_rgb(255, 150, 40);
                        let click = if pane == Pane::A { self.click.a } else { self.click.b };
                        if let Some(c) = click {
                            let s = to_screen(c);
                            painter.circle_stroke(s, 10.0, Stroke::new(2.5, orange));
                            painter.line_segment([s - Vec2::new(14.0, 0.0), s + Vec2::new(14.0, 0.0)], Stroke::new(1.0, orange));
                            painter.line_segment([s - Vec2::new(0.0, 14.0), s + Vec2::new(0.0, 14.0)], Stroke::new(1.0, orange));
                        }
                        if let Some(r) = &self.click.result {
                            if r.a == a && r.b == b {
                                for sg in r.main.iter().chain(r.more.iter()) {
                                    let q = if pane == Pane::A { sg.pa } else { sg.pb };
                                    let col = if sg.accept { GOOD } else { Color32::from_gray(150) };
                                    painter.circle_stroke(to_screen(q), if sg.main { 7.0 } else { 4.5 }, Stroke::new(if sg.main { 2.5 } else { 1.5 }, col));
                                }
                            }
                        }
                    }
                    if let Some((pp, pn)) = self.pe.pending {
                        if pp == pane {
                            let s = to_screen(self.to_px(idx, pn));
                            painter.circle_stroke(s, 9.0, Stroke::new(2.5, ACCENT));
                            painter.circle_filled(s, 2.0, ACCENT);
                        }
                    }
                    painter.text(rect.left_top() + Vec2::new(10.0, 8.0), Align2::LEFT_TOP, format!("Photo {}", idx + 1), FontId::proportional(14.0), Color32::WHITE);
                    // loupe while placing points
                    if self.pe.adding && v.zoom < 3.0 {
                        if let (Some(hp), Some(t)) = (resp.hover_pos(), tex_id) {
                            if img_rect.contains(hp) {
                                let lr = Rect::from_min_size(rect.right_bottom() - Vec2::new(170.0, 170.0), Vec2::splat(160.0));
                                let u = (hp - img_rect.min) / img_rect.size();
                                // magnifier shows MAGNIFIER_WIDTH pixels of the full-size photo
                                let full_w = self.photos[idx].w as f32;
                                let full_h = self.photos[idx].h as f32;
                                let hx = 0.5 * MAGNIFIER_WIDTH / full_w;
                                let hy = 0.5 * MAGNIFIER_WIDTH / full_h;
                                painter.rect_filled(lr.expand(3.0), 6.0, Color32::BLACK);
                                painter.image(t, lr, Rect::from_min_max(Pos2::new(u.x - hx, u.y - hy), Pos2::new(u.x + hx, u.y + hy)), Color32::WHITE);
                                painter.line_segment([lr.center() - Vec2::new(10.0, 0.0), lr.center() + Vec2::new(10.0, 0.0)], Stroke::new(1.0, Color32::from_rgb(255, 80, 80)));
                                painter.line_segment([lr.center() - Vec2::new(0.0, 10.0), lr.center() + Vec2::new(0.0, 10.0)], Stroke::new(1.0, Color32::from_rgb(255, 80, 80)));
                            }
                        }
                    }
                    // input
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(if self.pe.adding { egui::CursorIcon::Crosshair } else { egui::CursorIcon::Default });
                        let sc = ui.input(|i| i.smooth_scroll_delta.y);
                        if sc.abs() > 0.1 {
                            if let Some(hp) = resp.hover_pos() {
                                events.push(PaneEvent::Zoom(pane, hp, (sc * 0.0025).exp()));
                            }
                        }
                    }
                    if resp.double_clicked() {
                        events.push(PaneEvent::Fit(pane));
                    } else if resp.clicked() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            events.push(PaneEvent::Click(pane, p));
                        }
                    }
                    if resp.drag_started() {
                        let primary = resp.drag_started_by(egui::PointerButton::Primary);
                        if let Some(p) = resp.interact_pointer_pos() {
                            events.push(PaneEvent::DragStart(pane, p, primary));
                        }
                    }
                    if resp.dragged() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            events.push(PaneEvent::Drag(pane, p, resp.drag_delta()));
                        }
                    }
                    if resp.drag_stopped() {
                        events.push(PaneEvent::DragEnd);
                    }
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
            self.pe.pending = None;
            return;
        }
        self.handle_pane_events(events, &geo, &pts);
        if ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)) {
            self.delete_selected();
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.pe.pending = None;
        }
        if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Z)) && !ui.ctx().egui_wants_keyboard_input() {
            self.undo_last();
        }
    }

    /// Strip under the toolbar while the patch click mode is on.
    pub(super) fn click_panel(&mut self, ui: &mut egui::Ui) {
        let (a, b) = (self.pe.a, self.pe.b);
        let state = match (self.click.a, self.click.b) {
            (None, None) => "Patch click: click roughly the same spot in the LEFT and the RIGHT photo (a corner or a sharp detail).",
            (Some(_), None) => "Now click roughly the same spot in the RIGHT photo.",
            (None, Some(_)) => "Now click roughly the same spot in the LEFT photo.",
            _ => "",
        };
        if !state.is_empty() {
            ui.label(RichText::new(state).small().color(ACCENT));
        }
        let Some(r) = self.click.result.clone() else { return };
        if r.a != a || r.b != b {
            return;
        }
        egui::Frame::new().fill(CARD).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
            if let Some(e) = &r.error {
                ui.label(RichText::new(e).color(BAD));
                return;
            }
            ui.horizontal_wrapped(|ui| {
                if let Some(m) = &r.main {
                    ui.label(RichText::new(format!("Refined: score {:.3}, confidence {:.2}", m.score, m.conf)).strong());
                }
                ui.label(RichText::new(format!("local zoom ×{:.3}, rotation {:.1}° – {}", r.zoom(), r.rotation_deg(), r.j_source)).small().color(MUTED));
            });
            let n_on = r.main.iter().chain(r.more.iter()).filter(|s| s.accept).count();
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new(format!("✔ Accept {n_on} checked")).strong())).clicked() {
                    self.accept_click(false);
                }
                if ui.button("Accept all").clicked() {
                    self.accept_click(true);
                }
                if ui.button("✖ Discard").clicked() {
                    self.discard_click();
                }
                ui.label(RichText::new(format!("{} more pairs around the click (radius {:.0} px)", r.more.len(), self.click_cfg.more_radius)).small().color(MUTED));
            });
            let mut toggles: Vec<(usize, bool)> = Vec::new();
            egui::ScrollArea::vertical().id_salt("click_sugg").max_height(96.0).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (k, sg) in r.main.iter().chain(r.more.iter()).enumerate() {
                        let mut on = sg.accept;
                        let label = if sg.main { format!("clicked point · {:.2}", sg.score) } else { format!("#{k} · {:.2}", sg.score) };
                        if ui.checkbox(&mut on, label).on_hover_text(format!("score {:.3}, confidence {:.3}", sg.score, sg.conf)).changed() {
                            toggles.push((k, on));
                        }
                    }
                });
            });
            if let Some(res) = &mut self.click.result {
                for (k, on) in toggles {
                    if k == 0 && res.main.is_some() {
                        if let Some(m) = &mut res.main {
                            m.accept = on;
                        }
                    } else {
                        let i = if res.main.is_some() { k - 1 } else { k };
                        if let Some(s) = res.more.get_mut(i) {
                            s.accept = on;
                        }
                    }
                }
            }
        });
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

    pub(super) fn delete_selected(&mut self) {
        if let Some(k) = self.pe.selected.take() {
            if k < self.cps.len() {
                self.cps.remove(k);
                self.needs_align = true;
            }
        }
    }

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
        if let Some(f) = self.sel_frame {
            if f.key == key || (self.pe.dragging.is_some() && f.key.0 == k && f.fixed == fixed) {
                return Some(f);
            }
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
        // the click can be a few pixels off: look around it
        let mut best = ([s, 0.0, 0.0, s], -2.0f32);
        for (dx, dy) in [(0.0, 0.0), (-4.0, 0.0), (4.0, 0.0), (0.0, -4.0), (0.0, 4.0)] {
            let r = guided::best_rotation(&c, pf, s, (guess.0 + dx, guess.1 + dy));
            if r.1 > best.1 {
                best = r;
            }
        }
        best.0
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

    /// Find points for the current pair only (automatic, or growing from existing points).
    pub(super) fn run_pair_points(&mut self) {
        if self.busy() {
            self.set_status("Please wait until the current task has finished.", WARN);
            return;
        }
        let (a, b) = (self.pe.a, self.pe.b);
        let seeds = self.pair_seeds(a, b);
        let ga = self.photos[a].gray.clone();
        let gb = self.photos[b].gray.clone();
        let sky = self.match_settings.ignore_sky;
        let ra = sky.then(|| self.photos[a].work.clone());
        let rb = sky.then(|| self.photos[b].work.clone());
        let prior = self.scale_prior(a, b);
        let max = self.match_settings.points_per_pair;
        let (wa, ha) = self.work_dims(a);
        let (wb, hb) = self.work_dims(b);
        let mode = if seeds.is_empty() { "automatically" } else { "from your points" };
        self.start_job(&format!("Finding points between photo {} and {}", a + 1, b + 1), move |report, _| {
            let t0 = Instant::now();
            report(0.2, "Searching this photo pair");
            let found = if seeds.is_empty() {
                guided::auto_pair(&ga, &gb, ra.as_deref(), rb.as_deref(), prior, max)
            } else {
                guided::grow(&ga, &gb, ra.as_deref(), rb.as_deref(), &seeds, prior, max)
            };
            let cps = found
                .into_iter()
                .map(|(p, q, _)| ControlPoint {
                    a,
                    b,
                    pa: engine::norm_coord(p.0 as f32, p.1 as f32, wa, ha),
                    pb: engine::norm_coord(q.0 as f32, q.1 as f32, wb, hb),
                    ..ControlPoint::new(0, 0, [0.0; 2], [0.0; 2], false)
                })
                .collect();
            JobMsg::PairPoints { a, b, cps, ms: t0.elapsed().as_millis(), mode }
        });
    }

    pub(super) fn on_pair_points(&mut self, a: usize, b: usize, cps: Vec<ControlPoint>, ms: u128, mode: &str) {
        if a >= self.photos.len() || b >= self.photos.len() {
            return;
        }
        if cps.len() < 4 {
            self.set_status(
                format!(
                    "No reliable points found {mode} between photo {} and {} ({}). Add a point by hand on a clear detail both photos share.",
                    a + 1,
                    b + 1,
                    fmt_ms(ms)
                ),
                WARN,
            );
            return;
        }
        let sel = self.pe.selected.and_then(|k| self.cps.get(k).copied());
        self.cps.retain(|c| c.manual || !((c.a == a && c.b == b) || (c.a == b && c.b == a)));
        let n = cps.len();
        self.cps.extend(cps);
        self.matched = true;
        self.needs_align = true;
        self.pe.selected = sel.and_then(|s| self.cps.iter().position(|c| c.a == s.a && c.b == s.b && c.pa == s.pa && c.pb == s.pb));
        self.set_status(
            format!("Found {n} points {mode} between photo {} and {} in {}. Align again (step 3) to use them.", a + 1, b + 1, fmt_ms(ms)),
            GOOD,
        );
    }

    pub(super) fn request_full(&mut self, i: usize) {
        if self.full_tex.contains_key(&i) || self.full_pending.contains(&i) {
            return;
        }
        let long = self.photos[i].w.max(self.photos[i].h);
        if long <= self.photos[i].work.width().max(self.photos[i].work.height()) {
            return; // the working copy is already full size
        }
        self.full_pending.insert(i);
        let src = self.photos[i].source.clone();
        let tx = self.full_tx.clone();
        let name = self.photos[i].name.clone();
        std::thread::spawn(move || {
            if let Ok(img) = src.load_full() {
                // the two full-size photos shown in the point editor share the image-cache bound
                let max_px = (IMAGE_CACHE_UPPER_BOUND / 2 / 4) as f64;
                let px = img.width() as f64 * img.height() as f64;
                let long = img.width().max(img.height()) as f64;
                let side = if px > max_px { (long * (max_px / px).sqrt()) as u32 } else { long as u32 };
                let img = imgutil::downscale_rgb(&img, side.min(8192));
                let _ = tx.send((i, name, color_image(&img)));
            }
        });
    }
}
