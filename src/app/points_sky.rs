//! Points on sky or cloud: sky masks per photo (worker thread, cached in memory only), marking and removal.

use super::*;
use crate::sky_points::{self, PhotoSky, SkyPoint};

impl PanoApp {
    fn sky_key(&self, i: usize) -> usize {
        Arc::as_ptr(&self.photos[i].work) as usize
    }

    /// Photos that carry a point (not a control area) and have no sky mask yet: (key, working image).
    fn sky_missing(&self) -> Vec<(usize, Arc<RgbImage>)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for c in self.cps.iter().filter(|c| c.kind == CpKind::Point) {
            for i in [c.a, c.b] {
                if i >= self.photos.len() {
                    continue;
                }
                let key = self.sky_key(i);
                if !self.sky_masks.contains_key(&key) && seen.insert(key) {
                    out.push((key, self.photos[i].work.clone()));
                }
            }
        }
        out
    }

    /// Number of loaded photos that already have a sky mask.
    pub(super) fn sky_checked_photos(&self) -> usize {
        (0..self.photos.len()).filter(|&i| self.sky_masks.contains_key(&self.sky_key(i))).count()
    }

    /// Computes the missing sky masks on a worker thread (one pass over each working image).
    pub(super) fn start_sky_check(&mut self) {
        if self.busy() {
            return;
        }
        let missing = self.sky_missing();
        if missing.is_empty() {
            return;
        }
        self.start_job("Checking for sky and clouds", move |report, cancel| {
            let n = missing.len();
            let mut out = Vec::new();
            for (k, (key, work)) in missing.into_iter().enumerate() {
                if cancel.load(Ordering::Relaxed) || engine::cancelled() {
                    return JobMsg::Cancelled;
                }
                report(k as f32 / n as f32, &format!("Photo {} of {n}", k + 1));
                out.push((key, work.clone(), Arc::new(sky_points::mask_of(&work))));
            }
            JobMsg::SkyMasks(out)
        });
    }

    pub(super) fn on_sky_masks(&mut self, v: Vec<(usize, Arc<RgbImage>, Arc<crate::features::SkyMask>)>) {
        for (key, work, mask) in v {
            self.sky_masks.insert(key, (work, mask));
        }
        // masks of photos that are gone are dropped
        let live: HashSet<usize> = self.photos.iter().map(|p| Arc::as_ptr(&p.work) as usize).collect();
        self.sky_masks.retain(|k, _| live.contains(k));
        self.sky_version += 1;
        self.sky_cache = None;
        self.cp_rows_cache = None;
        let n = self.sky_points().len();
        self.set_status(
            if n == 0 { "No points on sky or cloud found.".to_string() } else { format!("{n} points on sky or cloud, shown in blue in the list (All points, F3).") },
            MUTED,
        );
    }

    /// The points on sky or cloud, cached until the points, the masks or the settings change.
    /// Empty while the check is switched off; photos without a mask are not judged.
    pub(super) fn sky_points(&mut self) -> Vec<SkyPoint> {
        if !self.sky_cfg.enabled {
            return Vec::new();
        }
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.cps.len().hash(&mut h);
        self.sky_version.hash(&mut h);
        self.sky_cfg.threshold.to_bits().hash(&mut h);
        for c in &self.cps {
            (c.a, c.b, c.kind as u8, c.pa[0].to_bits(), c.pa[1].to_bits(), c.pb[0].to_bits(), c.pb[1].to_bits()).hash(&mut h);
        }
        let key = h.finish();
        if let Some((k, d)) = &self.sky_cache
            && *k == key {
                return d.clone();
            }
        let photos: Vec<Option<PhotoSky>> = (0..self.photos.len())
            .map(|i| {
                let (_, mask) = self.sky_masks.get(&self.sky_key(i))?;
                let w = &self.photos[i].work;
                Some(PhotoSky { mask: mask.as_ref(), w: w.width() as usize, h: w.height() as usize })
            })
            .collect();
        let d = sky_points::find(&self.cps, &photos, &self.sky_cfg);
        self.sky_cache = Some((key, d.clone()));
        d
    }

    /// Removes the automatic points on sky or cloud as one undo step. Points placed by hand and
    /// protected points stay; the status line says how many and why.
    pub(super) fn remove_sky_points(&mut self) {
        let found = self.sky_points();
        let r = sky_points::removable(&self.cps, &found, self.sky_cfg.include_manual);
        let n = r.remove.len();
        let note = sky_points::removal_note(&r);
        if n == 0 {
            self.set_status(format!("Nothing to remove.{note}"), MUTED);
            return;
        }
        self.delete_points(r.remove, &format!("removing {n} points on sky or cloud"));
        self.sky_cache = None;
        self.set_status(format!("Removed {n} points on sky or cloud.{note} Ctrl+Z brings them back. Align again (step 3) to update."), MUTED);
    }

    /// Controls of the sky check: switch, threshold, "Check photos" / count and the remove button.
    /// `list` adds the list filter (used in the list of all points).
    pub(super) fn sky_ui(&mut self, ui: &mut egui::Ui, list: bool) {
        ui.checkbox(&mut self.sky_cfg.enabled, "Find points on sky or cloud")
            .on_hover_text("Marks points whose surroundings are sky or cloud in either photo (clouds move between shots). Nothing is changed until you press Remove.");
        if !self.sky_cfg.enabled {
            return;
        }
        let (lo, hi) = sky_points::THRESHOLD_RANGE;
        ui.label("above");
        ui.add(egui::DragValue::new(&mut self.sky_cfg.threshold).range(lo..=hi).speed(0.01).fixed_decimals(2))
            .on_hover_text("Share of sky or cloud around the point (circle of 20 px at 800 px photo width) above which it counts. Lower = stricter.");
        let missing = self.sky_missing().len();
        if missing > 0 {
            if ui.add_enabled(!self.busy(), egui::Button::new(format!("Check {missing} photos"))).on_hover_text("Looks for sky and clouds in the photos (runs in the background).").clicked() {
                self.start_sky_check();
            }
            return;
        }
        let checked = self.sky_checked_photos();
        ui.label(RichText::new(format!("sky checked in {checked} photo{}", if checked == 1 { "" } else { "s" })).small().color(MUTED))
            .on_hover_text("Sky masks are kept until the program closes; new points are checked at once. Photos added later get a \"Check\" button again.");
        let found = self.sky_points();
        let r = sky_points::removable(&self.cps, &found, self.sky_cfg.include_manual);
        ui.label(RichText::new(format!("{} points on sky or cloud", found.len())).small().color(if found.is_empty() { MUTED } else { WARN }));
        ui.checkbox(&mut self.sky_cfg.include_manual, "include points placed by hand")
            .on_hover_text("Also remove points you placed by hand. Protected points (🔒) always stay.");
        if ui
            .add_enabled(!r.remove.is_empty() && !self.busy(), egui::Button::new(sky_points::button_text(&r)))
            .on_hover_text("Removes the points on sky or cloud (one undo step). Protected points stay; points placed by hand only with \"include points placed by hand\".")
            .on_disabled_hover_text("Nothing to remove: the points found are protected or placed by hand (tick \"include points placed by hand\"), or none were found.")
            .clicked()
        {
            self.remove_sky_points();
        }
        if !r.remove.is_empty() && r.skipped_manual + r.skipped_protected > 0 {
            ui.label(RichText::new(format!("{} stay (by hand or protected)", r.skipped_manual + r.skipped_protected)).small().color(MUTED));
        }
        if list {
            ui.checkbox(&mut self.cp_only_sky, "only those");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::masks_tests::app_with_photos;
    use super::*;
    use crate::features::SkyMask;

    /// photo 0: sky in the upper half; photo 1: no sky
    fn setup() -> PanoApp {
        let mut app = app_with_photos(2);
        app.sky_cfg.enabled = true;
        let (gw, gh) = (12, 8);
        let sky: Vec<bool> = (0..gw * gh).map(|i| i / gw < 4).collect();
        let m0 = SkyMask { cell: 10.0, gw, gh, sky };
        let m1 = SkyMask { cell: 10.0, gw, gh, sky: vec![false; gw * gh] };
        for (i, m) in [m0, m1].into_iter().enumerate() {
            let w = app.photos[i].work.clone();
            let key = app.sky_key(i);
            app.sky_masks.insert(key, (w, Arc::new(m)));
        }
        let (w, h) = (app.photos[0].work.width() as usize, app.photos[0].work.height() as usize);
        let at = |x: f32, y: f32| engine::norm_coord(x, y, w, h);
        let mk = |y: f32, manual: bool, locked: bool| {
            let mut c = ControlPoint::new(0, 1, at(60.0, y), at(60.0, y), manual);
            c.locked = locked;
            c
        };
        // 0, 1: automatic on sky; 2: by hand on sky; 3: protected on sky; 4: automatic on the ground
        app.cps = vec![mk(10.0, false, false), mk(15.0, false, false), mk(12.0, true, false), mk(14.0, false, true), mk(70.0, false, false)];
        app.needs_align = false;
        app
    }

    #[test]
    fn marks_all_sky_points_but_offers_only_automatic_unprotected_ones() {
        let mut app = setup();
        let found = app.sky_points();
        assert_eq!(found.iter().map(|s| s.point).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        let r = sky_points::removable(&app.cps, &found, false);
        assert_eq!((r.remove, r.skipped_manual, r.skipped_protected), (vec![0, 1], 1, 1));
    }

    #[test]
    fn threshold_and_switch_change_the_result() {
        let mut app = setup();
        app.sky_cfg.threshold = 1.0;
        assert!(app.sky_points().is_empty());
        app.sky_cfg.threshold = 0.5;
        assert_eq!(app.sky_points().len(), 4);
        app.sky_cfg.enabled = false;
        assert!(app.sky_points().is_empty());
    }

    #[test]
    fn removal_is_one_undo_step_and_keeps_hand_and_protected_points() {
        let mut app = setup();
        let steps = app.undo.can_undo();
        app.remove_sky_points();
        assert_eq!(app.cps.len(), 3);
        assert!(app.cps.iter().any(|c| c.manual) && app.cps.iter().any(|c| c.locked));
        assert!(app.status.contains("Removed 2") && app.status.contains("protected") && app.status.contains("by hand"), "{}", app.status);
        assert!(app.needs_align);
        assert!(!steps && app.undo.can_undo());
        app.undo_last();
        assert_eq!(app.cps.len(), 5);
        assert!(!app.undo.can_undo(), "one undo step brings everything back");
    }

    #[test]
    fn with_the_option_hand_placed_points_go_too_but_protected_stay() {
        let mut app = setup();
        app.sky_cfg.include_manual = true;
        app.remove_sky_points();
        // left: the protected sky point and the ground point
        assert_eq!(app.cps.len(), 2);
        assert!(app.cps.iter().all(|c| !c.manual));
        assert!(app.cps.iter().any(|c| c.locked));
        app.undo_last();
        assert_eq!(app.cps.len(), 5);
    }

    #[test]
    fn photos_without_mask_are_reported_missing_and_not_judged() {
        let mut app = setup();
        assert!(app.sky_missing().is_empty());
        assert_eq!(app.sky_checked_photos(), 2);
        app.sky_masks.clear();
        assert_eq!(app.sky_checked_photos(), 0);
        assert_eq!(app.sky_missing().len(), 2);
        assert!(app.sky_points().is_empty());
        app.cps.clear();
        assert!(app.sky_missing().is_empty());
    }
}
