use super::*;

impl PanoApp {
    pub(super) fn fit_view(&mut self) {
        let rp = self.render_photos(true);
        stitch::fit_bounds(&mut self.view, &rp);
        self.preview_dirty = true;
    }

    pub(super) fn auto_crop(&mut self) {
        let rp = self.render_photos(true);
        self.view.crop = stitch::auto_crop(&rp, &self.view);
        self.preview_dirty = true;
    }

    /// Pick a sensible projection from the covered field of view.
    pub(super) fn choose_projection(&mut self) {
        self.view.projection = Projection::Rectilinear;
        self.view.yaw = 0.0;
        self.view.pitch = 0.0;
        self.view.roll = 0.0;
        self.view.projection = Projection::Equirectangular;
        self.fit_view();
        let (h, v) = self.view.fov_deg();
        self.view.projection = if h < 100.0 && v < 90.0 {
            Projection::Rectilinear
        } else if v > 120.0 {
            Projection::Equirectangular
        } else {
            Projection::Cylindrical
        };
        self.fit_view();
    }

    pub(super) fn create_auto(&mut self) {
        if self.photos.len() < 2 {
            self.set_status("Add at least two overlapping photos first.", WARN);
            return;
        }
        self.run_match(true);
    }

    // ----- step 4 -----
    pub(super) fn ui_frame(&mut self, ui: &mut egui::Ui) {
        if !self.aligned {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("The preview appears once the photos are aligned.").size(16.0).color(MUTED));
                ui.add_space(8.0);
                if primary_button(ui, "✨  Create panorama", self.photos.len() >= 2 && !self.busy()).clicked() {
                    self.create_auto();
                }
            });
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.label("Projection:");
            let before = self.view.projection;
            egui::ComboBox::from_id_salt("proj").width(220.0).selected_text(self.view.projection.name()).show_ui(ui, |ui| {
                for p in Projection::ALL {
                    ui.selectable_value(&mut self.view.projection, p, p.name()).on_hover_text(p.hint());
                }
            });
            if before != self.view.projection {
                self.fit_view();
                self.auto_crop();
            }
            if ui.button("⤢ Fit to photos").on_hover_text("Show all photos, remove the crop").clicked() {
                self.fit_view();
            }
            if ui.button("✂ Auto crop").on_hover_text("Largest rectangle without empty edges").clicked() {
                self.auto_crop();
            }
            if ui.button("No crop").clicked() {
                self.view.crop = [0.0, 1.0, 0.0, 1.0];
                self.preview_dirty = true;
            }
            if ui.button("⟲ Reset view").clicked() {
                self.view.yaw = 0.0;
                self.view.pitch = 0.0;
                self.view.roll = 0.0;
                self.fit_view();
            }
            if ui.checkbox(&mut self.match_brightness, "Match brightness").changed() {
                self.preview_dirty = true;
            }
            ui.checkbox(&mut self.show_outlines, "Show photo borders");
        });
        if let Some(n) = self.mask_hole_note() {
            ui.label(RichText::new(format!("⚠ {n}")).color(WARN));
        }
        let (hf, vf) = self.view.fov_deg();
        let nw = self.natural_width();
        let cb = self.view.crop_bounds();
        let nh = nw * (cb[3] - cb[2]) / (cb[1] - cb[0]);
        ui.label(
            RichText::new(format!(
                "{} · view {:.0}° × {:.0}° · full size {:.0} × {:.0} px   —   drag to move, Shift+drag to tilt, scroll to zoom, drag the crop edges to adjust",
                self.view.projection.hint(),
                hf,
                vf,
                nw,
                nh
            ))
            .small()
            .color(MUTED),
        );
        let avail = ui.available_size() - Vec2::new(0.0, 4.0);
        let (resp, painter) = ui.allocate_painter(avail, Sense::click_and_drag());
        painter.rect_filled(resp.rect, f32::from(RADIUS_CANVAS), BG);
        let aspect = self.view.aspect() as f32;
        let mut r = resp.rect.shrink(10.0);
        if r.width() / r.height() > aspect {
            let w = r.height() * aspect;
            r = Rect::from_center_size(r.center(), Vec2::new(w, r.height()));
        } else {
            let h = r.width() / aspect;
            r = Rect::from_center_size(r.center(), Vec2::new(r.width(), h));
        }
        // checkerboard behind (transparent areas)
        painter.rect_filled(r, 0.0, CANVAS);
        if let Some(t) = &self.preview_tex {
            painter.image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
        // crop frame
        let c = self.view.crop;
        let crop_rect = Rect::from_min_max(
            Pos2::new(r.min.x + c[0] as f32 * r.width(), r.min.y + c[2] as f32 * r.height()),
            Pos2::new(r.min.x + c[1] as f32 * r.width(), r.min.y + c[3] as f32 * r.height()),
        );
        painter.rect_stroke(crop_rect, 0.0, Stroke::new(2.0, Color32::WHITE), StrokeKind::Middle);
        for corner in [crop_rect.left_top(), crop_rect.right_top(), crop_rect.left_bottom(), crop_rect.right_bottom()] {
            painter.circle_filled(corner, 4.5, Color32::WHITE);
        }
        // thirds
        for k in 1..3 {
            let x = crop_rect.min.x + crop_rect.width() * k as f32 / 3.0;
            let y = crop_rect.min.y + crop_rect.height() * k as f32 / 3.0;
            painter.line_segment([Pos2::new(x, crop_rect.min.y), Pos2::new(x, crop_rect.max.y)], Stroke::new(0.5, Color32::from_white_alpha(60)));
            painter.line_segment([Pos2::new(crop_rect.min.x, y), Pos2::new(crop_rect.max.x, y)], Stroke::new(0.5, Color32::from_white_alpha(60)));
        }
        if self.show_outlines {
            let rp = self.render_photos(false);
            let m = stitch::Mapper::new(&self.view, self.view.bounds, 1000, 1000);
            let vr = self.view.rotation();
            for (i, p) in rp.iter().enumerate() {
                if !self.photos[i].enabled {
                    continue;
                }
                let pts: Vec<Option<Pos2>> = stitch::photo_outline(p, 30)
                    .into_iter()
                    .map(|d| m.to_px(d, &vr).map(|(x, y)| Pos2::new(r.min.x + x as f32 / 1000.0 * r.width(), r.min.y + y as f32 / 1000.0 * r.height())))
                    .collect();
                let col = hue(i);
                for w in pts.windows(2) {
                    if let (Some(p0), Some(p1)) = (w[0], w[1])
                        && (p0.x - p1.x).abs() < r.width() * 0.5 {
                            painter.line_segment([p0, p1], Stroke::new(1.5, col));
                        }
                }
                let centre = mtv(&p.rot, [0.0, 0.0, 1.0]);
                if let Some((x, y)) = m.to_px(centre, &vr) {
                    let pos = Pos2::new(r.min.x + x as f32 / 1000.0 * r.width(), r.min.y + y as f32 / 1000.0 * r.height());
                    painter.circle_filled(pos, 11.0, Color32::from_black_alpha(160));
                    painter.text(pos, Align2::CENTER_CENTER, format!("{}", i + 1), FontId::proportional(13.0), col);
                }
            }
        }
        // interaction
        let near_edge = |pos: Pos2| crop_edge_at(crop_rect, pos, CROP_GRAB);
        if let Some(h) = resp.hover_pos() {
            match near_edge(h) {
                Some(0) | Some(1) => ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal),
                Some(_) => ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical),
                None => ui.ctx().set_cursor_icon(egui::CursorIcon::Grab),
            }
        }
        if resp.drag_started() {
            // egui reports the start only after the pointer has moved a few pixels: test where it was pressed, not where it is now
            let pos = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()).unwrap_or_default();
            self.frame_drag = match near_edge(pos) {
                Some(e) => FrameDrag::Crop(e),
                None if ui.input(|i| i.modifiers.shift) => FrameDrag::Roll,
                None => FrameDrag::Rotate,
            };
        }
        let mut fast = false;
        if resp.dragged() {
            let d = resp.drag_delta();
            let b = self.view.bounds;
            let ux = (b[1] - b[0]) / r.width() as f64;
            let uy = (b[3] - b[2]) / r.height() as f64;
            match self.frame_drag {
                FrameDrag::Rotate => {
                    let s = if self.view.projection == Projection::Rectilinear { 0.6 } else { 1.0 };
                    self.view.yaw -= d.x as f64 * ux * s;
                    self.view.pitch = (self.view.pitch + d.y as f64 * uy * s).clamp(-1.57, 1.57);
                    self.preview_dirty = true;
                    fast = true;
                }
                FrameDrag::Roll => {
                    self.view.roll += d.x as f64 * 0.003;
                    self.preview_dirty = true;
                    fast = true;
                }
                FrameDrag::Crop(e) => {
                    if let Some(p) = resp.interact_pointer_pos() {
                        let fx = ((p.x - r.min.x) / r.width()).clamp(0.0, 1.0) as f64;
                        let fy = ((p.y - r.min.y) / r.height()).clamp(0.0, 1.0) as f64;
                        let c = &mut self.view.crop;
                        match e {
                            0 => c[0] = fx.min(c[1] - 0.02),
                            1 => c[1] = fx.max(c[0] + 0.02),
                            2 => c[2] = fy.min(c[3] - 0.02),
                            _ => c[3] = fy.max(c[2] + 0.02),
                        }
                        self.preview_dirty = true;
                        fast = true;
                    }
                }
                FrameDrag::None => {}
            }
        }
        if resp.drag_stopped() {
            if matches!(self.frame_drag, FrameDrag::Rotate | FrameDrag::Roll) {
                self.update_gains();
            }
            self.frame_drag = FrameDrag::None;
            self.preview_dirty = true;
        }
        if resp.hovered() {
            let s = ui.input(|i| i.smooth_scroll_delta.y);
            if s.abs() > 0.1 {
                let f = (1.0 - s as f64 * 0.0015).clamp(0.8, 1.25);
                let b = &mut self.view.bounds;
                let cx = 0.5 * (b[0] + b[1]);
                let cy = 0.5 * (b[2] + b[3]);
                let lim = self.view.projection.limits();
                b[0] = (cx + (b[0] - cx) * f).max(lim[0]);
                b[1] = (cx + (b[1] - cx) * f).min(lim[1]);
                b[2] = (cy + (b[2] - cy) * f).max(lim[2]);
                b[3] = (cy + (b[3] - cy) * f).min(lim[3]);
                self.preview_dirty = true;
                fast = true;
            }
        }
        if self.preview_dirty || self.preview_tex.is_none() {
            let px_w = (r.width() * ui.ctx().pixels_per_point()) as usize;
            self.request_preview(px_w.max(200), fast);
            ui.ctx().request_repaint();
        }
        if self.preview_rx.is_some() {
            ui.ctx().request_repaint();
        }
    }
}

/// How close (in points) the pointer has to be to a crop edge to grab it.
const CROP_GRAB: f32 = 8.0;

/// Which crop edge (0 left, 1 right, 2 top, 3 bottom) lies within `d` of `pos`.
fn crop_edge_at(crop_rect: Rect, pos: Pos2, d: f32) -> Option<usize> {
    let in_y = pos.y > crop_rect.min.y - d && pos.y < crop_rect.max.y + d;
    let in_x = pos.x > crop_rect.min.x - d && pos.x < crop_rect.max.x + d;
    if in_y && (pos.x - crop_rect.min.x).abs() < d {
        Some(0)
    } else if in_y && (pos.x - crop_rect.max.x).abs() < d {
        Some(1)
    } else if in_x && (pos.y - crop_rect.min.y).abs() < d {
        Some(2)
    } else if in_x && (pos.y - crop_rect.max.y).abs() < d {
        Some(3)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_edges_are_found_within_the_grab_distance() {
        let r = Rect::from_min_max(Pos2::new(100.0, 50.0), Pos2::new(300.0, 250.0));
        assert_eq!(crop_edge_at(r, Pos2::new(103.0, 120.0), 8.0), Some(0));
        assert_eq!(crop_edge_at(r, Pos2::new(297.0, 120.0), 8.0), Some(1));
        assert_eq!(crop_edge_at(r, Pos2::new(200.0, 55.0), 8.0), Some(2));
        assert_eq!(crop_edge_at(r, Pos2::new(200.0, 246.0), 8.0), Some(3));
        assert_eq!(crop_edge_at(r, Pos2::new(200.0, 150.0), 8.0), None, "inside the frame, away from the edges");
        assert_eq!(crop_edge_at(r, Pos2::new(60.0, 150.0), 8.0), None, "far outside");
    }
}
