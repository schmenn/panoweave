//! The window: a guided, step-by-step panorama studio plus an exposure-blending mode.

mod align;
mod connections;
mod export;
mod exposure;
mod frame;
mod photos;
mod points;
mod ui_helpers;

use self::{connections::*, export::*, ui_helpers::*};

use crate::align::{ControlPoint, Lens};
use crate::engine::{self, AlignResult, AlignSettings, LoadedPhoto, MatchResult, MatchSettings, Source};
use crate::fusion::{self, FusionWeights};
use crate::geometry::{self, h_apply, mtv, mv, M3, IDENTITY};
use crate::guided::{self, P2};
use crate::align::CpKind;
use crate::fine_align::{self, RobustLoss};
use crate::patch_matcher::{FeatureSpace, Metric};
use crate::geometry::{normalize, V3};
use crate::imgutil::{self, Gray, PhotoInfo};
use crate::project::{self, ProjectFile, ProjectPhoto};
use crate::sample;
use crate::stitch::{self, Blend, PanoView, Projection, RenderPhoto};
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2};
use image::RgbImage;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Magnifier: /CPEditorPanel/MagnifierWidth default 61 px (hugin.exe sub_14019f160).
const MAGNIFIER_WIDTH: f32 = 61.0;
/// Frame colours of the original's point editor (sub_14006cfe0): template RED, search area WHITE.
const TEMPLATE_FRAME_COLOR: Color32 = Color32::from_rgb(255, 60, 60);
const SEARCH_FRAME_COLOR: Color32 = Color32::WHITE;
/// Point colours in the order the original's point editor uses them (sub_140067330):
/// BLUE, GREEN, CYAN, GOLD, NAVY, DARK TURQUOISE, SALMON, MAROON, KHAKI
/// (RGB as defined by the wxWidgets colour database).
const POINT_COLORS: [Color32; 9] = [
    Color32::from_rgb(0, 0, 255),
    Color32::from_rgb(0, 255, 0),
    Color32::from_rgb(0, 255, 255),
    Color32::from_rgb(204, 127, 50),
    Color32::from_rgb(35, 35, 142),
    Color32::from_rgb(112, 147, 219),
    Color32::from_rgb(111, 66, 66),
    Color32::from_rgb(142, 35, 107),
    Color32::from_rgb(159, 159, 95),
];

fn point_color(i: usize) -> Color32 {
    POINT_COLORS[i % POINT_COLORS.len()]
}

/// Image-cache bound of the original: /ImageCache/UpperBound default 0x10000000 = 256 MiB
/// (hugin.exe sub_1400550e0, sub_14019f160). Used here for the full-size photos of the point editor.
const IMAGE_CACHE_UPPER_BOUND: u64 = 268435456; // 0x10000000
/// Share of the free memory the final render may use.
/// Memory budget of the low-memory export when the free memory of the computer is unknown (2 GB).
const FALLBACK_EXPORT_BUDGET: u64 = 2 << 30;
const ACCENT: Color32 = Color32::from_rgb(70, 160, 255);
const ACCENT_DARK: Color32 = Color32::from_rgb(34, 92, 160);
const GOOD: Color32 = Color32::from_rgb(90, 200, 120);
const WARN: Color32 = Color32::from_rgb(240, 190, 70);
const BAD: Color32 = Color32::from_rgb(235, 90, 80);
const PANEL: Color32 = Color32::from_rgb(28, 30, 35);
const BG: Color32 = Color32::from_rgb(20, 21, 25);
const CARD: Color32 = Color32::from_rgb(36, 39, 46);
const MUTED: Color32 = Color32::from_rgb(150, 156, 168);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Photos,
    Points,
    Align,
    Frame,
    Export,
}

impl Step {
    const ALL: [Step; 5] = [Step::Photos, Step::Points, Step::Align, Step::Frame, Step::Export];
    fn title(&self) -> &'static str {
        match self {
            Step::Photos => "Photos",
            Step::Points => "Matching points",
            Step::Align => "Align",
            Step::Frame => "Frame & crop",
            Step::Export => "Save",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Panorama,
    Exposure,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutFormat {
    Jpeg,
    Png,
    Tiff,
}

impl OutFormat {
    fn ext(&self) -> &'static str {
        match self {
            OutFormat::Jpeg => "jpg",
            OutFormat::Png => "png",
            OutFormat::Tiff => "tif",
        }
    }
    fn name(&self) -> &'static str {
        match self {
            OutFormat::Jpeg => "JPEG",
            OutFormat::Png => "PNG (with transparency)",
            OutFormat::Tiff => "TIFF (with transparency)",
        }
    }
}

struct Photo {
    name: String,
    source: Source,
    w: u32,
    h: u32,
    work: Arc<RgbImage>,
    gray: Arc<Gray>,
    info: PhotoInfo,
    thumb: egui::TextureHandle,
    tex: Option<egui::TextureHandle>,
    lens: usize,
    rot: M3,
    enabled: bool,
}

impl Photo {
    fn long(&self) -> f64 {
        self.w.max(self.h) as f64
    }
}

enum JobMsg {
    Loaded(Vec<Result<LoadedPhoto, String>>),
    Matched(MatchResult),
    Aligned(AlignResult),
    Rendered(Result<(PathBuf, u128, usize, usize), String>),
    BracketLoaded(Vec<Result<(String, Source, Arc<RgbImage>, RgbImage, PhotoInfo), String>>),
    FusedSaved(Result<(PathBuf, u128), String>),
    PairPoints { a: usize, b: usize, cps: Vec<ControlPoint>, ms: u128, mode: &'static str },
    /// "Search again" for one pair from the connection view
    PairChecked { a: usize, b: usize, cps: Vec<ControlPoint>, reports: Vec<crate::pair_check::PairReport> },
    /// patch click mode result
    ClickMatched(Box<crate::click_match::ClickResult>),
    /// the person pressed Cancel: the job's result is thrown away
    Cancelled,
}

struct Job {
    title: String,
    rx: Receiver<JobMsg>,
    progress: Arc<Mutex<(f32, String)>>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

struct Bracket {
    name: String,
    source: Source,
    work: Arc<RgbImage>,
    thumb: egui::TextureHandle,
    info: PhotoInfo,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Pane {
    A,
    B,
}

impl Pane {
    fn i(self) -> usize {
        match self {
            Pane::A => 0,
            Pane::B => 1,
        }
    }
    fn other(self) -> Pane {
        match self {
            Pane::A => Pane::B,
            Pane::B => Pane::A,
        }
    }
}

/// Zoom and position of one photo half in the point editor.
#[derive(Clone, Copy, Default)]
struct PaneView {
    /// 1 = whole photo fits
    zoom: f32,
    /// photo pixel shown in the middle
    center: Vec2,
    /// photo shown + 1 (0 = nothing yet)
    key: usize,
}

#[derive(Clone, Copy)]
struct PaneGeo {
    rect: Rect,
    origin: Pos2,
    scale: f32,
    fit: f32,
    w: f32,
    h: f32,
}

impl Default for PaneGeo {
    fn default() -> Self {
        PaneGeo { rect: Rect::NOTHING, origin: Pos2::ZERO, scale: 1.0, fit: 1.0, w: 1.0, h: 1.0 }
    }
}

enum PaneEvent {
    Fit(Pane),
    Zoom(Pane, Pos2, f32),
    Click(Pane, Pos2),
    DragStart(Pane, Pos2, bool),
    Drag(Pane, Pos2, Vec2),
    DragEnd,
}

#[derive(Default)]
struct PointEditor {
    a: usize,
    b: usize,
    selected: Option<usize>,
    adding: bool,
    pending: Option<(Pane, [f64; 2])>,
    dragging: Option<(usize, Pane)>,
    panning: Option<Pane>,
    views: [PaneView; 2],
    /// photo half whose point gets searched (the other one keeps its spot)
    moving: Option<Pane>,
}

/// Matching window of the selected point: which half stays, and the local zoom/rotation
/// from the fixed photo to the other one (working-image pixels).
#[derive(Clone, Copy, PartialEq)]
struct SelFrame {
    key: (usize, bool, i64, i64, i64, i64, usize),
    fixed: Pane,
    j: [f64; 4],
}

/// Columns of the control point list.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CpCol {
    Index,
    PhotoA,
    PhotoB,
    PairIndex,
    Error,
    Match,
    Kind,
}

#[derive(Clone, Copy, PartialEq)]
enum FrameDrag {
    None,
    Rotate,
    Roll,
    Crop(usize),
}

pub struct PanoApp {
    mode: Mode,
    step: Step,
    photos: Vec<Photo>,
    lenses: Vec<Lens>,
    cps: Vec<ControlPoint>,
    gains: Vec<[f32; 3]>,
    view: PanoView,
    aligned: bool,
    matched: bool,
    needs_align: bool,
    match_settings: MatchSettings,
    align_settings: AlignSettings,
    match_brightness: bool,
    last_match: Option<(usize, u128, u128, usize)>,
    last_align: Option<(f64, f64, usize, u128)>,
    job: Option<Job>,
    auto_chain: bool,
    status: String,
    status_color: Color32,
    pe: PointEditor,
    err_threshold: f64,
    // preview
    preview_tex: Option<egui::TextureHandle>,
    preview_size: [usize; 2],
    preview_gen: u64,
    preview_shown_gen: u64,
    preview_rx: Option<Receiver<(u64, egui::ColorImage, PanoView)>>,
    preview_view: Option<PanoView>,
    preview_dirty: bool,
    show_outlines: bool,
    frame_drag: FrameDrag,
    // export
    out_scale: f32,
    out_format: OutFormat,
    jpeg_quality: u8,
    export_opts: export::ExportOptions,
    blend: Blend,
    out_path: String,
    last_output: Option<PathBuf>,
    // exposure mode
    brackets: Vec<Bracket>,
    fusion_weights: FusionWeights,
    fusion_align: bool,
    fusion_tex: Option<egui::TextureHandle>,
    fusion_dirty: bool,
    fusion_rx: Option<Receiver<egui::ColorImage>>,
    fusion_out_path: String,
    // project
    project_path: Option<PathBuf>,
    pending_project: Option<ProjectFile>,
    // automation
    screenshot: Option<PathBuf>,
    shot_data: Arc<Mutex<Option<Arc<egui::ColorImage>>>>,
    shot_requested: bool,
    frames: u64,
    demo: Option<String>,
    demo_started: bool,
    show_help: bool,
    /// list of all control points (like the original's F3 list)
    show_cp_list: bool,
    /// fine alignment (patch matching) settings and last result
    fine_cfg: crate::fine_align::FineConfig,
    last_fine: Option<crate::fine_align::FineStats>,
    /// final render: multi-band levels (0 = automatic as before) and bicubic sampling
    blend_levels: usize,
    cubic_interp: bool,
    /// use the most detailed photo where photos overlap
    prefer_detail: bool,
    /// parallax correction fields per photo (from the last alignment)
    fields: Vec<Option<Arc<fine_align::DisplacementField>>>,
    parallax_cfg: fine_align::ParallaxConfig,
    /// overlap check window
    show_overlap: bool,
    ov_mode: u8,
    ov_alpha: f32,
    ov_tile: usize,
    ov_zoom: f32,
    ov_arrows: bool,
    ov_center: Option<((usize, usize), V3)>,
    ov_tex: Option<egui::TextureHandle>,
    ov_key: String,
    /// counts alignments (to refresh views)
    align_version: u64,
    cp_sort: (CpCol, bool),
    cp_only_pair: bool,
    cp_min_err: f64,
    /// sorted rows of the control point list and the number within each pair (cached)
    cp_rows_cache: Option<(String, Vec<usize>, Vec<usize>)>,
    render_started: bool,
    second_render_started: bool,
    auto_grow: bool,
    full_tex: HashMap<usize, egui::TextureHandle>,
    full_pending: HashSet<usize>,
    full_tx: Sender<(usize, String, egui::ColorImage)>,
    full_rx: Receiver<(usize, String, egui::ColorImage)>,
    /// matching window size (working-image pixels, odd)
    tpl_size: i32,
    /// search area in % of the photo width
    search_pct: f64,
    sel_frame: Option<SelFrame>,
    edge_tex: [Option<egui::TextureHandle>; 2],
    /// patch comparison window (its own OS window; the records live in patch_store)
    patch_win: crate::patch_window::PatchWindow,
    /// geometric pair check (part of the alignment) and its last result
    pair_cfg: crate::pair_check::PairCheckConfig,
    pair_summary: Option<crate::pair_check::PairSummary>,
    /// connection view (graph + warning list)
    show_graph: bool,
    graph_photo: Option<usize>,
    /// patch click mode of the point editor
    click: ClickState,
    click_cfg: crate::click_match::ClickConfig,
    undo: Vec<UndoEntry>,
}

/// Patch click mode: the two rough clicks and the last result (suggestions).
#[derive(Default)]
struct ClickState {
    on: bool,
    a: Option<P2>,
    b: Option<P2>,
    result: Option<crate::click_match::ClickResult>,
}

/// One step that can be undone in the point editor (points and the open suggestions).
struct UndoEntry {
    what: String,
    cps: Vec<ControlPoint>,
    result: Option<crate::click_match::ClickResult>,
}

/// Steps kept for undo.
const UNDO_DEPTH: usize = 30;

impl PanoApp {
    pub fn new(cc: &eframe::CreationContext<'_>, args: crate::Args) -> Self {
        setup_style(&cc.egui_ctx);
        let (full_tx, full_rx) = channel();
        let mut app = PanoApp {
            mode: Mode::Panorama,
            step: Step::Photos,
            photos: vec![],
            lenses: vec![],
            cps: vec![],
            gains: vec![],
            view: PanoView::default(),
            aligned: false,
            matched: false,
            needs_align: false,
            match_settings: MatchSettings::default(),
            align_settings: AlignSettings::default(),
            match_brightness: true,
            last_match: None,
            last_align: None,
            job: None,
            auto_chain: false,
            status: "Welcome! Add the photos of your panorama to begin.".into(),
            status_color: MUTED,
            pe: PointEditor { b: 1, ..Default::default() },
            err_threshold: 5.0,
            preview_tex: None,
            preview_size: [0, 0],
            preview_gen: 0,
            preview_shown_gen: 0,
            preview_rx: None,
            preview_view: None,
            preview_dirty: false,
            show_outlines: false,
            frame_drag: FrameDrag::None,
            out_scale: 70.0,
            out_format: OutFormat::Jpeg,
            jpeg_quality: 90,
            export_opts: export::ExportOptions::default(),
            blend: Blend::Smooth,
            out_path: String::new(),
            last_output: None,
            brackets: vec![],
            fusion_weights: FusionWeights::default(),
            fusion_align: true,
            fusion_tex: None,
            fusion_dirty: false,
            fusion_rx: None,
            fusion_out_path: String::new(),
            project_path: None,
            pending_project: None,
            screenshot: args.screenshot,
            shot_data: Arc::new(Mutex::new(None)),
            shot_requested: false,
            frames: 0,
            demo: args.demo,
            demo_started: false,
            show_help: false,
            show_cp_list: false,
            fine_cfg: crate::fine_align::FineConfig::default(),
            last_fine: None,
            blend_levels: 4,
            cubic_interp: true,
            prefer_detail: true,
            fields: Vec::new(),
            parallax_cfg: fine_align::ParallaxConfig::default(),
            show_overlap: false,
            ov_mode: 0,
            ov_alpha: 0.5,
            ov_tile: 32,
            ov_zoom: 1.0,
            ov_arrows: true,
            ov_center: None,
            ov_tex: None,
            ov_key: String::new(),
            align_version: 0,
            cp_sort: (CpCol::Index, true),
            cp_only_pair: false,
            cp_min_err: 0.0,
            cp_rows_cache: None,
            render_started: false,
            second_render_started: false,
            auto_grow: true,
            full_tex: HashMap::new(),
            full_pending: HashSet::new(),
            full_tx,
            full_rx,
            tpl_size: guided::TEMPLATE_SIZE,
            search_pct: guided::SEARCH_AREA_PERCENT,
            sel_frame: None,
            edge_tex: [None, None],
            patch_win: crate::patch_window::PatchWindow::new(),
            pair_cfg: crate::pair_check::PairCheckConfig::default(),
            pair_summary: None,
            show_graph: false,
            graph_photo: None,
            click: ClickState::default(),
            click_cfg: crate::click_match::ClickConfig::default(),
            undo: Vec::new(),
        };
        app.patch_win.no_save = app.screenshot.is_some();
        app.patch_win.demo_scroll = std::env::var("PW_SCROLL").ok().and_then(|v| v.parse().ok());
        if !args.files.is_empty() {
            app.add_paths(args.files);
        }
        app
    }

    fn set_status(&mut self, s: impl Into<String>, c: Color32) {
        self.status = s.into();
        self.status_color = c;
    }

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    fn start_job(&mut self, title: &str, f: impl FnOnce(&(dyn Fn(f32, &str) + Sync), &AtomicBool) -> JobMsg + Send + 'static) {
        let (tx, rx) = channel();
        let progress = Arc::new(Mutex::new((0.0f32, title.to_string())));
        let cancel = Arc::new(AtomicBool::new(false));
        engine::CANCEL.store(false, Ordering::Relaxed);
        let p2 = progress.clone();
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            let report = move |v: f32, s: &str| {
                if let Ok(mut g) = p2.lock() {
                    *g = (v, s.to_string());
                }
            };
            let msg = f(&report, &c2);
            let _ = tx.send(msg);
        });
        self.job = Some(Job { title: title.into(), rx, progress, cancel, started: Instant::now() });
    }

    // ---------------- pipeline ----------------

    fn active_indices(&self) -> Vec<usize> {
        (0..self.photos.len()).filter(|&i| self.photos[i].enabled).collect()
    }

    /// Per photo: joined to the biggest group of photos by control points (see `image_groups`).
    fn connected_photos(&self) -> Vec<bool> {
        let active = self.active_indices();
        let groups = image_groups(self.photos.len(), &active, &self.cps);
        let main = groups.iter().max_by_key(|g| g.len()).cloned().unwrap_or_default();
        (0..self.photos.len()).map(|i| main.contains(&i) || active.len() < 2).collect()
    }

    fn render_photos(&self, active_only: bool) -> Vec<RenderPhoto> {
        let trusted = self.connected_photos();
        self.photos
            .iter()
            .enumerate()
            .filter(|(_, p)| !active_only || p.enabled)
            .map(|(i, p)| RenderPhoto {
                img: p.work.clone(),
                w: p.work.width() as f64,
                h: p.work.height() as f64,
                rot: p.rot,
                lens: self.lenses[p.lens],
                gain: if self.match_brightness { self.gains.get(i).copied().unwrap_or([1.0; 3]) } else { [1.0; 3] },
                field: self.fields.get(i).cloned().flatten(),
                full_long: p.long(),
                trusted: trusted.get(i).copied().unwrap_or(true),
            })
            .collect()
    }

    fn update_gains(&mut self) {
        let active = self.active_indices();
        let rp = self.render_photos(true);
        let g = stitch::compute_gains(&rp, &self.view);
        self.gains = vec![[1.0; 3]; self.photos.len()];
        for (k, &i) in active.iter().enumerate() {
            self.gains[i] = g[k];
        }
    }

    // ---------------- per-frame plumbing ----------------

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(job) = &self.job {
            if let Ok(msg) = job.rx.try_recv() {
                let elapsed = job.started.elapsed().as_millis();
                let title = job.title.clone();
                let was_cancelled = job.cancel.load(Ordering::Relaxed) || engine::cancelled();
                self.job = None;
                engine::CANCEL.store(false, Ordering::Relaxed);
                let msg = if was_cancelled && !matches!(msg, JobMsg::Rendered(_) | JobMsg::FusedSaved(_)) { JobMsg::Cancelled } else { msg };
                match msg {
                    JobMsg::Cancelled => {
                        self.auto_chain = false;
                        if title.starts_with("Loading") {
                            // the original's wording (hugin.exe sub_14004d010)
                            self.set_status("Loading canceled – no photos were added.", WARN);
                        } else {
                            self.set_status(format!("{title}: canceled – nothing was changed."), WARN);
                        }
                    }
                    JobMsg::Loaded(r) => self.on_loaded(ctx, r),
                    JobMsg::Matched(m) => self.on_matched(m),
                    JobMsg::Aligned(a) => self.on_aligned(a),
                    JobMsg::Rendered(r) => match r {
                        Ok((p, ms, w, h)) => {
                            self.set_status(format!("Panorama saved ({w} × {h} px) in {}: {}", fmt_ms(ms), p.display()), GOOD);
                            self.last_output = Some(p);
                            // the patch comparison data are released after the export
                            crate::patch_store::STORE.free_after_export();
                        }
                        Err(e) if e == stitch::CANCELLED => self.set_status("Export canceled – no file was written.", WARN),
                        Err(e) => self.set_status(format!("{title} stopped: {e}"), BAD),
                    },
                    JobMsg::BracketLoaded(r) => {
                        let mut bad = 0;
                        for x in r {
                            match x {
                                Ok((name, source, work, thumb, info)) => {
                                    let t = ctx.load_texture(format!("br-{}-{name}", self.brackets.len()), color_image(&thumb), egui::TextureOptions::LINEAR);
                                    self.brackets.push(Bracket { name, source, work, thumb: t, info });
                                }
                                Err(_) => bad += 1,
                            }
                        }
                        // darkest first
                        self.brackets.sort_by(|a, b| {
                            let ea = a.info.brightness_ev().unwrap_or_else(|| mean_luma(&a.work));
                            let eb = b.info.brightness_ev().unwrap_or_else(|| mean_luma(&b.work));
                            ea.partial_cmp(&eb).unwrap_or(std::cmp::Ordering::Equal)
                        });
                        self.fusion_dirty = true;
                        self.set_status(
                            if bad == 0 { format!("{} exposures ready to blend.", self.brackets.len()) } else { format!("{bad} photos could not be read.") },
                            if bad == 0 { GOOD } else { WARN },
                        );
                    }
                    JobMsg::PairPoints { a, b, cps, ms, mode } => self.on_pair_points(a, b, cps, ms, mode),
                    JobMsg::PairChecked { a, b, cps, reports } => self.on_pair_checked(a, b, cps, reports),
                    JobMsg::ClickMatched(r) => self.on_click_matched(*r),
                    JobMsg::FusedSaved(r) => match r {
                        Ok((p, ms)) => {
                            self.set_status(format!("Blended picture saved in {}: {}", fmt_ms(ms), p.display()), GOOD);
                            self.last_output = Some(p);
                        }
                        Err(e) => self.set_status(format!("Blending stopped: {e}"), BAD),
                    },
                }
                let _ = elapsed;
            }
        }
        if let Some(rx) = &self.preview_rx {
            if let Ok((generation, img, view)) = rx.try_recv() {
                self.preview_rx = None;
                self.preview_size = img.size;
                match &mut self.preview_tex {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => self.preview_tex = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
                }
                self.preview_shown_gen = generation;
                self.preview_view = Some(view);
            }
        }
        while let Ok((i, name, img)) = self.full_rx.try_recv() {
            self.full_pending.remove(&i);
            // only if the photo at that place is still the same one
            if self.photos.get(i).map(|p| p.name == name).unwrap_or(false) {
                self.full_tex.insert(i, ctx.load_texture(format!("full-{i}-{name}"), img, egui::TextureOptions::LINEAR));
            }
        }
        if let Some(rx) = &self.fusion_rx {
            if let Ok(img) = rx.try_recv() {
                self.fusion_rx = None;
                match &mut self.fusion_tex {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => self.fusion_tex = Some(ctx.load_texture("fusion", img, egui::TextureOptions::LINEAR)),
                }
            }
        }
        if self.fusion_dirty && self.mode == Mode::Exposure {
            self.request_fusion_preview();
        }
    }

    fn request_preview(&mut self, max_w: usize, fast: bool) {
        if self.preview_rx.is_some() || !self.aligned {
            return;
        }
        let rp = self.render_photos(true);
        if rp.is_empty() {
            return;
        }
        let view = self.view.clone();
        let aspect = view.aspect().clamp(0.05, 40.0);
        let mut w = if fast { max_w.min(700) } else { max_w.min(1800) };
        let mut h = (w as f64 / aspect).round() as usize;
        if h > 1100 {
            h = 1100;
            w = (h as f64 * aspect) as usize;
        }
        let (w, h) = (w.max(16), h.max(8));
        self.preview_gen += 1;
        let generation = self.preview_gen;
        let prefer_detail = self.prefer_detail;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let px = stitch::render_preview(&rp, &view, w, h, true, prefer_detail);
            let _ = tx.send((generation, egui::ColorImage::from_rgba_unmultiplied([w, h], &px), view));
        });
        self.preview_rx = Some(rx);
        self.preview_dirty = false;
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            self.add_paths(dropped);
        }
    }

    fn automation(&mut self, ctx: &egui::Context) {
        self.frames += 1;
        if let Some(d) = self.demo.clone() {
            if !self.demo_started {
                self.demo_started = true;
                if d == "patches" || d == "patchdetail" {
                    // the screenshot only shows the main window, so the patch window is embedded;
                    // it is open from the start so the live preview runs during the matching
                    ctx.set_embed_viewports(true);
                    self.patch_win.open = true;
                }
                self.show_cp_list = d == "cplist";
                self.show_overlap = d == "overlap";
                match d.as_str() {
                    "exposure" => {
                        self.mode = Mode::Exposure;
                        self.load_bracket_sample();
                    }
                    "photos" => self.load_sample(),
                    _ => {
                        self.auto_chain = true;
                        if !self.busy() && self.photos.is_empty() {
                            self.load_sample();
                        }
                    }
                }
            }
            // after the sample photos are in, continue the chain
            if self.auto_chain && !self.busy() && !self.photos.is_empty() && !self.matched {
                self.create_auto();
            }
            if d == "render" && !self.busy() && self.aligned && self.last_output.is_none() && !self.render_started {
                self.render_started = true;
                // experiments: PW_SAVE_PROJECT=<file.pwproj> keeps the alignment, a .pwproj on the command line loads it again (no new alignment)
                if let Ok(p) = std::env::var("PW_SAVE_PROJECT") {
                    self.save_project(PathBuf::from(p));
                }
                self.step = Step::Export;
                self.out_scale = std::env::var("PW_SCALE").ok().and_then(|v| v.parse().ok()).unwrap_or(50.0);
                self.out_path = std::env::current_dir().unwrap_or_default().join("shots").join("demo_render.jpg").to_string_lossy().to_string();
                // experiments: PW_OUT=<file.png|tif|jpg> chooses file and format, PW_LOWMEM=1 the low-memory export
                if let Ok(o) = std::env::var("PW_OUT") {
                    self.out_format = match std::path::Path::new(&o).extension().and_then(|e| e.to_str()) {
                        Some("png") => OutFormat::Png,
                        Some("tif") | Some("tiff") => OutFormat::Tiff,
                        _ => OutFormat::Jpeg,
                    };
                    self.out_path = o;
                }
                self.run_render();
            }
            // PW_OUT2=<file>: after the first export, the same alignment is exported again (with another
            // PW_EXPORT_MB it shows that the memory does not change the picture)
            if d == "render" && self.render_started && !self.busy() && self.last_output.is_some() && !self.second_render_started {
                if let Ok(o2) = std::env::var("PW_OUT2") {
                    self.second_render_started = true;
                    // SAFETY: as below, UI thread only
                    unsafe { std::env::remove_var("PW_OLD_EXPORT") };
                    if let Ok(v) = std::env::var("PW_SEAMS2") {
                        // SAFETY: as below
                        unsafe { std::env::set_var("PW_SEAMS", v) };
                    }
                    if let Some(mb) = std::env::var("PW_EXPORT_MB2").ok().and_then(|v| v.parse::<u64>().ok()) {
                        // SAFETY: the demo runs on the UI thread, the worker threads do not read this variable
                        unsafe { std::env::set_var("PW_EXPORT_MB", mb.to_string()) };
                    }
                    self.out_path = o2;
                    self.last_output = None;
                    self.run_render();
                }
            }
            if !self.busy() && self.aligned && (d == "pointsel" || d == "pointdrag") && self.pe.selected.is_none() {
                // show a selected point zoomed in (for checking the frames)
                self.step = Step::Points;
                let (a, b) = (self.pe.a, self.pe.b);
                if let Some(k) = (0..self.cps.len()).find(|&k| self.cps[k].a == a && self.cps[k].b == b) {
                    self.pe.selected = Some(k);
                    self.pe.moving = Some(Pane::B);
                    let pa = self.to_px(a, self.cps[k].pa);
                    let pb = self.to_px(b, self.cps[k].pb);
                    for (i, p) in [(0usize, pa), (1, pb)] {
                        let idx = if i == 0 { a } else { b };
                        self.pe.views[i] = PaneView { zoom: 6.0, center: Vec2::new(p.0 as f32, p.1 as f32), key: idx + 1 };
                    }
                    if d == "pointdrag" {
                        self.pe.dragging = Some((k, Pane::B));
                    }
                }
            }
            if !self.busy() && self.aligned {
                match d.as_str() {
                    "points" | "pointsel" | "pointdrag" => self.step = Step::Points,
                    "overlap" => self.step = Step::Points,
                    "cplist" => {
                        self.step = Step::Points;
                        self.show_cp_list = true;
                    }
                    "align" => self.step = Step::Align,
                    "connections" => {
                        self.step = Step::Points;
                        self.show_graph = true;
                        if self.graph_photo.is_none() {
                            self.graph_photo = Some(0);
                        }
                    }
                    "patchclick" => {
                        self.step = Step::Points;
                        self.click.on = true;
                        if self.click.a.is_none() && self.click.result.is_none() {
                            // rough clicks: a real point of the pair, the right one 12 px off
                            let (a, b) = (2.min(self.photos.len() - 2), 3.min(self.photos.len() - 1));
                            self.pe.a = a;
                            self.pe.b = b;
                            let best = self.cps.iter().filter(|c| c.a == a && c.b == b && c.kind == CpKind::Point).min_by(|x, y| x.err.partial_cmp(&y.err).unwrap()).copied();
                            if let Some(c) = best {
                                let (pa, pb) = (self.to_px(a, c.pa), self.to_px(b, c.pb));
                                self.click.a = Some(pa);
                                self.click.b = Some((pb.0 + 12.0, pb.1 - 9.0));
                                self.run_click();
                            }
                        }
                    }
                    "patches" => self.step = Step::Align,
                    "patchdetail" => {
                        self.step = Step::Align;
                        if self.frames > 60 {
                            self.patch_win.open_first_detail(crate::patch_window::ViewMode::Overlay);
                        }
                    }
                    "export" => self.step = Step::Export,
                    _ => {}
                }
            }
        }
        if let Some(path) = self.screenshot.clone() {
            ctx.request_repaint();
            let settled = !self.busy() && self.preview_rx.is_none() && self.fusion_rx.is_none() && (self.step != Step::Frame || !self.preview_dirty);
            let ready = if self.demo.is_some() { settled && self.frames > 120 && (self.demo.as_deref() != Some("render") || (self.last_output.is_some() && (std::env::var("PW_OUT2").is_err() || self.second_render_started))) && (self.aligned || self.mode == Mode::Exposure || self.demo.as_deref() == Some("photos")) } else { self.frames >= 120 };
            if ready && !self.shot_requested {
                self.shot_requested = true;
                let slot = self.shot_data.clone();
                ctx.request_screenshot(move |img| {
                    *slot.lock().unwrap() = Some(img);
                });
            }
            let shot = self.shot_data.lock().unwrap().take();
            if let Some(img) = shot {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let [w, h] = img.size;
                let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                if let Err(e) = image::save_buffer(&path, &bytes, w as u32, h as u32, image::ColorType::Rgba8) {
                    eprintln!("screenshot failed: {e}");
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            if self.frames > 6000 && demo_clock().elapsed() > std::time::Duration::from_secs(DEMO_TIMEOUT_S) && !self.shot_requested {
                // never hang forever
                self.shot_requested = true;
                let slot = self.shot_data.clone();
                ctx.request_screenshot(move |img| *slot.lock().unwrap() = Some(img));
            }
        }
    }

    // ---------------- UI ----------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top")
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::symmetric(14, 8)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (r, p) = ui.allocate_painter(Vec2::new(30.0, 30.0), Sense::hover());
                    let c = r.rect.center();
                    p.circle_filled(c, 14.0, ACCENT_DARK);
                    p.line_segment([c + Vec2::new(-9.0, 4.0), c + Vec2::new(-3.0, -4.0)], Stroke::new(2.5, Color32::WHITE));
                    p.line_segment([c + Vec2::new(-3.0, -4.0), c + Vec2::new(2.0, 2.0)], Stroke::new(2.5, Color32::WHITE));
                    p.line_segment([c + Vec2::new(2.0, 2.0), c + Vec2::new(5.0, -1.0)], Stroke::new(2.5, Color32::WHITE));
                    p.line_segment([c + Vec2::new(5.0, -1.0), c + Vec2::new(9.0, 4.0)], Stroke::new(2.5, Color32::WHITE));
                    ui.label(RichText::new("PanoWeave").size(19.0).strong().color(Color32::WHITE));
                    ui.add_space(18.0);
                    for (m, label) in [(Mode::Panorama, "🌄  Panorama"), (Mode::Exposure, "◑  Exposure blend")] {
                        let sel = self.mode == m;
                        let b = egui::Button::new(RichText::new(label).size(15.0).color(if sel { Color32::WHITE } else { MUTED }))
                            .fill(if sel { ACCENT_DARK } else { Color32::TRANSPARENT })
                            .min_size(Vec2::new(0.0, 30.0));
                        if ui.add(b).clicked() {
                            self.mode = m;
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("?  Help").clicked() {
                            self.show_help = !self.show_help;
                        }
                        if self.mode == Mode::Panorama {
                            if ui.add_enabled(!self.photos.is_empty() && !self.busy(), egui::Button::new("💾  Save project")).clicked() {
                                let start = self.project_path.clone();
                                let mut d = rfd::FileDialog::new().add_filter("PanoWeave project", &["pwproj"]).set_file_name("panorama.pwproj");
                                if let Some(s) = start.as_ref().and_then(|p| p.parent()) {
                                    d = d.set_directory(s);
                                }
                                if let Some(p) = d.save_file() {
                                    self.save_project(p);
                                }
                            }
                            if ui.add_enabled(!self.busy(), egui::Button::new("📂  Open project")).clicked() {
                                if let Some(p) = rfd::FileDialog::new().add_filter("PanoWeave project", &["pwproj"]).pick_file() {
                                    self.open_project(&p);
                                }
                            }
                            if ui.add_enabled(!self.photos.is_empty() && !self.busy(), egui::Button::new("➕  New")).clicked() {
                                self.clear_all();
                                self.set_status("Started a new panorama.", MUTED);
                            }
                        }
                    });
                });
            });
    }

    fn step_done(&self, s: Step) -> bool {
        match s {
            Step::Photos => self.photos.len() >= 2,
            Step::Points => self.matched && !self.cps.is_empty(),
            Step::Align => self.aligned && !self.needs_align,
            Step::Frame => self.aligned && self.view.crop != [0.0, 1.0, 0.0, 1.0],
            Step::Export => self.last_output.is_some(),
        }
    }

    fn step_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("steps")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(egui::Margin::symmetric(14, 8)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (k, s) in Step::ALL.iter().enumerate() {
                        let sel = self.step == *s;
                        let done = self.step_done(*s);
                        let (rect, resp) = ui.allocate_exact_size(Vec2::new(178.0, 38.0), Sense::click());
                        let p = ui.painter();
                        let fill = if sel {
                            ACCENT_DARK
                        } else if resp.hovered() {
                            Color32::from_rgb(48, 52, 61)
                        } else {
                            CARD
                        };
                        p.rect_filled(rect, 8.0, fill);
                        let cc = rect.left_center() + Vec2::new(20.0, 0.0);
                        p.circle_filled(cc, 11.0, if done { GOOD } else if sel { ACCENT } else { Color32::from_rgb(70, 75, 88) });
                        p.text(cc, Align2::CENTER_CENTER, if done { "✔".to_string() } else { format!("{}", k + 1) }, FontId::proportional(13.0), Color32::WHITE);
                        p.text(
                            rect.left_center() + Vec2::new(38.0, 0.0),
                            Align2::LEFT_CENTER,
                            s.title(),
                            FontId::proportional(14.5),
                            if sel { Color32::WHITE } else { Color32::from_rgb(205, 210, 220) },
                        );
                        if resp.clicked() {
                            self.step = *s;
                        }
                        if k < 4 {
                            ui.label(RichText::new("›").size(18.0).color(MUTED));
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let can = self.photos.len() >= 2 && !self.busy();
                        if primary_button(ui, "✨  Create panorama", can)
                            .on_hover_text("Find points, align, choose projection and crop – all in one go.")
                            .clicked()
                        {
                            self.create_auto();
                        }
                    });
                });
            });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status")
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::symmetric(14, 6)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some(job) = &self.job {
                        let (v, s) = job.progress.lock().map(|g| g.clone()).unwrap_or((0.0, String::new()));
                        ui.add(egui::Spinner::new());
                        ui.label(RichText::new(&s).color(Color32::WHITE));
                        ui.add(egui::ProgressBar::new(v).desired_width(260.0).show_percentage());
                        ui.label(RichText::new(format!("{:.1} s", job.started.elapsed().as_secs_f32())).color(MUTED));
                        let stopping = job.cancel.load(Ordering::Relaxed);
                        if ui.add_enabled(!stopping, egui::Button::new(if stopping { "Stopping…" } else { "Cancel" })).clicked() {
                            // the render job watches its own flag, everything else the global one
                            job.cancel.store(true, Ordering::Relaxed);
                            engine::CANCEL.store(true, Ordering::Relaxed);
                        }
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(60));
                    } else {
                        ui.label(RichText::new(&self.status).color(self.status_color));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.mode == Mode::Panorama {
                            ui.label(
                                RichText::new(format!(
                                    "{} photos · {} points · {} lens{}",
                                    self.photos.len(),
                                    self.cps.len(),
                                    self.lenses.len(),
                                    if self.lenses.len() == 1 { "" } else { "es" }
                                ))
                                .color(MUTED),
                            );
                        }
                    });
                });
            });
    }

}

/// Automated screenshots give up waiting after this long.
const DEMO_TIMEOUT_S: u64 = 300;

fn demo_clock() -> &'static Instant {
    static T: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T.get_or_init(Instant::now)
}

impl eframe::App for PanoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.handle_drops(&ctx);
        self.automation(&ctx);
        if self.busy() || self.preview_rx.is_some() || self.fusion_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        self.top_bar(ui);
        self.status_bar(ui);
        match self.mode {
            Mode::Panorama => {
                self.step_bar(ui);
                self.photo_list(ui);
                egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::same(14))).show(ui, |ui| match self.step {
                    Step::Photos => self.ui_photos(ui),
                    Step::Points => self.ui_points(ui),
                    Step::Align => self.ui_align(ui),
                    Step::Frame => self.ui_frame(ui),
                    Step::Export => self.ui_export(ui),
                });
            }
            Mode::Exposure => self.ui_exposure(ui),
        }
        if self.mode == Mode::Panorama && ctx.input(|i| i.key_pressed(egui::Key::F3)) {
            self.show_cp_list = !self.show_cp_list;
        }
        if self.show_overlap && self.mode == Mode::Panorama {
            self.overlap_window(&ctx);
        }
        if self.show_cp_list && self.mode == Mode::Panorama {
            self.cp_list_window(&ctx);
        }
        if self.show_graph && self.mode == Mode::Panorama {
            self.graph_window(&ctx);
        }
        if self.show_help {
            self.help_window(&ctx);
        }
        if self.patch_win.open {
            let pc = crate::patch_window::PatchCtx {
                names: self.photos.iter().map(|p| p.name.clone()).collect(),
                rgb: self.photos.iter().map(|p| p.work.clone()).collect(),
                gains: (0..self.photos.len()).map(|i| self.gains.get(i).copied().unwrap_or([1.0; 3])).collect(),
                pano: if self.aligned { Some((self.view.clone(), self.natural_width())) } else { None },
                busy: self.busy(),
                stats: self.last_fine.as_ref(),
            };
            let act = self.patch_win.show(&ctx, &pc);
            if let Some(crate::patch_window::PatchAction::Restart) = act {
                self.run_restart();
            }
        } else {
            self.patch_win.closed_tick();
        }
    }
}

