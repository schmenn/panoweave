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
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self { enabled: false, size_mode: SizeMode::Percent, target_width: DEFAULT_TARGET_WIDTH, tiff_16bit: false }
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
    if let Some(d) = path.parent() {
        if !d.as_os_str().is_empty() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
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

/// 16 bit per channel TIFF (RGBA); takes the buffer to avoid a second copy.
pub(super) fn save_tiff16(path: &Path, rgba: Vec<u16>, w: usize, h: usize, stop: &dyn Fn() -> bool) -> Result<(), String> {
    write_atomically(path, stop, |tmp| {
        let buf = image::ImageBuffer::<image::Rgba<u16>, Vec<u16>>::from_raw(w as u32, h as u32, rgba).ok_or("wrong buffer size for the 16-bit picture")?;
        image::DynamicImage::ImageRgba16(buf).save_with_format(tmp, image::ImageFormat::Tiff).map_err(|e| e.to_string())
    })
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
        let (w, h) = self.export_size();
        // the 16 bit copy for TIFF (8 B per pixel) comes on top of the 8 bit one (4 B)
        let extra = if self.export_opts.sixteen_bit(self.out_format) { 4 * (w as u64) * (h as u64) } else { 0 };
        (w, h, stitch::estimate_bytes(w, h, self.blend) + extra, crate::sysmem::available())
    }

    /// Width and height in pixels of the panorama file at the chosen size.
    pub(super) fn export_size(&self) -> (usize, usize) {
        let cb = self.view.crop_bounds();
        let asp = (cb[1] - cb[0]) / (cb[3] - cb[2]);
        let w = output_width(self.natural_width(), self.out_scale, &self.export_opts);
        (w, ((w as f64 / asp).round() as usize).max(16))
    }

    /// Largest value of the size control (percent, or pixels of width) that fits into `usable` bytes.
    fn fit_size(&self, need: u64, usable: f64) -> f64 {
        let cur = if self.export_opts.width_mode() { self.export_size().0 as f64 } else { self.out_scale as f64 };
        (cur * (usable / need as f64).sqrt()).floor().max(1.0)
    }

    pub(super) fn run_render(&mut self) {
        if self.busy() || !self.aligned {
            return;
        }
        let (_, _, need, free) = self.render_memory();
        if let Some(free) = free {
            let usable = (free as f64 * RENDER_MEMORY_SHARE) as u64;
            if need > usable {
                let fit = self.fit_size(need, usable as f64);
                let unit = if self.export_opts.width_mode() { "px width" } else { "%" };
                self.set_status(
                    format!(
                        "This size needs about {:.1} GB of memory, but only {:.1} GB are free. Choose at most {:.0} {unit} (or close other programs).",
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
        let out_w = self.export_size().0;
        let blend = self.blend;
        let render_opts = stitch::RenderOptions { levels: self.blend_levels, cubic: self.cubic_interp, prefer_detail: self.prefer_detail };
        let fmt = self.out_format;
        let q = self.jpeg_quality;
        let sixteen = self.export_opts.sixteen_bit(fmt);
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
            // the job's own flag and the global one are both set by "Cancel"
            let stop = || cancel.load(Ordering::Relaxed) || engine::cancelled();
            let res = r.and_then(|(col, cov, w, h)| {
                if stop() {
                    return Err(stitch::CANCELLED.into());
                }
                report(0.97, "Converting the colours");
                if sixteen {
                    let rgba = stitch::to_rgba16(&col, &cov);
                    drop((col, cov));
                    report(0.98, "Saving the file");
                    save_tiff16(&path, rgba, w, h, &stop)
                } else {
                    let rgba = stitch::to_rgba8(&col, &cov);
                    drop((col, cov));
                    report(0.98, "Saving the file");
                    save_output_stoppable(&path, &rgba, w, h, fmt, q, &stop)
                }
                .map(|_| (path.clone(), t0.elapsed().as_millis(), w, h))
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
                    ui.checkbox(&mut self.export_opts.enabled, "More size and file options")
                        .on_hover_text("Off: size from the slider below, files as before. On: choose a width in pixels and 16-bit TIFF.");
                    if self.export_opts.enabled {
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut self.export_opts.size_mode, SizeMode::Percent, "Percent of full detail");
                            ui.radio_value(&mut self.export_opts.size_mode, SizeMode::Width, "Width in pixels");
                        });
                    }
                    if self.export_opts.width_mode() {
                        ui.horizontal(|ui| {
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
                    {
                        let (_, _, need, free) = self.render_memory();
                        match free {
                            Some(free) if need as f64 > free as f64 * RENDER_MEMORY_SHARE => {
                                let fit = self.fit_size(need, free as f64 * RENDER_MEMORY_SHARE);
                                let unit = if self.export_opts.width_mode() { "px" } else { "%" };
                                ui.label(
                                    RichText::new(format!(
                                        "Needs about {:.1} GB of memory, only {:.1} GB free – choose at most {fit:.0} {unit}.",
                                        crate::sysmem::gb(need),
                                        crate::sysmem::gb(free),
                                    ))
                                    .color(BAD),
                                );
                                if ui.button(format!("Use {fit:.0} {unit}")).clicked() {
                                    if self.export_opts.width_mode() {
                                        self.export_opts.target_width = fit as u32;
                                    } else {
                                        self.out_scale = fit as f32;
                                    }
                                }
                            }
                            Some(free) => {
                                ui.label(RichText::new(format!("needs about {:.1} GB of memory ({:.1} GB free)", crate::sysmem::gb(need), crate::sysmem::gb(free))).small().color(MUTED));
                            }
                            None => {}
                        }
                    }
                    if !self.export_opts.width_mode() {
                        ui.horizontal(|ui| {
                            for (t, v) in [("Small", 25.0), ("Medium", 50.0), ("Full", 100.0)] {
                                if ui.button(t).clicked() {
                                    self.out_scale = v;
                                }
                            }
                        });
                    }
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
                    if self.export_opts.enabled && self.out_format == OutFormat::Tiff {
                        ui.checkbox(&mut self.export_opts.tiff_16bit, "16 bits per colour channel (much larger file)");
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
        let o = ExportOptions { enabled: false, size_mode: SizeMode::Width, target_width: 999, tiff_16bit: true };
        assert_eq!(output_width(4000.0, 50.0, &o), 2000);
        assert!(!o.sixteen_bit(OutFormat::Tiff));
    }

    #[test]
    fn target_width_is_used_and_clamped() {
        let mut o = ExportOptions { enabled: true, size_mode: SizeMode::Width, target_width: 3000, tiff_16bit: true };
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
    fn tiff16_roundtrip_keeps_16_bits() {
        let d = tmp_dir("t16");
        let p = d.join("x.tif");
        let px: Vec<u16> = (0..4u16).flat_map(|i| [i * 1000 + 1, 40000 + i, 65535, 65535]).collect();
        save_tiff16(&p, px.clone(), 2, 2, &|| false).unwrap();
        let im = image::open(&p).unwrap().into_rgba16();
        assert_eq!(im.as_raw(), &px);
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
        let r = save_output_stoppable(&p, &vec![0u8; 4 * 4 * 4], 4, 4, OutFormat::Png, 90, &|| {
            calls.set(calls.get() + 1);
            calls.get() >= 2
        });
        assert_eq!(r, Err(stitch::CANCELLED.to_string()));
        assert_eq!(std::fs::read(&p).unwrap(), before);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
