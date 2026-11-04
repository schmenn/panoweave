//! Projections, panorama rendering (fast preview, feather and multi-band blending),
//! brightness/colour matching and automatic cropping.

use crate::align::{project, Lens};
use crate::geometry::*;
use crate::imgutil::{linear_to_srgb, sample_rgb_linear};
use image::RgbImage;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::f64::consts::{FRAC_PI_2, PI};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Projection {
    Equirectangular,
    Cylindrical,
    Rectilinear,
    Mercator,
    Stereographic,
    Fisheye,
}

impl Projection {
    pub const ALL: [Projection; 6] = [
        Projection::Equirectangular,
        Projection::Cylindrical,
        Projection::Rectilinear,
        Projection::Mercator,
        Projection::Stereographic,
        Projection::Fisheye,
    ];
    pub fn name(&self) -> &'static str {
        match self {
            Projection::Equirectangular => "Spherical (360°)",
            Projection::Cylindrical => "Cylindrical",
            Projection::Rectilinear => "Flat (rectilinear)",
            Projection::Mercator => "Mercator",
            Projection::Stereographic => "Little planet / stereographic",
            Projection::Fisheye => "Fisheye",
        }
    }
    pub fn hint(&self) -> &'static str {
        match self {
            Projection::Equirectangular => "Best for full 360° or very tall panoramas.",
            Projection::Cylindrical => "Classic wide panorama; straight vertical lines.",
            Projection::Rectilinear => "Keeps all straight lines straight; up to ~120° wide.",
            Projection::Mercator => "Like cylindrical, less stretched at top and bottom.",
            Projection::Stereographic => "Wide views with round shapes; point down for a 'little planet'.",
            Projection::Fisheye => "Even angular spacing, strong curvature.",
        }
    }
    /// Plane coordinates -> view direction (x right, y down, z forward).
    #[inline]
    pub fn to_dir(&self, x: f64, y: f64) -> Option<V3> {
        match self {
            Projection::Equirectangular => {
                if x.abs() > PI || y.abs() > FRAC_PI_2 {
                    return None;
                }
                let (sl, cl) = x.sin_cos();
                let (sp, cp) = (-y).sin_cos();
                Some([cp * sl, -sp, cp * cl])
            }
            Projection::Cylindrical => {
                if x.abs() > PI {
                    return None;
                }
                let (sl, cl) = x.sin_cos();
                Some(normalize([sl, y, cl]))
            }
            Projection::Mercator => {
                if x.abs() > PI {
                    return None;
                }
                let phi = (-y).sinh().atan();
                let (sl, cl) = x.sin_cos();
                let (sp, cp) = phi.sin_cos();
                Some([cp * sl, -sp, cp * cl])
            }
            Projection::Rectilinear => Some(normalize([x, y, 1.0])),
            Projection::Stereographic | Projection::Fisheye => {
                let r = (x * x + y * y).sqrt();
                if r < 1e-12 {
                    return Some([0.0, 0.0, 1.0]);
                }
                let th = if *self == Projection::Stereographic { 2.0 * (r / 2.0).atan() } else { r };
                if th > PI {
                    return None;
                }
                let s = th.sin();
                Some([s * x / r, s * y / r, th.cos()])
            }
        }
    }
    /// View direction -> plane coordinates.
    #[inline]
    pub fn from_dir(&self, d: V3) -> Option<(f64, f64)> {
        let d = normalize(d);
        match self {
            Projection::Equirectangular => Some((d[0].atan2(d[2]), (d[1]).clamp(-1.0, 1.0).asin())),
            Projection::Cylindrical => {
                let h = (d[0] * d[0] + d[2] * d[2]).sqrt();
                if h < 1e-9 {
                    return None;
                }
                Some((d[0].atan2(d[2]), (d[1] / h).clamp(-20.0, 20.0)))
            }
            Projection::Mercator => {
                let phi = (-d[1]).clamp(-1.0, 1.0).asin();
                if phi.abs() > 1.5 {
                    return None;
                }
                Some((d[0].atan2(d[2]), -(phi.tan().asinh())))
            }
            Projection::Rectilinear => {
                if d[2] <= 0.02 {
                    return None;
                }
                Some((d[0] / d[2], d[1] / d[2]))
            }
            Projection::Stereographic | Projection::Fisheye => {
                let th = d[2].clamp(-1.0, 1.0).acos();
                let rho = (d[0] * d[0] + d[1] * d[1]).sqrt();
                if rho < 1e-12 {
                    return Some((0.0, 0.0));
                }
                let r = if *self == Projection::Stereographic {
                    if th > 3.0 {
                        return None;
                    }
                    2.0 * (th / 2.0).tan()
                } else {
                    th
                };
                Some((r * d[0] / rho, r * d[1] / rho))
            }
        }
    }
    /// Largest sensible plane extent.
    pub fn limits(&self) -> [f64; 4] {
        match self {
            Projection::Equirectangular => [-PI, PI, -FRAC_PI_2, FRAC_PI_2],
            Projection::Cylindrical => [-PI, PI, -2.5, 2.5],
            Projection::Mercator => [-PI, PI, -2.5, 2.5],
            Projection::Rectilinear => [-4.0, 4.0, -4.0, 4.0],
            Projection::Stereographic => [-5.0, 5.0, -5.0, 5.0],
            Projection::Fisheye => [-PI, PI, -PI, PI],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PanoView {
    pub projection: Projection,
    /// extra rotation of the whole panorama chosen by the user
    pub yaw: f64,
    pub pitch: f64,
    pub roll: f64,
    /// visible plane extent [x0, x1, y0, y1]
    pub bounds: [f64; 4],
    /// crop in fractions of the bounds [left, right, top, bottom] (0..1)
    pub crop: [f64; 4],
}

impl Default for PanoView {
    fn default() -> Self {
        PanoView {
            projection: Projection::Cylindrical,
            yaw: 0.0,
            pitch: 0.0,
            roll: 0.0,
            bounds: [-1.0, 1.0, -0.5, 0.5],
            crop: [0.0, 1.0, 0.0, 1.0],
        }
    }
}

impl PanoView {
    /// world -> view rotation
    pub fn rotation(&self) -> M3 {
        camera_from_ypr(self.yaw, self.pitch, self.roll)
    }
    pub fn aspect(&self) -> f64 {
        (self.bounds[1] - self.bounds[0]) / (self.bounds[3] - self.bounds[2]).max(1e-9)
    }
    pub fn crop_bounds(&self) -> [f64; 4] {
        let [x0, x1, y0, y1] = self.bounds;
        let c = self.crop;
        [x0 + (x1 - x0) * c[0], x0 + (x1 - x0) * c[1], y0 + (y1 - y0) * c[2], y0 + (y1 - y0) * c[3]]
    }
    /// Horizontal & vertical field of view of the bounds, in degrees (approximate for flat).
    pub fn fov_deg(&self) -> (f64, f64) {
        let b = self.bounds;
        let p = self.projection;
        let ang = |a: (f64, f64), c: (f64, f64)| -> f64 {
            match (p.to_dir(a.0, a.1), p.to_dir(c.0, c.1)) {
                (Some(u), Some(v)) => dot(u, v).clamp(-1.0, 1.0).acos().to_degrees(),
                _ => 0.0,
            }
        };
        let h = match p {
            Projection::Equirectangular | Projection::Cylindrical | Projection::Mercator => (b[1] - b[0]).to_degrees(),
            _ => ang((b[0], 0.0), (0.0, 0.0)) + ang((0.0, 0.0), (b[1], 0.0)),
        };
        let v = match p {
            Projection::Equirectangular => (b[3] - b[2]).to_degrees(),
            _ => ang((0.0, b[2]), (0.0, 0.0)) + ang((0.0, 0.0), (0.0, b[3])),
        };
        (h, v)
    }
}

/// Everything the renderer needs about one photo.
#[derive(Clone)]
pub struct RenderPhoto {
    pub img: Arc<RgbImage>,
    /// pixel size the geometry refers to (equals img size unless only geometry is needed)
    pub w: f64,
    pub h: f64,
    pub rot: M3,
    pub lens: Lens,
    /// local parallax correction (added to the sampling position), if any
    pub field: Option<Arc<crate::fine_align::DisplacementField>>,
    pub gain: [f32; 3],
    /// long side of the ORIGINAL photo in pixels (its real resolution; `w`/`h` may describe a
    /// smaller working copy)
    pub full_long: f64,
    /// joined to the main group of photos by control points; a photo without any connection
    /// may sit at a wrong place, so it is only used where no connected photo covers the spot
    pub trusted: bool,
}

/// Quality of a photo = how many real pixels it has per angle (focal length × long side).
/// Where photos overlap, the one with the most detail is used (a zoomed-in or higher-resolution
/// photo beats a wide one). Photos whose detail differs by less than about 20 % count as equal
/// and are blended as before (seams in the middle of the overlap).
pub const TIERS_PER_OCTAVE: f64 = 2.0;
/// One tier of more detail counts this many times more (seam choice and soft fade).
pub const TIER_BASE: f64 = 4.0;
/// Near the border of a photo (centre weight below this) a more detailed photo hands over to
/// the next one, so its blurry/vignetted edge is not used when another photo covers the spot.
pub const EDGE_RAMP: f32 = 0.2;
/// Share of the centre weight kept in the seam score as a tie-break between equal photos.
pub const TIE_BREAK: f32 = 0.01;

/// Quality factor of a photo that is not joined to the others (it only fills gaps).
pub const UNTRUSTED_QUALITY: f32 = 1e-9;

/// Quality factor (≤ 1) per photo from the detail of every connected photo:
/// `TIER_BASE` ^ (tier - best tier).
pub fn quality_factors(photos: &[(f64, bool)]) -> Vec<f32> {
    let best = photos.iter().filter(|p| p.1).map(|p| p.0).fold(0.0, f64::max);
    photos
        .iter()
        .map(|&(d, trusted)| {
            if !trusted {
                return UNTRUSTED_QUALITY;
            }
            if best <= 0.0 || d <= 0.0 {
                return 1.0;
            }
            let tier = ((d / best).log2() * TIERS_PER_OCTAVE).round();
            TIER_BASE.powf(tier).max(1e-30) as f32
        })
        .collect()
}

/// Which photo wins a spot: quality first, inside the usable part of the photo, equal photos
/// by their distance from the border.
#[inline]
pub fn seam_score(q: f32, wt: f32) -> f32 {
    q * ((wt / EDGE_RAMP).min(1.0) + TIE_BREAK * wt)
}

/// Clip limits of the original's exposure mask (hugin.exe sub_1402d1120, advanced options
/// maskClipExposureLowerCutoff / UpperCutoff, f32 1/255 @0x1403a7b3c and 250/255 @0x1403a7b40).
pub const CLIP_LOWER: u8 = 1;
pub const CLIP_UPPER: u8 = 250;
/// Weight factor for clipped (blown-out or black) pixels where another photo can fill in.
pub const CLIP_WEIGHT: f32 = 1e-3;

impl RenderPhoto {
    /// Is the photo blown out (or completely black) at this spot? Works with the full photo
    /// or a smaller copy of it.
    #[inline]
    pub fn clipped_at(&self, x: f32, y: f32) -> bool {
        let sx = self.img.width() as f32 / self.w as f32;
        let sy = self.img.height() as f32 / self.h as f32;
        let xi = ((x + 0.5) * sx - 0.5).round().clamp(0.0, self.img.width() as f32 - 1.0) as u32;
        let yi = ((y + 0.5) * sy - 0.5).round().clamp(0.0, self.img.height() as f32 - 1.0) as u32;
        let p = self.img.get_pixel(xi, yi);
        let mx = p[0].max(p[1]).max(p[2]);
        mx > CLIP_UPPER || mx < CLIP_LOWER
    }

    /// Pixels per radian of the original photo.
    pub fn detail(&self) -> f64 {
        self.lens.f * self.full_long
    }

    /// world direction -> pixel position in `img`, plus centre weight.
    #[inline]
    pub fn lookup(&self, wdir: V3) -> Option<(f32, f32, f32)> {
        let c = mv(&self.rot, wdir);
        let p = project(&self.lens, c)?;
        let w = self.w;
        let h = self.h;
        let l = w.max(h);
        let mut x = p[0] * l + w * 0.5 - 0.5;
        let mut y = p[1] * l + h * 0.5 - 0.5;
        if let Some(f) = &self.field {
            let d = f.at((x + 0.5) / w, (y + 0.5) / h);
            x += d[0] * l;
            y += d[1] * l;
        }
        if x < -0.5 || y < -0.5 || x > w - 0.5 || y > h - 0.5 {
            return None;
        }
        let ex = 1.0 - (2.0 * (x + 0.5) / w - 1.0).abs();
        let ey = 1.0 - (2.0 * (y + 0.5) / h - 1.0).abs();
        let wgt = (ex * ey).max(1e-6) as f32;
        Some((x as f32, y as f32, wgt))
    }
}

/// Output pixel -> world direction mapper.
pub struct Mapper {
    pub proj: Projection,
    pub vt: M3,
    pub b: [f64; 4],
    pub w: usize,
    pub h: usize,
}

impl Mapper {
    pub fn new(view: &PanoView, bounds: [f64; 4], w: usize, h: usize) -> Self {
        Mapper { proj: view.projection, vt: transpose(&view.rotation()), b: bounds, w, h }
    }
    #[inline]
    pub fn dir(&self, px: usize, py: usize) -> Option<V3> {
        let x = self.b[0] + (px as f64 + 0.5) / self.w as f64 * (self.b[1] - self.b[0]);
        let y = self.b[2] + (py as f64 + 0.5) / self.h as f64 * (self.b[3] - self.b[2]);
        let d = self.proj.to_dir(x, y)?;
        Some(mv(&self.vt, d))
    }
    /// world dir -> output pixel (float)
    pub fn to_px(&self, wdir: V3, view_rot: &M3) -> Option<(f64, f64)> {
        let (x, y) = self.proj.from_dir(mv(view_rot, wdir))?;
        Some((
            (x - self.b[0]) / (self.b[1] - self.b[0]) * self.w as f64,
            (y - self.b[2]) / (self.b[3] - self.b[2]) * self.h as f64,
        ))
    }
}

/// Sample points along the border (and centre) of a photo, as world directions.
pub fn photo_outline(p: &RenderPhoto, steps: usize) -> Vec<V3> {
    let w = p.w;
    let h = p.h;
    let l = w.max(h);
    let mut pts = Vec::new();
    let to_dir = |x: f64, y: f64| -> V3 {
        let r = crate::align::ray(&p.lens, [(x - w * 0.5) / l, (y - h * 0.5) / l]);
        mtv(&p.rot, r)
    };
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        pts.push(to_dir(t * w, 0.0));
    }
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        pts.push(to_dir(w, t * h));
    }
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        pts.push(to_dir(w - t * w, h));
    }
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        pts.push(to_dir(0.0, h - t * h));
    }
    pts
}

/// Fit the view bounds around all photos.
pub fn fit_bounds(view: &mut PanoView, photos: &[RenderPhoto]) {
    let vr = view.rotation();
    let lim = view.projection.limits();
    let mut b = [f64::MAX, f64::MIN, f64::MAX, f64::MIN];
    let mut xs: Vec<f64> = Vec::new();
    for p in photos {
        let mut pts = photo_outline(p, 24);
        // also the centre and inner grid
        for gy in 1..4 {
            for gx in 1..4 {
                let w = p.w;
                let h = p.h;
                let l = w.max(h);
                let r = crate::align::ray(&p.lens, [(gx as f64 / 4.0 * w - w * 0.5) / l, (gy as f64 / 4.0 * h - h * 0.5) / l]);
                pts.push(mtv(&p.rot, r));
            }
        }
        for d in pts {
            match view.projection.from_dir(mv(&vr, d)) {
                Some((x, y)) => {
                    b[0] = b[0].min(x);
                    b[1] = b[1].max(x);
                    b[2] = b[2].min(y);
                    b[3] = b[3].max(y);
                    xs.push(x);
                }
                None => {
                    b = lim;
                }
            }
        }
    }
    if b[0] >= b[1] || b[2] >= b[3] {
        b = lim;
    }
    // wrap-around: photos crossing the back seam produce a full-width box
    let cyl = matches!(view.projection, Projection::Equirectangular | Projection::Cylindrical | Projection::Mercator);
    if cyl && b[1] - b[0] > 1.9 * PI {
        b[0] = -PI;
        b[1] = PI;
    }
    b[0] = b[0].max(lim[0]);
    b[1] = b[1].min(lim[1]);
    b[2] = b[2].max(lim[2]);
    b[3] = b[3].min(lim[3]);
    view.bounds = b;
    view.crop = [0.0, 1.0, 0.0, 1.0];
}

/// Fast preview: feathered blend. Returns RGBA bytes and coverage.
pub fn render_preview(photos: &[RenderPhoto], view: &PanoView, w: usize, h: usize, show_crop: bool, prefer_detail: bool) -> Vec<u8> {
    let bounds = view.bounds;
    let m = Mapper::new(view, bounds, w, h);
    let mut out = vec![0u8; w * h * 4];
    let cb = view.crop;
    // the more detailed photo counts more where photos overlap
    let qual = if prefer_detail { quality_factors(&photos.iter().map(|p| (p.detail(), p.trusted)).collect::<Vec<_>>()) } else { vec![1.0; photos.len()] };
    out.par_chunks_mut(w * 4).enumerate().for_each(|(py, row)| {
        for px in 0..w {
            let Some(d) = m.dir(px, py) else { continue };
            let mut acc = [0.0f32; 3];
            let mut ws = 0.0f32;
            for (pi, p) in photos.iter().enumerate() {
                if let Some((x, y, wt)) = p.lookup(d) {
                    let c = sample_rgb_linear(&p.img, x, y);
                    let wt = if p.clipped_at(x, y) { wt * wt * CLIP_WEIGHT } else { wt * wt } * qual[pi];
                    for k in 0..3 {
                        acc[k] += c[k] * p.gain[k] * wt;
                    }
                    ws += wt;
                }
            }
            if ws > 0.0 {
                let mut dim = 1.0;
                if show_crop {
                    let fx = (px as f64 + 0.5) / w as f64;
                    let fy = (py as f64 + 0.5) / h as f64;
                    if fx < cb[0] || fx > cb[1] || fy < cb[2] || fy > cb[3] {
                        dim = 0.35;
                    }
                }
                let o = px * 4;
                for k in 0..3 {
                    row[o + k] = (linear_to_srgb(acc[k] / ws * dim) * 255.0 + 0.5) as u8;
                }
                row[o + 3] = 255;
            }
        }
    });
    out
}

/// Low-resolution coverage mask (1 = some photo covers the pixel).
pub fn coverage(photos: &[RenderPhoto], view: &PanoView, w: usize, h: usize) -> Vec<bool> {
    let m = Mapper::new(view, view.bounds, w, h);
    (0..w * h)
        .into_par_iter()
        .map(|i| {
            let Some(d) = m.dir(i % w, i / w) else { return false };
            photos.iter().any(|p| p.lookup(d).is_some())
        })
        .collect()
}

/// Largest axis-aligned rectangle fully covered (fractions [l, r, t, b]).
pub fn auto_crop(photos: &[RenderPhoto], view: &PanoView) -> [f64; 4] {
    let aspect = view.aspect().clamp(0.05, 50.0);
    let w = 400usize;
    let h = ((w as f64 / aspect).round() as usize).clamp(8, 1600);
    let cov = coverage(photos, view, w, h);
    // maximal rectangle in a histogram per row
    let mut heights = vec![0usize; w];
    let mut best = (0usize, 0usize, 0usize, 0usize, 0usize); // area, x0, x1, y0, y1
    for y in 0..h {
        for x in 0..w {
            heights[x] = if cov[y * w + x] { heights[x] + 1 } else { 0 };
        }
        let mut stack: Vec<usize> = Vec::new();
        for x in 0..=w {
            let cur = if x < w { heights[x] } else { 0 };
            while let Some(&top) = stack.last() {
                if heights[top] <= cur {
                    break;
                }
                stack.pop();
                let height = heights[top];
                let left = stack.last().map(|v| v + 1).unwrap_or(0);
                let area = height * (x - left);
                if area > best.0 {
                    best = (area, left, x, y + 1 - height, y + 1);
                }
            }
            stack.push(x);
        }
    }
    if best.0 == 0 {
        return [0.0, 1.0, 0.0, 1.0];
    }
    let pad = 1.0;
    [
        ((best.1 as f64 + pad) / w as f64).min(1.0),
        ((best.2 as f64 - pad) / w as f64).max(0.0),
        ((best.3 as f64 + pad) / h as f64).min(1.0),
        ((best.4 as f64 - pad) / h as f64).max(0.0),
    ]
}

/// The clip limits as linear light values.
pub fn clip_limits_linear() -> (f32, f32) {
    let lut = crate::imgutil::srgb_to_linear_lut();
    (lut[CLIP_LOWER as usize], lut[CLIP_UPPER as usize])
}

/// Largest brightness change applied to one photo (factor 2 = 1 stop either way).
pub const MAX_GAIN: f32 = 2.0;
/// Overlap samples a photo pair needs before its brightness difference is trusted.
pub const MIN_PAIR_SAMPLES: usize = 40;

/// Brightness & colour gains so overlapping photos match.
///
/// For every overlapping photo pair the typical (median) brightness ratio is measured on
/// well-exposed pixels only (no clipped highlights, no deep shadows); then one gain per photo
/// is solved in log space so that all pair ratios fit best. Working in log space treats
/// "darker" and "brighter" alike, so no photo is pushed into over-exposure; gains are centred
/// on the connected photos' average (overall brightness stays) and limited to MAX_GAIN.
/// Photos without a measurable overlap keep their original brightness.
pub fn compute_gains(photos: &[RenderPhoto], view: &PanoView) -> Vec<[f32; 3]> {
    let n = photos.len();
    if n < 2 {
        return vec![[1.0; 3]; n];
    }
    let aspect = view.aspect().clamp(0.05, 50.0);
    let w = 480usize;
    let h = ((w as f64 / aspect).round() as usize).clamp(8, 1600);
    let m = Mapper::new(view, view.bounds, w, h);
    // per pixel samples of every covering photo (away from photo borders)
    let samples: Vec<Vec<(usize, [f32; 3])>> = (0..w * h)
        .into_par_iter()
        .map(|i| {
            let Some(d) = m.dir(i % w, i / w) else { return vec![] };
            let mut v = Vec::new();
            for (k, p) in photos.iter().enumerate() {
                if let Some((x, y, wt)) = p.lookup(d) {
                    if wt > 0.05 {
                        v.push((k, sample_rgb_linear(&p.img, x, y)));
                    }
                }
            }
            v
        })
        .collect();
    let mut ratios: std::collections::HashMap<(usize, usize), [Vec<f32>; 3]> = std::collections::HashMap::new();
    for s in &samples {
        for x in 0..s.len() {
            for y in x + 1..s.len() {
                let (i, ci) = s[x];
                let (j, cj) = s[y];
                let (i, j, ci, cj) = if i < j { (i, j, ci, cj) } else { (j, i, cj, ci) };
                let e = ratios.entry((i, j)).or_insert_with(|| [Vec::new(), Vec::new(), Vec::new()]);
                for ch in 0..3 {
                    let (a, b) = (ci[ch], cj[ch]);
                    // only values inside the original's clip limits (1/255 .. 250/255, as linear light)
                    let (lo, hi) = clip_limits_linear();
                    if a > lo && b > lo && a < hi && b < hi {
                        e[ch].push((a / b).ln());
                    }
                }
            }
        }
    }
    let mut connected = vec![false; n];
    let mut gains = vec![[1.0f32; 3]; n];
    for ch in 0..3 {
        let mut a = vec![vec![0.0f64; n]; n];
        let mut b = vec![0.0f64; n];
        let mut wsum = 0.0;
        for (&(i, j), r) in ratios.iter() {
            let v = &r[ch];
            if v.len() < MIN_PAIR_SAMPLES {
                continue;
            }
            let mut v = v.clone();
            v.sort_by(|x, y| x.partial_cmp(y).unwrap());
            let d = v[v.len() / 2] as f64; // ln(I_i / I_j)
            let wt = (v.len() as f64).sqrt();
            // want ln g_i + ln I_i = ln g_j + ln I_j  ->  x_i - x_j = -d
            a[i][i] += wt;
            a[j][j] += wt;
            a[i][j] -= wt;
            a[j][i] -= wt;
            b[i] -= wt * d;
            b[j] += wt * d;
            wsum += wt;
            connected[i] = true;
            connected[j] = true;
        }
        if wsum == 0.0 {
            continue;
        }
        // weak pull towards "no change" keeps the system solvable
        let lambda = 0.01 * wsum / n as f64;
        for i in 0..n {
            a[i][i] += lambda;
        }
        if let Some(x) = solve(a, b) {
            let conn: Vec<usize> = (0..n).filter(|&i| connected[i]).collect();
            let mean = conn.iter().map(|&i| x[i]).sum::<f64>() / conn.len().max(1) as f64;
            for &i in &conn {
                gains[i][ch] = ((x[i] - mean).exp() as f32).clamp(1.0 / MAX_GAIN, MAX_GAIN);
            }
        }
    }
    if std::env::var("PW_DEBUG").is_ok() {
        eprintln!("gain pairs used: {}", ratios.values().filter(|r| r[1].len() >= MIN_PAIR_SAMPLES).count());
    }
    gains
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Blend {
    Smooth,
    Feather,
}

/// Error text of a cancelled render (the GUI shows it as "canceled", not as a failure).
pub const CANCELLED: &str = "Cancelled";

pub struct Progress<'a> {
    pub report: &'a (dyn Fn(f32, &str) + Sync),
    pub cancel: &'a AtomicBool,
}

/// Final panorama render. `load(i)` delivers the full photo `i` (loaded lazily, one at a time).
/// Rough memory need of the final render in bytes (big buffers only).
pub fn estimate_bytes(w: usize, h: usize, blend: Blend) -> u64 {
    let px = (w as u64) * (h as u64);
    match blend {
        // seam map (6 B) + band pyramid (16 B x 1.34) + result (16 B) + 8-bit output (4 B) + row buffers
        Blend::Smooth => px * 50,
        // sums (16 B) + result/coverage (4 B) + 8-bit output (4 B)
        Blend::Feather => px * 26,
    }
}

/// A big buffer, or an error message instead of a crash when memory runs out.
pub fn alloc<T: Clone>(n: usize, v: T) -> Result<Vec<T>, String> {
    let mut x: Vec<T> = Vec::new();
    x.try_reserve_exact(n).map_err(|_| format!("not enough memory for the panorama ({:.1} GB needed for one buffer) – choose a smaller size", (n * std::mem::size_of::<T>()) as f64 / 1073741824.0))?;
    x.resize(n, v);
    Ok(x)
}

/// Final render options. `RenderOptions::default()` keeps the earlier behaviour
/// (automatic band count, bilinear sampling).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct RenderOptions {
    /// number of multi-band levels; 0 = automatic (as before)
    pub levels: usize,
    /// bicubic instead of bilinear sampling of the photos
    pub cubic: bool,
    /// use the most detailed photo where photos overlap (off = seams only by distance from
    /// the photo borders, as before)
    pub prefer_detail: bool,
}

pub fn render_final(
    count: usize,
    load: &(dyn Fn(usize, bool) -> Result<RenderPhoto, String> + Sync),
    view: &PanoView,
    out_w: usize,
    blend: Blend,
    ro: &RenderOptions,
    prog: &Progress,
) -> Result<(Vec<[f32; 3]>, Vec<f32>, usize, usize), String> {
    let sample = |img: &RgbImage, x: f32, y: f32| if ro.cubic { crate::imgutil::sample_rgb_linear_cubic(img, x, y) } else { sample_rgb_linear(img, x, y) };
    let cb = view.crop_bounds();
    let aspect = (cb[1] - cb[0]) / (cb[3] - cb[2]);
    let w = out_w.max(16);
    let h = ((w as f64 / aspect).round() as usize).max(16);
    let m = Mapper::new(view, cb, w, h);
    let vr = view.rotation();
    let cancelled = || prog.cancel.load(Ordering::Relaxed) || crate::engine::cancelled();

    // quality of every photo (real pixels per angle); needs only the geometry of each photo
    let mut details = Vec::with_capacity(count);
    for i in 0..count {
        let p = load(i, false)?;
        details.push((p.detail(), p.trusted));
    }
    let prefer = ro.prefer_detail;
    let qual = if prefer { quality_factors(&details) } else { vec![1.0; count] };
    // precompute directions once (row by row on the fly is cheaper in memory, so we recompute)
    match blend {
        Blend::Feather => {
            let mut acc = alloc(w * h, [0.0f32; 3])?;
            let mut ws = alloc(w * h, 0.0f32)?;
            for i in 0..count {
                if cancelled() {
                    return Err(CANCELLED.into());
                }
                (prog.report)(i as f32 / count as f32, &format!("Blending photo {} of {}", i + 1, count));
                let p = load(i, true)?;
                let (y0, y1) = roi_rows(&p, &m, &vr, h);
                acc.par_chunks_mut(w).zip(ws.par_chunks_mut(w)).enumerate().for_each(|(py, (ar, wr))| {
                    if py < y0 || py >= y1 {
                        return;
                    }
                    for px in 0..w {
                        let Some(d) = m.dir(px, py) else { continue };
                        if let Some((x, y, wt)) = p.lookup(d) {
                            let c = sample(&p.img, x, y);
                            let wt = if p.clipped_at(x, y) { wt * wt * CLIP_WEIGHT } else { wt * wt } * qual[i];
                            for k in 0..3 {
                                ar[px][k] += c[k] * p.gain[k] * wt;
                            }
                            wr[px] += wt;
                        }
                    }
                });
            }
            acc.par_iter_mut().zip(ws.par_iter_mut()).for_each(|(a, wv)| {
                if *wv > 0.0 {
                    for k in 0..3 {
                        a[k] /= *wv;
                    }
                    *wv = 1.0;
                }
            });
            Ok((acc, ws, w, h))
        }
        Blend::Smooth => {
            // pass 1: which photo is best at each pixel (seams follow the middle of overlaps)
            let mut best = alloc(w * h, u16::MAX)?;
            let mut best_w = alloc(w * h, 0.0f32)?;
            let mut photos_dims = Vec::new();
            for i in 0..count {
                if cancelled() {
                    return Err(CANCELLED.into());
                }
                (prog.report)(0.25 * i as f32 / count as f32, &format!("Finding seams ({} of {})", i + 1, count));
                let p = load(i, false)?;
                photos_dims.push(roi_rows(&p, &m, &vr, h));
                let (y0, y1) = photos_dims[i];
                best.par_chunks_mut(w).zip(best_w.par_chunks_mut(w)).enumerate().for_each(|(py, (br, bw))| {
                    if py < y0 || py >= y1 {
                        return;
                    }
                    for px in 0..w {
                        let Some(d) = m.dir(px, py) else { continue };
                        if let Some((x, y, wt)) = p.lookup(d) {
                            // the photo with the most detail wins (see `seam_score`)
                            let sc = if prefer { seam_score(qual[i], wt) } else { wt };
                            let wt = if p.clipped_at(x, y) { sc * CLIP_WEIGHT } else { sc };
                            if wt > bw[px] {
                                bw[px] = wt;
                                br[px] = i as u16;
                            }
                        }
                    }
                });
            }
            drop(best_w);
            let levels = if ro.levels > 0 { ro.levels.min(auto_levels(w, h)) } else { auto_levels(w, h) };
            let mut pyr = BlendPyramid::new(w, h, levels)?;
            for i in 0..count {
                if cancelled() {
                    return Err(CANCELLED.into());
                }
                (prog.report)(0.25 + 0.7 * i as f32 / count as f32, &format!("Blending photo {} of {}", i + 1, count));
                let p = load(i, true)?;
                let (y0, y1) = photos_dims[i];
                if y0 >= y1 {
                    continue;
                }
                // region of interest, padded and aligned to the coarsest level
                let align = 1usize << levels;
                let pad = align * 2;
                let ry0 = (y0.saturating_sub(pad) / align) * align;
                let ry1 = ((y1 + pad).div_ceil(align) * align).min(h.div_ceil(align) * align);
                let rh = ry1 - ry0;
                let rw = w.div_ceil(align) * align;
                let mut img = vec![[0.0f32; 3]; rw * rh];
                let mut valid = vec![0.0f32; rw * rh];
                let mut mask = vec![0.0f32; rw * rh];
                img.par_chunks_mut(rw)
                    .zip(valid.par_chunks_mut(rw))
                    .zip(mask.par_chunks_mut(rw))
                    .enumerate()
                    .for_each(|(ry, ((ir, vr_), mr))| {
                        let py = ry0 + ry;
                        if py >= h {
                            return;
                        }
                        for px in 0..w {
                            let Some(d) = m.dir(px, py) else { continue };
                            if let Some((x, y, _)) = p.lookup(d) {
                                let c = sample(&p.img, x, y);
                                ir[px] = [c[0] * p.gain[0], c[1] * p.gain[1], c[2] * p.gain[2]];
                                vr_[px] = 1.0;
                                if best[py * w + px] == i as u16 {
                                    mr[px] = 1.0;
                                }
                            }
                        }
                    });
                fill_holes(&mut img, &valid, rw, rh, usize::MAX);
                pyr.add(&img, &mask, rw, rh, ry0);
            }
            (prog.report)(0.96, "Finishing");
            let (col, wgt) = pyr.collapse();
            let mut cov = vec![0.0f32; w * h];
            cov.par_iter_mut().enumerate().for_each(|(i, c)| {
                if best[i] != u16::MAX {
                    *c = 1.0;
                }
            });
            drop(wgt);
            Ok((col, cov, w, h))
        }
    }
}

/// Receives one finished strip of the panorama: first row, colour and coverage of the inner rows
/// (`col.len() / width` rows) and the panorama width.
#[allow(dead_code)] // wired into the export in a later step
pub type StripSink<'a> = &'a mut dyn FnMut(usize, &[[f32; 3]], &[f32], usize) -> Result<(), String>;

/// Memory-saving final render (Smooth blend): the panorama is made strip by strip and every strip
/// goes to `sink`, so only one strip (plus a margin for the multi-band blend) is in memory.
/// Returns the panorama size. The strip height follows from `budget` (see `strip::plan`) and is
/// a multiple of the coarsest pyramid level so that every strip sees the same pyramid grid as
/// the whole image would. `render_final` stays the reference.
#[allow(dead_code)] // wired into the export in a later step
pub fn render_strips(
    count: usize,
    load: &(dyn Fn(usize, bool) -> Result<RenderPhoto, String> + Sync),
    view: &PanoView,
    out_w: usize,
    ro: &RenderOptions,
    cfg: &crate::strip::StripConfig,
    budget: u64,
    sink: StripSink,
    prog: &Progress,
) -> Result<(usize, usize), String> {
    let cancelled = || prog.cancel.load(Ordering::Relaxed) || crate::engine::cancelled();
    let cb = view.crop_bounds();
    let aspect = (cb[1] - cb[0]) / (cb[3] - cb[2]);
    let w = out_w.max(16);
    let h = ((w as f64 / aspect).round() as usize).max(16);
    let m = Mapper::new(view, cb, w, h);
    let vr = view.rotation();
    // per photo: quality and the output rows it touches (geometry only, cheap)
    let mut details = Vec::with_capacity(count);
    let mut photo_rows = Vec::with_capacity(count);
    for i in 0..count {
        let p = load(i, false)?;
        details.push((p.detail(), p.trusted));
        photo_rows.push(roi_rows(&p, &m, &vr, h));
    }
    let qual = if ro.prefer_detail { quality_factors(&details) } else { vec![1.0; count] };
    let levels = if ro.levels > 0 { ro.levels.min(auto_levels(w, h)) } else { auto_levels(w, h) };
    let plan = crate::strip::plan(w, h, levels, Blend::Smooth, budget, cfg)?;
    // strips start on the grid of the coarsest hole-fill level, so the fill is the same in every strip
    let align = 1usize << (levels + cfg.fill_extra_levels);
    let rows = if plan.rows >= h { h } else { (plan.rows / align).max(1) * align };
    let margin = plan.margin.div_ceil(align) * align;
    let strips = crate::strip::strips(h, rows);
    let n = strips.len();
    for (si, &(s0, s1)) in strips.iter().enumerate() {
        if cancelled() {
            return Err(CANCELLED.into());
        }
        (prog.report)(si as f32 / n as f32, &format!("Strip {} of {}", si + 1, n));
        let (ya, yb) = crate::strip::padded((s0, s1), margin, h);
        let (col, cov) = smooth_window(load, &m, &vr, &qual, &photo_rows, w, (ya, yb), levels, cfg.fill_extra_levels, ro, &cancelled)?;
        let a = (s0 - ya) * w;
        let b = (s1 - ya) * w;
        sink(s0, &col[a..b], &cov[a..b], w)?;
    }
    Ok((w, h))
}

/// Smooth blend of the output rows `[ya, yb)` only (`ya` must be a multiple of 2^(levels + fill_extra)). Same
/// two passes as in `render_final`, with all buffers just as high as the window.
#[allow(dead_code)] // wired into the export in a later step
#[allow(clippy::too_many_arguments)]
fn smooth_window(
    load: &(dyn Fn(usize, bool) -> Result<RenderPhoto, String> + Sync),
    m: &Mapper,
    _vr: &M3,
    qual: &[f32],
    photo_rows: &[(usize, usize)],
    w: usize,
    (ya, yb): (usize, usize),
    levels: usize,
    fill_extra: usize,
    ro: &RenderOptions,
    cancelled: &dyn Fn() -> bool,
) -> Result<(Vec<[f32; 3]>, Vec<f32>), String> {
    let sample = |img: &RgbImage, x: f32, y: f32| if ro.cubic { crate::imgutil::sample_rgb_linear_cubic(img, x, y) } else { sample_rgb_linear(img, x, y) };
    let wh = yb - ya;
    let prefer = ro.prefer_detail;
    // photos touching the window, with their rows relative to the window
    let touching = crate::strip::photos_for(photo_rows, (ya, yb));
    let rel = |i: usize| (photo_rows[i].0.max(ya) - ya, photo_rows[i].1.min(yb) - ya);
    let mut best = alloc(w * wh, u16::MAX)?;
    let mut best_w = alloc(w * wh, 0.0f32)?;
    for &i in &touching {
        if cancelled() {
            return Err(CANCELLED.into());
        }
        let p = load(i, false)?;
        let (y0, y1) = rel(i);
        best.par_chunks_mut(w).zip(best_w.par_chunks_mut(w)).enumerate().for_each(|(ry, (br, bw))| {
            if ry < y0 || ry >= y1 {
                return;
            }
            let py = ya + ry;
            for px in 0..w {
                let Some(d) = m.dir(px, py) else { continue };
                if let Some((x, y, wt)) = p.lookup(d) {
                    let sc = if prefer { seam_score(qual[i], wt) } else { wt };
                    let wt = if p.clipped_at(x, y) { sc * CLIP_WEIGHT } else { sc };
                    if wt > bw[px] {
                        bw[px] = wt;
                        br[px] = i as u16;
                    }
                }
            }
        });
    }
    drop(best_w);
    let mut pyr = BlendPyramid::new(w, wh, levels)?;
    // regions start on the grid of the coarsest fill level (`ya` is on it too)
    let align = 1usize << (levels + fill_extra);
    let pad = align * 2;
    for &i in &touching {
        if cancelled() {
            return Err(CANCELLED.into());
        }
        let p = load(i, true)?;
        let (y0, y1) = rel(i);
        if y0 >= y1 {
            continue;
        }
        let ry0 = (y0.saturating_sub(pad) / align) * align;
        let ry1 = ((y1 + pad).div_ceil(align) * align).min(wh.div_ceil(align) * align);
        let rh = ry1 - ry0;
        let rw = w.div_ceil(align) * align;
        let mut img = vec![[0.0f32; 3]; rw * rh];
        let mut valid = vec![0.0f32; rw * rh];
        let mut mask = vec![0.0f32; rw * rh];
        img.par_chunks_mut(rw).zip(valid.par_chunks_mut(rw)).zip(mask.par_chunks_mut(rw)).enumerate().for_each(|(ry, ((ir, vr_), mr))| {
            let wy = ry0 + ry;
            if wy >= wh {
                return;
            }
            let py = ya + wy;
            for px in 0..w {
                let Some(d) = m.dir(px, py) else { continue };
                if let Some((x, y, _)) = p.lookup(d) {
                    let c = sample(&p.img, x, y);
                    ir[px] = [c[0] * p.gain[0], c[1] * p.gain[1], c[2] * p.gain[2]];
                    vr_[px] = 1.0;
                    if best[wy * w + px] == i as u16 {
                        mr[px] = 1.0;
                    }
                }
            }
        });
        fill_holes(&mut img, &valid, rw, rh, levels + fill_extra);
        pyr.add(&img, &mask, rw, rh, ry0);
    }
    let (col, wgt) = pyr.collapse();
    drop(wgt);
    let mut cov = vec![0.0f32; w * wh];
    cov.par_iter_mut().enumerate().for_each(|(i, c)| {
        if best[i] != u16::MAX {
            *c = 1.0;
        }
    });
    Ok((col, cov))
}

/// Automatic number of multi-band levels for a panorama of `w` x `h` pixels (1..=7).
pub fn auto_levels(w: usize, h: usize) -> usize {
    let mut l = 1;
    while l < 7 && (w.min(h) >> (l + 1)) > 8 {
        l += 1;
    }
    l
}

/// Rows of the output touched by photo `p`.
fn roi_rows(p: &RenderPhoto, m: &Mapper, vr: &M3, h: usize) -> (usize, usize) {
    let mut y0 = f64::MAX;
    let mut y1 = f64::MIN;
    let pts = photo_outline(p, 32);
    let mut bad = false;
    for d in pts {
        match m.to_px(d, vr) {
            Some((_, y)) => {
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
            None => bad = true,
        }
    }
    if bad || y0 > y1 {
        return (0, h);
    }
    let mut y0 = (y0 - 4.0).floor().max(0.0) as usize;
    let mut y1 = ((y1 + 4.0).ceil().max(0.0) as usize).min(h);
    // a photo containing a pole of the view extends to the top/bottom
    if p.lookup(mtv(vr, [0.0, -1.0, 0.0])).is_some() {
        y0 = 0;
    }
    if p.lookup(mtv(vr, [0.0, 1.0, 0.0])).is_some() {
        y1 = h;
    }
    (y0, y1.max(y0))
}

/// Fill invalid pixels with smooth values from coarser levels (push-pull), so multi-band
/// blending does not pull in black at the seams.
fn fill_holes(img: &mut [[f32; 3]], valid: &[f32], w: usize, h: usize, max_levels: usize) {
    if w < 2 || h < 2 {
        return;
    }
    let mut levels: Vec<(Vec<[f32; 4]>, usize, usize)> = Vec::new();
    let base: Vec<[f32; 4]> = img
        .iter()
        .zip(valid)
        .map(|(c, v)| [c[0] * v, c[1] * v, c[2] * v, *v])
        .collect();
    levels.push((base, w, h));
    while levels.last().unwrap().1 > 1 && levels.last().unwrap().2 > 1 && levels.len() <= max_levels {
        let (prev, pw, ph) = levels.last().unwrap();
        let (nw, nh) = (pw.div_ceil(2), ph.div_ceil(2));
        let mut next = vec![[0.0f32; 4]; nw * nh];
        for y in 0..nh {
            for x in 0..nw {
                let mut s = [0.0f32; 4];
                for dy in 0..2 {
                    for dx in 0..2 {
                        let xx = (2 * x + dx).min(pw - 1);
                        let yy = (2 * y + dy).min(ph - 1);
                        let v = prev[yy * pw + xx];
                        for k in 0..4 {
                            s[k] += v[k];
                        }
                    }
                }
                next[y * nw + x] = s;
            }
        }
        levels.push((next, nw, nh));
        if nw * nh <= 1 {
            break;
        }
    }
    // pull: from coarse to fine, fill pixels without weight
    for l in (0..levels.len() - 1).rev() {
        let (coarse, cw, ch) = {
            let (c, cw, ch) = &levels[l + 1];
            (c.clone(), *cw, *ch)
        };
        let (fine, fw, fh) = &mut levels[l];
        for y in 0..*fh {
            for x in 0..*fw {
                let i = y * *fw + x;
                if fine[i][3] < 1e-6 {
                    let c = coarse[(y / 2).min(ch - 1) * cw + (x / 2).min(cw - 1)];
                    if c[3] > 1e-9 {
                        fine[i] = [c[0] / c[3], c[1] / c[3], c[2] / c[3], 1.0];
                    }
                } else if fine[i][3] != 1.0 {
                    let wv = fine[i][3];
                    fine[i] = [fine[i][0] / wv, fine[i][1] / wv, fine[i][2] / wv, 1.0];
                }
            }
        }
        let _ = fh;
    }
    let (base, _, _) = &levels[0];
    for (i, c) in img.iter_mut().enumerate() {
        if valid[i] < 0.5 {
            *c = [base[i][0], base[i][1], base[i][2]];
        }
    }
}

// ---------- Laplacian pyramid blending ----------

fn down2(src: &[[f32; 4]], w: usize, h: usize) -> (Vec<[f32; 4]>, usize, usize) {
    let nw = w.div_ceil(2);
    let nh = h.div_ceil(2);
    let k = [1.0f32, 4.0, 6.0, 4.0, 1.0];
    // horizontal
    let mut tmp = vec![[0.0f32; 4]; nw * h];
    tmp.par_chunks_mut(nw).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let mut s = [0.0f32; 4];
            for (t, kv) in k.iter().enumerate() {
                let xx = (2 * x as isize + t as isize - 2).clamp(0, w as isize - 1) as usize;
                let v = src[y * w + xx];
                for c in 0..4 {
                    s[c] += kv * v[c];
                }
            }
            for c in 0..4 {
                o[c] = s[c] / 16.0;
            }
        }
    });
    let mut out = vec![[0.0f32; 4]; nw * nh];
    out.par_chunks_mut(nw).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let mut s = [0.0f32; 4];
            for (t, kv) in k.iter().enumerate() {
                let yy = (2 * y as isize + t as isize - 2).clamp(0, h as isize - 1) as usize;
                let v = tmp[yy * nw + x];
                for c in 0..4 {
                    s[c] += kv * v[c];
                }
            }
            for c in 0..4 {
                o[c] = s[c] / 16.0;
            }
        }
    });
    (out, nw, nh)
}

fn up2(src: &[[f32; 4]], sw: usize, sh: usize, w: usize, h: usize) -> Vec<[f32; 4]> {
    let mut out = vec![[0.0f32; 4]; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let fy = ((y as f32 + 0.5) / 2.0 - 0.5).max(0.0);
        let y0 = (fy.floor() as usize).min(sh - 1);
        let y1 = (y0 + 1).min(sh - 1);
        let ty = fy - y0 as f32;
        for (x, o) in row.iter_mut().enumerate() {
            let fx = ((x as f32 + 0.5) / 2.0 - 0.5).max(0.0);
            let x0 = (fx.floor() as usize).min(sw - 1);
            let x1 = (x0 + 1).min(sw - 1);
            let tx = fx - x0 as f32;
            let a = src[y0 * sw + x0];
            let b = src[y0 * sw + x1];
            let c = src[y1 * sw + x0];
            let d = src[y1 * sw + x1];
            for k in 0..4 {
                o[k] = (a[k] * (1.0 - tx) + b[k] * tx) * (1.0 - ty) + (c[k] * (1.0 - tx) + d[k] * tx) * ty;
            }
        }
    });
    out
}

/// Gaussian & Laplacian pyramid of [r, g, b, mask] where the colour part is the Laplacian of the
/// image weighted by the Gaussian of the mask.
fn band_pyramid(img: &[[f32; 3]], mask: &[f32], w: usize, h: usize, levels: usize) -> Vec<(Vec<[f32; 4]>, usize, usize)> {
    let base: Vec<[f32; 4]> = img.iter().zip(mask).map(|(c, m)| [c[0], c[1], c[2], *m]).collect();
    let mut gauss = vec![(base, w, h)];
    for _ in 0..levels {
        let (p, pw, ph) = gauss.last().unwrap();
        let next = down2(p, *pw, *ph);
        gauss.push(next);
    }
    let mut out = Vec::with_capacity(levels + 1);
    for l in 0..=levels {
        let (g, gw, gh) = &gauss[l];
        let lap: Vec<[f32; 4]> = if l < levels {
            let (n, nw, nh) = &gauss[l + 1];
            let up = up2(n, *nw, *nh, *gw, *gh);
            g.iter()
                .zip(&up)
                .map(|(a, b)| {
                    let m = a[3];
                    [(a[0] - b[0]) * m, (a[1] - b[1]) * m, (a[2] - b[2]) * m, m]
                })
                .collect()
        } else {
            g.iter().map(|a| [a[0] * a[3], a[1] * a[3], a[2] * a[3], a[3]]).collect()
        };
        out.push((lap, *gw, *gh));
    }
    out
}

pub struct BlendPyramid {
    levels: usize,
    bands: Vec<(Vec<[f32; 4]>, usize, usize)>,
    w: usize,
    h: usize,
}

impl BlendPyramid {
    pub fn new(w: usize, h: usize, levels: usize) -> Result<Self, String> {
        let mut bands = Vec::new();
        let (mut lw, mut lh) = (w, h);
        for _ in 0..=levels {
            bands.push((alloc(lw * lh, [0.0f32; 4])?, lw, lh));
            lw = lw.div_ceil(2);
            lh = lh.div_ceil(2);
        }
        Ok(BlendPyramid { levels, bands, w, h })
    }
    /// Add a region (full width rw >= w, rows starting at y0, aligned to 2^levels).
    pub fn add(&mut self, img: &[[f32; 3]], mask: &[f32], rw: usize, rh: usize, y0: usize) {
        let p = band_pyramid(img, mask, rw, rh, self.levels);
        for (l, (band, bw, bh)) in p.into_iter().enumerate() {
            let (dst, dw, dh) = &mut self.bands[l];
            let oy = y0 >> l;
            let dw = *dw;
            let dh = *dh;
            dst.par_chunks_mut(dw).enumerate().for_each(|(y, row)| {
                if y < oy || y >= oy + bh {
                    return;
                }
                let sy = y - oy;
                for x in 0..dw.min(bw) {
                    let s = band[sy * bw + x];
                    for k in 0..4 {
                        row[x][k] += s[k];
                    }
                }
            });
            let _ = dh;
        }
    }
    pub fn collapse(self) -> (Vec<[f32; 3]>, Vec<f32>) {
        let mut bands = self.bands;
        // normalise each band by its mask weight
        for (b, _, _) in bands.iter_mut() {
            b.par_iter_mut().for_each(|v| {
                if v[3] > 1e-6 {
                    let m = v[3];
                    v[0] /= m;
                    v[1] /= m;
                    v[2] /= m;
                } else {
                    v[0] = 0.0;
                    v[1] = 0.0;
                    v[2] = 0.0;
                }
            });
        }
        let mut cur = bands.pop().unwrap();
        while let Some((b, bw, bh)) = bands.pop() {
            let up = up2(&cur.0, cur.1, cur.2, bw, bh);
            let merged: Vec<[f32; 4]> = b.iter().zip(&up).map(|(l, u)| [l[0] + u[0], l[1] + u[1], l[2] + u[2], l[3]]).collect();
            cur = (merged, bw, bh);
        }
        let (data, _, _) = cur;
        let col = data.iter().map(|v| [v[0].max(0.0), v[1].max(0.0), v[2].max(0.0)]).collect();
        let wgt = data.iter().map(|v| v[3]).collect();
        let _ = (self.w, self.h);
        (col, wgt)
    }
}

/// Convert linear float output to 8-bit sRGB RGBA.
pub fn to_rgba8(col: &[[f32; 3]], cov: &[f32]) -> Vec<u8> {
    let mut out = vec![0u8; col.len() * 4];
    out.par_chunks_mut(4).enumerate().for_each(|(i, o)| {
        if cov[i] > 0.0 {
            for k in 0..3 {
                o[k] = (linear_to_srgb(col[i][k]) * 255.0 + 0.5) as u8;
            }
            o[3] = 255;
        }
    });
    out
}

/// Convert linear float output to 16-bit sRGB RGBA (for 16-bit TIFF).
pub fn to_rgba16(col: &[[f32; 3]], cov: &[f32]) -> Vec<u16> {
    let mut out = vec![0u16; col.len() * 4];
    out.par_chunks_mut(4).enumerate().for_each(|(i, o)| {
        if cov[i] > 0.0 {
            for k in 0..3 {
                o[k] = (linear_to_srgb(col[i][k]).clamp(0.0, 1.0) * 65535.0 + 0.5) as u16;
            }
            o[3] = u16::MAX;
        }
    });
    out
}

#[cfg(test)]
mod quality_tests {
    use super::*;

    #[test]
    fn more_detailed_photo_wins_the_seam() {
        // wide photo and a zoomed-in one (1.76 x the detail, like the example photos)
        let q = quality_factors(&[(3860.0, true), (6810.0, true), (6810.0, true)]);
        assert!(q[0] < q[1] && (q[1] - 1.0).abs() < 1e-6 && (q[1] - q[2]).abs() < 1e-9);
        // inside the zoomed photo the zoomed photo wins even though the wide photo is more central
        assert!(seam_score(q[1], 0.30) > seam_score(q[0], 1.0));
        // right at the border of the zoomed photo the wide one takes over
        assert!(seam_score(q[1], 0.002) < seam_score(q[0], 1.0));
        // equal photos: the one further from its border wins, as before
        assert!(seam_score(q[1], 0.8) > seam_score(q[2], 0.4));
        // nearly equal detail (< 20 %) counts as equal
        let e = quality_factors(&[(1000.0, true), (1100.0, true)]);
        assert!((e[0] - e[1]).abs() < 1e-9);
        // a photo without connections only fills gaps, however detailed it is
        let u = quality_factors(&[(3860.0, true), (9000.0, false)]);
        assert!(seam_score(u[0], 0.05) > seam_score(u[1], 1.0));
    }
}

#[cfg(test)]
mod rgba16_tests {
    use super::*;

    #[test]
    fn sixteen_bit_matches_eight_bit_and_transparent_outside() {
        let col = [[0.0f32, 0.5, 1.0], [0.2, 0.2, 0.2]];
        let cov = [1.0f32, 0.0];
        let (a, b) = (to_rgba8(&col, &cov), to_rgba16(&col, &cov));
        for k in 0..3 {
            assert!((b[k] as f32 / 257.0 - a[k] as f32).abs() <= 1.0);
        }
        assert_eq!((a[3], b[3]), (255, 65535));
        assert!(b[4..].iter().all(|&v| v == 0));
        // 16 bit really has more levels
        assert_ne!(b[1] % 257, 0);
    }
}

#[cfg(test)]
mod strip_tests {
    use super::*;
    use crate::align::Lens;
    use crate::geometry::camera_from_ypr;
    use crate::strip::StripConfig;

    fn scene(n: usize, detail_boost: bool) -> Vec<RenderPhoto> {
        (0..n)
            .map(|i| {
                let img = crate::testutil::texture(300, 200, 7 + i as u64);
                RenderPhoto {
                    w: 300.0,
                    h: 200.0,
                    rot: camera_from_ypr((i as f64 * 22.0 - 22.0).to_radians(), (i as f64 * 3.0 - 3.0).to_radians(), 0.0),
                    lens: Lens::from_hfov_deg(50.0),
                    field: None,
                    gain: [1.0 + 0.05 * i as f32, 1.0, 1.0 - 0.03 * i as f32],
                    full_long: if detail_boost && i == 1 { 600.0 } else { 300.0 },
                    trusted: true,
                    img: Arc::new(img),
                }
            })
            .collect()
    }

    struct Diff {
        max: i32,
        differing: usize,
        total: usize,
        mean: f64,
    }

    fn compare(a: &[u8], b: &[u8]) -> Diff {
        assert_eq!(a.len(), b.len());
        let mut d = Diff { max: 0, differing: 0, total: a.len() / 4, mean: 0.0 };
        let mut sum = 0.0;
        for (x, y) in a.chunks(4).zip(b.chunks(4)) {
            let m = (0..4).map(|k| (x[k] as i32 - y[k] as i32).abs()).max().unwrap();
            d.max = d.max.max(m);
            sum += m as f64;
            if m > 0 {
                d.differing += 1;
            }
        }
        d.mean = sum / d.total as f64;
        d
    }

    /// Renders with strips of at most `strip_rows` rows. Returns (8-bit result, number of strips).
    fn strips_of(photos: &[RenderPhoto], view: &PanoView, width: usize, ro: &RenderOptions, cfg: &StripConfig) -> (Vec<u8>, usize, usize, usize) {
        let load = |i: usize, _px: bool| Ok(photos[i].clone());
        let cancel = AtomicBool::new(false);
        let prog = Progress { report: &|_, _| {}, cancel: &cancel };
        let mut out: Vec<u8> = Vec::new();
        let mut strips = 0;
        let mut sink = |y0: usize, c: &[[f32; 3]], v: &[f32], sw: usize| {
            let px = to_rgba8(c, v);
            assert_eq!(out.len(), y0 * sw * 4, "strips must arrive in order without gaps");
            out.extend_from_slice(&px);
            strips += 1;
            Ok(())
        };
        let (w, h) = render_strips(photos.len(), &load, view, width, ro, cfg, 8 << 30, &mut sink, &prog).unwrap();
        assert_eq!(out.len(), w * h * 4);
        (out, strips, w, h)
    }

    #[test]
    fn strips_equal_one_window_and_stay_close_to_the_whole_image_render() {
        let photos = scene(3, true);
        let mut view = PanoView::default();
        fit_bounds(&mut view, &photos);
        let load = |i: usize, _px: bool| Ok(photos[i].clone());
        let cancel = AtomicBool::new(false);
        let prog = Progress { report: &|_, _| {}, cancel: &cancel };
        for (rows, levels) in [(64, 4), (128, 6)] {
            let ro = RenderOptions { prefer_detail: true, levels, ..Default::default() };
            let (col, cov, w, h) = render_final(photos.len(), &load, &view, 2000, Blend::Smooth, &ro, &prog).unwrap();
            let reference = to_rgba8(&col, &cov);
            // one window = the whole image in a single strip
            let one = StripConfig { max_rows: 1 << 20, ..Default::default() };
            let (whole, n, _, _) = strips_of(&photos, &view, 2000, &ro, &one);
            assert_eq!(n, 1);
            // several strips must give the very same pixels as the single window
            let cfg = StripConfig { max_rows: rows, min_rows: 16, ..Default::default() };
            let (strips, n, w2, h2) = strips_of(&photos, &view, 2000, &ro, &cfg);
            assert_eq!((w, h), (w2, h2));
            assert!(n >= 4, "expected several strips, got {n}");
            let d = compare(&whole, &strips);
            assert!(d.max <= 1, "levels {levels}: strips differ from one window: max {} in {}/{} px", d.max, d.differing, d.total);
            // the earlier render fills holes over the whole photo, the strips over a local grid:
            // not identical, but visually the same
            let dr = compare(&reference, &strips);
            eprintln!("levels {levels}: {n} strips; vs render_final max {} mean {:.3} differing {}/{}", dr.max, dr.mean, dr.differing, dr.total);
            assert!(dr.mean < 1.5, "levels {levels}: mean difference {:.3}", dr.mean);
        }
    }

    #[test]
    fn cancelling_stops_the_strip_render() {
        let photos = scene(2, false);
        let mut view = PanoView::default();
        fit_bounds(&mut view, &photos);
        let load = |i: usize, _px: bool| Ok(photos[i].clone());
        let cancel = AtomicBool::new(true);
        let prog = Progress { report: &|_, _| {}, cancel: &cancel };
        let mut sink = |_: usize, _: &[[f32; 3]], _: &[f32], _: usize| Ok(());
        let e = render_strips(2, &load, &view, 400, &RenderOptions::default(), &StripConfig::default(), 8 << 30, &mut sink, &prog).unwrap_err();
        assert_eq!(e, CANCELLED);
    }
}
