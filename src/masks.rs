//! Masks the person paints on a photo to steer the seams: "use this photo here" (positive) and
//! "do not use this photo here" (negative). They are stored as strokes and polygons in coordinates
//! that do not depend on the resolution (fractions of the photo), and turned into a small picture
//! (`MaskBuf`) when the panorama is made. Later strokes paint over earlier ones; an eraser stroke
//! removes what is below it.

use serde::{Deserialize, Serialize};

/// What the mask says at a spot.
pub const NEUTRAL: u8 = 0;
pub const POSITIVE: u8 = 1;
pub const NEGATIVE: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MaskKind {
    /// use this photo here
    Positive,
    /// do not use this photo here
    Negative,
    /// remove the mask below
    Erase,
}

/// One brush stroke (a line of points with a round brush) or one filled polygon (lasso).
/// Coordinates are fractions of the photo: x / width and y / height; `radius` is a fraction of the long side.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MaskStroke {
    pub kind: MaskKind,
    pub points: Vec<[f32; 2]>,
    /// brush radius (fraction of the long side); ignored for polygons
    #[serde(default)]
    pub radius: f32,
    /// a filled polygon instead of a brush stroke
    #[serde(default)]
    pub polygon: bool,
}

/// The mask of one photo as a picture (1 byte per pixel: `NEUTRAL`, `POSITIVE`, `NEGATIVE`).
#[derive(Clone, Debug, PartialEq)]
pub struct MaskBuf {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

impl MaskBuf {
    /// The mask value at position (`x`, `y`) of a photo that is `pw` x `ph` pixels (any resolution of the same photo).
    #[inline]
    pub fn at(&self, x: f32, y: f32, pw: f64, ph: f64) -> u8 {
        let xi = (((x as f64 + 0.5) / pw) * self.w as f64).floor().clamp(0.0, self.w as f64 - 1.0) as usize;
        let yi = (((y as f64 + 0.5) / ph) * self.h as f64).floor().clamp(0.0, self.h as f64 - 1.0) as usize;
        self.data[yi * self.w + xi]
    }

    /// True if anything is painted.
    pub fn any(&self) -> bool {
        self.data.iter().any(|&v| v != NEUTRAL)
    }
}

/// Longest side of the picture a mask is painted into (pixels). The seams are found on a coarse grid,
/// so this is plenty.
pub const MASK_LONG_SIDE: usize = 1600;

fn value_of(kind: MaskKind) -> u8 {
    match kind {
        MaskKind::Positive => POSITIVE,
        MaskKind::Negative => NEGATIVE,
        MaskKind::Erase => NEUTRAL,
    }
}

/// Distance from point (`px`,`py`) to the segment (`ax`,`ay`)-(`bx`,`by`).
fn dist_to_segment(px: f32, py: f32, a: [f32; 2], b: [f32; 2]) -> f32 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= 1e-12 { 0.0 } else { (((px - a[0]) * dx + (py - a[1]) * dy) / len2).clamp(0.0, 1.0) };
    (px - (a[0] + t * dx)).hypot(py - (a[1] + t * dy))
}

/// Even-odd test: is the point inside the polygon?
fn in_polygon(px: f32, py: f32, pts: &[[f32; 2]]) -> bool {
    let mut inside = false;
    let mut j = pts.len() - 1;
    for i in 0..pts.len() {
        let (a, b) = (pts[i], pts[j]);
        if (a[1] > py) != (b[1] > py) && px < (b[0] - a[0]) * (py - a[1]) / (b[1] - a[1]) + a[0] {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Paints the strokes of a photo of `w` x `h` pixels into a picture of at most `MASK_LONG_SIDE` pixels.
/// Returns `None` when nothing is painted (so photos without masks cost nothing).
pub fn rasterize(strokes: &[MaskStroke], w: usize, h: usize) -> Option<MaskBuf> {
    if strokes.iter().all(|s| s.points.is_empty()) || w == 0 || h == 0 {
        return None;
    }
    let scale = (MASK_LONG_SIDE as f64 / w.max(h) as f64).min(1.0);
    let (mw, mh) = (((w as f64 * scale).round() as usize).max(1), ((h as f64 * scale).round() as usize).max(1));
    let mut buf = MaskBuf { w: mw, h: mh, data: vec![NEUTRAL; mw * mh] };
    let long = mw.max(mh) as f32;
    for s in strokes {
        if s.points.is_empty() {
            continue;
        }
        let v = value_of(s.kind);
        // work in picture pixels
        let pts: Vec<[f32; 2]> = s.points.iter().map(|p| [p[0] * mw as f32, p[1] * mh as f32]).collect();
        let rad = (s.radius * long).max(0.5);
        let margin = if s.polygon { 1.0 } else { rad + 1.0 };
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for p in &pts {
            x0 = x0.min(p[0]);
            y0 = y0.min(p[1]);
            x1 = x1.max(p[0]);
            y1 = y1.max(p[1]);
        }
        let (bx0, by0) = ((x0 - margin).floor().max(0.0) as usize, (y0 - margin).floor().max(0.0) as usize);
        let (bx1, by1) = (((x1 + margin).ceil().max(0.0) as usize).min(mw), ((y1 + margin).ceil().max(0.0) as usize).min(mh));
        for y in by0..by1 {
            for x in bx0..bx1 {
                let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
                let hit = if s.polygon {
                    pts.len() >= 3 && in_polygon(cx, cy, &pts)
                } else if pts.len() == 1 {
                    (cx - pts[0][0]).hypot(cy - pts[0][1]) <= rad
                } else {
                    pts.windows(2).any(|seg| dist_to_segment(cx, cy, seg[0], seg[1]) <= rad)
                };
                if hit {
                    buf.data[y * mw + x] = v;
                }
            }
        }
    }
    buf.any().then_some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brush(kind: MaskKind, pts: &[[f32; 2]], radius: f32) -> MaskStroke {
        MaskStroke { kind, points: pts.to_vec(), radius, polygon: false }
    }

    #[test]
    fn a_stroke_paints_a_round_line() {
        let m = rasterize(&[brush(MaskKind::Positive, &[[0.2, 0.5], [0.8, 0.5]], 0.05)], 400, 200).unwrap();
        // 400 x 200 picture; the line runs along the middle row with a radius of 20 px
        assert_eq!(m.at(200.0, 100.0, 400.0, 200.0), POSITIVE);
        assert_eq!(m.at(100.0, 110.0, 400.0, 200.0), POSITIVE);
        assert_eq!(m.at(100.0, 140.0, 400.0, 200.0), NEUTRAL);
        assert_eq!(m.at(10.0, 100.0, 400.0, 200.0), NEUTRAL);
        // the round ends reach a little beyond the points
        assert_eq!(m.at(70.0, 100.0, 400.0, 200.0), POSITIVE);
    }

    #[test]
    fn a_polygon_is_filled_and_later_strokes_paint_over_earlier_ones() {
        let square = MaskStroke { kind: MaskKind::Negative, points: vec![[0.25, 0.25], [0.75, 0.25], [0.75, 0.75], [0.25, 0.75]], radius: 0.0, polygon: true };
        let m = rasterize(&[square.clone()], 200, 200).unwrap();
        assert_eq!(m.at(100.0, 100.0, 200.0, 200.0), NEGATIVE);
        assert_eq!(m.at(20.0, 100.0, 200.0, 200.0), NEUTRAL);
        // positive on top, then an eraser through the middle
        let strokes = [square, brush(MaskKind::Positive, &[[0.4, 0.4], [0.6, 0.4]], 0.04), brush(MaskKind::Erase, &[[0.5, 0.3], [0.5, 0.7]], 0.02)];
        let m = rasterize(&strokes, 200, 200).unwrap();
        assert_eq!(m.at(85.0, 80.0, 200.0, 200.0), POSITIVE);
        assert_eq!(m.at(100.0, 100.0, 200.0, 200.0), NEUTRAL, "the eraser removes both below it");
        assert_eq!(m.at(60.0, 130.0, 200.0, 200.0), NEGATIVE);
    }

    #[test]
    fn nothing_painted_gives_no_mask_and_big_photos_get_a_small_picture() {
        assert!(rasterize(&[], 100, 100).is_none());
        assert!(rasterize(&[brush(MaskKind::Positive, &[], 0.1)], 100, 100).is_none());
        // painted and erased again: nothing left
        let both = [brush(MaskKind::Positive, &[[0.5, 0.5]], 0.1), brush(MaskKind::Erase, &[[0.5, 0.5]], 0.3)];
        assert!(rasterize(&both, 100, 100).is_none());
        let m = rasterize(&[brush(MaskKind::Positive, &[[0.5, 0.5]], 0.1)], 6000, 4000).unwrap();
        assert_eq!((m.w, m.h), (1600, 1067));
        // the position is looked up in the pixels of any resolution of the photo
        assert_eq!(m.at(3000.0, 2000.0, 6000.0, 4000.0), POSITIVE);
        assert_eq!(m.at(1500.0, 1000.0, 3000.0, 2000.0), POSITIVE);
        assert_eq!(m.at(100.0, 100.0, 6000.0, 4000.0), NEUTRAL);
    }

    #[test]
    fn strokes_survive_the_project_file_and_old_strokes_load() {
        let s = brush(MaskKind::Negative, &[[0.1, 0.2], [0.3, 0.4]], 0.02);
        let js = serde_json::to_string(&s).unwrap();
        let back: MaskStroke = serde_json::from_str(&js).unwrap();
        assert_eq!(back, s);
        // `radius` and `polygon` are optional
        let b: MaskStroke = serde_json::from_str(r#"{"kind":"Positive","points":[[0.5,0.5]]}"#).unwrap();
        assert!(!b.polygon && b.radius == 0.0);
    }
}
