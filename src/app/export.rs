use super::*;

/// How the size of the exported file is chosen (only used when `ExportOptions::enabled`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SizeMode {
    /// percentage of the full detail of the photos (the original slider)
    Percent,
    /// fixed width in pixels
    Width,
}

/// Smallest / largest accepted target width in pixels and the width offered first.
pub(super) const MIN_TARGET_WIDTH: u32 = 64;
pub(super) const MAX_TARGET_WIDTH: u32 = 60_000;
pub(super) const DEFAULT_TARGET_WIDTH: u32 = 8000;

/// Extra export options. With `enabled = false` (the default) the export behaves as before:
/// size from the percent slider, 8 bit per channel.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ExportOptions {
    pub enabled: bool,
    pub size_mode: SizeMode,
    pub target_width: u32,
    /// TIFF only: 16 bits per channel instead of 8
    pub tiff_16bit: bool,
    /// find the seams between the photos with the seam finder (on by default; around poles, lamps and people that
    /// the photos see from different sides); off = the photo with the best score wins at every pixel
    pub smart_seams: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self { enabled: false, size_mode: SizeMode::Percent, target_width: DEFAULT_TARGET_WIDTH, tiff_16bit: false, smart_seams: true }
    }
}

impl ExportOptions {
    pub fn width_mode(&self) -> bool {
        self.enabled && self.size_mode == SizeMode::Width
    }
    pub fn sixteen_bit(&self, fmt: OutFormat) -> bool {
        self.enabled && self.tiff_16bit && fmt == OutFormat::Tiff
    }
}

/// Output width in pixels for the chosen options (`natural` = width at 100 %).
pub(super) fn output_width(natural: f64, scale_pct: f32, o: &ExportOptions) -> usize {
    if o.width_mode() {
        o.target_width.clamp(MIN_TARGET_WIDTH, MAX_TARGET_WIDTH) as usize
    } else {
        ((natural * scale_pct as f64 / 100.0).round() as usize).max(16)
    }
}

/// Write via `write(temp_path)` into a hidden temporary file next to `path` and move it into
/// place only when finished and not cancelled, so a stop never leaves a half-written picture.
fn write_atomically(path: &Path, stop: &dyn Fn() -> bool, write: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), String> {
    if let Some(d) = path.parent()
        && !d.as_os_str().is_empty() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
    if stop() {
        return Err(stitch::CANCELLED.into());
    }
    // keep the extension last: the encoder picks the file format from it
    let stem = path.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "panorama".into());
    let tmp = path.with_file_name(format!(".{stem}.partial.{}", path.extension().and_then(|e| e.to_str()).unwrap_or("tmp")));
    let res = write(&tmp).and_then(|_| if stop() { Err(stitch::CANCELLED.into()) } else { std::fs::rename(&tmp, path).map_err(|e| format!("could not save {}: {e}", path.display())) });
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

pub(super) fn save_output(path: &Path, rgba: &[u8], w: usize, h: usize, fmt: OutFormat, quality: u8) -> Result<(), String> {
    save_output_stoppable(path, rgba, w, h, fmt, quality, &|| false)
}

/// Like `save_output`, but a cancel request (`stop`) before the file is complete leaves no file.
pub(super) fn save_output_stoppable(path: &Path, rgba: &[u8], w: usize, h: usize, fmt: OutFormat, quality: u8, stop: &dyn Fn() -> bool) -> Result<(), String> {
    write_atomically(path, stop, |tmp| match fmt {
        OutFormat::Jpeg => {
            let rgb: Vec<u8> = rgba.chunks(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
            let f = std::fs::File::create(tmp).map_err(|e| e.to_string())?;
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(f), quality);
            enc.encode(&rgb, w as u32, h as u32, image::ExtendedColorType::Rgb8).map_err(|e| e.to_string())
        }
        OutFormat::Png | OutFormat::Tiff => image::save_buffer(tmp, rgba, w as u32, h as u32, image::ColorType::Rgba8).map_err(|e| e.to_string()),
    })
}

/// What `run_render` keeps per photo: source, small copy, size, rotation, lens, gain, parallax field, trusted, mask, gain map, lens model.
pub(super) type ExportSrc = (Source, Arc<RgbImage>, u32, u32, M3, Lens, [f32; 3], Option<Arc<fine_align::DisplacementField>>, bool, Option<Arc<crate::masks::MaskBuf>>, Option<Arc<GainMap>>, Option<crate::lensphoto::LensModel>);

/// The photo as the renderer gets it (with its mask), `img` being the picture to sample from.
pub(super) fn render_photo_of(s: &ExportSrc, img: Arc<RgbImage>) -> RenderPhoto {
    let (_, _, w, h, rot, lens, gain, field, trusted, mask, gain_map, photometry) = s;
    RenderPhoto { img, w: *w as f64, h: *h as f64, rot: *rot, lens: *lens, gain: *gain, gain_map: gain_map.clone(), photometry: *photometry, field: field.clone(), full_long: (*w).max(*h) as f64, trusted: *trusted, mask: mask.clone() }
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

    /// Width and height in pixels of the panorama file at the chosen size.
    pub(super) fn export_size(&self) -> (usize, usize) {
        let cb = self.view.crop_bounds();
        let asp = (cb[1] - cb[0]) / (cb[3] - cb[2]);
        let w = output_width(self.natural_width(), self.out_scale, &self.export_opts);
        (w, ((w as f64 / asp).round() as usize).max(16))
    }

    /// Memory the export may use in total (strips plus photo cache), if known.
    fn strip_budget(&self) -> Option<u64> {
        // experiments: PW_EXPORT_MB=<megabytes> sets the budget by hand
        let manual = std::env::var("PW_EXPORT_MB").ok().and_then(|v| v.parse::<u64>().ok()).map(|mb| mb << 20);
        crate::strip::budget_bytes(crate::sysmem::available(), manual, &crate::strip::StripConfig::default())
    }

    /// Plan of the export at the chosen size: strip height, number of strips, memory per strip.
    /// The picture is the same whatever the plan is; less memory only means more strips.
    pub(super) fn strip_estimate(&self) -> Option<Result<crate::strip::Fit, String>> {
        let cfg = crate::strip::StripConfig::default();
        let (w, h) = self.export_size();
        let levels = if self.blend_levels > 0 { self.blend_levels.min(stitch::auto_levels(w, h)) } else { stitch::auto_levels(w, h) };
        Some(crate::strip::fit(w, h, levels, self.blend, self.strip_budget()?, &cfg))
    }

    /// The file format of the strip writers for the chosen output.
    fn write_format(&self) -> crate::strip_write::Format {
        match self.out_format {
            OutFormat::Jpeg => crate::strip_write::Format::Jpeg { quality: self.jpeg_quality },
            OutFormat::Png => crate::strip_write::Format::Png,
            OutFormat::Tiff => crate::strip_write::Format::Tiff { sixteen_bit: self.export_opts.sixteen_bit(self.out_format) },
        }
    }

    /// Where the file will be written: the chosen path, with the extension of the format when it has none or
    /// another one (a JPEG must not end up in "x.png"; the dialog shows the name that is used).
    pub(super) fn target_path(&self) -> PathBuf {
        let path = PathBuf::from(self.out_path.trim());
        match path.extension().and_then(|e| e.to_str()) {
            Some(e) if self.out_format.has_ext(e) => path,
            _ => path.with_extension(self.out_format.ext()),
        }
    }

    /// (estimated file size, free space on the drive of the target) in bytes.
    pub(super) fn disk_estimate(&self) -> (u64, Option<u64>) {
        let (w, h) = self.export_size();
        (crate::strip_write::estimated_file_bytes(w, h, self.write_format()), crate::sysmem::disk_free(&self.target_path()))
    }

    /// The active photos as the export needs them (a copy that the worker thread owns).
    pub(super) fn export_sources(&self) -> Vec<ExportSrc> {
        let active = self.active_indices();
        let trusted = self.connected_photos();
        // experiments: PW_GAINS=0 switches the brightness matching off
        let gains_on = self.match_brightness && std::env::var("PW_GAINS").map(|v| v != "0").unwrap_or(true);
        active
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
                    if gains_on { self.gains[i] } else { [1.0; 3] },
                    self.fields.get(i).cloned().flatten(),
                    trusted[i],
                    p.mask.buf.clone(),
                    if gains_on { self.gain_map_of(i) } else { None },
                    if gains_on { self.photometry_of(i) } else { None },
                )
            })
            .collect()
    }

    pub(super) fn run_render(&mut self) {
        if self.busy() || !self.aligned {
            return;
        }
        if self.out_path.trim().is_empty() {
            self.out_path = self.default_out_path("panorama", self.out_format.ext());
        }
        let path = self.target_path();
        let fmt = self.out_format;
        let (ew, eh) = self.export_size();
        if fmt == OutFormat::Jpeg && (ew > crate::strip_write::JPEG_MAX_SIDE || eh > crate::strip_write::JPEG_MAX_SIDE) {
            self.set_status(format!("JPEG files can be at most {} pixels wide or high (this picture is {ew} x {eh}) – choose TIFF or a smaller size.", crate::strip_write::JPEG_MAX_SIDE), BAD);
            return;
        }
        let (file_bytes, disk_free) = self.disk_estimate();
        if let Some(free) = disk_free
            && file_bytes > free {
                self.set_status(
                    format!("The file needs up to {:.1} GB, but only {:.1} GB are free on the drive of {}. Choose a smaller size or another folder.", crate::sysmem::gb(file_bytes), crate::sysmem::gb(free), path.display()),
                    BAD,
                );
                return;
            }
        let sources = self.export_sources();
        let view = self.view.clone();
        let out_w = ew;
        let blend = self.blend;
        let render_opts = stitch::RenderOptions { levels: std::env::var("PW_LEVELS").ok().and_then(|v| v.parse().ok()).unwrap_or(self.blend_levels), cubic: self.cubic_interp, prefer_detail: self.prefer_detail, seams: crate::seams::SeamConfig::for_export(self.export_opts.smart_seams) };
        let format = self.write_format();
        let cfg = crate::strip::StripConfig::default();
        let total = self.strip_budget().unwrap_or(FALLBACK_EXPORT_BUDGET);
        let (_, cache_bytes) = crate::strip::split_budget(total, &cfg);
        // The picture is made strip by strip. The strip height only follows from the memory that
        // may be used; it never changes a single pixel, so the result is the same on every computer.
        self.start_job("Creating the panorama", move |report, cancel| {
            let t0 = Instant::now();
            // memory of the program when the export starts and its highest value during the export (sampled), to check the plan
            let mem0 = crate::sysmem::process().map(|p| p.0).unwrap_or(0);
            let mem_max = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(mem0));
            let mem_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let sampler = {
                let (mx, st) = (mem_max.clone(), mem_stop.clone());
                std::thread::spawn(move || {
                    while !st.load(Ordering::Relaxed) {
                        if let Some((now, _)) = crate::sysmem::process() {
                            mx.fetch_max(now, Ordering::Relaxed);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                })
            };
            let cache = Mutex::new(crate::photo_cache::PhotoCache::new(cache_bytes));
            let loader = |i: usize, pixels: bool| -> Result<RenderPhoto, String> {
                // seams only need the geometry plus where the photo is blown out: the small copy is enough
                let img = if pixels { cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_or_load(i, || sources[i].0.load_full())? } else { sources[i].1.clone() };
                Ok(render_photo_of(&sources[i], img))
            };
            let prog = stitch::Progress { report, cancel };
            // the job's own flag and the global one are both set by "Cancel"
            let stop = || cancel.load(Ordering::Relaxed) || engine::cancelled();
            let res = (|| {
                // experiments: PW_OLD_EXPORT=1 renders the whole picture in memory (the earlier way), to compare
                if std::env::var("PW_OLD_EXPORT").is_ok_and(|v| v == "1") && !matches!(format, crate::strip_write::Format::Tiff { sixteen_bit: true }) {
                    let (col, cov, w, h) = stitch::render_final(sources.len(), &loader, &view, out_w, blend, &render_opts, &prog)?;
                    let rgba = stitch::to_rgba8(&col, &cov);
                    let out_fmt = match format {
                        crate::strip_write::Format::Jpeg { .. } => OutFormat::Jpeg,
                        crate::strip_write::Format::Png => OutFormat::Png,
                        crate::strip_write::Format::Tiff { .. } => OutFormat::Tiff,
                    };
                    let q = match format {
                        crate::strip_write::Format::Jpeg { quality } => quality,
                        _ => 90,
                    };
                    save_output_stoppable(&path, &rgba, w, h, out_fmt, q, &stop)?;
                    return Ok((path.clone(), t0.elapsed().as_millis(), w, h));
                }
                let job = stitch::prepare_strips(sources.len(), &loader, &view, out_w, blend, &render_opts, &cfg, total, &prog)?;
                // when the memory is tight the planner takes memory from the photo cache and, if needed, lowers the blend levels
                cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).set_capacity(job.cache_bytes);
                if job.levels_used < job.levels_wanted {
                    eprintln!("export: blend levels lowered from {} to {} to fit into the memory ({:.2} GB for the whole export)", job.levels_wanted, job.levels_used, crate::sysmem::gb(total));
                }
                let wo = crate::strip_write::WriteOptions::new(format, job.rows);
                // the unfinished file is written next to the final one (hidden, ".<name>.partial.<ext>")
                write_atomically(&path, &stop, |tmp| {
                    crate::strip_write::write_file(tmp, job.w, job.h, &wo, |sink| job.run(&loader, &mut |y0, c, v, _| sink.push(y0, c, v), &prog))
                })?;
                let c = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                eprintln!(
                    "export: {}x{} px, {} strips of {} rows, planned strip memory {:.2} GB, photo cache {} hits / {} misses ({:.2} GB), peak memory of the program {:.2} GB (at the start {:.2} GB, highest during the export {:.2} GB), {} ms",
                    job.w,
                    job.h,
                    job.count,
                    job.rows,
                    crate::sysmem::gb(job.peak_bytes),
                    c.hits,
                    c.misses,
                    crate::sysmem::gb(c.used()),
                    crate::sysmem::gb(crate::sysmem::process().map(|p| p.1).unwrap_or(0)),
                    crate::sysmem::gb(mem0),
                    crate::sysmem::gb(mem_max.load(Ordering::Relaxed)),
                    t0.elapsed().as_millis()
                );
                Ok((path.clone(), t0.elapsed().as_millis(), job.w, job.h))
            })();
            mem_stop.store(true, Ordering::Relaxed);
            let _ = sampler.join();
            cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
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
        // scrolls when the window is too low for the settings
        egui::ScrollArea::vertical().id_salt("export_scroll").auto_shrink([false, false]).show(ui, |ui| self.ui_export_body(ui));
    }

    fn ui_export_body(&mut self, ui: &mut egui::Ui) {
        let avail_w = ui.available_width();
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width((avail_w * 0.45).max(380.0));
                card(ui, |ui| {
                    ui.label(RichText::new("Size").strong());
                    let nw = self.natural_width();
                    ui.checkbox(&mut self.export_opts.enabled, "More size and file options")
                        .on_hover_text("Off: size from the slider below, files as before. On: choose a width in pixels and 16-bit TIFF.");
                    if self.export_opts.enabled {
                        ui.horizontal_wrapped(|ui| {
                            ui.radio_value(&mut self.export_opts.size_mode, SizeMode::Percent, "Percent of full detail");
                            ui.radio_value(&mut self.export_opts.size_mode, SizeMode::Width, "Width in pixels");
                        });
                    }
                    if self.export_opts.width_mode() {
                        ui.horizontal_wrapped(|ui| {
                            ui.add(egui::DragValue::new(&mut self.export_opts.target_width).range(MIN_TARGET_WIDTH..=MAX_TARGET_WIDTH).suffix(" px"));
                            for (t, v) in [("4000", 4000), ("8000", 8000), ("16000", 16000)] {
                                if ui.button(t).clicked() {
                                    self.export_opts.target_width = v;
                                }
                            }
                        });
                        if self.export_opts.target_width as f64 > nw * 1.001 {
                            ui.label(RichText::new(format!("The photos only hold about {nw:.0} px in this width - a larger file is just enlarged.")).small().color(WARN));
                        }
                    } else {
                        ui.add(egui::Slider::new(&mut self.out_scale, 5.0..=100.0).suffix(" %").text("of full detail"));
                    }
                    let (wi, hi) = self.export_size();
                    let (w, h) = (wi as f64, hi as f64);
                    ui.label(format!("{w:.0} × {h:.0} px  ({:.1} megapixels)", w * h / 1e6));
                    match self.strip_estimate() {
                        Some(Ok(f)) => {
                            let p = f.plan;
                            let free = crate::sysmem::available().map(|f| format!(", {:.1} GB free", crate::sysmem::gb(f))).unwrap_or_default();
                            let how = if p.count == 1 { "in one piece".to_string() } else { format!("in {} strips", p.count) };
                            ui.label(
                                RichText::new(format!("Memory: about {:.1} GB {how} ({free}). The picture is the same whatever the memory.", crate::sysmem::gb(p.peak_bytes)).replace("(, ", "(").replace("( ", "("))
                                    .small()
                                    .color(MUTED),
                            );
                            if f.levels < f.levels_wanted {
                                ui.label(
                                    RichText::new(format!(
                                        "Not enough free memory for {} blend levels at this size: {} are used (the blending zones get narrower, the seams stay where they are). Choose a smaller size or close other programs for the full quality.",
                                        f.levels_wanted, f.levels
                                    ))
                                    .small()
                                    .color(WARN),
                                );
                            }
                        }
                        Some(Err(e)) => {
                            ui.label(RichText::new(e).color(BAD));
                        }
                        None => {}
                    }
                    {
                        let (bytes, disk) = self.disk_estimate();
                        match disk {
                            Some(d) if bytes > d => {
                                ui.label(RichText::new(format!("The file needs up to {:.1} GB, only {:.1} GB are free on the drive.", crate::sysmem::gb(bytes), crate::sysmem::gb(d))).color(BAD));
                            }
                            Some(d) => {
                                ui.label(RichText::new(format!("File: up to {:.1} GB ({:.1} GB free on the drive)", crate::sysmem::gb(bytes), crate::sysmem::gb(d))).small().color(MUTED));
                            }
                            None => {}
                        }
                    }
                    if !self.export_opts.width_mode() {
                        ui.horizontal_wrapped(|ui| {
                            for (t, v) in [("Small", 25.0), ("Medium", 50.0), ("Full", 100.0)] {
                                if ui.button(t).clicked() {
                                    self.out_scale = v;
                                }
                            }
                        });
                    }
                    if w * h > 500e6 {
                        ui.label(RichText::new("Very large – this takes a while and needs a lot of disk space.").small().color(WARN));
                    }
                });
                ui.add_space(6.0);
                card(ui, |ui| {
                    ui.label(RichText::new("File").strong());
                    ui.horizontal_wrapped(|ui| {
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
                    if self.export_opts.enabled && self.out_format == OutFormat::Tiff {
                        ui.checkbox(&mut self.export_opts.tiff_16bit, "16 bits per colour channel (much larger file)");
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.out_path).desired_width(ui.available_width() - 90.0));
                        if ui.button("Choose…").clicked()
                            && let Some(p) = rfd::FileDialog::new()
                                .add_filter(self.out_format.name(), &[self.out_format.ext()])
                                .set_file_name(format!("panorama.{}", self.out_format.ext()))
                                .save_file()
                            {
                                self.out_path = p.to_string_lossy().to_string();
                            }
                    });
                    // say what is written where, before it happens
                    let target = self.target_path();
                    if !self.out_path.trim().is_empty() && target.as_path() != Path::new(self.out_path.trim()) {
                        ui.label(RichText::new(format!("Will be saved as {} (the extension follows the file type).", target.display())).small().color(WARN));
                    }
                    if target.exists() && self.last_output.as_deref() != Some(target.as_path()) {
                        ui.label(RichText::new("A file with this name already exists – it will be replaced.").small().color(WARN));
                    }
                });
                ui.add_space(6.0);
                card(ui, |ui| {
                    ui.label(RichText::new("Blending").strong());
                    ui.radio_value(&mut self.blend, Blend::Smooth, "Smooth seams (recommended) – hides seams at every detail level");
                    ui.radio_value(&mut self.blend, Blend::Feather, "Soft fade – faster, can show ghosting with moving subjects");
                    ui.checkbox(&mut self.match_brightness, "Match brightness and colour between photos");
                    ui.add_enabled_ui(self.match_brightness, |ui| {
                        let r = ui.checkbox(&mut self.gainmap_cfg.enabled, "Smooth brightness correction across each photo").on_hover_text("Corrects brightness gradients, vignetting and automatic exposure inside the photos, so that no photo shows up as a lighter or darker rectangle (for example in the sky).");
                        let l = ui
                            .checkbox(&mut self.lens_cfg.enabled, "Lens correction (response curve + vignetting per lens)")
                            .on_hover_text("Solves one tone curve (per colour) and one vignetting profile for every lens, from all photos taken with it. Helps when photos of two lenses or cameras differ in colour and contrast, or when the photo corners are darker.");
                        if r.changed() || l.changed() {
                            self.update_gains();
                            self.preview_dirty = true;
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
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
                    ui.add_enabled(self.blend == Blend::Smooth, egui::Checkbox::new(&mut self.export_opts.smart_seams, "Smart seams – steer the seams around poles, lamps and people (slower, recommended)"))
                        .on_hover_text("Off (default): the photo with the most detail wins where photos overlap. On: the seams are searched so that they run where the photos agree, so a close object that two photos see from different sides is not cut in two. The export takes some seconds longer. Only for Smooth seams.");
                    self.mask_button(ui);
                });
                ui.add_space(10.0);
                if primary_button(ui, "Export panorama", !self.busy()).clicked() {
                    self.run_render();
                }
                if let Some(p) = self.last_output.clone() {
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(format!("✔ Saved: {}", p.display())).color(GOOD));
                    });
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Open picture").clicked() {
                            let _ = open::that(&p);
                        }
                        if ui.button("Show in folder").clicked()
                            && let Some(d) = p.parent() {
                                let _ = open::that(d);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("panoweave_export_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn defaults_keep_the_old_size_and_format() {
        let o = ExportOptions::default();
        assert!(!o.enabled && !o.width_mode() && !o.sixteen_bit(OutFormat::Tiff));
        assert_eq!(output_width(4000.0, 70.0, &o), 2800);
        // switched off: width settings are ignored
        let o = ExportOptions { enabled: false, size_mode: SizeMode::Width, target_width: 999, tiff_16bit: true, smart_seams: true };
        assert_eq!(output_width(4000.0, 50.0, &o), 2000);
        assert!(!o.sixteen_bit(OutFormat::Tiff));
    }

    #[test]
    fn the_file_extension_follows_the_file_type() {
        let mut app = super::super::masks_tests::app_with_photos(0);
        for (path, fmt, want) in [
            ("C:/x/pano.png", OutFormat::Jpeg, "C:/x/pano.jpg"),
            ("C:/x/pano.JPEG", OutFormat::Jpeg, "C:/x/pano.JPEG"),
            ("C:/x/pano", OutFormat::Tiff, "C:/x/pano.tif"),
            ("C:/x/pano.tiff", OutFormat::Tiff, "C:/x/pano.tiff"),
            ("C:/x/pano.jpg", OutFormat::Png, "C:/x/pano.png"),
        ] {
            app.out_path = path.into();
            app.out_format = fmt;
            assert_eq!(app.target_path(), PathBuf::from(want), "{path} as {fmt:?}");
        }
    }

    #[test]
    fn target_width_is_used_and_clamped() {
        let mut o = ExportOptions { enabled: true, size_mode: SizeMode::Width, target_width: 3000, tiff_16bit: true, smart_seams: true };
        assert_eq!(output_width(4000.0, 10.0, &o), 3000);
        o.target_width = 1;
        assert_eq!(output_width(4000.0, 10.0, &o), MIN_TARGET_WIDTH as usize);
        assert!(o.sixteen_bit(OutFormat::Tiff) && !o.sixteen_bit(OutFormat::Png));
    }

    #[test]
    fn saves_all_formats_and_leaves_no_temp_file() {
        let d = tmp_dir("fmt");
        let rgba: Vec<u8> = (0..16 * 8).flat_map(|i| [(i * 2) as u8, 100, 200, 255]).collect();
        for (fmt, name) in [(OutFormat::Jpeg, "a.jpg"), (OutFormat::Png, "a.png"), (OutFormat::Tiff, "a.tif")] {
            let p = d.join(name);
            save_output(&p, &rgba, 16, 8, fmt, 90).unwrap();
            let im = image::open(&p).unwrap();
            assert_eq!((im.width(), im.height()), (16, 8));
        }
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 3, "only the three pictures, no leftovers");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn jpeg_quality_changes_file_size() {
        let d = tmp_dir("q");
        let rgba: Vec<u8> = (0..64 * 64).flat_map(|i| [(i * 7 % 251) as u8, (i * 13 % 241) as u8, (i % 199) as u8, 255]).collect();
        save_output(&d.join("lo.jpg"), &rgba, 64, 64, OutFormat::Jpeg, 50).unwrap();
        save_output(&d.join("hi.jpg"), &rgba, 64, 64, OutFormat::Jpeg, 100).unwrap();
        assert!(std::fs::metadata(d.join("hi.jpg")).unwrap().len() > std::fs::metadata(d.join("lo.jpg")).unwrap().len());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cancel_leaves_no_file_and_keeps_old_one() {
        let d = tmp_dir("cancel");
        let p = d.join("keep.png");
        let rgba = vec![255u8; 4 * 4 * 4];
        // cancelled before writing
        assert_eq!(save_output_stoppable(&p, &rgba, 4, 4, OutFormat::Png, 90, &|| true), Err(stitch::CANCELLED.to_string()));
        assert!(!p.exists());
        // cancelled after writing (second check): temp removed, previous file untouched
        save_output(&p, &rgba, 4, 4, OutFormat::Png, 90).unwrap();
        let before = std::fs::read(&p).unwrap();
        let calls = std::cell::Cell::new(0);
        let r = save_output_stoppable(&p, &[0u8; 4 * 4 * 4], 4, 4, OutFormat::Png, 90, &|| {
            calls.set(calls.get() + 1);
            calls.get() >= 2
        });
        assert_eq!(r, Err(stitch::CANCELLED.to_string()));
        assert_eq!(std::fs::read(&p).unwrap(), before);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
