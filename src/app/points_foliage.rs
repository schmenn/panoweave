//! Points on foliage: foliage maps per photo (worker thread, cached in memory only, like the sky masks),
//! marking and removal on request. The weighting itself happens in the optimiser (`foliage.rs`).

use super::*;
use crate::foliage::{self, FoliageMap, FoliagePoint};

impl PanoApp {
    fn fol_key(&self, i: usize) -> usize {
        Arc::as_ptr(&self.photos[i].work) as usize
    }

    /// Photos that carry a point (not a control area) and have no foliage map yet: (key, working image).
    fn fol_missing(&self) -> Vec<(usize, Arc<RgbImage>)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for c in self.cps.iter().filter(|c| c.kind == CpKind::Point) {
            for i in [c.a, c.b] {
                if i >= self.photos.len() {
                    continue;
                }
                let key = self.fol_key(i);
                if !self.fol_maps.contains_key(&key) && seen.insert(key) {
                    out.push((key, self.photos[i].work.clone()));
                }
            }
        }
        out
    }

    /// Computes the missing foliage maps on a worker thread (one pass over each working image).
    pub(super) fn start_foliage_check(&mut self) {
        if self.busy() {
            return;
        }
        let missing = self.fol_missing();
        if missing.is_empty() {
            return;
        }
        self.start_job("Checking for foliage", move |report, cancel| {
            let n = missing.len();
            let mut out = Vec::new();
            for (k, (key, work)) in missing.into_iter().enumerate() {
                if cancel.load(Ordering::Relaxed) || engine::cancelled() {
                    return JobMsg::Cancelled;
                }
                report(k as f32 / n as f32, &format!("Photo {} of {n}", k + 1));
                let map = foliage::map_of(&work, &imgutil::to_gray(&work));
                out.push((key, work, Arc::new(map)));
            }
            JobMsg::FoliageMaps(out)
        });
    }

    pub(super) fn on_foliage_maps(&mut self, v: Vec<(usize, Arc<RgbImage>, Arc<FoliageMap>)>) {
        for (key, work, map) in v {
            self.fol_maps.insert(key, (work, map));
        }
        let live: HashSet<usize> = self.photos.iter().map(|p| Arc::as_ptr(&p.work) as usize).collect();
        self.fol_maps.retain(|k, _| live.contains(k));
        self.fol_version += 1;
        self.fol_cache = None;
        let n = self.foliage_points().len();
        self.set_status(if n == 0 { "No points on foliage found.".to_string() } else { format!("{n} points on foliage (colour, fine structure and restless errors).") }, MUTED);
    }

    /// The points the filter counts as foliage, cached until the points, the maps or the settings change.
    /// Empty while the filter is switched off; photos without a map are not judged.
    pub(super) fn foliage_points(&mut self) -> Vec<FoliagePoint> {
        let cfg = self.fine_cfg.foliage;
        if !cfg.enabled {
            return Vec::new();
        }
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.cps.len().hash(&mut h);
        self.fol_version.hash(&mut h);
        cfg.remove_threshold.to_bits().hash(&mut h);
        for c in &self.cps {
            (c.a, c.b, c.kind as u8, c.pa[0].to_bits(), c.pa[1].to_bits(), c.pb[0].to_bits(), c.pb[1].to_bits(), c.err.to_bits()).hash(&mut h);
        }
        let key = h.finish();
        if let Some((k, d)) = &self.fol_cache
            && *k == key
        {
            return d.clone();
        }
        let maps: Vec<Option<&FoliageMap>> = (0..self.photos.len()).map(|i| self.fol_maps.get(&self.fol_key(i)).map(|m| m.1.as_ref())).collect();
        let d = foliage::find(&self.cps, &maps, &cfg);
        self.fol_cache = Some((key, d.clone()));
        d
    }

    /// Removes the automatic points on foliage as one undo step. Points placed by hand and protected points
    /// stay; the status line says how many and why.
    pub(super) fn remove_foliage_points(&mut self) {
        let found = self.foliage_points();
        let r = foliage::removable(&self.cps, &found, self.fine_cfg.foliage.include_manual);
        let n = r.remove.len();
        let note = foliage::removal_note(&r);
        if n == 0 {
            self.set_status(format!("Nothing to remove.{note}"), MUTED);
            return;
        }
        self.delete_points(r.remove, &format!("removing {n} points on foliage"));
        self.fol_cache = None;
        self.set_status(format!("Removed {n} points on foliage.{note} Ctrl+Z brings them back. Align again (step 3) to update."), MUTED);
    }

    /// Controls of the foliage check (only while the filter is on in the alignment settings): threshold,
    /// "Check photos", count and the remove button.
    pub(super) fn foliage_ui(&mut self, ui: &mut egui::Ui) {
        if !self.fine_cfg.foliage.enabled {
            return;
        }
        ui.label("Foliage").on_hover_text("Points and patches on trees and bushes count less when aligning (switch in step 3). Here you can also remove the points on foliage.");
        let (lo, hi) = foliage::THRESHOLD_RANGE;
        ui.add(egui::DragValue::new(&mut self.fine_cfg.foliage.remove_threshold).range(lo..=hi).speed(0.01).fixed_decimals(2))
            .on_hover_text("Foliage value from which a point counts as being on foliage (0..1; colour and fine structure count a little, restless errors after aligning decide). Lower = more points.");
        let missing = self.fol_missing().len();
        if missing > 0 {
            if ui.add_enabled(!self.busy(), egui::Button::new(format!("Check {missing} photos"))).on_hover_text("Looks for foliage in the photos (runs in the background).").clicked() {
                self.start_foliage_check();
            }
            return;
        }
        let found = self.foliage_points();
        let r = foliage::removable(&self.cps, &found, self.fine_cfg.foliage.include_manual);
        ui.label(RichText::new(format!("{} points on foliage", found.len())).small().color(if found.is_empty() { MUTED } else { WARN }));
        ui.checkbox(&mut self.fine_cfg.foliage.include_manual, "include points placed by hand")
            .on_hover_text("Also remove points you placed by hand. Protected points (🔒) always stay.");
        if ui
            .add_enabled(!r.remove.is_empty() && !self.busy(), egui::Button::new(crate::sky_points::button_text(&r)))
            .on_hover_text("Removes the points on foliage (one undo step). Protected points stay; points placed by hand only with \"include points placed by hand\".")
            .on_disabled_hover_text("Nothing to remove: the points found are protected or placed by hand (tick \"include points placed by hand\"), or none were found. Align first: restless errors after aligning are what marks moving leaves.")
            .clicked()
        {
            self.remove_foliage_points();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::masks_tests::app_with_photos;
    use super::*;

    /// two photos of 120 x 80 px with a foliage map that covers the left half; 12 points in a row, the
    /// first 6 in the left (foliage) half. Errors are restless everywhere.
    fn setup() -> PanoApp {
        let mut app = app_with_photos(2);
        let (gw, gh) = (12, 8);
        let score: Vec<f32> = (0..gw * gh).map(|i| if i % gw < 6 { 1.0 } else { 0.0 }).collect();
        for i in 0..2 {
            let w = app.photos[i].work.clone();
            let key = app.fol_key(i);
            app.fol_maps.insert(key, (w.clone(), Arc::new(FoliageMap { w: w.width() as usize, h: w.height() as usize, cell: 10.0, gw, gh, score: score.clone() })));
        }
        let (w, h) = (app.photos[0].work.width() as usize, app.photos[0].work.height() as usize);
        let at = |x: f32, y: f32| engine::norm_coord(x, y, w, h);
        // the row: errors jump between 0.2 and 4; 12 calm points (error 1) 30 px away set the typical error
        let mut v: Vec<ControlPoint> = (0..12)
            .map(|i| {
                let mut c = ControlPoint::new(0, 1, at(5.0 + 10.0 * i as f32, 40.0), at(5.0 + 10.0 * i as f32, 40.0), false);
                c.err = if i % 2 == 0 { 0.2 } else { 4.0 };
                c
            })
            .collect();
        v.extend((0..12).map(|i| {
            let mut c = ControlPoint::new(0, 1, at(5.0 + 10.0 * i as f32, 10.0), at(5.0 + 10.0 * i as f32, 10.0), false);
            c.err = 1.0;
            c
        }));
        app.cps = v;
        app.cps[1].locked = true;
        app.cps[3].manual = true;
        app.needs_align = false;
        app
    }

    #[test]
    fn finds_points_on_foliage_and_leaves_the_rest() {
        let mut app = setup();
        let found: Vec<usize> = app.foliage_points().iter().map(|f| f.point).collect();
        assert!(found.contains(&1) && found.contains(&5) && found.iter().all(|&p| p < 6), "left half only, calm points never: {found:?}");
        app.fine_cfg.foliage.enabled = false;
        assert!(app.foliage_points().is_empty(), "switched off finds nothing");
    }

    #[test]
    fn removal_is_one_undo_step_and_keeps_hand_and_protected_points() {
        let mut app = setup();
        let n0 = app.cps.len();
        app.remove_foliage_points();
        assert!(app.cps.len() < n0);
        assert!(app.cps.iter().any(|c| c.locked) && app.cps.iter().any(|c| c.manual));
        assert!(app.status.contains("on foliage") && app.status.contains("protected") && app.status.contains("by hand"), "{}", app.status);
        assert!(app.needs_align);
        app.undo_last();
        assert_eq!(app.cps.len(), n0);
        assert!(!app.undo.can_undo(), "one undo step brings everything back");
    }

    #[test]
    fn photos_without_map_are_reported_missing_and_not_judged() {
        let mut app = setup();
        assert!(app.fol_missing().is_empty());
        app.fol_maps.clear();
        assert_eq!(app.fol_missing().len(), 2);
        assert!(app.foliage_points().is_empty());
    }
}
