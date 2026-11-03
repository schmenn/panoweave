use super::*;

pub(super) fn save_output(path: &Path, rgba: &[u8], w: usize, h: usize, fmt: OutFormat, quality: u8) -> Result<(), String> {
    if let Some(d) = path.parent() {
        if !d.as_os_str().is_empty() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
    }
    match fmt {
        OutFormat::Jpeg => {
            let rgb: Vec<u8> = rgba.chunks(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
            let f = std::fs::File::create(path).map_err(|e| e.to_string())?;
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(f), quality);
            enc.encode(&rgb, w as u32, h as u32, image::ExtendedColorType::Rgb8).map_err(|e| e.to_string())
        }
        OutFormat::Png | OutFormat::Tiff => {
            image::save_buffer(path, rgba, w as u32, h as u32, image::ColorType::Rgba8).map_err(|e| e.to_string())
        }
    }
}

impl PanoApp {
    // ---------------- export ----------------

    pub(super) fn natural_width(&self) -> f64 {
        let act = self.active_indices();
        let best = act
            .iter()
            .map(|&i| self.lenses[self.photos[i].lens].f * self.photos[i].long())
            .fold(0.0, f64::max);
        let b = self.view.crop_bounds();
        // plane units are ~radians near the centre for all projections
        ((b[1] - b[0]) * best).max(64.0)
    }

    pub(super) fn default_out_path(&self, stem: &str, ext: &str) -> String {
        let dir = self
            .photos
            .iter()
            .find_map(|p| p.source.path().and_then(|q| q.parent().map(|d| d.to_path_buf())))
            .or_else(|| self.brackets.iter().find_map(|p| p.source.path().and_then(|q| q.parent().map(|d| d.to_path_buf()))))
            .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Pictures")))
            .unwrap_or_else(|| PathBuf::from("."));
        dir.join(format!("{stem}.{ext}")).to_string_lossy().to_string()
    }

    /// (width, height, bytes needed, bytes free) of the panorama file at the chosen size.
    pub(super) fn render_memory(&self) -> (usize, usize, u64, Option<u64>) {
        let nw = self.natural_width();
        let cb = self.view.crop_bounds();
        let asp = (cb[1] - cb[0]) / (cb[3] - cb[2]);
        let w = (nw * self.out_scale as f64 / 100.0).round().max(16.0) as usize;
        let h = ((w as f64 / asp).round() as usize).max(16);
        (w, h, stitch::estimate_bytes(w, h, self.blend), crate::sysmem::available())
    }

    pub(super) fn run_render(&mut self) {
        if self.busy() || !self.aligned {
            return;
        }
        let (_, _, need, free) = self.render_memory();
        if let Some(free) = free {
            let usable = (free as f64 * RENDER_MEMORY_SHARE) as u64;
            if need > usable {
                let fit = (self.out_scale as f64 * (usable as f64 / need as f64).sqrt()).floor().max(1.0);
                self.set_status(
                    format!(
                        "This size needs about {:.1} GB of memory, but only {:.1} GB are free. Choose at most {:.0} % (or close other programs).",
                        crate::sysmem::gb(need),
                        crate::sysmem::gb(free),
                        fit
                    ),
                    BAD,
                );
                return;
            }
        }
        if self.out_path.trim().is_empty() {
            self.out_path = self.default_out_path("panorama", self.out_format.ext());
        }
        let path = PathBuf::from(self.out_path.trim());
        let path = if path.extension().is_none() { path.with_extension(self.out_format.ext()) } else { path };
        let active = self.active_indices();
        type Src = (Source, Arc<RgbImage>, u32, u32, M3, Lens, [f32; 3], Option<Arc<fine_align::DisplacementField>>, bool);
        let trusted = self.connected_photos();
        let sources: Vec<Src> = active
            .iter()
            .map(|&i| {
                let p = &self.photos[i];
                (
                    p.source.clone(),
                    p.work.clone(),
                    p.w,
                    p.h,
                    p.rot,
                    self.lenses[p.lens],
                    if self.match_brightness { self.gains[i] } else { [1.0; 3] },
                    self.fields.get(i).cloned().flatten(),
                    trusted[i],
                )
            })
            .collect();
        let view = self.view.clone();
        let out_w = (self.natural_width() * self.out_scale as f64 / 100.0).round() as usize;
        let blend = self.blend;
        let render_opts = stitch::RenderOptions { levels: self.blend_levels, cubic: self.cubic_interp, prefer_detail: self.prefer_detail };
        let fmt = self.out_format;
        let q = self.jpeg_quality;
        self.start_job("Creating the panorama", move |report, cancel| {
            let t0 = Instant::now();
            let cache: Mutex<Option<(usize, Arc<RgbImage>)>> = Mutex::new(None);
            let loader = |i: usize, pixels: bool| -> Result<RenderPhoto, String> {
                let (src, work, w, h, rot, lens, gain, field, trusted) = &sources[i];
                let img = if pixels {
                    let mut c = cache.lock().unwrap();
                    match c.as_ref() {
                        Some((k, im)) if *k == i => im.clone(),
                        _ => {
                            let im = src.load_full()?;
                            *c = Some((i, im.clone()));
                            im
                        }
                    }
                } else {
                    // seams only need the geometry plus where the photo is blown out: the small copy is enough
                    work.clone()
                };
                Ok(RenderPhoto { img, w: *w as f64, h: *h as f64, rot: *rot, lens: *lens, gain: *gain, field: field.clone(), full_long: (*w).max(*h) as f64, trusted: *trusted })
            };
            let prog = stitch::Progress { report, cancel };
            let r = stitch::render_final(sources.len(), &loader, &view, out_w, blend, &render_opts, &prog);
            let res = r.and_then(|(col, cov, w, h)| {
                report(0.98, "Saving the file");
                let rgba = stitch::to_rgba8(&col, &cov);
                save_output(&path, &rgba, w, h, fmt, q).map(|_| (path.clone(), t0.elapsed().as_millis(), w, h))
            });
            JobMsg::Rendered(res)
        });
    }

    // ----- step 5 -----
    pub(super) fn ui_export(&mut self, ui: &mut egui::Ui) {
        if !self.aligned {
            ui.label(RichText::new("Align the photos first (or press ✨ Create panorama).").color(MUTED));
            return;
        }
        if self.out_path.is_empty() {
            self.out_path = self.default_out_path("panorama", self.out_format.ext());
        }
        let avail_w = ui.available_width();
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width((avail_w * 0.45).max(380.0));
                card(ui, |ui| {
                    ui.label(RichText::new("Size").strong());
                    let nw = self.natural_width();
                    let cb = self.view.crop_bounds();
                    let asp = (cb[1] - cb[0]) / (cb[3] - cb[2]);
                    ui.add(egui::Slider::new(&mut self.out_scale, 5.0..=100.0).suffix(" %").text("of full detail"));
                    let w = (nw * self.out_scale as f64 / 100.0).round();
                    let h = (w / asp).round();
                    ui.label(format!("{w:.0} × {h:.0} px  ({:.1} megapixels)", w * h / 1e6));
                    {
                        let (_, _, need, free) = self.render_memory();
                        match free {
                            Some(free) if need as f64 > free as f64 * RENDER_MEMORY_SHARE => {
                                let fit = (self.out_scale as f64 * ((free as f64 * RENDER_MEMORY_SHARE) / need as f64).sqrt()).floor().max(1.0);
                                ui.label(
                                    RichText::new(format!(
                                        "Needs about {:.1} GB of memory, only {:.1} GB free – choose at most {:.0} %.",
                                        crate::sysmem::gb(need),
                                        crate::sysmem::gb(free),
                                        fit
                                    ))
                                    .color(BAD),
                                );
                                if ui.button(format!("Use {fit:.0} %")).clicked() {
                                    self.out_scale = fit as f32;
                                }
                            }
                            Some(free) => {
                                ui.label(RichText::new(format!("needs about {:.1} GB of memory ({:.1} GB free)", crate::sysmem::gb(need), crate::sysmem::gb(free))).small().color(MUTED));
                            }
                            None => {}
                        }
                    }
                    ui.horizontal(|ui| {
                        for (t, v) in [("Small", 25.0), ("Medium", 50.0), ("Full", 100.0)] {
                            if ui.button(t).clicked() {
                                self.out_scale = v;
                            }
                        }
                    });
                    if w * h > 150e6 {
                        ui.label(RichText::new("Very large – this needs a lot of memory.").small().color(WARN));
                    }
                });
                ui.add_space(6.0);
                card(ui, |ui| {
                    ui.label(RichText::new("File").strong());
                    ui.horizontal(|ui| {
                        for f in [OutFormat::Jpeg, OutFormat::Png, OutFormat::Tiff] {
                            if ui.add(egui::Button::selectable(self.out_format == f, f.name())).clicked() {
                                self.out_format = f;
                                self.out_path = PathBuf::from(&self.out_path).with_extension(f.ext()).to_string_lossy().to_string();
                            }
                        }
                    });
                    if self.out_format == OutFormat::Jpeg {
                        ui.add(egui::Slider::new(&mut self.jpeg_quality, 50..=100).text("quality"));
                    }
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.out_path).desired_width(ui.available_width() - 90.0));
                        if ui.button("Choose…").clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .add_filter(self.out_format.name(), &[self.out_format.ext()])
                                .set_file_name(format!("panorama.{}", self.out_format.ext()))
                                .save_file()
                            {
                                self.out_path = p.to_string_lossy().to_string();
                            }
                        }
                    });
                });
                ui.add_space(6.0);
                card(ui, |ui| {
                    ui.label(RichText::new("Blending").strong());
                    ui.radio_value(&mut self.blend, Blend::Smooth, "Smooth seams (recommended) – hides seams at every detail level");
                    ui.radio_value(&mut self.blend, Blend::Feather, "Soft fade – faster, can show ghosting with moving subjects");
                    ui.checkbox(&mut self.match_brightness, "Match brightness and colour between photos");
                    ui.horizontal(|ui| {
                        ui.label("Seam transition (detail levels)");
                        egui::ComboBox::from_id_salt("blend_levels")
                            .selected_text(if self.blend_levels == 0 { "Automatic (wide, as before)".to_string() } else { format!("{} (narrow ≈ {} px)", self.blend_levels, 1usize << self.blend_levels) })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.blend_levels, 0, "Automatic (wide, as before)");
                                for l in 2..=7usize {
                                    ui.selectable_value(&mut self.blend_levels, l, format!("{} (≈ {} px)", l, 1usize << l));
                                }
                            });
                    });
                    ui.checkbox(&mut self.cubic_interp, "Sharper resampling (bicubic instead of bilinear)");
                    if ui
                        .checkbox(&mut self.prefer_detail, "Use the sharpest photo where photos overlap")
                        .on_hover_text("Where photos lie on top of each other, the one with the most real detail (zoomed in or higher resolution) is used. Off: seams in the middle of the overlaps, as before.")
                        .changed()
                    {
                        self.preview_dirty = true;
                    }
                });
                ui.add_space(10.0);
                if primary_button(ui, "💾  Create panorama file", !self.busy()).clicked() {
                    self.run_render();
                }
                if let Some(p) = self.last_output.clone() {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("✔ Saved: {}", p.display())).color(GOOD));
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Open picture").clicked() {
                            let _ = open::that(&p);
                        }
                        if ui.button("Show in folder").clicked() {
                            if let Some(d) = p.parent() {
                                let _ = open::that(d);
                            }
                        }
                    });
                }
            });
            ui.add_space(12.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("Preview").strong());
                if let Some(t) = &self.preview_tex {
                    let w = ui.available_width();
                    let c = self.view.crop;
                    let asp = self.view.aspect() as f32 * ((c[1] - c[0]) / (c[3] - c[2])) as f32;
                    let size = Vec2::new(w, w / asp);
                    let uv = Rect::from_min_max(Pos2::new(c[0] as f32, c[2] as f32), Pos2::new(c[1] as f32, c[3] as f32));
                    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
                    ui.painter().image(t.id(), rect, uv, Color32::WHITE);
                } else {
                    ui.label(RichText::new("Open step 4 once to build the preview.").color(MUTED));
                    self.preview_dirty = true;
                    self.request_preview(1400, false);
                }
            });
        });
    }
}
