use super::*;

/// Sizes shared by the two halves of the point editor.
pub(super) struct PaneLayout {
    pub(super) a: usize,
    pub(super) b: usize,
    pub(super) n: usize,
    pub(super) pane_w: f32,
    pub(super) pane_h: f32,
    pub(super) frame: Option<SelFrame>,
}

/// Where one photo half is drawn (screen geometry and the texture shown).
#[derive(Clone, Copy)]
struct PaneDraw {
    pane: Pane,
    idx: usize,
    rect: Rect,
    origin: Pos2,
    scale: f32,
    v: PaneView,
    tex_id: Option<egui::TextureId>,
    img_rect: Rect,
}

impl PaneDraw {
    fn to_screen(self, p: P2) -> Pos2 {
        self.origin + Vec2::new((p.0 as f32 + 0.5) * self.scale, (p.1 as f32 + 0.5) * self.scale)
    }
}

impl PanoApp {
    /// One photo half of the point editor: header, picture, points, frames, input.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw_pane(
        &mut self,
        ui: &mut egui::Ui,
        pane: Pane,
        lay: &PaneLayout,
        pts: &[(usize, P2, P2)],
        events: &mut Vec<PaneEvent>,
        geo: &mut [PaneGeo; 2],
        new_a: &mut usize,
        new_b: &mut usize,
    ) {
        let (a, b, pane_w, pane_h) = (lay.a, lay.b, lay.pane_w, lay.pane_h);
        let idx = if pane == Pane::A { a } else { b };
        ui.set_max_width(pane_w);
        self.pane_header(ui, pane, lay, events, new_a, new_b);
        let (resp, painter) = ui.allocate_painter(Vec2::new(pane_w, pane_h), Sense::click_and_drag());
        let rect = resp.rect;
        let painter = painter.with_clip_rect(rect);
        let (w, h) = self.work_dims(idx);
        let key = idx + 1;
        let fit = (rect.width() / w as f32).min(rect.height() / h as f32);
        {
            let v = &mut self.pe.views[pane.i()];
            if v.key != key {
                *v = PaneView { zoom: 1.0, center: Vec2::new(w as f32 / 2.0, h as f32 / 2.0), key };
            }
        }
        let v = self.pe.views[pane.i()];
        let scale = fit * v.zoom;
        let origin = rect.center() - v.center * scale;
        geo[pane.i()] = PaneGeo { rect, origin, scale, fit, w: w as f32, h: h as f32 };
        painter.rect_filled(rect, f32::from(RADIUS_CANVAS), BG);
        let img_rect = Rect::from_min_size(origin, Vec2::new(w as f32 * scale, h as f32 * scale));
        let tex_id = self.full_tex.get(&idx).map(|t| t.id()).or_else(|| self.photos[idx].tex.as_ref().map(|t| t.id()));
        let d = PaneDraw { pane, idx, rect, origin, scale, v, tex_id, img_rect };
        if let Some(t) = tex_id {
            painter.image(t, img_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
        self.paint_edges(ui, &painter, &d, lay.frame, pts);
        self.paint_points(&painter, &d, pts);
        self.paint_selection_frames(&painter, &d, lay.frame, pts);
        self.paint_click_marks(&painter, &d, a, b);
        self.paint_area(&painter, pane, origin, scale);
        self.paint_pending(&painter, &d);
        self.paint_select_shape(&painter, &d, &resp);
        painter.text(rect.left_top() + Vec2::new(10.0, 8.0), Align2::LEFT_TOP, format!("Photo {}", idx + 1), FontId::proportional(14.0), Color32::WHITE);
        self.paint_loupe(&painter, &d, &resp);
        self.pane_input(ui, &resp, pane, events);
    }

    /// Photo choice, fit button and zoom label above a half.
    fn pane_header(&mut self, ui: &mut egui::Ui, pane: Pane, lay: &PaneLayout, events: &mut Vec<PaneEvent>, new_a: &mut usize, new_b: &mut usize) {
        let (a, b, n, pane_w) = (lay.a, lay.b, lay.n, lay.pane_w);
        ui.horizontal(|ui| {
            if pane == Pane::A {
                let total = |i: usize| self.cps.iter().filter(|c| c.a == i || c.b == i).count();
                egui::ComboBox::from_id_salt("left_photo")
                    .width((pane_w - 190.0).max(120.0))
                    .truncate()
                    .selected_text(format!("{}. {}  —  {} points", a + 1, self.photos[a].name, total(a)))
                    .show_ui(ui, |ui| {
                        for i in 0..n {
                            ui.selectable_value(new_a, i, format!("{}. {}  —  {} points", i + 1, self.photos[i].name, total(i)));
                        }
                    });
            } else {
                let label = |j: usize| -> RichText {
                    let (c, mean, mx) = self.pair_stats(a, j);
                    let t = if c == 0 {
                        format!("{}. {}  —  no points", j + 1, self.photos[j].name)
                    } else if self.aligned {
                        format!("{}. {}  —  {} points · ⌀ {:.1} px · max {:.1} px", j + 1, self.photos[j].name, c, mean, mx)
                    } else {
                        format!("{}. {}  —  {} points", j + 1, self.photos[j].name, c)
                    };
                    let col = if c == 0 { MUTED } else if self.aligned { err_color(mx) } else { Color32::WHITE };
                    RichText::new(t).color(col)
                };
                egui::ComboBox::from_id_salt("right_photo").width((pane_w - 190.0).max(120.0)).truncate().selected_text(label(b)).show_ui(ui, |ui| {
                    ui.set_min_width(380.0);
                    for j in 0..n {
                        if j != a {
                            ui.selectable_value(new_b, j, label(j));
                        }
                    }
                });
            }
            let v = self.pe.views[pane.i()];
            if ui.small_button("⤢ Fit").clicked() {
                events.push(PaneEvent::Fit(pane));
            }
            ui.label(RichText::new(format!("{:.0} %", v.zoom.max(0.01) * 100.0)).small().color(MUTED));
        });
    }

    /// Edges around the point while it is being moved.
    fn paint_edges(&mut self, ui: &egui::Ui, painter: &egui::Painter, d: &PaneDraw, frame: Option<SelFrame>, pts: &[(usize, P2, P2)]) {
        let PaneDraw { pane, idx, origin, scale, .. } = *d;
        if let (Some((dk, _)), Some(fr)) = (self.pe.dragging, frame)
            && let Some((_, pa, pb)) = pts.iter().find(|t| t.0 == dk) {
                let p = if pane == Pane::A { *pa } else { *pb };
                let (sf, sm) = guided::step_sizes(&fr.j);
                let r = (self.tpl_size / 2) as f64;
                let half = if pane == fr.fixed {
                    2.0 * r * sf
                } else {
                    self.local_radius(idx) + r * sm
                }
                .clamp(16.0, 220.0)
                    + 6.0;
                let (img, x0, y0, ew, eh) = edge_image(&self.photos[idx].gray, p, half);
                let tid = match &mut self.edge_tex[pane.i()] {
                    Some(t) => {
                        t.set(img, egui::TextureOptions::LINEAR);
                        t.id()
                    }
                    None => {
                        let t = ui.ctx().load_texture(format!("edges-{}", pane.i()), img, egui::TextureOptions::LINEAR);
                        let id = t.id();
                        self.edge_tex[pane.i()] = Some(t);
                        id
                    }
                };
                let rmin = origin + Vec2::new(x0 as f32 * scale, y0 as f32 * scale);
                painter.rect_filled(Rect::from_min_size(rmin, Vec2::new(ew as f32 * scale, eh as f32 * scale)), 0.0, Color32::from_black_alpha(70));
                painter.image(tid, Rect::from_min_size(rmin, Vec2::new(ew as f32 * scale, eh as f32 * scale)), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            }
    }

    /// The pair's points of one half.
    fn paint_points(&self, painter: &egui::Painter, d: &PaneDraw, pts: &[(usize, P2, P2)]) {
        let PaneDraw { pane, rect, v, .. } = *d;
        for (num, (k, pa, pb)) in pts.iter().enumerate() {
            let p = if pane == Pane::A { *pa } else { *pb };
            let s = d.to_screen(p);
            if !rect.expand(10.0).contains(s) {
                continue;
            }
            let c = &self.cps[*k];
            let col = if self.aligned { err_color(c.err) } else { point_color(num) };
            let sel = self.pe.selected == Some(*k);
            if self.pe.sel.contains(k) && !sel {
                painter.circle_stroke(s, 10.0, Stroke::new(3.0, Color32::BLACK));
                painter.circle_stroke(s, 10.0, Stroke::new(1.6, ACCENT));
            }
            if sel {
                let r = 11.0;
                painter.circle_stroke(s, r, Stroke::new(3.0, Color32::BLACK));
                painter.circle_stroke(s, r, Stroke::new(1.6, Color32::WHITE));
                for d in [Vec2::new(1.0, 0.0), Vec2::new(0.0, 1.0)] {
                    painter.line_segment([s - d * (r + 6.0), s - d * 3.0], Stroke::new(1.4, Color32::WHITE));
                    painter.line_segment([s + d * 3.0, s + d * (r + 6.0)], Stroke::new(1.4, Color32::WHITE));
                }
                let lock = if c.locked { "🔒 " } else { "" };
                let info = if self.aligned { format!("{lock}#{} · {:.2} px", num + 1, c.err) } else { format!("{lock}#{}", num + 1) };
                painter.text(s + Vec2::new(14.0, -14.0), Align2::LEFT_BOTTOM, info, FontId::proportional(12.5), Color32::WHITE);
            } else {
                painter.circle_stroke(s, 6.0, Stroke::new(1.5, Color32::BLACK));
                if c.kind == CpKind::Point {
                    painter.circle_stroke(s, 5.0, Stroke::new(if c.manual { 2.4 } else { 1.8 }, col));
                } else {
                    // control area: small square
                    painter.rect_stroke(Rect::from_center_size(s, Vec2::splat(8.0)), 0.0, Stroke::new(1.4, col), StrokeKind::Middle);
                }
                if c.locked {
                    painter.text(s + Vec2::new(7.0, 7.0), Align2::LEFT_TOP, "🔒", FontId::proportional(11.0), Color32::WHITE);
                }
                if v.zoom > 2.5 {
                    painter.text(s + Vec2::new(7.0, -7.0), Align2::LEFT_BOTTOM, format!("{}", num + 1), FontId::proportional(11.0), col);
                }
            }
        }
    }

    /// Matching window and search area of the selected point.
    fn paint_selection_frames(&self, painter: &egui::Painter, d: &PaneDraw, frame: Option<SelFrame>, pts: &[(usize, P2, P2)]) {
        let PaneDraw { pane, idx, .. } = *d;
        if let (Some(fr), Some(k)) = (frame, self.pe.selected)
            && let Some((_, pa, pb)) = pts.iter().find(|t| t.0 == k) {
                let p = if pane == Pane::A { *pa } else { *pb };
                let (sf, sm) = guided::step_sizes(&fr.j);
                // half size = TemplateSize / 2 (integer), as the original draws it
                let r = (self.tpl_size / 2) as f64;
                let yellow = TEMPLATE_FRAME_COLOR;
                let blue = SEARCH_FRAME_COLOR;
                let quad = |m: &dyn Fn(f64, f64) -> P2| -> Vec<Pos2> {
                    [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0), (-1.0, -1.0)].iter().map(|&(x, y)| d.to_screen(m(x, y))).collect()
                };
                let window = if pane == fr.fixed {
                    let h = r * sf;
                    quad(&|x, y| (p.0 + x * h, p.1 + y * h))
                } else {
                    // the same patch as it appears in this photo (zoomed / rotated)
                    let h = r * sf;
                    let j = fr.j;
                    quad(&|x, y| (p.0 + (j[0] * x + j[1] * y) * h, p.1 + (j[2] * x + j[3] * y) * h))
                };
                painter.line(window.clone(), Stroke::new(3.2, Color32::from_black_alpha(150)));
                painter.line(window.clone(), Stroke::new(1.6, yellow));
                let label_at = window.iter().fold(Pos2::new(f32::MAX, f32::MIN), |m, q| Pos2::new(m.x.min(q.x), m.y.max(q.y)));
                if pane != fr.fixed {
                    let sr = if self.pe.dragging.is_some() {
                        self.local_radius(idx) + r * sm
                    } else {
                        // original: half = SearchAreaPercent * image width / 200
                        (self.search_pct * self.photos[idx].w as f64 / 200.0).min(guided::SEARCH_AREA_MAX_PX / 2.0) * self.work_per_full(idx)
                    };
                    let outer = quad(&|x, y| (p.0 + x * sr, p.1 + y * sr));
                    painter.extend(egui::Shape::dashed_line(&outer, Stroke::new(3.0, Color32::from_black_alpha(130)), 7.0, 5.0));
                    painter.extend(egui::Shape::dashed_line(&outer, Stroke::new(1.4, blue), 7.0, 5.0));
                    if (outer[2].x - outer[0].x) > 70.0 {
                        painter.text(outer[0] + Vec2::new(3.0, -2.0), Align2::LEFT_BOTTOM, "search area", FontId::proportional(11.0), blue);
                    }
                }
                if (window[2].x - window[0].x).abs() > 50.0 || (window[2].y - window[0].y).abs() > 50.0 {
                    painter.text(label_at + Vec2::new(0.0, 3.0), Align2::LEFT_TOP, "match window", FontId::proportional(11.0), yellow);
                }
            }
    }

    /// Patch click: rough clicks (orange), refined spot and suggestions.
    fn paint_click_marks(&self, painter: &egui::Painter, d: &PaneDraw, a: usize, b: usize) {
        let pane = d.pane;
        if self.pe.tool == PointTool::PatchClick {
            let orange = MARK_CLICK;
            let click = if pane == Pane::A { self.click.a } else { self.click.b };
            if let Some(c) = click {
                let s = d.to_screen(c);
                painter.circle_stroke(s, 10.0, Stroke::new(2.5, orange));
                painter.line_segment([s - Vec2::new(14.0, 0.0), s + Vec2::new(14.0, 0.0)], Stroke::new(1.0, orange));
                painter.line_segment([s - Vec2::new(0.0, 14.0), s + Vec2::new(0.0, 14.0)], Stroke::new(1.0, orange));
            }
            if let Some(r) = &self.click.result
                && r.a == a && r.b == b {
                    for sg in r.main.iter().chain(r.more.iter()) {
                        let q = if pane == Pane::A { sg.pa } else { sg.pb };
                        let col = if sg.accept { GOOD } else { MUTED };
                        painter.circle_stroke(d.to_screen(q), if sg.main { 7.0 } else { 4.5 }, Stroke::new(if sg.main { 2.5 } else { 1.5 }, col));
                    }
                }
        }
    }

    /// First click of a point being placed.
    fn paint_pending(&self, painter: &egui::Painter, d: &PaneDraw) {
        let PaneDraw { pane, idx, .. } = *d;
        if let Some((pp, pn)) = self.pe.pending
            && pp == pane {
                let s = d.to_screen(self.to_px(idx, pn));
                painter.circle_stroke(s, 9.0, Stroke::new(2.5, ACCENT));
                painter.circle_filled(s, 2.0, ACCENT);
            }
    }

    /// The selection shape being dragged and the brush circle.
    fn paint_select_shape(&self, painter: &egui::Painter, d: &PaneDraw, resp: &egui::Response) {
        let tool = self.pe.tool;
        if !tool.is_select() {
            return;
        }
        let stroke = Stroke::new(select_shapes::SHAPE_LINE_W, ACCENT);
        let halo = Stroke::new(select_shapes::SHAPE_HALO_W, Color32::BLACK);
        if tool == PointTool::SelectBrush
            && let Some(hp) = resp.hover_pos() {
                select_shapes::paint_brush_ring(painter, hp, self.brush_radius(), ACCENT);
            }
        if tool == PointTool::SelectLasso && self.pe.lasso_ctx.is_some_and(|c| c.0 == d.pane) {
            let pts: Vec<Pos2> = self.pe.lasso.corners.iter().map(|p| d.to_screen(*p)).collect();
            select_shapes::paint_lasso(painter, &pts, resp.hover_pos(), self.pe.lasso.dragging(), ACCENT);
        }
        let Some(sd) = self.pe.sel_drag.as_ref().filter(|s| s.pane == d.pane) else { return };
        if tool == PointTool::SelectRect
            && let (Some(a), Some(b)) = (sd.path.first(), sd.path.last()) {
                let r = Rect::from_two_pos(d.to_screen(*a), d.to_screen(*b));
                painter.rect_filled(r, 0.0, ACCENT.gamma_multiply(0.15));
                painter.rect_stroke(r, 0.0, halo, StrokeKind::Middle);
                painter.rect_stroke(r, 0.0, stroke, StrokeKind::Middle);
            }
    }

    /// Loupe while placing points.
    fn paint_loupe(&self, painter: &egui::Painter, d: &PaneDraw, resp: &egui::Response) {
        let PaneDraw { rect, idx, v, tex_id, img_rect, .. } = *d;
        if self.pe.tool == PointTool::Add && v.zoom < 3.0
            && let (Some(hp), Some(t)) = (resp.hover_pos(), tex_id)
                && img_rect.contains(hp) {
                    let lr = Rect::from_min_size(rect.right_bottom() - Vec2::new(170.0, 170.0), Vec2::splat(160.0));
                    let u = (hp - img_rect.min) / img_rect.size();
                    // magnifier shows MAGNIFIER_WIDTH pixels of the full-size photo
                    let full_w = self.photos[idx].w as f32;
                    let full_h = self.photos[idx].h as f32;
                    let hx = 0.5 * MAGNIFIER_WIDTH / full_w;
                    let hy = 0.5 * MAGNIFIER_WIDTH / full_h;
                    painter.rect_filled(lr.expand(3.0), f32::from(RADIUS_CARD), Color32::BLACK);
                    painter.image(t, lr, Rect::from_min_max(Pos2::new(u.x - hx, u.y - hy), Pos2::new(u.x + hx, u.y + hy)), Color32::WHITE);
                    painter.line_segment([lr.center() - Vec2::new(10.0, 0.0), lr.center() + Vec2::new(10.0, 0.0)], Stroke::new(1.0, MARK_RED));
                    painter.line_segment([lr.center() - Vec2::new(0.0, 10.0), lr.center() + Vec2::new(0.0, 10.0)], Stroke::new(1.0, MARK_RED));
                }
    }

    /// Scroll, click and drag input of one half.
    fn pane_input(&self, ui: &egui::Ui, resp: &egui::Response, pane: Pane, events: &mut Vec<PaneEvent>) {
        if resp.hovered() {
            ui.ctx().set_cursor_icon(if self.pe.tool == PointTool::Add || self.pe.tool.is_select() { egui::CursorIcon::Crosshair } else { egui::CursorIcon::Default });
            let sc = ui.input(|i| i.smooth_scroll_delta.y);
            if sc.abs() > 0.1
                && let Some(hp) = resp.hover_pos() {
                    events.push(PaneEvent::Zoom(pane, hp, (sc * 0.0025).exp()));
                }
        }
        if resp.double_clicked() {
            // a double click closes a lasso that is being clicked, otherwise it fits the photo
            let closing = self.pe.tool == PointTool::SelectLasso
                && self.pe.lasso_ctx.is_some_and(|c| c.0 == pane)
                && self.pe.lasso.corners.len() >= select_shapes::LASSO_MIN_CORNERS;
            events.push(if closing { PaneEvent::LassoClose } else { PaneEvent::Fit(pane) });
        } else if resp.clicked()
            && let Some(p) = resp.interact_pointer_pos() {
                let mods = ui.input(|i| i.modifiers);
                events.push(PaneEvent::Click(pane, p, mods));
            }
        if resp.drag_started() {
            let primary = resp.drag_started_by(egui::PointerButton::Primary);
            // egui reports a drag only after the mouse has moved a few pixels; the Area tool wants its
            // corner where the button went down, not where the threshold was passed
            let press = if self.pe.tool == PointTool::Area { ui.input(|i| i.pointer.press_origin()) } else { None };
            if let Some(p) = drag_start_pos(press, resp.interact_pointer_pos()) {
                let mods = ui.input(|i| i.modifiers);
                events.push(PaneEvent::DragStart(pane, p, primary, mods));
            }
        }
        if resp.dragged()
            && let Some(p) = resp.interact_pointer_pos() {
                events.push(PaneEvent::Drag(pane, p, resp.drag_delta()));
            }
        if resp.drag_stopped() {
            events.push(PaneEvent::DragEnd);
        }
    }
}

/// Where a drag starts: the press position when it is wanted and known, else the current position.
fn drag_start_pos(press: Option<Pos2>, now: Option<Pos2>) -> Option<Pos2> {
    press.or(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drag_starts_where_the_button_went_down() {
        let (down, after_threshold) = (Pos2::new(100.0, 100.0), Pos2::new(105.0, 103.0));
        assert_eq!(drag_start_pos(Some(down), Some(after_threshold)), Some(down));
        assert_eq!(drag_start_pos(None, Some(after_threshold)), Some(after_threshold));
        assert_eq!(drag_start_pos(None, None), None);
    }
}
