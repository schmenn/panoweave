//! One shared helper for secondary windows: a real operating-system window (egui immediate
//! viewport) that can be moved over all monitors, with position and size remembered in
//! `%APPDATA%\PanoWeave\settings.json` (section `windows`). Closing it only clears the "open"
//! flag. Where viewports are not available (screenshot / demo runs, or `embed_viewports`) the same
//! content is shown in an ordinary embedded `egui::Window`.

use eframe::egui::{self, Color32, Context, Id, ViewportBuilder, ViewportClass, ViewportId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Windows moves minimised windows to about (-32000, -32000); positions below this are ignored.
const PARKED_POS: f32 = -10000.0;
/// Where a window goes when its saved position is not on any screen (points).
pub(super) const FALLBACK_POS: [f32; 2] = [40.0, 60.0];
/// Smallest saved size that is believed (points).
const MIN_SAVED: f32 = 200.0;
/// Largest saved size that is believed (points).
const MAX_SAVED: f32 = 20000.0;
/// Saving the geometry at most this often while the window is moved or resized.
const SAVE_EVERY: Duration = Duration::from_millis(1500);
/// Name of the section in settings.json.
const SECTION: &str = "windows";

/// What differs between the secondary windows.
pub(super) struct WinSpec {
    /// stable name: viewport id and key in the settings file
    pub key: &'static str,
    pub title: String,
    pub size: [f32; 2],
    pub min_size: [f32; 2],
    /// background of the window (`None`: the theme's panel colour)
    pub fill: Option<Color32>,
    /// Ctrl+Z / Ctrl+Y typed in this window undo / redo the control points (not for windows with
    /// their own history, like the masks)
    pub undo_keys: bool,
}

/// Position (top-left of the outer frame, points) and inner size (points) of a window.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct Geom {
    pub pos: Option<[f32; 2]>,
    pub size: Option<[f32; 2]>,
}

/// Is a saved size believable?
pub(super) fn size_ok(s: [f32; 2]) -> bool {
    s.iter().all(|v| v.is_finite() && (MIN_SAVED..=MAX_SAVED).contains(v))
}

/// Decides where a saved geometry opens. `visible(pos, size)` tells whether the title bar lies on
/// a connected screen. Returns (position or `None` for the system's choice, size).
pub(super) fn restore(saved: Geom, default_size: [f32; 2], visible: impl Fn([f32; 2], [f32; 2]) -> bool) -> (Option<[f32; 2]>, [f32; 2]) {
    let size = saved.size.filter(|s| size_ok(*s)).unwrap_or(default_size);
    let pos = match saved.pos {
        None => None,
        Some(p) if p.iter().all(|v| v.is_finite()) && p[0] > PARKED_POS && p[1] > PARKED_POS && visible(p, size) => Some(p),
        // a monitor was unplugged, or a minimised window's parking position was saved
        Some(_) => Some(FALLBACK_POS),
    };
    (pos, size)
}

/// Reads the geometry of window `key` from the parsed settings file.
pub(super) fn read_geom(root: &serde_json::Value, key: &str) -> Geom {
    root.get(SECTION).and_then(|w| w.get(key)).and_then(|g| serde_json::from_value(g.clone()).ok()).unwrap_or_default()
}

/// Puts the geometry of window `key` into the parsed settings file (other keys stay as they are).
pub(super) fn write_geom(root: &mut serde_json::Value, key: &str, g: &Geom) {
    if !root.is_object() {
        *root = serde_json::json!({});
    }
    if !root.get(SECTION).is_some_and(|w| w.is_object()) {
        root[SECTION] = serde_json::json!({});
    }
    root[SECTION][key] = serde_json::to_value(g).unwrap_or(serde_json::Value::Null);
}

fn default_settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("PanoWeave").join("settings.json"))
}

fn load_root(path: &Path) -> serde_json::Value {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(serde_json::Value::Null)
}

fn store_geom(path: &Path, key: &str, g: &Geom) -> Result<(), String> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let mut root = load_root(path);
    write_geom(&mut root, key, g);
    std::fs::write(path, serde_json::to_string_pretty(&root).unwrap_or_default()).map_err(|e| e.to_string())
}

struct WinState {
    now: Geom,
    saved: Geom,
    last_save: Instant,
    start_pos: Option<[f32; 2]>,
    start_size: [f32; 2],
    focus_once: bool,
}

/// All secondary windows of the app. Lives in the app state; take it out with `std::mem::take`
/// while a window is drawn so the draw closure may borrow the rest of the app.
#[derive(Default)]
pub(super) struct ViewportWindows {
    wins: HashMap<&'static str, WinState>,
    /// automatic runs (`--screenshot`) must not write the settings
    pub no_save: bool,
    /// undo key seen in a window of its own since the app last looked (see `handle_undo_keys`)
    pub undo_request: Option<super::history::UndoKey>,
    /// other settings file (tests)
    path: Option<PathBuf>,
}

impl ViewportWindows {
    fn path(&self) -> Option<PathBuf> {
        self.path.clone().or_else(default_settings_path)
    }

    /// Shows window `spec` while `*open` is set. `force_embed`: always an embedded `egui::Window`
    /// (screenshot / demo runs, where only the main window is captured).
    pub(super) fn show(&mut self, ctx: &Context, spec: &WinSpec, open: &mut bool, force_embed: bool, mut content: impl FnMut(&mut egui::Ui)) {
        if !*open {
            self.forget(spec.key);
            return;
        }
        if force_embed {
            ctx.set_embed_viewports(true);
        }
        if ctx.embed_viewports() {
            let mut o = true;
            egui::Window::new(spec.title.clone()).id(Id::unique(("vpwin", spec.key))).open(&mut o).default_size(spec.size).default_pos(FALLBACK_POS).resizable(true).show(ctx, |ui| content(ui));
            *open = o;
            return;
        }
        let path = self.path();
        let st = self.wins.entry(spec.key).or_insert_with(|| {
            let saved = path.as_deref().map(|p| read_geom(&load_root(p), spec.key)).unwrap_or_default();
            let ppp = ctx.input(|i| i.viewport().native_pixels_per_point).unwrap_or(1.0);
            let (start_pos, start_size) = restore(saved, spec.size, |p, s| crate::patch_window::on_screen(p, s, ppp));
            if saved.pos.is_some() && start_pos == Some(FALLBACK_POS) && saved.pos != start_pos {
                eprintln!("window '{}': saved position {:?} is not on any screen - opening at {FALLBACK_POS:?}", spec.key, saved.pos);
            }
            WinState { now: saved, saved, last_save: Instant::now(), start_pos, start_size, focus_once: true }
        });
        let mut b = ViewportBuilder::default().with_title(spec.title.clone()).with_inner_size(st.start_size).with_min_inner_size(spec.min_size);
        if let Some(p) = st.start_pos {
            b = b.with_position(p);
        }
        let panel_fill = spec.fill;
        let mut close = false;
        let mut undo_req = None;
        let want_undo = spec.undo_keys;
        ctx.show_viewport_immediate(ViewportId::from_hash_of(("pw_window", spec.key)), b, |ui, class| {
            let ctx = ui.ctx().clone();
            let (req, outer, inner, minimized) = ctx.input(|i| (i.viewport().close_requested(), i.viewport().outer_rect, i.viewport().inner_rect, i.viewport().minimized.unwrap_or(false)));
            close |= req;
            if std::mem::take(&mut st.focus_once) && class != ViewportClass::EmbeddedWindow {
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            if !minimized {
                if let Some(r) = outer.filter(|r| r.min.x > PARKED_POS && r.min.y > PARKED_POS) {
                    st.now.pos = Some([r.min.x, r.min.y]);
                }
                if let Some(r) = inner.filter(|r| r.width() >= 1.0 && r.height() >= 1.0) {
                    st.now.size = Some([r.width(), r.height()]);
                }
            }
            let fill = panel_fill.unwrap_or(ui.visuals().panel_fill);
            egui::CentralPanel::default().frame(egui::Frame::new().fill(fill).inner_margin(egui::Margin::same(8))).show(ui, |ui| {
                if class == ViewportClass::EmbeddedWindow {
                    // the backend could not open a real window: give the embedded one a way out
                    if ui.small_button("Close").clicked() {
                        close = true;
                    }
                }
                content(ui);
            });
            // after the content, so a window that handles the key itself keeps it
            if want_undo && class != ViewportClass::EmbeddedWindow {
                undo_req = undo_req.or(super::history::read_undo_key(&ctx));
            }
        });
        if undo_req.is_some() {
            self.undo_request = undo_req;
        }
        if close {
            *open = false;
        }
        let force = !*open;
        self.persist(spec.key, force);
        if force {
            self.wins.remove(spec.key);
        }
    }

    /// The window was closed: write its geometry and drop the state.
    fn forget(&mut self, key: &'static str) {
        if self.wins.contains_key(key) {
            self.persist(key, true);
            self.wins.remove(key);
        }
    }

    fn persist(&mut self, key: &'static str, force: bool) {
        let no_save = self.no_save;
        let path = self.path();
        let Some(st) = self.wins.get_mut(key) else { return };
        if no_save || st.now == st.saved || !(force || st.last_save.elapsed() >= SAVE_EVERY) {
            return;
        }
        let Some(path) = path else { return };
        match store_geom(&path, key, &st.now) {
            Ok(()) => st.saved = st.now,
            Err(e) => eprintln!("window '{key}': settings not saved: {e}"),
        }
        st.last_save = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pw_vp_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("settings.json")
    }

    #[test]
    fn defaults_without_saved_geometry() {
        let (pos, size) = restore(Geom::default(), [800.0, 600.0], |_, _| true);
        assert_eq!((pos, size), (None, [800.0, 600.0]));
    }

    #[test]
    fn saved_position_on_screen_is_kept() {
        let g = Geom { pos: Some([2100.0, 80.0]), size: Some([900.0, 700.0]) };
        let (pos, size) = restore(g, [800.0, 600.0], |_, _| true);
        assert_eq!((pos, size), (Some([2100.0, 80.0]), [900.0, 700.0]));
    }

    #[test]
    fn unplugged_monitor_falls_back_and_stays_visible() {
        // only a screen 0..1920 wide is left
        let on = |p: [f32; 2], _: [f32; 2]| p[0] >= 0.0 && p[0] < 1800.0;
        let g = Geom { pos: Some([2100.0, 80.0]), size: Some([900.0, 700.0]) };
        let (pos, size) = restore(g, [800.0, 600.0], on);
        assert_eq!(pos, Some(FALLBACK_POS));
        assert_eq!(size, [900.0, 700.0]);
        assert!(on(FALLBACK_POS, size));
    }

    #[test]
    fn parked_or_broken_values_are_not_used() {
        for p in [[-32000.0, -32000.0], [f32::NAN, 5.0], [-10001.0, 0.0]] {
            let (pos, _) = restore(Geom { pos: Some(p), size: None }, [800.0, 600.0], |_, _| true);
            assert_eq!(pos, Some(FALLBACK_POS));
        }
        let (_, size) = restore(Geom { pos: None, size: Some([3.0, 99999.0]) }, [800.0, 600.0], |_, _| true);
        assert_eq!(size, [800.0, 600.0]);
        assert!(!size_ok([f32::INFINITY, 300.0]));
    }

    #[test]
    fn negative_position_on_a_left_monitor_is_allowed() {
        let g = Geom { pos: Some([-1500.0, 100.0]), size: Some([900.0, 700.0]) };
        let (pos, _) = restore(g, [800.0, 600.0], |p, _| p[0] > -1920.0);
        assert_eq!(pos, Some([-1500.0, 100.0]));
    }

    #[test]
    fn geometry_roundtrip_keeps_other_settings() {
        let mut root = serde_json::json!({"patch_window": {"zoom": 3}, "windows": {"b": {"pos": [1.0, 2.0], "size": null}}});
        let g = Geom { pos: Some([10.0, 20.0]), size: Some([640.0, 480.0]) };
        write_geom(&mut root, "masks", &g);
        assert_eq!(read_geom(&root, "masks"), g);
        assert_eq!(read_geom(&root, "b").pos, Some([1.0, 2.0]));
        assert_eq!(root["patch_window"]["zoom"], 3);
        assert_eq!(read_geom(&root, "none"), Geom::default());
        // garbage in the file gives defaults, not a panic
        let mut bad = serde_json::json!({"windows": 5});
        assert_eq!(read_geom(&bad, "masks"), Geom::default());
        write_geom(&mut bad, "masks", &g);
        assert_eq!(read_geom(&bad, "masks"), g);
        let mut not_object = serde_json::json!([1, 2]);
        write_geom(&mut not_object, "m", &g);
        assert_eq!(read_geom(&not_object, "m"), g);
    }

    #[test]
    fn file_roundtrip_and_persist_rules() {
        let path = tmp("file");
        let g = Geom { pos: Some([5.0, 6.0]), size: Some([700.0, 500.0]) };
        store_geom(&path, "masks", &g).unwrap();
        store_geom(&path, "graph", &Geom { pos: None, size: Some([300.0, 300.0]) }).unwrap();
        let root = load_root(&path);
        assert_eq!(read_geom(&root, "masks"), g);
        assert_eq!(read_geom(&root, "graph").size, Some([300.0, 300.0]));

        let mut v = ViewportWindows { path: Some(path.clone()), ..Default::default() };
        let st = |now| WinState { now, saved: Geom::default(), last_save: Instant::now(), start_pos: None, start_size: [1.0, 1.0], focus_once: false };
        let moved = Geom { pos: Some([9.0, 9.0]), size: Some([800.0, 600.0]) };
        v.wins.insert("cp", st(moved));
        v.persist("cp", false); // too soon: not written
        assert_eq!(read_geom(&load_root(&path), "cp"), Geom::default());
        v.persist("cp", true);
        assert_eq!(read_geom(&load_root(&path), "cp"), moved);
        // automatic runs write nothing
        v.no_save = true;
        v.wins.insert("ov", st(moved));
        v.persist("ov", true);
        assert_eq!(read_geom(&load_root(&path), "ov"), Geom::default());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn closing_forgets_the_state() {
        let mut v = ViewportWindows { no_save: true, ..Default::default() };
        v.wins.insert("m", WinState { now: Geom::default(), saved: Geom::default(), last_save: Instant::now(), start_pos: None, start_size: [1.0, 1.0], focus_once: false });
        v.forget("m");
        assert!(v.wins.is_empty());
    }
}
