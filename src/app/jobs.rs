use super::*;

impl PanoApp {
    pub(super) fn poll(&mut self, ctx: &egui::Context) {
        if let Some(job) = &self.job {
            if let Ok(msg) = job.rx.try_recv() {
                let elapsed = job.started.elapsed().as_millis();
                let title = job.title.clone();
                let was_cancelled = job.cancel.load(Ordering::Relaxed) || engine::cancelled();
                self.job = None;
                engine::CANCEL.store(false, Ordering::Relaxed);
                let msg = if was_cancelled && !matches!(msg, JobMsg::Rendered(_) | JobMsg::FusedSaved(_)) { JobMsg::Cancelled } else { msg };
                match msg {
                    JobMsg::Cancelled => {
                        self.auto_chain = false;
                        if title.starts_with("Loading") {
                            // the original's wording
                            self.set_status("Loading canceled – no photos were added.", WARN);
                        } else {
                            self.set_status(format!("{title}: canceled – nothing was changed."), WARN);
                        }
                    }
                    JobMsg::Loaded(r) => self.on_loaded(ctx, r),
                    JobMsg::Matched(m) => self.on_matched(m),
                    JobMsg::Aligned(a) => self.on_aligned(a),
                    JobMsg::Rendered(r) => match r {
                        Ok((p, ms, w, h)) => {
                            self.set_status(format!("Panorama exported ({w} × {h} px) in {}: {}", fmt_ms(ms), p.display()), GOOD);
                            self.last_output = Some(p);
                            // the patch comparison data are released after the export
                            crate::patch_store::STORE.free_after_export();
                        }
                        Err(e) if e == stitch::CANCELLED => self.set_status("Export canceled – no file was written.", WARN),
                        Err(e) => self.set_status(format!("{title} stopped: {e}"), BAD),
                    },
                    JobMsg::BracketLoaded(r) => {
                        let mut bad = 0;
                        for x in r {
                            match x {
                                Ok((name, source, work, thumb, info)) => {
                                    let t = ctx.load_texture(format!("br-{}-{name}", self.brackets.len()), color_image(&thumb), egui::TextureOptions::LINEAR);
                                    self.brackets.push(Bracket { name, source, work, thumb: t, info });
                                }
                                Err(_) => bad += 1,
                            }
                        }
                        // darkest first
                        self.brackets.sort_by(|a, b| {
                            let ea = a.info.brightness_ev().unwrap_or_else(|| mean_luma(&a.work));
                            let eb = b.info.brightness_ev().unwrap_or_else(|| mean_luma(&b.work));
                            ea.partial_cmp(&eb).unwrap_or(std::cmp::Ordering::Equal)
                        });
                        self.fusion_dirty = true;
                        self.set_status(
                            if bad == 0 { format!("{} exposures ready to blend.", self.brackets.len()) } else { format!("{bad} photos could not be read.") },
                            if bad == 0 { GOOD } else { WARN },
                        );
                    }
                    JobMsg::SkyMasks(v) => self.on_sky_masks(v),
                    JobMsg::PairPoints { a, b, cps, ms, mode } => self.on_pair_points(a, b, cps, ms, mode),
                    JobMsg::PairChecked { a, b, cps, reports } => self.on_pair_checked(a, b, cps, reports),
                    JobMsg::ClickMatched(r) => self.on_click_matched(*r),
                    JobMsg::FusedSaved(r) => match r {
                        Ok((p, ms)) => {
                            self.set_status(format!("Blended picture exported in {}: {}", fmt_ms(ms), p.display()), GOOD);
                            self.last_output = Some(p);
                        }
                        Err(e) => self.set_status(format!("Blending stopped: {e}"), BAD),
                    },
                }
                let _ = elapsed;
            }
        }
        if let Some(rx) = &self.preview_rx {
            if let Ok((generation, img, view)) = rx.try_recv() {
                self.preview_rx = None;
                self.preview_size = img.size;
                match &mut self.preview_tex {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => self.preview_tex = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
                }
                self.preview_shown_gen = generation;
                self.preview_view = Some(view);
            }
        }
        while let Ok((i, name, img)) = self.full_rx.try_recv() {
            self.full_pending.remove(&i);
            // only if the photo at that place is still the same one
            if self.photos.get(i).map(|p| p.name == name).unwrap_or(false) {
                self.full_tex.insert(i, ctx.load_texture(format!("full-{i}-{name}"), img, egui::TextureOptions::LINEAR));
            }
        }
        if let Some(rx) = &self.fusion_rx {
            if let Ok(img) = rx.try_recv() {
                self.fusion_rx = None;
                match &mut self.fusion_tex {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => self.fusion_tex = Some(ctx.load_texture("fusion", img, egui::TextureOptions::LINEAR)),
                }
            }
        }
        if self.fusion_dirty && self.mode == Mode::Exposure {
            self.request_fusion_preview();
        }
    }

    pub(super) fn request_preview(&mut self, max_w: usize, fast: bool) {
        if self.preview_rx.is_some() || !self.aligned {
            return;
        }
        let rp = self.render_photos(true);
        if rp.is_empty() {
            return;
        }
        let view = self.view.clone();
        let aspect = view.aspect().clamp(0.05, 40.0);
        let mut w = if fast { max_w.min(700) } else { max_w.min(1800) };
        let mut h = (w as f64 / aspect).round() as usize;
        if h > 1100 {
            h = 1100;
            w = (h as f64 * aspect) as usize;
        }
        let (w, h) = (w.max(16), h.max(8));
        self.preview_gen += 1;
        let generation = self.preview_gen;
        let prefer_detail = self.prefer_detail;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let px = stitch::render_preview(&rp, &view, w, h, true, prefer_detail);
            let _ = tx.send((generation, egui::ColorImage::from_rgba_unmultiplied([w, h], &px), view));
        });
        self.preview_rx = Some(rx);
        self.preview_dirty = false;
    }

    pub(super) fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            self.add_paths(dropped);
        }
    }
}
