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
    assert!(app.export_sources()[0].9.is_some());
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

#[test]
fn a_project_with_a_missing_photo_keeps_the_rest_and_is_not_overwritten() {
    let mut app = app_with_photos(3);
    app.cps = vec![
        ControlPoint::new(0, 1, [0.1; 2], [0.2; 2], false),
        ControlPoint::new(1, 2, [0.3; 2], [0.4; 2], false),
        ControlPoint::new(0, 2, [0.05, 0.06], [0.07, 0.08], true),
    ];
    let (w, h) = (app.photos[2].w as usize, app.photos[2].h as usize);
    app.photos[2].mask.commit(stroke(MaskKind::Negative), w, h);
    let dir = std::env::temp_dir().join(format!("pw_missing_photo_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t.pwproj");
    app.save_project(file.clone());
    let pf = project::load(&file).unwrap();
    std::fs::remove_file(&pf.photos[1].path).unwrap();
    // what `open_project` + the loader do, without the worker thread
    let ctx = egui::Context::default();
    let mut other = app_with_photos(0);
    other.project_path = Some(file.clone());
    let loaded = pf.photos.iter().map(|pp| engine::load(Source::File(pp.path.clone()), pp.path.file_name().unwrap().to_string_lossy().to_string())).collect();
    other.pending_project = Some(pf);
    other.on_loaded(&ctx, loaded);
    assert_eq!(other.photos.len(), 2);
    assert_eq!(other.cps.len(), 1, "only the point between the two photos that are there");
    assert_eq!((other.cps[0].a, other.cps[0].b, other.cps[0].pa), (0, 1, [0.05, 0.06]));
    assert_eq!(other.photos[1].mask.strokes.len(), 1, "the mask of photo 3 is now on photo 2");
    assert!(other.status.contains("missing") && other.status.contains("2 points"), "{}", other.status);
    assert!(other.project_path.is_none(), "saving must not silently replace the complete project");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn removing_photos_or_starting_anew_forgets_photo_numbers() {
    let mut app = app_with_photos(3);
    app.graph_photo = Some(2);
    app.click.a = Some((1.0, 1.0));
    app.remove_photo(2);
    assert!(app.graph_photo.is_none() && app.click.a.is_none());
    app.graph_photo = Some(1);
    app.out_path = "C:/old/panorama.jpg".into();
    app.needs_align = true;
    app.clear_all();
    assert!(app.graph_photo.is_none());
    assert!(app.out_path.is_empty() && app.last_output.is_none() && !app.needs_align);
}
