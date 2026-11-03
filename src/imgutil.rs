//! Small image helpers: float grey images, blur, resampling, loading (JPEG/PNG/TIFF/RAW).

use image::{Rgb, RgbImage};
use std::path::Path;
use std::sync::OnceLock;

#[derive(Clone)]
pub struct Gray {
    pub w: usize,
    pub h: usize,
    pub data: Vec<f32>,
}

impl Gray {
    pub fn new(w: usize, h: usize) -> Self {
        Self { w, h, data: vec![0.0; w * h] }
    }
    #[inline]
    pub fn at(&self, x: usize, y: usize) -> f32 {
        self.data[y * self.w + x]
    }
    #[inline]
    pub fn at_clamped(&self, x: isize, y: isize) -> f32 {
        let x = x.clamp(0, self.w as isize - 1) as usize;
        let y = y.clamp(0, self.h as isize - 1) as usize;
        self.data[y * self.w + x]
    }
    /// Bilinear sample with clamped borders.
    #[inline]
    pub fn sample(&self, x: f32, y: f32) -> f32 {
        let x0 = x.floor();
        let y0 = y.floor();
        let fx = x - x0;
        let fy = y - y0;
        let xi = x0 as isize;
        let yi = y0 as isize;
        let a = self.at_clamped(xi, yi);
        let b = self.at_clamped(xi + 1, yi);
        let c = self.at_clamped(xi, yi + 1);
        let d = self.at_clamped(xi + 1, yi + 1);
        (a * (1.0 - fx) + b * fx) * (1.0 - fy) + (c * (1.0 - fx) + d * fx) * fy
    }
}

pub fn to_gray(img: &RgbImage) -> Gray {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut g = Gray::new(w, h);
    for (i, p) in img.pixels().enumerate() {
        g.data[i] = (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) / 255.0;
    }
    g
}

fn gauss_kernel(sigma: f32) -> Vec<f32> {
    let r = (sigma * 3.0).ceil().max(1.0) as isize;
    let mut k: Vec<f32> = (-r..=r).map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let s: f32 = k.iter().sum();
    k.iter_mut().for_each(|v| *v /= s);
    k
}

pub fn blur(src: &Gray, sigma: f32) -> Gray {
    if sigma <= 0.01 {
        return src.clone();
    }
    let k = gauss_kernel(sigma);
    let r = (k.len() / 2) as isize;
    let (w, h) = (src.w, src.h);
    let mut tmp = Gray::new(w, h);
    for y in 0..h {
        let row = &src.data[y * w..(y + 1) * w];
        for x in 0..w {
            let mut s = 0.0;
            for (i, kv) in k.iter().enumerate() {
                let xx = (x as isize + i as isize - r).clamp(0, w as isize - 1) as usize;
                s += kv * row[xx];
            }
            tmp.data[y * w + x] = s;
        }
    }
    let mut out = Gray::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let mut s = 0.0;
            for (i, kv) in k.iter().enumerate() {
                let yy = (y as isize + i as isize - r).clamp(0, h as isize - 1) as usize;
                s += kv * tmp.data[yy * w + x];
            }
            out.data[y * w + x] = s;
        }
    }
    out
}

/// Resize a grey image by `scale` (< 1) with pre-blur.
pub fn resize_gray(src: &Gray, scale: f32) -> Gray {
    let nw = ((src.w as f32 * scale).round() as usize).max(1);
    let nh = ((src.h as f32 * scale).round() as usize).max(1);
    let pre = if scale < 1.0 { blur(src, 0.5 / scale - 0.4) } else { src.clone() };
    let mut out = Gray::new(nw, nh);
    let sx = src.w as f32 / nw as f32;
    let sy = src.h as f32 / nh as f32;
    for y in 0..nh {
        for x in 0..nw {
            out.data[y * nw + x] = pre.sample((x as f32 + 0.5) * sx - 0.5, (y as f32 + 0.5) * sy - 0.5);
        }
    }
    out
}

/// sRGB byte -> linear light lookup table.
pub fn srgb_to_linear_lut() -> &'static [f32; 256] {
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0.0f32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let c = i as f32 / 255.0;
            *v = if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
        }
        t
    })
}

#[inline]
pub fn linear_to_srgb(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

/// Bilinear sample of an RGB byte image in linear light. x,y in pixel coordinates (centre of pixel 0 = 0.0).
#[inline]
pub fn sample_rgb_linear(img: &RgbImage, x: f32, y: f32) -> [f32; 3] {
    let lut = srgb_to_linear_lut();
    let w = img.width() as isize;
    let h = img.height() as isize;
    let x0 = x.floor();
    let y0 = y.floor();
    let fx = x - x0;
    let fy = y - y0;
    let xi = x0 as isize;
    let yi = y0 as isize;
    let raw = img.as_raw();
    let get = |xx: isize, yy: isize| -> [f32; 3] {
        let xx = xx.clamp(0, w - 1) as usize;
        let yy = yy.clamp(0, h - 1) as usize;
        let o = (yy * w as usize + xx) * 3;
        [lut[raw[o] as usize], lut[raw[o + 1] as usize], lut[raw[o + 2] as usize]]
    };
    let a = get(xi, yi);
    let b = get(xi + 1, yi);
    let c = get(xi, yi + 1);
    let d = get(xi + 1, yi + 1);
    let mut out = [0.0; 3];
    for k in 0..3 {
        out[k] = (a[k] * (1.0 - fx) + b[k] * fx) * (1.0 - fy) + (c[k] * (1.0 - fx) + d[k] * fx) * fy;
    }
    out
}

/// Bicubic (Catmull-Rom) sample in linear light, for a sharper final render.
#[inline]
pub fn sample_rgb_linear_cubic(img: &RgbImage, x: f32, y: f32) -> [f32; 3] {
    let lut = srgb_to_linear_lut();
    let w = img.width() as isize;
    let h = img.height() as isize;
    let raw = img.as_raw();
    let x0 = x.floor();
    let y0 = y.floor();
    let tx = x - x0;
    let ty = y - y0;
    let cr = |t: f32| -> [f32; 4] {
        let t2 = t * t;
        let t3 = t2 * t;
        [-0.5 * t3 + t2 - 0.5 * t, 1.5 * t3 - 2.5 * t2 + 1.0, -1.5 * t3 + 2.0 * t2 + 0.5 * t, 0.5 * t3 - 0.5 * t2]
    };
    let (wx, wy) = (cr(tx), cr(ty));
    let mut out = [0.0f32; 3];
    for (j, wyj) in wy.iter().enumerate() {
        let yy = (y0 as isize + j as isize - 1).clamp(0, h - 1) as usize;
        for (i, wxi) in wx.iter().enumerate() {
            let xx = (x0 as isize + i as isize - 1).clamp(0, w - 1) as usize;
            let o = (yy * w as usize + xx) * 3;
            let k = wxi * wyj;
            out[0] += k * lut[raw[o] as usize];
            out[1] += k * lut[raw[o + 1] as usize];
            out[2] += k * lut[raw[o + 2] as usize];
        }
    }
    [out[0].max(0.0), out[1].max(0.0), out[2].max(0.0)]
}

pub fn downscale_rgb(img: &RgbImage, max_side: u32) -> RgbImage {
    let long = img.width().max(img.height());
    if long <= max_side {
        return img.clone();
    }
    let s = max_side as f32 / long as f32;
    let nw = ((img.width() as f32 * s).round() as u32).max(1);
    let nh = ((img.height() as f32 * s).round() as u32).max(1);
    image::imageops::resize(img, nw, nh, image::imageops::FilterType::Triangle)
}

pub const RAW_EXTS: &[&str] = &[
    "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "orf", "rw2", "raf", "dng", "pef", "srw", "3fr",
    "erf", "kdc", "mef", "mos", "mrw", "x3f", "iiq", "rwl",
];
pub const STD_EXTS: &[&str] = &["jpg", "jpeg", "png", "tif", "tiff", "bmp", "webp"];

pub fn is_raw(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| RAW_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            RAW_EXTS.contains(&e.as_str()) || STD_EXTS.contains(&e.as_str())
        })
        .unwrap_or(false)
}

/// Load any supported photo as 8-bit sRGB.
pub fn load_photo(path: &Path) -> Result<RgbImage, String> {
    if is_raw(path) {
        let img = imagepipe::simple_decode_8bit(path, 0, 0)?;
        RgbImage::from_raw(img.width as u32, img.height as u32, img.data)
            .ok_or_else(|| "RAW decoder returned an unexpected image size".to_string())
    } else {
        use image::ImageDecoder;
        let reader = image::ImageReader::open(path).map_err(|e| e.to_string())?.with_guessed_format().map_err(|e| e.to_string())?;
        let mut dec = reader.into_decoder().map_err(|e| e.to_string())?;
        let orient = dec.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
        let mut img = image::DynamicImage::from_decoder(dec).map_err(|e| e.to_string())?;
        img.apply_orientation(orient);
        Ok(img.to_rgb8())
    }
}

/// Information from the photo's EXIF block.
#[derive(Clone, Debug, Default)]
pub struct PhotoInfo {
    pub focal_mm: Option<f64>,
    pub focal_35mm: Option<f64>,
    pub exposure_time: Option<f64>,
    pub f_number: Option<f64>,
    pub iso: Option<f64>,
    pub camera: Option<String>,
}

impl PhotoInfo {
    /// Exposure value (relative brightness), larger = brighter image.
    pub fn brightness_ev(&self) -> Option<f64> {
        let t = self.exposure_time?;
        let n = self.f_number.unwrap_or(8.0);
        let iso = self.iso.unwrap_or(100.0);
        Some((t * iso / 100.0 / (n * n)).log2())
    }
}

pub fn read_info(path: &Path) -> PhotoInfo {
    let mut info = PhotoInfo::default();
    let Ok(file) = std::fs::File::open(path) else { return info };
    let mut reader = std::io::BufReader::new(file);
    let Ok(ex) = exif::Reader::new().read_from_container(&mut reader) else { return info };
    let rational = |tag: exif::Tag| -> Option<f64> {
        let f = ex.get_field(tag, exif::In::PRIMARY)?;
        match &f.value {
            exif::Value::Rational(v) if !v.is_empty() => Some(v[0].to_f64()),
            exif::Value::SRational(v) if !v.is_empty() => Some(v[0].to_f64()),
            _ => f.value.get_uint(0).map(|u| u as f64),
        }
    };
    info.focal_mm = rational(exif::Tag::FocalLength).filter(|v| *v > 0.0);
    info.focal_35mm = rational(exif::Tag::FocalLengthIn35mmFilm).filter(|v| *v > 0.0);
    info.exposure_time = rational(exif::Tag::ExposureTime).filter(|v| *v > 0.0);
    info.f_number = rational(exif::Tag::FNumber).filter(|v| *v > 0.0);
    info.iso = rational(exif::Tag::PhotographicSensitivity).filter(|v| *v > 0.0);
    if let Some(f) = ex.get_field(exif::Tag::Model, exif::In::PRIMARY) {
        info.camera = Some(f.display_value().to_string().trim_matches('"').to_string());
    }
    info
}

#[allow(dead_code)]
pub fn put(img: &mut RgbImage, x: u32, y: u32, c: [f32; 3]) {
    img.put_pixel(
        x,
        y,
        Rgb([
            (c[0].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
            (c[1].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
            (c[2].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
        ]),
    );
}
