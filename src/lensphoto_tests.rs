use super::*;
use crate::align::Lens;
use crate::geometry::{camera_from_ypr, mtv, mv};
use crate::imgutil::{linear_to_srgb, sample_rgb_linear, srgb_to_linear_lut};
use crate::stitch::{compute_gains, fit_bounds, render_preview, render_strips, to_rgba8, Blend, Mapper, Progress, RenderOptions};
use image::RgbImage;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const PW: u32 = 300;
const PH: u32 = 200;

/// The true model of the two lenses of the test scene: lens 0 (three photos, the reference)
/// with sRGB response and some vignetting, lens 1 (two photos) with another response per channel
/// and stronger vignetting.
fn truth() -> [LensModel; 2] {
    [LensModel { gamma: [1.0; 3], vig: [-0.25, -0.05] }, LensModel { gamma: [1.1, 1.02, 0.92], vig: [-0.35, -0.1] }]
}

const EXPOSURE: [[f32; 3]; 5] = [[1.0, 1.0, 1.0], [0.85, 0.9, 0.8], [1.1, 1.05, 1.15], [0.9, 0.95, 0.9], [1.05, 1.0, 1.1]];

fn lens_of() -> Vec<usize> {
    vec![0, 1, 0, 1, 0]
}

/// Photos of one textured scene; photo `i` of lens `l` sees `scene * exposure_i * v_l(r)` and
/// stores it through the inverse of its response (`value = linear^(1/γ)`).
fn scene(models: &[LensModel; 2]) -> (Vec<RenderPhoto>, PanoView) {
    let lo = lens_of();
    let mut photos: Vec<RenderPhoto> = (0..5)
        .map(|i| RenderPhoto {
            img: Arc::new(RgbImage::new(1, 1)),
            w: PW as f64,
            h: PH as f64,
            rot: camera_from_ypr((i as f64 * 14.0 - 28.0).to_radians(), (if i % 2 == 0 { 3.0f64 } else { -3.0 }).to_radians(), 0.0),
            lens: Lens::from_hfov_deg(50.0),
            field: None,
            gain: [1.0; 3],
            gain_map: None,
            photometry: None,
            full_long: PW as f64,
            trusted: true,
            mask: None,
        })
        .collect();
    let mut view = PanoView::default();
    fit_bounds(&mut view, &photos);
    let (tw, th) = (450u32, 200u32);
    let tex = crate::testutil::texture(tw, th, 5);
    let lut = srgb_to_linear_lut();
    let b = view.bounds;
    let vr = view.rotation();
    for (i, p) in photos.iter_mut().enumerate() {
        let m = models[lo[i]];
        let mut img = RgbImage::new(PW, PH);
        for y in 0..PH {
            for x in 0..PW {
                let u = [((x as f64 + 0.5) - PW as f64 * 0.5) / PW as f64, ((y as f64 + 0.5) - PH as f64 * 0.5) / PW as f64];
                let dir = mtv(&p.rot, crate::align::ray(&p.lens, u));
                let Some((px, py)) = view.projection.from_dir(mv(&vr, dir)) else { continue };
                let tx = (((px - b[0]) / (b[1] - b[0])) * tw as f64).clamp(0.0, tw as f64 - 1.0) as u32;
                let ty = (((py - b[2]) / (b[3] - b[2])) * th as f64).clamp(0.0, th as f64 - 1.0) as u32;
                let t = tex.get_pixel(tx, ty);
                let v = m.vignetting(radius2(x as f32, y as f32, PW as f32, PH as f32));
                let mut c = [0u8; 3];
                for k in 0..3 {
                    let lin = (0.05 + lut[t[k] as usize] * 0.45) * EXPOSURE[i][k] * v;
                    c[k] = (linear_to_srgb(lin.powf(1.0 / m.gamma[k])) * 255.0 + 0.5) as u8;
                }
                img.put_pixel(x, y, image::Rgb(c));
            }
        }
        p.img = Arc::new(img);
    }
    (photos, view)
}

fn on() -> LensPhotoConfig {
    LensPhotoConfig { enabled: true, ..Default::default() }
}

fn with_fit(photos: &[RenderPhoto], fit: &LensFit) -> Vec<RenderPhoto> {
    photos.iter().zip(lens_of()).enumerate().map(|(i, (p, l))| RenderPhoto { gain: fit.gains[i], photometry: fit.models[l], ..p.clone() }).collect()
}

/// Median over blocks of the preview of |mean(ln A - ln B)| for all overlapping photo pairs.
fn overlap_error(photos: &[RenderPhoto], view: &PanoView) -> f64 {
    let (w, h, bs) = (400usize, 150usize, 8usize);
    let m = Mapper::new(view, view.bounds, w, h);
    let mut sums: std::collections::HashMap<(usize, usize, usize, usize, usize), (f64, f64)> = std::collections::HashMap::new();
    for py in 0..h {
        for px in 0..w {
            let Some(d) = m.dir(px, py) else { continue };
            let s: Vec<(usize, [f32; 3])> = photos
                .iter()
                .enumerate()
                .filter_map(|(i, p)| {
                    let (x, y, wt) = p.lookup(d)?;
                    (wt >= 0.05).then(|| (i, p.shade(sample_rgb_linear(&p.img, x, y), x, y)))
                })
                .collect();
            for a in 0..s.len() {
                for b in a + 1..s.len() {
                    for k in 0..3 {
                        let e = sums.entry((px / bs, py / bs, s[a].0, s[b].0, k)).or_insert((0.0, 0.0));
                        e.0 += (s[a].1[k].max(1e-4) / s[b].1[k].max(1e-4)).ln() as f64;
                        e.1 += 1.0;
                    }
                }
            }
        }
    }
    let mut v: Vec<f64> = sums.values().filter(|e| e.1 >= (bs * bs / 2) as f64).map(|e| (e.0 / e.1).abs()).collect();
    assert!(v.len() > 100, "the photos should overlap: {} blocks", v.len());
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[test]
fn the_fit_finds_response_and_vignetting_of_two_lenses() {
    let t = truth();
    let (photos, view) = scene(&t);
    let fit = fit_lens_models(&photos, &lens_of(), &view, &on(), &GainMapConfig::default()).expect("enough comparisons");
    eprintln!("fit: {:?}, cost {:.5} -> {:.5}", fit.models, fit.cost_gains_only, fit.cost_final);
    for l in 0..2 {
        let m = fit.models[l].expect("both lenses have enough comparisons");
        for r in [0.3f32, 0.6, 0.9, 1.0] {
            let (a, b) = (m.vignetting(r * r), t[l].vignetting(r * r));
            assert!((a / b).ln().abs() < 0.04, "lens {l}: vignetting at r = {r} is {a:.3}, true {b:.3}");
        }
        for k in 0..3 {
            assert!((m.gamma[k] - t[l].gamma[k]).abs() < 0.03, "lens {l}: gamma {:?}, true {:?}", m.gamma, t[l].gamma);
        }
    }
    // the reference lens (most comparisons) keeps γ = 1 exactly
    assert_eq!(fit.models[0].unwrap().gamma, [1.0; 3]);
    assert!(fit.cost_final < fit.cost_gains_only * 0.5, "the model should explain most of the differences");
}

#[test]
fn the_model_lowers_the_differences_in_the_overlaps() {
    let (mut photos, view) = scene(&truth());
    let fit = fit_lens_models(&photos, &lens_of(), &view, &on(), &GainMapConfig::default()).unwrap();
    let corrected = with_fit(&photos, &fit);
    let g = compute_gains(&photos, &view);
    for (p, g) in photos.iter_mut().zip(g) {
        p.gain = g;
    }
    let (before, after) = (overlap_error(&photos, &view), overlap_error(&corrected, &view));
    eprintln!("median difference in the overlaps: factors only {before:.4}, lens model {after:.4}");
    assert!(after < before * 0.5, "{before:.4} -> {after:.4}");
}

#[test]
fn photos_that_match_get_no_or_a_near_identity_model() {
    let id = [LensModel::IDENTITY; 2];
    let (photos, view) = scene(&id);
    let fit = fit_lens_models(&photos, &lens_of(), &view, &on(), &GainMapConfig::default()).unwrap();
    for m in fit.models.iter().flatten() {
        for r in [0.5f32, 1.0] {
            assert!((m.vignetting(r * r) - 1.0).abs() < 0.03, "{m:?}");
        }
        assert!(m.gamma.iter().all(|g| (g - 1.0).abs() < 0.02), "{m:?}");
    }
}

#[test]
fn implausible_parts_are_dropped_and_small_lenses_are_skipped() {
    let (photos, view) = scene(&truth());
    // the response of lens 1 is outside a (very narrow) plausible range: only γ goes back to 1
    let narrow = LensPhotoConfig { gamma_range: (0.99, 1.01), ..on() };
    let fit = fit_lens_models(&photos, &lens_of(), &view, &narrow, &GainMapConfig::default()).unwrap();
    let m1 = fit.models[1].expect("vignetting stays");
    assert_eq!(m1.gamma, [1.0; 3]);
    assert!(m1.vig[0] < -0.1, "vignetting of lens 1 should still be found: {m1:?}");
    // vignetting outside the range: dropped as well
    let narrow = LensPhotoConfig { vig_range: (0.95, 1.05), gamma_range: (0.99, 1.01), ..on() };
    let fit = fit_lens_models(&photos, &lens_of(), &view, &narrow, &GainMapConfig::default()).unwrap();
    assert!(fit.models.iter().all(|m| m.is_none()), "{:?}", fit.models);
    // too few comparisons per lens: no model, but the photo factors are still solved
    let few = LensPhotoConfig { min_samples: usize::MAX, ..on() };
    let fit = fit_lens_models(&photos, &lens_of(), &view, &few, &GainMapConfig::default()).unwrap();
    assert!(fit.models.iter().all(|m| m.is_none()));
    assert!(fit.gains.iter().any(|g| (g[0] - 1.0).abs() > 0.02));
    // the limits themselves
    assert!(LensModel { gamma: [1.0; 3], vig: [-0.7, 0.0] }.plausible_vig((0.2, 1.1)));
    assert!(!LensModel { gamma: [1.0; 3], vig: [-0.7, -0.2] }.plausible_vig((0.2, 1.1)));
    assert!(!LensModel { gamma: [1.0; 3], vig: [0.3, 0.0] }.plausible_vig((0.2, 1.1)));
    assert!(!LensModel { gamma: [1.3, 1.0, 1.0], vig: [0.0; 2] }.plausible_gamma((0.8, 1.25)));
    assert!(LensModel::IDENTITY.plausible_gamma((0.8, 1.25)) && LensModel::IDENTITY.plausible_vig((0.2, 1.1)));
}

#[test]
fn switched_off_nothing_changes() {
    let (photos, view) = scene(&truth());
    let off = LensPhotoConfig { enabled: false, ..Default::default() };
    assert!(fit_lens_models(&photos, &lens_of(), &view, &off, &GainMapConfig::default()).is_none());
    // without a model the colour is exactly c * gain_at, an identity model changes nothing either
    let p = RenderPhoto { gain: [1.1, 0.9, 1.3], ..photos[1].clone() };
    let c = [0.123f32, 0.456, 0.789];
    let g = p.gain_at(10.0, 20.0);
    assert_eq!(p.shade(c, 10.0, 20.0), [c[0] * g[0], c[1] * g[1], c[2] * g[2]]);
    let id = RenderPhoto { photometry: Some(LensModel::IDENTITY), ..p.clone() };
    assert_eq!(id.shade(c, 10.0, 20.0), p.shade(c, 10.0, 20.0));
    let ids: Vec<RenderPhoto> = photos.iter().map(|p| RenderPhoto { photometry: Some(LensModel::IDENTITY), ..p.clone() }).collect();
    assert_eq!(render_preview(&photos, &view, 400, 150, false, true), render_preview(&ids, &view, 400, 150, false, true));
}

#[test]
fn the_fit_is_deterministic() {
    let (photos, view) = scene(&truth());
    let a = fit_lens_models(&photos, &lens_of(), &view, &on(), &GainMapConfig::default());
    let b = fit_lens_models(&photos, &lens_of(), &view, &on(), &GainMapConfig::default());
    assert_eq!(a, b);
}

#[test]
fn apply_undoes_response_and_vignetting() {
    let m = LensModel { gamma: [1.1, 1.0, 0.9], vig: [-0.3, -0.1] };
    // centre: no vignetting; corner: v(1) = 0.6
    let c = m.apply([0.25, 0.25, 0.25], 149.5, 99.5, 300.0, 200.0);
    assert!((c[0] - 0.25f32.powf(1.1)).abs() < 1e-6 && (c[1] - 0.25).abs() < 1e-6 && (c[2] - 0.25f32.powf(0.9)).abs() < 1e-6);
    let c = m.apply([0.3, 0.3, 0.3], -0.5, -0.5, 300.0, 200.0);
    assert!((c[1] - 0.5).abs() < 1e-5, "{c:?}");
    assert!((radius2(-0.5, -0.5, 300.0, 200.0) - 1.0).abs() < 1e-6);
}

/// The strip height must not change the picture with lens models either.
#[test]
fn strip_height_changes_nothing_with_lens_models() {
    let (photos, view) = scene(&truth());
    let fit = fit_lens_models(&photos, &lens_of(), &view, &on(), &GainMapConfig::default()).unwrap();
    let photos = with_fit(&photos, &fit);
    assert!(photos.iter().all(|p| p.photometry.is_some()));
    let load = |i: usize, _px: bool| Ok(photos[i].clone());
    let cancel = AtomicBool::new(false);
    let prog = Progress { report: &|_, _| {}, cancel: &cancel };
    for blend in [Blend::Feather, Blend::Smooth] {
        let ro = RenderOptions { prefer_detail: true, levels: 4, ..Default::default() };
        let run = |rows: usize| {
            let cfg = crate::strip::StripConfig { max_rows: rows, min_rows: 16.min(rows), ..Default::default() };
            let mut out: Vec<u8> = Vec::new();
            let mut sink = |_: usize, c: &[[f32; 3]], v: &[f32], _: usize| {
                out.extend_from_slice(&to_rgba8(c, v));
                Ok(())
            };
            render_strips(photos.len(), &load, &view, 1200, blend, &ro, &cfg, 8 << 30, &mut sink, &prog).unwrap();
            out
        };
        let whole = run(1 << 20);
        let strips = run(64);
        let max = whole.iter().zip(&strips).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
        assert!(max <= if blend == Blend::Feather { 0 } else { 1 }, "{blend:?}: strips differ from one window by {max}");
    }
}
