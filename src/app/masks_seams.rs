//! "Show seams" of the mask window: runs the seam finder on a small panorama (on a worker thread)
//! and shows its seam map, so the person sees what a mask changes.

use super::*;
use std::hash::{Hash, Hasher};

/// Width of the small panorama the seam preview is made for (pixels).
const SEAM_PREVIEW_WIDTH: usize = 1600;
/// The seam map under the photo: at most this tall, at most this share of the window's room, and
/// the room its title and note take.
const SEAM_PANEL_MAX_H: f32 = 360.0;
const SEAM_PANEL_SHARE: f32 = 0.45;
const SEAM_PANEL_TITLE_H: f32 = 52.0;

/// The seam map of the active photos as a picture (every photo in its own colour, seams black).
pub(super) fn seam_image(photos: Vec<RenderPhoto>, view: &PanoView, prefer_detail: bool) -> Result<egui::ColorImage, String> {
    let count = photos.len();
    let loader = |i: usize, _pixels: bool| -> Result<RenderPhoto, String> { Ok(photos[i].clone()) };
    let ro = stitch::RenderOptions { levels: 0, cubic: false, prefer_detail, seams: crate::seams::SeamConfig { enabled: true, ..crate::seams::SeamConfig::for_export(true) } };
    let report = |_: f32, _: &str| {};
    let cancel = AtomicBool::new(false);
    let prog = stitch::Progress { report: &report, cancel: &cancel };
    let job = stitch::prepare_strips(count, &loader, view, SEAM_PREVIEW_WIDTH, Blend::Smooth, &ro, &crate::strip::StripConfig::default(), FALLBACK_EXPORT_BUDGET, &prog)?;
    let labels = job.labels().ok_or("The seam finder gave no result.")?;
    Ok(color_image(&labels.debug_image()))
}

impl PanoApp {
    /// Height of the seam panel under the photo: the seam map at full width, at most `SEAM_PANEL_MAX_H`
    /// and at most `SEAM_PANEL_SHARE` of the room, plus its title lines.
    pub(super) fn seam_panel_height(&self, width: f32, room: f32) -> f32 {
        let aspect = self.mk.seam_tex.as_ref().map(|t| t.size_vec2()).map(|s| s.x / s.y.max(1.0)).unwrap_or(self.view.aspect() as f32).max(0.5);
        (width / aspect).min(SEAM_PANEL_MAX_H).min(room * SEAM_PANEL_SHARE) + SEAM_PANEL_TITLE_H
    }

    /// Changes whenever the seams could look different (alignment, photos used, masks).
    pub(super) fn seam_key(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.align_version.hash(&mut h);
        self.prefer_detail.hash(&mut h);
        for i in self.active_indices() {
            (i, self.photos[i].mask.rev).hash(&mut h);
        }
        h.finish()
    }

    /// Starts the seam preview if the masks or the alignment changed since the last one.
    pub(super) fn update_seam_preview(&mut self) {
        let key = self.seam_key();
        if self.mk.seam_rx.is_some() || (self.mk.seam_key == Some(key)) || !self.aligned || self.active_indices().len() < 2 {
            return;
        }
        self.mk.seam_key = Some(key);
        let (photos, view, detail) = (self.render_photos(true), self.view.clone(), self.prefer_detail);
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(seam_image(photos, &view, detail));
        });
        self.mk.seam_rx = Some(rx);
    }

    /// Takes a finished seam preview from the worker thread.
    pub(super) fn poll_seam_preview(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.mk.seam_rx else { return };
        match rx.try_recv() {
            Ok(Ok(img)) => {
                self.mk.seam_tex = Some(ctx.load_texture("mask-seams", img, egui::TextureOptions::NEAREST));
                self.mk.seam_err = None;
                self.mk.seam_rx = None;
            }
            Ok(Err(e)) => {
                self.mk.seam_err = Some(e);
                self.mk.seam_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.mk.seam_err = Some("The seam preview stopped unexpectedly.".into());
                self.mk.seam_rx = None;
            }
        }
    }

    /// The seam map next to the photo.
    pub(super) fn seam_panel(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Seams in the panorama").strong());
        ui.label(RichText::new("Each colour is one photo, black lines are the seams. It updates after every change.").small().color(MUTED));
        if !self.aligned {
            ui.label(RichText::new("Align the photos first.").color(WARN));
            return;
        }
        if let Some(e) = &self.mk.seam_err {
            ui.label(RichText::new(format!("Could not find the seams: {e}")).color(BAD));
        }
        if let Some(t) = &self.mk.seam_tex {
            let s = t.size_vec2();
            // as wide as there is room, but not taller than its share of the window
            let w = ui.available_width().min(SEAM_PANEL_MAX_H * s.x / s.y.max(1.0)).min(ui.available_height().max(80.0) * s.x / s.y.max(1.0));
            ui.add(egui::Image::new(egui::load::SizedTexture::new(t.id(), Vec2::new(w, w * s.y / s.x))).corner_radius(CornerRadius::same(RADIUS)));
        }
        if self.mk.seam_rx.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Finding seams…").color(MUTED));
            });
        }
    }
}
