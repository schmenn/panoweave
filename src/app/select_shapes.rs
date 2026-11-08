//! Selection shapes of the point editor (rectangle, lasso, brush): pure geometry that says which
//! points lie in a shape. All coordinates are pixels of one photo (the pane that was dragged in).

use super::*;
use std::collections::HashSet;

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
