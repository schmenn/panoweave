//! Near objects in the Masks window: "Keep near objects from one photo". The proposal comes from `near_seams.rs`;
//! it is only shown until the person presses "Apply". Applied, it is an ordinary set of mask strokes (marked `auto`)
//! that is seen and changed in the editor like any other, one undo step per photo, and "Remove automatic masks" takes
//! them all away again.

use super::*;
use crate::near_seams::{self, NearObject, NearSeamConfig};

impl PanoApp {
    /// Near objects of the current alignment and the proposal for each. The regions are looked for again in the errors
    /// of the alignment as it is now (so it also works after loading a project); the ones the last alignment found
    /// from rejected matches are kept.
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

    /// Puts the proposed masks on the photos (after taking away the ones proposed before): green on the photo that
    /// keeps an object, red on the others. One undo step per photo. Returns the number of strokes added.
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

    /// The row in the Masks window: find, show and apply the proposal.
    pub(super) fn near_seams_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let tip = "A car or railing close to the camera shows twice or blurred where two photos are blended. This finds such objects and proposes to take each from exactly one photo (the one that shows it whole and nearest to its middle): green on that photo, red on the others. Nothing changes until you press Apply; the masks are then ordinary masks you can change.";
            if ui.add_enabled(self.aligned, egui::Button::new("🚗  Keep near objects from one photo")).on_hover_text(tip).on_disabled_hover_text("Needs an alignment: align the photos first.").clicked() {
                let plan = self.near_plan();
                if plan.is_empty() {
                    self.mk.near = Some(Vec::new());
                } else {
                    self.mk.near = Some(plan);
                }
            }
            if ui.add_enabled(self.has_auto_masks(), egui::Button::new("Remove automatic masks")).on_hover_text("Takes away the masks this button proposed and you applied (masks you painted stay).").clicked() {
                let n = self.remove_auto_masks();
                self.mk.near = None;
                self.set_status(format!("{n} automatic mask stroke{} removed.", if n == 1 { "" } else { "s" }), GOOD);
            }
        });
        let Some(plan) = self.mk.near.clone() else { return };
        if plan.is_empty() {
            ui.label(RichText::new("No near object found in the alignment (needs points and patches that stay far off together in one place).").small().color(MUTED));
            return;
        }
        for o in &plan {
            ui.label(RichText::new(o.text()).small().color(if o.keeper.is_some() { MASK_GREEN } else { MUTED }));
        }
        ui.horizontal_wrapped(|ui| {
            let any = plan.iter().any(|o| o.keeper.is_some());
            if ui.add_enabled(any, egui::Button::new("Apply")).on_hover_text("Paints the proposed green and red masks (an earlier proposal is replaced).").clicked() {
                let n = self.apply_near_seams(&plan);
                self.mk.near = None;
                self.set_status(format!("{n} mask stroke{} added for {} near object{}. Change them in the editor or press “Remove automatic masks”.", if n == 1 { "" } else { "s" }, plan.iter().filter(|o| o.keeper.is_some()).count(), if plan.len() == 1 { "" } else { "s" }), GOOD);
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

    fn block(x0: usize, y0: usize) -> CoarseMask {
        let mut m = CoarseMask::default();
        for y in y0..y0 + 3 {
            for x in x0..x0 + 3 {
                m.set(x, y);
            }
        }
        m
    }

    fn two_photo_app() -> (PanoApp, Vec<NearObject>) {
        let app = app_with_photos(2);
        // photo 0 sees the object in the middle, photo 1 at its right edge: photo 0 keeps it
        let layers = crate::depth::Layers {
            regions: vec![NearRegion { a: 0, b: 1, mask_a: block(6, 6), mask_b: block(13, 6), dims_a: (120, 80), dims_b: (120, 80), shift_px: 20.0, count: 30, source: Source::Residuals }],
        };
        let plan = near_seams::plan(&layers, 2, &NearSeamConfig::default());
        (app, plan)
    }

    #[test]
    fn applied_masks_are_green_on_the_keeper_red_on_the_rest_and_can_be_removed() {
        let (mut app, plan) = two_photo_app();
        assert_eq!(plan[0].keeper, Some(0));
        let n = app.apply_near_seams(&plan);
        assert_eq!(n, 3);
        assert!(app.photos[0].mask.strokes.iter().all(|s| s.auto && s.kind == MaskKind::Positive));
        // by default only the keeper gets green; the others stay free
        assert!(app.photos[1].mask.strokes.is_empty());
        assert!(app.photos[0].mask.buf.is_some() && app.photos[1].mask.buf.is_none());
        // applying again replaces the proposal instead of stacking it
        assert_eq!(app.apply_near_seams(&plan), 3);
        assert_eq!(app.photos[0].mask.strokes.len(), 3);
        // one undo step per photo takes the proposal back
        assert!(app.photos[0].mask.undo(120, 80) && app.photos[0].mask.undo(120, 80));
        assert!(app.photos[0].mask.strokes.is_empty());
        app.apply_near_seams(&plan);
        assert!(app.has_auto_masks());
        assert_eq!(app.remove_auto_masks(), 3);
        assert!(!app.has_auto_masks() && !app.photos[0].mask.any() && !app.photos[1].mask.any());
    }

    #[test]
    fn masks_painted_by_hand_stay_when_the_automatic_ones_are_replaced_or_removed() {
        let (mut app, plan) = two_photo_app();
        let (w, h) = (app.photos[0].w as usize, app.photos[0].h as usize);
        app.photos[0].mask.commit(crate::masks::MaskStroke { kind: MaskKind::Negative, points: vec![[0.9, 0.9]], radius: 0.05, polygon: false, auto: false }, w, h);
        app.apply_near_seams(&plan);
        assert_eq!(app.photos[0].mask.strokes.len(), 4);
        app.remove_auto_masks();
        assert_eq!(app.photos[0].mask.strokes.len(), 1);
        assert!(!app.photos[0].mask.strokes[0].auto);
    }

    #[test]
    fn nothing_is_applied_without_a_keeper_and_the_export_gets_the_masks() {
        let mut app = app_with_photos(2);
        assert_eq!(app.apply_near_seams(&[]), 0);
        assert!(app.render_photos(true).iter().all(|r| r.mask.is_none()));
        let (mut app2, plan) = two_photo_app();
        app2.apply_near_seams(&plan);
        let r = app2.render_photos(true);
        assert!(r[0].mask.is_some() && r[1].mask.is_none());
    }
}
