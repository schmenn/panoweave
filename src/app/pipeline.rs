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

    pub(super) fn update_gains(&mut self) {
        let active = self.active_indices();
        let mut rp = self.render_photos(true);
        for p in rp.iter_mut() {
            p.photometry = None;
        }
        let mut g = stitch::compute_gains(&rp, &self.view);
        self.gains = vec![[1.0; 3]; self.photos.len()];
        self.gain_maps = vec![None; self.photos.len()];
        self.lens_models = vec![None; self.photos.len()];
        let mut models: Vec<Option<crate::lensphoto::LensModel>> = vec![None; rp.len()];
        if self.lens_cfg.is_on() {
            // one model per lens, solved together with factors of the photos
            let t0 = std::time::Instant::now();
            let lens_of: Vec<usize> = active.iter().map(|&i| self.photos[i].lens).collect();
            let cfg = crate::lensphoto::LensPhotoConfig { enabled: true, ..self.lens_cfg.clone() };
            match crate::lensphoto::fit_lens_models(&rp, &lens_of, &self.view, &cfg, &self.gainmap_cfg) {
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
                    g = if std::env::var("PW_LENS_GAINS").is_ok_and(|v| v == "fit") { fit.gains } else { stitch::compute_gains(&rp, &self.view) };
                }
                None => eprintln!("lens correction: not enough comparisons in the overlaps, not used"),
            }
        }
        for (k, &i) in active.iter().enumerate() {
            self.gains[i] = g[k];
            self.lens_models[i] = models[k];
        }
        if self.gainmap_cfg.is_on() {
            // the maps are solved on top of the global factors (and the lens models)
            for ((p, g), m) in rp.iter_mut().zip(&g).zip(&models) {
                p.gain = *g;
                p.gain_map = None;
                p.photometry = *m;
            }
            let t0 = std::time::Instant::now();
            let maps = crate::gainmap::compute_gain_maps(&rp, &self.view, &self.gainmap_cfg);
            if std::env::var("PW_DEBUG").is_ok() {
                eprintln!("gain maps: {} of {} photos, {} ms", maps.iter().flatten().count(), maps.len(), t0.elapsed().as_millis());
            }
            for (m, &i) in maps.into_iter().zip(&active) {
                self.gain_maps[i] = m.map(Arc::new);
            }
        }
    }
}
