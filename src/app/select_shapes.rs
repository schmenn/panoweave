//! The selection shapes shared by the point editor and the mask editor (rectangle, lasso, brush):
//! pure geometry that says which points lie in a shape, the lasso builder (one behaviour for both
//! editors) and the one way these shapes are drawn. All coordinates are pixels of one photo (the
//! pane that was dragged in).

use super::*;
use std::collections::HashSet;

/// Width of the outline of a shape being drawn, in screen pixels.
pub(super) const SHAPE_LINE_W: f32 = 1.6;
/// Width of the dark halo under it (keeps it visible on light and dark photos).
pub(super) const SHAPE_HALO_W: f32 = 3.2;
/// Radius of the dots on the corners of a lasso that is being clicked.
const CORNER_DOT_R: f32 = 3.5;
/// A lasso needs at least this many corners.
pub(super) const LASSO_MIN_CORNERS: usize = 3;
/// A lasso drag whose outline is smaller than this (screen pixels across) is a shaky click: it sets one corner.
const LASSO_DRAG_MIN_PX: f64 = 12.0;
/// Corners of a dragged lasso are taken this far apart (screen pixels).
const LASSO_STEP_PX: f64 = 4.0;

/// The one help text of the lasso; the editors add what is special to them.
macro_rules! lasso_hint {
    () => {
        "Lasso: drag around an area (closes when you let go), or click corner by corner and close with a double click or Enter. Esc cancels, Ctrl+Z takes the last corner back."
    };
}
/// Status line of the point editor while the lasso is chosen.
pub(super) const LASSO_POINTS_HINT: &str =
    concat!(lasso_hint!(), " Shift adds to the selection, Ctrl takes points out. Delete removes the selection; protected points stay. The right mouse button moves the photo.");
/// Tooltip of the lasso button of the point editor.
pub(super) const LASSO_TIP: &str = concat!(lasso_hint!(), " Shift adds to the selection, Ctrl takes points out. Clicking or dragging a point still selects or moves it.");
/// Status line of the mask editor while the lasso is chosen.
pub(super) const LASSO_MASK_HINT: &str = concat!(lasso_hint!(), " Scroll to zoom, drag with the right mouse button to move.");

/// The lasso of both editors. Drag around an area from scratch: it closes when the button is
/// released. Click corner by corner: close with a double click or Enter. A drag that is no bigger
/// than a shaky click sets one corner; a drag while corners are being clicked joins them (still open).
/// Esc throws it away. Pure logic: positions are photo pixels, `scale` is screen pixels per photo pixel.
#[derive(Default, Debug)]
pub(super) struct Lasso {
    pub(super) corners: Vec<P2>,
    /// a drag is going on; the index of the corner it started with
    drag_from: Option<usize>,
}

impl Lasso {
    /// Something is being drawn.
    pub(super) fn busy(&self) -> bool {
        !self.corners.is_empty()
    }

    /// An outline is being dragged (its many corners are not drawn as dots).
    pub(super) fn dragging(&self) -> bool {
        self.drag_from.is_some()
    }

    /// Throws away what is being drawn.
    pub(super) fn cancel(&mut self) {
        self.corners.clear();
        self.drag_from = None;
    }

    /// Takes the last corner back; false if there is none.
    pub(super) fn pop_corner(&mut self) -> bool {
        self.drag_from = None;
        self.corners.pop().is_some()
    }

    /// Button pressed: a drag starts (it sets one corner if it stays a shaky click).
    pub(super) fn down(&mut self, p: P2) {
        self.drag_from = Some(self.corners.len());
        self.corners.push(p);
    }

    /// Pointer moved with the button held. `bounds` (photo size) keeps the outline inside the photo.
    pub(super) fn drag(&mut self, p: P2, scale: f64, bounds: Option<P2>) {
        if self.drag_from.is_none() {
            return;
        }
        let p = bounds.map_or(p, |b| (p.0.clamp(0.0, b.0), p.1.clamp(0.0, b.1)));
        let far = self.corners.last().is_none_or(|l| (l.0 - p.0).hypot(l.1 - p.1) * scale >= LASSO_STEP_PX);
        if far {
            self.corners.push(p);
        }
    }

    /// Button released. Returns the finished polygon when an outline was dragged around an area
    /// from scratch.
    pub(super) fn up(&mut self, scale: f64) -> Option<Vec<P2>> {
        let start = self.drag_from.take()?;
        let path = &self.corners[start.min(self.corners.len())..];
        let init = ((f64::MAX, f64::MAX), (f64::MIN, f64::MIN));
        let (lo, hi) = path.iter().fold(init, |(lo, hi), q| ((lo.0.min(q.0), lo.1.min(q.1)), (hi.0.max(q.0), hi.1.max(q.1))));
        let drawn = path.len() >= LASSO_MIN_CORNERS && (hi.0 - lo.0).hypot(hi.1 - lo.1) * scale >= LASSO_DRAG_MIN_PX;
        if !drawn {
            // a click that moved a little: one corner, as a click would have set
            self.corners.truncate(start + 1);
            None
        } else if start == 0 {
            self.close()
        } else {
            // dragged while clicking corners: the outline joins the corners, still open
            None
        }
    }

    /// A click sets a corner.
    pub(super) fn click(&mut self, p: P2) {
        self.corners.push(p);
    }

    /// Double click or Enter: the finished polygon, or None (and nothing left over) with fewer than
    /// three corners.
    pub(super) fn close(&mut self) -> Option<Vec<P2>> {
        self.drag_from = None;
        let corners = std::mem::take(&mut self.corners);
        (corners.len() >= LASSO_MIN_CORNERS).then_some(corners)
    }
}

/// Draws a lasso: halo and outline through the corners (to the pointer while corners are being
/// clicked), a faint line back to the start, dots on the corners.
pub(super) fn paint_lasso(painter: &egui::Painter, pts: &[Pos2], hover: Option<Pos2>, dragging: bool, color: Color32) {
    if pts.is_empty() {
        return;
    }
    let mut line = pts.to_vec();
    if let (false, Some(h)) = (dragging, hover) {
        line.push(h);
    }
    if line.len() >= 2 {
        if line.len() >= 3 {
            let back = [line[line.len() - 1], line[0]];
            painter.add(egui::Shape::line_segment(back, Stroke::new(1.0, color.gamma_multiply(0.5))));
        }
        painter.add(egui::Shape::line(line.clone(), Stroke::new(SHAPE_HALO_W, Color32::BLACK)));
        painter.add(egui::Shape::line(line, Stroke::new(SHAPE_LINE_W, color)));
    }
    if !dragging {
        for p in pts {
            painter.circle_filled(*p, CORNER_DOT_R, color);
            painter.circle_stroke(*p, CORNER_DOT_R, Stroke::new(1.0, Color32::BLACK));
        }
    }
}

/// Draws the brush circle under the pointer.
pub(super) fn paint_brush_ring(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32) {
    painter.circle_stroke(center, radius, Stroke::new(SHAPE_HALO_W, Color32::BLACK));
    painter.circle_stroke(center, radius, Stroke::new(SHAPE_LINE_W, color));
}

impl PanoApp {
    /// A lasso of the point editor is finished: selects the points inside (Shift adds, Ctrl takes out).
    pub(super) fn finish_lasso(&mut self, poly: &[P2], pts: &[(usize, P2)], mods: egui::Modifiers) {
        let hits = points_in(&Shape::Lasso(poly), pts);
        self.apply_hits(&hits, SelOp::for_tool(PointTool::SelectLasso, mods.shift, mods.command));
        let n = self.pe.sel.len();
        self.set_status(format!("{n} point{} selected.", if n == 1 { "" } else { "s" }), MUTED);
    }

    /// Drops a lasso that no longer fits: other tool, other pair on screen.
    pub(super) fn sync_lasso(&mut self) {
        let pair = (self.pe.a, self.pe.b);
        if self.pe.tool != PointTool::SelectLasso || self.pe.lasso_ctx.is_some_and(|c| c.1 != pair) {
            self.pe.lasso.cancel();
            self.pe.lasso_ctx = None;
        }
    }

    /// The lasso of the point editor ends (double click, Enter or released drag): selects the points inside.
    pub(super) fn lasso_finish(&mut self, poly: Option<Vec<P2>>, pts: &[(usize, P2, P2)]) {
        let Some((pane, _, mods)) = self.pe.lasso_ctx else { return };
        let Some(poly) = poly else { return };
        let pane_pts: Vec<(usize, P2)> = pts.iter().map(|(k, pa, pb)| (*k, if pane == Pane::A { *pa } else { *pb })).collect();
        self.finish_lasso(&poly, &pane_pts, mods);
    }

    /// Enter: closes the lasso being clicked.
    pub(super) fn lasso_enter(&mut self, pts: &[(usize, P2, P2)]) {
        if self.pe.tool == PointTool::SelectLasso && self.pe.lasso.busy() {
            let poly = self.pe.lasso.close();
            self.lasso_finish(poly, pts);
        }
    }

    /// Pointer events of the lasso tool (same behaviour as in the mask editor, see `Lasso`).
    /// Returns true when the event was used. A click or drag that starts on a point still selects
    /// or moves that point, unless a lasso is already being drawn.
    pub(super) fn lasso_event(
        &mut self,
        ev: &PaneEvent,
        geo: &[PaneGeo; 2],
        px_of: &dyn Fn(Pane, Pos2) -> P2,
        on_point: &dyn Fn(Pane, Pos2) -> bool,
        pts: &[(usize, P2, P2)],
    ) -> bool {
        if self.pe.tool != PointTool::SelectLasso {
            return false;
        }
        let pair = (self.pe.a, self.pe.b);
        let in_pane = |app: &PanoApp, pane: Pane| app.pe.lasso.busy() && app.pe.lasso_ctx.is_some_and(|c| c.0 == pane);
        match *ev {
            PaneEvent::Click(pane, pos, mods) if self.pe.lasso.busy() || !on_point(pane, pos) => {
                self.lasso_begin(pane, pair, mods);
                self.pe.lasso.click(px_of(pane, pos));
                true
            }
            PaneEvent::DragStart(pane, pos, true, mods) if in_pane(self, pane) || !on_point(pane, pos) || mods.shift || mods.command => {
                self.pe.dragging = None;
                self.pe.drag_before = None;
                self.pe.panning = None;
                self.lasso_begin(pane, pair, mods);
                self.pe.lasso.down(px_of(pane, pos));
                true
            }
            PaneEvent::Drag(pane, pos, _) if self.pe.lasso.dragging() => {
                if self.pe.lasso_ctx.is_some_and(|c| c.0 == pane) {
                    let g = &geo[pane.i()];
                    self.pe.lasso.drag(px_of(pane, pos), g.scale as f64, None);
                }
                true
            }
            PaneEvent::DragEnd if self.pe.lasso.dragging() => {
                let scale = self.pe.lasso_ctx.map_or(1.0, |c| geo[c.0.i()].scale as f64);
                let poly = self.pe.lasso.up(scale);
                self.pe.panning = None;
                self.lasso_finish(poly, pts);
                true
            }
            PaneEvent::LassoClose => {
                let poly = self.pe.lasso.close();
                self.lasso_finish(poly, pts);
                true
            }
            _ => false,
        }
    }

    /// A lasso starts or continues in `pane` (one in the other pane is dropped).
    fn lasso_begin(&mut self, pane: Pane, pair: (usize, usize), mods: egui::Modifiers) {
        if self.pe.lasso_ctx.is_some_and(|c| c.0 != pane || c.1 != pair) {
            self.pe.lasso.cancel();
        }
        if !self.pe.lasso.busy() {
            self.pe.lasso_ctx = Some((pane, pair, mods));
        }
    }

    /// Applies `hits` to the multiple selection.
    pub(super) fn apply_hits(&mut self, hits: &[usize], op: SelOp) {
        if op != SelOp::Replace && self.pe.sel.is_empty() {
            self.pe.sel.extend(self.pe.selected.filter(|&s| s < self.cps.len()));
        }
        op.apply(&mut self.pe.sel, hits);
        // the multiple selection is the truth while a shape tool is used
        self.pe.selected = None;
    }
}

/// Default brush radius in screen pixels (`PointEditor::brush_px` starts at 0 = this).
pub(super) const BRUSH_PX_DEFAULT: f32 = 28.0;
/// Range of the brush radius in the toolbar, in screen pixels.
pub(super) const BRUSH_PX_RANGE: std::ops::RangeInclusive<f32> = 6.0..=200.0;

/// A selection shape in photo pixels.
pub(super) enum Shape<'a> {
    /// two opposite corners
    Rect(P2, P2),
    /// closed polygon (the last point joins the first)
    Lasso(&'a [P2]),
    /// centre line (a stroke) and radius
    Brush(&'a [P2], f64),
}

/// What a finished shape does to the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SelOp {
    Replace,
    Add,
    Subtract,
}

impl SelOp {
    /// Rectangle / lasso: plain = replace, Shift = add, Ctrl = subtract. Brush: dragging collects
    /// (also with Shift), Ctrl subtracts.
    pub(super) fn for_tool(tool: PointTool, shift: bool, ctrl: bool) -> SelOp {
        match (tool, ctrl, shift) {
            (_, true, _) => SelOp::Subtract,
            (PointTool::SelectBrush, false, _) => SelOp::Add,
            (_, false, true) => SelOp::Add,
            _ => SelOp::Replace,
        }
    }

    pub(super) fn apply(self, sel: &mut HashSet<usize>, hits: &[usize]) {
        match self {
            SelOp::Replace => {
                sel.clear();
                sel.extend(hits.iter().copied());
            }
            SelOp::Add => sel.extend(hits.iter().copied()),
            SelOp::Subtract => {
                for k in hits {
                    sel.remove(k);
                }
            }
        }
    }
}

/// Even-odd point-in-polygon test (polygons with fewer than 3 corners contain nothing).
pub(super) fn in_polygon(p: P2, poly: &[P2]) -> bool {
    if poly.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[j]);
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0 {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Distance from `p` to the segment `a`-`b`.
pub(super) fn segment_distance(p: P2, a: P2, b: P2) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= 0.0 { 0.0 } else { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0) };
    (p.0 - (a.0 + t * dx)).hypot(p.1 - (a.1 + t * dy))
}

/// The keys of the points (key, position) that lie in the shape, in input order.
pub(super) fn points_in(shape: &Shape, pts: &[(usize, P2)]) -> Vec<usize> {
    pts.iter()
        .filter(|(_, p)| match shape {
            Shape::Rect(a, b) => p.0 >= a.0.min(b.0) && p.0 <= a.0.max(b.0) && p.1 >= a.1.min(b.1) && p.1 <= a.1.max(b.1),
            Shape::Lasso(poly) => in_polygon(*p, poly),
            Shape::Brush(path, r) => match path {
                [] => false,
                [one] => (p.0 - one.0).hypot(p.1 - one.1) <= *r,
                _ => path.windows(2).any(|w| segment_distance(*p, w[0], w[1]) <= *r),
            },
        })
        .map(|(k, _)| *k)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> Vec<(usize, P2)> {
        // 5 x 5 points at 0, 10, ..., 40; key = row * 5 + col
        (0..25).map(|i| (i, ((i % 5) as f64 * 10.0, (i / 5) as f64 * 10.0))).collect()
    }

    #[test]
    fn rectangle_takes_inside_and_edges_in_any_corner_order() {
        let g = grid();
        let mut a = points_in(&Shape::Rect((10.0, 10.0), (20.0, 20.0)), &g);
        a.sort_unstable();
        assert_eq!(a, vec![6, 7, 11, 12]);
        let mut b = points_in(&Shape::Rect((20.0, 20.0), (10.0, 10.0)), &g);
        b.sort_unstable();
        assert_eq!(a, b);
        assert!(points_in(&Shape::Rect((11.0, 11.0), (19.0, 19.0)), &g).is_empty());
    }

    #[test]
    fn lasso_triangle_and_concave_shape() {
        let g = grid();
        let tri = [(0.0, 0.0), (40.0, 0.0), (0.0, 40.0)];
        let mut h = points_in(&Shape::Lasso(&tri), &g);
        h.sort_unstable();
        // (0,0) (10,0) (20,0) (0,10) (10,10) (0,20) are inside, the points on the hypotenuse are decided by the half-open rule
        assert!(h.contains(&0) && h.contains(&1) && h.contains(&5) && h.contains(&6));
        assert!(!h.contains(&24) && !h.contains(&18));
        // a "U": the notch in the middle is outside
        let u = [(0.0, 0.0), (40.0, 0.0), (40.0, 40.0), (30.0, 40.0), (30.0, 5.0), (10.0, 5.0), (10.0, 40.0), (0.0, 40.0)];
        assert!(in_polygon((5.0, 30.0), &u));
        assert!(!in_polygon((20.0, 30.0), &u));
        assert!(in_polygon((20.0, 2.0), &u));
        // fewer than 3 corners select nothing
        assert!(points_in(&Shape::Lasso(&[(0.0, 0.0), (40.0, 40.0)]), &g).is_empty());
    }

    #[test]
    fn brush_follows_the_stroke() {
        let g = grid();
        // a horizontal stroke along y = 20 with radius 4 takes the middle row only
        let mut r = points_in(&Shape::Brush(&[(-5.0, 20.0), (45.0, 20.0)], 4.0), &g);
        r.sort_unstable();
        assert_eq!(r, vec![10, 11, 12, 13, 14]);
        // a single dab
        assert_eq!(points_in(&Shape::Brush(&[(10.0, 10.0)], 3.0), &g), vec![6]);
        // a dab between points takes nothing
        assert!(points_in(&Shape::Brush(&[(15.0, 15.0)], 3.0), &g).is_empty());
        assert!(points_in(&Shape::Brush(&[], 3.0), &g).is_empty());
    }

    #[test]
    fn operations_replace_add_subtract() {
        let mut sel: HashSet<usize> = [1, 2].into_iter().collect();
        SelOp::Add.apply(&mut sel, &[2, 3]);
        assert_eq!(sel.len(), 3);
        SelOp::Subtract.apply(&mut sel, &[1, 9]);
        assert!(!sel.contains(&1) && sel.len() == 2);
        SelOp::Replace.apply(&mut sel, &[7]);
        assert_eq!(sel.len(), 1);
        assert!(sel.contains(&7));
    }

    #[test]
    fn dragged_shapes_change_the_app_selection() {
        let mut app = super::super::masks_tests::app_with_photos(2);
        let g = grid();
        app.cps = g.iter().map(|(_, p)| ControlPoint::new(0, 1, [p.0, p.1], [0.0; 2], false)).collect();
        // rectangle replaces, then a brush stroke with Ctrl takes one point out again
        app.pe.tool = PointTool::SelectRect;
        app.pe.selected = Some(24);
        app.pe.sel_drag = Some(SelDrag { pane: Pane::A, path: vec![(9.0, 9.0), (21.0, 21.0)], mods: Default::default() });
        app.apply_select(&g, 3.0, true);
        let mut got: Vec<usize> = app.pe.sel.iter().copied().collect();
        got.sort_unstable();
        assert_eq!(got, vec![6, 7, 11, 12]);
        assert_eq!(app.pe.selected, None);
        app.pe.tool = PointTool::SelectBrush;
        let ctrl = egui::Modifiers { ctrl: true, command: true, ..Default::default() };
        app.pe.sel_drag = Some(SelDrag { pane: Pane::A, path: vec![(10.0, 10.0)], mods: ctrl });
        app.apply_select(&g, 3.0, false);
        assert_eq!(app.pe.sel.len(), 3);
        assert!(!app.pe.sel.contains(&6));
        // a rectangle drag does nothing before the drag ends
        app.pe.tool = PointTool::SelectRect;
        app.pe.sel_drag = Some(SelDrag { pane: Pane::A, path: vec![(0.0, 0.0), (40.0, 40.0)], mods: Default::default() });
        app.apply_select(&g, 3.0, false);
        assert_eq!(app.pe.sel.len(), 3);
    }

    fn pane_setup() -> (PanoApp, [PaneGeo; 2], Vec<(usize, P2, P2)>) {
        let mut app = super::super::masks_tests::app_with_photos(2);
        app.pe.a = 0;
        app.pe.b = 1;
        let px = [(10.0, 10.0), (30.0, 10.0), (90.0, 90.0)];
        app.cps = px.iter().map(|_| ControlPoint::new(0, 1, [0.0; 2], [0.0; 2], false)).collect();
        let pts: Vec<(usize, P2, P2)> = px.iter().enumerate().map(|(k, p)| (k, *p, *p)).collect();
        let g = PaneGeo { rect: Rect::from_min_size(Pos2::ZERO, Vec2::splat(200.0)), origin: Pos2::ZERO, scale: 1.0, fit: 1.0, w: 200.0, h: 200.0 };
        (app, [g, g], pts)
    }

    #[test]
    fn a_single_brush_click_selects_the_points_under_it() {
        let (mut app, geo, pts) = pane_setup();
        app.pe.tool = PointTool::SelectBrush;
        app.pe.brush_px = 25.0;
        app.handle_pane_events(vec![PaneEvent::Click(Pane::A, Pos2::new(10.5, 10.5), Default::default())], &geo, &pts);
        let mut got: Vec<usize> = app.pe.sel.iter().copied().collect();
        got.sort_unstable();
        assert_eq!(got, vec![0, 1]);
        assert!(app.pe.sel_drag.is_none());
    }

    #[test]
    fn rectangle_drag_on_a_point_moves_the_point_and_on_empty_space_draws() {
        let (mut app, geo, pts) = pane_setup();
        app.pe.tool = PointTool::SelectRect;
        app.handle_pane_events(vec![PaneEvent::DragStart(Pane::A, Pos2::new(90.5, 90.5), true, Default::default())], &geo, &pts);
        assert_eq!(app.pe.dragging, Some((2, Pane::A)));
        assert!(app.pe.sel_drag.is_none());
        app.handle_pane_events(vec![PaneEvent::DragEnd], &geo, &pts);
        app.handle_pane_events(vec![PaneEvent::DragStart(Pane::A, Pos2::new(150.0, 150.0), true, Default::default())], &geo, &pts);
        assert!(app.pe.sel_drag.is_some() && app.pe.dragging.is_none());
    }

    fn drag_lasso(l: &mut Lasso, path: &[P2], scale: f64) -> Option<Vec<P2>> {
        l.down(path[0]);
        for p in &path[1..] {
            l.drag(*p, scale, None);
        }
        l.up(scale)
    }

    #[test]
    fn lasso_drag_closes_on_release_and_clicks_close_with_close() {
        let mut l = Lasso::default();
        let poly = drag_lasso(&mut l, &[(0.0, 0.0), (50.0, 0.0), (50.0, 40.0), (0.0, 40.0)], 1.0).expect("a dragged outline closes on release");
        assert_eq!(poly.len(), 4);
        assert!(!l.busy() && !l.dragging());
        // clicked corners: two are no area, three are
        l.click((0.0, 0.0));
        l.click((50.0, 0.0));
        assert!(l.close().is_none() && !l.busy());
        for c in [(0.0, 0.0), (50.0, 0.0), (25.0, 40.0)] {
            l.click(c);
        }
        assert_eq!(l.close().map(|p| p.len()), Some(3));
    }

    #[test]
    fn a_shaky_click_keeps_its_corner_and_a_drag_between_clicks_stays_open() {
        let mut l = Lasso::default();
        assert!(drag_lasso(&mut l, &[(10.0, 10.0), (14.0, 12.0), (15.0, 13.0)], 1.0).is_none());
        assert_eq!(l.corners, vec![(10.0, 10.0)], "the shaky click set exactly one corner");
        l.click((60.0, 10.0));
        // a real drag while corners are being clicked joins them and stays open
        assert!(drag_lasso(&mut l, &[(60.0, 50.0), (40.0, 60.0), (20.0, 60.0)], 1.0).is_none());
        assert!(l.corners.len() >= 4 && l.busy());
        assert_eq!(l.close().map(|p| p.len() >= 4), Some(true));
        // the size limit counts screen pixels: the same outline at 0.1 screen px per photo px is a click
        assert!(drag_lasso(&mut l, &[(0.0, 0.0), (50.0, 0.0), (50.0, 40.0)], 0.1).is_none());
        assert_eq!(l.corners.len(), 1);
    }

    #[test]
    fn cancel_and_pop_corner_take_back_what_was_drawn() {
        let mut l = Lasso::default();
        l.click((0.0, 0.0));
        l.click((9.0, 0.0));
        assert!(l.pop_corner() && l.corners.len() == 1);
        l.down((20.0, 20.0));
        l.cancel();
        assert!(!l.busy() && !l.dragging() && !l.pop_corner());
        // clamped to the photo
        l.down((10.0, 10.0));
        l.drag((-30.0, 500.0), 1.0, Some((100.0, 80.0)));
        assert_eq!(l.corners.last(), Some(&(0.0, 80.0)));
    }

    fn lasso_events(app: &mut PanoApp, geo: &[PaneGeo; 2], pts: &[(usize, P2, P2)], evs: Vec<PaneEvent>) {
        app.handle_pane_events(evs, geo, pts);
    }

    #[test]
    fn the_lasso_of_the_point_editor_drags_clicks_and_cancels_like_the_mask_lasso() {
        let (mut app, geo, pts) = pane_setup();
        app.pe.tool = PointTool::SelectLasso;
        let at = |x: f32, y: f32| Pos2::new(x, y);
        let none = egui::Modifiers::default();
        let sel = |app: &PanoApp| {
            let mut v: Vec<usize> = app.pe.sel.iter().copied().collect();
            v.sort_unstable();
            v
        };
        // drag around the first two points: selected on release
        lasso_events(
            &mut app,
            &geo,
            &pts,
            vec![
                PaneEvent::DragStart(Pane::A, at(0.5, 0.5), true, none),
                PaneEvent::Drag(Pane::A, at(50.5, 0.5), Vec2::ZERO),
                PaneEvent::Drag(Pane::A, at(50.5, 30.5), Vec2::ZERO),
                PaneEvent::Drag(Pane::A, at(0.5, 30.5), Vec2::ZERO),
            ],
        );
        assert!(app.pe.sel.is_empty() && app.pe.lasso.dragging(), "nothing is selected before the button is released");
        lasso_events(&mut app, &geo, &pts, vec![PaneEvent::DragEnd]);
        assert_eq!(sel(&app), vec![0, 1]);
        assert!(!app.pe.lasso.busy());
        // a shaky click sets one corner and selects nothing
        app.pe.sel.clear();
        lasso_events(&mut app, &geo, &pts, vec![PaneEvent::DragStart(Pane::A, at(150.5, 150.5), true, none), PaneEvent::Drag(Pane::A, at(153.5, 151.5), Vec2::ZERO), PaneEvent::DragEnd]);
        assert_eq!(app.pe.lasso.corners.len(), 1);
        assert!(app.pe.sel.is_empty());
        // Esc (what points.rs does) throws it away
        app.pe.lasso.cancel();
        // clicked corners close with Enter; Shift adds to the selection
        app.pe.sel.insert(2);
        let shift = egui::Modifiers { shift: true, ..Default::default() };
        let corners = [at(0.5, 0.5), at(60.5, 0.5), at(60.5, 40.5), at(0.5, 40.5)];
        let evs = corners.iter().enumerate().map(|(i, c)| PaneEvent::Click(Pane::A, *c, if i == 0 { shift } else { none })).collect();
        lasso_events(&mut app, &geo, &pts, evs);
        assert_eq!(app.pe.lasso.corners.len(), 4);
        app.lasso_enter(&pts);
        assert_eq!(sel(&app), vec![0, 1, 2]);
        assert!(!app.pe.lasso.busy());
        // a click on a point with no lasso going on still selects that point
        lasso_events(&mut app, &geo, &pts, vec![PaneEvent::Click(Pane::A, at(10.5, 10.5), none)]);
        assert_eq!(app.pe.selected, Some(0));
        assert!(!app.pe.lasso.busy());
        // another tool or another pair drops a lasso in progress
        app.pe.lasso.click((1.0, 1.0));
        app.pe.lasso_ctx = Some((Pane::A, (app.pe.a, app.pe.b), none));
        app.sync_lasso();
        assert!(app.pe.lasso.busy());
        app.pe.tool = PointTool::Edit;
        app.sync_lasso();
        assert!(!app.pe.lasso.busy());
    }

    #[test]
    fn modifiers_choose_the_operation() {
        use PointTool::*;
        assert_eq!(SelOp::for_tool(SelectRect, false, false), SelOp::Replace);
        assert_eq!(SelOp::for_tool(SelectLasso, true, false), SelOp::Add);
        assert_eq!(SelOp::for_tool(SelectRect, false, true), SelOp::Subtract);
        assert_eq!(SelOp::for_tool(SelectBrush, false, false), SelOp::Add);
        assert_eq!(SelOp::for_tool(SelectBrush, true, false), SelOp::Add);
        assert_eq!(SelOp::for_tool(SelectBrush, false, true), SelOp::Subtract);
    }
}
