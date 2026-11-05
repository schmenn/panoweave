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
}

impl Default for OverlapConfig {
    fn default() -> Self {
        OverlapConfig { prefilter: true, min_ratio: 1.25 }
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
        let ratio = plane_scale / pixel_angle(photo);
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
        let g = self.photo.gain_at(px, py);
        Some([c[0] * g[0], c[1] * g[1], c[2] * g[2]])
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

/// Render `rw` x `rh` RGBA pixels of the two photos on `plane`.
pub fn render_overlap(photos: [&RenderPhoto; 2], plane: &Plane, rw: usize, rh: usize, mode: OverlapMode, alpha: f32, tile: usize, cfg: &OverlapConfig) -> Vec<u8> {
    use rayon::prelude::*;
    let pa = Prepared::new(photos[0], plane.scale, cfg);
    let pb = Prepared::new(photos[1], plane.scale, cfg);
    let tile = tile.max(1);
    let mut rgba = vec![0u8; rw * rh * 4];
    rgba.par_chunks_mut(rw * 4).enumerate().for_each(|(y, row)| {
        for x in 0..rw {
            let d = plane.dir(x as f64 - rw as f64 * 0.5, y as f64 - rh as f64 * 0.5);
            let (ca, cb) = (pa.sample(d), pb.sample(d));
            let o = x * 4;
            let lin = match (ca, cb) {
                (Some(u), Some(v)) => match mode {
                    OverlapMode::Blend => [u[0] * (1.0 - alpha) + v[0] * alpha, u[1] * (1.0 - alpha) + v[1] * alpha, u[2] * (1.0 - alpha) + v[2] * alpha],
                    OverlapMode::Difference => {
                        let t = ((luma(u) - luma(v)).abs() / DIFF_FULL_SCALE).min(1.0);
                        let h = heat(t);
                        row[o..o + 3].copy_from_slice(&h);
                        row[o + 3] = 255;
                        continue;
                    }
                    OverlapMode::Checker => {
                        if ((x / tile) + (y / tile)) % 2 == 0 { u } else { v }
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
        let lens = Lens { f, k1: 0.0, from_exif: false };
        let l = size as f64;
        let mut img = RgbImage::new(size, size);
        for y in 0..size {
            for x in 0..size {
                let n = [(x as f64 + 0.5 - l / 2.0) / l, (y as f64 + 0.5 - l / 2.0) / l];
                let d = crate::geometry::normalize([n[0] / f, n[1] / f, 1.0]);
                let v = scene(d);
                img.put_pixel(x, y, image::Rgb([v, v, v]));
            }
        }
        RenderPhoto { img: Arc::new(img), w: l, h: l, rot: ID, lens, field: None, gain: [1.0; 3], gain_map: None, full_long: l, trusted: true, mask: None }
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
