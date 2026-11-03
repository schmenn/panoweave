//! Exposure fusion: merge a bracketed series into one well-exposed picture
//! (weights for contrast, saturation and good exposure, blended with Laplacian pyramids).

use crate::features;
use crate::geometry::{h_apply, mul33};
use crate::imgutil::{downscale_rgb, to_gray};
use image::RgbImage;
use rayon::prelude::*;

#[derive(Clone, Copy, Debug)]
pub struct FusionWeights {
    pub contrast: f32,
    pub saturation: f32,
    pub exposure: f32,
}

impl Default for FusionWeights {
    fn default() -> Self {
        FusionWeights { contrast: 1.0, saturation: 1.0, exposure: 1.0 }
    }
}

type Px = [f32; 3];

fn to_f32(img: &RgbImage) -> Vec<Px> {
    img.pixels().map(|p| [p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0]).collect()
}

fn weight_map(img: &[Px], w: usize, h: usize, fw: &FusionWeights) -> Vec<f32> {
    let gray: Vec<f32> = img.iter().map(|p| 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2]).collect();
    (0..w * h)
        .into_par_iter()
        .map(|i| {
            let x = i % w;
            let y = i / w;
            let g = |xx: usize, yy: usize| gray[yy * w + xx];
            let lap = if x > 0 && y > 0 && x + 1 < w && y + 1 < h {
                (4.0 * g(x, y) - g(x - 1, y) - g(x + 1, y) - g(x, y - 1) - g(x, y + 1)).abs()
            } else {
                0.0
            };
            let p = img[i];
            let mu = (p[0] + p[1] + p[2]) / 3.0;
            let sat = (((p[0] - mu).powi(2) + (p[1] - mu).powi(2) + (p[2] - mu).powi(2)) / 3.0).sqrt();
            let e = |v: f32| (-(v - 0.5).powi(2) / (2.0 * 0.2 * 0.2)).exp();
            let exp = e(p[0]) * e(p[1]) * e(p[2]);
            (lap + 1e-3).powf(fw.contrast) * (sat + 1e-3).powf(fw.saturation) * (exp + 1e-6).powf(fw.exposure) + 1e-12
        })
        .collect()
}

fn down(src: &[[f32; 4]], w: usize, h: usize) -> (Vec<[f32; 4]>, usize, usize) {
    let nw = w.div_ceil(2);
    let nh = h.div_ceil(2);
    let k = [1.0f32, 4.0, 6.0, 4.0, 1.0];
    let mut tmp = vec![[0.0f32; 4]; nw * h];
    tmp.par_chunks_mut(nw).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            for (t, kv) in k.iter().enumerate() {
                let xx = (2 * x as isize + t as isize - 2).clamp(0, w as isize - 1) as usize;
                let v = src[y * w + xx];
                for c in 0..4 {
                    o[c] += kv * v[c] / 16.0;
                }
            }
        }
    });
    let mut out = vec![[0.0f32; 4]; nw * nh];
    out.par_chunks_mut(nw).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            for (t, kv) in k.iter().enumerate() {
                let yy = (2 * y as isize + t as isize - 2).clamp(0, h as isize - 1) as usize;
                let v = tmp[yy * nw + x];
                for c in 0..4 {
                    o[c] += kv * v[c] / 16.0;
                }
            }
        }
    });
    (out, nw, nh)
}

fn up(src: &[[f32; 4]], sw: usize, sh: usize, w: usize, h: usize) -> Vec<[f32; 4]> {
    let mut out = vec![[0.0f32; 4]; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let fy = ((y as f32 + 0.5) / 2.0 - 0.5).max(0.0);
        let y0 = (fy as usize).min(sh - 1);
        let y1 = (y0 + 1).min(sh - 1);
        let ty = fy - y0 as f32;
        for (x, o) in row.iter_mut().enumerate() {
            let fx = ((x as f32 + 0.5) / 2.0 - 0.5).max(0.0);
            let x0 = (fx as usize).min(sw - 1);
            let x1 = (x0 + 1).min(sw - 1);
            let tx = fx - x0 as f32;
            for c in 0..4 {
                let a = src[y0 * sw + x0][c] * (1.0 - tx) + src[y0 * sw + x1][c] * tx;
                let b = src[y1 * sw + x0][c] * (1.0 - tx) + src[y1 * sw + x1][c] * tx;
                o[c] = a * (1.0 - ty) + b * ty;
            }
        }
    });
    out
}

/// Warp `src` onto the frame of a reference image with homography `h_ref_to_src`.
fn warp(src: &RgbImage, h_ref_to_src: &[f64; 9], w: u32, h: u32) -> RgbImage {
    let mut out = RgbImage::new(w, h);
    let raw = src.as_raw();
    let (sw, sh) = (src.width() as i64, src.height() as i64);
    out.par_chunks_mut(w as usize * 3).enumerate().for_each(|(y, row)| {
        for x in 0..w as usize {
            let (sx, sy) = h_apply(h_ref_to_src, x as f64, y as f64);
            let x0 = sx.floor() as i64;
            let y0 = sy.floor() as i64;
            let tx = (sx - x0 as f64) as f32;
            let ty = (sy - y0 as f64) as f32;
            let get = |xx: i64, yy: i64, c: usize| -> f32 {
                let xx = xx.clamp(0, sw - 1);
                let yy = yy.clamp(0, sh - 1);
                raw[((yy * sw + xx) * 3) as usize + c] as f32
            };
            for c in 0..3 {
                let a = get(x0, y0, c) * (1.0 - tx) + get(x0 + 1, y0, c) * tx;
                let b = get(x0, y0 + 1, c) * (1.0 - tx) + get(x0 + 1, y0 + 1, c) * tx;
                row[x * 3 + c] = (a * (1.0 - ty) + b * ty).round().clamp(0.0, 255.0) as u8;
            }
        }
    });
    out
}

/// Align all images to the middle one (handheld brackets). Returns aligned copies.
pub fn align_stack(images: &[RgbImage]) -> Vec<RgbImage> {
    if images.len() < 2 {
        return images.to_vec();
    }
    let r = images.len() / 2;
    let (w, h) = (images[r].width(), images[r].height());
    let work = 1400u32;
    let feats: Vec<(features::Features, f64)> = images
        .par_iter()
        .map(|im| {
            let small = downscale_rgb(im, work);
            let s = small.width() as f64 / im.width() as f64;
            let mut g = to_gray(&small);
            // equalise brightness so dark and bright frames describe alike
            let mean = g.data.iter().sum::<f32>() / g.data.len() as f32;
            let gain = 0.45 / mean.max(0.01);
            g.data.iter_mut().for_each(|v| *v = (*v * gain).min(1.0));
            (features::detect(&g, 3000), s)
        })
        .collect();
    images
        .par_iter()
        .enumerate()
        .map(|(i, im)| {
            if i == r {
                return im.clone();
            }
            let (fr, sr) = &feats[r];
            let (fi, si) = &feats[i];
            if let Some(pm) = features::match_pair(fr, fi, 400, 7 + i as u64) {
                // small_ref -> small_i ; full_ref -> small_ref -> small_i -> full_i
                let s_ref = [*sr, 0.0, 0.0, 0.0, *sr, 0.0, 0.0, 0.0, 1.0];
                let s_i_inv = [1.0 / *si, 0.0, 0.0, 0.0, 1.0 / *si, 0.0, 0.0, 0.0, 1.0];
                let hfull = mul33(&s_i_inv, &mul33(&pm.h, &s_ref));
                warp(im, &hfull, w, h)
            } else {
                image::imageops::resize(im, w, h, image::imageops::FilterType::Triangle)
            }
        })
        .collect()
}

/// Fuse a stack of same-size images.
pub fn fuse(images: &[RgbImage], fw: &FusionWeights) -> RgbImage {
    if images.is_empty() {
        return RgbImage::new(1, 1);
    }
    let (w, h) = (images[0].width() as usize, images[0].height() as usize);
    let imgs: Vec<Vec<Px>> = images
        .par_iter()
        .map(|im| {
            if im.width() as usize == w && im.height() as usize == h {
                to_f32(im)
            } else {
                to_f32(&image::imageops::resize(im, w as u32, h as u32, image::imageops::FilterType::Triangle))
            }
        })
        .collect();
    let weights: Vec<Vec<f32>> = imgs.par_iter().map(|im| weight_map(im, w, h, fw)).collect();
    // normalise
    let mut sums = vec![0.0f32; w * h];
    for wm in &weights {
        for (s, v) in sums.iter_mut().zip(wm) {
            *s += v;
        }
    }
    let mut levels = 1;
    while levels < 9 && (w.min(h) >> (levels + 1)) > 4 {
        levels += 1;
    }
    let mut acc: Vec<(Vec<[f32; 4]>, usize, usize)> = Vec::new();
    for (im, wm) in imgs.iter().zip(&weights) {
        let base: Vec<[f32; 4]> = im.iter().zip(wm.iter().zip(&sums)).map(|(p, (v, s))| [p[0], p[1], p[2], v / s]).collect();
        let mut g = vec![(base, w, h)];
        for _ in 0..levels {
            let (p, pw, ph) = g.last().unwrap();
            let n = down(p, *pw, *ph);
            g.push(n);
        }
        for l in 0..=levels {
            let (gl, gw, gh) = &g[l];
            let band: Vec<[f32; 4]> = if l < levels {
                let (n, nw, nh) = &g[l + 1];
                let u = up(n, *nw, *nh, *gw, *gh);
                gl.iter().zip(&u).map(|(a, b)| [(a[0] - b[0]) * a[3], (a[1] - b[1]) * a[3], (a[2] - b[2]) * a[3], a[3]]).collect()
            } else {
                gl.iter().map(|a| [a[0] * a[3], a[1] * a[3], a[2] * a[3], a[3]]).collect()
            };
            if acc.len() <= l {
                acc.push((band, *gw, *gh));
            } else {
                for (d, s) in acc[l].0.iter_mut().zip(&band) {
                    for c in 0..4 {
                        d[c] += s[c];
                    }
                }
            }
        }
    }
    let mut cur = acc.pop().unwrap();
    while let Some((b, bw, bh)) = acc.pop() {
        let u = up(&cur.0, cur.1, cur.2, bw, bh);
        cur = (b.iter().zip(&u).map(|(l, u)| [l[0] + u[0], l[1] + u[1], l[2] + u[2], 1.0]).collect(), bw, bh);
    }
    let mut out = RgbImage::new(w as u32, h as u32);
    for (i, p) in out.pixels_mut().enumerate() {
        let v = cur.0[i];
        *p = image::Rgb([
            (v[0].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
            (v[1].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
            (v[2].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
        ]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{solid, texture};

    #[test]
    fn default_weights_are_one() {
        let w = FusionWeights::default();
        assert_eq!((w.contrast, w.saturation, w.exposure), (1.0, 1.0, 1.0));
    }

    #[test]
    fn empty_stack_gives_placeholder() {
        let out = fuse(&[], &FusionWeights::default());
        assert_eq!(out.dimensions(), (1, 1));
    }

    #[test]
    fn weights_are_positive_and_finite() {
        let img = texture(40, 30, 4);
        let px = to_f32(&img);
        let wm = weight_map(&px, 40, 30, &FusionWeights::default());
        assert_eq!(wm.len(), 40 * 30);
        assert!(wm.iter().all(|v| v.is_finite() && *v > 0.0));
    }

    #[test]
    fn well_exposed_pixels_weigh_more_than_clipped() {
        let mid = to_f32(&solid(8, 8, [128, 128, 128]));
        let white = to_f32(&solid(8, 8, [255, 255, 255]));
        let fw = FusionWeights::default();
        let wm = weight_map(&mid, 8, 8, &fw)[27];
        let ww = weight_map(&white, 8, 8, &fw)[27];
        assert!(wm > ww * 100.0);
        // switching the exposure term off (power 0) makes them equal
        let off = FusionWeights { exposure: 0.0, ..fw };
        let a = weight_map(&mid, 8, 8, &off)[27];
        let b = weight_map(&white, 8, 8, &off)[27];
        assert!((a - b).abs() < 1e-6);
    }

    #[test]
    fn single_image_is_reproduced() {
        let img = texture(64, 48, 5);
        let out = fuse(std::slice::from_ref(&img), &FusionWeights::default());
        assert_eq!(out.dimensions(), (64, 48));
        let max_diff = img.as_raw().iter().zip(out.as_raw()).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
        assert!(max_diff <= 2, "max diff {max_diff}");
    }

    #[test]
    fn identical_images_fuse_to_themselves() {
        let img = texture(64, 48, 6);
        let out = fuse(&[img.clone(), img.clone(), img.clone()], &FusionWeights::default());
        let max_diff = img.as_raw().iter().zip(out.as_raw()).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
        assert!(max_diff <= 2, "max diff {max_diff}");
    }

    #[test]
    fn fusion_stays_between_inputs_and_prefers_good_exposure() {
        // dark, middle and bright flat grey: the well-exposed one must dominate
        let stack = [solid(32, 32, [20, 20, 20]), solid(32, 32, [128, 128, 128]), solid(32, 32, [245, 245, 245])];
        let out = fuse(&stack, &FusionWeights::default());
        let v = out.get_pixel(16, 16)[0] as i32;
        assert!((v - 128).abs() < 25, "got {v}");
        assert!(out.pixels().all(|p| p[0] >= 15 && p[0] <= 250));
    }

    #[test]
    fn different_sizes_are_resized_to_first() {
        let a = texture(48, 32, 7);
        let b = texture(24, 16, 8);
        let out = fuse(&[a, b], &FusionWeights::default());
        assert_eq!(out.dimensions(), (48, 32));
    }

    #[test]
    fn pyramid_down_up_sizes_and_constants() {
        let src = vec![[0.5f32, 0.5, 0.5, 1.0]; 9 * 7];
        let (d, dw, dh) = down(&src, 9, 7);
        assert_eq!((dw, dh), (5, 4));
        assert!(d.iter().all(|p| (p[0] - 0.5).abs() < 1e-5));
        let u = up(&d, dw, dh, 9, 7);
        assert_eq!(u.len(), 63);
        assert!(u.iter().all(|p| (p[0] - 0.5).abs() < 1e-5));
    }

    #[test]
    fn warp_with_identity_keeps_image() {
        let img = texture(30, 20, 9);
        let id = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let out = warp(&img, &id, 30, 20);
        assert_eq!(out.as_raw(), img.as_raw());
    }

    #[test]
    fn warp_translation_shifts_content() {
        let img = texture(40, 20, 10);
        // reference pixel x samples source pixel x + 5
        let h = [1.0, 0.0, 5.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let out = warp(&img, &h, 30, 20);
        for x in 0..30 {
            assert_eq!(out.get_pixel(x, 7), img.get_pixel(x + 5, 7));
        }
    }
}
