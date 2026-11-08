//! Overlap check: two photos laid over each other on a flat plane (blend, difference heat map,
//! checkerboard). The sampling lives here (not in the egui closure) so it can be tested.
//!
//! A photo that has more pixels per angle than the plane is low-pass filtered to the plane's
//! sampling step first; otherwise it is point-sampled with the coarser raster of the plane,
//! the fine texture aliases and the difference mode shows texture as a difference.

use crate::fine_align::Plane;
use crate::imgutil;
use crate::stitch::RenderPhoto;
use image::imageops::{self, FilterType};
use std::sync::Arc;

/// Look of the overlap view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OverlapMode {
    /// A and B mixed by `alpha` (0 = A, 1 = B)
    Blend,
    /// heat map of the brightness difference
    Difference,
    /// tiles of A and B
    Checker,
}

/// Settings of the overlap view.
#[derive(Clone, Copy, Debug)]
pub struct OverlapConfig {
    /// low-pass filter a photo that is finer than the plane (off = point sampling as before)
    pub prefilter: bool,
    /// below this ratio (photo pixels per plane pixel) a photo is not filtered at all
    pub min_ratio: f64,
    /// difference mode: bring B's brightness and contrast to A's first (see `ToneFit`);
    /// off = the plain brightness difference as before
    pub match_tone: bool,
}

impl Default for OverlapConfig {
    fn default() -> Self {
        OverlapConfig { prefilter: true, min_ratio: 1.25, match_tone: true }
    }
}

/// Difference mode: brightness difference (in sRGB) that is shown as full white.
pub const DIFF_FULL_SCALE: f32 = 0.25;

/// One photo ready for sampling: possibly a filtered, smaller copy of the working image.
pub struct Prepared {
    pub photo: RenderPhoto,
    /// `img` size / `photo.w`
    sx: f32,
    sy: f32,
    /// ratio that was used for the filter (1 = none)
    pub ratio: f64,
}

/// Radians per working-image pixel of the photo (centre).
fn pixel_angle(p: &RenderPhoto) -> f64 {
    1.0 / (p.lens.f * p.w.max(p.h))
}

impl Prepared {
    pub fn new(photo: &RenderPhoto, plane_scale: f64, cfg: &OverlapConfig) -> Prepared {
        Prepared::with_step(photo, plane_scale, cfg)
    }

    /// Like `new`, but the photo is averaged to the angular step `step` (radians).
    pub fn with_step(photo: &RenderPhoto, step: f64, cfg: &OverlapConfig) -> Prepared {
        let ratio = step / pixel_angle(photo);
        let mut photo = photo.clone();
        let mut used = 1.0;
        if cfg.prefilter && ratio > cfg.min_ratio {
            let (iw, ih) = (photo.img.width(), photo.img.height());
            let nw = ((iw as f64 / ratio).round() as u32).max(1);
            let nh = ((ih as f64 / ratio).round() as u32).max(1);
            // Triangle widens its kernel when shrinking = area-aware averaging
            photo.img = Arc::new(imageops::resize(&*photo.img, nw, nh, FilterType::Triangle));
            used = ratio;
        }
        let sx = photo.img.width() as f32 / photo.w as f32;
        let sy = photo.img.height() as f32 / photo.h as f32;
        Prepared { photo, sx, sy, ratio: used }
    }

    /// Colour (linear light, gain applied) in direction `d`.
    #[inline]
    pub fn sample(&self, d: [f64; 3]) -> Option<[f32; 3]> {
        let (px, py, _) = self.photo.lookup(d)?;
        let c = imgutil::sample_rgb_linear(&self.photo.img, (px + 0.5) * self.sx - 0.5, (py + 0.5) * self.sy - 0.5);
        Some(self.photo.shade(c, px, py))
    }
}

/// Heat map of a difference 0..1: black - red - yellow - white.
fn heat(t: f32) -> [u8; 3] {
    [((t * 3.0).min(1.0) * 255.0) as u8, ((t * 3.0 - 1.0).clamp(0.0, 1.0) * 255.0) as u8, ((t * 3.0 - 2.0).clamp(0.0, 1.0) * 255.0) as u8]
}

/// Brightness (sRGB) of a linear colour.
fn luma(c: [f32; 3]) -> f32 {
    imgutil::linear_to_srgb(0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2])
}

#[inline]
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// Render `rw` x `rh` RGBA pixels of the two photos on `plane`.
pub fn render_overlap(photos: [&RenderPhoto; 2], plane: &Plane, rw: usize, rh: usize, mode: OverlapMode, alpha: f32, tile: usize, cfg: &OverlapConfig) -> Vec<u8> {
    use rayon::prelude::*;
    let pa = Prepared::new(photos[0], plane.scale, cfg);
    let pb = Prepared::new(photos[1], plane.scale, cfg);
    let tile = tile.max(1);
    // checkerboard stays untouched: it shows the photos as they are
    let fit = if cfg.match_tone && mode != OverlapMode::Checker { fit_tone(&pa, &pb, plane, rw, rh) } else { ToneFit::NONE };
    let mut rgba = vec![0u8; rw * rh * 4];
    rgba.par_chunks_mut(rw * 4).enumerate().for_each(|(y, row)| {
        for x in 0..rw {
            let d = plane.dir(x as f64 - rw as f64 * 0.5, y as f64 - rh as f64 * 0.5);
            let (ca, mut cb) = (pa.sample(d), pb.sample(d));
            if mode == OverlapMode::Blend && fit != ToneFit::NONE {
                cb = cb.map(|v| fit.apply_rgb(v));
            }
            let o = x * 4;
            let lin = match (ca, cb) {
                (Some(u), Some(v)) => match mode {
                    OverlapMode::Blend => [u[0] * (1.0 - alpha) + v[0] * alpha, u[1] * (1.0 - alpha) + v[1] * alpha, u[2] * (1.0 - alpha) + v[2] * alpha],
                    OverlapMode::Difference => {
                        let t = ((luma(u) - fit.apply(luma(v))).abs() / DIFF_FULL_SCALE).min(1.0);
                        let h = heat(t);
                        row[o..o + 3].copy_from_slice(&h);
                        row[o + 3] = 255;
                        continue;
                    }
                    OverlapMode::Checker => {
                        if ((x / tile) + (y / tile)).is_multiple_of(2) { u } else { v }
                    }
                },
                (Some(u), None) | (None, Some(u)) => [u[0] * 0.35, u[1] * 0.35, u[2] * 0.35],
                (None, None) => [0.02, 0.02, 0.025],
            };
            for k in 0..3 {
                row[o + k] = (imgutil::linear_to_srgb(lin[k]) * 255.0) as u8;
            }
            row[o + 3] = 255;
        }
    });
    rgba
}

/// Smallest common area (pixels) from which the tone of the two photos is compared.
pub const MATCH_MIN_PIXELS: usize = 200;
/// Largest contrast factor the tone match applies (and its inverse).
pub const MATCH_LIMIT: f32 = 4.0;

/// Brightness (sRGB luma) of B mapped onto A's: `luma_b * slope + offset`, found so that mean and
/// spread of both photos over their common area agree. Two different cameras (or the same one
/// with different exposure and tone mapping) differ in brightness and contrast, which is not a
/// geometry error but fills the difference view with texture-shaped red.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ToneFit {
    pub slope: f32,
    pub offset: f32,
    /// pixels both photos cover (sampled with a stride of 2)
    pub n: usize,
    /// mean and standard deviation of A and B (sRGB luma) over those pixels
    pub mean: [f32; 2],
    pub std: [f32; 2],
}

impl ToneFit {
    /// no change
    pub const NONE: ToneFit = ToneFit { slope: 1.0, offset: 0.0, n: 0, mean: [0.0; 2], std: [0.0; 2] };

    #[inline]
    pub fn apply(&self, l: f32) -> f32 {
        l * self.slope + self.offset
    }

    /// The same mapping for a colour (linear light): applied to every channel in sRGB.
    #[inline]
    pub fn apply_rgb(&self, c: [f32; 3]) -> [f32; 3] {
        let f = |v: f32| srgb_to_linear(self.apply(imgutil::linear_to_srgb(v)).clamp(0.0, 1.0));
        [f(c[0]), f(c[1]), f(c[2])]
    }
}

/// Tone fit of B to A from sums of luma: n, sum a, sum b, sum a², sum b².
fn fit_from_sums(n: usize, sa: f64, sb: f64, saa: f64, sbb: f64) -> ToneFit {
    if n < MATCH_MIN_PIXELS {
        return ToneFit { n, ..ToneFit::NONE };
    }
    let m = n as f64;
    let (ma, mb) = (sa / m, sb / m);
    let (va, vb) = ((saa / m - ma * ma).max(0.0), (sbb / m - mb * mb).max(0.0));
    let (da, db) = (va.sqrt(), vb.sqrt());
    // a flat photo has no contrast to compare: only the mean is matched
    let slope = if db > 1e-4 && da > 1e-4 { ((da / db) as f32).clamp(1.0 / MATCH_LIMIT, MATCH_LIMIT) } else { 1.0 };
    let offset = ma as f32 - slope * mb as f32;
    ToneFit { slope, offset, n, mean: [ma as f32, mb as f32], std: [da as f32, db as f32] }
}

/// Tone fit of the two prepared photos over the area both cover on `plane`.
fn fit_tone(pa: &Prepared, pb: &Prepared, plane: &Plane, rw: usize, rh: usize) -> ToneFit {
    let (mut n, mut sa, mut sb, mut saa, mut sbb) = (0usize, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for y in (0..rh).step_by(2) {
        for x in (0..rw).step_by(2) {
            let d = plane.dir(x as f64 - rw as f64 * 0.5, y as f64 - rh as f64 * 0.5);
            if let (Some(u), Some(v)) = (pa.sample(d), pb.sample(d)) {
                let (a, b) = (luma(u) as f64, luma(v) as f64);
                n += 1;
                sa += a;
                sb += b;
                saa += a * a;
                sbb += b * b;
            }
        }
    }
    fit_from_sums(n, sa, sb, saa, sbb)
}

/// Statistics of the common area, for the experiments (`PW_OVERLAP_STATS`).
#[derive(Clone, Copy, Debug)]
pub struct OverlapStats {
    pub fit: ToneFit,
    /// mean difference mode value without and with the tone match
    pub diff_raw: f32,
    pub diff_fit: f32,
}

pub fn overlap_stats(photos: [&RenderPhoto; 2], plane: &Plane, rw: usize, rh: usize, cfg: &OverlapConfig) -> OverlapStats {
    let pa = Prepared::new(photos[0], plane.scale, cfg);
    let pb = Prepared::new(photos[1], plane.scale, cfg);
    let fit = fit_tone(&pa, &pb, plane, rw, rh);
    let (mut raw, mut fitted, mut n) = (0.0f64, 0.0f64, 0usize);
    for y in 0..rh {
        for x in 0..rw {
            let d = plane.dir(x as f64 - rw as f64 * 0.5, y as f64 - rh as f64 * 0.5);
            if let (Some(u), Some(v)) = (pa.sample(d), pb.sample(d)) {
                let (a, b) = (luma(u), luma(v));
                raw += ((a - b).abs() / DIFF_FULL_SCALE).min(1.0) as f64;
                fitted += ((a - fit.apply(b)).abs() / DIFF_FULL_SCALE).min(1.0) as f64;
                n += 1;
            }
        }
    }
    let m = n.max(1) as f64;
    OverlapStats { fit, diff_raw: (raw / m) as f32, diff_fit: (fitted / m) as f32 }
}

/// Check: one photo rendered by the final export (`stitch::render_final`, feather blending of
/// one photo, flat view of the plane) against the overlap view's own sampling of the same plane
/// (unfiltered, as the view would show it at zoom 1 for the finest photo). Returns the mean
/// absolute difference (sRGB, 0..1) over the pixels both cover, the mean signed difference
/// (export minus view) and the pixel count. Nothing here is used by the GUI itself.
pub fn compare_with_export(photo: &RenderPhoto, plane: &Plane, rw: usize, rh: usize) -> Result<(f32, f32, usize), String> {
    use crate::geometry::{M3, ypr_of};
    use crate::stitch::{Blend, PanoView, Progress, Projection, RenderOptions, render_final};
    use std::sync::atomic::AtomicBool;
    // world -> view rotation with the plane's axes as rows
    let r: M3 = [plane.ex, plane.ey, plane.d];
    let (yaw, pitch, roll) = ypr_of(&r);
    let half = (rw as f64 * 0.5 * plane.scale, rh as f64 * 0.5 * plane.scale);
    let view = PanoView { projection: Projection::Rectilinear, yaw, pitch, roll, bounds: [-half.0, half.0, -half.1, half.1], crop: [0.0, 1.0, 0.0, 1.0] };
    let cancel = AtomicBool::new(false);
    let report = |_: f32, _: &str| {};
    let prog = Progress { report: &report, cancel: &cancel };
    let load = |_: usize, _: bool| -> Result<RenderPhoto, String> { Ok(photo.clone()) };
    let (col, cov, w, h) = render_final(1, &load, &view, rw, Blend::Feather, &RenderOptions::default(), &prog)?;
    let off = OverlapConfig { prefilter: false, ..Default::default() };
    let pa = Prepared::new(photo, plane.scale, &off);
    let (mut sum, mut signed, mut n) = (0.0f64, 0.0f64, 0usize);
    for y in 0..h.min(rh) {
        for x in 0..w.min(rw) {
            if cov[y * w + x] <= 0.0 {
                continue;
            }
            // the export samples at pixel centres (x + 0.5), the overlap view at x (see render_overlap)
            let d = plane.dir(x as f64 + 0.5 - rw as f64 * 0.5, y as f64 + 0.5 - rh as f64 * 0.5);
            if let Some(v) = pa.sample(d) {
                let (e, o) = (luma(col[y * w + x]), luma(v));
                sum += (e - o).abs() as f64;
                signed += (e - o) as f64;
                n += 1;
            }
        }
    }
    let m = n.max(1) as f64;
    Ok(((sum / m) as f32, (signed / m) as f32, n))
}

/// Diagnostics: mean difference mode value when both photos are averaged to `blur` times the
/// plane's step (1 = as in the view) and B is sampled `shift` plane pixels away from A.
pub fn scan_difference(photos: [&RenderPhoto; 2], plane: &Plane, rw: usize, rh: usize, blur: f64, shift: (f64, f64)) -> f32 {
    let cfg = OverlapConfig::default();
    let pa = Prepared::with_step(photos[0], plane.scale * blur, &cfg);
    let pb = Prepared::with_step(photos[1], plane.scale * blur, &cfg);
    let fit = if cfg.match_tone { fit_tone(&pa, &pb, plane, rw, rh) } else { ToneFit::NONE };
    let (mut sum, mut n) = (0.0f64, 0usize);
    for y in (0..rh).step_by(2) {
        for x in (0..rw).step_by(2) {
            let (u, v) = (x as f64 - rw as f64 * 0.5, y as f64 - rh as f64 * 0.5);
            if let (Some(a), Some(b)) = (pa.sample(plane.dir(u, v)), pb.sample(plane.dir(u + shift.0, v + shift.1))) {
                sum += ((luma(a) - fit.apply(luma(b))).abs() / DIFF_FULL_SCALE).min(1.0) as f64;
                n += 1;
            }
        }
    }
    (sum / n.max(1) as f64) as f32
}

/// Mean difference mode value (0..1) over the pixels both photos cover, and how many those are.
pub fn mean_difference(photos: [&RenderPhoto; 2], plane: &Plane, rw: usize, rh: usize, cfg: &OverlapConfig) -> (f32, usize) {
    let pa = Prepared::new(photos[0], plane.scale, cfg);
    let pb = Prepared::new(photos[1], plane.scale, cfg);
    let (mut sum, mut n) = (0.0f64, 0usize);
    for y in 0..rh {
        for x in 0..rw {
            let d = plane.dir(x as f64 - rw as f64 * 0.5, y as f64 - rh as f64 * 0.5);
            if let (Some(u), Some(v)) = (pa.sample(d), pb.sample(d)) {
                sum += ((luma(u) - luma(v)).abs() / DIFF_FULL_SCALE).min(1.0) as f64;
                n += 1;
            }
        }
    }
    ((sum / n.max(1) as f64) as f32, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::align::Lens;
    use crate::geometry::M3;
    use image::RgbImage;

    const ID: M3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

    /// Smooth scene as a function of the direction (so every resolution sees the same picture),
    /// with fine texture (period of a few coarse pixels).
    fn scene(d: [f64; 3]) -> u8 {
        let (x, y) = (d[0] / d[2], d[1] / d[2]);
        let v = 0.5 + 0.25 * (x * 150.0).sin() * (y * 130.0).cos() + 0.15 * (x * 700.0 + y * 300.0).sin() + 0.1 * (x * 7.0).sin();
        (v.clamp(0.0, 1.0) * 255.0) as u8
    }

    fn photo(size: u32, f: f64) -> RenderPhoto {
        photo_tone(size, f, 1.0, 0.0)
    }

    /// Same scene with the brightness `v * contrast + shift` (another camera, another tone curve).
    fn photo_tone(size: u32, f: f64, contrast: f64, shift: f64) -> RenderPhoto {
        let lens = Lens { f, k1: 0.0, from_exif: false };
        let l = size as f64;
        let mut img = RgbImage::new(size, size);
        for y in 0..size {
            for x in 0..size {
                let n = [(x as f64 + 0.5 - l / 2.0) / l, (y as f64 + 0.5 - l / 2.0) / l];
                let d = crate::geometry::normalize([n[0] / f, n[1] / f, 1.0]);
                let v = ((scene(d) as f64 / 255.0 * contrast + shift).clamp(0.0, 1.0) * 255.0) as u8;
                img.put_pixel(x, y, image::Rgb([v, v, v]));
            }
        }
        RenderPhoto { img: Arc::new(img), w: l, h: l, rot: ID, lens, field: None, gain: [1.0; 3], gain_map: None, photometry: None, full_long: l, trusted: true, mask: None }
    }

    /// Two photos of the same scene, exactly fitting, one with 4 times the pixels per angle.
    #[test]
    fn resolution_difference_does_not_show_as_difference() {
        let coarse = photo(300, 1.0);
        let fine = photo(1200, 1.0);
        let plane = Plane::new([0.0, 0.0, 1.0], &ID, coarse_angle(&coarse));
        let on = OverlapConfig::default();
        let off = OverlapConfig { prefilter: false, ..on };
        let (with, n) = mean_difference([&coarse, &fine], &plane, 200, 200, &on);
        let (without, _) = mean_difference([&coarse, &fine], &plane, 200, 200, &off);
        assert!(n > 30000, "overlap too small: {n}");
        // the same photo against itself is the floor of what the sampling itself gives
        let (same, _) = mean_difference([&coarse, &coarse], &plane, 200, 200, &on);
        eprintln!("mean difference: filtered {with:.4}, unfiltered {without:.4}, coarse vs coarse {same:.4}");
        assert!(with < 0.08, "filtered difference too big: {with}");
        assert!(without > 2.0 * with, "without the filter the difference should be clearly bigger: {without} vs {with}");
    }

    /// The overlap view samples a photo exactly as the export does (same geometry, same pixels).
    #[test]
    fn view_matches_export_for_one_photo() {
        let p = photo(300, 1.0);
        // plane not centred on the photo and tilted (checks the axes, not only the middle)
        let plane = Plane::new(crate::geometry::normalize([0.1, -0.05, 1.0]), &ID, coarse_angle(&p));
        let (diff, signed, n) = compare_with_export(&p, &plane, 160, 120).unwrap();
        eprintln!("export vs view: mean abs diff {diff:.5}, signed {signed:.5}, {n} px");
        assert!(n > 10000);
        assert!(diff < 0.01, "view and export differ: {diff}");
    }

    /// A photo with another contrast/brightness of the same scene: the plain difference is big,
    /// the tone-matched one small; with the switch off the picture is the plain one.
    #[test]
    fn tone_match_removes_brightness_and_contrast_difference() {
        let a = photo(300, 1.0);
        let b = photo_tone(300, 1.0, 0.6, 0.2);
        let plane = Plane::new([0.0, 0.0, 1.0], &ID, coarse_angle(&a));
        let on = OverlapConfig::default();
        let off = OverlapConfig { match_tone: false, ..on };
        let st = overlap_stats([&a, &b], &plane, 200, 200, &on);
        eprintln!("tone fit {:?}, diff raw {:.4}, matched {:.4}", st.fit, st.diff_raw, st.diff_fit);
        assert!(st.diff_raw > 0.15, "test photos should differ clearly: {}", st.diff_raw);
        assert!(st.diff_fit < 0.1 * st.diff_raw, "tone match should remove most of it: {} vs {}", st.diff_fit, st.diff_raw);
        assert!((st.fit.slope - 1.0 / 0.6).abs() < 0.1, "slope {}", st.fit.slope);
        // switch off: the same picture as without any tone code
        let img_off = render_overlap([&a, &b], &plane, 64, 64, OverlapMode::Difference, 0.5, 8, &off);
        let img_on = render_overlap([&a, &b], &plane, 64, 64, OverlapMode::Difference, 0.5, 8, &on);
        assert_ne!(img_off, img_on);
        let brightness = |v: &[u8]| v.chunks(4).map(|p| p[0] as u64).sum::<u64>();
        assert!(brightness(&img_on) < brightness(&img_off) / 3);
        // identical photos: the fit changes nothing
        let same = overlap_stats([&a, &a], &plane, 200, 200, &on);
        assert!((same.fit.slope - 1.0).abs() < 1e-3 && same.fit.offset.abs() < 1e-3);
        // checkerboard is not touched
        let c_off = render_overlap([&a, &b], &plane, 64, 64, OverlapMode::Checker, 0.5, 8, &off);
        let c_on = render_overlap([&a, &b], &plane, 64, 64, OverlapMode::Checker, 0.5, 8, &on);
        assert_eq!(c_off, c_on);
    }

    #[test]
    fn tone_fit_needs_a_common_area() {
        let f = fit_from_sums(5, 1.0, 2.0, 3.0, 4.0);
        assert_eq!((f.slope, f.offset), (1.0, 0.0));
        // flat photo B: only the mean is matched
        let f = fit_from_sums(1000, 500.0, 300.0, 270.0, 90.0);
        assert_eq!(f.slope, 1.0);
        assert!((f.offset - 0.2).abs() < 1e-5);
    }

    fn coarse_angle(p: &RenderPhoto) -> f64 {
        1.0 / (p.lens.f * p.w.max(p.h))
    }

    #[test]
    fn filter_only_for_finer_photo() {
        let coarse = photo(100, 1.0);
        let fine = photo(400, 1.0);
        let cfg = OverlapConfig::default();
        let s = coarse_angle(&coarse);
        assert_eq!(Prepared::new(&coarse, s, &cfg).ratio, 1.0);
        assert!((Prepared::new(&fine, s, &cfg).ratio - 4.0).abs() < 1e-9);
        let off = OverlapConfig { prefilter: false, ..cfg };
        assert_eq!(Prepared::new(&fine, s, &off).ratio, 1.0);
        // zoomed in (plane finer than the photo): untouched
        assert_eq!(Prepared::new(&fine, s / 8.0, &cfg).ratio, 1.0);
    }
}
