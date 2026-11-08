//! The big photo of the mask window: zoom, move, paint, and the mask drawn over the photo.

use super::masks_tool::{screen_radius, to_screen, Ptr};
use super::masks_ui::{overlay_image, MASK_ALPHA};
use super::*;
use crate::masks::{MaskKind, MaskStroke};

fn kind_color(k: MaskKind, alpha: u8) -> Color32 {
    let c = match k {
        MaskKind::Positive => MASK_GREEN,
        MaskKind::Negative => MASK_RED,
        MaskKind::Erase => Color32::WHITE,
    };
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

/// Zoom `view` by `f` around the screen position `pos`, keeping the photo point below it in place.
pub(super) fn zoom_view(view: &mut PaneView, g: &PaneGeo, pos: Pos2, f: f32) {
    let p_img = (pos - g.origin) / g.scale;
    view.zoom = (view.zoom * f).clamp(0.5, 80.0);
    let ns = g.fit * view.zoom;
    view.center = p_img - (pos - g.rect.center()) / ns;
    view.center.x = view.center.x.clamp(0.0, g.w);
    view.center.y = view.center.y.clamp(0.0, g.h);
}

impl PanoApp {
    /// The photo with its mask; handles the input.
    pub(super) fn mask_canvas(&mut self, ui: &mut egui::Ui, size: Vec2) {
        let sel = self.mk.sel;
        let (resp, painter) = ui.allocate_painter(size, Sense::click_and_drag());
        let rect = resp.rect;
        let painter = painter.with_clip_rect(rect);
        let (w, h) = self.work_dims(sel);
        let fit = (rect.width() / w as f32).min(rect.height() / h as f32);
        if self.mk.view.key != sel + 1 {
            self.mk.view = PaneView { zoom: 1.0, center: Vec2::new(w as f32 / 2.0, h as f32 / 2.0), key: sel + 1 };
        }
        let scale = fit * self.mk.view.zoom;
        let origin = rect.center() - self.mk.view.center * scale;
        self.mk.geo = PaneGeo { rect, origin, scale, fit, w: w as f32, h: h as f32 };
        let geo = self.mk.geo;
        painter.rect_filled(rect, f32::from(RADIUS_CANVAS), BG);
        let img_rect = Rect::from_min_size(origin, Vec2::new(w as f32 * scale, h as f32 * scale));
        let full = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        if let Some(t) = self.photos[sel].tex.as_ref() {
            painter.image(t.id(), img_rect, full, Color32::WHITE);
        }
        self.paint_mask_overlay(ui.ctx(), &painter, img_rect);
        self.paint_unfinished(&painter, &geo, resp.hover_pos());
        painter.text(rect.left_top() + Vec2::new(10.0, 8.0), Align2::LEFT_TOP, format!("Photo {}  ·  {:.0} %", sel + 1, self.mk.view.zoom * 100.0), FontId::proportional(13.0), Color32::WHITE);
        self.mask_input(ui, &resp);
    }

    /// The mask as a see-through picture over the photo.
    fn paint_mask_overlay(&mut self, ctx: &egui::Context, painter: &egui::Painter, img_rect: Rect) {
        let sel = self.mk.sel;
        if self.mk.hide_mask {
            return;
        }
        let m = &self.photos[sel].mask;
        let Some(buf) = m.buf.clone() else { return };
        if self.mk.overlay.as_ref().is_none_or(|(i, rev, _)| *i != sel || *rev != m.rev) {
            let tex = ctx.load_texture("mask-overlay", overlay_image(&buf), egui::TextureOptions::LINEAR);
            self.mk.overlay = Some((sel, m.rev, tex));
        }
        if let Some((_, _, t)) = &self.mk.overlay {
            painter.image(t.id(), img_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
    }

    /// The stroke or lasso that is being drawn, and the brush circle under the pointer.
    fn paint_unfinished(&self, painter: &egui::Painter, geo: &PaneGeo, hover: Option<Pos2>) {
        let t = &self.mk.tool;
        let ring = if t.tool.kind() == MaskKind::Erase { Color32::WHITE } else { kind_color(t.tool.kind(), 255) };
        if let Some(s) = &t.stroke {
            paint_brush_stroke(painter, geo, s);
        }
        if t.tool.is_lasso() {
            let pts: Vec<Pos2> = t.corners.iter().map(|&c| to_screen(geo, c)).collect();
            let col = kind_color(t.tool.kind(), 255);
            let mut line = pts.clone();
            if let (Some(h), false) = (hover, pts.is_empty()) {
                line.push(h);
            }
            if line.len() >= 2 {
                painter.add(egui::Shape::line(line, Stroke::new(2.0, col)));
            }
            for p in &pts {
                painter.circle_filled(*p, 3.5, col);
                painter.circle_stroke(*p, 3.5, Stroke::new(1.0, Color32::BLACK));
            }
        } else if let Some(h) = hover {
            let r = screen_radius(geo, t.radius).max(2.0);
            painter.circle_stroke(h, r, Stroke::new(2.5, Color32::BLACK));
            painter.circle_stroke(h, r, Stroke::new(1.2, ring));
        }
    }

    /// Mouse and keyboard: zoom, move (right or middle button), paint (left button), undo and redo.
    fn mask_input(&mut self, ui: &mut egui::Ui, resp: &egui::Response) {
        let sel = self.mk.sel;
        let geo = self.mk.geo;
        let (w, h) = (self.photos[sel].w as usize, self.photos[sel].h as usize);
        if resp.hovered() {
            let sc = ui.input(|i| i.smooth_scroll_delta.y);
            if let (true, Some(hp)) = (sc.abs() > 0.1, resp.hover_pos()) {
                zoom_view(&mut self.mk.view, &geo, hp, (sc * 0.0025).exp());
            }
        }
        if resp.dragged_by(egui::PointerButton::Secondary) || resp.dragged_by(egui::PointerButton::Middle) {
            let v = &mut self.mk.view;
            v.center -= resp.drag_delta() / (geo.fit * v.zoom);
            v.center.x = v.center.x.clamp(0.0, geo.w);
            v.center.y = v.center.y.clamp(0.0, geo.h);
        }
        let prim = egui::PointerButton::Primary;
        let mut events: Vec<Ptr> = Vec::new();
        let cur = resp.interact_pointer_pos();
        if resp.drag_started_by(prim) {
            events.push(Ptr::Down(ui.input(|i| i.pointer.press_origin()).or(cur).unwrap_or(geo.rect.center())));
        }
        if resp.dragged_by(prim) {
            events.extend(cur.map(Ptr::Move));
        }
        if resp.drag_stopped_by(prim) {
            events.push(Ptr::Up);
        }
        if let (true, Some(p)) = (resp.clicked_by(prim), cur) {
            if self.mk.tool.tool.is_lasso() {
                events.push(if resp.double_clicked() { Ptr::DoubleClick } else { Ptr::Click(p) });
            } else {
                events.extend([Ptr::Down(p), Ptr::Up]);
            }
        }
        // In its own OS window the keys belong to the window while it has the focus (egui input is
        // per window, so the main window's shortcuts cannot clash). Embedded in the main window
        // they only count while the pointer is over it, so other steps keep their own shortcuts.
        let own_window = ui.ctx().viewport_id() != egui::ViewportId::ROOT;
        if (own_window && ui.input(|i| i.focused)) || (!own_window && ui.rect_contains_pointer(ui.clip_rect())) {
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                events.push(Ptr::Enter);
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                events.push(Ptr::Escape);
            }
            self.mask_shortcuts(ui, w, h);
        }
        for ev in events {
            self.mk.tool.handle(ev, &geo, &mut self.photos[sel].mask, w, h);
        }
    }

    /// Ctrl+Z, Ctrl+Y, Ctrl+Shift+Z.
    fn mask_shortcuts(&mut self, ui: &mut egui::Ui, w: usize, h: usize) {
        use egui::{Key, Modifiers};
        let sel = self.mk.sel;
        let redo = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || i.consume_key(Modifiers::COMMAND, Key::Y));
        let undo = !redo && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Z));
        if redo {
            self.mk.tool.cancel();
            self.photos[sel].mask.redo(w, h);
        } else if undo && !self.mk.tool.pop_corner() {
            self.photos[sel].mask.undo(w, h);
        }
    }
}

/// A brush stroke that is not finished yet (round brush along the points).
fn paint_brush_stroke(painter: &egui::Painter, geo: &PaneGeo, s: &MaskStroke) {
    let r = screen_radius(geo, s.radius).max(1.0);
    let col = kind_color(s.kind, MASK_ALPHA);
    let pts: Vec<Pos2> = s.points.iter().map(|&p| to_screen(geo, p)).collect();
    if pts.len() >= 2 {
        painter.add(egui::Shape::line(pts.clone(), Stroke::new(2.0 * r, col)));
    }
    for p in [pts.first(), pts.last()].into_iter().flatten() {
        painter.circle_filled(*p, r, col);
    }
}
