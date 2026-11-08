use super::state::{ready, Ready};
use super::*;

impl PanoApp {
    pub(super) fn active_indices(&self) -> Vec<usize> {
        (0..self.photos.len()).filter(|&i| self.photos[i].enabled).collect()
    }

    /// Per photo: joined to the biggest group of photos by control points (see `image_groups`).
    pub(super) fn connected_photos(&self) -> Vec<bool> {
        let active = self.active_indices();
        let groups = image_groups(self.photos.len(), &active, &self.cps);
        let main = groups.iter().max_by_key(|g| g.len()).cloned().unwrap_or_default();
        (0..self.photos.len()).map(|i| main.contains(&i) || active.len() < 2).collect()
    }

    pub(super) fn render_photos(&self, active_only: bool) -> Vec<RenderPhoto> {
        let trusted = self.connected_photos();
        self.photos
            .iter()
            .enumerate()
            .filter(|(_, p)| !active_only || p.enabled)
            .map(|(i, p)| RenderPhoto {
                img: p.work.clone(),
                w: p.work.width() as f64,
                h: p.work.height() as f64,
                rot: p.rot,
                lens: self.lenses[p.lens],
                gain: if self.match_brightness { self.gains.get(i).copied().unwrap_or([1.0; 3]) } else { [1.0; 3] },
                gain_map: self.gain_map_of(i),
                photometry: self.photometry_of(i),
                field: self.fields.get(i).cloned().flatten(),
                full_long: p.long(),
                trusted: trusted.get(i).copied().unwrap_or(true),
                mask: p.mask.buf.clone(),
            })
            .collect()
    }

    /// The smooth brightness correction of photo `i` (only with "match brightness" and the correction switched on).
    pub(super) fn gain_map_of(&self, i: usize) -> Option<Arc<GainMap>> {
        if self.match_brightness && self.gainmap_cfg.is_on() { self.gain_maps.get(i).cloned().flatten() } else { None }
    }

    /// The response curve and vignetting correction of photo `i` (only with "match brightness" and the lens correction switched on).
    pub(super) fn photometry_of(&self, i: usize) -> Option<crate::lensphoto::LensModel> {
        if self.match_brightness && self.lens_cfg.is_on() { self.lens_models.get(i).copied().flatten() } else { None }
    }

    /// Brightness factors, lens models and gain maps are worked out again on a worker thread (up to
    /// about a second with the lens correction on: before, the window stood still meanwhile). Until the
    /// result arrives the old values stay; previews and the export wait for it (`gains_pending`).
    pub(super) fn update_gains(&mut self) {
        let active = self.active_indices();
        let rp = self.render_photos(true);
        let lens_of: Vec<usize> = active.iter().map(|&i| self.photos[i].lens).collect();
        let (view, lens_cfg, gainmap_cfg) = (self.view.clone(), self.lens_cfg.clone(), self.gainmap_cfg.clone());
        let n = self.photos.len();
        self.gains_gen += 1;
        let generation = self.gains_gen;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let u = compute_gains(rp, &active, &lens_of, n, &view, &lens_cfg, &gainmap_cfg);
            let _ = tx.send((generation, u));
        });
        self.gains_rx = Some(rx);
    }

    /// A brightness update is still being worked out.
    pub(super) fn gains_pending(&self) -> bool {
        self.gains_rx.is_some()
    }

    /// Takes over a finished brightness update; one asked for later replaces an older one, and one made
    /// for other photos (removed / new project meanwhile) is dropped.
    pub(super) fn poll_gains(&mut self) {
        match self.gains_rx.as_ref().map(ready) {
            Some(Ready::Value((generation, u))) => {
                self.gains_rx = None;
                // only for the photos as they are now (a photo added meanwhile changes the count)
                if generation == self.gains_gen && u.gains.len() == self.photos.len() {
                    self.gains = u.gains;
                    self.gain_maps = u.gain_maps;
                    self.lens_models = u.lens_models;
                    self.preview_dirty = true;
                }
            }
            Some(Ready::Gone) => {
                self.gains_rx = None;
                self.set_status("Brightness matching could not be worked out (internal error) – the previous values are kept.", BAD);
            }
            _ => {}
        }
    }
}

/// Result of `compute_gains`, indexed by photo number.
pub(super) struct GainsUpdate {
    pub gains: Vec<[f32; 3]>,
    pub gain_maps: Vec<Option<Arc<GainMap>>>,
    pub lens_models: Vec<Option<crate::lensphoto::LensModel>>,
}

/// Brightness factors, lens models (when switched on) and gain maps (when switched on) for the active
/// photos `active` (`rp` and `lens_of` in the same order) out of `n` photos. Pure function, runs on a
/// worker thread.
pub(super) fn compute_gains(
    mut rp: Vec<RenderPhoto>,
    active: &[usize],
    lens_of: &[usize],
    n: usize,
    view: &PanoView,
    lens_cfg: &crate::lensphoto::LensPhotoConfig,
    gainmap_cfg: &crate::gainmap::GainMapConfig,
) -> GainsUpdate {
    let t_all = std::time::Instant::now();
    for p in rp.iter_mut() {
        p.photometry = None;
    }
    let mut g = stitch::compute_gains(&rp, view);
    let mut out = GainsUpdate { gains: vec![[1.0; 3]; n], gain_maps: vec![None; n], lens_models: vec![None; n] };
    let mut models: Vec<Option<crate::lensphoto::LensModel>> = vec![None; rp.len()];
    if lens_cfg.is_on() {
        // one model per lens, solved together with factors of the photos
        let t0 = std::time::Instant::now();
        let cfg = crate::lensphoto::LensPhotoConfig { enabled: true, ..lens_cfg.clone() };
        match crate::lensphoto::fit_lens_models(&rp, lens_of, view, &cfg, gainmap_cfg) {
            Some(fit) => {
                eprintln!(
                    "lens correction: {} of {} lenses modelled, cost per comparison {:.5} -> {:.5}, {} ms",
                    fit.models.iter().flatten().count(),
                    fit.models.len(),
                    fit.cost_gains_only,
                    fit.cost_final,
                    t0.elapsed().as_millis()
                );
                for (k, &l) in lens_of.iter().enumerate() {
                    models[k] = fit.models.get(l).copied().flatten();
                    rp[k].photometry = models[k];
                }
                // the global factors are measured again on the corrected photos (same method as without
                // the model); experiments: PW_LENS_GAINS=fit uses the factors of the fit instead
                g = if std::env::var("PW_LENS_GAINS").is_ok_and(|v| v == "fit") { fit.gains } else { stitch::compute_gains(&rp, view) };
            }
            None => eprintln!("lens correction: not enough comparisons in the overlaps, not used"),
        }
    }
    for (k, &i) in active.iter().enumerate() {
        out.gains[i] = g[k];
        out.lens_models[i] = models[k];
    }
    if gainmap_cfg.is_on() {
        // the maps are solved on top of the global factors (and the lens models)
        for ((p, g), m) in rp.iter_mut().zip(&g).zip(&models) {
            p.gain = *g;
            p.gain_map = None;
            p.photometry = *m;
        }
        let t0 = std::time::Instant::now();
        let maps = crate::gainmap::compute_gain_maps(&rp, view, gainmap_cfg);
        if std::env::var("PW_DEBUG").is_ok() {
            eprintln!("gain maps: {} of {} photos, {} ms", maps.iter().flatten().count(), maps.len(), t0.elapsed().as_millis());
        }
        for (m, &i) in maps.into_iter().zip(active) {
            out.gain_maps[i] = m.map(Arc::new);
        }
    }
    eprintln!(
        "brightness update: {} ms ({} photos, lens correction {}, gain maps {})",
        t_all.elapsed().as_millis(),
        active.len(),
        if lens_cfg.is_on() { "on" } else { "off" },
        if gainmap_cfg.is_on() { "on" } else { "off" }
    );
    out
}
