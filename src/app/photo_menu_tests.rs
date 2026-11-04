//! Tests of the photo menu logic (no window needed).

use super::masks_tests::{app_with_photos, stroke};
use super::photo_menu::*;
use super::*;
use crate::masks::MaskKind;

fn st(has_mask: bool, busy: bool, from_file: bool, n: usize) -> MenuState {
    MenuState { has_mask, enabled: true, photo_count: n, from_file, busy }
}

fn find(v: &[MenuItem], a: PhotoAction) -> Option<&MenuItem> {
    v.iter().find(|m| m.action == a)
}

#[test]
fn clear_masks_only_shows_with_masks() {
    assert!(find(&menu_items(&st(false, false, true, 3)), PhotoAction::ClearMasks).is_none());
    assert!(find(&menu_items(&st(true, false, true, 3)), PhotoAction::ClearMasks).is_some());
}

#[test]
fn entries_that_do_not_fit_are_grey_with_a_hint() {
    let v = menu_items(&st(false, true, false, 1));
    for a in [PhotoAction::ToggleEnabled, PhotoAction::UseLeft, PhotoAction::UseRight, PhotoAction::ShowInFolder, PhotoAction::Remove] {
        let m = find(&v, a).unwrap();
        assert!(!m.usable && !m.hint.is_empty(), "{a:?}");
    }
    assert!(find(&v, PhotoAction::EditMasks).unwrap().usable);
    assert!(find(&v, PhotoAction::Remove).unwrap().danger);
    let ok = menu_items(&st(false, false, true, 2));
    assert!(ok.iter().all(|m| m.usable));
}

#[test]
fn enable_and_disable_switch_and_mark_for_realignment() {
    let mut app = app_with_photos(3);
    app.needs_align = false;
    app.preview_dirty = false;
    assert_eq!(find(&menu_items(&app.menu_state(1)), PhotoAction::ToggleEnabled).unwrap().label, "Disable photo");
    app.apply_photo_action(1, PhotoAction::ToggleEnabled);
    assert!(!app.photos[1].enabled && app.needs_align && app.preview_dirty);
    assert_eq!(find(&menu_items(&app.menu_state(1)), PhotoAction::ToggleEnabled).unwrap().label, "Enable photo");
    app.apply_photo_action(1, PhotoAction::ToggleEnabled);
    assert!(app.photos[1].enabled);
}

#[test]
fn edit_masks_opens_the_window_with_that_photo_from_any_step() {
    let mut app = app_with_photos(3);
    for step in Step::ALL {
        app.step = step;
        app.mk.open = false;
        app.mk.sel = 0;
        app.apply_photo_action(2, PhotoAction::EditMasks);
        assert!(app.mk.open && app.mk.sel == 2, "{step:?}");
    }
    app.open_masks_for(99);
    assert_eq!(app.mk.sel, 2, "unknown photo is ignored");
}

#[test]
fn clear_masks_can_be_taken_back() {
    let mut app = app_with_photos(2);
    let (w, h) = (app.photos[0].w as usize, app.photos[0].h as usize);
    app.photos[0].mask.commit(stroke(MaskKind::Positive), w, h);
    app.apply_photo_action(0, PhotoAction::ClearMasks);
    assert!(!app.photos[0].mask.any());
    assert!(app.photos[0].mask.undo(w, h) && app.photos[0].mask.any());
    // nothing to clear: no change, no panic
    app.apply_photo_action(1, PhotoAction::ClearMasks);
}

#[test]
fn point_editor_sides_follow_the_menu_and_stay_different() {
    let mut app = app_with_photos(3);
    app.pe.a = 0;
    app.pe.b = 1;
    app.apply_photo_action(2, PhotoAction::UseLeft);
    assert_eq!((app.pe.a, app.pe.b), (2, 1));
    app.apply_photo_action(1, PhotoAction::UseLeft);
    assert_eq!((app.pe.a, app.pe.b), (1, 2), "the old left photo moves to the right");
    app.apply_photo_action(0, PhotoAction::UseRight);
    assert_eq!((app.pe.a, app.pe.b), (1, 0));
    app.apply_photo_action(1, PhotoAction::UseRight);
    assert_eq!((app.pe.a, app.pe.b), (0, 1));
    let mut one = app_with_photos(1);
    one.apply_photo_action(0, PhotoAction::UseLeft);
}

#[test]
fn remove_takes_the_photo_and_its_mask_away() {
    let mut app = app_with_photos(3);
    let (w, h) = (app.photos[1].w as usize, app.photos[1].h as usize);
    app.photos[1].mask.commit(stroke(MaskKind::Negative), w, h);
    app.apply_photo_action(1, PhotoAction::Remove);
    assert_eq!(app.photos.len(), 2);
    assert_eq!(app.masked_photos(), 0);
    assert!(app.needs_align);
}

#[test]
fn info_and_folder_for_sample_photos_are_harmless() {
    let mut app = app_with_photos(1);
    app.apply_photo_action(0, PhotoAction::Info);
    assert!(app.status.contains("p0.png") && app.status.contains("120 × 80"), "{}", app.status);
    let before = app.status.clone();
    app.apply_photo_action(0, PhotoAction::ShowInFolder);
    assert_eq!(app.status, before, "memory photo: nothing happens");
}
