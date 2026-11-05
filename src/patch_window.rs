//! Patch comparison window: shows every patch comparison of the fine alignment, live while
//! the matching runs and afterwards as a list with a detail view.
//!
//! - its own OS window (egui viewport), movable to a second monitor, opened by a button;
//! - all records live in [`crate::patch_store::STORE`]; closing the window loses nothing;
//! - the matching never depends on this module: it only writes into the store and, while the
//!   window is open, sends events (thread-safe queue, read here in the GUI thread, throttled);
//! - pictures (thumbnails, full score fields, image export) are computed in worker threads;
//! - window size, position, column widths, sorting, filters and thumbnail size are kept in the
//!   tool's settings file.
//!
//! Assumptions (as agreed): patches shifted by hand keep their dx/dy when the optimisation is
//! restarted and are not matched again; only the current round is kept; no jumping to the
//! patch in the panorama; no typing of dx/dy; no CSV/HTML export.

use eframe::egui;
use crate::fine_align::{self, FineStats};
use crate::patch_matcher;
use crate::patch_store::{self as ps, Change, Event, LiveFrame, PatchData, PatchState, Phase, RoundInfo, Status, STORE};
use crate::patch_viz::{self as pv, Colormap, DiffSpace, HeatMode};
use crate::stitch::{Mapper, PanoView};
use egui::{Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Vec2};
use image::RgbImage;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The window is redrawn at most this often while data arrive (ms); events in between are
/// merged and only the newest state is drawn.
pub const GUI_REFRESH_MS: u64 = 80;
/// New table rows are taken over in batches at most this often (ms).
pub const TABLE_BATCH_MS: u64 = 100;
/// Thumbnail size (pixels): default and range of the slider.
pub const THUMB_DEFAULT: f32 = 96.0;
pub const THUMB_RANGE: (f32, f32) = (48.0, 192.0);
/// Thumbnail textures kept at most (only visible rows ask for one).
pub const THUMB_CACHE_MAX: usize = 3000;
/// Thumbnails computed in the background at the same time at most.
pub const THUMB_JOBS_MAX: usize = 48;
/// Detail view zoom (100 % .. 800 %).
pub const ZOOM_RANGE: (usize, usize) = (1, 8);
pub const ZOOM_DEFAULT: usize = 4;
/// Image export: default zoom factor (nearest neighbour).
pub const EXPORT_ZOOM_DEFAULT: usize = 4;
pub const EXPORT_ZOOM_RANGE: (usize, usize) = (1, 16);
/// Settings are written at most this often (ms).
pub const SETTINGS_SAVE_MS: u64 = 1000;
/// Keyboard steps for shifting by hand (pixels): plain, with Shift, with Ctrl.
pub const KEY_STEP: f32 = 1.0;
pub const KEY_STEP_FINE: f32 = 0.1;
pub const KEY_STEP_COARSE: f32 = 10.0;
/// Size of the live preview pictures and of the score map in the detail view (points).
const LIVE_CANVAS: f32 = 170.0;
const LIVE_DIFF: f32 = 120.0;
const MAP_BOX: f32 = 230.0;
const ROW_PAD: f32 = 3.0;
/// How much darker (lighter in light mode) every second row is.
const ALT_ROW_SHADE: f32 = 0.12;
const HEADER_H: f32 = 24.0;
const MIN_COL_W: f32 = 28.0;
const COLORBAR_STEPS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Hash)]
pub enum Col {
    Thumb,
    Id,
    Pair,
    Pos,
    Dx,
    Dy,
    Mag,
    Score,
    Uniq,
    Tex,
    Conf,
    Status,
    Active,
}

const COLS: [Col; 13] = [Col::Thumb, Col::Id, Col::Pair, Col::Pos, Col::Dx, Col::Dy, Col::Mag, Col::Score, Col::Uniq, Col::Tex, Col::Conf, Col::Status, Col::Active];

impl Col {
    fn title(&self) -> &'static str {
        match self {
            Col::Thumb => "Difference",
            Col::Id => "ID",
            Col::Pair => "Photo pair",
            Col::Pos => "Position (x, y)",
            Col::Dx => "dx",
            Col::Dy => "dy",
            Col::Mag => "Shift",
            Col::Score => "Score",
            Col::Uniq => "Uniqueness",
            Col::Tex => "Texture",
            Col::Conf => "Confidence",
            Col::Status => "Status",
            Col::Active => "Active",
        }
    }
    fn default_w(&self) -> f32 {
        match self {
            Col::Thumb => 104.0,
            Col::Id => 52.0,
            Col::Pair => 190.0,
            Col::Pos => 120.0,
            Col::Dx | Col::Dy | Col::Mag => 64.0,
            Col::Score => 64.0,
            Col::Uniq => 84.0,
            Col::Tex => 84.0,
            Col::Conf => 80.0,
            Col::Status => 250.0,
            Col::Active => 54.0,
        }
    }
    fn numeric(&self) -> bool {
        matches!(self, Col::Id | Col::Dx | Col::Dy | Col::Mag | Col::Score | Col::Uniq | Col::Tex | Col::Conf)
    }
    fn sortable(&self) -> bool {
        *self != Col::Thumb
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ViewMode {
    Side,
    Overlay,
    Wipe,
}

/// Which patches the image export writes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ExportWhich {
    All,
    Filtered,
    Selected,
}

/// Filter bar (all filters combined with AND). Thresholds are typed text ("" = no limit).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Filter {
    pub pair: Option<(usize, usize)>,
    /// valid, rejected, disabled by hand, shifted by hand
    pub show: [bool; 4],
    pub reason: Option<String>,
    pub score_min: String,
    pub score_max: String,
    pub conf_min: String,
    pub conf_max: String,
    pub mag_min: String,
    pub mag_max: String,
}

impl Default for Filter {
    fn default() -> Self {
        Filter {
            pair: None,
            show: [true; 4],
            reason: None,
            score_min: String::new(),
            score_max: String::new(),
            conf_min: String::new(),
            conf_max: String::new(),
            mag_min: String::new(),
            mag_max: String::new(),
        }
    }
}

const CATEGORY_NAMES: [&str; 4] = ["valid", "rejected", "disabled by hand", "shifted by hand"];

/// Everything the window keeps in the settings file.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WinSettings {
    pub pos: Option<[f32; 2]>,
    pub size: [f32; 2],
    pub col_w: Vec<f32>,
    pub sort: Option<(Col, bool)>,
    pub filter: Filter,
    pub thumb: f32,
    pub heat: HeatMode,
    pub space: DiffSpace,
    pub auto_scale: bool,
    pub live: bool,
    pub sample_every: u32,
    pub delay_ms: u32,
    pub manual_conf: f32,
    pub zoom: usize,
    pub view: ViewMode,
    pub alpha: f32,
    pub wipe: f32,
    pub detail_w: f32,
    pub export_zoom: usize,
    pub export_overlay: bool,
    pub export_which: ExportWhich,
}

impl Default for WinSettings {
    fn default() -> Self {
        WinSettings {
            pos: None,
            size: [1280.0, 860.0],
            col_w: COLS.iter().map(|c| c.default_w()).collect(),
            // like the original's control point list: number column, ascending
            sort: Some((Col::Id, true)),
            filter: Filter::default(),
            thumb: THUMB_DEFAULT,
            heat: HeatMode::AbsColor(Colormap::Viridis),
            space: DiffSpace::Feature,
            auto_scale: true,
            live: true,
            sample_every: ps::SAMPLE_EVERY_DEFAULT,
            delay_ms: ps::FRAME_DELAY_DEFAULT_MS,
            manual_conf: ps::MANUAL_CONFIDENCE_DEFAULT,
            zoom: ZOOM_DEFAULT,
            view: ViewMode::Side,
            alpha: 0.5,
            wipe: 0.5,
            detail_w: 560.0,
            export_zoom: EXPORT_ZOOM_DEFAULT,
            export_overlay: true,
            export_which: ExportWhich::Filtered,
        }
    }
}

/// Where a window goes when its saved position is not usable: the original's dialogs read
/// a saved position of -1 and use (0, 44) when either is negative.
const FALLBACK_POS: [f32; 2] = [0.0, 44.0];

/// Windows moves minimised windows to about (-32000, -32000); positions below this are ignored.
const PARKED_POS: f32 = -10000.0;
/// Part of the title bar that must lie on a screen (points): half width and height.
const TITLE_PROBE: (f32, f32) = (60.0, 24.0);

#[cfg(windows)]
mod screen {
    #[repr(C)]
    pub struct WinRect {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
    }
    pub const MONITOR_DEFAULTTONULL: u32 = 0;
    #[link(name = "user32")]
    unsafe extern "system" {
        pub fn MonitorFromRect(r: *const WinRect, flags: u32) -> isize;
    }
}

/// Is the middle of the title bar of a window at `pos` (points) on a connected monitor?
pub(crate) fn on_screen(pos: [f32; 2], size: [f32; 2], ppp: f32) -> bool {
    if pos[0] <= PARKED_POS || pos[1] <= PARKED_POS {
        return false;
    }
    #[cfg(windows)]
    {
        let cx = pos[0] + size[0] * 0.5;
        let r = screen::WinRect {
            left: ((cx - TITLE_PROBE.0) * ppp) as i32,
            top: (pos[1] * ppp) as i32,
            right: ((cx + TITLE_PROBE.0) * ppp) as i32,
            bottom: ((pos[1] + TITLE_PROBE.1) * ppp) as i32,
        };
        // SAFETY: plain Win32 call with a valid rectangle
        unsafe { screen::MonitorFromRect(&r, screen::MONITOR_DEFAULTTONULL) != 0 }
    }
    #[cfg(not(windows))]
    {
        let _ = (size, ppp);
        true
    }
}

fn settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("PanoWeave").join("settings.json"))
}

fn load_settings() -> WinSettings {
    let Some(p) = settings_path() else { return WinSettings::default() };
    let Ok(text) = std::fs::read_to_string(&p) else { return WinSettings::default() };
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let mut s: WinSettings = v.get("patch_window").and_then(|w| serde_json::from_value(w.clone()).ok()).unwrap_or_default();
    if s.col_w.len() != COLS.len() {
        s.col_w = COLS.iter().map(|c| c.default_w()).collect();
    }
    s.thumb = s.thumb.clamp(THUMB_RANGE.0, THUMB_RANGE.1);
    s.zoom = s.zoom.clamp(ZOOM_RANGE.0, ZOOM_RANGE.1);
    s
}

fn save_settings(s: &WinSettings) -> Result<(), String> {
    let p = settings_path().ok_or("no settings folder")?;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let mut v: serde_json::Value = std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(serde_json::json!({}));
    if !v.is_object() {
        v = serde_json::json!({});
    }
    v["patch_window"] = serde_json::to_value(s).map_err(|e| e.to_string())?;
    std::fs::write(&p, serde_json::to_string_pretty(&v).unwrap_or_default()).map_err(|e| e.to_string())
}

/// What the main window gives the patch window each frame.
pub struct PatchCtx<'a> {
    pub names: Vec<String>,
    pub rgb: Vec<Arc<RgbImage>>,
    /// brightness gain per photo (exposure matching, linear light)
    pub gains: Vec<[f32; 3]>,
    pub pano: Option<(PanoView, f64)>,
    pub busy: bool,
    pub stats: Option<&'a FineStats>,
}

pub enum PatchAction {
    Restart,
}

struct ExportJob {
    progress: Arc<Mutex<(usize, usize, String)>>,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

#[derive(Default)]
struct DetailTex {
    key: String,
    a: Option<TextureHandle>,
    b: Option<TextureHandle>,
    before: Option<TextureHandle>,
    after: Option<TextureHandle>,
    n: usize,
    scale: f32,
    mean_before: Option<f32>,
    mean_after: Option<f32>,
    note: String,
    map_key: String,
    map: Option<TextureHandle>,
    map_n: usize,
    map_lo: f32,
    map_hi: f32,
    map_vals: Vec<f32>,
    live_score: Option<f32>,
}

#[derive(Default)]
struct LiveTex {
    b_key: usize,
    b: Option<TextureHandle>,
    a: Option<TextureHandle>,
    diff: Option<TextureHandle>,
}

pub struct PatchWindow {
    pub open: bool,
    /// automated runs (screenshots) never write the person's settings file
    pub no_save: bool,
    /// automated runs: scroll the detail view to this offset
    pub demo_scroll: Option<f32>,
    /// Ctrl+Z / Ctrl+Y typed in this window (the app undoes / redoes the control points)
    pub undo_request: Option<crate::app::history::UndoKey>,
    pano_key: String,
    save_now: bool,
    /// focus the window once after it was opened
    focus_once: bool,
    s: WinSettings,
    saved: WinSettings,
    last_save: Instant,
    builder_pos: Option<[f32; 2]>,
    builder_size: [f32; 2],
    rx: Option<Receiver<Event>>,
    version: u64,
    last_snapshot: Instant,
    data: Vec<Arc<PatchData>>,
    state: Vec<PatchState>,
    by_id: HashMap<u32, usize>,
    pos: Vec<Option<(f64, f64)>>,
    info: RoundInfo,
    round_no: u64,
    rows: Vec<usize>,
    rows_key: String,
    sel: HashSet<u32>,
    anchor: Option<usize>,
    detail: Option<u32>,
    pan: Vec2,
    drag_shift: Option<(f32, f32)>,
    drag_start: (f32, f32),
    dtex: DetailTex,
    full_maps: HashMap<u32, Arc<Vec<f32>>>,
    full_pending: HashSet<u32>,
    full_tx: Sender<(u64, u32, Arc<Vec<f32>>)>,
    full_rx: Receiver<(u64, u32, Arc<Vec<f32>>)>,
    thumbs: HashMap<u32, (String, TextureHandle, bool)>,
    thumb_pending: HashSet<u32>,
    thumb_tx: Sender<(u64, u32, String, Option<(usize, Vec<u8>)>, bool)>,
    thumb_rx: Receiver<(u64, u32, String, Option<(usize, Vec<u8>)>, bool)>,
    thumb_gen: u64,
    /// finished thumbnails waiting for upload (textures are made in the GUI thread)
    thumb_queue: Vec<(u32, String, usize, Vec<u8>, bool)>,
    live_new: Option<Box<LiveFrame>>,
    live_shown: Option<Box<LiveFrame>>,
    live_tex: LiveTex,
    last_live_draw: Instant,
    paused: bool,
    current: Option<(u32, usize, usize, usize)>,
    want_view: Option<u32>,
    mean_mag: f32,
    median_mag: f32,
    export: Option<ExportJob>,
    msg: String,
    action: Option<PatchAction>,
}

fn lerp_col(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

const C_RED: Color32 = Color32::from_rgb(220, 60, 50);
const C_YELLOW: Color32 = Color32::from_rgb(230, 200, 40);
const C_GREEN: Color32 = Color32::from_rgb(40, 190, 80);
const C_GREY: Color32 = Color32::from_rgb(120, 120, 125);

/// Continuous red - yellow - green by confidence (0, 0.5, 1).
fn conf_colour(c: f32) -> Color32 {
    if c < 0.5 { lerp_col(C_RED, C_YELLOW, c / 0.5) } else { lerp_col(C_YELLOW, C_GREEN, (c - 0.5) / 0.5) }
}

/// Row background: the status colour blended into the theme's background so the theme's text
/// colour stays readable in dark and light mode.
fn row_bg(c: Color32, dark: bool, bg: Color32) -> Color32 {
    if dark { lerp_col(bg, c, 0.30) } else { lerp_col(Color32::WHITE, c, 0.32) }
}

fn category(st: &PatchState) -> usize {
    if st.status == Status::DisabledManual {
        2
    } else if st.manual {
        3
    } else if st.status == Status::Rejected {
        1
    } else {
        0
    }
}

fn parse(s: &str) -> Option<f32> {
    s.trim().replace(',', ".").parse::<f32>().ok()
}

fn colour_image(w: usize, h: usize, px: &[u8]) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied([w, h], px)
}

/// Grey picture of a feature plane, stretched to its 1..99 % range (for the live preview).
fn grey_plane(v: &[f32], m: &[bool], s: usize) -> egui::ColorImage {
    let mut vals: Vec<f32> = v.iter().zip(m).filter(|(_, ok)| **ok).map(|(x, _)| *x).collect();
    let (lo, hi) = if vals.len() > 4 {
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        (vals[vals.len() / 100], vals[vals.len() * 99 / 100])
    } else {
        (0.0, 1.0)
    };
    let span = (hi - lo).max(1e-6);
    let mut px = vec![0u8; s * s * 4];
    for i in 0..s * s {
        let c = if m[i] { (((v[i] - lo) / span).clamp(0.0, 1.0) * 255.0) as u8 } else { 60 };
        px[i * 4..i * 4 + 4].copy_from_slice(&[c, c, c, 255]);
    }
    colour_image(s, s, &px)
}

fn fmt_opt(v: f32, d: usize) -> String {
    if v.is_finite() { format!("{v:.d$}") } else if v.is_infinite() { "∞".into() } else { "–".into() }
}

fn fmt_bytes(b: usize) -> String {
    if b >= 1 << 30 { format!("{:.2} GB", b as f64 / (1u64 << 30) as f64) } else { format!("{:.1} MB", b as f64 / (1u64 << 20) as f64) }
}

fn sanitize(s: &str) -> String {
    let stem = std::path::Path::new(s).file_stem().map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|| s.to_string());
    stem.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

impl Default for PatchWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl PatchWindow {
    pub fn new() -> Self {
        let s = load_settings();
        let (full_tx, full_rx) = channel();
        let (thumb_tx, thumb_rx) = channel();
        PatchWindow {
            open: false,
            no_save: false,
            demo_scroll: None,
            pano_key: String::new(),
            save_now: false,
            focus_once: false,
            undo_request: None,
            saved: s.clone(),
            builder_pos: s.pos,
            builder_size: s.size,
            s,
            last_save: Instant::now(),
            rx: None,
            version: 0,
            last_snapshot: Instant::now() - Duration::from_secs(10),
            data: Vec::new(),
            state: Vec::new(),
            by_id: HashMap::new(),
            pos: Vec::new(),
            info: RoundInfo::default(),
            round_no: 0,
            rows: Vec::new(),
            rows_key: String::new(),
            sel: HashSet::new(),
            anchor: None,
            detail: None,
            pan: Vec2::ZERO,
            drag_shift: None,
            drag_start: (0.0, 0.0),
            dtex: DetailTex::default(),
            full_maps: HashMap::new(),
            full_pending: HashSet::new(),
            full_tx,
            full_rx,
            thumbs: HashMap::new(),
            thumb_pending: HashSet::new(),
            thumb_tx,
            thumb_rx,
            thumb_gen: 1,
            thumb_queue: Vec::new(),
            live_new: None,
            live_shown: None,
            live_tex: LiveTex::default(),
            last_live_draw: Instant::now(),
            paused: false,
            current: None,
            want_view: None,
            mean_mag: f32::NAN,
            median_mag: f32::NAN,
            export: None,
            msg: String::new(),
            action: None,
        }
    }

    /// Open the detail view of the first listed patch as soon as there is one (demo / tests).
    pub fn open_first_detail(&mut self, view: ViewMode) {
        self.s.view = view;
        if self.detail.is_none() {
            self.ensure_rows();
            if let Some(&i) = self.rows.first() {
                let id = self.data[i].id;
                self.sel.insert(id);
                self.open_detail(id);
            }
        }
    }

    /// Called every frame while the window is closed: stop listening, keep nothing in the GUI.
    pub fn closed_tick(&mut self) {
        if self.rx.is_some() {
            STORE.unsubscribe();
            self.rx = None;
            self.live_new = None;
            self.live_shown = None;
            self.live_tex = LiveTex::default();
            // textures and the snapshot are dropped so no record is held by the window
            self.thumbs.clear();
            self.thumb_pending.clear();
            self.dtex = DetailTex::default();
            self.data.clear();
            self.state.clear();
            self.by_id.clear();
            self.pos.clear();
            self.rows.clear();
            self.rows_key.clear();
            self.version = 0;
            self.persist(true);
        }
    }

    fn persist(&mut self, force: bool) {
        if !self.no_save && self.s != self.saved && (force || self.last_save.elapsed() >= Duration::from_millis(SETTINGS_SAVE_MS)) {
            if let Err(e) = save_settings(&self.s) {
                eprintln!("patch window: settings not saved: {e}");
            }
            self.saved = self.s.clone();
            self.last_save = Instant::now();
        }
    }

    /// Show the window (if open). Returns an action for the main window.
    pub fn show(&mut self, ctx: &egui::Context, pc: &PatchCtx) -> Option<PatchAction> {
        if !self.open {
            self.closed_tick();
            return None;
        }
        if self.rx.is_none() {
            self.rx = Some(STORE.subscribe());
            // a saved position is only used when it lies on a connected screen (a second monitor
            // may be gone, or Windows reported the parking position of a minimised window)
            let ppp = ctx.input(|i| i.viewport().native_pixels_per_point).unwrap_or(1.0);
            // like the original's dialogs: a negative
            // saved coordinate means "not usable" and the window goes to (0, 44)
            let wanted = self.s.pos.map(|p| if p[0] < 0.0 || p[1] < 0.0 { FALLBACK_POS } else { p });
            self.builder_pos = match wanted {
                Some(p) if on_screen(p, self.s.size, ppp) => Some(p),
                Some(p) => {
                    eprintln!("patch window: saved position {p:?} is not on any screen – opening at {FALLBACK_POS:?}");
                    Some(FALLBACK_POS)
                }
                None => None,
            };
            self.s.pos = self.builder_pos;
            self.builder_size = self.s.size;
            self.focus_once = true;
            self.last_snapshot = Instant::now() - Duration::from_secs(10);
        }
        STORE.set_live(self.s.live, self.s.sample_every, self.s.delay_ms);
        self.pump(pc);
        let mut b = egui::ViewportBuilder::default().with_title("Patch comparison").with_inner_size(self.builder_size).with_min_inner_size([760.0, 520.0]);
        if let Some(p) = self.builder_pos {
            b = b.with_position(p);
        }
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("patch_compare"), b, |ui, class| {
            let (close, outer, inner) = ui.ctx().input(|i| (i.viewport().close_requested(), i.viewport().outer_rect, i.viewport().inner_rect));
            if close {
                self.open = false;
            }
            if std::mem::take(&mut self.focus_once) && class != egui::ViewportClass::EmbeddedWindow {
                // bring the new window to the front
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            let minimized = ui.ctx().input(|i| i.viewport().minimized).unwrap_or(false);
            if class != egui::ViewportClass::EmbeddedWindow && !minimized {
                if let Some(r) = outer {
                    // never keep the far-away position Windows gives minimised windows
                    if r.min.x > PARKED_POS && r.min.y > PARKED_POS {
                        self.s.pos = Some([r.min.x, r.min.y]);
                    }
                }
                if let Some(r) = inner {
                    if r.width() >= 1.0 && r.height() >= 1.0 {
                        self.s.size = [r.width(), r.height()];
                    }
                }
            }
            self.ui(ui, pc);
            // Ctrl+Z / Ctrl+Y act on the control points; the app performs the step
            if let Some(k) = crate::app::history::read_undo_key(ui.ctx()) {
                self.undo_request = Some(k);
            }
        });
        let running = self.info.phase == Phase::Running || !self.thumb_pending.is_empty() || !self.full_pending.is_empty() || self.export.is_some();
        if running {
            ctx.request_repaint_after(Duration::from_millis(GUI_REFRESH_MS));
        }
        let now = std::mem::take(&mut self.save_now);
        self.persist(now);
        if !self.open {
            self.closed_tick();
        }
        self.action.take()
    }

    // ---------------- data ----------------

    /// Take over events and (in batches) new records. Runs in the GUI thread.
    fn pump(&mut self, pc: &PatchCtx) {
        let mut force = false;
        if let Some(rx) = &self.rx {
            while let Ok(e) = rx.try_recv() {
                match e {
                    Event::MatchingStarted { .. } => {
                        force = true;
                        self.current = None;
                    }
                    Event::PatchStarted { id, levels, image_a, image_b } => self.current = Some((id, levels, image_a, image_b)),
                    Event::PatchProgress(f) => self.live_new = Some(f),
                    Event::PatchFinished { .. } => {}
                    Event::MatchingFinished { .. } | Event::MatchingCancelled | Event::Freed => force = true,
                }
            }
        }
        // live preview: only the newest frame, at most every GUI_REFRESH_MS
        if !self.paused && self.last_live_draw.elapsed() >= Duration::from_millis(GUI_REFRESH_MS) {
            if let Some(f) = self.live_new.take() {
                self.live_shown = Some(f);
                self.last_live_draw = Instant::now();
            }
        }
        let pk = pc.pano.as_ref().map(|(v, w)| format!("{:?}{:?}{:.0}", v.bounds, v.projection, w)).unwrap_or_default();
        if pk != self.pano_key {
            self.pano_key = pk;
            force = true;
            self.version = 0;
        }
        let v = STORE.version();
        if v != self.version && (force || self.last_snapshot.elapsed() >= Duration::from_millis(TABLE_BATCH_MS)) {
            self.reload(pc);
        }
        while let Ok((generation, id, sig, img, rejected)) = self.thumb_rx.try_recv() {
            self.thumb_pending.remove(&id);
            if generation != self.thumb_gen {
                continue;
            }
            match img {
                Some((w, px)) => {
                    if self.thumbs.len() >= THUMB_CACHE_MAX {
                        self.thumbs.clear();
                    }
                    // the texture is drawn at the slider size by the GPU (no recomputation)
                    let _ = (id, &sig);
                    self.pending_thumb_texture(id, sig, w, px, rejected);
                }
                None => eprintln!("patch window: thumbnail of patch {id} could not be drawn (no image data)"),
            }
        }
        while let Ok((round, id, map)) = self.full_rx.try_recv() {
            self.full_pending.remove(&id);
            if round == self.round_no {
                self.full_maps.insert(id, map);
                self.dtex.map_key.clear();
            }
        }
        if let Some(job) = &self.export {
            if job.done.load(Ordering::Acquire) {
                self.msg = job.progress.lock().map(|g| g.2.clone()).unwrap_or_default();
                self.export = None;
            }
        }
    }

    fn pending_thumb_texture(&mut self, id: u32, sig: String, w: usize, px: Vec<u8>, rejected: bool) {
        self.thumb_queue.push((id, sig, w, px, rejected));
    }

    fn reload(&mut self, pc: &PatchCtx) {
        let (v, data, state) = STORE.snapshot();
        let info = STORE.info();
        if info.number != self.round_no || info.phase == Phase::Freed {
            // a new round (or freed): drop everything that belongs to the old one
            self.round_no = info.number;
            self.thumbs.clear();
            self.thumb_pending.clear();
            self.thumb_gen += 1;
            self.full_maps.clear();
            self.full_pending.clear();
            self.sel.clear();
            self.anchor = None;
            self.detail = None;
            self.dtex = DetailTex::default();
            self.drag_shift = None;
        }
        self.version = v;
        self.last_snapshot = Instant::now();
        self.by_id = data.iter().enumerate().map(|(i, d)| (d.id, i)).collect();
        // positions in the panorama (output pixels at the natural size)
        let mapper = pc.pano.as_ref().map(|(view, w)| {
            let h = (w / view.aspect()).max(1.0);
            (Mapper::new(view, view.bounds, *w as usize, h as usize), view.rotation())
        });
        self.pos = data.iter().map(|d| mapper.as_ref().and_then(|(m, r)| m.to_px(d.dir, r))).collect();
        let mut mags: Vec<f32> = state.iter().filter(|s| s.status == Status::Valid).map(|s| s.magnitude()).filter(|m| m.is_finite()).collect();
        if mags.is_empty() {
            self.mean_mag = f32::NAN;
            self.median_mag = f32::NAN;
        } else {
            self.mean_mag = mags.iter().sum::<f32>() / mags.len() as f32;
            mags.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            self.median_mag = mags[mags.len() / 2];
        }
        self.data = data;
        self.state = state;
        self.info = info;
        self.rows_key.clear();
        if let Some(w) = self.want_view {
            if self.by_id.contains_key(&w) {
                self.want_view = None;
                self.open_detail(w);
            }
        }
    }

    fn passes(&self, i: usize) -> bool {
        let (d, st) = (&self.data[i], &self.state[i]);
        let f = &self.s.filter;
        if let Some(p) = f.pair {
            if (d.image_a, d.image_b) != p {
                return false;
            }
        }
        if !f.show[category(st)] {
            return false;
        }
        if let Some(r) = &f.reason {
            if st.reason != r.as_str() {
                return false;
            }
        }
        let within = |v: f32, lo: &str, hi: &str| {
            if let Some(l) = parse(lo) {
                if !(v >= l) {
                    return false;
                }
            }
            if let Some(h) = parse(hi) {
                if !(v <= h) {
                    return false;
                }
            }
            true
        };
        within(st.score, &f.score_min, &f.score_max) && within(st.confidence, &f.conf_min, &f.conf_max) && within(st.magnitude(), &f.mag_min, &f.mag_max)
    }

    fn sort_value(&self, i: usize, c: Col) -> f64 {
        let (d, st) = (&self.data[i], &self.state[i]);
        match c {
            Col::Thumb | Col::Id => d.id as f64,
            Col::Pair => (d.image_a * 100_000 + d.image_b) as f64,
            Col::Pos => self.pos[i].map(|p| p.0 * 1e6 + p.1).unwrap_or(f64::MAX),
            Col::Dx => st.dx as f64,
            Col::Dy => st.dy as f64,
            Col::Mag => st.magnitude() as f64,
            Col::Score => st.score as f64,
            Col::Uniq => {
                if d.uniqueness.is_infinite() {
                    1e12
                } else {
                    d.uniqueness as f64
                }
            }
            Col::Tex => d.texture as f64,
            Col::Conf => st.confidence as f64,
            Col::Status => category(st) as f64,
            Col::Active => st.active() as u8 as f64,
        }
    }

    fn ensure_rows(&mut self) {
        let key = format!("{}|{:?}|{:?}", self.version, self.s.sort, self.s.filter);
        if key == self.rows_key {
            return;
        }
        let mut rows: Vec<usize> = (0..self.data.len()).filter(|&i| self.passes(i)).collect();
        match self.s.sort {
            Some((c, asc)) => {
                let mut keyed: Vec<(f64, u32, usize)> = rows.iter().map(|&i| (self.sort_value(i, c), self.data[i].id, i)).collect();
                keyed.sort_by(|a, b| {
                    let o = a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1));
                    if asc { o } else { o.reverse() }
                });
                rows = keyed.into_iter().map(|k| k.2).collect();
            }
            None => rows.sort_by_key(|&i| self.data[i].id),
        }
        self.rows = rows;
        self.rows_key = key;
    }

    fn pair_name(&self, pc: &PatchCtx, a: usize, b: usize) -> String {
        let n = |i: usize| pc.names.get(i).map(|s| sanitize(s)).unwrap_or_else(|| format!("photo {}", i + 1));
        format!("{} – {}", n(a), n(b))
    }

    fn status_text(&self, st: &PatchState) -> String {
        let mut t = match category(st) {
            0 => "valid".to_string(),
            1 => format!("rejected: {}", st.reason),
            2 => "disabled by hand".to_string(),
            _ => "shifted by hand".to_string(),
        };
        if let Some(c) = st.changed {
            t.push_str(&format!(" · ✎ {} – not applied yet", c.name()));
        }
        t
    }

    // ---------------- edits ----------------

    fn set_active(&mut self, ids: &[u32], active: bool) {
        STORE.edit(ids, |_, st| {
            if active && st.status == Status::DisabledManual {
                st.status = st.prev_status;
                st.reason = st.prev_reason;
                st.changed = if st.changed == Some(Change::Disabled) { None } else { Some(Change::Enabled) };
            } else if !active && st.status != Status::DisabledManual {
                st.prev_status = st.status;
                st.prev_reason = st.reason;
                st.status = Status::DisabledManual;
                st.reason = "disabled by hand";
                st.changed = if st.changed == Some(Change::Enabled) { None } else { Some(Change::Disabled) };
            }
        });
        self.last_snapshot = Instant::now() - Duration::from_secs(10);
    }

    fn reset(&mut self, ids: &[u32]) {
        STORE.edit(ids, |d, st| {
            if !st.manual {
                return;
            }
            st.manual = false;
            st.dx = d.auto.dx;
            st.dy = d.auto.dy;
            st.score = d.auto.score;
            st.confidence = d.auto.confidence;
            st.cp = st.cp_auto;
            if st.status != Status::DisabledManual {
                st.status = st.auto_status;
                st.reason = st.auto_reason;
            } else {
                st.prev_status = st.auto_status;
                st.prev_reason = st.auto_reason;
            }
            st.changed = Some(Change::Reset);
        });
        self.last_snapshot = Instant::now() - Duration::from_secs(10);
    }

    fn commit_shift(&mut self, id: u32, shift: (f32, f32)) {
        let conf = self.s.manual_conf;
        STORE.edit(&[id], |d, st| {
            let (dx, dy) = shift;
            st.manual = true;
            st.dx = dx;
            st.dy = dy;
            // the score at the new position is computed again
            if let Some(rb) = d.region_b() {
                st.score = patch_matcher::score_at_shift(&d.region_a(), &rb, &d.cfg, dx, dy).unwrap_or(f32::NAN);
            }
            st.confidence = conf;
            st.cp = fine_align::manual_cp(d, st, dx, dy, conf);
            if st.status != Status::DisabledManual {
                st.status = Status::Valid;
                st.reason = "shifted by hand";
            }
            st.changed = Some(Change::Moved);
        });
        if st_missing_cp(&self.data, &self.by_id, id) {
            self.msg = format!("Patch {id}: the shifted spot lies outside photo B – it cannot be used.");
            eprintln!("{}", self.msg);
        }
        self.last_snapshot = Instant::now() - Duration::from_secs(10);
    }

    fn open_detail(&mut self, id: u32) {
        if self.detail != Some(id) {
            self.detail = Some(id);
            self.drag_shift = None;
            self.pan = Vec2::ZERO;
            self.dtex.key.clear();
            self.dtex.map_key.clear();
        }
    }

    fn step_detail(&mut self, delta: isize) {
        self.ensure_rows();
        let Some(id) = self.detail else { return };
        let Some(&i) = self.by_id.get(&id) else { return };
        let pos = self.rows.iter().position(|&r| r == i);
        let next = match pos {
            Some(p) => (p as isize + delta).clamp(0, self.rows.len() as isize - 1) as usize,
            None => 0,
        };
        if let Some(&r) = self.rows.get(next) {
            let nid = self.data[r].id;
            self.open_detail(nid);
            self.sel.clear();
            self.sel.insert(nid);
            self.anchor = Some(next);
        }
    }

    // ---------------- drawing ----------------

    fn ui(&mut self, ui: &mut egui::Ui, pc: &PatchCtx) {
        let ctx = ui.ctx().clone();
        self.flush_thumb_textures(&ctx);
        let fill = ui.visuals().panel_fill;
        let frame = egui::Frame::new().fill(fill).inner_margin(egui::Margin::symmetric(10, 4));
        egui::Panel::top("pw_live").resizable(false).frame(frame).show(ui, |ui| {
            ui.add_space(4.0);
            self.live_ui(ui, pc);
            ui.add_space(4.0);
        });
        egui::Panel::top("pw_filters").frame(frame.fill(ui.visuals().faint_bg_color.lerp_to_gamma(fill, 0.5))).show(ui, |ui| {
            ui.add_space(3.0);
            self.filter_ui(ui, pc);
            ui.add_space(3.0);
        });
        egui::Panel::bottom("pw_actions").frame(frame).show(ui, |ui| {
            ui.add_space(3.0);
            self.actions_ui(ui, pc);
            ui.add_space(3.0);
        });
        if self.detail.is_some() {
            let r = egui::Panel::right("pw_detail").resizable(true).default_size(self.s.detail_w).min_size(420.0).frame(frame).show(ui, |ui| {
                let mut sa = egui::ScrollArea::vertical().id_salt("pw_detail_scroll").auto_shrink([false, false]);
                if let Some(y) = self.demo_scroll {
                    sa = sa.vertical_scroll_offset(y);
                }
                sa.show(ui, |ui| self.detail_ui(ui, pc));
            });
            self.s.detail_w = r.response.rect.width().round();
        }
        egui::CentralPanel::default().frame(frame).show(ui, |ui| self.table_ui(ui, pc));
        self.keys(&ctx);
    }

    fn flush_thumb_textures(&mut self, ctx: &egui::Context) {
        for (id, sig, w, px, rejected) in std::mem::take(&mut self.thumb_queue) {
            let t = ctx.load_texture(format!("pw-thumb-{id}"), colour_image(w, w, &px), TextureOptions::NEAREST);
            self.thumbs.insert(id, (sig, t, rejected));
        }
    }

    fn keys(&mut self, ctx: &egui::Context) {
        if self.detail.is_none() || ctx.egui_wants_keyboard_input() {
            return;
        }
        let (m, up, down, left, right, pgup, pgdn) = ctx.input(|i| {
            (
                i.modifiers,
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::PageUp),
                i.key_pressed(egui::Key::PageDown),
            )
        });
        if pgup {
            self.step_detail(-1);
        }
        if pgdn {
            self.step_detail(1);
        }
        if self.s.view == ViewMode::Overlay {
            // arrows shift photo B against A
            let step = if m.shift { KEY_STEP_FINE } else if m.ctrl || m.command { KEY_STEP_COARSE } else { KEY_STEP };
            let (mut dx, mut dy) = (0.0, 0.0);
            if left {
                dx -= step;
            }
            if right {
                dx += step;
            }
            if up {
                dy -= step;
            }
            if down {
                dy += step;
            }
            if dx != 0.0 || dy != 0.0 {
                if let Some(id) = self.detail {
                    if let Some(&i) = self.by_id.get(&id) {
                        let st = &self.state[i];
                        // moving B right means the matching spot in B lies further left
                        let s = (st.dx - dx, st.dy - dy);
                        self.commit_shift(id, s);
                    }
                }
            }
        } else {
            if up || left {
                self.step_detail(-1);
            }
            if down || right {
                self.step_detail(1);
            }
        }
    }

    fn live_ui(&mut self, ui: &mut egui::Ui, pc: &PatchCtx) {
        let info = self.info.clone();
        ui.horizontal(|ui| {
            // --- the moving overlay ---
            ui.vertical(|ui| {
                ui.set_width(LIVE_CANVAS * 2.0 + LIVE_DIFF + 40.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Live preview").strong());
                    ui.checkbox(&mut self.s.live, "Live preview on").on_hover_text("Off: only progress and statistics, no moving overlay (fastest).");
                    let t = if self.paused { "▶ Resume display" } else { "⏸ Pause display" };
                    if ui.button(t).on_hover_text("Freezes only the display – the matching keeps running.").clicked() {
                        self.paused = !self.paused;
                    }
                    let cur = self.current.map(|c| c.0).or(self.live_shown.as_ref().map(|f| f.id));
                    if ui.add_enabled(cur.is_some(), egui::Button::new("View current patch")).on_hover_text("Opens the detail view of the patch being matched as soon as it is finished.").clicked() {
                        if let Some(id) = cur {
                            if self.by_id.contains_key(&id) {
                                self.open_detail(id);
                            } else {
                                self.want_view = Some(id);
                            }
                        }
                    }
                });
                ui.add_enabled_ui(self.s.live, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Show every");
                        ui.add(egui::DragValue::new(&mut self.s.sample_every).range(ps::SAMPLE_EVERY_RANGE.0..=ps::SAMPLE_EVERY_RANGE.1));
                        ui.label("th tested offset ·");
                        ui.label("Wait per frame");
                        ui.add(egui::Slider::new(&mut self.s.delay_ms, ps::FRAME_DELAY_RANGE_MS.0..=ps::FRAME_DELAY_RANGE_MS.1).suffix(" ms")).on_hover_text(
                            "Values above 0 slow the matching down noticeably, because the worker waits for the display. At 0 nothing waits – frames are simply skipped.",
                        );
                    });
                });
                if !self.s.live {
                    ui.label(RichText::new("Live preview off – fastest mode, only progress and statistics.").small().weak());
                } else if let Some(f) = self.live_shown.take() {
                    self.live_canvas(ui, &f, pc);
                    self.live_shown = Some(f);
                } else {
                    ui.label(RichText::new("The overlay appears here while patches are being matched.").small().weak());
                }
            });
            ui.separator();
            // --- progress and statistics ---
            ui.vertical(|ui| {
                let title = if info.label.is_empty() { "No matching yet".to_string() } else { info.label.clone() };
                ui.label(RichText::new(title).strong());
                let frac = if info.total > 0 { info.done as f32 / info.total as f32 } else { 0.0 };
                ui.label(format!("Patch {} of {}", info.done, info.total));
                ui.add(egui::ProgressBar::new(frac).desired_width(280.0).show_percentage());
                ui.label(format!("valid {} · rejected {} · disabled {}", info.valid, info.rejected, info.done.saturating_sub(info.valid + info.rejected)));
                match info.phase {
                    Phase::Running => {
                        if info.done > 0 && info.total > info.done {
                            let per = info.elapsed_ms as f64 / info.done as f64;
                            let left = per * (info.total - info.done) as f64 / 1000.0;
                            ui.label(format!("about {left:.0} s left ({:.1} ms per patch)", per));
                        }
                    }
                    Phase::Finished => {
                        ui.label(format!("finished in {:.1} s", info.elapsed_ms as f64 / 1000.0));
                    }
                    Phase::Cancelled => {
                        ui.label(RichText::new("cancelled – the finished patches stay in the list").color(C_YELLOW));
                    }
                    _ => {}
                }
                ui.label(format!("shift of valid patches: mean {} px, median {} px", fmt_opt(self.mean_mag, 3), fmt_opt(self.median_mag, 3)));
                if let Some((_, levels, a, b)) = self.current {
                    // the frame knows how many levels the search really used (small patches use fewer)
                    let lvl = self.live_shown.as_ref().filter(|f| Some(f.id) == self.current.map(|c| c.0)).map(|f| (f.level, f.levels));
                    let pair = self.pair_name(pc, a, b);
                    match lvl {
                        Some((l, n)) => ui.label(format!("current: {pair}, level {} of {} (factor {})", l + 1, n + 1, 1 << l)),
                        None => ui.label(format!("current: {pair}, pyramid with up to {} levels", levels + 1)),
                    };
                }
                ui.add_space(4.0);
                let mem = info.bytes;
                let t = format!("Patch data in memory: {}", fmt_bytes(mem));
                if mem > ps::MEMORY_WARN_BYTES {
                    ui.label(RichText::new(format!("⚠ {t} – a lot; lower the number of areas per pair if memory gets tight.")).color(C_YELLOW));
                } else {
                    ui.label(RichText::new(t).small().weak());
                }
                if let Some(st) = pc.stats {
                    if st.list_total > 0 {
                        ui.label(
                            RichText::new(format!(
                                "Optimisation statistics of this round: {} patches measured, {} valid, {} control areas used",
                                st.list_total, st.list_valid, st.list_areas
                            ))
                            .small()
                            .weak(),
                        );
                    }
                }
            });
        });
    }

    fn live_canvas(&mut self, ui: &mut egui::Ui, f: &LiveFrame, pc: &PatchCtx) {
        let ctx = ui.ctx().clone();
        let im = &f.images;
        let key = Arc::as_ptr(im) as usize;
        if self.live_tex.b_key != key {
            self.live_tex.b_key = key;
            self.live_tex.b = Some(ctx.load_texture("pw-live-b", grey_plane(&im.b, &im.mb, im.size), TextureOptions::NEAREST));
            self.live_tex.a = Some(ctx.load_texture("pw-live-a", grey_plane(&im.a, &im.ma, im.size), TextureOptions::NEAREST));
        }
        let n = (2 * im.half + 1) as usize;
        let mx = f.diff.iter().filter(|v| v.is_finite()).cloned().fold(0.0f32, f32::max).max(1e-6);
        let mut px = vec![0u8; n * n * 4];
        for i in 0..n * n {
            let v = f.diff[i];
            let c = if v.is_finite() { pv::colormap(Colormap::Viridis, v / mx) } else { [0.45, 0.45, 0.5] };
            px[i * 4..i * 4 + 4].copy_from_slice(&[(c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8, 255]);
        }
        self.live_tex.diff = Some(ctx.load_texture("pw-live-diff", colour_image(n, n, &px), TextureOptions::NEAREST));
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(LIVE_CANVAS), Sense::hover());
            let p = ui.painter_at(rect);
            let s = im.size as f32;
            let k = LIVE_CANVAS / s;
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            if let Some(t) = &self.live_tex.b {
                p.image(t.id(), rect, uv, Color32::WHITE);
            }
            let c = (im.size / 2) as f32;
            let h = im.half as f32;
            // search area of this level
            let r = im.radius as f32;
            let sr = Rect::from_min_max(rect.min + Vec2::splat((c - h - r) * k), rect.min + Vec2::splat((c + h + r + 1.0) * k));
            dashed_rect(&p, sr, Color32::from_rgb(230, 230, 240));
            // patch A (template) moved to the tested offset, half transparent
            let tr = Rect::from_min_size(rect.min + Vec2::new((c - h + f.offset.0) * k, (c - h + f.offset.1) * k), Vec2::splat((2.0 * h + 1.0) * k));
            let auv = Rect::from_min_max(Pos2::new((c - h) / s, (c - h) / s), Pos2::new((c + h + 1.0) / s, (c + h + 1.0) / s));
            if let Some(t) = &self.live_tex.a {
                p.image(t.id(), tr, auv, Color32::from_rgba_unmultiplied(255, 200, 120, 140));
            }
            p.rect_stroke(tr, 0.0, Stroke::new(1.5, if f.last { C_GREEN } else { C_YELLOW }), StrokeKind::Inside);
            ui.vertical(|ui| {
                let (r2, _) = ui.allocate_exact_size(Vec2::splat(LIVE_DIFF), Sense::hover());
                if let Some(t) = &self.live_tex.diff {
                    ui.painter().image(t.id(), r2, uv, Color32::WHITE);
                }
                ui.label(RichText::new("difference at this offset").small().weak());
            });
            ui.vertical(|ui| {
                ui.label(format!("Patch {} – {}", f.id, self.pair_name(pc, f.image_a, f.image_b)));
                ui.label(format!("Level {} of {} (factor {})", f.level + 1, f.levels + 1, 1 << f.level));
                ui.label(format!("search area ±{} (= ±{} px)", im.radius, im.radius << f.level));
                ui.label(format!("offset ({:.2}, {:.2})", f.offset.0, f.offset.1));
                ui.label(RichText::new(format!("score {:.3}", f.score)).strong());
                if f.last {
                    ui.label(RichText::new("optimum found").color(C_GREEN));
                }
            });
        });
    }

    fn filter_ui(&mut self, ui: &mut egui::Ui, pc: &PatchCtx) {
        let mut pairs: Vec<(usize, usize)> = self.data.iter().map(|d| (d.image_a, d.image_b)).collect();
        pairs.sort();
        pairs.dedup();
        let mut reasons: Vec<&'static str> = self.state.iter().filter(|s| s.status == Status::Rejected).map(|s| s.reason).collect();
        reasons.sort();
        reasons.dedup();
        ui.horizontal_wrapped(|ui| {
            ui.label("Photo pair");
            let sel = match self.s.filter.pair {
                Some((a, b)) => self.pair_name(pc, a, b),
                None => "All".into(),
            };
            egui::ComboBox::from_id_salt("pw_pair").selected_text(sel).width(200.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.s.filter.pair, None, "All");
                for p in &pairs {
                    let t = self.pair_name(pc, p.0, p.1);
                    ui.selectable_value(&mut self.s.filter.pair, Some(*p), t);
                }
            });
            ui.separator();
            for (k, name) in CATEGORY_NAMES.iter().enumerate() {
                ui.checkbox(&mut self.s.filter.show[k], *name);
            }
            ui.label("reason");
            let rs = self.s.filter.reason.clone().unwrap_or_else(|| "any".into());
            egui::ComboBox::from_id_salt("pw_reason").selected_text(rs).width(170.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.s.filter.reason, None, "any");
                for r in &reasons {
                    ui.selectable_value(&mut self.s.filter.reason, Some(r.to_string()), *r);
                }
            });
            ui.separator();
            let f = &mut self.s.filter;
            for (label, lo, hi) in [("Score", &mut f.score_min, &mut f.score_max), ("Confidence", &mut f.conf_min, &mut f.conf_max), ("Shift px", &mut f.mag_min, &mut f.mag_max)] {
                ui.label(label);
                ui.add(egui::TextEdit::singleline(lo).hint_text("min").desired_width(40.0));
                ui.add(egui::TextEdit::singleline(hi).hint_text("max").desired_width(40.0));
            }
            if ui.button("Reset filters").clicked() {
                self.s.filter = Filter::default();
            }
            self.ensure_rows();
            ui.label(RichText::new(format!("{} of {} patches visible", self.rows.len(), self.data.len())).strong());
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Thumbnails");
            ui.add(egui::Slider::new(&mut self.s.thumb, THUMB_RANGE.0..=THUMB_RANGE.1).suffix(" px"));
            ui.separator();
            ui.label("Heatmap");
            let before = (self.s.heat, self.s.space, self.s.auto_scale);
            egui::ComboBox::from_id_salt("pw_heat").selected_text(self.s.heat.name()).show_ui(ui, |ui| {
                for m in HeatMode::all() {
                    ui.selectable_value(&mut self.s.heat, m, m.name());
                }
            });
            egui::ComboBox::from_id_salt("pw_space")
                .selected_text(match self.s.space {
                    DiffSpace::Feature => "on the matching features",
                    DiffSpace::RawColor => "on the photo (colour)",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.s.space, DiffSpace::Feature, "on the matching features");
                    ui.selectable_value(&mut self.s.space, DiffSpace::RawColor, "on the photo (colour)");
                });
            ui.checkbox(&mut self.s.auto_scale, "automatic scale (99th percentile)").on_hover_text("Off: fixed scale like the original's difference view - the difference is amplified 4 times (a difference of 64 of 255 is shown at full strength).");
            if before != (self.s.heat, self.s.space, self.s.auto_scale) {
                // recomputed in the background, visible rows first (only they ask)
                self.thumb_gen += 1;
                self.thumb_pending.clear();
                self.dtex.key.clear();
            }
            ui.separator();
            legend(ui);
        });
    }

    fn actions_ui(&mut self, ui: &mut egui::Ui, pc: &PatchCtx) {
        ui.horizontal_wrapped(|ui| {
            let changes = STORE.pending_changes();
            let n: usize = changes.values().sum();
            let detail = if n == 0 {
                "no changes".to_string()
            } else {
                let mut v: Vec<String> = changes.iter().map(|(k, v)| format!("{v} {k}")).collect();
                v.sort();
                v.join(", ")
            };
            ui.label(format!("Changes not applied yet: {detail}"));
            let can = !pc.busy && !self.data.is_empty() && self.info.phase != Phase::Running;
            if ui
                .add_enabled(can, egui::Button::new(RichText::new(format!("⟳ Restart optimisation ({n} changed)")).strong()))
                .on_hover_text("Optimises again with all active patches and their current shifts (disabled ones are left out), then measures a new round for this list.")
                .clicked()
            {
                self.action = Some(PatchAction::Restart);
            }
            ui.separator();
            if ui.add_enabled(!self.data.is_empty(), egui::Button::new("Disable all rejected")).clicked() {
                let ids: Vec<u32> = self.data.iter().zip(&self.state).filter(|(_, s)| s.status == Status::Rejected).map(|(d, _)| d.id).collect();
                self.set_active(&ids, false);
            }
            if ui.add_enabled(!self.rows.is_empty(), egui::Button::new("Disable all filtered")).clicked() {
                let ids: Vec<u32> = self.rows.iter().map(|&i| self.data[i].id).collect();
                self.set_active(&ids, false);
            }
            ui.label("Confidence of patches shifted by hand");
            ui.add(egui::DragValue::new(&mut self.s.manual_conf).range(0.05..=1.0).speed(0.01));
            ui.separator();
            self.export_ui(ui, pc);
            if !self.msg.is_empty() {
                ui.label(RichText::new(&self.msg).small());
            }
        });
    }

    fn export_ui(&mut self, ui: &mut egui::Ui, pc: &PatchCtx) {
        if let Some(job) = &self.export {
            let (done, total, _) = job.progress.lock().map(|g| g.clone()).unwrap_or((0, 0, String::new()));
            ui.add(egui::ProgressBar::new(if total > 0 { done as f32 / total as f32 } else { 0.0 }).desired_width(160.0).text(format!("export {done}/{total}")));
            if ui.button("Cancel").clicked() {
                job.cancel.store(true, Ordering::Release);
            }
            return;
        }
        egui::ComboBox::from_id_salt("pw_export_which")
            .selected_text(match self.s.export_which {
                ExportWhich::All => "all patches",
                ExportWhich::Filtered => "only filtered (visible)",
                ExportWhich::Selected => "only selected",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.s.export_which, ExportWhich::All, "all patches");
                ui.selectable_value(&mut self.s.export_which, ExportWhich::Filtered, "only filtered (visible)");
                ui.selectable_value(&mut self.s.export_which, ExportWhich::Selected, "only selected");
            });
        ui.label("zoom");
        ui.add(egui::DragValue::new(&mut self.s.export_zoom).range(EXPORT_ZOOM_RANGE.0..=EXPORT_ZOOM_RANGE.1).suffix("×"));
        ui.checkbox(&mut self.s.export_overlay, "overlay.png");
        if ui.add_enabled(!self.data.is_empty(), egui::Button::new("🖼 Export images…")).clicked() {
            let idx: Vec<usize> = match self.s.export_which {
                ExportWhich::All => (0..self.data.len()).collect(),
                ExportWhich::Filtered => self.rows.clone(),
                ExportWhich::Selected => (0..self.data.len()).filter(|&i| self.sel.contains(&self.data[i].id)).collect(),
            };
            if idx.is_empty() {
                self.msg = "Nothing to export (no patch selected or visible).".into();
            } else if let Some(dir) = rfd::FileDialog::new().set_title("Folder for the patch images").pick_folder() {
                self.start_export(dir, idx, pc);
            }
        }
    }

    fn start_export(&mut self, dir: PathBuf, idx: Vec<usize>, pc: &PatchCtx) {
        let items: Vec<(Arc<PatchData>, PatchState)> = idx.iter().map(|&i| (self.data[i].clone(), self.state[i].clone())).collect();
        let names = pc.names.clone();
        let rgb = pc.rgb.clone();
        let gains = pc.gains.clone();
        let (mode, space, auto, zoom, overlay) = (self.s.heat, self.s.space, self.s.auto_scale, self.s.export_zoom, self.s.export_overlay);
        let progress = Arc::new(Mutex::new((0usize, items.len(), String::new())));
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (p2, c2, d2) = (progress.clone(), cancel.clone(), done.clone());
        std::thread::spawn(move || {
            let mut errors = 0;
            let mut written = 0;
            for (k, (d, st)) in items.iter().enumerate() {
                if c2.load(Ordering::Acquire) {
                    break;
                }
                let n = |i: usize| names.get(i).map(|s| sanitize(s)).unwrap_or_else(|| format!("photo{}", i + 1));
                let folder = dir.join(format!("patch_{}_{}_{}", d.id, n(d.image_a), n(d.image_b)));
                let pair = if space == DiffSpace::RawColor { rgb.get(d.image_a).zip(rgb.get(d.image_b)).map(|(a, b)| (a.as_ref(), b.as_ref(), gain_of(&gains, d.image_a), gain_of(&gains, d.image_b))) } else { None };
                match export_one(&folder, d, st, mode, space, auto, zoom, overlay, pair) {
                    Ok(()) => written += 1,
                    Err(e) => {
                        errors += 1;
                        eprintln!("patch export {}: {e}", d.id);
                    }
                }
                if let Ok(mut g) = p2.lock() {
                    g.0 = k + 1;
                }
            }
            if let Ok(mut g) = p2.lock() {
                g.2 = if c2.load(Ordering::Acquire) {
                    format!("Export cancelled after {written} patches.")
                } else if errors > 0 {
                    format!("Exported {written} patches, {errors} could not be written (see the log).")
                } else {
                    format!("Exported {written} patches to {}.", dir.display())
                };
            }
            d2.store(true, Ordering::Release);
        });
        self.export = Some(ExportJob { progress, cancel, done });
    }

    fn table_ui(&mut self, ui: &mut egui::Ui, pc: &PatchCtx) {
        self.ensure_rows();
        if self.data.is_empty() {
            let t = match self.info.phase {
                Phase::Freed => "Released after the export – start a new matching (step 3, “Align photos” with fine alignment on).",
                Phase::Running => "Matching is running – the first patches appear here in a moment.",
                Phase::Finished | Phase::Cancelled => "This round measured no patches.",
                Phase::Empty => "No patch comparisons yet. Align the photos with “Fine alignment (patch matching)” switched on (step 3).",
            };
            ui.centered_and_justified(|ui| ui.label(RichText::new(t).size(15.0).weak()));
            return;
        }
        let dark = ui.visuals().dark_mode;
        let bg = ui.visuals().panel_fill;
        let text_col = ui.visuals().strong_text_color();
        let weak_col = ui.visuals().weak_text_color();
        let row_h = self.s.thumb + 2.0 * ROW_PAD;
        let widths = self.s.col_w.clone();
        let total_w: f32 = widths.iter().sum();
        egui::ScrollArea::horizontal().id_salt("pw_hscroll").auto_shrink([false, false]).show(ui, |ui| {
            ui.set_min_width(total_w);
            // header (click = sort up, down, off; drag the edge = width)
            let (hr, _) = ui.allocate_exact_size(Vec2::new(total_w, HEADER_H), Sense::hover());
            let mut x = hr.min.x;
            for (k, c) in COLS.iter().enumerate() {
                let r = Rect::from_min_size(Pos2::new(x, hr.min.y), Vec2::new(widths[k], HEADER_H));
                let resp = ui.interact(r.shrink2(Vec2::new(4.0, 0.0)), ui.scope_id().with(("pw_head", k)), Sense::click());
                if resp.clicked() && c.sortable() {
                    self.s.sort = match self.s.sort {
                        Some((cc, true)) if cc == *c => Some((*c, false)),
                        Some((cc, false)) if cc == *c => None,
                        _ => Some((*c, true)),
                    };
                }
                let arrow = match self.s.sort {
                    Some((cc, true)) if cc == *c => " ▲",
                    Some((cc, false)) if cc == *c => " ▼",
                    _ => "",
                };
                let p = ui.painter_at(r);
                p.rect_filled(r, 0.0, ui.visuals().faint_bg_color);
                let (al, pos) = if c.numeric() { (Align2::RIGHT_CENTER, Pos2::new(r.max.x - 6.0, r.center().y)) } else { (Align2::LEFT_CENTER, Pos2::new(r.min.x + 6.0, r.center().y)) };
                p.text(pos, al, format!("{}{arrow}", c.title()), FontId::proportional(13.0), text_col);
                let edge = Rect::from_min_max(Pos2::new(r.max.x - 3.0, r.min.y), Pos2::new(r.max.x + 3.0, r.max.y));
                let er = ui.interact(edge, ui.scope_id().with(("pw_edge", k)), Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
                if er.dragged() {
                    self.s.col_w[k] = (self.s.col_w[k] + er.drag_delta().x).max(MIN_COL_W);
                }
                if er.drag_stopped() {
                    // the original writes a column width as soon as the drag ends
                    self.save_now = true;
                }
                ui.painter().line_segment([Pos2::new(r.max.x, r.min.y + 4.0), Pos2::new(r.max.x, r.max.y - 4.0)], Stroke::new(1.0, weak_col));
                x += widths[k];
            }
            // rows: only the visible ones are drawn (and only they ask for thumbnails)
            let rows = self.rows.clone();
            let mut clicked: Option<(usize, egui::Modifiers, bool)> = None;
            let mut toggled: Vec<(u32, bool)> = Vec::new();
            let mut ctx_action: Option<(u8, usize)> = None;
            egui::ScrollArea::vertical().id_salt("pw_rows").auto_shrink([false, false]).show_rows(ui, row_h, rows.len(), |ui, range| {
                for ri in range {
                    let i = rows[ri];
                    let (d, st) = (self.data[i].clone(), self.state[i].clone());
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(total_w, row_h), Sense::click());
                    let base = match category(&st) {
                        2 => C_GREY,
                        1 => C_RED,
                        _ => conf_colour(st.confidence),
                    };
                    let p = ui.painter_at(rect);
                    let mut fill = row_bg(base, dark, bg);
                    if ri % 2 == 1 {
                        // alternate row colours, as in the original's list
                        fill = lerp_col(fill, if dark { Color32::BLACK } else { Color32::WHITE }, ALT_ROW_SHADE);
                    }
                    p.rect_filled(rect, 0.0, fill);
                    if self.sel.contains(&d.id) {
                        p.rect_stroke(rect.shrink(1.0), 2.0, Stroke::new(2.0, Color32::from_rgb(70, 160, 255)), StrokeKind::Inside);
                    }
                    if self.detail == Some(d.id) {
                        p.rect_stroke(rect.shrink(3.0), 2.0, Stroke::new(1.0, text_col), StrokeKind::Inside);
                    }
                    let disabled = st.status == Status::DisabledManual;
                    let mut x = rect.min.x;
                    for (k, c) in COLS.iter().enumerate() {
                        let cell = Rect::from_min_size(Pos2::new(x, rect.min.y), Vec2::new(widths[k], row_h));
                        let cp = ui.painter_at(cell);
                        match c {
                            Col::Thumb => {
                                let tr = Rect::from_min_size(cell.min + Vec2::splat(ROW_PAD), Vec2::splat(self.s.thumb.min(cell.width() - 2.0 * ROW_PAD)));
                                self.draw_thumb(&cp, tr, &d, &st, pc);
                            }
                            Col::Active => {
                                let mut a = st.active();
                                // a child Ui: the checkbox must not move the row layout
                                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_center_size(cell.center(), Vec2::splat(20.0))));
                                let r = child.add(egui::Checkbox::without_text(&mut a));
                                if r.changed() {
                                    toggled.push((d.id, a));
                                }
                            }
                            _ => {
                                let t = self.cell_text(i, *c, pc);
                                let col = if disabled { weak_col } else { text_col };
                                let font = FontId::proportional(13.0);
                                let (al, pos) = if c.numeric() { (Align2::RIGHT_CENTER, Pos2::new(cell.max.x - 6.0, cell.center().y)) } else { (Align2::LEFT_CENTER, Pos2::new(cell.min.x + 6.0, cell.center().y)) };
                                let r = cp.text(pos, al, t, font, col);
                                if disabled {
                                    // struck through
                                    cp.line_segment([Pos2::new(r.min.x, r.center().y), Pos2::new(r.max.x, r.center().y)], Stroke::new(1.0, weak_col));
                                }
                            }
                        }
                        x += widths[k];
                    }
                    if resp.clicked() {
                        clicked = Some((ri, ui.input(|i| i.modifiers), false));
                    }
                    if resp.double_clicked() {
                        clicked = Some((ri, ui.input(|i| i.modifiers), true));
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Disable selected").clicked() {
                            ctx_action = Some((0, ri));
                            ui.close();
                        }
                        if ui.button("Enable selected").clicked() {
                            ctx_action = Some((1, ri));
                            ui.close();
                        }
                        if ui.button("Reset selected to the automatic result").clicked() {
                            ctx_action = Some((2, ri));
                            ui.close();
                        }
                        if ui.button("Open detail view").clicked() {
                            ctx_action = Some((3, ri));
                            ui.close();
                        }
                    });
                }
            });
            for (id, a) in toggled {
                self.set_active(&[id], a);
            }
            if let Some((ri, m, _dbl)) = clicked {
                let id = self.data[rows[ri]].id;
                if m.shift {
                    let a = self.anchor.unwrap_or(ri);
                    let (lo, hi) = (a.min(ri), a.max(ri));
                    if !(m.ctrl || m.command) {
                        self.sel.clear();
                    }
                    for r in lo..=hi {
                        self.sel.insert(self.data[rows[r]].id);
                    }
                } else if m.ctrl || m.command {
                    if !self.sel.remove(&id) {
                        self.sel.insert(id);
                    }
                    self.anchor = Some(ri);
                } else {
                    self.sel.clear();
                    self.sel.insert(id);
                    self.anchor = Some(ri);
                    self.open_detail(id);
                }
            }
            if let Some((act, ri)) = ctx_action {
                let id = self.data[rows[ri]].id;
                if !self.sel.contains(&id) {
                    self.sel.clear();
                    self.sel.insert(id);
                }
                let ids: Vec<u32> = self.sel.iter().copied().collect();
                match act {
                    0 => self.set_active(&ids, false),
                    1 => self.set_active(&ids, true),
                    2 => self.reset(&ids),
                    _ => self.open_detail(id),
                }
            }
        });
    }

    fn cell_text(&self, i: usize, c: Col, pc: &PatchCtx) -> String {
        let (d, st) = (&self.data[i], &self.state[i]);
        match c {
            Col::Id => d.id.to_string(),
            Col::Pair => self.pair_name(pc, d.image_a, d.image_b),
            Col::Pos => self.pos[i].map(|p| format!("{:.0}, {:.0}", p.0, p.1)).unwrap_or_else(|| "–".into()),
            Col::Dx => fmt_opt(st.dx, 2),
            Col::Dy => fmt_opt(st.dy, 2),
            Col::Mag => fmt_opt(st.magnitude(), 2),
            Col::Score => fmt_opt(st.score, 3),
            Col::Uniq => fmt_opt(d.uniqueness, 3),
            Col::Tex => format!("{:.2e}", d.texture),
            Col::Conf => fmt_opt(st.confidence, 3),
            Col::Status => self.status_text(st),
            _ => String::new(),
        }
    }

    fn thumb_sig(&self, st: &PatchState) -> String {
        format!("{}|{:.3}|{:.3}|{}|{}", self.thumb_gen, st.dx, st.dy, st.manual, category(st))
    }

    fn draw_thumb(&mut self, p: &egui::Painter, r: Rect, d: &Arc<PatchData>, st: &PatchState, pc: &PatchCtx) {
        let sig = self.thumb_sig(st);
        let fresh = self.thumbs.get(&d.id).map(|t| t.0 == sig).unwrap_or(false);
        if let Some((_, t, rejected)) = self.thumbs.get(&d.id) {
            p.image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            if *rejected {
                // marker: rejected, so the difference is shown without correction
                p.rect_stroke(r, 0.0, Stroke::new(2.0, C_RED), StrokeKind::Inside);
                p.text(r.right_top() + Vec2::new(-4.0, 2.0), Align2::RIGHT_TOP, "✕ uncorrected", FontId::proportional(10.0), C_RED);
            }
        } else {
            p.rect_filled(r, 2.0, Color32::from_gray(70));
            p.text(r.center(), Align2::CENTER_CENTER, "…", FontId::proportional(14.0), Color32::from_gray(200));
        }
        if !fresh && !self.thumb_pending.contains(&d.id) && self.thumb_pending.len() < THUMB_JOBS_MAX {
            self.thumb_pending.insert(d.id);
            let corrected = st.status == Status::Valid || st.manual;
            let shift = if corrected { Some((st.dx, st.dy)) } else { None };
            let (mode, space, auto, generation) = (self.s.heat, self.s.space, self.s.auto_scale, self.thumb_gen);
            let d2 = d.clone();
            let rgb = if space == DiffSpace::RawColor { pc.rgb.get(d.image_a).cloned().zip(pc.rgb.get(d.image_b).cloned()) } else { None };
            let gg = (gain_of(&pc.gains, d.image_a), gain_of(&pc.gains, d.image_b));
            let tx = self.thumb_tx.clone();
            let id = d.id;
            rayon::spawn(move || {
                let img = pv::thumbnail(&d2, shift, mode, space, auto, rgb.as_ref().map(|(a, b)| (a.as_ref(), b.as_ref(), gg.0, gg.1)));
                let img = match img {
                    Some(i) => Some(i),
                    // photo B was not sampled: show patch A in grey instead of a difference
                    None => {
                        let n = d2.a_size;
                        let px: Vec<[f32; 3]> = d2.a.iter().map(|&b| [if b == 0 { 0.0 } else { (b - 1) as f32 / 254.0 }; 3]).collect();
                        let valid: Vec<bool> = d2.a.iter().map(|&b| b != 0).collect();
                        Some(pv::patch_rgba(&px, &valid, n, 1, false))
                    }
                };
                let _ = tx.send((generation, id, sig, img, !corrected));
            });
        }
    }

    fn detail_ui(&mut self, ui: &mut egui::Ui, pc: &PatchCtx) {
        let Some(id) = self.detail else { return };
        let Some(&i) = self.by_id.get(&id) else {
            ui.label("This patch is no longer in the list (a new round started or the data were released).");
            if ui.button("Close").clicked() {
                self.detail = None;
            }
            return;
        };
        let d = self.data[i].clone();
        let st = self.state[i].clone();
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Patch {} – {}", d.id, self.pair_name(pc, d.image_a, d.image_b))).strong().size(15.0));
            if ui.button("◀ Previous").clicked() {
                self.step_detail(-1);
            }
            if ui.button("Next ▶").clicked() {
                self.step_detail(1);
            }
            if ui.button("✕").on_hover_text("Close the detail view").clicked() {
                self.detail = None;
            }
        });
        ui.horizontal_wrapped(|ui| {
            let mut a = st.active();
            if ui.checkbox(&mut a, "active (used in the optimisation)").changed() {
                self.set_active(&[id], a);
            }
            if ui.add_enabled(st.manual, egui::Button::new("Reset to the automatic result")).clicked() {
                self.reset(&[id]);
                self.drag_shift = None;
            }
            ui.label(RichText::new(self.status_text(&st)).small());
        });
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.s.view, ViewMode::Side, "Side by side");
            ui.selectable_value(&mut self.s.view, ViewMode::Overlay, "Overlay (shift by hand)");
            ui.selectable_value(&mut self.s.view, ViewMode::Wipe, "Wipe");
            ui.label("Zoom");
            let mut z = self.s.zoom as f32 * 100.0;
            if ui.add(egui::Slider::new(&mut z, (ZOOM_RANGE.0 * 100) as f32..=(ZOOM_RANGE.1 * 100) as f32).step_by(100.0).suffix(" %")).changed() {
                self.s.zoom = ((z / 100.0).round() as usize).clamp(ZOOM_RANGE.0, ZOOM_RANGE.1);
            }
            match self.s.view {
                ViewMode::Overlay => {
                    ui.label("Transparency");
                    ui.add(egui::Slider::new(&mut self.s.alpha, 0.0..=1.0).custom_formatter(|v, _| format!("{:.0} %", v * 100.0)));
                }
                ViewMode::Wipe => {
                    ui.label("Dividing line");
                    ui.add(egui::Slider::new(&mut self.s.wipe, 0.0..=1.0).show_value(false));
                }
                ViewMode::Side => {}
            }
        });
        if self.s.view == ViewMode::Overlay {
            ui.label(RichText::new("Drag photo B with the mouse, or use the arrow keys (1 px, Shift 0.1 px, Ctrl 10 px). Page up / down: previous / next patch.").small().weak());
        } else {
            ui.label(RichText::new("Arrow keys or Page up / down: previous / next patch.").small().weak());
        }
        let corrected = st.status == Status::Valid || st.manual || self.drag_shift.is_some();
        let shift = self.drag_shift.unwrap_or((st.dx, st.dy));
        let shift_show = if corrected { shift } else { (st.dx, st.dy) };
        let rgb = if self.s.space == DiffSpace::RawColor { pc.rgb.get(d.image_a).cloned().zip(pc.rgb.get(d.image_b).cloned()) } else { None };
        let gg = (gain_of(&pc.gains, d.image_a), gain_of(&pc.gains, d.image_b));
        let space = if self.s.space == DiffSpace::RawColor && rgb.is_none() { DiffSpace::Feature } else { self.s.space };
        let zoom = self.s.zoom;
        // pictures (cached by everything they depend on)
        let key = format!("{}|{:.3}|{:.3}|{:?}|{:?}|{}|{}", d.id, shift_show.0, shift_show.1, self.s.heat, space, self.s.auto_scale, zoom);
        if key != self.dtex.key {
            self.dtex.key = key;
            self.build_detail_textures(&ctx, &d, shift_show, space, rgb.as_ref().map(|(a, b)| (a.as_ref(), b.as_ref(), gg.0, gg.1)));
            self.dtex.live_score = if self.drag_shift.is_some() {
                d.region_b().and_then(|rb| patch_matcher::score_at_shift(&d.region_a(), &rb, &d.cfg, shift.0, shift.1))
            } else {
                None
            };
        }
        if !self.dtex.note.is_empty() {
            ui.label(RichText::new(&self.dtex.note).color(C_YELLOW));
        }
        // --- A and B ---
        ui.add_space(4.0);
        ui.label(RichText::new(if space == DiffSpace::RawColor { "Patch A and photo B at the found position (colour)" } else { "Patch A and photo B at the found position" }).strong());
        let n = self.dtex.n.max(1);
        let img = n as f32 * zoom as f32;
        let avail = ui.available_width();
        match self.s.view {
            ViewMode::Side => {
                let bw = ((avail - 16.0) / 2.0).min(img).max(60.0);
                ui.horizontal(|ui| {
                    for (label, t) in [("A", self.dtex.a.clone()), ("B", self.dtex.b.clone())] {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(label).small());
                            let (r, resp) = ui.allocate_exact_size(Vec2::splat(bw), Sense::drag());
                            if resp.dragged() {
                                self.pan += resp.drag_delta();
                            }
                            self.clamp_pan(img, bw);
                            let p = ui.painter_at(r);
                            p.rect_filled(r, 0.0, Color32::from_gray(40));
                            if let Some(t) = t {
                                p.image(t.id(), Rect::from_min_size(r.min + self.pan, Vec2::splat(img)), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                            }
                            crosshair(&p, Rect::from_min_size(r.min + self.pan, Vec2::splat(img)));
                        });
                    }
                });
                ui.label(RichText::new("Both pictures scroll together (drag).").small().weak());
            }
            ViewMode::Overlay | ViewMode::Wipe => {
                let bw = (avail - 8.0).min(img.max(240.0)).max(120.0);
                let (r, resp) = ui.allocate_exact_size(Vec2::splat(bw), Sense::click_and_drag());
                let off = Vec2::splat((bw - img) * 0.5);
                let ir = Rect::from_min_size(r.min + off, Vec2::splat(img));
                let p = ui.painter_at(r);
                p.rect_filled(r, 0.0, Color32::from_gray(40));
                let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
                if let Some(t) = &self.dtex.a {
                    p.image(t.id(), ir, uv, Color32::WHITE);
                }
                if self.s.view == ViewMode::Overlay {
                    if let Some(t) = &self.dtex.b {
                        let a = (self.s.alpha * 255.0) as u8;
                        p.image(t.id(), ir, uv, Color32::from_rgba_unmultiplied(255, 255, 255, a));
                    }
                    if resp.drag_started() {
                        self.drag_start = shift;
                        self.drag_shift = Some(shift);
                    }
                    if resp.dragged() {
                        let total = resp.total_drag_delta().unwrap_or_default() / zoom as f32;
                        // dragging B to the right: the matching spot in B lies further left
                        self.drag_shift = Some((self.drag_start.0 - total.x, self.drag_start.1 - total.y));
                    }
                    if resp.drag_stopped() {
                        if let Some(s) = self.drag_shift.take() {
                            self.commit_shift(id, s);
                        }
                    }
                } else {
                    let xw = r.min.x + bw * self.s.wipe;
                    if resp.dragged() {
                        if let Some(pp) = resp.interact_pointer_pos() {
                            self.s.wipe = ((pp.x - r.min.x) / bw).clamp(0.0, 1.0);
                        }
                    }
                    if let Some(t) = &self.dtex.b {
                        let clip = Rect::from_min_max(Pos2::new(xw, r.min.y), r.max);
                        ui.painter_at(clip).image(t.id(), ir, uv, Color32::WHITE);
                    }
                    p.line_segment([Pos2::new(xw, r.min.y), Pos2::new(xw, r.max.y)], Stroke::new(2.0, C_YELLOW));
                    p.text(Pos2::new(xw - 4.0, r.min.y + 4.0), Align2::RIGHT_TOP, "A", FontId::proportional(12.0), C_YELLOW);
                    p.text(Pos2::new(xw + 4.0, r.min.y + 4.0), Align2::LEFT_TOP, "B", FontId::proportional(12.0), C_YELLOW);
                }
                crosshair(&p, ir);
            }
        }
        ui.label(format!(
            "shift dx {:.2}, dy {:.2} px{}{}",
            shift.0,
            shift.1,
            if self.drag_shift.is_some() { " (moving…)" } else { "" },
            self.dtex.live_score.map(|s| format!(" · score here {s:.3}")).unwrap_or_default()
        ));
        // --- differences before / after ---
        ui.add_space(6.0);
        ui.label(RichText::new("Difference before and after the correction (same scale)").strong());
        let hw = ((avail - 16.0) / 2.0).min(n as f32 * zoom as f32).max(80.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("before (dx = 0, dy = 0)").small());
                let (r, _) = ui.allocate_exact_size(Vec2::splat(hw), Sense::hover());
                if let Some(t) = &self.dtex.before {
                    ui.painter().image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                }
            });
            ui.vertical(|ui| {
                ui.label(RichText::new(format!("after (dx = {:.2}, dy = {:.2})", shift_show.0, shift_show.1)).small());
                let (r, _) = ui.allocate_exact_size(Vec2::splat(hw), Sense::hover());
                if corrected {
                    if let Some(t) = &self.dtex.after {
                        ui.painter().image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    }
                } else {
                    ui.painter().rect_filled(r, 0.0, Color32::from_gray(45));
                    ui.painter().text(r.center(), Align2::CENTER_CENTER, "no valid result", FontId::proportional(14.0), C_RED);
                }
            });
        });
        colour_bar(ui, self.s.heat, self.dtex.scale, space, avail.min(2.0 * hw + 8.0));
        match (self.dtex.mean_before, self.dtex.mean_after) {
            (Some(b), Some(a)) if corrected => {
                let imp = if b > 0.0 { (b - a) / b * 100.0 } else { 0.0 };
                ui.label(format!("mean |difference|: before {:.4}, after {:.4} – improvement {imp:.1} %", b, a));
            }
            (Some(b), _) => {
                ui.label(format!("mean |difference| before {:.4}", b));
            }
            _ => {
                ui.label(RichText::new("no difference available (photo B was not sampled for this patch)").weak());
            }
        }
        // --- score map ---
        ui.add_space(6.0);
        self.score_map_ui(ui, &d, &st, shift, corrected);
        // --- values ---
        ui.add_space(6.0);
        ui.label(RichText::new("Values").strong());
        egui::Grid::new("pw_values").num_columns(2).striped(true).spacing([14.0, 3.0]).show(ui, |ui| {
            let mut row = |k: &str, v: String| {
                ui.label(k);
                ui.label(v);
                ui.end_row();
            };
            row("ID", d.id.to_string());
            row("Photos", self.pair_name(pc, d.image_a, d.image_b));
            row("Kind", format!("{:?}{}", d.kind, if d.hand_point { " (placed by hand)" } else { "" }));
            row("Centre in the panorama", self.pos[i].map(|p| format!("{:.1}, {:.1}", p.0, p.1)).unwrap_or_else(|| "–".into()));
            row("Patch size", format!("{} px", d.cfg.patch_size));
            row("Search radius", format!("{} px", d.cfg.search_radius));
            row("Measure", d.cfg.metric.name().into());
            row("Feature space", d.cfg.feature_space.name().into());
            row("Pyramid levels", format!("{}", d.levels + 1));
            row("dx / dy (current)", format!("{:.3} / {:.3} px", st.dx, st.dy));
            row("Shift", format!("{:.3} px", st.magnitude()));
            row("dx / dy (automatic)", format!("{:.3} / {:.3} px", d.auto.dx, d.auto.dy));
            row("Score", fmt_opt(st.score, 4));
            row("Score (automatic)", fmt_opt(d.auto.score, 4));
            row("Uniqueness", fmt_opt(d.uniqueness, 3));
            row("Texture", format!("{:.3e}", d.texture));
            row("Confidence", fmt_opt(st.confidence, 3));
            row("Mean grey difference before", if d.diff_before.is_finite() { format!("{:.2} / 255", d.diff_before) } else { "–".into() });
            row("Status", st.status.name().into());
            row("Reason", st.reason.into());
            row("Automatic result", format!("{} ({})", d.auto.reason, if d.auto.valid { "valid" } else { "rejected" }));
            row("Shifted by hand", if st.manual { "yes".into() } else { "no".into() });
            row("Used in the optimisation", match (&st.cp, st.active()) {
                (Some(c), true) => format!("yes, weight {:.3}", c.weight),
                _ => "no".into(),
            });
            row("Patch memory", fmt_bytes(d.bytes()));
        });
    }

    fn clamp_pan(&mut self, img: f32, bw: f32) {
        let lim = (img - bw).max(0.0);
        if img <= bw {
            self.pan = Vec2::splat((bw - img) * 0.5);
        } else {
            self.pan.x = self.pan.x.clamp(-lim, 0.0);
            self.pan.y = self.pan.y.clamp(-lim, 0.0);
        }
    }

    fn build_detail_textures(&mut self, ctx: &egui::Context, d: &PatchData, shift: (f32, f32), space: DiffSpace, rgb: Option<pv::Raw>) {
        let zoom = self.s.zoom;
        self.dtex.note.clear();
        let opt = TextureOptions::NEAREST;
        match pv::patch_pair(d, shift, space, rgb, true) {
            Some(pp) => {
                self.dtex.n = pp.n;
                let norm = false;
                let (w, px) = pv::patch_rgba(&pp.a, &pp.va, pp.n, zoom, norm);
                self.dtex.a = Some(ctx.load_texture("pw-d-a", colour_image(w, w, &px), opt));
                let (w, px) = pv::patch_rgba(&pp.b, &pp.vb, pp.n, zoom, norm);
                self.dtex.b = Some(ctx.load_texture("pw-d-b", colour_image(w, w, &px), opt));
            }
            None => {
                // B was not sampled: show A alone
                let n = d.a_size;
                let px: Vec<[f32; 3]> = d.a.iter().map(|&b| [if b == 0 { 0.0 } else { (b - 1) as f32 / 254.0 }; 3]).collect();
                let valid: Vec<bool> = d.a.iter().map(|&b| b != 0).collect();
                let (w, px) = pv::patch_rgba(&px, &valid, n, zoom, false);
                self.dtex.n = n;
                self.dtex.a = Some(ctx.load_texture("pw-d-a", colour_image(w, w, &px), opt));
                self.dtex.b = None;
                self.dtex.note = ps::REASON_B_SKIPPED.to_string();
                eprintln!("patch {}: {}", d.id, ps::REASON_B_SKIPPED);
            }
        }
        let before = pv::diff(d, (0.0, 0.0), space, rgb);
        let after = pv::diff(d, shift, space, rgb);
        match (&before, &after) {
            (Some(b), Some(a)) => {
                let scale = pv::shared_scale(&[b, a], self.s.auto_scale, self.s.heat == HeatMode::Signed);
                self.dtex.scale = scale;
                let (w, px) = pv::heat_rgba(b, self.s.heat, scale, zoom);
                self.dtex.before = Some(ctx.load_texture("pw-d-before", colour_image(w, w, &px), opt));
                let (w, px) = pv::heat_rgba(a, self.s.heat, scale, zoom);
                self.dtex.after = Some(ctx.load_texture("pw-d-after", colour_image(w, w, &px), opt));
                self.dtex.mean_before = b.mean_abs();
                self.dtex.mean_after = a.mean_abs();
            }
            _ => {
                self.dtex.before = None;
                self.dtex.after = None;
                self.dtex.mean_before = None;
                self.dtex.mean_after = None;
            }
        }
    }

    fn score_map_ui(&mut self, ui: &mut egui::Ui, d: &Arc<PatchData>, st: &PatchState, shift: (f32, f32), corrected: bool) {
        let ctx = ui.ctx().clone();
        let r = d.map_radius;
        let n = (2 * r + 1) as usize;
        let full = self.full_maps.get(&d.id).cloned();
        if full.is_none() && d.b.is_some() && !self.full_pending.contains(&d.id) {
            // the full field (every shift on the finest level) is computed in the background
            self.full_pending.insert(d.id);
            let (tx, d2, round) = (self.full_tx.clone(), d.clone(), self.round_no);
            rayon::spawn(move || {
                if let Some(rb) = d2.region_b() {
                    let m = patch_matcher::full_score_map(&d2.region_a(), &rb, &d2.cfg, d2.map_radius);
                    let _ = tx.send((round, d2.id, Arc::new(m)));
                }
            });
        }
        let key = format!("{}|{}", d.id, full.is_some());
        if key != self.dtex.map_key {
            self.dtex.map_key = key;
            let vals = full.as_ref().map(|m| m.as_ref().clone()).unwrap_or_else(|| d.map_f32());
            let zm = ((MAP_BOX / n as f32).floor() as usize).max(1);
            let (w, px, lo, hi) = pv::score_rgba(&vals, n, zm);
            self.dtex.map = Some(ctx.load_texture("pw-d-map", colour_image(w, w, &px), TextureOptions::NEAREST));
            self.dtex.map_n = n;
            self.dtex.map_lo = lo;
            self.dtex.map_hi = hi;
            self.dtex.map_vals = vals;
        }
        ui.label(RichText::new("Score map of the search window").strong());
        ui.label(
            RichText::new(if full.is_some() {
                "every shift on the finest level"
            } else if d.b.is_some() {
                "what the search tested (coarse levels as blocks) – the full field is being computed…"
            } else {
                "no search (photo B was not sampled)"
            })
            .small()
            .weak(),
        );
        let margin = Vec2::new(34.0, 4.0);
        let (outer, _) = ui.allocate_exact_size(Vec2::new(MAP_BOX + margin.x + 70.0, MAP_BOX + 30.0), Sense::hover());
        let rect = Rect::from_min_size(outer.min + margin, Vec2::splat(MAP_BOX));
        let p = ui.painter().clone();
        if let Some(t) = &self.dtex.map {
            p.image(t.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
        let k = MAP_BOX / n as f32;
        let to = |x: f32, y: f32| rect.min + Vec2::new((x + r as f32 + 0.5) * k, (y + r as f32 + 0.5) * k);
        let tc = ui.visuals().text_color();
        // axes in pixels, 0 in the middle
        for t in [-r, -r / 2, 0, r / 2, r] {
            let px = to(t as f32, 0.0).x;
            p.text(Pos2::new(px, rect.max.y + 3.0), Align2::CENTER_TOP, t.to_string(), FontId::proportional(10.0), tc);
            let py = to(0.0, t as f32).y;
            p.text(Pos2::new(rect.min.x - 4.0, py), Align2::RIGHT_CENTER, t.to_string(), FontId::proportional(10.0), tc);
        }
        p.text(Pos2::new(rect.center().x, rect.max.y + 15.0), Align2::CENTER_TOP, "dx (px)", FontId::proportional(10.0), tc);
        p.text(Pos2::new(outer.min.x, rect.min.y - 2.0), Align2::LEFT_BOTTOM, "dy", FontId::proportional(10.0), tc);
        // zero point
        p.circle_stroke(to(0.0, 0.0), 5.0, Stroke::new(1.5, Color32::WHITE));
        // second-best peak
        let (_, second) = pv::peaks(&self.dtex.map_vals, n);
        if let Some((x, y, v)) = second {
            let c = to(x as f32 - r as f32, y as f32 - r as f32);
            p.add(egui::Shape::convex_polygon(vec![c + Vec2::new(0.0, -6.0), c + Vec2::new(5.0, 4.0), c + Vec2::new(-5.0, 4.0)], Color32::TRANSPARENT, Stroke::new(1.5, C_YELLOW)));
            p.text(c + Vec2::new(7.0, 0.0), Align2::LEFT_CENTER, format!("{v:.2}"), FontId::proportional(10.0), C_YELLOW);
        }
        // found optimum (sub-pixel), the automatic one
        let o = to(d.auto.dx, d.auto.dy);
        p.line_segment([o + Vec2::new(-6.0, -6.0), o + Vec2::new(6.0, 6.0)], Stroke::new(2.0, C_RED));
        p.line_segment([o + Vec2::new(-6.0, 6.0), o + Vec2::new(6.0, -6.0)], Stroke::new(2.0, C_RED));
        // current (hand) position
        if st.manual || self.drag_shift.is_some() {
            let c = to(shift.0, shift.1);
            p.add(egui::Shape::convex_polygon(vec![c + Vec2::new(0.0, -6.0), c + Vec2::new(6.0, 0.0), c + Vec2::new(0.0, 6.0), c + Vec2::new(-6.0, 0.0)], Color32::TRANSPARENT, Stroke::new(2.0, Color32::from_rgb(90, 200, 255))));
        }
        // legend of the map
        let lx = rect.max.x + 8.0;
        p.text(Pos2::new(lx, rect.min.y), Align2::LEFT_TOP, format!("max {:.3}", self.dtex.map_hi), FontId::proportional(10.0), tc);
        p.text(Pos2::new(lx, rect.max.y), Align2::LEFT_BOTTOM, format!("min {:.3}", self.dtex.map_lo), FontId::proportional(10.0), tc);
        ui.label(
            RichText::new(format!(
                "✕ optimum ({:.2}, {:.2}) px · ○ zero point · △ second peak{}{}",
                d.auto.dx,
                d.auto.dy,
                second.map(|(x, y, v)| format!(" at ({}, {}), score {:.3}", x as isize - r, y as isize - r, v)).unwrap_or_else(|| " (none)".into()),
                if st.manual || self.drag_shift.is_some() { format!(" · ◇ by hand ({:.2}, {:.2})", shift.0, shift.1) } else { String::new() }
            ))
            .small(),
        );
        let _ = corrected;
    }
}

fn st_missing_cp(data: &[Arc<PatchData>], by_id: &HashMap<u32, usize>, id: u32) -> bool {
    let Some(&i) = by_id.get(&id) else { return false };
    let d = &data[i];
    d.geo_b.dir_to_px(d.plane.dir(0.0, 0.0)).is_none()
}

/// Brightness gain of a photo (1 when unknown).
fn gain_of(g: &[[f32; 3]], i: usize) -> [f32; 3] {
    g.get(i).copied().unwrap_or([1.0; 3])
}

fn dashed_rect(p: &egui::Painter, r: Rect, col: Color32) {
    let pts = vec![r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
    p.extend(egui::Shape::dashed_line(&pts, Stroke::new(1.0, col), 4.0, 3.0));
}

fn crosshair(p: &egui::Painter, r: Rect) {
    let c = r.center();
    let col = Color32::from_rgba_unmultiplied(255, 80, 80, 160);
    p.line_segment([Pos2::new(c.x - 6.0, c.y), Pos2::new(c.x + 6.0, c.y)], Stroke::new(1.0, col));
    p.line_segment([Pos2::new(c.x, c.y - 6.0), Pos2::new(c.x, c.y + 6.0)], Stroke::new(1.0, col));
}

fn legend(ui: &mut egui::Ui) {
    ui.label(RichText::new("Rows:").small());
    let (r, _) = ui.allocate_exact_size(Vec2::new(90.0, 12.0), Sense::hover());
    let p = ui.painter();
    for k in 0..30 {
        let t = k as f32 / 29.0;
        let x0 = r.min.x + r.width() * k as f32 / 30.0;
        p.rect_filled(Rect::from_min_max(Pos2::new(x0, r.min.y), Pos2::new(x0 + r.width() / 30.0 + 0.5, r.max.y)), 0.0, conf_colour(t));
    }
    ui.label(RichText::new("confidence 0 … 0.5 … 1").small());
    let (r2, _) = ui.allocate_exact_size(Vec2::new(12.0, 12.0), Sense::hover());
    ui.painter().rect_filled(r2, 0.0, C_RED);
    ui.label(RichText::new("rejected").small());
    let (r3, _) = ui.allocate_exact_size(Vec2::new(12.0, 12.0), Sense::hover());
    ui.painter().rect_filled(r3, 0.0, C_GREY);
    ui.label(RichText::new("disabled").small());
}

fn colour_bar(ui: &mut egui::Ui, mode: HeatMode, scale: f32, space: DiffSpace, width: f32) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(width.max(120.0), 26.0), Sense::hover());
    let bar = Rect::from_min_size(r.min, Vec2::new(r.width(), 10.0));
    let p = ui.painter();
    for k in 0..COLORBAR_STEPS {
        let t = k as f32 / (COLORBAR_STEPS - 1) as f32;
        let c = match mode {
            HeatMode::Signed => pv::heat_colour(mode, 0.0, (t * 2.0 - 1.0) * scale, scale),
            _ => pv::heat_colour(mode, t * scale, 0.0, scale),
        };
        let x0 = bar.min.x + bar.width() * k as f32 / COLORBAR_STEPS as f32;
        p.rect_filled(
            Rect::from_min_max(Pos2::new(x0, bar.min.y), Pos2::new(x0 + bar.width() / COLORBAR_STEPS as f32 + 0.5, bar.max.y)),
            0.0,
            Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8),
        );
    }
    let tc = ui.visuals().text_color();
    let unit = if space == DiffSpace::RawColor { 255.0 } else { 1.0 };
    let f = |v: f32| if unit > 1.0 { format!("{:.0}", v * unit) } else { format!("{v:.3}") };
    let (lo, mid, hi) = if mode == HeatMode::Signed { (f(-scale), "0".to_string(), f(scale)) } else { (f(0.0), f(scale * 0.5), f(scale)) };
    p.text(Pos2::new(bar.min.x, bar.max.y + 2.0), Align2::LEFT_TOP, lo, FontId::proportional(10.0), tc);
    p.text(Pos2::new(bar.center().x, bar.max.y + 2.0), Align2::CENTER_TOP, mid, FontId::proportional(10.0), tc);
    p.text(Pos2::new(bar.max.x, bar.max.y + 2.0), Align2::RIGHT_TOP, hi, FontId::proportional(10.0), tc);
}

/// Write the PNGs of one patch (worker thread).
#[allow(clippy::too_many_arguments)]
fn export_one(folder: &std::path::Path, d: &PatchData, st: &PatchState, mode: HeatMode, space: DiffSpace, auto: bool, zoom: usize, overlay: bool, rgb: Option<pv::Raw>) -> Result<(), String> {
    std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    let save = |name: &str, w: usize, px: &[u8]| image::save_buffer(folder.join(name), px, w as u32, w as u32, image::ColorType::Rgba8).map_err(|e| format!("{name}: {e}"));
    let corrected = st.status == Status::Valid || st.manual;
    let shift = (st.dx, st.dy);
    let space = if space == DiffSpace::RawColor && rgb.is_none() { DiffSpace::Feature } else { space };
    match pv::patch_pair(d, shift, space, rgb, true) {
        Some(pp) => {
            let (w, px) = pv::patch_rgba(&pp.a, &pp.va, pp.n, zoom, false);
            save("a.png", w, &px)?;
            let (w, pxb) = pv::patch_rgba(&pp.b, &pp.vb, pp.n, zoom, false);
            save("b.png", w, &pxb)?;
            if overlay {
                let mix: Vec<u8> = px.iter().zip(&pxb).map(|(a, b)| ((*a as u16 + *b as u16) / 2) as u8).collect();
                save("overlay.png", w, &mix)?;
            }
        }
        None => {
            let n = d.a_size;
            let px: Vec<[f32; 3]> = d.a.iter().map(|&b| [if b == 0 { 0.0 } else { (b - 1) as f32 / 254.0 }; 3]).collect();
            let valid: Vec<bool> = d.a.iter().map(|&b| b != 0).collect();
            let (w, px) = pv::patch_rgba(&px, &valid, n, zoom, false);
            save("a.png", w, &px)?;
        }
    }
    let before = pv::diff(d, (0.0, 0.0), space, rgb);
    let after = if corrected { pv::diff(d, shift, space, rgb) } else { None };
    if let Some(b) = &before {
        let mut list = vec![b];
        if let Some(a) = &after {
            list.push(a);
        }
        let scale = pv::shared_scale(&list, auto, mode == HeatMode::Signed);
        let (w, px) = pv::heat_rgba(b, mode, scale, zoom);
        save("diff_before.png", w, &px)?;
        if let Some(a) = &after {
            let (w, px) = pv::heat_rgba(a, mode, scale, zoom);
            save("diff_after.png", w, &px)?;
        }
    }
    let n = (2 * d.map_radius + 1) as usize;
    let map = match d.region_b() {
        Some(rb) => patch_matcher::full_score_map(&d.region_a(), &rb, &d.cfg, d.map_radius),
        None => d.map_f32(),
    };
    let (w, px, _, _) = pv::score_rgba(&map, n, zoom);
    save("score_map.png", w, &px)?;
    Ok(())
}

#[cfg(test)]
mod screen_tests {
    use super::*;

    #[test]
    fn positions_off_every_screen_are_rejected() {
        assert!(on_screen([100.0, 100.0], [800.0, 600.0], 1.0));
        assert!(!on_screen([-32000.0, -32000.0], [800.0, 600.0], 1.0));
        assert!(!on_screen([60000.0, 100.0], [800.0, 600.0], 1.0));
    }
}
