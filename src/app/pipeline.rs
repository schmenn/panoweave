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

    pub(super) fn update_gains(&mut self) {
        let active = self.active_indices();
        let mut rp = self.render_photos(true);
        let g = stitch::compute_gains(&rp, &self.view);
        self.gains = vec![[1.0; 3]; self.photos.len()];
        self.gain_maps = vec![None; self.photos.len()];
        for (k, &i) in active.iter().enumerate() {
            self.gains[i] = g[k];
        }
        if self.gainmap_cfg.is_on() {
            // the maps are solved on top of the global factors
            for (p, g) in rp.iter_mut().zip(&g) {
                p.gain = *g;
                p.gain_map = None;
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
