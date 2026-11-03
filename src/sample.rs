//! Built-in sample photos: a procedurally generated landscape photographed from one spot
//! (for trying the app without own photos, and for self-tests).

use crate::align::{ray, Lens};
use crate::geometry::*;
use image::RgbImage;
use rayon::prelude::*;

fn hash3(x: i32, y: i32, z: i32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x8da6b343) ^ (y as u32).wrapping_mul(0xd8163841) ^ (z as u32).wrapping_mul(0xcb1ab31f);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1e995);
    h ^= h >> 15;
    (h & 0xffffff) as f32 / 0xffffff as f32
}

fn vnoise(p: [f32; 3]) -> f32 {
    let i = [p[0].floor(), p[1].floor(), p[2].floor()];
    let f = [p[0] - i[0], p[1] - i[1], p[2] - i[2]];
    let s = [f[0] * f[0] * (3.0 - 2.0 * f[0]), f[1] * f[1] * (3.0 - 2.0 * f[1]), f[2] * f[2] * (3.0 - 2.0 * f[2])];
    let (x, y, z) = (i[0] as i32, i[1] as i32, i[2] as i32);
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let c = |dx, dy, dz| hash3(x + dx, y + dy, z + dz);
    let x00 = lerp(c(0, 0, 0), c(1, 0, 0), s[0]);
    let x10 = lerp(c(0, 1, 0), c(1, 1, 0), s[0]);
    let x01 = lerp(c(0, 0, 1), c(1, 0, 1), s[0]);
    let x11 = lerp(c(0, 1, 1), c(1, 1, 1), s[0]);
    lerp(lerp(x00, x10, s[1]), lerp(x01, x11, s[1]), s[2])
}

fn fbm(p: [f32; 3], oct: usize) -> f32 {
    let mut a = 0.5;
    let mut f = 1.0;
    let mut s = 0.0;
    let mut n = 0.0;
    for _ in 0..oct {
        s += a * vnoise([p[0] * f, p[1] * f, p[2] * f]);
        n += a;
        a *= 0.5;
        f *= 2.03;
    }
    s / n
}

/// Linear radiance of the scene in direction d (x right, y down, z forward).
pub fn scene(d: V3) -> [f32; 3] {
    let d = normalize(d);
    let el = (-d[1]).asin() as f32;
    let az = d[0].atan2(d[2]) as f32;
    let ring = [az.cos(), az.sin(), 0.0f32];
    let pr = |k: f32, e: f32| [ring[0] * k, ring[1] * k, e];
    // mountains
    let ridge = 0.05 + 0.13 * fbm(pr(2.2, 0.3), 5) + 0.05 * fbm(pr(9.0, 1.7), 3);
    let hills = 0.02 + 0.05 * fbm(pr(4.0, 5.1), 4);
    let df = [d[0] as f32, d[1] as f32, d[2] as f32];
    if el > ridge {
        // sky with clouds and sun
        let t = ((el - ridge) / 1.2).clamp(0.0, 1.0);
        let mut c = [0.55 + (0.12 - 0.55) * t, 0.72 + (0.28 - 0.72) * t, 1.05 + (0.75 - 1.05) * t];
        let sun = normalize([0.55, -0.42, 0.72]);
        let sd = dot(d, sun) as f32;
        let glow = ((sd - 0.9).max(0.0) * 10.0).powi(2);
        let cl = fbm([df[0] * 3.0 + 7.0, df[1] * 6.0, df[2] * 3.0], 6);
        let cloud = ((cl - 0.52) * 4.0).clamp(0.0, 1.0) * (1.0 - t * 0.6);
        for k in 0..3 {
            c[k] = c[k] * (1.0 - cloud) + (1.1 - 0.25 * cl) * cloud + glow * 1.5;
        }
        if sd > 0.9995 {
            return [25.0, 23.0, 18.0];
        }
        return c;
    }
    if el > hills {
        // rock & snow
        let n = fbm([df[0] * 40.0, df[1] * 40.0, df[2] * 40.0], 5);
        let n2 = fbm([df[0] * 140.0, df[1] * 140.0, df[2] * 140.0], 3);
        let snow = el > ridge - 0.035 - 0.03 * n;
        if snow {
            let v = 0.85 + 0.15 * n2;
            return [v * 0.95, v * 0.97, v];
        }
        let v = 0.12 + 0.22 * n + 0.1 * n2;
        return [v * 1.0, v * 0.92, v * 0.85];
    }
    if el > -0.01 {
        // forested hills with small houses
        let n = fbm([df[0] * 90.0, df[1] * 90.0, df[2] * 90.0], 4);
        let mut c = [0.05 + 0.08 * n, 0.12 + 0.14 * n, 0.05 + 0.05 * n];
        let cell = (az * 18.0).floor();
        if hash3(cell as i32, 3, 9) > 0.55 && el < hills * 0.6 {
            let fx = az * 18.0 - cell;
            let top = hills * (0.25 + 0.3 * hash3(cell as i32, 1, 1));
            if (0.2..0.7).contains(&fx) && el < top {
                let wx = ((fx - 0.2) * 14.0).fract();
                let wy = ((el + 0.01) / top * 4.0).fract();
                let window = wx > 0.35 && wx < 0.75 && wy > 0.3 && wy < 0.7;
                c = if window { [0.9, 0.75, 0.35] } else { [0.75, 0.68, 0.58] };
            } else if (0.15..0.75).contains(&fx) && el < top + 0.01 * (1.0 - ((fx - 0.45) * 3.5).abs()) {
                c = [0.45, 0.12, 0.08];
            }
        }
        return c;
    }
    // lake / meadow
    let n = fbm([df[0] * 25.0, df[1] * 25.0, df[2] * 25.0], 5);
    if az.sin() > 0.35 && el > -0.25 {
        let refl = scene([d[0], -d[1].abs() * 0.98, d[2]]);
        let rip = fbm([df[0] * 160.0, df[1] * 400.0, df[2] * 160.0], 3);
        return [refl[0] * 0.55 * (0.8 + 0.4 * rip), refl[1] * 0.6 * (0.8 + 0.4 * rip), refl[2] * 0.7 * (0.8 + 0.4 * rip)];
    }
    let n2 = fbm([df[0] * 200.0, df[1] * 200.0, df[2] * 200.0], 3);
    let flowers = n2 > 0.72;
    if flowers {
        return [0.9, 0.85, 0.2];
    }
    [0.12 + 0.12 * n, 0.22 + 0.16 * n + 0.05 * n2, 0.06 + 0.04 * n]
}

pub struct SampleShot {
    pub name: String,
    pub img: RgbImage,
}

/// Render one "photo" of the scene.
pub fn shoot(yaw_deg: f64, pitch_deg: f64, roll_deg: f64, hfov: f64, w: u32, h: u32, k1: f64, exposure: f32) -> RgbImage {
    let lens = Lens { k1, ..Lens::from_hfov_deg(hfov) };
    let rot = camera_from_ypr(yaw_deg.to_radians(), pitch_deg.to_radians(), roll_deg.to_radians());
    let l = w.max(h) as f64;
    let mut img = RgbImage::new(w, h);
    let wu = w as usize;
    img.par_chunks_mut(wu * 3).enumerate().for_each(|(y, row)| {
        for x in 0..wu {
            let mut acc = [0.0f32; 3];
            for (sx, sy) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
                let u = (x as f64 + sx - w as f64 * 0.5) / l;
                let v = (y as f64 + sy - h as f64 * 0.5) / l;
                let c = scene(mtv(&rot, ray(&lens, [u, v])));
                for k in 0..3 {
                    acc[k] += c[k] * 0.25;
                }
            }
            let u = (x as f64 - w as f64 * 0.5) / l;
            let v = (y as f64 - h as f64 * 0.5) / l;
            let vig = (1.0 - 0.5 * (u * u + v * v)) as f32;
            for k in 0..3 {
                let lin = (acc[k] * exposure * vig).max(0.0);
                // gentle film-like roll-off then sRGB
                let tone = lin / (1.0 + 0.15 * lin);
                row[x * 3 + k] = (crate::imgutil::linear_to_srgb(tone) * 255.0 + 0.5) as u8;
            }
        }
    });
    img
}

/// Five overlapping handheld-style shots of the landscape.
pub fn panorama_set() -> Vec<SampleShot> {
    let yaws = [-84.0, -42.0, 0.0, 41.0, 83.0];
    let pitches = [3.0, 1.5, 2.5, 4.0, 2.0];
    let rolls = [1.2, -0.8, 0.4, -1.5, 0.9];
    let exps = [1.0f32, 0.82, 1.12, 0.92, 1.06];
    yaws.par_iter()
        .enumerate()
        .map(|(i, &y)| SampleShot {
            name: format!("sample_{:02}.jpg", i + 1),
            img: shoot(y, pitches[i], rolls[i], 62.0, 960, 640, -0.04, exps[i]),
        })
        .collect()
}

/// Three exposures of the same view (dark, normal, bright).
pub fn bracket_set() -> Vec<SampleShot> {
    [(0.25f32, "dark"), (1.0, "normal"), (4.0, "bright")]
        .par_iter()
        .map(|(e, n)| SampleShot { name: format!("bracket_{}.jpg", n), img: shoot(25.0, 8.0, 0.0, 70.0, 960, 640, 0.0, *e) })
        .collect()
}
