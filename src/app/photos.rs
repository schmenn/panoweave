use super::*;

impl PanoApp {
    // ---------------- photos ----------------

    pub(super) fn add_paths(&mut self, paths: Vec<PathBuf>) {
        let mut imgs = Vec::new();
        for p in paths {
            if p.extension().map(|e| e.to_string_lossy().eq_ignore_ascii_case("pwproj")).unwrap_or(false) {
                self.open_project(&p);
                return;
            }
            if imgutil::is_supported(&p) {
                imgs.push(p);
            }
        }
        if imgs.is_empty() {
            self.set_status("Those files are not photos I can read (JPEG, PNG, TIFF or RAW).", WARN);
            return;
        }
        if self.busy() {
            self.set_status("Please wait until the current task has finished.", WARN);
            return;
        }
        if self.mode == Mode::Exposure {
            self.load_brackets(imgs.into_iter().map(|p| (p.file_name().unwrap_or_default().to_string_lossy().to_string(), Source::File(p))).collect());
            return;
        }
        let items: Vec<(String, Source)> =
            imgs.into_iter().map(|p| (p.file_name().unwrap_or_default().to_string_lossy().to_string(), Source::File(p))).collect();
        self.load_sources(items);
    }

    pub(super) fn load_sources(&mut self, items: Vec<(String, Source)>) {
        let n = items.len();
        self.start_job(&format!("Loading {n} photos"), move |report, _c| {
            use rayon::prelude::*;
            let done = std::sync::atomic::AtomicUsize::new(0);
            // only a few photos are decoded at the same time: decoding needs much more memory
            // than the photo keeps afterwards
            let r: Vec<_> = engine::load_pool().install(|| {
                items
                    .into_par_iter()
                    .map(|(name, src)| {
                        if engine::cancelled() {
                            return Err("cancelled".to_string());
                        }
                        let r = engine::load(src, name);
                        let k = done.fetch_add(1, Ordering::Relaxed) + 1;
                        report(k as f32 / n as f32, &format!("Loading photos ({k} of {n})"));
                        r
                    })
                    .collect()
            });
            JobMsg::Loaded(r)
        });
    }

    pub(super) fn load_sample(&mut self) {
        if self.busy() {
            return;
        }
        self.start_job("Creating sample photos", |report, _| {
            report(0.1, "Creating sample photos");
            let shots = sample::panorama_set();
            let r = shots
                .into_iter()
                .map(|s| engine::load(Source::Memory(Arc::new(s.img)), s.name))
                .collect();
            JobMsg::Loaded(r)
        });
    }

    pub(super) fn lens_for(&mut self, p: &LoadedPhoto) -> usize {
        let hfov = engine::exif_hfov(&p.info);
        let long = p.w.max(p.h);
        // same lens = same focal length & same photo size as an existing photo
        for (i, ph) in self.photos.iter().enumerate() {
            if ph.long() as u32 == long && ph.info.focal_mm == p.info.focal_mm && ph.info.focal_35mm == p.info.focal_35mm {
                return self.photos[i].lens;
            }
        }
        let lens = match hfov {
            Some(h) => Lens { from_exif: true, ..Lens::from_hfov_deg(h) },
            None => Lens::from_hfov_deg(50.0),
        };
        self.lenses.push(lens);
        self.lenses.len() - 1
    }

    pub(super) fn on_loaded(&mut self, ctx: &egui::Context, res: Vec<Result<LoadedPhoto, String>>) {
        let mut errs = Vec::new();
        let mut added = 0;
        for r in res {
            match r {
                Ok(p) => {
                    let lens = self.lens_for(&p);
                    let thumb = ctx.load_texture(format!("thumb-{}-{}", self.photos.len(), p.name), color_image(&p.thumb), egui::TextureOptions::LINEAR);
                    self.photos.push(Photo {
                        name: p.name,
                        source: p.source,
                        w: p.w,
                        h: p.h,
                        work: p.work,
                        gray: p.gray,
                        info: p.info,
                        thumb,
                        tex: None,
                        lens,
                        rot: IDENTITY,
                        enabled: true,
                        mask: Default::default(),
                    });
                    added += 1;
                }
                Err(e) => errs.push(e),
            }
        }
        self.gains = vec![[1.0; 3]; self.photos.len()];
        self.gain_maps = vec![None; self.photos.len()];
        self.lens_models = vec![None; self.photos.len()];
        if let Some(pf) = self.pending_project.take() {
            let total = pf.photos.len();
            // which photos of the project were loaded (the loader keeps the order and leaves out what failed)
            let loaded: Vec<PathBuf> = self.photos.iter().filter_map(|p| if let Source::File(f) = &p.source { Some(f.clone()) } else { None }).collect();
            let mut next = loaded.iter().peekable();
            let found: Vec<bool> = pf.photos.iter().map(|pp| next.next_if(|f| **f == pp.path).is_some()).collect();
            let missing: Vec<String> = pf.photos.iter().zip(&found).filter(|(_, f)| !**f).map(|(pp, _)| pp.path.display().to_string()).collect();
            let (pf, dropped) = project::keep_photos(pf, &found);
            if pf.photos.len() != self.photos.len() {
                // cannot happen with the loader keeping the order; do not apply anything rather than mix photos up
                self.set_status("The photos of the project could not be matched to the files – nothing of the project was applied.", BAD);
                self.project_path = None;
                return;
            }
            self.apply_project(pf);
            if missing.is_empty() {
                self.set_status(format!("Project opened with {} photos.", self.photos.len()), GOOD);
            } else {
                // a save must not replace the complete project by the reduced one without the person choosing so
                self.project_path = None;
                let why = errs.first().map(|e| format!(" ({e})")).unwrap_or_default();
                self.set_status(
                    format!(
                        "Project opened with {} of {total} photos – missing: {}{why}. {dropped} points to them were left out. Save under a new name to keep the original file complete.",
                        self.photos.len(),
                        missing.join(", ")
                    ),
                    WARN,
                );
            }
            return;
        }
        if added > 0 {
            self.matched = false;
            self.aligned = false;
        }
        if errs.is_empty() {
            self.set_status(format!("Added {added} photos. Press “Create panorama” – or go step by step."), GOOD);
        } else {
            self.set_status(format!("Added {added} photos, {} could not be read: {}", errs.len(), errs[0]), WARN);
        }
    }

    pub(super) fn remove_photo(&mut self, i: usize) {
        self.full_tex.clear();
        self.fields.clear();
        self.photos.remove(i);
        self.mk.sel = 0;
        self.mk.tool.cancel();
        self.mk.overlay = None;
        self.mk.seam_key = None;
        self.gains.remove(i);
        if i < self.gain_maps.len() {
            self.gain_maps.remove(i);
        }
        if i < self.lens_models.len() {
            self.lens_models.remove(i);
        }
        // photo numbers shift: older steps would point at the wrong photos
        self.undo.clear();
        self.cps.retain(|c| c.a != i && c.b != i);
        for c in self.cps.iter_mut() {
            if c.a > i {
                c.a -= 1;
            }
            if c.b > i {
                c.b -= 1;
            }
        }
        self.pe = PointEditor { a: 0, b: 1.min(self.photos.len().saturating_sub(1)), ..Default::default() };
        self.forget_photo_numbers();
        self.needs_align = true;
        self.preview_dirty = true;
    }

    /// State that names photos by their number; after removing a photo or starting anew it would point at other photos.
    fn forget_photo_numbers(&mut self) {
        // a brightness update still being worked out belongs to the old photo numbers
        self.gains_rx = None;
        self.graph_photo = None;
        self.click = ClickState::default();
        self.pair_summary = None;
        self.sky_cache = None;
        self.dup_cache = None;
        self.cp_rows_cache = None;
        self.ov_tex = None;
    }

    pub(super) fn clear_all(&mut self) {
        self.full_tex.clear();
        self.fields.clear();
        self.last_fine = None;
        self.photos.clear();
        self.lenses.clear();
        self.cps.clear();
        self.undo.clear();
        self.gains.clear();
        self.gain_maps.clear();
        self.view = PanoView::default();
        self.aligned = false;
        self.matched = false;
        self.preview_tex = None;
        self.project_path = None;
        self.step = Step::Photos;
        self.last_match = None;
        self.last_align = None;
        self.pe = PointEditor { b: 1, ..Default::default() };
        self.mk = Default::default();
        self.forget_photo_numbers();
        self.lens_models.clear();
        self.needs_align = false;
        // the export of a new project goes next to its own photos, not into the old project's folder
        self.out_path.clear();
        self.last_output = None;
        // a preview of the old project still being computed must not show up in the new one
        self.preview_rx = None;
        self.preview_dirty = true;
    }

    // ---------------- project files ----------------

    pub(super) fn save_project(&mut self, path: PathBuf) {
        let mut photos = Vec::new();
        let photo_dir = path.with_extension("").to_string_lossy().to_string() + "_photos";
        for p in &self.photos {
            let pp = match &p.source {
                Source::File(f) => f.clone(),
                Source::Memory(img) => {
                    let _ = std::fs::create_dir_all(&photo_dir);
                    let f = PathBuf::from(&photo_dir).join(Path::new(&p.name).with_extension("png"));
                    if let Err(e) = img.save(&f) {
                        self.set_status(format!("Could not save a sample photo: {e}"), BAD);
                        return;
                    }
                    f
                }
            };
            let field = self.fields.get(photos.len()).and_then(|f| f.as_ref()).map(|f| (**f).clone());
            photos.push(ProjectPhoto { path: pp, lens: p.lens, rot: p.rot, enabled: p.enabled, field, masks: p.mask.strokes.clone() });
        }
        let pf = ProjectFile {
            version: project::VERSION,
            photos,
            lenses: self.lenses.clone(),
            cps: self.cps.clone(),
            view: self.view.clone(),
            aligned: self.aligned,
            parallax: Some(self.parallax_cfg.clone()),
        };
        match project::save(&path, &pf) {
            Ok(0) => {
                self.set_status(format!("Project saved: {}", path.display()), GOOD);
                self.project_path = Some(path);
            }
            Ok(n) => {
                self.set_status(format!("Project saved: {} ({n} invalid measurement values of points were saved as 0)", path.display()), WARN);
                self.project_path = Some(path);
            }
            Err(e) => self.set_status(format!("Could not save the project: {e}"), BAD),
        }
    }

    pub(super) fn open_project(&mut self, path: &Path) {
        if self.busy() {
            return;
        }
        match project::load(path) {
            Ok(pf) => {
                self.clear_all();
                self.mode = Mode::Panorama;
                let items: Vec<(String, Source)> = pf
                    .photos
                    .iter()
                    .map(|p| (p.path.file_name().unwrap_or_default().to_string_lossy().to_string(), Source::File(p.path.clone())))
                    .collect();
                self.project_path = Some(path.to_path_buf());
                self.pending_project = Some(pf);
                self.load_sources(items);
            }
            Err(e) => self.set_status(format!("Could not open the project: {e}"), BAD),
        }
    }

    /// Takes over a project whose photos are exactly `self.photos` (in the same order).
    pub(super) fn apply_project(&mut self, pf: ProjectFile) {
        self.lenses = pf.lenses;
        for (p, pp) in self.photos.iter_mut().zip(&pf.photos) {
            p.lens = pp.lens;
            p.rot = pp.rot;
            p.enabled = pp.enabled;
            p.mask.load(pp.masks.clone(), p.w as usize, p.h as usize);
        }
        if let Some(pc) = pf.parallax {
            self.parallax_cfg = pc;
        }
        self.fields = pf.photos.iter().map(|pp| pp.field.clone().map(Arc::new)).collect();
        self.cps = pf.cps;
        self.undo.clear();
        self.view = pf.view;
        self.aligned = pf.aligned;
        self.matched = !self.cps.is_empty();
        if self.aligned {
            self.update_gains();
            self.step = Step::Frame;
            self.preview_dirty = true;
        }
    }

    pub(super) fn photo_list(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("photos")
            .resizable(true)
            .default_size(250.0)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(egui::Margin::same(10)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Your photos").strong().size(16.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(!self.busy(), egui::Button::new("➕ Add")).clicked() {
                            self.pick_photos();
                        }
                    });
                });
                ui.add_space(4.0);
                let comps = crate::align::components(self.photos.len(), &self.cps);
                let main_group = most_common(&comps);
                let mut per_photo = vec![0usize; self.photos.len()];
                for c in &self.cps {
                    if c.a < per_photo.len() {
                        per_photo[c.a] += 1;
                    }
                    if c.b < per_photo.len() && c.b != c.a {
                        per_photo[c.b] += 1;
                    }
                }
                let mut remove = None;
                let mut toggled = false;
                let mut menu_resp: Vec<(usize, egui::Response)> = Vec::new();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    if self.photos.is_empty() {
                        ui.add_space(30.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("No photos yet").color(MUTED));
                            ui.label(RichText::new("Drop photos onto the window\nor press ➕ Add").color(MUTED).small());
                        });
                    }
                    let busy = self.busy();
                    for i in 0..self.photos.len() {
                        let menu_state = self.menu_state(i);
                        let p = &mut self.photos[i];
                        let connected = !self.matched || comps.get(i) == Some(&main_group);
                        let pts = per_photo[i];
                        let highlight = self.step == Step::Points && (self.pe.a == i || self.pe.b == i);
                        egui::Frame::new()
                            .fill(if highlight { ACCENT_SOFT } else { CARD })
                            .corner_radius(CornerRadius::same(RADIUS_CARD))
                            .inner_margin(egui::Margin::same(TILE_MARGIN))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    let tw = 72.0;
                                    let ts = p.thumb.size_vec2();
                                    let th = tw * ts.y / ts.x;
                                    let img = egui::Image::new(egui::load::SizedTexture::new(p.thumb.id(), Vec2::new(tw, th.min(72.0))))
                                        .corner_radius(CornerRadius::same(RADIUS))
                                        .tint(if p.enabled { Color32::WHITE } else { DIMMED_TINT })
                                        .sense(Sense::click());
                                    menu_resp.push((i, ui.add(img)));
                                    ui.vertical(|ui| {
                                        menu_resp.push((i, ui.add(egui::Label::new(RichText::new(format!("{}. {}", i + 1, p.name)).strong().color(Color32::WHITE)).sense(Sense::click()))));
                                        ui.label(RichText::new(format!("{} × {}", p.w, p.h)).small().color(MUTED));
                                        if menu_state.has_mask {
                                            ui.label(RichText::new("🖌 has masks").small().color(MASK_GREEN));
                                        }
                                        if self.matched {
                                            let (t, c) = if !p.enabled {
                                                ("left out".to_string(), MUTED)
                                            } else if connected {
                                                (format!("{pts} points"), GOOD)
                                            } else {
                                                ("no overlap found".to_string(), BAD)
                                            };
                                            ui.label(RichText::new(t).small().color(c));
                                        }
                                        ui.horizontal(|ui| {
                                            // a running job (alignment, point search) works with the photos in use at its start
                                            if ui
                                                .add_enabled(!busy, egui::Checkbox::new(&mut p.enabled, RichText::new("use").small()))
                                                .on_disabled_hover_text("Wait until the running job is finished (or cancel it).")
                                                .changed()
                                            {
                                                toggled = true;
                                            }
                                            if ui.small_button("🗑").on_hover_text("Remove this photo").clicked() {
                                                remove = Some(i);
                                            }
                                        });
                                    });
                                });
                            });
                        ui.add_space(2.0);
                    }
                });
                let chosen: Vec<(usize, super::photo_menu::PhotoAction)> = menu_resp.iter().filter_map(|(i, r)| self.photo_menu(r, *i)).collect();
                if toggled {
                    self.needs_align = true;
                    self.preview_dirty = true;
                }
                if remove.is_none() {
                    for (i, a) in chosen {
                        self.apply_photo_action(i, a);
                    }
                }
                if let Some(i) = remove
                    && !self.busy() {
                        self.remove_photo(i);
                    }
            });
    }

    pub(super) fn pick_photos(&mut self) {
        let files = rfd::FileDialog::new()
            .add_filter("Photos", &["jpg", "jpeg", "png", "tif", "tiff", "dng", "cr2", "cr3", "nef", "arw", "orf", "rw2", "raf", "pef", "srw"])
            .add_filter("All files", &["*"])
            .pick_files();
        if let Some(f) = files {
            self.add_paths(f);
        }
    }

    // ----- step 1 -----
    pub(super) fn ui_photos(&mut self, ui: &mut egui::Ui) {
        if self.photos.is_empty() {
            let avail = ui.available_size();
            ui.add_space((avail.y * 0.12).max(10.0));
            ui.vertical_centered(|ui| {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(avail.x.min(720.0), 300.0), Sense::click());
                let p = ui.painter();
                let hovered = resp.hovered() || ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
                p.rect_filled(rect, f32::from(RADIUS_CARD), if hovered { ACCENT_SOFT } else { FAINT });
                dashed_rect(p, rect.shrink(6.0), if hovered { ACCENT } else { BORDER_STRONG });
                // illustration: three overlapping frames
                let c = rect.center() - Vec2::new(0.0, 60.0);
                for (k, dx) in [-46.0f32, 0.0, 46.0].iter().enumerate() {
                    let r = Rect::from_center_size(c + Vec2::new(*dx, (k as f32 - 1.0).abs() * 4.0), Vec2::new(70.0, 50.0));
                    p.rect_filled(r, f32::from(RADIUS), [ACCENT_DARK, ACCENT_MID, ACCENT_DARK][k]);
                    p.rect_stroke(r, f32::from(RADIUS), Stroke::new(2.0, ACCENT), StrokeKind::Inside);
                }
                p.text(rect.center() + Vec2::new(0.0, 10.0), Align2::CENTER_CENTER, "Drop your photos here", FontId::proportional(24.0), Color32::WHITE);
                p.text(
                    rect.center() + Vec2::new(0.0, 42.0),
                    Align2::CENTER_CENTER,
                    "or click to choose them  ·  JPEG, PNG, TIFF and camera RAW",
                    FontId::proportional(15.0),
                    MUTED,
                );
                p.text(
                    rect.center() + Vec2::new(0.0, 74.0),
                    Align2::CENTER_CENTER,
                    "Tip: let neighbouring photos overlap by about a third.",
                    FontId::proportional(13.0),
                    MUTED,
                );
                if resp.clicked() && !self.busy() {
                    self.pick_photos();
                }
                ui.add_space(18.0);
                ui.horizontal_wrapped(|ui| {
                    let w = 2.0 * 230.0 + GAP;
                    ui.add_space((ui.available_width() - w).max(0.0) / 2.0);
                    if ui.add_enabled(!self.busy(), egui::Button::new(RichText::new("🖼  Try it with sample photos").size(BIG_FONT)).min_size(Vec2::new(230.0, BIG_H))).clicked() {
                        self.load_sample();
                    }
                    if ui.add_enabled(!self.busy(), egui::Button::new(RichText::new("📂  Open a saved project").size(BIG_FONT)).min_size(Vec2::new(230.0, BIG_H))).clicked()
                        && let Some(p) = rfd::FileDialog::new().add_filter("PanoWeave project", &["pwproj"]).pick_file() {
                            self.open_project(&p);
                        }
                });
                ui.add_space(26.0);
                ui.horizontal_wrapped(|ui| {
                    let w = 3.0 * 230.0 + 2.0 * GAP;
                    ui.add_space((ui.available_width() - w).max(0.0) / 2.0);
                    for (t, d) in [
                        ("1 · Add photos", "Shoot from one spot, turning the camera."),
                        ("2 · Create panorama", "One click finds points, aligns and frames."),
                        ("3 · Fine-tune & export", "Adjust the frame, fix points if needed."),
                    ] {
                        egui::Frame::new().fill(CARD).corner_radius(CornerRadius::same(RADIUS_CARD)).inner_margin(egui::Margin::same(CARD_MARGIN)).show(ui, |ui| {
                            ui.set_width(230.0 - 2.0 * CARD_MARGIN as f32);
                            ui.vertical(|ui| {
                                ui.label(RichText::new(t).strong().color(Color32::WHITE));
                                ui.label(RichText::new(d).small().color(MUTED));
                            });
                        });
                    }
                });
            });
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.heading(format!("{} photos", self.photos.len()));
            ui.add_space(10.0);
            if ui.add_enabled(!self.busy(), egui::Button::new("➕ Add more")).clicked() {
                self.pick_photos();
            }
            if ui.add_enabled(!self.busy() && self.photos.len() >= 2, egui::Button::new("🔍  Find points only")).clicked() {
                self.run_match(false);
            }
        });
        ui.label(RichText::new("Everything is ready. Press ✨ Create panorama (top right) for the automatic way, or go through the steps yourself.").color(MUTED));
        ui.add_space(6.0);
        // lenses
        card(ui, |ui| {
            ui.label(RichText::new("Lens").strong());
            ui.label(RichText::new("The field of view is read from the photo information when possible; otherwise it is measured automatically during alignment.").small().color(MUTED));
            let mut changed = false;
            for (li, lens) in self.lenses.iter_mut().enumerate() {
                let users: Vec<String> = self.photos.iter().enumerate().filter(|(_, p)| p.lens == li).map(|(i, _)| (i + 1).to_string()).collect();
                if users.is_empty() {
                    continue;
                }
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("Lens {} (photos {}):", li + 1, users.join(", ")));
                    let mut deg = lens.hfov_deg();
                    if ui.add(egui::DragValue::new(&mut deg).range(1.0..=170.0).speed(0.2).suffix("° wide").max_decimals(1)).changed() {
                        *lens = Lens { f: Lens::from_hfov_deg(deg).f, k1: lens.k1, from_exif: true };
                        changed = true;
                    }
                    ui.label(
                        RichText::new(if lens.from_exif { "fixed / from photo info" } else { "will be measured" }).small().color(MUTED),
                    );
                    if lens.from_exif && ui.small_button("measure instead").clicked() {
                        lens.from_exif = false;
                        changed = true;
                    }
                });
            }
            if changed {
                self.needs_align = true;
            }
        });
        ui.add_space(8.0);
        let mut menu_resp: Vec<(usize, egui::Response)> = Vec::new();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // a grid: `horizontal_wrapped` does not wrap frames (their size is only known after drawing),
            // so the tiles are put into rows of as many as fit the width
            let per_row = tiles_per_row(ui.available_width(), PHOTO_TILE_W + 2.0 * f32::from(TILE_MARGIN), ui.spacing().item_spacing.x);
            let idx: Vec<usize> = (0..self.photos.len()).collect();
            for row in idx.chunks(per_row) {
                ui.horizontal(|ui| {
                for &i in row {
                    let p = &self.photos[i];
                    egui::Frame::new().fill(CARD).corner_radius(CornerRadius::same(RADIUS_CARD)).inner_margin(egui::Margin::same(TILE_MARGIN)).show(ui, |ui| {
                        ui.vertical(|ui| {
                            let s = p.thumb.size_vec2();
                            let w = PHOTO_TILE_W;
                            let tint = if p.enabled { Color32::WHITE } else { DIMMED_TINT };
                            let img = egui::Image::new(egui::load::SizedTexture::new(p.thumb.id(), Vec2::new(w, w * s.y / s.x))).corner_radius(CornerRadius::same(RADIUS)).tint(tint).sense(Sense::click());
                            menu_resp.push((i, ui.add(img)));
                            let name = if p.mask.any() { format!("{}. {}  🖌", i + 1, p.name) } else { format!("{}. {}", i + 1, p.name) };
                            ui.label(RichText::new(name).small());
                            let mut info = format!("{} × {}", p.w, p.h);
                            if let Some(f) = p.info.focal_mm {
                                info += &format!(" · {f:.0} mm");
                            }
                            if let Some(t) = p.info.exposure_time {
                                info += &if t < 1.0 { format!(" · 1/{:.0} s", 1.0 / t) } else { format!(" · {t:.1} s") };
                            }
                            ui.label(RichText::new(info).small().color(MUTED));
                        });
                    });
                }
                });
            }
        });
        let chosen: Vec<_> = menu_resp.iter().filter_map(|(i, r)| self.photo_menu(r, *i)).collect();
        for (i, a) in chosen {
            self.apply_photo_action(i, a);
        }
    }

    // ----- step 2 -----
    pub(super) fn ensure_tex(&mut self, ctx: &egui::Context, i: usize) {
        if self.photos[i].tex.is_none() {
            let img = color_image(&self.photos[i].work);
            self.photos[i].tex = Some(ctx.load_texture(format!("work-{i}-{}", self.photos[i].name), img, egui::TextureOptions::LINEAR));
        }
    }
}
