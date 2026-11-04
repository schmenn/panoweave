//! The "Masks" window: choose a photo, paint where it must (green) or must not (red) be used.
//! The strokes live in `Photo::mask`; the painting itself is in `masks_canvas.rs`.

use super::masks_tool::{MaskTool, Tool, MAX_BRUSH, MIN_BRUSH};
use super::viewport_window::WinSpec;
use super::*;

/// Colours of the mask overlay and the tools.
pub(super) const MASK_GREEN: Color32 = Color32::from_rgb(60, 200, 90);
pub(super) const MASK_RED: Color32 = Color32::from_rgb(235, 70, 60);
/// How see-through the mask is drawn over the photo (0..255).
pub(super) const MASK_ALPHA: u8 = 102;

/// State of the mask window (not saved in the project; the strokes are).
#[derive(Default)]
pub(super) struct MaskUi {
    pub(super) open: bool,
    /// photo shown
    pub(super) sel: usize,
    pub(super) tool: MaskTool,
    pub(super) view: PaneView,
    pub(super) geo: PaneGeo,
    /// do not draw the mask over the photo
    pub(super) hide_mask: bool,
    pub(super) show_seams: bool,
    /// overlay picture of the shown photo: (photo, mask revision, texture)
    pub(super) overlay: Option<(usize, u64, egui::TextureHandle)>,
    pub(super) seam_tex: Option<egui::TextureHandle>,
    pub(super) seam_rx: Option<Receiver<Result<egui::ColorImage, String>>>,
    pub(super) seam_key: Option<u64>,
    pub(super) seam_err: Option<String>,
}

/// The mask as a see-through picture: green where the photo must be used, red where it must not.
pub(super) fn overlay_image(m: &crate::masks::MaskBuf) -> egui::ColorImage {
    let px = m
        .data
        .iter()
        .map(|&v| match v {
            crate::masks::POSITIVE => Color32::from_rgba_unmultiplied(MASK_GREEN.r(), MASK_GREEN.g(), MASK_GREEN.b(), MASK_ALPHA),
            crate::masks::NEGATIVE => Color32::from_rgba_unmultiplied(MASK_RED.r(), MASK_RED.g(), MASK_RED.b(), MASK_ALPHA),
            _ => Color32::TRANSPARENT,
        })
        .collect();
    egui::ColorImage { size: [m.w, m.h], source_size: Vec2::new(m.w as f32, m.h as f32), pixels: px }
}

fn tool_color(t: Tool) -> Color32 {
    match t.kind() {
        crate::masks::MaskKind::Positive => MASK_GREEN,
        crate::masks::MaskKind::Negative => MASK_RED,
        crate::masks::MaskKind::Erase => Color32::WHITE,
    }
}

impl PanoApp {
    /// How many photos have a mask.
    pub(super) fn masked_photos(&self) -> usize {
        self.photos.iter().filter(|p| p.mask.any()).count()
    }

    /// The "Edit masks..." button of the Blending card (only for Smooth seams).
    pub(super) fn mask_button(&mut self, ui: &mut egui::Ui) {
        let smooth = self.blend == Blend::Smooth;
        ui.horizontal(|ui| {
            let b = ui.add_enabled(smooth, egui::Button::new("🖌  Edit masks…")).on_hover_text("Paint where a photo must or must not be used, for example to keep a pole or a person whole.");
            if !smooth {
                b.on_disabled_hover_text("Masks only steer Smooth seams.");
                ui.label(RichText::new("Masks only steer Smooth seams.").small().color(MUTED));
            } else if b.clicked() {
                self.mk.open = true;
            }
            let n = self.masked_photos();
            if n > 0 {
                ui.label(RichText::new(format!("{n} photo{} with masks", if n == 1 { "" } else { "s" })).small().color(MASK_GREEN));
            }
        });
    }

    pub(super) fn mask_window(&mut self, ctx: &egui::Context) {
        self.poll_seam_preview(ctx);
        let mut open = self.mk.open;
        let spec = WinSpec { key: "masks", title: "Masks".into(), size: [1000.0, 660.0], min_size: [640.0, 420.0], fill: None };
        let mut vp = std::mem::take(&mut self.vp);
        vp.show(ctx, &spec, &mut open, self.demo.is_some(), |ui| self.mask_contents(ui));
        self.vp = vp;
        self.mk.open = open;
        if !open {
            self.mk.tool.cancel();
        }
    }

    fn mask_contents(&mut self, ui: &mut egui::Ui) {
        if self.photos.is_empty() {
            ui.label(RichText::new("Add photos first.").color(MUTED));
            return;
        }
        if self.mk.sel >= self.photos.len() {
            self.mk.sel = 0;
        }
        let ctx = ui.ctx().clone();
        self.ensure_tex(&ctx, self.mk.sel);
        ui.label(RichText::new("Paint green where this photo must be used, red where it must not be used. Used by Smooth seams.").color(MUTED));
        if self.mk.show_seams {
            self.update_seam_preview();
        }
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(170.0);
                self.mask_photo_list(ui);
            });
            ui.vertical(|ui| {
                self.mask_toolbar(ui);
                ui.horizontal_top(|ui| {
                    let seam_w = if self.mk.show_seams { 300.0 } else { 0.0 };
                    let canvas = Vec2::new((ui.available_width() - seam_w - 12.0).max(260.0), (ui.available_height() - 4.0).max(300.0));
                    self.mask_canvas(ui, canvas);
                    if self.mk.show_seams {
                        ui.vertical(|ui| {
                            ui.set_width(seam_w);
                            self.seam_panel(ui);
                        });
                    }
                });
            });
        });
    }

    /// Photos with a small picture; photos with a mask are marked.
    fn mask_photo_list(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(520.0).show(ui, |ui| {
            let mut pick = None;
            for (i, p) in self.photos.iter().enumerate() {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 56.0), Sense::click());
                let sel = i == self.mk.sel;
                let fill = if sel { Color32::from_rgb(38, 50, 68) } else if resp.hovered() { Color32::from_rgb(44, 48, 57) } else { PANEL };
                ui.painter().rect_filled(rect, 6.0, fill);
                if sel {
                    ui.painter().rect_stroke(rect, 6.0, Stroke::new(1.5, ACCENT), StrokeKind::Inside);
                }
                let ts = p.thumb.size_vec2();
                let tw = 64.0f32.min(50.0 * ts.x / ts.y);
                let tr = Rect::from_min_size(rect.min + Vec2::new(4.0, 3.0), Vec2::new(tw, tw * ts.y / ts.x));
                ui.painter().image(p.thumb.id(), tr, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), if p.enabled { Color32::WHITE } else { Color32::from_gray(90) });
                let tx = rect.min.x + 72.0;
                ui.painter().text(Pos2::new(tx, rect.min.y + 8.0), Align2::LEFT_TOP, format!("{}. {}", i + 1, p.name), FontId::proportional(12.5), Color32::WHITE);
                let (t, c) = if p.mask.any() { ("● has a mask", MASK_GREEN) } else { ("no mask", MUTED) };
                ui.painter().text(Pos2::new(tx, rect.min.y + 30.0), Align2::LEFT_TOP, t, FontId::proportional(11.5), c);
                if resp.clicked() {
                    pick = Some(i);
                }
            }
            if let Some(i) = pick {
                if i != self.mk.sel {
                    self.mk.sel = i;
                    self.mk.tool.cancel();
                }
            }
        });
    }

    fn mask_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            for t in Tool::ALL {
                let txt = RichText::new(t.label()).color(tool_color(t));
                if ui.selectable_label(self.mk.tool.tool == t, txt).clicked() && self.mk.tool.tool != t {
                    self.mk.tool.cancel();
                    self.mk.tool.tool = t;
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            let sel = self.mk.sel;
            ui.add(egui::Slider::new(&mut self.mk.tool.radius, MIN_BRUSH..=MAX_BRUSH).logarithmic(true).fixed_decimals(3).text("Brush size"))
                .on_hover_text("Fraction of the long side of the photo. Also the size of the eraser.");
            let (w, h) = (self.photos[sel].w as usize, self.photos[sel].h as usize);
            if ui.add_enabled(self.photos[sel].mask.can_undo(), egui::Button::new("↶ Undo")).on_hover_text("Ctrl+Z").clicked() {
                self.photos[sel].mask.undo(w, h);
            }
            if ui.add_enabled(self.photos[sel].mask.can_redo(), egui::Button::new("↷ Redo")).on_hover_text("Ctrl+Y or Ctrl+Shift+Z").clicked() {
                self.photos[sel].mask.redo(w, h);
            }
            if ui.add_enabled(!self.photos[sel].mask.strokes.is_empty(), egui::Button::new("Clear this photo")).clicked() {
                self.photos[sel].mask.clear(w, h);
            }
            if ui.button("⤢ Fit").on_hover_text("Show the whole photo").clicked() {
                self.mk.view.key = 0;
            }
            ui.checkbox(&mut self.mk.hide_mask, "Hide mask");
            let aligned = self.aligned;
            ui.add_enabled(aligned, egui::Checkbox::new(&mut self.mk.show_seams, "Show seams"))
                .on_hover_text("Shows where the seam finder puts the seams, with your masks. Takes a few seconds after each change.")
                .on_disabled_hover_text("Needs an alignment: align the photos first (steps Matching points and Align).");
        });
        let hint = if self.mk.tool.tool.is_lasso() {
            "Lasso: click to set corners. Double click or Enter closes the shape, Esc cancels. Scroll to zoom, drag with the right mouse button to move."
        } else {
            "Brush: press and drag. Scroll to zoom, drag with the right mouse button to move the photo."
        };
        ui.label(RichText::new(hint).small().color(MUTED));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::masks::{MaskBuf, NEGATIVE, NEUTRAL, POSITIVE};

    #[test]
    fn the_overlay_is_green_red_and_see_through() {
        let m = MaskBuf { w: 3, h: 1, data: vec![POSITIVE, NEGATIVE, NEUTRAL] };
        let img = overlay_image(&m);
        assert_eq!(img.size, [3, 1]);
        assert_eq!(img.pixels[0], Color32::from_rgba_unmultiplied(60, 200, 90, MASK_ALPHA));
        assert_eq!(img.pixels[1], Color32::from_rgba_unmultiplied(235, 70, 60, MASK_ALPHA));
        assert_eq!(img.pixels[2], Color32::TRANSPARENT);
        assert!((MASK_ALPHA as f32 / 255.0 - 0.4).abs() < 0.01);
    }
}
