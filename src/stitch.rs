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
    pub fn to_dir(self, x: f64, y: f64) -> Option<V3> {
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
                let th = if self == Projection::Stereographic { 2.0 * (r / 2.0).atan() } else { r };
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
    #[allow(clippy::wrong_self_convention)] // the projection itself is `self`; the name pairs with `to_dir`
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
    /// smooth brightness correction across the photo on top of `gain` (None: `gain` only)
    pub gain_map: Option<Arc<crate::gainmap::GainMap>>,
    /// response curve and vignetting of the photo's lens, applied before `gain` (None: nothing)
    pub photometry: Option<crate::lensphoto::LensModel>,
    /// long side of the ORIGINAL photo in pixels (its real resolution; `w`/`h` may describe a
    /// smaller working copy)
    pub full_long: f64,
    /// joined to the main group of photos by control points; a photo without any connection
    /// may sit at a wrong place, so it is only used where no connected photo covers the spot
    pub trusted: bool,
    /// what the person painted on this photo to steer the seams (None: nothing)
    pub mask: Option<Arc<crate::masks::MaskBuf>>,
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

/// Clip limits of the original's exposure mask: 1/255 (lower) and 250/255 (upper).
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
        !(CLIP_LOWER..=CLIP_UPPER).contains(&mx)
    }

    /// What the person painted at this spot (`masks::NEUTRAL`, `POSITIVE`, `NEGATIVE`). Works with the full photo
    /// or a smaller copy of it, like `clipped_at`.
    #[inline]
    pub fn mask_at(&self, x: f32, y: f32) -> u8 {
        match &self.mask {
            Some(m) => m.at(x, y, self.w, self.h),
            None => crate::masks::NEUTRAL,
        }
    }

    /// Brightness factor at the photo position (`x`, `y`) (pixels like `lookup`): `gain`, times
    /// the correction map when there is one. Works with the full photo or a smaller copy.
    #[inline]
    pub fn gain_at(&self, x: f32, y: f32) -> [f32; 3] {
        match &self.gain_map {
            None => self.gain,
            Some(m) => {
                let c = m.at((x + 0.5) / self.w as f32, (y + 0.5) / self.h as f32);
                [self.gain[0] * c[0], self.gain[1] * c[1], self.gain[2] * c[2]]
            }
        }
    }

    /// The sampled linear colour `c` at the photo position (`x`, `y`) with the lens model applied
    /// (unchanged without one). Positions as in `gain_at`.
    #[inline]
    pub fn photometry_at(&self, c: [f32; 3], x: f32, y: f32) -> [f32; 3] {
        match &self.photometry {
            None => c,
            Some(m) => m.apply(c, x, y, self.w as f32, self.h as f32),
        }
    }

    /// The sampled linear colour `c` at the photo position as it goes into the panorama: lens
    /// model, then `gain_at`. Without a lens model exactly `c * gain_at(x, y)`.
    #[inline]
    pub fn shade(&self, c: [f32; 3], x: f32, y: f32) -> [f32; 3] {
        let c = self.photometry_at(c, x, y);
        let g = self.gain_at(x, y);
        [c[0] * g[0], c[1] * g[1], c[2] * g[2]]
    }

    /// Pixels per radian of the original photo.
    pub fn detail(&self) -> f64 {
        self.lens.f * self.full_long
    }

    /// Like `lookup`, but `None` where the person painted "do not use this photo here": such a pixel
    /// does not exist for this photo, so nothing of it reaches the picture – not even blurred through
    /// the blending. Where every photo is masked the panorama has a hole (see `mask_holes`).
    #[inline]
    pub fn lookup_used(&self, wdir: V3) -> Option<(f32, f32, f32)> {
        let (x, y, wt) = self.lookup(wdir)?;
        if self.mask_at(x, y) == crate::masks::NEGATIVE { None } else { Some((x, y, wt)) }
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
    outline(p, steps, false)
}

/// Outline of the photo; with `follow_field` the edge is moved the way the parallax correction
/// moves it (the photo's border is where the corrected position leaves the image).
fn outline(p: &RenderPhoto, steps: usize, follow_field: bool) -> Vec<V3> {
    let w = p.w;
    let h = p.h;
    let l = w.max(h);
    let mut pts = Vec::new();
    let to_dir = |x: f64, y: f64| -> V3 {
        let (mut x, mut y) = (x, y);
        if let (true, Some(f)) = (follow_field, &p.field) {
            let d = f.at(x / w, y / h);
            x -= d[0] * l;
            y -= d[1] * l;
        }
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
                if let Some((x, y, wt)) = p.lookup_used(d) {
                    let c = sample_rgb_linear(&p.img, x, y);
                    let wt = if p.clipped_at(x, y) { wt * wt * CLIP_WEIGHT } else { wt * wt } * qual[pi];
                    let c = p.shade(c, x, y);
                    for k in 0..3 {
                        acc[k] += c[k] * wt;
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

/// Low-resolution coverage mask (1 = some photo covers the pixel). Masks do not count here: a hole
/// that masks leave in the middle would make "Auto crop" cut the panorama down around it; it is
/// shown in the preview and reported (`mask_holes`) instead.
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

/// Experiments (`PW_MASK_MAP=<file.png>` on export): the preview with the masks drawn in – orange where
/// some covering photo is masked "do not use", magenta where all are (a hole), green where one is
/// masked "use". Shows where masks land in the panorama (a moving boat is elsewhere in every photo).
pub fn mask_map(photos: &[RenderPhoto], view: &PanoView, w: usize, h: usize) -> Vec<u8> {
    let mut px = render_preview(photos, view, w, h, false, false);
    let m = Mapper::new(view, view.bounds, w, h);
    px.par_chunks_mut(w * 4).enumerate().for_each(|(py, row)| {
        for x in 0..w {
            let Some(d) = m.dir(x, py) else { continue };
            let (mut neg, mut pos, mut any, mut used) = (false, false, false, false);
            for p in photos {
                if let Some((u, v, _)) = p.lookup(d) {
                    any = true;
                    match p.mask_at(u, v) {
                        crate::masks::NEGATIVE => neg = true,
                        crate::masks::POSITIVE => pos = true,
                        _ => {}
                    }
                    used |= p.mask_at(u, v) != crate::masks::NEGATIVE;
                }
            }
            // magenta: masked in every covering photo (a hole), orange: masked in some of them only
            // (the others still show what is there), green: a photo is masked "use" here
            let tint = if any && !used { Some([255u8, 0, 255]) } else if neg { Some([255, 150, 0]) } else if pos { Some([40, 255, 80]) } else { None };
            if let Some(t) = tint {
                let o = x * 4;
                for k in 0..3 {
                    row[o + k] = ((row[o + k] as u16 + t[k] as u16) / 2) as u8;
                }
                row[o + 3] = 255;
            }
        }
    });
    px
}

/// Share of the panorama (within the crop) that a photo covers but that every covering photo has
/// masked "do not use": a hole the masks make. Checked on a small grid of `w` × `h` points.
pub fn mask_holes(photos: &[RenderPhoto], view: &PanoView, w: usize, h: usize) -> f32 {
    let m = Mapper::new(view, view.bounds, w, h);
    let cb = view.crop;
    let (holes, inside) = (0..w * h)
        .into_par_iter()
        .filter(|&i| {
            let fx = ((i % w) as f64 + 0.5) / w as f64;
            let fy = ((i / w) as f64 + 0.5) / h as f64;
            fx >= cb[0] && fx <= cb[1] && fy >= cb[2] && fy <= cb[3]
        })
        .map(|i| {
            let Some(d) = m.dir(i % w, i / w) else { return (0usize, 0usize) };
            if !photos.iter().any(|p| p.lookup(d).is_some()) {
                return (0, 0);
            }
            (usize::from(!photos.iter().any(|p| p.lookup_used(d).is_some())), 1)
        })
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    if inside == 0 { 0.0 } else { holes as f32 / inside as f32 }
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
                if let Some((x, y, wt)) = p.lookup_used(d)
                    && wt > 0.05 {
                        v.push((k, p.photometry_at(sample_rgb_linear(&p.img, x, y), x, y)));
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
        // seam map (6 B) + preferred photos of the seam finder (2 B) + band pyramid (16 B x 1.34) + result (16 B) + 8-bit output (4 B) + row buffers
        Blend::Smooth => px * 52,
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

/// Added to the score of the photo the seam finder prefers at a pixel, so it wins wherever it covers the pixel.
const PREFERRED_BONUS: f32 = 1e9;
/// A photo painted "use here" beats everything else (also the seam finder's choice).
const POSITIVE_BONUS: f32 = 1e11;
/// A photo painted "do not use here" only fills the spot when nothing else covers it.
const NEGATIVE_FACTOR: f32 = 1e-6;

/// The score of photo `p` at its position (`x`, `y`) after the person's mask and the seam finder's choice.
#[inline]
fn adjust_score(p: &RenderPhoto, x: f32, y: f32, score: f32, preferred: bool) -> f32 {
    match p.mask_at(x, y) {
        crate::masks::POSITIVE => score + POSITIVE_BONUS,
        crate::masks::NEGATIVE => score * NEGATIVE_FACTOR,
        _ if preferred => score + PREFERRED_BONUS,
        _ => score,
    }
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
    /// seam finder (Smooth blending); off = the photo with the best score wins at every pixel
    pub seams: crate::seams::SeamConfig,
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
    let mut all_rows = Vec::with_capacity(count);
    for i in 0..count {
        let p = load(i, false)?;
        details.push((p.detail(), p.trusted));
        all_rows.push(roi_rows(&p, &m, &vr, h));
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
                        if let Some((x, y, wt)) = p.lookup_used(d) {
                            let c = sample(&p.img, x, y);
                            let wt = if p.clipped_at(x, y) { wt * wt * CLIP_WEIGHT } else { wt * wt } * qual[i];
                            let c = p.shade(c, x, y);
                            for k in 0..3 {
                                ar[px][k] += c[k] * wt;
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
            // pass 1: which photo is best at each pixel (seams follow the middle of overlaps,
            // or the seam finder's choice)
            (prog.report)(0.0, "Finding seams");
            let labels = compute_labels(load, &m, &qual, &all_rows, w, h, ro, &cancelled)?;
            let pref = labels.as_ref().map(|l| l.preferred_rows(w, (0, h)));
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
                    let prow = pref.as_ref().map(|v| &v[py * w..(py + 1) * w]);
                    for px in 0..w {
                        let Some(d) = m.dir(px, py) else { continue };
                        if let Some((x, y, wt)) = p.lookup_used(d) {
                            // the photo with the most detail wins (see `seam_score`)
                            let sc = if prefer { seam_score(qual[i], wt) } else { wt };
                            let wt = if p.clipped_at(x, y) { sc * CLIP_WEIGHT } else { sc };
                            let wt = adjust_score(&p, x, y, wt, prow.is_some_and(|r| r[px] == i as u16));
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
                            if let Some((x, y, _)) = p.lookup_used(d) {
                                let c = sample(&p.img, x, y);
                                ir[px] = p.shade(c, x, y);
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
pub type StripSink<'a> = &'a mut dyn FnMut(usize, &[[f32; 3]], &[f32], usize) -> Result<(), String>;

/// Everything the strip render needs, worked out once: panorama size, strip height and the rows
/// every photo touches. `prepare_strips` makes it, `run` renders.
pub struct StripJob {
    pub w: usize,
    pub h: usize,
    /// inner rows per strip (the last one may be lower)
    pub rows: usize,
    /// number of strips
    pub count: usize,
    /// planned peak memory of one strip in bytes
    pub peak_bytes: u64,
    /// blend levels asked for and used (fewer when the memory is tight, see `strip::fit`)
    pub levels_wanted: usize,
    pub levels_used: usize,
    /// bytes the photo cache may keep
    pub cache_bytes: u64,
    blend: Blend,
    margin: usize,
    levels: usize,
    fill_extra: usize,
    ro: RenderOptions,
    m: Mapper,
    vr: M3,
    qual: Vec<f32>,
    photo_rows: Vec<(usize, usize)>,
    /// where the seam finder wants which photo (Smooth with the seam finder on)
    labels: Option<Arc<crate::seams::LabelMap>>,
}

/// Plans the memory-saving final render. The strip height follows from `budget`
/// (see `strip::plan`); it never changes the result, only how many strips are made. An error
/// message comes back if even the smallest strip does not fit into `budget`.
pub fn prepare_strips(
    count: usize,
    load: &(dyn Fn(usize, bool) -> Result<RenderPhoto, String> + Sync),
    view: &PanoView,
    out_w: usize,
    blend: Blend,
    ro: &RenderOptions,
    cfg: &crate::strip::StripConfig,
    budget: u64,
    prog: &Progress,
) -> Result<StripJob, String> {
    // `budget` is the whole memory of the export (strips and photo cache)
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
    let fit = crate::strip::fit(w, h, levels, blend, budget, cfg)?;
    let (plan, levels) = (fit.plan, fit.levels);
    // the seam finder decides once for the whole panorama, so the picture does not depend on the strips
    let labels = if blend == Blend::Smooth {
        let cancelled = || prog.cancel.load(Ordering::Relaxed) || crate::engine::cancelled();
        (prog.report)(0.0, "Finding seams");
        compute_labels(load, &m, &qual, &photo_rows, w, h, ro, &cancelled)?.map(Arc::new)
    } else {
        None
    };
    Ok(StripJob { blend, labels, w, h, rows: plan.rows, count: plan.count, peak_bytes: plan.peak_bytes, levels_wanted: fit.levels_wanted, levels_used: levels, cache_bytes: fit.cache_bytes, margin: plan.margin, levels, fill_extra: cfg.fill_extra_levels, ro: *ro, m, vr, qual, photo_rows })
}

impl StripJob {
    /// The seam finder's result (None when the seam finder was off or the blending is not Smooth).
    pub fn labels(&self) -> Option<&Arc<crate::seams::LabelMap>> {
        self.labels.as_ref()
    }

    /// Renders the panorama strip by strip and hands every strip to `sink` (in order from the top).
    pub fn run(
        &self,
        load: &(dyn Fn(usize, bool) -> Result<RenderPhoto, String> + Sync),
        sink: StripSink,
        prog: &Progress,
    ) -> Result<(), String> {
        let cancelled = || prog.cancel.load(Ordering::Relaxed) || crate::engine::cancelled();
        let strips = crate::strip::strips(self.h, self.rows);
        let n = strips.len();
        let (load_ns, sink_ns) = (std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0));
        let t_run = std::time::Instant::now();
        let timed_load = |i: usize, full: bool| {
            let t = std::time::Instant::now();
            let r = load(i, full);
            load_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
            r
        };
        let load: &(dyn Fn(usize, bool) -> Result<RenderPhoto, String> + Sync) = &timed_load;
        for (si, &(s0, s1)) in strips.iter().enumerate() {
            if cancelled() {
                return Err(CANCELLED.into());
            }
            (prog.report)(si as f32 / n as f32, &format!("Strip {} of {}", si + 1, n));
            let (ya, yb) = crate::strip::padded((s0, s1), self.margin, self.h);
            let (col, cov) = match self.blend {
                Blend::Smooth => smooth_window(load, &self.m, &self.vr, &self.qual, &self.photo_rows, self.w, (ya, yb), self.levels, self.fill_extra, &self.ro, self.labels.as_deref(), &cancelled)?,
                Blend::Feather => feather_window(load, &self.m, &self.qual, &self.photo_rows, self.w, (ya, yb), &self.ro, &cancelled)?,
            };
            let a = (s0 - ya) * self.w;
            let b = (s1 - ya) * self.w;
            let t = std::time::Instant::now();
            sink(s0, &col[a..b], &cov[a..b], self.w)?;
            sink_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        if std::env::var("PW_TIMING").is_ok() {
            let sec = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed) as f64 / 1e9;
            eprintln!("timing: strips {:.1} s in total, of which loading photos {:.1} s, writing {:.1} s", t_run.elapsed().as_secs_f64(), sec(&load_ns), sec(&sink_ns));
        }
        Ok(())
    }
}

/// `prepare_strips` and `run` in one go; returns the panorama size.
#[allow(dead_code)] // used by the tests
pub fn render_strips(
    count: usize,
    load: &(dyn Fn(usize, bool) -> Result<RenderPhoto, String> + Sync),
    view: &PanoView,
    out_w: usize,
    blend: Blend,
    ro: &RenderOptions,
    cfg: &crate::strip::StripConfig,
    budget: u64,
    sink: StripSink,
    prog: &Progress,
) -> Result<(usize, usize), String> {
    let job = prepare_strips(count, load, view, out_w, blend, ro, cfg, budget, prog)?;
    job.run(load, sink, prog)?;
    Ok((job.w, job.h))
}

/// The seam finder's label map for the whole panorama (None when the seam finder is off). Every photo
/// is looked at once in the middle of every block of a coarse grid; only the small working copy of the
/// photo is needed for that.
#[allow(clippy::too_many_arguments)]
fn compute_labels(
    load: &(dyn Fn(usize, bool) -> Result<RenderPhoto, String> + Sync),
    m: &Mapper,
    qual: &[f32],
    photo_rows: &[(usize, usize)],
    w: usize,
    h: usize,
    ro: &RenderOptions,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<crate::seams::LabelMap>, String> {
    use crate::seams::{PhotoBlocks, Sample, VALID};
    let cfg = ro.seams;
    if !cfg.enabled {
        return Ok(None);
    }
    let (bsz, bw, bh) = crate::seams::grid(w, h, cfg.max_side);
    let flat = flat_limits();
    let t0 = std::time::Instant::now();
    let mut photos = Vec::with_capacity(photo_rows.len());
    for (i, &(y0, y1)) in photo_rows.iter().enumerate() {
        if cancelled() {
            return Err(CANCELLED.into());
        }
        let p = load(i, false)?;
        let by0 = (y0 / bsz).min(bh);
        let by1 = y1.div_ceil(bsz).min(bh).max(by0);
        let mut pb = PhotoBlocks::new(by0, by1 - by0, bw);
        let (sx, sy) = (p.img.width() as f32 / p.w as f32, p.img.height() as f32 / p.h as f32);
        pb.samples.par_chunks_mut(bw).enumerate().for_each(|(r, row)| {
            let py = ((by0 + r) * bsz + bsz / 2).min(h - 1);
            for (bx, out) in row.iter_mut().enumerate() {
                let px = (bx * bsz + bsz / 2).min(w - 1);
                let Some(d) = m.dir(px, py) else { continue };
                let Some((x, y, wt)) = p.lookup(d) else { continue };
                // a more detailed photo only counts where there is detail to gain: in flat areas (sky) its
                // advantage fades out, so a zoomed-in photo does not cut a visible patch out of the sky
                let q = if ro.prefer_detail && flat.0 < flat.1 {
                    let tex = block_texture(&p, m, px, py, bsz, w, h, (sx, sy));
                    let t = ((tex - flat.0) / (flat.1 - flat.0)).clamp(0.0, 1.0);
                    qual[i].powf(t)
                } else {
                    qual[i]
                };
                let sc = if ro.prefer_detail { seam_score(q, wt) } else { wt };
                let score = if p.clipped_at(x, y) { sc * CLIP_WEIGHT } else { sc };
                let c = sample_rgb_linear(&p.img, (x + 0.5) * sx - 0.5, (y + 0.5) * sy - 0.5);
                let to8 = |v: f32| (crate::imgutil::linear_to_srgb(v) * 255.0 + 0.5) as u8;
                let c = p.shade(c, x, y);
                let flags = VALID
                    | match p.mask_at(x, y) {
                        crate::masks::POSITIVE => crate::seams::POSITIVE,
                        crate::masks::NEGATIVE => crate::seams::NEGATIVE,
                        crate::masks::GUARD => crate::seams::GUARD,
                        _ => 0,
                    };
                *out = Sample { rgb: [to8(c[0]), to8(c[1]), to8(c[2])], flags, score };
            }
        });
        photos.push(pb);
    }
    let t1 = t0.elapsed();
    let labels = crate::seams::find(&photos, bw, bh, &cfg, cancelled)?;
    if std::env::var("PW_SEAM_DEBUG").is_ok() {
        eprintln!("seam finder: grid {bw}x{bh} blocks of {bsz} px, {} photos, blocks {:.1} s, graph cuts {:.1} s", photos.len(), t1.as_secs_f32(), (t0.elapsed() - t1).as_secs_f32());
    }
    let map = crate::seams::LabelMap { bw, bh, bsz, labels };
    if let Ok(f) = std::env::var("PW_SEAM_DEBUG") {
        // experiments: a picture of the seams (one block = one pixel)
        let _ = map.debug_image().save(f);
    }
    Ok(Some(map))
}

/// Texture (local brightness change, sRGB 0..1) below which a block counts as flat, and above which as
/// textured; between the two a detail advantage fades in. `PW_FLAT_LO` / `PW_FLAT_HI` (0 = switched off).
const FLAT_LO: f32 = 0.02;
const FLAT_HI: f32 = 0.08;

fn flat_limits() -> (f32, f32) {
    let get = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    (get("PW_FLAT_LO", FLAT_LO), get("PW_FLAT_HI", FLAT_HI))
}

/// How much the picture changes across one block around the output pixel (`px`, `py`): the largest
/// brightness step to the four neighbours one block away (looked up through the same view direction,
/// so every photo is measured at the same angular scale).
#[allow(clippy::too_many_arguments)]
fn block_texture(p: &RenderPhoto, m: &Mapper, px: usize, py: usize, bsz: usize, w: usize, h: usize, (sx, sy): (f32, f32)) -> f32 {
    let luma = |x: usize, y: usize| -> Option<f32> {
        let d = m.dir(x.min(w - 1), y.min(h - 1))?;
        let (u, v, _) = p.lookup(d)?;
        let c = sample_rgb_linear(&p.img, (u + 0.5) * sx - 0.5, (v + 0.5) * sy - 0.5);
        Some(crate::imgutil::linear_to_srgb(0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]))
    };
    let Some(c) = luma(px, py) else { return 0.0 };
    let mut best = 0.0f32;
    for (dx, dy) in [(bsz as isize, 0isize), (-(bsz as isize), 0), (0, bsz as isize), (0, -(bsz as isize))] {
        let (nx, ny) = (px as isize + dx, py as isize + dy);
        if nx < 0 || ny < 0 {
            continue;
        }
        if let Some(l) = luma(nx as usize, ny as usize) {
            best = best.max((l - c).abs());
        }
    }
    best
}

/// Feather blend of the output rows `[ya, yb)` only. Every pixel depends on its own position only,
/// so this gives exactly the same pixels as the whole-image render (no margin needed).
#[allow(clippy::too_many_arguments)]
fn feather_window(
    load: &(dyn Fn(usize, bool) -> Result<RenderPhoto, String> + Sync),
    m: &Mapper,
    qual: &[f32],
    photo_rows: &[(usize, usize)],
    w: usize,
    (ya, yb): (usize, usize),
    ro: &RenderOptions,
    cancelled: &dyn Fn() -> bool,
) -> Result<(Vec<[f32; 3]>, Vec<f32>), String> {
    let sample = |img: &RgbImage, x: f32, y: f32| if ro.cubic { crate::imgutil::sample_rgb_linear_cubic(img, x, y) } else { sample_rgb_linear(img, x, y) };
    let wh = yb - ya;
    let mut acc = alloc(w * wh, [0.0f32; 3])?;
    let mut ws = alloc(w * wh, 0.0f32)?;
    for i in crate::strip::photos_for(photo_rows, (ya, yb)) {
        if cancelled() {
            return Err(CANCELLED.into());
        }
        let p = load(i, true)?;
        let (y0, y1) = (photo_rows[i].0.max(ya) - ya, photo_rows[i].1.min(yb) - ya);
        acc.par_chunks_mut(w).zip(ws.par_chunks_mut(w)).enumerate().for_each(|(ry, (ar, wr))| {
            if ry < y0 || ry >= y1 {
                return;
            }
            let py = ya + ry;
            for px in 0..w {
                let Some(d) = m.dir(px, py) else { continue };
                if let Some((x, y, wt)) = p.lookup_used(d) {
                    let c = sample(&p.img, x, y);
                    let wt = if p.clipped_at(x, y) { wt * wt * CLIP_WEIGHT } else { wt * wt } * qual[i];
                    let c = p.shade(c, x, y);
                    for k in 0..3 {
                        ar[px][k] += c[k] * wt;
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
    Ok((acc, ws))
}

/// Smooth blend of the output rows `[ya, yb)` only (`ya` must be a multiple of 2^(levels + fill_extra)). Same
/// two passes as in `render_final`, with all buffers just as high as the window.
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
    labels: Option<&crate::seams::LabelMap>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(Vec<[f32; 3]>, Vec<f32>), String> {
    let sample = |img: &RgbImage, x: f32, y: f32| if ro.cubic { crate::imgutil::sample_rgb_linear_cubic(img, x, y) } else { sample_rgb_linear(img, x, y) };
    let wh = yb - ya;
    let prefer = ro.prefer_detail;
    let pref = labels.map(|l| l.preferred_rows(w, (ya, yb)));
    // photos touching the window, with their rows relative to the window
    let touching = crate::strip::photos_for(photo_rows, (ya, yb));
    let rel = |i: usize| (photo_rows[i].0.max(ya) - ya, photo_rows[i].1.min(yb) - ya);
    let t_a = std::time::Instant::now();
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
            let prow = pref.as_ref().map(|v| &v[ry * w..(ry + 1) * w]);
            for px in 0..w {
                let Some(d) = m.dir(px, py) else { continue };
                if let Some((x, y, wt)) = p.lookup_used(d) {
                    let sc = if prefer { seam_score(qual[i], wt) } else { wt };
                    let wt = if p.clipped_at(x, y) { sc * CLIP_WEIGHT } else { sc };
                    let wt = adjust_score(&p, x, y, wt, prow.is_some_and(|r| r[px] == i as u16));
                    if wt > bw[px] {
                        bw[px] = wt;
                        br[px] = i as u16;
                    }
                }
            }
        });
    }
    drop(best_w);
    let (mut d_fill, mut d_samp, mut d_add) = (0.0f64, 0.0f64, 0.0f64);
    let d_best = t_a.elapsed().as_secs_f64();
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
        let t_s = std::time::Instant::now();
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
                if let Some((x, y, _)) = p.lookup_used(d) {
                    let c = sample(&p.img, x, y);
                    ir[px] = p.shade(c, x, y);
                    vr_[px] = 1.0;
                    if best[wy * w + px] == i as u16 {
                        mr[px] = 1.0;
                    }
                }
            }
        });
        d_samp += t_s.elapsed().as_secs_f64();
        let t_f = std::time::Instant::now();
        fill_holes(&mut img, &valid, rw, rh, levels + fill_extra);
        d_fill += t_f.elapsed().as_secs_f64();
        let t_p = std::time::Instant::now();
        pyr.add(&img, &mask, rw, rh, ry0);
        d_add += t_p.elapsed().as_secs_f64();
    }
    let t_c = std::time::Instant::now();
    let (col, wgt) = pyr.collapse();
    if std::env::var("PW_TIMING").is_ok() {
        eprintln!("timing window: best {d_best:.2} s, sampling {d_samp:.2} s, fill {d_fill:.2} s, pyramid add {d_add:.2} s, collapse {:.2} s ({} photos)", t_c.elapsed().as_secs_f64(), touching.len());
    }
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

/// Extra rows (output pixels) around the rows a photo touches; larger with a parallax field.
const ROI_RESERVE: f64 = 4.0;
const ROI_RESERVE_FIELD: f64 = 16.0;

/// Rows of the output touched by photo `p`.
fn roi_rows(p: &RenderPhoto, m: &Mapper, vr: &M3, h: usize) -> (usize, usize) {
    let mut y0 = f64::MAX;
    let mut y1 = f64::MIN;
    let mut pts = photo_outline(p, 32);
    if p.field.is_some() {
        // the parallax field moves the edge of the photo: take both outlines
        pts.extend(outline(p, 32, true));
    }
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
    // a little extra for the change of the field between the edge and the inside of the photo
    let reserve = if p.field.is_some() { ROI_RESERVE_FIELD } else { ROI_RESERVE };
    let mut y0 = (y0 - reserve).floor().max(0.0) as usize;
    let mut y1 = ((y1 + reserve).ceil().max(0.0) as usize).min(h);
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
    // every output pixel depends only on its own inputs, so the rows run on all cores (same result as one core)
    let mut levels: Vec<(Vec<[f32; 4]>, usize, usize)> = Vec::new();
    let mut base = vec![[0.0f32; 4]; w * h];
    base.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let (c, v) = (img[y * w + x], valid[y * w + x]);
            *o = [c[0] * v, c[1] * v, c[2] * v, v];
        }
    });
    levels.push((base, w, h));
    while levels.last().unwrap().1 > 1 && levels.last().unwrap().2 > 1 && levels.len() <= max_levels {
        let (prev, pw, ph) = levels.last().unwrap();
        let (pw, ph) = (*pw, *ph);
        let (nw, nh) = (pw.div_ceil(2), ph.div_ceil(2));
        let mut next = vec![[0.0f32; 4]; nw * nh];
        next.par_chunks_mut(nw).enumerate().for_each(|(y, row)| {
            for (x, o) in row.iter_mut().enumerate() {
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
                *o = s;
            }
        });
        levels.push((next, nw, nh));
        if nw * nh <= 1 {
            break;
        }
    }
    // pull: from coarse to fine, fill pixels without weight
    for l in (0..levels.len() - 1).rev() {
        let (fine_part, coarse_part) = levels.split_at_mut(l + 1);
        let (coarse, cw, ch) = (&coarse_part[0].0, coarse_part[0].1, coarse_part[0].2);
        let (fine, fw, _) = &mut fine_part[l];
        let fw = *fw;
        fine.par_chunks_mut(fw).enumerate().for_each(|(y, row)| {
            for (x, px) in row.iter_mut().enumerate() {
                if px[3] < 1e-6 {
                    let c = coarse[(y / 2).min(ch - 1) * cw + (x / 2).min(cw - 1)];
                    if c[3] > 1e-9 {
                        *px = [c[0] / c[3], c[1] / c[3], c[2] / c[3], 1.0];
                    }
                } else if px[3] != 1.0 {
                    let wv = px[3];
                    *px = [px[0] / wv, px[1] / wv, px[2] / wv, 1.0];
                }
            }
        });
    }
    let (base, _, _) = &levels[0];
    img.par_iter_mut().enumerate().for_each(|(i, c)| {
        if valid[i] < 0.5 {
            *c = [base[i][0], base[i][1], base[i][2]];
        }
    });
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
    let base: Vec<[f32; 4]> = img.par_iter().zip(mask.par_iter()).map(|(c, m)| [c[0], c[1], c[2], *m]).collect();
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
            g.par_iter()
                .zip(up.par_iter())
                .map(|(a, b)| {
                    let m = a[3];
                    [(a[0] - b[0]) * m, (a[1] - b[1]) * m, (a[2] - b[2]) * m, m]
                })
                .collect()
        } else {
            g.par_iter().map(|a| [a[0] * a[3], a[1] * a[3], a[2] * a[3], a[3]]).collect()
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
            let merged: Vec<[f32; 4]> = b.par_iter().zip(up.par_iter()).map(|(l, u)| [l[0] + u[0], l[1] + u[1], l[2] + u[2], l[3]]).collect();
            cur = (merged, bw, bh);
        }
        let (data, _, _) = cur;
        let col = data.par_iter().map(|v| [v[0].max(0.0), v[1].max(0.0), v[2].max(0.0)]).collect();
        let wgt = data.par_iter().map(|v| v[3]).collect();
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
mod fill_tests {
    use super::*;

    // the single-core version from before the rows ran in parallel
    fn fill_holes_reference(img: &mut [[f32; 3]], valid: &[f32], w: usize, h: usize, max_levels: usize) {
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

    fn picture(w: usize, h: usize) -> (Vec<[f32; 3]>, Vec<f32>) {
        let mut img = vec![[0.0f32; 3]; w * h];
        let mut valid = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                // a photo-like patch with a ragged edge and a hole inside
                let inside = x + (y * 7) % 11 < w * 2 / 3 && !(x > w / 4 && x < w / 3 && y > h / 3 && y < h / 2);
                if inside {
                    img[y * w + x] = [x as f32 / w as f32, y as f32 / h as f32, ((x * y) % 17) as f32 / 17.0];
                    valid[y * w + x] = if (x + y) % 13 == 0 { 0.5 } else { 1.0 };
                }
            }
        }
        (img, valid)
    }

    #[test]
    fn parallel_fill_gives_exactly_the_single_core_result() {
        for (w, h) in [(37usize, 29usize), (128, 96), (5, 3)] {
            let (img, valid) = picture(w, h);
            let (mut a, mut b) = (img.clone(), img);
            fill_holes(&mut a, &valid, w, h, usize::MAX);
            fill_holes_reference(&mut b, &valid, w, h, usize::MAX);
            assert_eq!(a, b, "{w}x{h}");
            let (mut a, mut b) = (a.clone(), b);
            fill_holes(&mut a, &valid, w, h, 2);
            fill_holes_reference(&mut b, &valid, w, h, 2);
            assert_eq!(a, b, "{w}x{h} with 2 levels");
        }
    }

    #[test]
    fn valid_pixels_stay_and_holes_get_filled() {
        let (w, h) = (64usize, 48usize);
        let (img, valid) = picture(w, h);
        let mut out = img.clone();
        fill_holes(&mut out, &valid, w, h, usize::MAX);
        for i in 0..w * h {
            if valid[i] >= 0.5 {
                assert_eq!(out[i], img[i]);
            }
        }
        assert!((0..w * h).any(|i| valid[i] < 0.5 && out[i] != [0.0; 3]));
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
                    gain_map: None,
                    photometry: None,
                    full_long: if detail_boost && i == 1 { 600.0 } else { 300.0 },
                    trusted: true,
                    mask: None,
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
    fn strips_of(photos: &[RenderPhoto], view: &PanoView, width: usize, blend: Blend, ro: &RenderOptions, cfg: &StripConfig) -> (Vec<u8>, usize, usize, usize) {
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
        let (w, h) = render_strips(photos.len(), &load, view, width, blend, ro, cfg, 8 << 30, &mut sink, &prog).unwrap();
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
            let (whole, n, _, _) = strips_of(&photos, &view, 2000, Blend::Smooth, &ro, &one);
            assert_eq!(n, 1);
            // several strips must give the very same pixels as the single window
            let cfg = StripConfig { max_rows: rows, min_rows: 16, ..Default::default() };
            let (strips, n, w2, h2) = strips_of(&photos, &view, 2000, Blend::Smooth, &ro, &cfg);
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

    /// Red masks make holes and leave pixels out of the blending; the strip height must still change nothing.
    #[test]
    fn with_red_masks_the_strip_height_still_changes_nothing() {
        let mut photos = scene(3, false);
        // the middle photo masked red in its centre, the last one on its right half
        let mut a = vec![crate::masks::NEUTRAL; 64];
        for y in 2..6 {
            for x in 2..6 {
                a[y * 8 + x] = crate::masks::NEGATIVE;
            }
        }
        let b: Vec<u8> = (0..64).map(|i| if i % 8 >= 4 { crate::masks::NEGATIVE } else { crate::masks::NEUTRAL }).collect();
        photos[1].mask = Some(Arc::new(crate::masks::MaskBuf { w: 8, h: 8, data: a }));
        photos[2].mask = Some(Arc::new(crate::masks::MaskBuf { w: 8, h: 8, data: b }));
        let mut view = PanoView::default();
        fit_bounds(&mut view, &photos);
        assert!(mask_holes(&photos, &view, 200, 60) > 0.0, "the right half of the last photo is a hole");
        for blend in [Blend::Smooth, Blend::Feather] {
            let ro = RenderOptions { levels: 5, ..Default::default() };
            let one = StripConfig { max_rows: 1 << 20, ..Default::default() };
            let (whole, _, _, _) = strips_of(&photos, &view, 1500, blend, &ro, &one);
            let cfg = StripConfig { max_rows: 64, min_rows: 16, ..Default::default() };
            let (strips, n, _, _) = strips_of(&photos, &view, 1500, blend, &ro, &cfg);
            assert!(n >= 3);
            let d = compare(&whole, &strips);
            assert!(d.max <= 1, "{blend:?}: strips differ from one window: max {} in {}/{} px", d.max, d.differing, d.total);
        }
    }

    #[test]
    fn feather_strips_are_exactly_the_whole_image_render() {
        let photos = scene(3, true);
        let mut view = PanoView::default();
        fit_bounds(&mut view, &photos);
        let load = |i: usize, _px: bool| Ok(photos[i].clone());
        let cancel = AtomicBool::new(false);
        let prog = Progress { report: &|_, _| {}, cancel: &cancel };
        let ro = RenderOptions { prefer_detail: true, ..Default::default() };
        let (col, cov, w, h) = render_final(photos.len(), &load, &view, 900, Blend::Feather, &ro, &prog).unwrap();
        let reference = to_rgba8(&col, &cov);
        for rows in [1, 17, 64, 100_000] {
            let cfg = StripConfig { max_rows: rows, min_rows: 1, ..Default::default() };
            let mut out: Vec<u8> = Vec::new();
            let mut sink = |y0: usize, c: &[[f32; 3]], v: &[f32], sw: usize| {
                assert_eq!(out.len(), y0 * sw * 4);
                out.extend_from_slice(&to_rgba8(c, v));
                Ok(())
            };
            let (w2, h2) = render_strips(photos.len(), &load, &view, 900, Blend::Feather, &ro, &cfg, 8 << 30, &mut sink, &prog).unwrap();
            assert_eq!((w, h), (w2, h2));
            let d = compare(&reference, &out);
            assert_eq!(d.max, 0, "rows {rows}: {} pixels differ", d.differing);
        }
    }

    /// The rows a photo touches must contain every row where the photo is really visible. With a
    /// parallax field the photo's edge moves, so the rows have to follow it (checked pixel by pixel).
    #[test]
    fn rows_of_a_photo_cover_everything_it_shows_even_with_a_parallax_field() {
        let mut photos = scene(3, false);
        for (i, p) in photos.iter_mut().enumerate() {
            let (gw, gh) = (8, 6);
            // a smooth field of up to +-0.03 / 0.04 of the long side, different for every photo
            let d = (0..gw * gh).map(|k| {
                let (x, y) = ((k % gw) as f32 / gw as f32, (k / gw) as f32 / gh as f32);
                [0.03 * ((i as f32 + 1.0) * 2.0 * x).sin(), 0.04 * ((i as f32 + 1.0) * 1.5 + 3.0 * y).sin() + 0.02]
            });
            p.field = Some(Arc::new(crate::fine_align::DisplacementField { gw, gh, d: d.collect() }));
        }
        let mut view = PanoView::default();
        fit_bounds(&mut view, &photos);
        let cb = view.crop_bounds();
        let (w, h) = (900usize, ((900.0 / ((cb[1] - cb[0]) / (cb[3] - cb[2]))).round() as usize).max(16));
        let m = Mapper::new(&view, cb, w, h);
        let vr = view.rotation();
        for (i, p) in photos.iter().enumerate() {
            let (y0, y1) = roi_rows(p, &m, &vr, h);
            let rows: Vec<usize> = (0..h).filter(|&py| (0..w).any(|px| m.dir(px, py).is_some_and(|d| p.lookup(d).is_some()))).collect();
            let (lo, hi) = (*rows.first().unwrap(), *rows.last().unwrap() + 1);
            assert!(y0 <= lo && y1 >= hi, "photo {i}: visible rows {lo}..{hi}, but only {y0}..{y1} are used");
        }
    }

    #[test]
    fn with_the_seam_finder_the_strip_height_still_changes_nothing() {
        let photos = scene(3, true);
        let mut view = PanoView::default();
        fit_bounds(&mut view, &photos);
        let seams = crate::seams::SeamConfig { enabled: true, max_side: 120, ..Default::default() };
        let ro = RenderOptions { prefer_detail: true, levels: 4, seams, ..Default::default() };
        let off = RenderOptions { seams: Default::default(), ..ro };
        let one = StripConfig { max_rows: 1 << 20, ..Default::default() };
        let (whole, n, _, _) = strips_of(&photos, &view, 1400, Blend::Smooth, &ro, &one);
        assert_eq!(n, 1);
        for rows in [32, 64, 160] {
            let cfg = StripConfig { max_rows: rows, min_rows: 16, ..Default::default() };
            let (strips, n, _, _) = strips_of(&photos, &view, 1400, Blend::Smooth, &ro, &cfg);
            assert!(n >= 2, "rows {rows}: {n} strips");
            let d = compare(&whole, &strips);
            assert!(d.max <= 1, "rows {rows}: strips differ from one window: max {} in {}/{} px", d.max, d.differing, d.total);
        }
        // the seam finder changes the picture (the seams move), the switch off gives the earlier picture
        let (plain, _, _, _) = strips_of(&photos, &view, 1400, Blend::Smooth, &off, &one);
        let (plain2, _, _, _) = strips_of(&photos, &view, 1400, Blend::Smooth, &RenderOptions { seams: crate::seams::SeamConfig { enabled: false, max_side: 77, ..Default::default() }, ..ro }, &one);
        assert_eq!(compare(&plain, &plain2).max, 0, "off is off, whatever the other seam settings are");
        let load = |i: usize, _px: bool| Ok(photos[i].clone());
        let cancel = AtomicBool::new(false);
        let prog = Progress { report: &|_, _| {}, cancel: &cancel };
        let (col, cov, _, _) = render_final(photos.len(), &load, &view, 1400, Blend::Smooth, &ro, &prog).unwrap();
        let reference = to_rgba8(&col, &cov);
        let dr = compare(&reference, &whole);
        eprintln!("seam finder on: strips vs render_final: mean {:.3} max {}; seams on vs off: mean {:.3}", dr.mean, dr.max, compare(&plain, &whole).mean);
        assert!(dr.mean < 1.5, "the whole-image render and the strips must agree about the seams: mean {:.3}", dr.mean);
    }

    /// Two plain photos (red and blue) that overlap. A mask painted over the whole blue photo must decide the
    /// overlap, with the seam finder on and off, and in strips as well as in the whole-image render.
    #[test]
    fn masks_decide_which_photo_shows_in_the_overlap() {
        let mut photos: Vec<RenderPhoto> = [[200u8, 30, 30], [30, 30, 200]]
            .iter()
            .enumerate()
            .map(|(i, c)| RenderPhoto {
                w: 300.0,
                h: 200.0,
                rot: camera_from_ypr((i as f64 * 22.0 - 11.0).to_radians(), 0.0, 0.0),
                lens: Lens::from_hfov_deg(50.0),
                field: None,
                gain: [1.0; 3],
                gain_map: None,
                photometry: None,
                full_long: 300.0,
                trusted: true,
                mask: None,
                img: Arc::new(crate::testutil::solid(300, 200, *c)),
            })
            .collect();
        let mut view = PanoView::default();
        fit_bounds(&mut view, &photos);
        let cb = view.crop_bounds();
        let (w, h) = (700usize, ((700.0 / ((cb[1] - cb[0]) / (cb[3] - cb[2]))).round() as usize).max(16));
        let m = Mapper::new(&view, cb, w, h);
        let cancel = AtomicBool::new(false);
        let prog = Progress { report: &|_, _| {}, cancel: &cancel };
        let all = |v: u8| Some(Arc::new(crate::masks::MaskBuf { w: 4, h: 4, data: vec![v; 16] }));
        for seams_on in [false, true] {
            let ro = RenderOptions { prefer_detail: true, levels: 3, seams: crate::seams::SeamConfig { enabled: seams_on, max_side: 140, ..Default::default() }, ..Default::default() };
            for (mask_kind, winner_inside_overlap) in [(crate::masks::POSITIVE, 1usize), (crate::masks::NEGATIVE, 0usize)] {
                photos[1].mask = all(mask_kind);
                let load = |i: usize, _px: bool| Ok(photos[i].clone());
                let (col, cov, _, _) = render_final(2, &load, &view, w, Blend::Smooth, &ro, &prog).unwrap();
                let (mut checked, mut wrong) = (0, 0);
                for py in 0..h {
                    for px in 0..w {
                        let Some(d) = m.dir(px, py) else { continue };
                        let (a, b) = (photos[0].lookup(d), photos[1].lookup(d));
                        // only pixels well inside both photos
                        let (Some(a), Some(b)) = (a, b) else { continue };
                        if a.2 < 0.15 || b.2 < 0.15 || cov[py * w + px] == 0.0 {
                            continue;
                        }
                        checked += 1;
                        let c = col[py * w + px];
                        let is_blue = c[2] > c[0];
                        if is_blue != (winner_inside_overlap == 1) {
                            wrong += 1;
                        }
                    }
                }
                assert!(checked > 500, "the photos should overlap well: {checked} pixels checked");
                assert!(wrong * 50 < checked, "seams {seams_on}, mask {mask_kind}: {wrong} of {checked} overlap pixels show the wrong photo");
            }
        }
    }

    /// A photo masked red everywhere gives nothing to the picture: no colour of it is blended into the
    /// overlap (before, its low frequencies were, which made masked things look blurred), and where
    /// only that photo covers the panorama there is a hole, which `mask_holes` reports.
    #[test]
    fn a_red_mask_leaves_nothing_of_the_photo_and_a_hole_where_no_other_covers() {
        let mut photos: Vec<RenderPhoto> = [[200u8, 30, 30], [30, 30, 200]]
            .iter()
            .enumerate()
            .map(|(i, c)| RenderPhoto {
                w: 300.0,
                h: 200.0,
                rot: camera_from_ypr((i as f64 * 22.0 - 11.0).to_radians(), 0.0, 0.0),
                lens: Lens::from_hfov_deg(50.0),
                field: None,
                gain: [1.0; 3],
                gain_map: None,
                photometry: None,
                full_long: 300.0,
                trusted: true,
                mask: None,
                img: Arc::new(crate::testutil::solid(300, 200, *c)),
            })
            .collect();
        let mut view = PanoView::default();
        fit_bounds(&mut view, &photos);
        assert_eq!(mask_holes(&photos, &view, 120, 40), 0.0, "no masks, no holes");
        photos[1].mask = Some(Arc::new(crate::masks::MaskBuf { w: 4, h: 4, data: vec![crate::masks::NEGATIVE; 16] }));
        let holes = mask_holes(&photos, &view, 120, 40);
        assert!(holes > 0.1 && holes < 0.9, "the part only the blue photo covers is a hole: {holes}");
        let cb = view.crop_bounds();
        let (w, h) = (700usize, ((700.0 / ((cb[1] - cb[0]) / (cb[3] - cb[2]))).round() as usize).max(16));
        let m = Mapper::new(&view, cb, w, h);
        let cancel = AtomicBool::new(false);
        let prog = Progress { report: &|_, _| {}, cancel: &cancel };
        let load = |i: usize, _px: bool| Ok(photos[i].clone());
        for blend in [Blend::Smooth, Blend::Feather] {
            let ro = RenderOptions { levels: 4, ..Default::default() };
            let (col, cov, _, _) = render_final(2, &load, &view, w, blend, &ro, &prog).unwrap();
            let (mut red, mut hole) = (0, 0);
            for py in 0..h {
                for px in 0..w {
                    let Some(d) = m.dir(px, py) else { continue };
                    let (a, b) = (photos[0].lookup(d), photos[1].lookup(d));
                    let c = col[py * w + px];
                    if a.is_some() && cov[py * w + px] > 0.0 {
                        // inside the red photo, also right next to the blue one: no blue at all
                        assert!(c[2] < c[0] * 0.25, "{blend:?}: blue blended in at ({px}, {py}): {c:?}");
                        red += 1;
                    } else if a.is_none() && b.is_some() {
                        assert_eq!(cov[py * w + px], 0.0, "{blend:?}: only the masked photo covers ({px}, {py}) – a hole");
                        hole += 1;
                    }
                }
            }
            assert!(red > 1000 && hole > 1000, "{blend:?}: {red} red, {hole} hole pixels");
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
        let e = render_strips(2, &load, &view, 400, Blend::Smooth, &RenderOptions::default(), &StripConfig::default(), 8 << 30, &mut sink, &prog).unwrap_err();
        assert_eq!(e, CANCELLED);
    }
}
