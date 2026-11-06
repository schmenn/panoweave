use super::*;

impl PanoApp {
    // ---------------- exposure blending ----------------

    pub(super) fn load_brackets(&mut self, items: Vec<(String, Source)>) {
        let n = items.len();
        self.start_job("Loading bracketed photos", move |report, _| {
            use rayon::prelude::*;
            let r = engine::load_pool().install(|| items
                .into_par_iter()
                .map(|(name, src)| {
                    if engine::cancelled() {
                        return Err("cancelled".to_string());
                    }
                    let full = src.load_full()?;
                    let info = match &src {
                        Source::File(p) => imgutil::read_info(p),
                        _ => PhotoInfo::default(),
                    };
                    let work = Arc::new(imgutil::downscale_rgb(&full, 1200));
                    let thumb = imgutil::downscale_rgb(&work, 200);
                    Ok((name, src, work, thumb, info))
                })
                .collect());
            report(1.0, &format!("Loaded {n}"));
            JobMsg::BracketLoaded(r)
        });
    }

    pub(super) fn load_bracket_sample(&mut self) {
        if self.busy() {
            return;
        }
        self.start_job("Creating sample brackets", |_, _| {
            let shots = sample::bracket_set();
            JobMsg::BracketLoaded(
                shots
                    .into_iter()
                    .map(|s| {
                        let full = Arc::new(s.img);
                        let thumb = imgutil::downscale_rgb(&full, 200);
                        Ok((s.name, Source::Memory(full.clone()), full, thumb, PhotoInfo::default()))
                    })
                    .collect(),
            )
        });
    }

    pub(super) fn request_fusion_preview(&mut self) {
        if self.fusion_rx.is_some() || self.brackets.len() < 2 {
            return;
        }
        self.fusion_dirty = false;
        let imgs: Vec<RgbImage> = self.brackets.iter().map(|b| (*b.work).clone()).collect();
        let fw = self.fusion_weights;
        let al = self.fusion_align;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let imgs = if al { fusion::align_stack(&imgs) } else { imgs };
            let out = fusion::fuse(&imgs, &fw);
            let _ = tx.send(color_image(&out));
        });
        self.fusion_rx = Some(rx);
    }

    pub(super) fn run_fusion_export(&mut self) {
        if self.busy() || self.brackets.len() < 2 {
            return;
        }
        if self.fusion_out_path.trim().is_empty() {
            self.fusion_out_path = self.default_out_path("blended", "jpg");
        }
        let path = PathBuf::from(self.fusion_out_path.trim());
        let sources: Vec<Source> = self.brackets.iter().map(|b| b.source.clone()).collect();
        let fw = self.fusion_weights;
        let al = self.fusion_align;
        let q = self.jpeg_quality;
        self.start_job("Blending exposures", move |report, _| {
            let t0 = Instant::now();
            let res = (|| {
                report(0.05, "Loading full-size photos");
                let mut imgs = Vec::new();
                for s in &sources {
                    imgs.push((*s.load_full()?).clone());
                }
                if al {
                    report(0.3, "Aligning handheld shots");
                    imgs = fusion::align_stack(&imgs);
                }
                report(0.55, "Blending");
                let out = fusion::fuse(&imgs, &fw);
                report(0.95, "Saving");
                let rgba: Vec<u8> = out.pixels().flat_map(|p| [p[0], p[1], p[2], 255]).collect();
                let fmt = match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()) {
                    Some(e) if e == "png" => OutFormat::Png,
                    Some(e) if e == "tif" || e == "tiff" => OutFormat::Tiff,
                    _ => OutFormat::Jpeg,
                };
                save_output(&path, &rgba, out.width() as usize, out.height() as usize, fmt, q)?;
                Ok((path.clone(), t0.elapsed().as_millis()))
            })();
            JobMsg::FusedSaved(res)
        });
    }

    // ----- exposure mode -----
    pub(super) fn ui_exposure(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("brackets")
            .resizable(true)
            .default_size(250.0)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(egui::Margin::same(10)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Exposures").strong().size(16.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(!self.busy(), egui::Button::new("➕ Add")).clicked() {
                            self.pick_photos();
                        }
                    });
                });
                let mut remove = None;
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for (i, b) in self.brackets.iter().enumerate() {
                        egui::Frame::new().fill(CARD).corner_radius(CornerRadius::same(RADIUS_CARD)).inner_margin(egui::Margin::same(TILE_MARGIN)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let s = b.thumb.size_vec2();
                                ui.add(egui::Image::new(egui::load::SizedTexture::new(b.thumb.id(), Vec2::new(72.0, 72.0 * s.y / s.x))).corner_radius(CornerRadius::same(RADIUS)));
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(&b.name).strong());
                                    if let Some(t) = b.info.exposure_time {
                                        ui.label(RichText::new(if t < 1.0 { format!("1/{:.0} s", 1.0 / t) } else { format!("{t:.1} s") }).small().color(MUTED));
                                    }
                                    if ui.small_button("🗑").clicked() {
                                        remove = Some(i);
                                    }
                                });
                            });
                        });
                    }
                });
                if let Some(i) = remove {
                    self.brackets.remove(i);
                    self.fusion_dirty = true;
                }
            });
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::same(14))).show(ui, |ui| {
            if self.brackets.is_empty() {
                ui.add_space(60.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Blend a bracketed series into one picture").size(22.0).color(Color32::WHITE));
                    ui.label(RichText::new("Add 2–9 photos of the same view taken with different exposures. Dark areas come from the bright shots, bright areas from the dark shots – no halos, no tone mapping needed.").color(MUTED));
                    ui.add_space(14.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.add_space((ui.available_width() - 470.0).max(0.0) / 2.0);
                        if primary_button(ui, "➕  Add exposures…", !self.busy()).clicked() {
                            self.pick_photos();
                        }
                        if ui.add_enabled(!self.busy(), egui::Button::new("🖼  Try with sample exposures").min_size(Vec2::new(0.0, BIG_H))).clicked() {
                            self.load_bracket_sample();
                        }
                    });
                });
                return;
            }
            ui.horizontal_wrapped(|ui| {
                let mut ch = false;
                ch |= ui.checkbox(&mut self.fusion_align, "Align handheld shots").changed();
                ui.separator();
                ch |= ui.add(egui::Slider::new(&mut self.fusion_weights.exposure, 0.0..=3.0).text("good exposure")).changed();
                ch |= ui.add(egui::Slider::new(&mut self.fusion_weights.contrast, 0.0..=3.0).text("detail")).changed();
                ch |= ui.add(egui::Slider::new(&mut self.fusion_weights.saturation, 0.0..=3.0).text("colour")).changed();
                if ch {
                    self.fusion_dirty = true;
                }
            });
            ui.horizontal(|ui| {
                if self.fusion_out_path.is_empty() {
                    self.fusion_out_path = self.default_out_path("blended", "jpg");
                }
                ui.add(egui::TextEdit::singleline(&mut self.fusion_out_path).desired_width(420.0));
                if ui.button("Choose…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().add_filter("Pictures", &["jpg", "png", "tif"]).set_file_name("blended.jpg").save_file() {
                        self.fusion_out_path = p.to_string_lossy().to_string();
                    }
                }
                if primary_button(ui, "Export blended picture", !self.busy() && self.brackets.len() >= 2).clicked() {
                    self.run_fusion_export();
                }
                if self.fusion_rx.is_some() {
                    ui.add(egui::Spinner::new());
                    ui.ctx().request_repaint();
                }
            });
            ui.add_space(6.0);
            let avail = ui.available_size();
            if let Some(t) = &self.fusion_tex {
                let s = t.size_vec2();
                let sc = (avail.x / s.x).min((avail.y - 4.0) / s.y);
                let (rect, _) = ui.allocate_exact_size(avail, Sense::hover());
                let r = Rect::from_center_size(rect.center(), s * sc);
                ui.painter().image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            } else if self.brackets.len() < 2 {
                ui.label(RichText::new("Add at least two exposures.").color(MUTED));
            }
        });
    }
}
