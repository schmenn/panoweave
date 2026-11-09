use super::*;

/// "Find more points for these two images": a found point closer than this (normalised
/// coordinates, long image side = 1, so 0.004 is about 6 px of a 1600 px photo) to a point the pair
/// already has, in either photo, is not added a second time.
pub(super) const NEW_POINT_MIN_DIST: f64 = 0.004;

/// Splits a search result for the pair (`a`, `b`) into the points to add and counts the ones
/// that lie on an existing point of the pair (or on an earlier new one). The existing points are
/// never touched.
pub(super) fn new_pair_points(existing: &[ControlPoint], found: Vec<ControlPoint>, a: usize, b: usize, min_dist: f64) -> (Vec<ControlPoint>, usize) {
    // existing points as (position in photo a, position in photo b)
    let mut taken: Vec<([f64; 2], [f64; 2])> = existing
        .iter()
        .filter_map(|c| if c.a == a && c.b == b { Some((c.pa, c.pb)) } else if c.a == b && c.b == a { Some((c.pb, c.pa)) } else { None })
        .collect();
    let near = |p: [f64; 2], q: [f64; 2]| (p[0] - q[0]).hypot(p[1] - q[1]) < min_dist;
    let mut add = Vec::new();
    let mut there = 0;
    for c in found {
        if taken.iter().any(|t| near(t.0, c.pa) || near(t.1, c.pb)) {
            there += 1;
        } else {
            taken.push((c.pa, c.pb));
            add.push(c);
        }
    }
    (add, there)
}

impl PanoApp {
    /// The parallax hint of the last "Find points": which pairs have matches on a nearer object that do not fit the
    /// rest, with buttons for the two ways out. Nothing is applied before a click.
    pub(super) fn ui_parallax_hints(&mut self, ui: &mut egui::Ui) {
        if self.parallax_hints.is_empty() || self.parallax_cfg.enabled {
            return;
        }
        card(ui, |ui| {
            ui.label(RichText::new("Nearby object").strong().color(WARN));
            for h in self.parallax_hints.iter().take(PARALLAX_HINTS_SHOWN) {
                ui.label(format!("{} The group lies {} {}.", h.text(), h.place(), h.a + 1));
            }
            if self.parallax_hints.len() > PARALLAX_HINTS_SHOWN {
                ui.label(RichText::new(format!("… and {} more pairs (see the log).", self.parallax_hints.len() - PARALLAX_HINTS_SHOWN)).small().color(MUTED));
            }
            ui.label(RichText::new("These matches are not used as points (the rotation model cannot fulfil them). Parallax mode corrects such shifts locally near the seams; a mask keeps the object from showing twice.").small().color(MUTED));
            ui.horizontal_wrapped(|ui| {
                if ui.button("Turn on Parallax mode").on_hover_text("Switches the option “Parallax mode” in step 3 on. Align again afterwards.").clicked() {
                    self.parallax_cfg.enabled = true;
                    self.needs_align = true;
                    self.set_status("Parallax mode is on. Press “Align photos” to use it.", GOOD);
                }
                if ui.button("Open masks").on_hover_text("Opens the mask editor: paint the object red to keep it out of the panorama, or green to take it from one photo.").clicked() {
                    self.mk.open = true;
                }
            });
        });
        ui.add_space(6.0);
    }

    pub(super) fn run_match(&mut self, chain: bool) {
        if self.busy() || self.photos.len() < 2 {
            return;
        }
        self.auto_chain = chain;
        let grays: Vec<Arc<Gray>> = self.photos.iter().map(|p| p.gray.clone()).collect();
        let works: Vec<Arc<RgbImage>> = self.photos.iter().map(|p| p.work.clone()).collect();
        let manual: Vec<ControlPoint> = self.cps.iter().filter(|c| c.manual || c.locked).copied().collect();
        let mut settings = self.match_settings;
        // with Parallax mode on the near objects are handled by the mode itself: no check, no extra work
        settings.parallax_hint.enabled &= !self.parallax_cfg.enabled;
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
        self.parallax_hints = m.parallax;
        let n = self.photos.len();
        let comps = crate::align::components(n, &self.cps);
        let groups = comps.iter().max().map(|v| v + 1).unwrap_or(0);
        let msg = format!(
            "Found {} matching points between {} photo pairs in {}.",
            self.cps.len(),
            m.pairs.len(),
            fmt_ms(m.detect_ms + m.match_ms)
        );
        if let Some(h) = self.parallax_hints.first() {
            let more = if self.parallax_hints.len() > 1 { format!(" (and {} more pairs)", self.parallax_hints.len() - 1) } else { String::new() };
            let text = format!("{msg} {} Try Parallax mode or mask the object – see step 3{more}.", h.text());
            self.set_status(text, WARN);
        } else if groups > 1 {
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
        // adds to the points of the pair, never replaces or moves one (also not the automatic
        // ones: the person may have checked them); spots that are taken already are skipped
        let (fresh, there) = new_pair_points(&self.cps, cps, a, b, NEW_POINT_MIN_DIST);
        let n = fresh.len();
        if n == 0 {
            self.set_status(
                format!("Photos {} and {}: 0 new points, {there} already there (found {mode} in {}). Nothing changed.", a + 1, b + 1, fmt_ms(ms)),
                WARN,
            );
            return;
        }
        self.push_undo(&format!("finding points for photos {}/{}", a + 1, b + 1));
        self.cps.extend(fresh);
        self.matched = true;
        self.needs_align = true;
        self.set_status(
            format!("Photos {} and {}: {n} new, {there} already there (found {mode} in {}). Your points stay. Align again (step 3) to use them.", a + 1, b + 1, fmt_ms(ms)),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cp(a: usize, b: usize, pa: [f64; 2], pb: [f64; 2], manual: bool) -> ControlPoint {
        ControlPoint::new(a, b, pa, pb, manual)
    }

    #[test]
    fn find_more_adds_to_the_pair_and_skips_spots_that_are_taken() {
        let mut hand = cp(0, 1, [0.10, 0.10], [0.20, 0.20], true);
        hand.locked = true;
        // an automatic point stored in the other order (photo 1 first) must count as well
        let auto_swapped = cp(1, 0, [0.50, 0.50], [0.40, 0.40], false);
        let other_pair = cp(1, 2, [0.30, 0.30], [0.31, 0.31], false);
        let existing = vec![hand, auto_swapped, other_pair];
        let found = vec![
            cp(0, 1, [0.1001, 0.1002], [0.2001, 0.2002], false), // on the hand point
            cp(0, 1, [0.40, 0.4001], [0.50, 0.5001], false),     // on the swapped automatic point
            cp(0, 1, [0.30, 0.30], [0.31, 0.31], false),         // new (only the other pair has this spot)
            cp(0, 1, [0.3001, 0.3001], [0.3101, 0.3101], false), // twice in the result: second one is skipped
            cp(0, 1, [0.70, 0.70], [0.71, 0.71], false),         // new
        ];
        let (add, there) = new_pair_points(&existing, found, 0, 1, NEW_POINT_MIN_DIST);
        assert_eq!((add.len(), there), (2, 3));
        assert_eq!(add[0].pa, [0.30, 0.30]);
    }

    #[test]
    fn on_pair_points_keeps_every_existing_point_and_reports_new_and_present() {
        let mut app = super::super::masks_tests::app_with_photos(2);
        app.cps = vec![cp(0, 1, [0.1, 0.1], [0.2, 0.2], true), cp(0, 1, [0.3, 0.3], [0.4, 0.4], false)];
        let before = app.cps.clone();
        let found: Vec<ControlPoint> = vec![
            cp(0, 1, [0.1, 0.1], [0.2, 0.2], false),
            cp(0, 1, [0.5, 0.1], [0.6, 0.2], false),
            cp(0, 1, [0.5, 0.3], [0.6, 0.4], false),
            cp(0, 1, [0.5, 0.5], [0.6, 0.6], false),
            cp(0, 1, [0.5, 0.7], [0.6, 0.8], false),
        ];
        app.on_pair_points(0, 1, found, 5, "from your points");
        assert_eq!(app.cps.len(), 6);
        assert!(app.cps.iter().zip(&before).all(|(x, y)| x.pa == y.pa && x.pb == y.pb && x.manual == y.manual), "existing points are untouched");
        assert!(app.status.contains("4 new, 1 already there"), "{}", app.status);
        app.undo_last();
        assert_eq!(app.cps.len(), 2);
    }
}
