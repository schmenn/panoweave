use super::*;

impl PanoApp {
    pub(super) fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top")
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::symmetric(14, 8)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (r, p) = ui.allocate_painter(Vec2::new(30.0, 30.0), Sense::hover());
                    let c = r.rect.center();
                    p.circle_filled(c, 14.0, ACCENT_DARK);
                    p.line_segment([c + Vec2::new(-9.0, 4.0), c + Vec2::new(-3.0, -4.0)], Stroke::new(2.5, Color32::WHITE));
                    p.line_segment([c + Vec2::new(-3.0, -4.0), c + Vec2::new(2.0, 2.0)], Stroke::new(2.5, Color32::WHITE));
                    p.line_segment([c + Vec2::new(2.0, 2.0), c + Vec2::new(5.0, -1.0)], Stroke::new(2.5, Color32::WHITE));
                    p.line_segment([c + Vec2::new(5.0, -1.0), c + Vec2::new(9.0, 4.0)], Stroke::new(2.5, Color32::WHITE));
                    ui.label(RichText::new("PanoWeave").size(19.0).strong().color(Color32::WHITE));
                    ui.add_space(18.0);
                    for (m, label) in [(Mode::Panorama, "🌄  Panorama"), (Mode::Exposure, "◑  Exposure blend")] {
                        let sel = self.mode == m;
                        let b = egui::Button::new(RichText::new(label).size(BIG_FONT).color(if sel { Color32::WHITE } else { MUTED }))
                            .fill(if sel { ACCENT_DARK } else { Color32::TRANSPARENT })
                            .min_size(Vec2::new(0.0, BIG_H));
                        if ui.add(b).clicked() {
                            self.mode = m;
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("?  Help").clicked() {
                            self.show_help = !self.show_help;
                        }
                        if self.mode == Mode::Panorama {
                            if ui.add_enabled(!self.photos.is_empty() && !self.busy(), egui::Button::new("💾  Save project")).clicked() {
                                let start = self.project_path.clone();
                                let mut d = rfd::FileDialog::new().add_filter("PanoWeave project", &["pwproj"]).set_file_name("panorama.pwproj");
                                if let Some(s) = start.as_ref().and_then(|p| p.parent()) {
                                    d = d.set_directory(s);
                                }
                                if let Some(p) = d.save_file() {
                                    self.save_project(p);
                                }
                            }
                            if ui.add_enabled(!self.busy(), egui::Button::new("📂  Open project")).clicked()
                                && let Some(p) = rfd::FileDialog::new().add_filter("PanoWeave project", &["pwproj"]).pick_file() {
                                    self.open_project(&p);
                                }
                            if ui.add_enabled(!self.photos.is_empty() && !self.busy(), egui::Button::new("➕  New")).clicked() {
                                self.clear_all();
                                self.set_status("Started a new panorama.", MUTED);
                            }
                        }
                    });
                });
            });
    }

    pub(super) fn step_done(&self, s: Step) -> bool {
        match s {
            Step::Photos => self.photos.len() >= 2,
            Step::Points => self.matched && !self.cps.is_empty(),
            Step::Align => self.aligned && !self.needs_align,
            Step::Frame => self.aligned && self.view.crop != [0.0, 1.0, 0.0, 1.0],
            Step::Export => self.last_output.is_some(),
        }
    }

    pub(super) fn step_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("steps")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(egui::Margin::symmetric(14, 8)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (k, s) in Step::ALL.iter().enumerate() {
                        let sel = self.step == *s;
                        let done = self.step_done(*s);
                        let (rect, resp) = ui.allocate_exact_size(Vec2::new(178.0, 38.0), Sense::click());
                        let p = ui.painter();
                        let fill = if sel {
                            ACCENT_DARK
                        } else if resp.hovered() {
                            Color32::from_rgb(48, 52, 61)
                        } else {
                            CARD
                        };
                        p.rect_filled(rect, f32::from(RADIUS_CARD), fill);
                        let cc = rect.left_center() + Vec2::new(20.0, 0.0);
                        p.circle_filled(cc, 11.0, if done { GOOD } else if sel { ACCENT } else { Color32::from_rgb(70, 75, 88) });
                        p.text(cc, Align2::CENTER_CENTER, if done { "✔".to_string() } else { format!("{}", k + 1) }, FontId::proportional(13.0), Color32::WHITE);
                        p.text(
                            rect.left_center() + Vec2::new(38.0, 0.0),
                            Align2::LEFT_CENTER,
                            s.title(),
                            FontId::proportional(14.5),
                            if sel { Color32::WHITE } else { Color32::from_rgb(205, 210, 220) },
                        );
                        if resp.clicked() {
                            self.step = *s;
                        }
                        if k < 4 {
                            ui.label(RichText::new("›").size(18.0).color(MUTED));
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let can = self.photos.len() >= 2 && !self.busy();
                        if primary_button(ui, "✨  Create panorama", can)
                            .on_hover_text("Find points, align, choose projection and crop – all in one go.")
                            .clicked()
                        {
                            self.create_auto();
                        }
                    });
                });
            });
    }

    pub(super) fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status")
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::symmetric(14, 6)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some(job) = &self.job {
                        let (v, s) = job.progress.lock().map(|g| g.clone()).unwrap_or((0.0, String::new()));
                        ui.add(egui::Spinner::new());
                        ui.label(RichText::new(&s).color(Color32::WHITE));
                        ui.add(egui::ProgressBar::new(v).desired_width(260.0).show_percentage());
                        ui.label(RichText::new(format!("{:.1} s", job.started.elapsed().as_secs_f32())).color(MUTED));
                        let stopping = job.cancel.load(Ordering::Relaxed);
                        if ui.add_enabled(!stopping, egui::Button::new(if stopping { "Stopping…" } else { "Cancel" })).clicked() {
                            // the render job watches its own flag, everything else the global one
                            job.cancel.store(true, Ordering::Relaxed);
                            engine::CANCEL.store(true, Ordering::Relaxed);
                        }
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(60));
                    } else {
                        ui.label(RichText::new(&self.status).color(self.status_color));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.mode == Mode::Panorama {
                            ui.label(
                                RichText::new(format!(
                                    "{} photos · {} points · {} lens{}",
                                    self.photos.len(),
                                    self.cps.len(),
                                    self.lenses.len(),
                                    if self.lenses.len() == 1 { "" } else { "es" }
                                ))
                                .color(MUTED),
                            );
                        }
                    });
                });
            });
    }
}
