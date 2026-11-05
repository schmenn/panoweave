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
        if n > 0 {
            self.push_undo(&format!("finding points for photos {}/{}", a + 1, b + 1));
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

    pub(super) fn on_matched(&mut self, m: MatchResult) {
        if !self.cps.is_empty() {
            self.push_undo("finding points in all photos");
        }
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
        self.push_undo(&format!("finding points for photos {}/{}", a + 1, b + 1));
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
