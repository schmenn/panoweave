//! The right-click menu of a single photo. Which entries exist and what they do is decided by
//! plain functions (`menu_items`, `apply_photo_action`), so it can be tested without a window.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum PhotoAction {
    EditMasks,
    ClearMasks,
    ToggleEnabled,
    UseLeft,
    UseRight,
    Info,
    ShowInFolder,
    Remove,
}

/// What the menu needs to know about the photo and the app.
#[derive(Clone, Copy, Debug)]
pub(super) struct MenuState {
    pub(super) has_mask: bool,
    pub(super) enabled: bool,
    pub(super) photo_count: usize,
    pub(super) from_file: bool,
    pub(super) busy: bool,
}

pub(super) struct MenuItem {
    pub(super) action: PhotoAction,
    pub(super) label: &'static str,
    pub(super) usable: bool,
    /// tells why the entry is grey (or what it does)
    pub(super) hint: &'static str,
    pub(super) danger: bool,
}

/// The entries in the order they are shown; entries that do not fit now are marked, not left out.
/// "Clear masks" only appears when the photo has masks.
pub(super) fn menu_items(s: &MenuState) -> Vec<MenuItem> {
    let item = |action, label, usable, hint, danger| MenuItem { action, label, usable, hint, danger };
    let wait = "Wait until the current task has finished.";
    let mut v = vec![item(PhotoAction::EditMasks, "🖌  Edit masks…", true, "Paint where this photo must or must not be used.", false)];
    if s.has_mask {
        v.push(item(PhotoAction::ClearMasks, "Clear masks of this photo", true, "Removes everything painted on this photo. Can be taken back in the mask window.", false));
    }
    v.push(item(
        PhotoAction::ToggleEnabled,
        if s.enabled { "Disable photo" } else { "Enable photo" },
        !s.busy,
        if s.busy { wait } else { "Leaves the photo out of (or takes it back into) the panorama." },
        false,
    ));
    let two = s.photo_count >= 2;
    let hint = if two { "Shows this photo on this side in the Matching points step." } else { "Needs at least two photos." };
    v.push(item(PhotoAction::UseLeft, "Use as left photo in the point editor", two, hint, false));
    v.push(item(PhotoAction::UseRight, "Use as right photo in the point editor", two, hint, false));
    v.push(item(PhotoAction::Info, "Photo info…", true, "Name, size and focal length in the status line.", false));
    v.push(item(
        PhotoAction::ShowInFolder,
        "Show in folder",
        s.from_file,
        if s.from_file { "Opens the folder of the photo file." } else { "This photo is not a file (sample photo)." },
        false,
    ));
    v.push(item(
        PhotoAction::Remove,
        "Remove photo",
        !s.busy,
        if s.busy { wait } else { "Takes the photo out of the project (its masks go too)." },
        true,
    ));
    v
}

/// One line about a photo: name, size, focal length when known, file.
pub(super) fn info_line(p: &Photo) -> String {
    let mut s = format!("{}: {} × {} px", p.name, p.w, p.h);
    if let Some(f) = p.info.focal_mm {
        s += &format!(", {f:.0} mm");
    }
    if let Source::File(f) = &p.source {
        s += &format!(" ({})", f.display());
    }
    s
}

impl PanoApp {
    pub(super) fn menu_state(&self, i: usize) -> MenuState {
        let p = &self.photos[i];
        MenuState { has_mask: p.mask.any(), enabled: p.enabled, photo_count: self.photos.len(), from_file: matches!(p.source, Source::File(_)), busy: self.busy() }
    }

    /// Opens the mask window with photo `i` chosen, from any step.
    pub(super) fn open_masks_for(&mut self, i: usize) {
        if i >= self.photos.len() {
            return;
        }
        self.mk.tool.cancel();
        self.mk.sel = i;
        self.mk.view.key = 0;
        self.mk.open = true;
    }

    /// Does what a menu entry says. Out-of-range photos and entries that cannot apply do nothing.
    pub(super) fn apply_photo_action(&mut self, i: usize, a: PhotoAction) {
        if i >= self.photos.len() {
            return;
        }
        if !menu_items(&self.menu_state(i)).iter().any(|m| m.action == a && m.usable) {
            return;
        }
        match a {
            PhotoAction::EditMasks => {
                self.open_masks_for(i);
                if self.blend != Blend::Smooth {
                    self.set_status("Masks only steer Smooth seams: choose them in the Export step.", WARN);
                }
            }
            PhotoAction::ClearMasks => {
                let (w, h) = (self.photos[i].w as usize, self.photos[i].h as usize);
                if self.photos[i].mask.clear(w, h) {
                    self.mk.tool.cancel();
                    self.set_status(format!("Masks of {} cleared (undo in the mask window).", self.photos[i].name), GOOD);
                }
            }
            PhotoAction::ToggleEnabled => {
                self.photos[i].enabled = !self.photos[i].enabled;
                self.needs_align = true;
                self.preview_dirty = true;
            }
            PhotoAction::UseLeft | PhotoAction::UseRight => {
                let left = a == PhotoAction::UseLeft;
                let (mine, other) = if left { (self.pe.a, self.pe.b) } else { (self.pe.b, self.pe.a) };
                if mine != i {
                    // the two sides always show different photos: the old one moves over
                    let moved = if other == i { mine } else { other };
                    let (na, nb) = if left { (i, moved) } else { (moved, i) };
                    self.pe.a = na;
                    self.pe.b = nb;
                    self.pe.selected = None;
                    self.pe.pending = None;
                }
            }
            PhotoAction::Info => {
                let line = info_line(&self.photos[i]);
                self.set_status(line, MUTED);
            }
            PhotoAction::ShowInFolder => {
                if let Source::File(f) = &self.photos[i].source {
                    match f.parent() {
                        Some(d) => {
                            if let Err(e) = open::that(d) {
                                self.set_status(format!("Could not open the folder: {e}"), BAD);
                            }
                        }
                        None => self.set_status("This photo has no folder.", WARN),
                    }
                }
            }
            PhotoAction::Remove => self.remove_photo(i),
        }
    }

    /// Attaches the right-click menu of photo `i` to `resp`; returns the chosen entry (apply it
    /// after the drawing loop, the caller usually holds a borrow of the photos).
    pub(super) fn photo_menu(&self, resp: &egui::Response, i: usize) -> Option<(usize, PhotoAction)> {
        let items = menu_items(&self.menu_state(i));
        let title = format!("{}. {}", i + 1, self.photos[i].name);
        let mut chosen = None;
        resp.context_menu(|ui| {
            ui.label(RichText::new(title).strong());
            ui.separator();
            for m in &items {
                if m.action == PhotoAction::Remove {
                    ui.separator();
                }
                let text = if m.danger { RichText::new(m.label).color(BAD) } else { RichText::new(m.label) };
                let r = ui.add_enabled(m.usable, egui::Button::new(text)).on_hover_text(m.hint).on_disabled_hover_text(m.hint);
                if r.clicked() {
                    chosen = Some((i, m.action));
                    ui.close();
                }
            }
        });
        chosen
    }
}
