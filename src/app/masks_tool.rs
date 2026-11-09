//! The mask tools: turns pointer events (already in screen coordinates) into strokes.
//! Brush: press, drag, release = one stroke. Lasso: the same lasso as in the point editor
//! (`select_shapes::Lasso`: drag around an area, or click corner by corner and close with a double
//! click or Enter; Escape throws it away). Pure logic, no window code.

use super::masks_state::PhotoMask;
use super::select_shapes::Lasso;
use super::types::PaneGeo;
use crate::masks::{MaskKind, MaskStroke};
use eframe::egui::{Pos2, Vec2};

/// Brush size is a fraction of the long side of the photo.
pub(super) const DEFAULT_BRUSH: f32 = 0.02;
pub(super) const MIN_BRUSH: f32 = 0.002;
pub(super) const MAX_BRUSH: f32 = 0.10;
/// A new point of a brush stroke is only taken when it is this far (in brush radii) from the last one.
const STEP_IN_RADII: f32 = 0.25;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Tool {
    BrushPositive,
    BrushNegative,
    LassoPositive,
    LassoNegative,
    Erase,
}

impl Tool {
    pub(super) const ALL: [Tool; 5] = [Tool::BrushPositive, Tool::BrushNegative, Tool::LassoPositive, Tool::LassoNegative, Tool::Erase];

    pub(super) fn kind(self) -> MaskKind {
        match self {
            Tool::BrushPositive | Tool::LassoPositive => MaskKind::Positive,
            Tool::BrushNegative | Tool::LassoNegative => MaskKind::Negative,
            Tool::Erase => MaskKind::Erase,
        }
    }

    pub(super) fn is_lasso(self) -> bool {
        matches!(self, Tool::LassoPositive | Tool::LassoNegative)
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Tool::BrushPositive => "Brush: use this photo",
            Tool::BrushNegative => "Brush: do not use",
            Tool::LassoPositive => "Lasso: use this photo",
            Tool::LassoNegative => "Lasso: do not use",
            Tool::Erase => "Eraser",
        }
    }
}

/// What happened with the pointer.
#[derive(Clone, Copy, Debug)]
pub(super) enum Ptr {
    Down(Pos2),
    Move(Pos2),
    Up,
    Click(Pos2),
    DoubleClick,
    Enter,
    Escape,
}

/// Screen position to fractions of the photo (x / width, y / height); may lie outside 0..1.
pub(super) fn to_frac(geo: &PaneGeo, pos: Pos2) -> [f32; 2] {
    [(pos.x - geo.origin.x) / (geo.w * geo.scale), (pos.y - geo.origin.y) / (geo.h * geo.scale)]
}

/// Fractions of the photo to screen position.
pub(super) fn to_screen(geo: &PaneGeo, f: [f32; 2]) -> Pos2 {
    geo.origin + Vec2::new(f[0] * geo.w * geo.scale, f[1] * geo.h * geo.scale)
}

/// Brush radius on the screen (pixels) for a radius given as a fraction of the long side.
pub(super) fn screen_radius(geo: &PaneGeo, radius: f32) -> f32 {
    radius * geo.w.max(geo.h) * geo.scale
}

fn inside(f: [f32; 2]) -> bool {
    (0.0..=1.0).contains(&f[0]) && (0.0..=1.0).contains(&f[1])
}

fn clamp01(f: [f32; 2]) -> [f32; 2] {
    [f[0].clamp(0.0, 1.0), f[1].clamp(0.0, 1.0)]
}

/// Photo pixels (the lasso works in them, in both editors) to fractions of the photo and back.
fn px_to_frac(geo: &PaneGeo, p: (f64, f64)) -> [f32; 2] {
    [(p.0 / geo.w as f64) as f32, (p.1 / geo.h as f64) as f32]
}

fn frac_to_px(geo: &PaneGeo, f: [f32; 2]) -> (f64, f64) {
    (f[0] as f64 * geo.w as f64, f[1] as f64 * geo.h as f64)
}

pub(super) struct MaskTool {
    pub(super) tool: Tool,
    pub(super) radius: f32,
    /// the brush stroke being painted
    pub(super) stroke: Option<MaskStroke>,
    /// the lasso being drawn; its corners are photo pixels of the working image (`geo.w` x `geo.h`)
    pub(super) lasso: Lasso,
}

impl Default for MaskTool {
    fn default() -> Self {
        // red first: taking something out (a boat, a person) is the common case, and a green stroke by
        // mistake forces a photo in where it was not wanted
        Self { tool: Tool::BrushNegative, radius: DEFAULT_BRUSH, stroke: None, lasso: Lasso::default() }
    }
}

impl MaskTool {
    /// Throws away what is being drawn (tool or photo changed).
    pub(super) fn cancel(&mut self) {
        self.stroke = None;
        self.lasso.cancel();
    }

    #[cfg(test)]
    pub(super) fn busy(&self) -> bool {
        self.stroke.is_some() || self.lasso.busy()
    }

    /// Takes the last lasso corner back; false if there is none.
    pub(super) fn pop_corner(&mut self) -> bool {
        self.lasso.pop_corner()
    }

    /// Feeds one event of the pointer; returns true if the mask of the photo changed (a stroke was added).
    pub(super) fn handle(&mut self, ev: Ptr, geo: &PaneGeo, mask: &mut PhotoMask, w: usize, h: usize) -> bool {
        match ev {
            Ptr::Down(p) if !self.tool.is_lasso() => {
                let f = to_frac(geo, p);
                if inside(f) {
                    self.stroke = Some(MaskStroke { kind: self.tool.kind(), points: vec![f], radius: self.radius, polygon: false });
                }
                false
            }
            Ptr::Down(p) => {
                // lasso: a drag draws the outline (or, while corners are being clicked, adds to them)
                let f = to_frac(geo, p);
                if inside(f) {
                    self.lasso.down(frac_to_px(geo, f));
                }
                false
            }
            Ptr::Move(p) if self.lasso.dragging() => {
                let f = to_frac(geo, p);
                self.lasso.drag(frac_to_px(geo, f), geo.scale as f64, Some((geo.w as f64, geo.h as f64)));
                false
            }
            Ptr::Up if self.lasso.dragging() => match self.lasso.up(geo.scale as f64) {
                Some(poly) => self.commit_lasso(poly, geo, mask, w, h),
                None => false,
            },
            Ptr::Move(p) => {
                let f = clamp01(to_frac(geo, p));
                let step = self.radius * STEP_IN_RADII * geo.w.max(geo.h);
                if let Some(s) = &mut self.stroke {
                    let far = s.points.last().is_none_or(|l| ((f[0] - l[0]) * geo.w).hypot((f[1] - l[1]) * geo.h) >= step);
                    if far {
                        s.points.push(f);
                    }
                }
                false
            }
            Ptr::Up => match self.stroke.take() {
                Some(s) => mask.commit(s, w, h),
                None => false,
            },
            Ptr::Click(p) if self.tool.is_lasso() => {
                let f = to_frac(geo, p);
                if inside(f) {
                    self.lasso.click(frac_to_px(geo, f));
                }
                false
            }
            Ptr::DoubleClick | Ptr::Enter if self.tool.is_lasso() => match self.lasso.close() {
                Some(poly) => self.commit_lasso(poly, geo, mask, w, h),
                None => false,
            },
            Ptr::Escape => {
                self.cancel();
                false
            }
            _ => false,
        }
    }

    /// A finished lasso becomes a polygon stroke (corners as fractions of the photo).
    fn commit_lasso(&mut self, poly: Vec<(f64, f64)>, geo: &PaneGeo, mask: &mut PhotoMask, w: usize, h: usize) -> bool {
        let points: Vec<[f32; 2]> = poly.into_iter().map(|p| clamp01(px_to_frac(geo, p))).collect();
        mask.commit(MaskStroke { kind: self.tool.kind(), points, radius: 0.0, polygon: true }, w, h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::masks::{NEGATIVE, POSITIVE};
    use eframe::egui::Rect;

    /// A 200 x 100 photo shown at 2 screen pixels per photo pixel, its corner at (10, 20).
    fn geo() -> PaneGeo {
        PaneGeo { rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(500.0, 300.0)), origin: Pos2::new(10.0, 20.0), scale: 2.0, fit: 2.0, w: 200.0, h: 100.0 }
    }

    fn at(t: &mut MaskTool, m: &mut PhotoMask, ev: Ptr) -> bool {
        t.handle(ev, &geo(), m, 200, 100)
    }

    fn p(x: f32, y: f32) -> Pos2 {
        Pos2::new(x, y)
    }

    #[test]
    fn screen_and_fractions_convert_both_ways() {
        let g = geo();
        assert_eq!(to_frac(&g, p(10.0, 20.0)), [0.0, 0.0]);
        assert_eq!(to_frac(&g, p(410.0, 220.0)), [1.0, 1.0]);
        let f = to_frac(&g, p(210.0, 120.0));
        assert_eq!(f, [0.5, 0.5]);
        assert_eq!(to_screen(&g, f), p(210.0, 120.0));
        // 2 % of the long side (200 px) = 4 photo pixels = 8 screen pixels
        assert!((screen_radius(&g, 0.02) - 8.0).abs() < 1e-4);
    }

    #[test]
    fn a_brush_stroke_is_made_from_press_drag_release() {
        let (mut t, mut m) = (MaskTool { tool: Tool::BrushPositive, ..Default::default() }, PhotoMask::default());
        assert!(!at(&mut t, &mut m, Ptr::Down(p(110.0, 120.0))));
        at(&mut t, &mut m, Ptr::Move(p(112.0, 120.0))); // too close: skipped
        at(&mut t, &mut m, Ptr::Move(p(210.0, 120.0)));
        at(&mut t, &mut m, Ptr::Move(p(900.0, 120.0))); // outside: clamped to the edge
        assert!(m.strokes.is_empty(), "nothing is committed before the button is released");
        assert!(at(&mut t, &mut m, Ptr::Up));
        assert_eq!(m.strokes.len(), 1);
        let s = &m.strokes[0];
        assert_eq!(s.kind, MaskKind::Positive);
        assert_eq!(s.points.len(), 3);
        assert_eq!(s.points[2][0], 1.0);
        assert!(!s.polygon && s.radius == DEFAULT_BRUSH);
        assert_eq!(m.buf.as_ref().unwrap().at(100.0, 50.0, 200.0, 100.0), POSITIVE);
    }

    #[test]
    fn pressing_outside_the_photo_paints_nothing_and_a_single_dab_works() {
        let (mut t, mut m) = (MaskTool { tool: Tool::BrushNegative, ..Default::default() }, PhotoMask::default());
        at(&mut t, &mut m, Ptr::Down(p(2.0, 2.0)));
        assert!(!at(&mut t, &mut m, Ptr::Up));
        at(&mut t, &mut m, Ptr::Down(p(210.0, 120.0)));
        assert!(at(&mut t, &mut m, Ptr::Up));
        assert_eq!(m.strokes[0].points.len(), 1);
        assert_eq!(m.buf.as_ref().unwrap().at(100.0, 50.0, 200.0, 100.0), NEGATIVE);
    }

    #[test]
    fn the_editor_starts_with_the_red_brush() {
        assert_eq!(MaskTool::default().tool, Tool::BrushNegative);
    }

    #[test]
    fn the_eraser_makes_an_erase_stroke_with_the_chosen_size() {
        let (mut t, mut m) = (MaskTool { tool: Tool::Erase, radius: 0.05, ..Default::default() }, PhotoMask::default());
        at(&mut t, &mut m, Ptr::Down(p(210.0, 120.0)));
        at(&mut t, &mut m, Ptr::Up);
        assert_eq!((m.strokes[0].kind, m.strokes[0].radius), (MaskKind::Erase, 0.05));
    }

    #[test]
    fn a_lasso_closes_with_double_click_or_enter_and_needs_three_corners() {
        let (mut t, mut m) = (MaskTool { tool: Tool::LassoNegative, ..Default::default() }, PhotoMask::default());
        for c in [(60.0, 50.0), (360.0, 50.0)] {
            at(&mut t, &mut m, Ptr::Click(p(c.0, c.1)));
        }
        assert!(!at(&mut t, &mut m, Ptr::Enter), "two corners are no area");
        assert!(!t.busy());
        for c in [(60.0, 50.0), (360.0, 50.0), (360.0, 190.0), (60.0, 190.0)] {
            at(&mut t, &mut m, Ptr::Click(p(c.0, c.1)));
        }
        assert!(t.busy());
        assert!(at(&mut t, &mut m, Ptr::DoubleClick));
        assert!(m.strokes[0].polygon && m.strokes[0].points.len() == 4 && m.strokes[0].kind == MaskKind::Negative);
        assert_eq!(m.buf.as_ref().unwrap().at(100.0, 50.0, 200.0, 100.0), NEGATIVE);
        // Enter closes as well
        for c in [(60.0, 50.0), (360.0, 50.0), (200.0, 190.0)] {
            at(&mut t, &mut m, Ptr::Click(p(c.0, c.1)));
        }
        assert!(at(&mut t, &mut m, Ptr::Enter));
        assert_eq!(m.strokes.len(), 2);
    }

    /// Like the lasso of the point editor: press, drag around, release.
    #[test]
    fn a_dragged_lasso_closes_on_release() {
        let (mut t, mut m) = (MaskTool { tool: Tool::LassoNegative, ..Default::default() }, PhotoMask::default());
        at(&mut t, &mut m, Ptr::Down(p(60.0, 50.0)));
        for q in [(200.0, 50.0), (360.0, 50.0), (360.0, 190.0), (200.0, 190.0), (60.0, 190.0)] {
            at(&mut t, &mut m, Ptr::Move(p(q.0, q.1)));
        }
        assert!(m.strokes.is_empty());
        assert!(at(&mut t, &mut m, Ptr::Up), "the area is masked on release");
        assert!(m.strokes[0].polygon && m.strokes[0].points.len() == 6);
        assert_eq!(m.buf.as_ref().unwrap().at(100.0, 50.0, 200.0, 100.0), NEGATIVE);
        assert!(!t.busy());
    }

    /// A click that moved a few pixels arrives as a tiny drag: it must still set one corner.
    #[test]
    fn a_shaky_click_on_the_lasso_still_sets_a_corner() {
        let (mut t, mut m) = (MaskTool { tool: Tool::LassoPositive, ..Default::default() }, PhotoMask::default());
        at(&mut t, &mut m, Ptr::Down(p(100.0, 100.0)));
        at(&mut t, &mut m, Ptr::Move(p(105.0, 103.0)));
        assert!(!at(&mut t, &mut m, Ptr::Up));
        assert_eq!(t.lasso.corners.len(), 1);
        at(&mut t, &mut m, Ptr::Click(p(300.0, 100.0)));
        // a drag while corners are being clicked joins them and leaves the shape open
        at(&mut t, &mut m, Ptr::Down(p(300.0, 150.0)));
        at(&mut t, &mut m, Ptr::Move(p(250.0, 190.0)));
        at(&mut t, &mut m, Ptr::Move(p(150.0, 190.0)));
        assert!(!at(&mut t, &mut m, Ptr::Up));
        assert!(t.lasso.corners.len() >= 4 && m.strokes.is_empty());
        assert!(at(&mut t, &mut m, Ptr::Enter));
        assert_eq!(m.buf.as_ref().unwrap().at(100.0, 60.0, 200.0, 100.0), POSITIVE);
    }

    #[test]
    fn escape_and_cancel_throw_away_what_is_drawn() {
        let (mut t, mut m) = (MaskTool { tool: Tool::LassoPositive, ..Default::default() }, PhotoMask::default());
        at(&mut t, &mut m, Ptr::Click(p(100.0, 100.0)));
        at(&mut t, &mut m, Ptr::Click(p(120.0, 100.0)));
        assert!(t.pop_corner() && t.lasso.corners.len() == 1);
        at(&mut t, &mut m, Ptr::Escape);
        assert!(!t.busy() && m.strokes.is_empty());
        t.tool = Tool::BrushPositive;
        at(&mut t, &mut m, Ptr::Down(p(100.0, 100.0)));
        assert!(t.busy());
        t.cancel();
        assert!(!at(&mut t, &mut m, Ptr::Up) && m.strokes.is_empty());
    }
}
