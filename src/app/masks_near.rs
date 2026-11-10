//! Near objects in the Masks window: "Keep seams off near objects". The proposal comes from `near_seams.rs`; it is drawn
//! over the photo and only applied on "Apply". Applied, it is an ordinary set of mask strokes (kind `Guard`, marked
//! `auto`) that is seen and changed in the editor like any other, one undo step per photo, and "Remove automatic
//! masks" takes them all away again. The seam finder then keeps its seams off these outlines.

use super::*;
use crate::near_seams::{self, NearObject, NearSeamConfig};

impl PanoApp {
    /// Near objects of the current alignment and the proposal for each. The regions are looked for again in the errors
    /// of the alignment as it is now (so it also works after loading a project).
    pub(super) fn near_plan(&self) -> Vec<NearObject> {
        near_seams::plan(&self.near_layers(), self.photos.len(), &NearSeamConfig::default().with_env())
    }

    /// The near objects (per photo pair) of the alignment as it is now.
    pub(super) fn near_layers(&self) -> crate::depth::Layers {
        let dcfg = self.fine_cfg.depth;
        let mut layers = self.layers.clone();
        let active = self.active_indices();
        let cps: Vec<ControlPoint> = self.cps.iter().filter(|c| active.contains(&c.a) && active.contains(&c.b)).copied().collect();
        let lens_of: Vec<usize> = self.photos.iter().map(|p| p.lens).collect();
        let long: Vec<f64> = self.photos.iter().map(|p| p.long()).collect();
        let rots: Vec<M3> = self.photos.iter().map(|p| p.rot).collect();
        let dims: Vec<(usize, usize)> = self.photos.iter().map(|p| (p.gray.w, p.gray.h)).collect();
        if self.aligned && !self.lenses.is_empty() {
            crate::depth::refresh(&mut layers, &lens_of, &long, &self.lenses, &rots, &cps, &dims, &dcfg);
        }
        layers
    }

    /// Puts the proposed outlines on the photos (after taking away the ones proposed before). One undo step per
    /// photo. Returns the number of strokes added.
    pub(super) fn apply_near_seams(&mut self, objs: &[NearObject]) -> usize {
        let cfg = NearSeamConfig::default().with_env();
        let mut per_photo: Vec<Vec<crate::masks::MaskStroke>> = vec![Vec::new(); self.photos.len()];
        for o in objs {
            for (p, s) in near_seams::strokes(o, &cfg) {
                if let Some(v) = per_photo.get_mut(p) {
                    v.push(s);
                }
            }
        }
        let mut n = 0;
        for (i, strokes) in per_photo.into_iter().enumerate() {
            let (w, h) = (self.photos[i].w as usize, self.photos[i].h as usize);
            let had = self.photos[i].mask.strokes.iter().any(|s| s.auto);
            if strokes.is_empty() && !had {
                continue;
            }
            n += strokes.len();
            self.photos[i].mask.replace_auto(strokes, w, h);
        }
        self.preview_dirty = true;
        n
    }

    /// Takes away every automatic mask (one undo step per photo). Returns how many strokes went.
    pub(super) fn remove_auto_masks(&mut self) -> usize {
        let mut n = 0;
        for p in self.photos.iter_mut() {
            let (w, h) = (p.w as usize, p.h as usize);
            n += p.mask.replace_auto(Vec::new(), w, h);
        }
        self.preview_dirty = true;
        n
    }

    /// Whether any photo has an automatic mask.
    pub(super) fn has_auto_masks(&self) -> bool {
        self.photos.iter().any(|p| p.mask.strokes.iter().any(|s| s.auto))
    }

    /// The outline discs of the waiting proposal that lie in photo `photo` (drawn over the photo before Apply).
    pub(super) fn near_preview(&self, photo: usize) -> Vec<crate::masks::MaskStroke> {
        let cfg = NearSeamConfig::default().with_env();
        let Some(plan) = &self.mk.near else { return Vec::new() };
        plan.iter().flat_map(|o| near_seams::strokes(o, &cfg)).filter(|(p, _)| *p == photo).map(|(_, s)| s).collect()
    }

    /// The row in the Masks window: find, show and apply the proposal.
    pub(super) fn near_seams_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let tip = "A car or railing close to the camera shows twice or blurred where two photos are blended. This finds such objects and proposes a tight outline of each; with Apply the seams are not allowed to cut through the outline, they run round it (which photo shows which part is decided as before). The outline is shown in the photo first. Nothing changes until you press Apply; the outline is then an ordinary mask you can change or erase.";
            if ui.add_enabled(self.aligned, egui::Button::new("🚗  Keep seams off near objects")).on_hover_text(tip).on_disabled_hover_text("Needs an alignment: align the photos first.").clicked() {
                self.mk.near = Some(self.near_plan());
                // show a photo that has an outline
                if let Some(p) = self.mk.near.as_ref().and_then(|v| v.first()).and_then(|o| o.photos.first()).map(|p| p.0) {
                    self.mk.sel = p;
                }
            }
            if ui.add_enabled(self.has_auto_masks(), egui::Button::new("Remove automatic masks")).on_hover_text("Takes away the outlines this button proposed and you applied (masks you painted stay).").clicked() {
                let n = self.remove_auto_masks();
                self.mk.near = None;
                self.set_status(format!("{n} automatic mask stroke{} removed.", if n == 1 { "" } else { "s" }), GOOD);
            }
        });
        let Some(plan) = self.mk.near.clone() else { return };
        if plan.is_empty() {
            ui.label(RichText::new("No sure near object found (several photo pairs, or many points, must agree on a clear shift).").small().color(MUTED));
            return;
        }
        for o in &plan {
            ui.label(RichText::new(o.text()).small().color(ACCENT));
        }
        ui.horizontal_wrapped(|ui| {
            if ui.button("Apply").on_hover_text("Adds the outlines as masks (an earlier proposal is replaced).").clicked() {
                let n = self.apply_near_seams(&plan);
                self.mk.near = None;
                self.set_status(format!("{n} outline stroke{} added for {} near object{}. Change them in the editor or press “Remove automatic masks”.", if n == 1 { "" } else { "s" }, plan.len(), if plan.len() == 1 { "" } else { "s" }), GOOD);
            }
            if ui.button("Cancel").clicked() {
                self.mk.near = None;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::masks_tests::app_with_photos;
    use super::*;
    use crate::depth::{CoarseMask, NearRegion, Source};
    use crate::masks::MaskKind;

    fn cells(x0: usize) -> CoarseMask {
        let mut m = CoarseMask::default();
        for y in 6..9 {
            for xx in x0..x0 + 3 {
                m.set(xx, y);
            }
        }
        m
    }

    fn pts(x0: f32) -> Vec<[f32; 2]> {
        (0..15).map(|i| [x0 + 0.03 * (i % 5) as f32, 0.4 + 0.03 * (i / 5) as f32]).collect()
    }

    fn two_photo_app() -> (PanoApp, Vec<NearObject>) {
        let app = app_with_photos(2);
        let layers = crate::depth::Layers {
            regions: vec![NearRegion { a: 0, b: 1, mask_a: cells(6), mask_b: cells(10), dims_a: (120, 80), dims_b: (120, 80), shift_px: 20.0, count: 60, source: Source::Residuals, pts_a: pts(0.4), pts_b: pts(0.65) }],
        };
        let plan = near_seams::plan(&layers, 2, &NearSeamConfig::default());
        (app, plan)
    }

    #[test]
    fn applied_outlines_are_guards_on_every_photo_and_can_be_removed() {
        let (mut app, plan) = two_photo_app();
        assert_eq!(plan.len(), 1);
        let n = app.apply_near_seams(&plan);
        assert_eq!(n, 30);
        for (p, x) in [(0usize, 0.46f32), (1, 0.71)] {
            assert!(app.photos[p].mask.strokes.iter().all(|s| s.auto && s.kind == MaskKind::Guard), "photo {p}");
            assert_eq!(app.photos[p].mask.buf.as_ref().unwrap().at(x * 120.0, 0.43 * 80.0, 120.0, 80.0), crate::masks::GUARD, "photo {p}");
        }
        // applying again replaces the proposal instead of stacking it
        assert_eq!(app.apply_near_seams(&plan), 30);
        assert_eq!(app.photos[0].mask.strokes.len(), 15);
        // one undo step per photo takes the proposal back
        assert!(app.photos[0].mask.undo(120, 80) && app.photos[0].mask.undo(120, 80));
        assert!(app.photos[0].mask.strokes.is_empty());
        app.apply_near_seams(&plan);
        assert!(app.has_auto_masks());
        assert_eq!(app.remove_auto_masks(), 30);
        assert!(!app.has_auto_masks() && !app.photos[0].mask.any() && !app.photos[1].mask.any());
    }

    #[test]
    fn masks_painted_by_hand_stay_and_paint_over_the_outline() {
        let (mut app, plan) = two_photo_app();
        let (w, h) = (app.photos[0].w as usize, app.photos[0].h as usize);
        // a red dab exactly on an outline point
        let spot = plan[0].photos[0].1[0];
        app.photos[0].mask.commit(crate::masks::MaskStroke { kind: MaskKind::Negative, points: vec![spot], radius: 0.01, polygon: false, auto: false }, w, h);
        app.apply_near_seams(&plan);
        assert_eq!(app.photos[0].mask.strokes.len(), 16);
        assert_eq!(app.photos[0].mask.buf.as_ref().unwrap().at(spot[0] * w as f32, spot[1] * h as f32, w as f64, h as f64), crate::masks::NEGATIVE, "the person's stroke wins");
        app.remove_auto_masks();
        assert_eq!(app.photos[0].mask.strokes.len(), 1);
        assert!(!app.photos[0].mask.strokes[0].auto);
    }

    #[test]
    fn nothing_is_applied_without_a_proposal_and_the_export_gets_the_guards() {
        let mut app = app_with_photos(2);
        assert_eq!(app.apply_near_seams(&[]), 0);
        assert!(app.render_photos(true).iter().all(|r| r.mask.is_none()));
        let (mut app2, plan) = two_photo_app();
        // the waiting proposal is only drawn, nothing is in the masks yet
        app2.mk.near = Some(plan.clone());
        assert_eq!(app2.near_preview(0).len(), 15);
        assert!(app2.render_photos(true).iter().all(|r| r.mask.is_none()));
        app2.apply_near_seams(&plan);
        assert!(app2.render_photos(true).iter().all(|r| r.mask.is_some()));
    }
}
