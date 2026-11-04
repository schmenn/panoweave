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
                field: self.fields.get(i).cloned().flatten(),
                full_long: p.long(),
                trusted: trusted.get(i).copied().unwrap_or(true),
            })
            .collect()
    }

    pub(super) fn update_gains(&mut self) {
        let active = self.active_indices();
        let rp = self.render_photos(true);
        let g = stitch::compute_gains(&rp, &self.view);
        self.gains = vec![[1.0; 3]; self.photos.len()];
        for (k, &i) in active.iter().enumerate() {
            self.gains[i] = g[k];
        }
    }
}
