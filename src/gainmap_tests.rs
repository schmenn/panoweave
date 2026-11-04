use super::*;
use crate::align::Lens;
use crate::geometry::{camera_from_ypr, mtv, mv};
use crate::imgutil::{linear_to_srgb, srgb_to_linear_lut};
use crate::stitch::{compute_gains, fit_bounds, render_preview, render_strips, to_rgba8, Blend, Progress, RenderOptions};
use image::RgbImage;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const PW: u32 = 300;
const PH: u32 = 200;

/// Brightness change of one photo as a function of the position in the photo (fractions).
type Change = Box<dyn Fn(f32, f32) -> f32>;

fn no_change() -> Change {
    Box::new(|_, _| 1.0)
}

/// Photos of one and the same textured scene at the given yaw angles (degrees); photo `i` is
/// multiplied by `changes[i]` in linear light. `blob` paints a dark square into photo 1 only.
fn scene(yaws: &[f64], changes: Vec<Change>, blob: bool) -> (Vec<RenderPhoto>, PanoView) {
    let mut photos: Vec<RenderPhoto> = yaws
        .iter()
        .map(|&yaw| RenderPhoto {
            img: Arc::new(RgbImage::new(1, 1)),
            w: PW as f64,
            h: PH as f64,
            rot: camera_from_ypr(yaw.to_radians(), 0.0, 0.0),
            lens: Lens::from_hfov_deg(50.0),
            field: None,
            gain: [1.0; 3],
            gain_map: None,
            full_long: PW as f64,
            trusted: true,
            mask: None,
        })
        .collect();
    let mut view = PanoView::default();
    fit_bounds(&mut view, &photos);
    // the scene: a texture over the whole plane of the view, a bit dark so nothing clips
    let (tw, th) = (450u32, 160u32);
    let tex = crate::testutil::texture(tw, th, 11);
    let lut = srgb_to_linear_lut();
    let b = view.bounds;
    let vr = view.rotation();
    for (i, p) in photos.iter_mut().enumerate() {
        let mut img = RgbImage::new(PW, PH);
        for y in 0..PH {
            for x in 0..PW {
                let u = [((x as f64 + 0.5) - PW as f64 * 0.5) / PW as f64, ((y as f64 + 0.5) - PH as f64 * 0.5) / PW as f64];
                let dir = mtv(&p.rot, crate::align::ray(&p.lens, u));
                let Some((px, py)) = view.projection.from_dir(mv(&vr, dir)) else { continue };
                let tx = (((px - b[0]) / (b[1] - b[0])) * tw as f64).clamp(0.0, tw as f64 - 1.0) as u32;
                let ty = (((py - b[2]) / (b[3] - b[2])) * th as f64).clamp(0.0, th as f64 - 1.0) as u32;
                let t = tex.get_pixel(tx, ty);
                let f = changes[i]((x as f32 + 0.5) / PW as f32, (y as f32 + 0.5) / PH as f32);
                let mut c = [0u8; 3];
                for k in 0..3 {
                    c[k] = (linear_to_srgb(lut[t[k] as usize] * 0.5 * f) * 255.0 + 0.5) as u8;
                }
                if blob && i == 1 && (110..150).contains(&x) && (60..100).contains(&y) {
                    c = [12, 12, 12];
                }
                img.put_pixel(x, y, image::Rgb(c));
            }
        }
        p.img = Arc::new(img);
    }
    (photos, view)
}

fn apply_global(photos: &mut [RenderPhoto], view: &PanoView) {
    let g = compute_gains(photos, view);
    for (p, g) in photos.iter_mut().zip(g) {
        p.gain = g;
    }
}

fn with_maps(photos: &[RenderPhoto], maps: &[Option<GainMap>]) -> Vec<RenderPhoto> {
    photos.iter().zip(maps).map(|(p, m)| RenderPhoto { gain_map: m.clone().map(Arc::new), ..p.clone() }).collect()
}

/// Median over blocks of the preview of |mean(ln A - ln B)| (all channels, pairs of photos that
/// see the block well). Averaging over blocks removes the resampling noise at sharp edges.
fn overlap_error(photos: &[RenderPhoto], view: &PanoView) -> f64 {
    overlap_stats(photos, view).0
}

fn overlap_stats(photos: &[RenderPhoto], view: &PanoView) -> (f64, f64) {
    let (w, h, bs) = (400usize, 130usize, 8usize);
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
                    if wt < 0.15 {
                        return None;
                    }
                    let c = sample_rgb_linear(&p.img, x, y);
                    let g = p.gain_at(x, y);
                    Some((i, [c[0] * g[0], c[1] * g[1], c[2] * g[2]]))
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
    (v[v.len() / 2], v[v.len() * 9 / 10])
}

fn changes_abc() -> Vec<Change> {
    vec![
        Box::new(|_, fy| 1.0 + 0.3 * fy),
        Box::new(|fx, fy| 1.0 - 0.35 * (((fx - 0.5) * 2.0).powi(2) + ((fy - 0.5) * 2.0).powi(2)) * 0.5),
        no_change(),
    ]
}

#[test]
fn gradients_and_vignettes_are_corrected_inside_the_photos() {
    let (mut photos, view) = scene(&[-20.0, 0.0, 20.0], changes_abc(), false);
    apply_global(&mut photos, &view);
    let before = overlap_error(&photos, &view);
    let maps = compute_gain_maps(&photos, &view, &GainMapConfig::default());
    assert!(maps.iter().all(|m| m.is_some()));
    let after = overlap_error(&with_maps(&photos, &maps), &view);
    // what remains with identical photos (resampling noise at sharp edges) cannot be corrected
    let (same, sv) = scene(&[-20.0, 0.0, 20.0], vec![no_change(), no_change(), no_change()], false);
    let floor = overlap_error(&same, &sv);
    eprintln!("median brightness difference in the overlaps: global factor only {before:.4}, with maps {after:.4} (noise floor {floor:.4})");
    assert!(before - floor > 0.02, "the scene should need a correction: {before} (floor {floor})");
    assert!(after - floor < (before - floor) * 0.5, "maps should at least halve the correctable difference: {before:.4} -> {after:.4} (floor {floor:.4})");
}

#[test]
fn photos_without_differences_keep_maps_near_one() {
    let (mut photos, view) = scene(&[-20.0, 0.0, 20.0], vec![no_change(), no_change(), no_change()], false);
    apply_global(&mut photos, &view);
    let maps = compute_gain_maps(&photos, &view, &GainMapConfig::default());
    for m in maps.iter().flatten() {
        for c in &m.d {
            for v in c {
                assert!((v - 1.0).abs() < 0.01, "map value {v} should stay at 1");
            }
        }
    }
}

#[test]
fn a_moving_object_in_one_photo_does_not_bend_the_map() {
    let (mut photos, view) = scene(&[-20.0, 0.0, 20.0], changes_abc(), false);
    apply_global(&mut photos, &view);
    let clean = compute_gain_maps(&photos, &view, &GainMapConfig::default());
    let (mut with_blob, view2) = scene(&[-20.0, 0.0, 20.0], changes_abc(), true);
    assert_eq!(view.bounds, view2.bounds);
    apply_global(&mut with_blob, &view2);
    let dirty = compute_gain_maps(&with_blob, &view2, &GainMapConfig::default());
    let mut worst = 0.0f32;
    for (a, b) in clean.iter().zip(&dirty) {
        for (x, y) in a.as_ref().unwrap().d.iter().zip(&b.as_ref().unwrap().d) {
            for k in 0..3 {
                worst = worst.max((x[k] / y[k]).ln().abs());
            }
        }
    }
    eprintln!("largest change of a map value caused by the moving object: {worst:.4}");
    assert!(worst < 0.05, "the object changed a map value by {worst}");
}

#[test]
fn a_photo_without_overlap_gets_no_map() {
    let (mut photos, view) = scene(&[-20.0, 0.0, 20.0, 150.0], vec![no_change(), no_change(), no_change(), no_change()], false);
    apply_global(&mut photos, &view);
    let maps = compute_gain_maps(&photos, &view, &GainMapConfig::default());
    assert!(maps[0].is_some() && maps[1].is_some() && maps[2].is_some());
    assert!(maps[3].is_none());
    // switched off: no maps at all
    let off = GainMapConfig { enabled: false, ..Default::default() };
    assert!(compute_gain_maps(&photos, &view, &off).iter().all(|m| m.is_none()));
}

#[test]
fn maps_are_deterministic() {
    let (mut photos, view) = scene(&[-20.0, 0.0, 20.0], changes_abc(), false);
    apply_global(&mut photos, &view);
    let a = compute_gain_maps(&photos, &view, &GainMapConfig::default());
    let b = compute_gain_maps(&photos, &view, &GainMapConfig::default());
    assert_eq!(a, b);
}

#[test]
fn without_a_map_the_gain_is_exactly_the_old_one_and_a_unit_map_changes_nothing() {
    let (mut photos, view) = scene(&[-20.0, 0.0, 20.0], changes_abc(), false);
    apply_global(&mut photos, &view);
    for p in &photos {
        assert_eq!(p.gain_at(12.3, 45.6), p.gain);
    }
    let unit: Vec<RenderPhoto> = photos.iter().map(|p| RenderPhoto { gain_map: Some(Arc::new(GainMap::identity(4, 3))), ..p.clone() }).collect();
    let (w, h) = (500, 160);
    assert_eq!(render_preview(&photos, &view, w, h, false, true), render_preview(&unit, &view, w, h, false, true));
}

/// The strip height must not change the picture, with a real (non-trivial) map as well.
#[test]
fn strip_height_changes_nothing_with_maps() {
    let (mut photos, view) = scene(&[-20.0, 0.0, 20.0], changes_abc(), false);
    apply_global(&mut photos, &view);
    let maps = compute_gain_maps(&photos, &view, &GainMapConfig::default());
    let photos = with_maps(&photos, &maps);
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

#[test]
fn map_interpolates_and_clamps_at_the_border() {
    let m = GainMap { gw: 2, gh: 2, d: vec![[1.0, 2.0, 3.0], [2.0, 2.0, 3.0], [1.0, 2.0, 3.0], [2.0, 2.0, 3.0]] };
    assert_eq!(m.at(0.0, 0.0), [1.0, 2.0, 3.0]);
    assert_eq!(m.at(1.0, 1.0), [2.0, 2.0, 3.0]);
    assert!((m.at(0.5, 0.3)[0] - 1.5).abs() < 1e-6);
    // outside the photo: the border value
    assert_eq!(m.at(-3.0, 0.5), m.at(0.0, 0.5));
    assert_eq!(m.at(7.0, 9.0), m.at(1.0, 1.0));
    // empty / unit maps
    assert_eq!(GainMap { gw: 0, gh: 0, d: vec![] }.at(0.3, 0.3), [1.0; 3]);
    assert_eq!(GainMap::identity(5, 4).at(0.37, 0.81), [1.0; 3]);
    let (n, w) = cell(0.5, 0.5, 5, 4);
    assert!((w.iter().sum::<f32>() - 1.0).abs() < 1e-6 && n.iter().all(|&i| i < 20));
    assert_eq!(grid_size(4000.0, 3000.0, 5), (5, 4));
    assert_eq!(grid_size(3000.0, 4000.0, 5), (4, 5));
}
