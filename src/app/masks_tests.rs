//! Tests of the mask tool wired into the app: project file round trip and hand-over to the renderer.

use super::*;
use crate::masks::{MaskKind, MaskStroke, POSITIVE};

pub(super) fn app_with_photos(n: usize) -> PanoApp {
    let ctx = egui::Context::default();
    let mut app = PanoApp::with_context(&ctx, crate::Args { screenshot: None, demo: None, files: vec![] });
    let loaded = (0..n)
        .map(|i| {
            let img = RgbImage::from_fn(120, 80, |x, y| image::Rgb([(x * 2) as u8, (y * 3) as u8, (i * 40) as u8]));
            engine::load(Source::Memory(Arc::new(img)), format!("p{i}.png"))
        })
        .collect();
    app.on_loaded(&ctx, loaded);
    assert_eq!(app.photos.len(), n);
    app
}

pub(super) fn stroke(kind: MaskKind) -> MaskStroke {
    MaskStroke { kind, points: vec![[0.3, 0.4], [0.6, 0.5]], radius: 0.05, polygon: false }
}

#[test]
fn photos_without_masks_hand_over_no_mask() {
    let app = app_with_photos(2);
    assert!(app.render_photos(true).iter().all(|r| r.mask.is_none()));
    assert!(app.export_sources().iter().all(|s| render_photo_of(s, s.1.clone()).mask.is_none()));
    assert_eq!(app.masked_photos(), 0);
}

#[test]
fn a_painted_photo_hands_its_mask_to_the_renderer_and_the_export() {
    let mut app = app_with_photos(3);
    let (w, h) = (app.photos[1].w as usize, app.photos[1].h as usize);
    app.photos[1].mask.commit(stroke(MaskKind::Positive), w, h);
    assert_eq!(app.masked_photos(), 1);
    let rp = app.render_photos(true);
    assert!(rp[0].mask.is_none() && rp[2].mask.is_none());
    let m = rp[1].mask.as_ref().expect("mask of photo 2");
    assert_eq!(m.at(0.45 * w as f32, 0.45 * h as f32, w as f64, h as f64), POSITIVE);
    // the export's loader builds the same photo
    let src = app.export_sources();
    let r = render_photo_of(&src[1], src[1].1.clone());
    assert_eq!(r.mask.as_ref().unwrap().data, m.data);
    assert!(render_photo_of(&src[0], src[0].1.clone()).mask.is_none());
    // a photo that is left out does not shift the masks of the others
    app.photos[0].enabled = false;
    let rp = app.render_photos(true);
    assert_eq!(rp.len(), 2);
    assert!(rp[0].mask.is_some());
    assert_eq!(app.export_sources()[0].9.is_some(), true);
}

#[test]
fn masks_survive_saving_and_opening_a_project() {
    let mut app = app_with_photos(2);
    let (w, h) = (app.photos[0].w as usize, app.photos[0].h as usize);
    app.photos[0].mask.commit(stroke(MaskKind::Negative), w, h);
    app.photos[0].mask.commit(MaskStroke { kind: MaskKind::Positive, points: vec![[0.1, 0.1], [0.9, 0.1], [0.5, 0.9]], radius: 0.0, polygon: true }, w, h);
    let dir = std::env::temp_dir().join(format!("pw_mask_roundtrip_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t.pwproj");
    app.save_project(file.clone());
    assert!(file.exists(), "{}", app.status);
    let pf = project::load(&file).unwrap();
    assert_eq!(pf.photos[0].masks, app.photos[0].mask.strokes);
    assert!(pf.photos[1].masks.is_empty());
    let mut other = app_with_photos(2);
    other.apply_project(pf);
    assert_eq!(other.photos[0].mask.strokes, app.photos[0].mask.strokes);
    assert_eq!(other.photos[0].mask.buf.as_deref(), app.photos[0].mask.buf.as_deref());
    assert!(other.photos[1].mask.buf.is_none());
    assert!(!other.photos[0].mask.can_undo(), "opening a project is no undo step");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn removing_a_photo_takes_its_mask_with_it() {
    let mut app = app_with_photos(3);
    let (w, h) = (app.photos[0].w as usize, app.photos[0].h as usize);
    app.photos[0].mask.commit(stroke(MaskKind::Positive), w, h);
    app.remove_photo(0);
    assert_eq!(app.masked_photos(), 0);
    assert_eq!(app.render_photos(false).len(), 2);
}
