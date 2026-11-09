use super::*;

impl PanoApp {
    pub(super) fn run_align(&mut self, chain: bool) {
        if self.busy() || self.photos.is_empty() {
            return;
        }
        let active = self.active_indices();
        // the original refuses to optimise without 2 active images or without control points
        if active.len() < 2 {
            self.auto_chain = false;
            self.set_status("The project does not contain enough active photos. Tick “use” for at least 2 photos. Alignment canceled.", BAD);
            return;
        }
        if !self.cps.iter().any(|c| active.contains(&c.a) && active.contains(&c.b)) {
            self.auto_chain = false;
            self.set_status("There are no control points between the active photos. Find or add points first (step 2). Alignment canceled.", BAD);
            return;
        }
        self.auto_chain = chain;
        let lens_of: Vec<usize> = self.photos.iter().map(|p| p.lens).collect();
        let long: Vec<f64> = self.photos.iter().map(|p| p.long()).collect();
        let lenses = self.lenses.clone();
        let cps: Vec<ControlPoint> = self.cps.iter().filter(|c| active.contains(&c.a) && active.contains(&c.b)).copied().collect();
        let s = AlignSettings { ..self.align_settings };
        let s = AlignSettings { fine_tune_fov: s.fine_tune_fov, distortion: s.distortion, remove_outliers: s.remove_outliers, level: s.level };
        let fine_cfg = self.fine_cfg.clone();
        let par_cfg = self.parallax_cfg.clone();
        let fa: Vec<fine_align::FaPhoto> = self.photos.iter().map(|p| fine_align::FaPhoto { gray: p.gray.clone(), rgb: p.work.clone() }).collect();
        let pc_cfg = self.pair_cfg.clone();
        let msettings = self.match_settings;
        let used: Vec<bool> = (0..self.photos.len()).map(|i| active.contains(&i)).collect();
        self.start_job("Aligning photos", move |report, _| {
            let (focal, dist) = (s.fine_tune_fov, s.distortion);
            // foliage (moves between the shots) gets a smaller weight: first from colour and structure and the
            // errors of the last alignment, then again from the errors of this one
            let fol_v = if fine_cfg.foliage.enabled { fine_align::foliage_maps(&fa) } else { Vec::new() };
            let fol: Vec<Option<&crate::foliage::FoliageMap>> = (0..fa.len()).map(|i| fol_v.get(i)).collect();
            let mut cps = cps;
            crate::foliage::rate(&mut cps, &fol, &fine_cfg.foliage);
            let mut r0 = engine::align_all(&lens_of, &lenses, &long, &cps, &s, report);
            crate::foliage::rate(&mut r0.cps, &fol, &fine_cfg.foliage);
            // geometric pair check: overlapping pairs without enough points are searched again
            let (mut r, summary) = if pc_cfg.enabled {
                crate::pair_check::run_rounds(&fa, &lens_of, &lenses, &long, r0, &s, &used, &pc_cfg, &fine_cfg.matcher, msettings, report)
            } else {
                let sm = crate::pair_check::summary_only(&fa, &lens_of, &r0.lenses, &r0.rots, &r0.cps, &used, &pc_cfg);
                (r0, sm)
            };
            for l in &summary.log {
                eprintln!("pair check: {l}");
            }
            r.pairs = Some(summary);
            crate::foliage::rate(&mut r.cps, &fol, &fine_cfg.foliage);
            if fine_cfg.enabled {
                let (rots, lenses2, cps2, st) = fine_align::run(&fa, &lens_of, &long, &r.lenses, &r.rots, &r.cps, focal, dist, &fine_cfg, report);
                r.rots = rots;
                r.lenses = lenses2;
                r.cps = cps2;
                let errs: Vec<f64> = r.cps.iter().filter(|c| c.kind == CpKind::Point).map(|c| c.err).collect();
                if !errs.is_empty() {
                    r.mean_err = errs.iter().sum::<f64>() / errs.len() as f64;
                    r.max_err = errs.iter().cloned().fold(0.0, f64::max);
                }
                r.fine = Some(st);
                if par_cfg.enabled {
                    report(0.97, "Parallax mode: measuring local corrections");
                    r.fields = fine_align::parallax_fields(&fa, &lens_of, &r.lenses, &r.rots, &r.cps, &fine_cfg, &par_cfg);
                }
            }
            JobMsg::Aligned(r)
        });
    }

    /// "Restart optimisation" from the patch comparison window: optimise with the patches of
    /// the list as they are now (disabled ones left out, shifted ones with their shift).
    pub(super) fn run_restart(&mut self) {
        if self.busy() {
            return;
        }
        if !self.aligned {
            self.set_status("Align the photos first – the patch list belongs to an alignment.", WARN);
            return;
        }
        let active = self.active_indices();
        if active.len() < 2 {
            self.set_status("The project does not contain enough active photos. Tick “use” for at least 2 photos. Optimization canceled.", BAD);
            return;
        }
        let lens_of: Vec<usize> = self.photos.iter().map(|p| p.lens).collect();
        let long: Vec<f64> = self.photos.iter().map(|p| p.long()).collect();
        let lenses = self.lenses.clone();
        let rots: Vec<M3> = self.photos.iter().map(|p| p.rot).collect();
        let cps: Vec<ControlPoint> = self.cps.iter().filter(|c| active.contains(&c.a) && active.contains(&c.b)).copied().collect();
        let (focal, dist) = (self.align_settings.fine_tune_fov, self.align_settings.distortion);
        let fine_cfg = self.fine_cfg.clone();
        let par_cfg = self.parallax_cfg.clone();
        let old_fields = self.fields.clone();
        let fa: Vec<fine_align::FaPhoto> = self.photos.iter().map(|p| fine_align::FaPhoto { gray: p.gray.clone(), rgb: p.work.clone() }).collect();
        self.start_job("Restarting the optimisation", move |report, _| {
            let t0 = Instant::now();
            let (rots, lenses2, cps2, st) = fine_align::reoptimize(&fa, &lens_of, &long, &lenses, &rots, &cps, focal, dist, &fine_cfg, report);
            let errs: Vec<f64> = cps2.iter().filter(|c| c.kind == CpKind::Point).map(|c| c.err).collect();
            let (mean_err, max_err) = if errs.is_empty() { (0.0, 0.0) } else { (errs.iter().sum::<f64>() / errs.len() as f64, errs.iter().cloned().fold(0.0, f64::max)) };
            let fields = if par_cfg.enabled { fine_align::parallax_fields(&fa, &lens_of, &lenses2, &rots, &cps2, &fine_cfg, &par_cfg) } else { old_fields };
            JobMsg::Aligned(AlignResult { rots, lenses: lenses2, cps: cps2, removed: 0, ms: t0.elapsed().as_millis(), mean_err, max_err, fine: Some(st), fields, pairs: None })
        });
    }

    pub(super) fn on_aligned(&mut self, a: AlignResult) {
        if let Some(p) = &a.pairs {
            self.pair_summary = Some(p.clone());
        }
        let pair_note = a.pairs.as_ref().filter(|p| p.ran).map(|p| {
            let added: usize = p.reports.iter().map(|r| r.added).sum();
            let weak = p.reports.iter().filter(|r| r.weak).count();
            format!(
                " Pair check: {} overlapping pairs, +{added} points in {} pairs ({} → {} connected pairs){}.",
                p.overlaps.len(),
                p.reports.iter().filter(|r| r.added > 0).count(),
                p.pairs_before,
                p.pairs_after,
                if weak > 0 { format!(", {weak} still weak – see “Connections”") } else { String::new() }
            )
        });
        self.last_fine = a.fine.clone();
        self.fields = a.fields.clone();
        self.fields.resize(self.photos.len(), None);
        self.align_version += 1;
        self.ov_center = None;
        let active = self.active_indices();
        for (i, p) in self.photos.iter_mut().enumerate() {
            if active.contains(&i) {
                p.rot = a.rots[i];
            }
        }
        self.lenses = a.lenses;
        // keep points of disabled photos, replace the rest
        let mut cps: Vec<ControlPoint> = self.cps.iter().filter(|c| !(active.contains(&c.a) && active.contains(&c.b))).copied().collect();
        cps.extend(a.cps);
        // outliers the alignment dropped can be brought back; changed errors are no step
        let removed = self.cps.len().saturating_sub(cps.len());
        if removed > 0 {
            self.push_undo(&format!("alignment removing {removed} outlier points"));
        }
        self.cps = cps;
        self.aligned = true;
        self.needs_align = false;
        self.last_align = Some((a.mean_err, a.max_err, a.removed, a.ms));
        let was_chain = self.auto_chain;
        if was_chain || self.view.bounds == PanoView::default().bounds {
            self.choose_projection();
        } else {
            self.fit_view();
        }
        self.update_gains();
        if was_chain {
            self.auto_crop();
            self.step = Step::Frame;
        }
        self.auto_chain = false;
        let groups = image_groups(self.photos.len(), &active, &self.cps);
        self.preview_dirty = true;
        let q = if a.mean_err < 1.5 { "excellent" } else if a.mean_err < 3.0 { "good" } else { "rough – check the points" };
        let fine_note = a.fine.as_ref().map(fine_attention).unwrap_or_default();
        self.set_status(
            format!(
                "Aligned in {}: average error {:.2} px ({q}){}.{}{fine_note}",
                fmt_ms(a.ms),
                a.mean_err,
                if a.removed > 0 { format!(", {} bad points removed automatically", a.removed) } else { String::new() },
                pair_note.unwrap_or_default()
            ),
            if a.mean_err < 3.0 && fine_note.is_empty() { GOOD } else { WARN },
        );
        if groups.len() >= 2 && active.len() > 1 {
            // like the original after the assistant: warn with the list
            // of groups and open the point editor with the first photo of the first two groups
            self.set_status(
                format!(
                    "Warning {} unconnected image groups found: {}. Please create control points between unconnected images in step 2, then press “Align photos” again.{fine_note}",
                    groups.len(),
                    groups_text(&groups)
                ),
                WARN,
            );
            self.pe.a = groups[0][0];
            self.pe.b = groups[1][0];
            self.pe.selected = None;
            self.pe.pending = None;
            if was_chain {
                self.step = Step::Points;
            }
        }
    }

    // ----- step 3 -----
    pub(super) fn ui_align(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if primary_button(ui, "📐  Align photos", !self.busy() && !self.cps.is_empty()).clicked() {
                self.run_align(false);
            }
            if ui
                .selectable_label(self.patch_win.open, "🔬  Patch comparison")
                .on_hover_text("Opens a separate window that shows every patch comparison of the fine alignment – live while it runs and afterwards as a list.")
                .clicked()
            {
                self.patch_win.open = !self.patch_win.open;
            }
            // the mask editor is also in the export step (Blending); here it is easier to find
            let masks = ui
                .add_enabled(!self.photos.is_empty(), egui::Button::selectable(self.mk.open, "🖌  Masks"))
                .on_hover_text("Opens the mask editor: paint where a photo must (green) or must not (red) be used, for example to keep a pole or a person whole. Masks steer the seams of “Smooth seams”.");
            if masks.clicked() {
                self.mk.open = !self.mk.open;
            }
            if ui.add_enabled(self.aligned && !self.busy(), egui::Button::new("⟲  Level the horizon")).clicked() {
                let mut rots: Vec<M3> = self.photos.iter().map(|p| p.rot).collect();
                let used: Vec<bool> = self.photos.iter().map(|p| p.enabled).collect();
                crate::align::straighten(&mut rots, &used);
                for (p, r) in self.photos.iter_mut().zip(rots) {
                    p.rot = r;
                }
                self.fit_view();
                self.set_status("Horizon levelled.", GOOD);
            }
            if self.needs_align && self.aligned {
                ui.label(RichText::new("Something changed – align again to update.").color(WARN));
            }
        });
        ui.add_space(6.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            card(ui, |ui| {
                ui.label(RichText::new("Result").strong().size(16.0));
                if let Some((mean, max, removed, ms)) = self.last_align {
                    let (q, c) = if mean < 1.5 { ("Excellent", GOOD) } else if mean < 3.0 { ("Good", GOOD) } else if mean < 6.0 { ("Fair", WARN) } else { ("Poor", BAD) };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(q).size(26.0).strong().color(c));
                        ui.add_space(12.0);
                        ui.vertical(|ui| {
                            ui.label(format!("Average error {mean:.2} px · worst point {max:.1} px"));
                            ui.label(RichText::new(format!("{} points used · {removed} wrong matches removed automatically · {}", self.cps.len(), fmt_ms(ms))).small().color(MUTED));
                        });
                    });
                    if mean >= 3.0 {
                        ui.label(RichText::new("Tip: open step 2, look at the pairs marked red and delete wrong points, or use “Clean up”.").small().color(WARN));
                    }
                } else {
                    ui.label(RichText::new("Not aligned yet. Press “Align photos”.").color(MUTED));
                }
                let comps = crate::align::components(self.photos.len(), &self.cps);
                let main = most_common(&comps);
                let lonely: Vec<String> = (0..self.photos.len()).filter(|&i| self.photos[i].enabled && comps[i] != main).map(|i| (i + 1).to_string()).collect();
                if self.matched && !lonely.is_empty() {
                    ui.label(
                        RichText::new(format!(
                            "⚠ Photo{} {} {} not connected to the rest. Add points by hand in step 2, or untick “use” in the photo list.",
                            if lonely.len() > 1 { "s" } else { "" },
                            lonely.join(", "),
                            if lonely.len() > 1 { "are" } else { "is" }
                        ))
                        .color(WARN),
                    );
                }
            });
            ui.add_space(6.0);
            card(ui, |ui| {
                ui.label(RichText::new("Options").strong());
                ui.checkbox(&mut self.align_settings.fine_tune_fov, "Measure the lens field of view precisely");
                ui.checkbox(&mut self.align_settings.distortion, "Correct lens distortion (barrel / pincushion)");
                ui.checkbox(&mut self.align_settings.remove_outliers, "Remove wrong matches automatically");
                ui.checkbox(&mut self.align_settings.level, "Level the horizon automatically");
            });
            ui.add_space(6.0);
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Pair check (overlapping photos without points)").strong());
                    if ui.button("🕸 Connections").clicked() {
                        self.show_graph = !self.show_graph;
                    }
                });
                ui.checkbox(&mut self.pair_cfg.enabled, "After aligning, search overlapping pairs that have too few points again");
                ui.label(
                    RichText::new("Uses the estimated overlap of every pair: the point finder only inside the overlap, then a search predicted through a third photo, then patch matching. Pairs that still have too few points are reported, nothing is made up. Off = exactly as before.")
                        .small()
                        .color(MUTED),
                );
                ui.add_enabled_ui(self.pair_cfg.enabled, |ui| {
                    egui::Grid::new("pair_grid").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                        let c = &mut self.pair_cfg;
                        ui.label("Overlap from");
                        let mut pct = c.min_overlap * 100.0;
                        let (lo, hi) = crate::pair_check::MIN_OVERLAP_RANGE;
                        if ui.add(egui::DragValue::new(&mut pct).range(lo * 100.0..=hi * 100.0).speed(0.5).suffix(" %")).on_hover_text(format!("share of the smaller photo; {:.0} % = obviously overlapping", crate::pair_check::OBVIOUS_OVERLAP * 100.0)).changed() {
                            c.min_overlap = pct / 100.0;
                        }
                        ui.end_row();
                        ui.label("Too few below");
                        ui.add(egui::DragValue::new(&mut c.min_points).range(1..=100).suffix(" points"));
                        ui.end_row();
                        ui.label("Rounds at most");
                        ui.add(egui::DragValue::new(&mut c.max_rounds).range(1..=5));
                        ui.end_row();
                        ui.label("Steps");
                        ui.vertical(|ui| {
                            ui.checkbox(&mut c.restrict_search, "point finder only inside the overlap");
                            ui.checkbox(&mut c.predict_via_third, "predict through a third photo");
                            ui.checkbox(&mut c.patch_fallback, "patch matching in the overlap");
                        });
                        ui.end_row();
                    });
                });
            });
            ui.add_space(6.0);
            card(ui, |ui| {
                ui.label(RichText::new("Fine alignment (patch matching)").strong());
                ui.checkbox(&mut self.fine_cfg.enabled, "Compare image patches after aligning and refine the alignment (sharper seams)");
                ui.label(
                    RichText::new("Slides small patches of overlapping photos over each other to find the remaining shift with sub-pixel accuracy, refines the points, adds control areas along the seams and re-aligns in rounds. Off = exactly as before.")
                        .small()
                        .color(MUTED),
                );
                ui.checkbox(&mut self.fine_cfg.foliage.enabled, "Count points and patches on foliage less (trees and bushes move between the shots)")
                    .on_hover_text("Colour (green to brown), very fine irregular structure and restless residual errors after aligning mark foliage; those points and patches get a smaller weight (not removed), areas are placed away from foliage if possible. Protected points and points placed by hand are never touched. Off = exactly as before.");
                ui.add_enabled_ui(self.fine_cfg.enabled, |ui| {
                    ui.checkbox(&mut self.fine_cfg.refine_points, "Refine control points");
                    ui.checkbox(&mut self.fine_cfg.areas, "Add control areas in the overlaps (denser along the seams)");
                    ui.checkbox(&mut self.parallax_cfg.enabled, "Parallax mode: local correction near the seams (for close objects)");
                    egui::CollapsingHeader::new("Advanced settings").id_salt("fine_adv").show(ui, |ui| {
                        egui::Grid::new("fine_grid").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                            let m = &mut self.fine_cfg.matcher;
                            ui.label("Patch size");
                            ui.add(egui::DragValue::new(&mut m.patch_size).range(24..=256).suffix(" px"));
                            ui.end_row();
                            ui.label("Search radius");
                            ui.add(egui::DragValue::new(&mut m.search_radius).range(2.0..=64.0).speed(0.2).suffix(" px"));
                            ui.end_row();
                            ui.label("Measure");
                            egui::ComboBox::from_id_salt("metric").selected_text(m.metric.name()).show_ui(ui, |ui| {
                                for x in Metric::ALL {
                                    ui.selectable_value(&mut m.metric, x, x.name());
                                }
                            });
                            ui.end_row();
                            ui.label("Compare");
                            egui::ComboBox::from_id_salt("fspace").selected_text(m.feature_space.name()).show_ui(ui, |ui| {
                                for x in FeatureSpace::ALL {
                                    ui.selectable_value(&mut m.feature_space, x, x.name());
                                }
                            });
                            ui.end_row();
                            ui.label("Pyramid levels");
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut m.use_pyramid, "");
                                ui.add_enabled(m.use_pyramid, egui::DragValue::new(&mut m.pyramid_levels).range(1..=4));
                            });
                            ui.end_row();
                            ui.label("Min. similarity");
                            ui.add(egui::DragValue::new(&mut m.min_score).range(0.0..=1.0).speed(0.01));
                            ui.end_row();
                            ui.label("Min. uniqueness");
                            ui.add(egui::DragValue::new(&mut m.min_uniqueness).range(1.0..=3.0).speed(0.01));
                            ui.end_row();
                            ui.label("Min. texture");
                            let mut t = m.min_texture * 1e5;
                            if ui.add(egui::DragValue::new(&mut t).range(0.0..=100.0).speed(0.1).suffix(" ×1e-5")).changed() {
                                m.min_texture = t / 1e5;
                            }
                            ui.end_row();
                            ui.label("Forward-backward check");
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut m.forward_backward, "");
                                ui.add_enabled(m.forward_backward, egui::DragValue::new(&mut m.fb_tolerance).range(0.05..=5.0).speed(0.01).suffix(" px"));
                            });
                            ui.end_row();
                            ui.label("Valid pixels needed");
                            ui.add(egui::DragValue::new(&mut m.min_valid_fraction).range(0.3..=1.0).speed(0.01));
                            ui.end_row();
                            ui.label("Pre-blur (sigma)");
                            ui.add(egui::DragValue::new(&mut m.blur_sigma).range(0.0..=3.0).speed(0.01));
                            ui.end_row();
                            let f = &mut self.fine_cfg;
                            ui.label("Point refinement: template");
                            ui.add(egui::DragValue::new(&mut f.point_template).range(9..=101).suffix(" px"));
                            ui.end_row();
                            ui.label("Point refinement: search");
                            ui.add(egui::DragValue::new(&mut f.point_search).range(1.0..=50.0).speed(0.1).suffix(" px"));
                            ui.end_row();
                            ui.label("Point refinement: min. correlation");
                            ui.add(egui::DragValue::new(&mut f.point_min_score).range(0.0..=1.0).speed(0.01));
                            ui.end_row();
                            ui.label("Area grid cell");
                            ui.add(egui::DragValue::new(&mut f.grid_cell).range(16..=256).suffix(" px"));
                            ui.end_row();
                            ui.label("Max. areas per pair");
                            ui.add(egui::DragValue::new(&mut f.max_areas_per_pair).range(10..=2000));
                            ui.end_row();
                            ui.label("Dense band around the seam");
                            ui.add(egui::DragValue::new(&mut f.seam_band).range(0.0..=1000.0).suffix(" px"));
                            ui.end_row();
                            ui.label("Weight of hand-drawn areas");
                            ui.add(egui::DragValue::new(&mut f.manual_area_weight).range(0.1..=10.0).speed(0.05));
                            ui.end_row();
                            ui.label("Areas vs. points (balance)");
                            ui.add(egui::DragValue::new(&mut f.area_weight_scale).range(0.0..=10.0).speed(0.05));
                            ui.end_row();
                            ui.label("Robust loss");
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut f.robust_loss, RobustLoss::Huber, "Huber");
                                ui.radio_value(&mut f.robust_loss, RobustLoss::Cauchy, "Cauchy");
                            });
                            ui.end_row();
                            ui.label("Rounds");
                            ui.add(egui::DragValue::new(&mut f.refine_iterations).range(1..=10));
                            ui.end_row();
                            ui.label("Stop when shift below");
                            ui.add(egui::DragValue::new(&mut f.converge_threshold).range(0.01..=2.0).speed(0.01).suffix(" px"));
                            ui.end_row();
                            ui.label("Points that fail the check");
                            ui.checkbox(&mut f.drop_invalid_points, "remove (otherwise keep with low weight)");
                            ui.end_row();
                            ui.label("Control area placement");
                            ui.checkbox(&mut f.pick_textured, "Where the photo has structure")
                                .on_hover_text("On: each area sits at the spot with structure in two directions near its grid point, plain surfaces get none. Off: plain grid");
                            ui.end_row();
                            ui.label("Patch comparison window");
                            ui.checkbox(&mut f.record_patches, "Record patches for this window")
                                .on_hover_text("Off: faster and less memory, the alignment is the same, the window stays empty");
                            ui.end_row();
                            let p = &mut self.parallax_cfg;
                            ui.label("Parallax: largest correction");
                            ui.add(egui::DragValue::new(&mut p.max_shift).range(1.0..=30.0).speed(0.1).suffix(" px"));
                            ui.end_row();
                            ui.label("Parallax: smoothing");
                            ui.add(egui::DragValue::new(&mut p.smooth_cells).range(0.5..=5.0).speed(0.05).suffix(" cells"));
                            ui.end_row();
                        });
                        if ui.button("Reset to defaults").clicked() {
                            self.fine_cfg = crate::fine_align::FineConfig::default();
                            self.parallax_cfg = crate::fine_align::ParallaxConfig::default();
                        }
                    });
                });
                if let Some(st) = &self.last_fine {
                    ui.separator();
                    ui.label(fine_summary(st));
                    if st.rolled_back {
                        ui.label(RichText::new("The last round was undone because it made the result worse.").color(WARN));
                    }
                    if st.cells_skipped > 0 {
                        ui.label(format!("{} grid cells without structure were left out", st.cells_skipped));
                    }
                    egui::CollapsingHeader::new(format!("Why {} patches were not used", st.patches_total - st.patches_valid)).id_salt("fine_reasons").show(ui, |ui| {
                        for (k, v) in &st.reasons {
                            if k != "ok" {
                                ui.label(format!("{v} × {k}"));
                            }
                        }
                    });
                    egui::CollapsingHeader::new("Log").id_salt("fine_log").show(ui, |ui| {
                        for l in &st.log {
                            ui.label(RichText::new(l).small().monospace());
                        }
                    });
                }
            });
            ui.add_space(6.0);
            if self.aligned {
                card(ui, |ui| {
                    ui.label(RichText::new("Camera directions").strong());
                    egui::Grid::new("dirs").striped(true).spacing(Vec2::new(18.0, 4.0)).show(ui, |ui| {
                        for h in ["Photo", "Left/right", "Up/down", "Tilt", "Points", "Avg. error"] {
                            ui.label(RichText::new(h).color(MUTED));
                        }
                        ui.end_row();
                        for (i, p) in self.photos.iter().enumerate() {
                            let (y, pi, r) = geometry::ypr_of(&p.rot);
                            let mine: Vec<&ControlPoint> = self.cps.iter().filter(|c| c.a == i || c.b == i).collect();
                            let avg = if mine.is_empty() { 0.0 } else { mine.iter().map(|c| c.err).sum::<f64>() / mine.len() as f64 };
                            ui.label(format!("{}. {}", i + 1, p.name));
                            ui.label(format!("{:+.1}°", y.to_degrees()));
                            ui.label(format!("{:+.1}°", pi.to_degrees()));
                            ui.label(format!("{:+.1}°", r.to_degrees()));
                            ui.label(format!("{}", mine.len()));
                            ui.label(RichText::new(format!("{avg:.2} px")).color(err_color(avg)));
                            ui.end_row();
                        }
                    });
                    ui.add_space(4.0);
                    for (li, l) in self.lenses.iter().enumerate() {
                        ui.label(RichText::new(format!("Lens {}: {:.1}° wide, distortion {:+.3}", li + 1, l.hfov_deg(), l.k1)).small().color(MUTED));
                    }
                });
            }
        });
    }
}
