//! Area tool of the point editor: draw a rough rectangle round the same surface (a sign, a number
//! plate, a wall) in the left and in the right photo, in either order. The areas are compared
//! exactly on a worker thread (`area_match.rs`) and come back as point suggestions that are only
//! added after "Accept" (one undo step); "Discard" drops them.

use super::*;
use crate::area_match::{self, AreaResult, Rect as AreaRect};

/// A rectangle being drawn or moved (photo pixels of its pane).
struct AreaDrag {
    pane: Pane,
    /// where the drag started
    start: P2,
    /// the rectangle before a move (None = a new one is drawn); Esc brings it back
    before: Option<AreaRect>,
    /// moving the existing rectangle (otherwise drawing a new one)
    moving: bool,
}

/// The two rectangles (index = pane), the drag in progress and the last result.
#[derive(Default)]
pub(super) struct AreaState {
    /// the photo pair the rectangles belong to; another pair on screen drops them
    pair: (usize, usize),
    pub(super) rect: [Option<AreaRect>; 2],
    drag: Option<AreaDrag>,
    pub(super) result: Option<AreaResult>,
}

/// Everything the worker thread needs.
pub(super) struct AreaInput {
    a: usize,
    b: usize,
    ga: Arc<Gray>,
    gb: Arc<Gray>,
    rects: (AreaRect, AreaRect),
    priors: crate::click_match::ClickPriors,
    cfg: crate::area_match::AreaConfig,
}

impl AreaInput {
    /// The comparison itself (None = cancelled).
    pub(super) fn compute(&self) -> Option<AreaResult> {
        area_match::run(self.a, self.b, &self.ga, &self.gb, self.rects.0, self.rects.1, &self.priors, &self.cfg)
    }
}

fn contains(r: &AreaRect, p: P2) -> bool {
    p.0 >= r.0.0 && p.0 <= r.1.0 && p.1 >= r.0.1 && p.1 <= r.1.1
}

/// Rectangles smaller than this on both sides (photo pixels) count as a click, not as a drawing.
const MIN_DRAWN_PX: f64 = 4.0;

impl PanoApp {
    /// Other photos on screen (switch, swap sides, photo removed): the rectangles were drawn in the old ones.
    pub(super) fn area_sync_pair(&mut self) {
        if self.area.pair != (self.pe.a, self.pe.b) {
            self.area = AreaState { pair: (self.pe.a, self.pe.b), ..Default::default() };
        }
    }

    /// The "Area" button of the tool row (not there when the tool is switched off in the configuration).
    pub(super) fn area_tool_button(&mut self, ui: &mut egui::Ui) {
        if !self.area_cfg.enabled {
            return;
        }
        if ui
            .add(egui::Button::selectable(self.pe.tool == PointTool::Area, "▭ Area"))
            .on_hover_text("Draw a rough rectangle round the same surface (a sign, a number plate, a wall) in the left and in the right photo. The two areas are then compared exactly and several points with real structure are proposed.")
            .clicked()
        {
            self.pe.tool = PointTool::toggle(self.pe.tool, PointTool::Area);
            self.pe.pending = None;
            self.pe.sel_drag = None;
        }
    }

    /// Esc in the area tool: cancels a drag, else clears the rectangles and suggestions.
    /// Returns false when there was nothing to cancel (Esc then leaves the tool).
    pub(super) fn area_escape(&mut self) -> bool {
        if self.pe.tool != PointTool::Area {
            return false;
        }
        if let Some(d) = self.area.drag.take() {
            self.area.rect[d.pane.i()] = d.before;
            return true;
        }
        if self.area.rect.iter().any(|r| r.is_some()) || self.area.result.is_some() {
            self.area.rect = [None, None];
            self.area.result = None;
            return true;
        }
        false
    }

    /// Mouse events of the two halves while the Area tool is on. Returns true when the event is used up
    /// (the right mouse button and scrolling still move and zoom the photo).
    pub(super) fn area_event(&mut self, ev: &PaneEvent, px_of: &dyn Fn(Pane, Pos2) -> P2) -> bool {
        match ev {
            PaneEvent::Click(..) => true,
            PaneEvent::DragStart(pane, pos, true, _) => {
                let p = px_of(*pane, *pos);
                let before = self.area.rect[pane.i()];
                let moving = before.is_some_and(|r| contains(&r, p));
                if !moving {
                    self.area.rect[pane.i()] = Some((p, p));
                }
                self.area.drag = Some(AreaDrag { pane: *pane, start: p, before, moving });
                // changed rectangles: the old suggestions no longer belong to them
                self.area.result = None;
                true
            }
            PaneEvent::Drag(pane, pos, _) => {
                let Some(d) = self.area.drag.as_ref().filter(|d| d.pane == *pane) else { return false };
                let p = px_of(*pane, *pos);
                self.area.rect[pane.i()] = if d.moving {
                    let (dx, dy) = (p.0 - d.start.0, p.1 - d.start.1);
                    d.before.map(|r| ((r.0.0 + dx, r.0.1 + dy), (r.1.0 + dx, r.1.1 + dy)))
                } else {
                    Some((d.start, p))
                };
                true
            }
            PaneEvent::DragEnd => {
                let Some(d) = self.area.drag.take() else { return false };
                let (w, h) = self.work_dims(if d.pane == Pane::A { self.pe.a } else { self.pe.b });
                if let Some(r) = self.area.rect[d.pane.i()] {
                    let r = area_match::clamp_rect(r, w, h);
                    let (sx, sy) = (r.1.0 - r.0.0, r.1.1 - r.0.1);
                    // a click without a real drag keeps what was there before
                    self.area.rect[d.pane.i()] = if !d.moving && sx < MIN_DRAWN_PX && sy < MIN_DRAWN_PX { d.before } else { Some(r) };
                }
                true
            }
            _ => false,
        }
    }

    /// What the worker needs, or None while a rectangle is missing.
    pub(super) fn area_input(&self) -> Option<AreaInput> {
        let (ra, rb) = (self.area.rect[0]?, self.area.rect[1]?);
        let (a, b) = (self.pe.a, self.pe.b);
        let seeds = self.pair_seeds(a, b);
        let ca = ((ra.0.0 + ra.1.0) * 0.5, (ra.0.1 + ra.1.1) * 0.5);
        let align = if self.aligned && !self.needs_align { self.predict_from_alignment(a, b, ca).map(|(_, j)| j) } else { None };
        Some(AreaInput {
            a,
            b,
            ga: self.photos[a].gray.clone(),
            gb: self.photos[b].gray.clone(),
            rects: (ra, rb),
            priors: crate::click_match::ClickPriors { seeds, align },
            cfg: self.area_cfg.clone(),
        })
    }

    /// Both rectangles are there: compare them on a worker thread.
    pub(super) fn run_area(&mut self) {
        if self.busy() {
            return;
        }
        let Some(input) = self.area_input() else { return };
        self.area.result = None;
        self.start_job("Area: comparing", move |_, cancel| match input.compute() {
            Some(r) if !cancel.load(Ordering::Relaxed) => JobMsg::AreaMatched(Box::new(r)),
            _ => JobMsg::Cancelled,
        });
    }

    pub(super) fn on_area_matched(&mut self, r: AreaResult) {
        for l in &r.log {
            eprintln!("area: {l}");
        }
        if (r.a, r.b) != self.area.pair {
            // the pair on screen changed while the job ran: the result belongs to other photos
            self.set_status("Area: the photo pair changed meanwhile – result dropped, draw the rectangles again.", WARN);
            return;
        }
        match (&r.error, &r.note) {
            (Some(e), _) => self.set_status(e.clone(), BAD),
            (None, note) => self.set_status(
                format!(
                    "Area: {} points proposed (mean score {:.2}, zoom ×{:.2}, rotation {:.1}°) – accept or discard them below.{}",
                    r.points.len(),
                    r.mean_score(),
                    r.zoom(),
                    r.rotation_deg(),
                    note.as_ref().map(|n| format!(" {n}")).unwrap_or_default()
                ),
                if note.is_some() { WARN } else { GOOD },
            ),
        }
        self.area.result = Some(r);
    }

    /// Adds the ticked suggestions (`all`: every one) as hand-placed points: one undo step.
    pub(super) fn accept_area(&mut self, all: bool) {
        let Some(r) = self.area.result.clone() else { return };
        if r.error.is_some() || (r.a, r.b) != (self.pe.a, self.pe.b) {
            return;
        }
        let chosen: Vec<_> = r.points.iter().filter(|s| all || s.accept).collect();
        if chosen.is_empty() {
            self.set_status("Area: no point is ticked – tick the ones you want or use \"Accept all\".", WARN);
            return;
        }
        self.push_undo("adding the area points");
        for s in &chosen {
            let mut c = ControlPoint::new(r.a, r.b, self.to_norm(r.a, s.pa), self.to_norm(r.b, s.pb), true);
            c.conf = s.conf;
            c.score = s.score;
            c.weight = (s.conf as f64).max(0.1) * self.area_cfg.weight;
            self.cps.push(c);
        }
        let n = chosen.len();
        self.area.result = None;
        self.area.rect = [None, None];
        self.needs_align = true;
        self.set_status(format!("{n} points added from the areas. Align again (step 3) to use them. Ctrl+Z undoes it."), GOOD);
    }

    /// Drops the suggestions; the points and the undo list stay as they are.
    pub(super) fn discard_area(&mut self) {
        self.area.result = None;
    }

    /// The rectangles and, when there is a result, its points, in the half `pane` (screen geometry as in `draw_pane`).
    pub(super) fn paint_area(&self, painter: &egui::Painter, pane: Pane, origin: Pos2, scale: f32) {
        if self.pe.tool != PointTool::Area {
            return;
        }
        let to_screen = |p: P2| origin + Vec2::new((p.0 as f32 + 0.5) * scale, (p.1 as f32 + 0.5) * scale);
        if let Some(r) = self.area.rect[pane.i()] {
            let rect = Rect::from_two_pos(to_screen(r.0), to_screen(r.1));
            painter.rect_filled(rect, 0.0, ACCENT.gamma_multiply(0.12));
            painter.rect_stroke(rect, 0.0, Stroke::new(3.2, Color32::BLACK), StrokeKind::Middle);
            painter.rect_stroke(rect, 0.0, Stroke::new(1.6, ACCENT), StrokeKind::Middle);
        }
        let Some(res) = self.area.result.as_ref().filter(|r| (r.a, r.b) == (self.pe.a, self.pe.b)) else { return };
        for (k, s) in res.points.iter().enumerate() {
            let col = if s.accept { GOOD } else if s.weak { WARN } else { MUTED };
            let c = to_screen(if pane == Pane::A { s.pa } else { s.pb });
            painter.circle_stroke(c, 6.5, Stroke::new(3.0, Color32::BLACK));
            painter.circle_stroke(c, 6.5, Stroke::new(1.8, col));
            painter.text(c + Vec2::new(8.0, -8.0), Align2::LEFT_BOTTOM, format!("{}", k + 1), FontId::proportional(11.0), col);
        }
    }

    /// Strip under the toolbar while the Area tool is on.
    pub(super) fn area_panel(&mut self, ui: &mut egui::Ui) {
        let (has_a, has_b) = (self.area.rect[0].is_some(), self.area.rect[1].is_some());
        let state = match (has_a, has_b) {
            (false, false) => "Area: drag a rough rectangle round a surface (sign, number plate, wall) in one photo, then round the same surface in the other. Drag inside a rectangle to move it, drag outside to draw it again. Esc clears.",
            (true, false) => "Now drag a rectangle round the same surface in the RIGHT photo.",
            (false, true) => "Now drag a rectangle round the same surface in the LEFT photo.",
            (true, true) => "Both rectangles are there – move or redraw them if needed, then press \"Match the areas\".",
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(state).small().color(ACCENT));
            let ready = has_a && has_b && self.area.drag.is_none() && !self.busy();
            if ui.add_enabled(ready, egui::Button::new(RichText::new("▶ Match the areas").strong())).on_hover_text("Compares the two areas exactly (patch matching, sub-pixel) and proposes points where there is real structure.").clicked() {
                self.run_area();
            }
            if ui.add_enabled(has_a || has_b || self.area.result.is_some(), egui::Button::new("✖ Clear")).on_hover_text("Remove the rectangles and the suggestions (Esc)").clicked() {
                self.area.rect = [None, None];
                self.area.result = None;
            }
        });
        let Some(r) = self.area.result.clone() else { return };
        if (r.a, r.b) != (self.pe.a, self.pe.b) {
            return;
        }
        egui::Frame::new().fill(CARD).corner_radius(CornerRadius::same(RADIUS_CARD)).inner_margin(egui::Margin::same(TILE_MARGIN)).show(ui, |ui| {
            if let Some(e) = &r.error {
                ui.label(RichText::new(e).color(BAD));
                return;
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{} points, mean score {:.3}", r.points.len(), r.mean_score())).strong());
                ui.label(RichText::new(format!("zoom ×{:.3}, rotation {:.1}° – start {}", r.zoom(), r.rotation_deg(), r.map_source)).small().color(MUTED));
            });
            if let Some(n) = &r.note {
                ui.label(RichText::new(n).small().color(WARN));
            }
            let n_on = r.points.iter().filter(|s| s.accept).count();
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new(format!("✔ Accept {n_on} checked")).strong())).clicked() {
                    self.accept_area(false);
                }
                if ui.button("Accept all").clicked() {
                    self.accept_area(true);
                }
                if ui.button("✖ Discard").clicked() {
                    self.discard_area();
                }
            });
            let mut toggles: Vec<(usize, bool)> = Vec::new();
            egui::ScrollArea::vertical().id_salt("area_sugg").max_height(96.0).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (k, sg) in r.points.iter().enumerate() {
                        let mut on = sg.accept;
                        let text = RichText::new(format!("#{} · {:.2}{}", k + 1, sg.score, if sg.weak { " weak" } else { "" }));
                        let text = if sg.weak { text.color(WARN) } else { text };
                        if ui.checkbox(&mut on, text).on_hover_text(format!("score {:.3}, confidence {:.3}, structure {:.1e}", sg.score, sg.conf, sg.texture)).changed() {
                            toggles.push((k, on));
                        }
                    }
                });
            });
            if let Some(res) = &mut self.area.result {
                for (k, on) in toggles {
                    if let Some(s) = res.points.get_mut(k) {
                        s.accept = on;
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::masks_tests::app_with_photos;
    use super::*;
    use crate::area_match::AreaSuggestion;
    use crate::testutil;

    fn suggestion(k: usize, weak: bool) -> AreaSuggestion {
        AreaSuggestion { pa: (20.0 + 5.0 * k as f64, 30.0), pb: (22.0 + 5.0 * k as f64, 31.0), score: if weak { 0.82 } else { 0.95 }, conf: 0.6, texture: 1e-3, weak, accept: !weak }
    }

    fn app_with_result() -> PanoApp {
        let mut app = app_with_photos(2);
        app.pe.a = 0;
        app.pe.b = 1;
        app.area_sync_pair();
        app.area.rect = [Some(((10.0, 10.0), (60.0, 50.0))), Some(((12.0, 11.0), (62.0, 51.0)))];
        let mut r = AreaResult { a: 0, b: 1, map: [1.0, 0.0, 0.0, 1.0, 2.0, 1.0], map_source: "test".into(), points: Vec::new(), error: None, note: None, log: Vec::new() };
        r.points = (0..5).map(|k| suggestion(k, k == 4)).collect();
        app.area.result = Some(r);
        app
    }

    #[test]
    fn accepting_adds_the_ticked_points_by_hand_as_one_undo_step() {
        let mut app = app_with_result();
        assert!(!app.undo.can_undo() && app.cps.is_empty());
        app.accept_area(false);
        // four ticked, the weak one is not
        assert_eq!(app.cps.len(), 4);
        assert!(app.cps.iter().all(|c| c.manual && c.kind == CpKind::Point && (c.a, c.b) == (0, 1)));
        assert!(app.needs_align);
        assert!(app.area.result.is_none());
        assert!(app.status.contains("4 points added"), "{}", app.status);
        app.undo_last();
        assert!(app.cps.is_empty(), "one undo step takes all of them back");
        assert!(!app.undo.can_undo());
    }

    #[test]
    fn accept_all_takes_the_weak_ones_too() {
        let mut app = app_with_result();
        app.accept_area(true);
        assert_eq!(app.cps.len(), 5);
    }

    #[test]
    fn discarding_changes_nothing() {
        let mut app = app_with_result();
        let before = app.cps.len();
        app.needs_align = false;
        app.discard_area();
        assert!(app.area.result.is_none());
        assert_eq!(app.cps.len(), before);
        assert!(!app.undo.can_undo(), "discarding makes no undo step");
        assert!(!app.needs_align);
        // nothing ticked: nothing is added either
        let mut app = app_with_result();
        app.area.result.as_mut().unwrap().points.iter_mut().for_each(|p| p.accept = false);
        app.accept_area(false);
        assert!(app.cps.is_empty() && !app.undo.can_undo());
    }

    #[test]
    fn rectangles_belong_to_the_pair_on_screen() {
        let mut app = app_with_result();
        app.pe.b = 0;
        app.pe.a = 1;
        app.area_sync_pair();
        assert!(app.area.rect.iter().all(|r| r.is_none()) && app.area.result.is_none());
    }

    #[test]
    fn drawing_moving_and_escape() {
        let mut app = app_with_photos(2);
        app.pe.a = 0;
        app.pe.b = 1;
        app.area_sync_pair();
        app.pe.tool = PointTool::Area;
        let px_of = |_: Pane, p: Pos2| -> P2 { (p.x as f64, p.y as f64) };
        let m = egui::Modifiers::NONE;
        // draw in the left photo: the right mouse button is left to panning
        assert!(!app.area_event(&PaneEvent::DragStart(Pane::A, Pos2::new(10.0, 10.0), false, m), &px_of));
        assert!(app.area_event(&PaneEvent::DragStart(Pane::A, Pos2::new(10.0, 10.0), true, m), &px_of));
        assert!(app.area_event(&PaneEvent::Drag(Pane::A, Pos2::new(60.0, 40.0), Vec2::ZERO), &px_of));
        assert!(app.area_event(&PaneEvent::DragEnd, &px_of));
        assert_eq!(app.area.rect[0], Some(((10.0, 10.0), (60.0, 40.0))));
        // dragging inside moves it
        app.area_event(&PaneEvent::DragStart(Pane::A, Pos2::new(30.0, 20.0), true, m), &px_of);
        app.area_event(&PaneEvent::Drag(Pane::A, Pos2::new(35.0, 22.0), Vec2::ZERO), &px_of);
        app.area_event(&PaneEvent::DragEnd, &px_of);
        assert_eq!(app.area.rect[0], Some(((15.0, 12.0), (65.0, 42.0))));
        // Esc during a drag brings the rectangle back, then clears, then leaves the tool
        app.area_event(&PaneEvent::DragStart(Pane::A, Pos2::new(30.0, 20.0), true, m), &px_of);
        app.area_event(&PaneEvent::Drag(Pane::A, Pos2::new(50.0, 30.0), Vec2::ZERO), &px_of);
        assert!(app.area_escape());
        assert_eq!(app.area.rect[0], Some(((15.0, 12.0), (65.0, 42.0))));
        assert!(app.area_escape());
        assert!(app.area.rect.iter().all(|r| r.is_none()));
        assert!(!app.area_escape(), "nothing left: Esc leaves the tool");
    }

    /// The whole path on two synthetic photos: the right one is the left one shifted by a few pixels.
    #[test]
    fn the_areas_of_two_photos_give_points_that_are_added_exactly() {
        let mut app = app_with_photos(2);
        let tex = testutil::texture(400, 300, 5);
        // photo 1 = photo 0 moved by (9, -6) pixels (copy with an offset)
        let mut moved = image::RgbImage::new(400, 300);
        for y in 0..300u32 {
            for x in 0..400u32 {
                let (sx, sy) = (x as i32 - 9, y as i32 + 6);
                let p = if sx >= 0 && sy >= 0 && sx < 400 && sy < 300 { *tex.get_pixel(sx as u32, sy as u32) } else { image::Rgb([0, 0, 0]) };
                moved.put_pixel(x, y, p);
            }
        }
        for (i, img) in [tex, moved].into_iter().enumerate() {
            app.photos[i].gray = Arc::new(imgutil::to_gray(&img));
            app.photos[i].work = Arc::new(img);
        }
        app.pe.a = 0;
        app.pe.b = 1;
        app.area_sync_pair();
        app.area.rect = [Some(((100.0, 80.0), (260.0, 200.0))), Some(((104.0, 71.0), (272.0, 197.0)))];
        let input = app.area_input().expect("both rectangles");
        let r = input.compute().expect("not cancelled");
        assert!(r.error.is_none(), "{:?} {:?}", r.error, r.log);
        for p in &r.points {
            assert!((p.pb.0 - (p.pa.0 + 9.0)).abs() < 0.2 && (p.pb.1 - (p.pa.1 - 6.0)).abs() < 0.2, "{:?} -> {:?}", p.pa, p.pb);
        }
        app.area.pair = (0, 1);
        app.on_area_matched(r);
        let n = app.area.result.as_ref().unwrap().points.iter().filter(|p| p.accept).count();
        app.accept_area(false);
        assert_eq!(app.cps.len(), n);
        assert!(n >= 5, "{n} points");
    }

    #[test]
    fn switched_off_the_tool_leaves_the_editor_as_before() {
        // with the tool off there is no button (see `area_tool_button`); the editor's tool stays what it was
        let mut app = app_with_photos(2);
        app.area_cfg.enabled = false;
        assert_eq!(app.pe.tool, PointTool::Edit);
        assert!(!app.area_escape(), "Esc does nothing in the area tool when another tool is on");
        assert!(app.area.rect.iter().all(|r| r.is_none()));
    }
}
