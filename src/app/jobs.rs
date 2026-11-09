use super::*;
use super::state::{ready, Ready};

impl PanoApp {
    pub(super) fn poll(&mut self, ctx: &egui::Context) {
        let got = match self.job.as_ref().map(|j| ready(&j.rx)) {
            Some(Ready::Value(m)) => Some(m),
            // the worker ended without a result (it should send one even on a panic; this is the last safety net)
            Some(Ready::Gone) => Some(JobMsg::Failed("the worker stopped without a result".into())),
            _ => None,
        };
        if let Some(msg) = got
            && let Some(job) = &self.job {
                let elapsed = job.started.elapsed().as_millis();
                let title = job.title.clone();
                let was_cancelled = job.cancel.load(Ordering::Relaxed) || engine::cancelled();
                self.job = None;
                engine::CANCEL.store(false, Ordering::Relaxed);
                let msg = if was_cancelled && !matches!(msg, JobMsg::Rendered(_) | JobMsg::FusedSaved(_) | JobMsg::Failed(_)) { JobMsg::Cancelled } else { msg };
                match msg {
                    JobMsg::Failed(e) => {
                        self.auto_chain = false;
                        self.pending_project = None;
                        eprintln!("{title}: failed: {e}");
                        let log = crate::crashlog::log_path().map(|p| format!(" Details: {}", p.display())).unwrap_or_default();
                        self.set_status(format!("{title}: failed with an internal error ({e}) – nothing was changed.{log}"), BAD);
                    }
                    JobMsg::Cancelled => {
                        self.auto_chain = false;
                        if self.pending_project.take().is_some() {
                            // the project was not opened: it must not be applied to photos added later
                            self.project_path = None;
                        }
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
                            match self.mask_hole_note() {
                                Some(n) => self.set_status(format!("Panorama exported ({w} × {h} px) in {}: {}. {n}", fmt_ms(ms), p.display()), WARN),
                                None => self.set_status(format!("Panorama exported ({w} × {h} px) in {}: {}", fmt_ms(ms), p.display()), GOOD),
                            }
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
                    JobMsg::AreaMatched(r) => self.on_area_matched(*r),
                    JobMsg::Snapped(r) => self.on_snapped(*r),
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
        self.poll_gains();
        if self.gains_pending() {
            // nothing else may cause a frame while the worker runs
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        match self.preview_rx.as_ref().map(ready) {
            Some(Ready::Gone) => {
                // the preview worker ended without a picture: allow a new one instead of waiting for ever
                self.preview_rx = None;
                self.set_status("The preview could not be made (internal error) – change the view to try again.", BAD);
            }
            Some(Ready::Value((generation, img, view, holes))) => {
                self.preview_rx = None;
                self.mask_hole_share = holes;
                self.preview_size = img.size;
                match &mut self.preview_tex {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => self.preview_tex = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
                }
                self.preview_shown_gen = generation;
                self.preview_view = Some(view);
            }
            _ => {}
        }
        while let Ok((i, name, img)) = self.full_rx.try_recv() {
            self.full_pending.remove(&i);
            // only if the photo at that place is still the same one
            if self.photos.get(i).map(|p| p.name == name).unwrap_or(false) {
                self.full_tex.insert(i, ctx.load_texture(format!("full-{i}-{name}"), img, egui::TextureOptions::LINEAR));
            }
        }
        match self.fusion_rx.as_ref().map(ready) {
            Some(Ready::Gone) => {
                self.fusion_rx = None;
                self.set_status("The blended preview could not be made (internal error).", BAD);
            }
            Some(Ready::Value(img)) => {
                self.fusion_rx = None;
                match &mut self.fusion_tex {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => self.fusion_tex = Some(ctx.load_texture("fusion", img, egui::TextureOptions::LINEAR)),
                }
            }
            _ => {}
        }
        if self.fusion_dirty && self.mode == Mode::Exposure {
            self.request_fusion_preview();
        }
    }

    pub(super) fn request_preview(&mut self, max_w: usize, fast: bool) {
        // with a brightness update on its way the preview would show the old brightness: wait for it
        if self.preview_rx.is_some() || !self.aligned || self.gains_pending() {
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
            // holes the masks make (checked on a coarse grid; only when a photo has a mask)
            let holes = if rp.iter().any(|p| p.mask.is_some()) { stitch::mask_holes(&rp, &view, MASK_HOLE_GRID, ((MASK_HOLE_GRID as f64 / aspect) as usize).max(8)) } else { 0.0 };
            let _ = tx.send((generation, egui::ColorImage::from_rgba_unmultiplied([w, h], &px), view, holes));
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

#[cfg(test)]
mod tests {
    use super::super::masks_tests::app_with_photos;
    use super::*;

    fn poll_until_idle(app: &mut PanoApp) {
        let ctx = egui::Context::default();
        let t0 = Instant::now();
        while app.busy() {
            assert!(t0.elapsed().as_secs() < 20, "the job never finished");
            app.poll(&ctx);
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_panicking_job_is_reported_and_the_program_is_not_busy_for_ever() {
        let mut app = app_with_photos(2);
        app.start_job("Test job", |_, _| panic!("boom in the worker"));
        poll_until_idle(&mut app);
        assert!(app.status.contains("Test job: failed") && app.status.contains("boom in the worker"), "{}", app.status);
        assert_eq!(app.photos.len(), 2, "nothing was changed");
        // and the next job runs normally
        app.start_job("Next", |_, _| JobMsg::Cancelled);
        poll_until_idle(&mut app);
    }

    #[test]
    fn a_preview_worker_that_vanished_allows_a_new_preview() {
        let mut app = app_with_photos(2);
        let (tx, rx) = channel();
        drop(tx);
        app.preview_rx = Some(rx);
        app.poll(&egui::Context::default());
        assert!(app.preview_rx.is_none(), "a new preview can be requested");
        assert!(app.status.contains("preview could not be made"), "{}", app.status);
    }

    fn gains_update(n: usize, v: f32) -> super::super::pipeline::GainsUpdate {
        super::super::pipeline::GainsUpdate { gains: vec![[v; 3]; n], gain_maps: vec![None; n], lens_models: vec![None; n] }
    }

    /// The brightness update runs on a worker; its result is taken over when it arrives, and the
    /// export and previews wait for it meanwhile.
    #[test]
    fn the_brightness_update_is_taken_over_when_it_arrives() {
        let mut app = app_with_photos(2);
        let ctx = egui::Context::default();
        app.update_gains();
        assert!(app.gains_pending(), "the export waits");
        let t0 = Instant::now();
        while app.gains_pending() {
            assert!(t0.elapsed().as_secs() < 20, "the update never arrived");
            app.poll(&ctx);
            std::thread::yield_now();
        }
        assert_eq!(app.gains.len(), 2);
        assert!(app.preview_dirty, "the preview is made again with the new values");
    }

    /// A result made for other photos (one added or removed meanwhile, or an older request) is dropped.
    #[test]
    fn a_brightness_update_for_other_photos_is_dropped() {
        let mut app = app_with_photos(2);
        let ctx = egui::Context::default();
        // wrong number of photos
        let (tx, rx) = channel();
        tx.send((app.gains_gen, gains_update(3, 2.0))).unwrap();
        app.gains_rx = Some(rx);
        app.poll(&ctx);
        assert!(!app.gains_pending() && app.gains.iter().all(|g| *g != [2.0; 3]));
        // an older request
        let (tx, rx) = channel();
        app.gains_gen += 1;
        tx.send((app.gains_gen - 1, gains_update(2, 2.0))).unwrap();
        app.gains_rx = Some(rx);
        app.poll(&ctx);
        assert!(!app.gains_pending() && app.gains.iter().all(|g| *g != [2.0; 3]));
        // the right one
        let (tx, rx) = channel();
        tx.send((app.gains_gen, gains_update(2, 2.0))).unwrap();
        app.gains_rx = Some(rx);
        app.poll(&ctx);
        assert_eq!(app.gains, vec![[2.0; 3]; 2]);
        // a worker that ended without a result is reported, the old values stay
        let (tx, rx) = channel::<(u64, super::super::pipeline::GainsUpdate)>();
        drop(tx);
        app.gains_rx = Some(rx);
        app.poll(&ctx);
        assert!(!app.gains_pending() && app.status.contains("Brightness matching could not be worked out"), "{}", app.status);
        assert_eq!(app.gains, vec![[2.0; 3]; 2]);
    }
}
