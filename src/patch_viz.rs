//! Pictures for the patch comparison window and its image export: difference heatmaps,
//! patch cut-outs and score fields as RGBA buffers. Pure functions, no GUI code, so they run
//! in worker threads (thumbnails, export) as well as for the detail view.

use crate::patch_matcher;
use crate::patch_store::PatchData;
use image::RgbImage;
use serde::{Deserialize, Serialize};

/// Colour photos of a pair with their brightness gains (linear light, per channel): the
/// original's difference view compares exposure-matched images (it
/// scales each image by 1 / (2^EV * exposure) and divides by its white balance).
pub type Raw<'a> = (&'a RgbImage, &'a RgbImage, [f32; 3], [f32; 3]);

/// Fixed heatmap scale: the original's difference view draws |A - B| amplified 4 times
/// (two additive passes that each double the result), so the fixed
/// scale shows a difference of 1/4 at full strength.
pub const FIXED_DIFF_GAIN: f32 = 4.0;

/// Largest master thumbnail (pixels); patches larger than this are reduced.
pub const THUMB_MASTER_PX: usize = 192;
/// Percentile used for the automatic heatmap scale.
pub const AUTO_SCALE_PERCENTILE: f32 = 0.99;
/// Stripe period of the hatching for invalid pixels (pixels of the picture).
const HATCH_PERIOD: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Colormap {
    Viridis,
    Magma,
    Jet,
}

impl Colormap {
    pub const ALL: [Colormap; 3] = [Colormap::Viridis, Colormap::Magma, Colormap::Jet];
    pub fn name(&self) -> &'static str {
        match self {
            Colormap::Viridis => "viridis",
            Colormap::Magma => "magma",
            Colormap::Jet => "jet",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum HeatMode {
    /// |difference| in grey, bright = big difference
    AbsGray,
    /// |difference| with a colour scale
    AbsColor(Colormap),
    /// A minus B, blue = B brighter, red = A brighter, white = 0
    Signed,
}

impl HeatMode {
    pub fn name(&self) -> String {
        match self {
            HeatMode::AbsGray => "Absolute, grey".into(),
            HeatMode::AbsColor(c) => format!("Absolute, colour ({})", c.name()),
            HeatMode::Signed => "Signed (A − B)".into(),
        }
    }
    pub fn all() -> Vec<HeatMode> {
        let mut v = vec![HeatMode::AbsGray];
        v.extend(Colormap::ALL.iter().map(|c| HeatMode::AbsColor(*c)));
        v.push(HeatMode::Signed);
        v
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum DiffSpace {
    /// the features the matching compares (brightness or edges, after the pre-blur)
    Feature,
    /// the visible photo colours
    RawColor,
}

/// A difference picture over the template window (n x n).
#[derive(Clone)]
pub struct DiffImage {
    pub n: usize,
    /// A - B (signed)
    pub signed: Vec<f32>,
    /// |A - B| (mean over colour channels in colour mode)
    pub abs: Vec<f32>,
    pub valid: Vec<bool>,
}

impl DiffImage {
    pub fn mean_abs(&self) -> Option<f32> {
        let (mut s, mut n) = (0.0f64, 0usize);
        for (v, ok) in self.abs.iter().zip(&self.valid) {
            if *ok {
                s += *v as f64;
                n += 1;
            }
        }
        if n == 0 { None } else { Some((s / n as f64) as f32) }
    }
}

/// Bicubic sample of a masked grid (None when a needed pixel is invalid).
pub fn cubic(v: &[f32], m: &[bool], s: usize, x: f32, y: f32) -> Option<f32> {
    let (x0, y0) = (x.floor() as isize, y.floor() as isize);
    let (tx, ty) = (x - x0 as f32, y - y0 as f32);
    if x0 < 1 || y0 < 1 || x0 + 2 >= s as isize || y0 + 2 >= s as isize {
        return None;
    }
    let w = |t: f32| -> [f32; 4] {
        // Catmull-Rom
        let t2 = t * t;
        let t3 = t2 * t;
        [-0.5 * t3 + t2 - 0.5 * t, 1.5 * t3 - 2.5 * t2 + 1.0, -1.5 * t3 + 2.0 * t2 + 0.5 * t, 0.5 * t3 - 0.5 * t2]
    };
    let (wx, wy) = (w(tx), w(ty));
    let mut acc = 0.0;
    for j in 0..4 {
        for i in 0..4 {
            let idx = (y0 + j as isize - 1) as usize * s + (x0 + i as isize - 1) as usize;
            if !m[idx] {
                return None;
            }
            acc += v[idx] * wx[i] * wy[j];
        }
    }
    Some(acc)
}

/// Bilinear colour sample of an image at working-image pixel p (scaled to the image).
fn sample_rgb(img: &RgbImage, x: f64, y: f64, gain: [f32; 3]) -> Option<[f32; 3]> {
    let (w, h) = (img.width() as f64, img.height() as f64);
    if x < 0.0 || y < 0.0 || x > w - 1.0 || y > h - 1.0 {
        return None;
    }
    let c = crate::imgutil::sample_rgb_linear(img, x as f32, y as f32);
    let f = |k: usize| crate::imgutil::linear_to_srgb((c[k] * gain[k]).clamp(0.0, 1.0));
    Some([f(0), f(1), f(2)])
}

/// Template-window pictures of A and B (B shifted by `shift`; None = as it lies now).
/// Feature space: the stored cut-outs; colour: sampled again from the working photos.
pub struct PatchPair {
    pub n: usize,
    /// RGB 0..1 (grey repeated in feature space)
    pub a: Vec<[f32; 3]>,
    pub b: Vec<[f32; 3]>,
    pub va: Vec<bool>,
    pub vb: Vec<bool>,
    /// the values the difference is computed on (feature or luma)
    pub fa: Vec<f32>,
    pub fb: Vec<f32>,
}

pub fn patch_pair(d: &PatchData, shift: (f32, f32), space: DiffSpace, rgb: Option<Raw>, grey_images: bool) -> Option<PatchPair> {
    let h = d.half() as isize;
    let n = (2 * h + 1) as usize;
    match (space, rgb) {
        (DiffSpace::RawColor, Some((ia, ib, ga, gb))) => {
            let mut p = PatchPair { n, a: vec![[0.0; 3]; n * n], b: vec![[0.0; 3]; n * n], va: vec![false; n * n], vb: vec![false; n * n], fa: vec![0.0; n * n], fb: vec![0.0; n * n] };
            let sa = (ia.width() as f64 / d.geo_a.w, ia.height() as f64 / d.geo_a.h);
            let sb = (ib.width() as f64 / d.geo_b.w, ib.height() as f64 / d.geo_b.h);
            for y in -h..=h {
                for x in -h..=h {
                    let i = (y + h) as usize * n + (x + h) as usize;
                    let da = d.plane.dir(x as f64, y as f64);
                    if let Some(q) = d.geo_a.dir_to_px(da).filter(|q| d.geo_a.inside(*q, 0.0)) {
                        if let Some(c) = sample_rgb(ia, q.0 * sa.0, q.1 * sa.1, ga) {
                            p.a[i] = c;
                            p.va[i] = true;
                            p.fa[i] = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
                        }
                    }
                    let db = d.plane.dir(x as f64 + shift.0 as f64, y as f64 + shift.1 as f64);
                    if let Some(q) = d.geo_b.dir_to_px(db).filter(|q| d.geo_b.inside(*q, 0.0)) {
                        if let Some(c) = sample_rgb(ib, q.0 * sb.0, q.1 * sb.1, gb) {
                            p.b[i] = c;
                            p.vb[i] = true;
                            p.fb[i] = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
                        }
                    }
                }
            }
            Some(p)
        }
        _ => {
            let ra = d.region_a();
            let rb = d.region_b()?;
            let s = rb.size;
            let (fa, ma) = if grey_images { (ra.data.clone(), ra.mask.clone()) } else { patch_matcher::feature_plane(&ra, &d.cfg) };
            let (fb, mb) = if grey_images { (rb.data.clone(), rb.mask.clone()) } else { patch_matcher::feature_plane(&rb, &d.cfg) };
            let c = (s / 2) as isize;
            let mut p = PatchPair { n, a: vec![[0.0; 3]; n * n], b: vec![[0.0; 3]; n * n], va: vec![false; n * n], vb: vec![false; n * n], fa: vec![0.0; n * n], fb: vec![0.0; n * n] };
            for y in -h..=h {
                for x in -h..=h {
                    let i = (y + h) as usize * n + (x + h) as usize;
                    let (ax, ay) = (c + x, c + y);
                    if ax >= 0 && ay >= 0 && (ax as usize) < s && (ay as usize) < s {
                        let j = ay as usize * s + ax as usize;
                        if ma[j] {
                            p.fa[i] = fa[j];
                            p.va[i] = true;
                        }
                    }
                    let (bx, by) = ((c + x) as f32 + shift.0, (c + y) as f32 + shift.1);
                    let v = if shift.0.fract() == 0.0 && shift.1.fract() == 0.0 {
                        let (ix, iy) = (bx as isize, by as isize);
                        if ix >= 0 && iy >= 0 && (ix as usize) < s && (iy as usize) < s && mb[iy as usize * s + ix as usize] {
                            Some(fb[iy as usize * s + ix as usize])
                        } else {
                            None
                        }
                    } else {
                        // sub-pixel shifts are shown with bicubic interpolation
                        cubic(&fb, &mb, s, bx, by)
                    };
                    if let Some(v) = v {
                        p.fb[i] = v;
                        p.vb[i] = true;
                    }
                }
            }
            for i in 0..n * n {
                p.a[i] = [p.fa[i]; 3];
                p.b[i] = [p.fb[i]; 3];
            }
            Some(p)
        }
    }
}

/// Difference picture: A minus B (B shifted by `shift`).
pub fn diff(d: &PatchData, shift: (f32, f32), space: DiffSpace, rgb: Option<Raw>) -> Option<DiffImage> {
    let p = patch_pair(d, shift, space, rgb, false)?;
    let n = p.n;
    let mut out = DiffImage { n, signed: vec![0.0; n * n], abs: vec![0.0; n * n], valid: vec![false; n * n] };
    let colour = space == DiffSpace::RawColor && rgb.is_some();
    for i in 0..n * n {
        if p.va[i] && p.vb[i] {
            out.valid[i] = true;
            out.signed[i] = p.fa[i] - p.fb[i];
            out.abs[i] = if colour { (0..3).map(|c| (p.a[i][c] - p.b[i][c]).abs()).sum::<f32>() / 3.0 } else { (p.fa[i] - p.fb[i]).abs() };
        }
    }
    Some(out)
}

/// Shared scale of several difference pictures: 99th percentile (auto) or the fixed range.
pub fn shared_scale(imgs: &[&DiffImage], auto: bool, signed: bool) -> f32 {
    if !auto {
        return 1.0 / FIXED_DIFF_GAIN;
    }
    let mut v: Vec<f32> = Vec::new();
    for im in imgs {
        for (i, ok) in im.valid.iter().enumerate() {
            if *ok {
                v.push(if signed { im.signed[i].abs() } else { im.abs[i] });
            }
        }
    }
    if v.is_empty() {
        return 1.0;
    }
    let k = ((v.len() as f32 - 1.0) * AUTO_SCALE_PERCENTILE) as usize;
    v.select_nth_unstable_by(k, |a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[k].max(1e-6)
}

fn poly(t: f32, c: [[f32; 3]; 7]) -> [f32; 3] {
    let mut r = [0.0; 3];
    for k in 0..3 {
        let mut acc = c[6][k];
        for j in (0..6).rev() {
            acc = acc * t + c[j][k];
        }
        r[k] = acc.clamp(0.0, 1.0);
    }
    r
}

/// Colour of t in 0..1 for a colour map (polynomial fits of the published maps).
pub fn colormap(cm: Colormap, t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    match cm {
        Colormap::Viridis => poly(
            t,
            [
                [0.277_727_3, 0.005_407_344, 0.334_099_8],
                [0.105_093_04, 1.404_613_5, 1.384_590_1],
                [-0.330_861_8, 0.214_847_56, 0.095_095_165],
                [-4.634_230_5, -5.799_101, -19.332_441],
                [6.228_27, 14.179_934, 56.690_55],
                [4.776_385, -13.745_146, -65.353_035],
                [-5.435_456, 4.645_852_6, 26.312_435],
            ],
        ),
        Colormap::Magma => poly(
            t,
            [
                [-0.002_136_485, -0.000_749_655, -0.005_386_127],
                [0.251_660_5, 0.677_523_2, 2.494_026_6],
                [8.353_717, -3.577_719_5, 0.314_467_9],
                [-27.668_734, 14.264_73, -13.649_213],
                [52.176_14, -27.943_605, 12.944_169],
                [-50.768_524, 29.046_58, 4.234_153],
                [18.655_705, -11.489_774, -5.601_961_4],
            ],
        ),
        Colormap::Jet => {
            let f = |x: f32| (1.5 - (4.0 * t - x).abs()).clamp(0.0, 1.0);
            [f(3.0), f(2.0), f(1.0)]
        }
    }
}

/// Diverging blue - white - red for -1..1.
pub fn diverging(t: f32) -> [f32; 3] {
    let t = t.clamp(-1.0, 1.0);
    if t >= 0.0 { [1.0, 1.0 - t * 0.85, 1.0 - t * 0.9] } else { [1.0 + t * 0.9, 1.0 + t * 0.6, 1.0] }
}

/// Colour of one difference value in a mode (scale = value shown at full strength).
pub fn heat_colour(mode: HeatMode, abs: f32, signed: f32, scale: f32) -> [f32; 3] {
    match mode {
        HeatMode::AbsGray => [(abs / scale).clamp(0.0, 1.0); 3],
        HeatMode::AbsColor(cm) => colormap(cm, abs / scale),
        HeatMode::Signed => diverging(signed / scale),
    }
}

fn hatch(x: usize, y: usize) -> [u8; 4] {
    // invalid pixels: grey stripes on transparent (never black, black looks like a difference)
    if (x + y) % HATCH_PERIOD < HATCH_PERIOD / 2 { [140, 140, 150, 200] } else { [0, 0, 0, 0] }
}

fn to8(c: [f32; 3]) -> [u8; 4] {
    [(c[0] * 255.0).round() as u8, (c[1] * 255.0).round() as u8, (c[2] * 255.0).round() as u8, 255]
}

/// RGBA picture of a difference image, `zoom` pixels per value (nearest neighbour).
pub fn heat_rgba(img: &DiffImage, mode: HeatMode, scale: f32, zoom: usize) -> (usize, Vec<u8>) {
    let z = zoom.max(1);
    let w = img.n * z;
    let mut out = vec![0u8; w * w * 4];
    for y in 0..w {
        for x in 0..w {
            let i = (y / z) * img.n + x / z;
            let px = if img.valid[i] { to8(heat_colour(mode, img.abs[i], img.signed[i], scale)) } else { hatch(x, y) };
            out[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&px);
        }
    }
    (w, out)
}

/// RGBA picture of patch pixels (grey or colour), invalid pixels hatched.
pub fn patch_rgba(v: &[[f32; 3]], valid: &[bool], n: usize, zoom: usize, normalise: bool) -> (usize, Vec<u8>) {
    let z = zoom.max(1);
    let w = n * z;
    // feature planes (edges) are small numbers: stretch them for display
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    if normalise {
        for (c, ok) in v.iter().zip(valid) {
            if *ok {
                lo = lo.min(c[0]);
                hi = hi.max(c[0]);
            }
        }
    }
    let span = if normalise && hi > lo { hi - lo } else { 1.0 };
    let base = if normalise && hi > lo { lo } else { 0.0 };
    let mut out = vec![0u8; w * w * 4];
    for y in 0..w {
        for x in 0..w {
            let i = (y / z) * n + x / z;
            let px = if valid[i] {
                let c = v[i];
                to8([((c[0] - base) / span).clamp(0.0, 1.0), ((c[1] - base) / span).clamp(0.0, 1.0), ((c[2] - base) / span).clamp(0.0, 1.0)])
            } else {
                hatch(x, y)
            };
            out[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&px);
        }
    }
    (w, out)
}

/// RGBA picture of a score field (viridis; untested shifts hatched), `zoom` pixels per shift.
pub fn score_rgba(map: &[f32], n: usize, zoom: usize) -> (usize, Vec<u8>, f32, f32) {
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for v in map.iter().filter(|v| v.is_finite()) {
        lo = lo.min(*v);
        hi = hi.max(*v);
    }
    if lo > hi {
        lo = 0.0;
        hi = 1.0;
    }
    let span = (hi - lo).max(1e-6);
    let z = zoom.max(1);
    let w = n * z;
    let mut out = vec![0u8; w * w * 4];
    for y in 0..w {
        for x in 0..w {
            let v = map[(y / z) * n + x / z];
            let px = if v.is_finite() { to8(colormap(Colormap::Viridis, (v - lo) / span)) } else { hatch(x, y) };
            out[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&px);
        }
    }
    (w, out, lo, hi)
}

/// Best and second-best peak of a score field (second outside a 5x5 neighbourhood of the best).
pub fn peaks(map: &[f32], n: usize) -> (Option<(usize, usize, f32)>, Option<(usize, usize, f32)>) {
    let mut best: Option<(usize, usize, f32)> = None;
    for y in 0..n {
        for x in 0..n {
            let v = map[y * n + x];
            if v.is_finite() && best.map(|b| v > b.2).unwrap_or(true) {
                best = Some((x, y, v));
            }
        }
    }
    let Some(b) = best else { return (None, None) };
    let mut second: Option<(usize, usize, f32)> = None;
    for y in 0..n {
        for x in 0..n {
            let v = map[y * n + x];
            if !v.is_finite() || ((x as isize - b.0 as isize).abs() <= 2 && (y as isize - b.1 as isize).abs() <= 2) {
                continue;
            }
            // only local maxima count as peaks
            let mut is_max = true;
            for (ox, oy) in [(-1isize, 0isize), (1, 0), (0, -1), (0, 1)] {
                let (xx, yy) = (x as isize + ox, y as isize + oy);
                if xx >= 0 && yy >= 0 && (xx as usize) < n && (yy as usize) < n {
                    let w = map[yy as usize * n + xx as usize];
                    if w.is_finite() && w > v {
                        is_max = false;
                    }
                }
            }
            if is_max && second.map(|s| v > s.2).unwrap_or(true) {
                second = Some((x, y, v));
            }
        }
    }
    (best, second)
}

/// Master thumbnail of a patch: the difference after the correction (before it when the patch
/// was rejected). Returns (size, RGBA, rejected marker needed).
pub fn thumbnail(d: &PatchData, shift: Option<(f32, f32)>, mode: HeatMode, space: DiffSpace, auto: bool, rgb: Option<Raw>) -> Option<(usize, Vec<u8>)> {
    let img = diff(d, shift.unwrap_or((0.0, 0.0)), space, rgb)?;
    let scale = shared_scale(&[&img], auto, mode == HeatMode::Signed);
    let (w, mut px) = heat_rgba(&img, mode, scale, 1);
    if w > THUMB_MASTER_PX {
        // reduce by taking every k-th value
        let k = w.div_ceil(THUMB_MASTER_PX);
        let w2 = w / k;
        let mut o = vec![0u8; w2 * w2 * 4];
        for y in 0..w2 {
            for x in 0..w2 {
                let s = ((y * k) * w + x * k) * 4;
                o[(y * w2 + x) * 4..(y * w2 + x) * 4 + 4].copy_from_slice(&px[s..s + 4]);
            }
        }
        px = o;
        return Some((w2, px));
    }
    Some((w, px))
}
