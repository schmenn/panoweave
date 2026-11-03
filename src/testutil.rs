//! Helpers for unit tests: synthetic images (no real photos needed).

use crate::geometry::Rng;
use image::{Rgb, RgbImage};

struct Octave {
    cell: f32,
    amp: f32,
    gw: usize,
    vals: Vec<f32>,
}

/// Smooth, feature-rich random texture (value noise at three scales), deterministic per seed.
/// Gives corners everywhere, so it is suitable for point finding tests.
pub fn texture(w: u32, h: u32, seed: u64) -> RgbImage {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    // three channels x three octaves
    let mut chans: Vec<Vec<Octave>> = Vec::new();
    for _ in 0..3 {
        let mut octs = Vec::new();
        for (cell, amp) in [(24.0f32, 0.45f32), (9.0, 0.35), (4.0, 0.2)] {
            let gw = (w as f32 / cell).ceil() as usize + 2;
            let gh = (h as f32 / cell).ceil() as usize + 2;
            octs.push(Octave { cell, amp, gw, vals: (0..gw * gh).map(|_| rng.unit() as f32).collect() });
        }
        chans.push(octs);
    }
    let mut img = RgbImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let mut px = [0u8; 3];
            for (c, octs) in chans.iter().enumerate() {
                let mut acc = 0.0f32;
                for o in octs {
                    let fx = x as f32 / o.cell;
                    let fy = y as f32 / o.cell;
                    let (x0, y0) = (fx as usize, fy as usize);
                    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
                    let g = |xx: usize, yy: usize| o.vals[yy * o.gw + xx];
                    let a = g(x0, y0) * (1.0 - tx) + g(x0 + 1, y0) * tx;
                    let b = g(x0, y0 + 1) * (1.0 - tx) + g(x0 + 1, y0 + 1) * tx;
                    acc += (a * (1.0 - ty) + b * ty) * o.amp;
                }
                px[c] = (acc * 255.0).clamp(0.0, 255.0) as u8;
            }
            img.put_pixel(x, y, Rgb(px));
        }
    }
    // sharp-edged random rectangles give strong, distinctive corners
    for _ in 0..(w * h / 150).max(8) {
        let rw = 5 + rng.below(30) as u32;
        let rh = 5 + rng.below(30) as u32;
        let x0 = rng.below(w as usize) as u32;
        let y0 = rng.below(h as usize) as u32;
        let col = Rgb([rng.below(256) as u8, rng.below(256) as u8, rng.below(256) as u8]);
        for y in y0..(y0 + rh).min(h) {
            for x in x0..(x0 + rw).min(w) {
                img.put_pixel(x, y, col);
            }
        }
    }
    img
}

pub fn solid(w: u32, h: u32, rgb: [u8; 3]) -> RgbImage {
    RgbImage::from_pixel(w, h, Rgb(rgb))
}

/// Crop `w` x `h` starting at (x0, y0).
pub fn crop(img: &RgbImage, x0: u32, y0: u32, w: u32, h: u32) -> RgbImage {
    image::imageops::crop_imm(img, x0, y0, w, h).to_image()
}
