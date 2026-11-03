//! The window: a guided, step-by-step panorama studio plus an exposure-blending mode.

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
const RENDER_MEMORY_SHARE: f64 = 0.8;
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

/// Groups of photos joined by control points (each sorted, groups sorted by their first photo);
/// a photo without points is a group of its own.
fn image_groups(n: usize, active: &[usize], cps: &[ControlPoint]) -> Vec<Vec<usize>> {
    let act: Vec<ControlPoint> = cps.iter().filter(|c| active.contains(&c.a) && active.contains(&c.b)).copied().collect();
    let comp = crate::align::components(n, &act);
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut ids: Vec<usize> = active.to_vec();
    ids.sort();
    for i in ids {
        match groups.iter_mut().find(|g| comp[g[0]] == comp[i]) {
            Some(g) => g.push(i),
            None => groups.push(vec![i]),
        }
    }
    groups
}

/// The original's way of listing groups (hugin.exe sub_140034190): every group in brackets,
/// runs of consecutive numbers written as a range: "[1-4, 6], [5, 7]" (photo numbers as shown).
fn groups_text(groups: &[Vec<usize>]) -> String {
    groups
        .iter()
        .map(|g| {
            let mut parts: Vec<String> = Vec::new();
            let mut k = 0;
            while k < g.len() {
                let start = g[k];
                let mut end = start;
                while k + 1 < g.len() && g[k + 1] == end + 1 {
                    k += 1;
                    end = g[k];
                }
                parts.push(if end > start { format!("{}-{}", start + 1, end + 1) } else { (start + 1).to_string() });
                k += 1;
            }
            format!("[{}]", parts.join(", "))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Edge labels of the connection view are drawn only on lines at least this long (points).
const GRAPH_LABEL_MIN_LEN: f32 = 70.0;

/// Steps kept for undo.
const UNDO_DEPTH: usize = 30;

fn setup_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = Vec2::new(8.0, 7.0);
        s.spacing.button_padding = Vec2::new(10.0, 5.0);
        s.spacing.interact_size.y = 26.0;
        let v = &mut s.visuals;
        v.panel_fill = PANEL;
        v.window_fill = CARD;
        v.extreme_bg_color = BG;
        v.faint_bg_color = Color32::from_rgb(32, 34, 40);
        v.selection.bg_fill = ACCENT_DARK;
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.hyperlink_color = ACCENT;
        v.widgets.inactive.corner_radius = CornerRadius::same(6);
        v.widgets.hovered.corner_radius = CornerRadius::same(6);
        v.widgets.active.corner_radius = CornerRadius::same(6);
        v.widgets.noninteractive.corner_radius = CornerRadius::same(6);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(48, 52, 61);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(60, 65, 77);
        v.widgets.active.weak_bg_fill = ACCENT_DARK;
        v.window_corner_radius = CornerRadius::same(10);
        s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.5));
        s.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.5));
        s.text_styles.insert(egui::TextStyle::Heading, FontId::proportional(21.0));
        s.text_styles.insert(egui::TextStyle::Small, FontId::proportional(12.0));
    });
}

fn color_image(img: &RgbImage) -> egui::ColorImage {
    egui::ColorImage::from_rgb([img.width() as usize, img.height() as usize], img.as_raw())
}

fn primary_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).color(Color32::WHITE).strong().size(15.0))
            .fill(ACCENT_DARK)
            .stroke(Stroke::new(1.0, ACCENT))
            .min_size(Vec2::new(0.0, 34.0)),
    )
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn err_color(e: f64) -> Color32 {
    if e < 2.0 {
        GOOD
    } else if e < 5.0 {
        WARN
    } else {
        BAD
    }
}

fn fmt_ms(ms: u128) -> String {
    if ms < 1000 { format!("{ms} ms") } else { format!("{:.1} s", ms as f64 / 1000.0) }
}

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

    // ---------------- photos ----------------

    fn add_paths(&mut self, paths: Vec<PathBuf>) {
        let mut imgs = Vec::new();
        for p in paths {
            if p.extension().map(|e| e.to_string_lossy().eq_ignore_ascii_case("pwproj")).unwrap_or(false) {
                self.open_project(&p);
                return;
            }
            if imgutil::is_supported(&p) {
                imgs.push(p);
            }
        }
        if imgs.is_empty() {
            self.set_status("Those files are not photos I can read (JPEG, PNG, TIFF or RAW).", WARN);
            return;
        }
        if self.busy() {
            self.set_status("Please wait until the current task has finished.", WARN);
            return;
        }
        if self.mode == Mode::Exposure {
            self.load_brackets(imgs.into_iter().map(|p| (p.file_name().unwrap_or_default().to_string_lossy().to_string(), Source::File(p))).collect());
            return;
        }
        let items: Vec<(String, Source)> =
            imgs.into_iter().map(|p| (p.file_name().unwrap_or_default().to_string_lossy().to_string(), Source::File(p))).collect();
        self.load_sources(items);
    }

    fn load_sources(&mut self, items: Vec<(String, Source)>) {
        let n = items.len();
        self.start_job(&format!("Loading {n} photos"), move |report, _c| {
            use rayon::prelude::*;
            let done = std::sync::atomic::AtomicUsize::new(0);
            // only a few photos are decoded at the same time: decoding needs much more memory
            // than the photo keeps afterwards
            let r: Vec<_> = engine::load_pool().install(|| {
                items
                    .into_par_iter()
                    .map(|(name, src)| {
                        if engine::cancelled() {
                            return Err("cancelled".to_string());
                        }
                        let r = engine::load(src, name);
                        let k = done.fetch_add(1, Ordering::Relaxed) + 1;
                        report(k as f32 / n as f32, &format!("Loading photos ({k} of {n})"));
                        r
                    })
                    .collect()
            });
            JobMsg::Loaded(r)
        });
    }

    fn load_sample(&mut self) {
        if self.busy() {
            return;
        }
        self.start_job("Creating sample photos", |report, _| {
            report(0.1, "Creating sample photos");
            let shots = sample::panorama_set();
            let r = shots
                .into_iter()
                .map(|s| engine::load(Source::Memory(Arc::new(s.img)), s.name))
                .collect();
            JobMsg::Loaded(r)
        });
    }

    fn lens_for(&mut self, p: &LoadedPhoto) -> usize {
        let hfov = engine::exif_hfov(&p.info);
        let long = p.w.max(p.h);
        // same lens = same focal length & same photo size as an existing photo
        for (i, ph) in self.photos.iter().enumerate() {
            if ph.long() as u32 == long && ph.info.focal_mm == p.info.focal_mm && ph.info.focal_35mm == p.info.focal_35mm {
                return self.photos[i].lens;
            }
        }
        let lens = match hfov {
            Some(h) => Lens { from_exif: true, ..Lens::from_hfov_deg(h) },
            None => Lens::from_hfov_deg(50.0),
        };
        self.lenses.push(lens);
        self.lenses.len() - 1
    }

    fn on_loaded(&mut self, ctx: &egui::Context, res: Vec<Result<LoadedPhoto, String>>) {
        let mut errs = Vec::new();
        let mut added = 0;
        for r in res {
            match r {
                Ok(p) => {
                    let lens = self.lens_for(&p);
                    let thumb = ctx.load_texture(format!("thumb-{}-{}", self.photos.len(), p.name), color_image(&p.thumb), egui::TextureOptions::LINEAR);
                    self.photos.push(Photo {
                        name: p.name,
                        source: p.source,
                        w: p.w,
                        h: p.h,
                        work: p.work,
                        gray: p.gray,
                        info: p.info,
                        thumb,
                        tex: None,
                        lens,
                        rot: IDENTITY,
                        enabled: true,
                    });
                    added += 1;
                }
                Err(e) => errs.push(e),
            }
        }
        self.gains = vec![[1.0; 3]; self.photos.len()];
        if let Some(pf) = self.pending_project.take() {
            self.apply_project(pf);
            self.set_status(format!("Project opened with {} photos.", self.photos.len()), GOOD);
            return;
        }
        if added > 0 {
            self.matched = false;
            self.aligned = false;
        }
        if errs.is_empty() {
            self.set_status(format!("Added {added} photos. Press “Create panorama” – or go step by step."), GOOD);
        } else {
            self.set_status(format!("Added {added} photos, {} could not be read: {}", errs.len(), errs[0]), WARN);
        }
    }

    fn remove_photo(&mut self, i: usize) {
        self.full_tex.clear();
        self.fields.clear();
        self.photos.remove(i);
        self.gains.remove(i);
        self.cps.retain(|c| c.a != i && c.b != i);
        for c in self.cps.iter_mut() {
            if c.a > i {
                c.a -= 1;
            }
            if c.b > i {
                c.b -= 1;
            }
        }
        self.pe = PointEditor { a: 0, b: 1.min(self.photos.len().saturating_sub(1)), ..Default::default() };
        self.needs_align = true;
        self.preview_dirty = true;
    }

    fn clear_all(&mut self) {
        self.full_tex.clear();
        self.fields.clear();
        self.last_fine = None;
        self.photos.clear();
        self.lenses.clear();
        self.cps.clear();
        self.gains.clear();
        self.view = PanoView::default();
        self.aligned = false;
        self.matched = false;
        self.preview_tex = None;
        self.project_path = None;
        self.step = Step::Photos;
        self.last_match = None;
        self.last_align = None;
        self.pe = PointEditor { b: 1, ..Default::default() };
    }

    // ---------------- pipeline ----------------

    fn active_indices(&self) -> Vec<usize> {
        (0..self.photos.len()).filter(|&i| self.photos[i].enabled).collect()
    }

    fn run_match(&mut self, chain: bool) {
        if self.busy() || self.photos.len() < 2 {
            return;
        }
        self.auto_chain = chain;
        let grays: Vec<Arc<Gray>> = self.photos.iter().map(|p| p.gray.clone()).collect();
        let works: Vec<Arc<RgbImage>> = self.photos.iter().map(|p| p.work.clone()).collect();
        let manual: Vec<ControlPoint> = self.cps.iter().filter(|c| c.manual).copied().collect();
        let settings = self.match_settings;
        self.start_job("Finding matching points", move |report, _| {
            JobMsg::Matched(engine::find_points(&works, &grays, &manual, settings, report))
        });
    }

    fn run_align(&mut self, chain: bool) {
        if self.busy() || self.photos.is_empty() {
            return;
        }
        let active = self.active_indices();
        // the original refuses to optimise without 2 active images or without control points
        // (hugin.exe sub_140172d70, sub_14017ddb0)
        if active.len() < 2 {
            self.auto_chain = false;
            self.set_status("The project does not contain enough active photos. Tick “use” for at least 2 photos. Alignment canceled.", BAD);
            return;
        }
        if !self.cps.iter().any(|c| active.contains(&c.a) && active.contains(&c.b)) {
            self.auto_chain = false;
            self.set_status("There are no control points between the active photos. Find or add points first (step 2). Alignment canceled.", BAD);
            return;
        }
        self.auto_chain = chain;
        let lens_of: Vec<usize> = self.photos.iter().map(|p| p.lens).collect();
        let long: Vec<f64> = self.photos.iter().map(|p| p.long()).collect();
        let lenses = self.lenses.clone();
        let cps: Vec<ControlPoint> = self.cps.iter().filter(|c| active.contains(&c.a) && active.contains(&c.b)).copied().collect();
        let s = AlignSettings { ..self.align_settings };
        let s = AlignSettings { fine_tune_fov: s.fine_tune_fov, distortion: s.distortion, remove_outliers: s.remove_outliers, level: s.level };
        let fine_cfg = self.fine_cfg.clone();
        let par_cfg = self.parallax_cfg.clone();
        let fa: Vec<fine_align::FaPhoto> = self.photos.iter().map(|p| fine_align::FaPhoto { gray: p.gray.clone(), rgb: p.work.clone() }).collect();
        let pc_cfg = self.pair_cfg.clone();
        let msettings = self.match_settings;
        let used: Vec<bool> = (0..self.photos.len()).map(|i| active.contains(&i)).collect();
        self.start_job("Aligning photos", move |report, _| {
            let (focal, dist) = (s.fine_tune_fov, s.distortion);
            let r0 = engine::align_all(&lens_of, &lenses, &long, &cps, &s, report);
            // geometric pair check: overlapping pairs without enough points are searched again
            let (mut r, summary) = if pc_cfg.enabled {
                crate::pair_check::run_rounds(&fa, &lens_of, &lenses, &long, r0, &s, &used, &pc_cfg, &fine_cfg.matcher, msettings, report)
            } else {
                let sm = crate::pair_check::summary_only(&fa, &lens_of, &r0.lenses, &r0.rots, &r0.cps, &used, &pc_cfg);
                (r0, sm)
            };
            for l in &summary.log {
                eprintln!("pair check: {l}");
            }
            r.pairs = Some(summary);
            if fine_cfg.enabled {
                let (rots, lenses2, cps2, st) = fine_align::run(&fa, &lens_of, &long, &r.lenses, &r.rots, &r.cps, focal, dist, &fine_cfg, report);
                r.rots = rots;
                r.lenses = lenses2;
                r.cps = cps2;
                let errs: Vec<f64> = r.cps.iter().filter(|c| c.kind == CpKind::Point).map(|c| c.err).collect();
                if !errs.is_empty() {
                    r.mean_err = errs.iter().sum::<f64>() / errs.len() as f64;
                    r.max_err = errs.iter().cloned().fold(0.0, f64::max);
                }
                r.fine = Some(st);
                if par_cfg.enabled {
                    report(0.97, "Parallax mode: measuring local corrections");
                    r.fields = fine_align::parallax_fields(&fa, &lens_of, &r.lenses, &r.rots, &r.cps, &fine_cfg, &par_cfg);
                }
            }
            JobMsg::Aligned(r)
        });
    }

    /// "Restart optimisation" from the patch comparison window: optimise with the patches of
    /// the list as they are now (disabled ones left out, shifted ones with their shift).
    fn run_restart(&mut self) {
        if self.busy() {
            return;
        }
        if !self.aligned {
            self.set_status("Align the photos first – the patch list belongs to an alignment.", WARN);
            return;
        }
        let active = self.active_indices();
        if active.len() < 2 {
            self.set_status("The project does not contain enough active photos. Tick “use” for at least 2 photos. Optimization canceled.", BAD);
            return;
        }
        let lens_of: Vec<usize> = self.photos.iter().map(|p| p.lens).collect();
        let long: Vec<f64> = self.photos.iter().map(|p| p.long()).collect();
        let lenses = self.lenses.clone();
        let rots: Vec<M3> = self.photos.iter().map(|p| p.rot).collect();
        let cps: Vec<ControlPoint> = self.cps.iter().filter(|c| active.contains(&c.a) && active.contains(&c.b)).copied().collect();
        let (focal, dist) = (self.align_settings.fine_tune_fov, self.align_settings.distortion);
        let fine_cfg = self.fine_cfg.clone();
        let par_cfg = self.parallax_cfg.clone();
        let old_fields = self.fields.clone();
        let fa: Vec<fine_align::FaPhoto> = self.photos.iter().map(|p| fine_align::FaPhoto { gray: p.gray.clone(), rgb: p.work.clone() }).collect();
        self.start_job("Restarting the optimisation", move |report, _| {
            let t0 = Instant::now();
            let (rots, lenses2, cps2, st) = fine_align::reoptimize(&fa, &lens_of, &long, &lenses, &rots, &cps, focal, dist, &fine_cfg, report);
            let errs: Vec<f64> = cps2.iter().filter(|c| c.kind == CpKind::Point).map(|c| c.err).collect();
            let (mean_err, max_err) = if errs.is_empty() { (0.0, 0.0) } else { (errs.iter().sum::<f64>() / errs.len() as f64, errs.iter().cloned().fold(0.0, f64::max)) };
            let fields = if par_cfg.enabled { fine_align::parallax_fields(&fa, &lens_of, &lenses2, &rots, &cps2, &fine_cfg, &par_cfg) } else { old_fields };
            JobMsg::Aligned(AlignResult { rots, lenses: lenses2, cps: cps2, removed: 0, ms: t0.elapsed().as_millis(), mean_err, max_err, fine: Some(st), fields, pairs: None })
        });
    }

    /// "Search again" for one pair (connection view): part 1 for this pair only.
    fn run_pair_recheck(&mut self, a: usize, b: usize) {
        if self.busy() {
            return;
        }
        if !self.aligned {
            self.set_status("Align the photos first – the search uses the estimated overlap.", WARN);
            return;
        }
        let lens_of: Vec<usize> = self.photos.iter().map(|p| p.lens).collect();
        let lenses = self.lenses.clone();
        let rots: Vec<M3> = self.photos.iter().map(|p| p.rot).collect();
        let active = self.active_indices();
        let used: Vec<bool> = (0..self.photos.len()).map(|i| active.contains(&i)).collect();
        let cps: Vec<ControlPoint> = self.cps.iter().filter(|c| active.contains(&c.a) && active.contains(&c.b)).copied().collect();
        let cfg = self.pair_cfg.clone();
        let matcher = self.fine_cfg.matcher.clone();
        let ms = self.match_settings;
        let fa: Vec<fine_align::FaPhoto> = self.photos.iter().map(|p| fine_align::FaPhoto { gray: p.gray.clone(), rgb: p.work.clone() }).collect();
        self.start_job(&format!("Searching photos {} and {} again", a + 1, b + 1), move |report, _| {
            let (cps, reports, _) = crate::pair_check::check_pairs(&fa, &lens_of, &lenses, &rots, &cps, &used, Some((a, b)), &cfg, &matcher, ms, report);
            for l in crate::pair_check::report_lines(&reports) {
                eprintln!("pair check: {l}");
            }
            JobMsg::PairChecked { a, b, cps, reports }
        });
    }

    fn on_pair_checked(&mut self, a: usize, b: usize, cps: Vec<ControlPoint>, reports: Vec<crate::pair_check::PairReport>) {
        let n = cps.len();
        if let Some(sm) = &mut self.pair_summary {
            for r in &reports {
                sm.reports.retain(|x| !(x.a == r.a && x.b == r.b));
                sm.reports.push(r.clone());
            }
            sm.reports.sort_by_key(|r| (r.a, r.b));
        }
        let steps: Vec<String> = reports.iter().flat_map(|r| r.steps.iter().map(|s| format!("{}: {}", s.step, s.note))).collect();
        if reports.is_empty() {
            self.set_status(format!("Photos {} and {}: the estimated overlap is below {:.0} % or the pair already has enough points.", a + 1, b + 1, self.pair_cfg.min_overlap * 100.0), WARN);
            return;
        }
        self.cps.extend(cps);
        if n > 0 {
            self.set_status(format!("Photos {} and {}: {n} new points ({}). Aligning again…", a + 1, b + 1, steps.join("; ")), GOOD);
            self.needs_align = true;
            self.run_align(false);
        } else {
            self.set_status(format!("Photos {} and {}: no reliable points found ({}). Try “Manual”.", a + 1, b + 1, steps.join("; ")), WARN);
        }
    }

    /// Patch click mode: both rough clicks are there, refine them in a worker thread.
    fn run_click(&mut self) {
        let (Some(ca), Some(cb)) = (self.click.a, self.click.b) else { return };
        if self.busy() {
            return;
        }
        let (a, b) = (self.pe.a, self.pe.b);
        // local transform: from points of this pair, else from the alignment, else searched
        let seeds = self.pair_seeds(a, b);
        let hint: Option<([f64; 4], String)> = if let Some(j) = crate::click_match::similarity_from(&seeds) {
            Some((j, format!("from the {} points of this pair", seeds.len())))
        } else if seeds.len() == 1 {
            None
        } else if self.aligned && !self.needs_align {
            self.predict(a, b, ca).map(|(_, j)| (j, "from the alignment (lens, zoom, rotation)".to_string()))
        } else {
            None
        };
        let s0 = self.scale_prior(a, b);
        let (ga, gb) = (self.photos[a].gray.clone(), self.photos[b].gray.clone());
        let cfg = self.click_cfg.clone();
        self.start_job("Patch click: refining", move |_, _| JobMsg::ClickMatched(Box::new(crate::click_match::run(a, b, &ga, &gb, ca, cb, hint, s0, &cfg))));
    }

    fn on_click_matched(&mut self, r: crate::click_match::ClickResult) {
        for l in &r.log {
            eprintln!("patch click: {l}");
        }
        match (&r.error, &r.main) {
            (Some(e), _) => self.set_status(e.clone(), BAD),
            (None, Some(m)) => self.set_status(
                format!(
                    "Patch click: refined (score {:.3}, confidence {:.2}, zoom ×{:.2}, rotation {:.1}°) and {} more pairs found – accept or discard them below.",
                    m.score,
                    m.conf,
                    r.zoom(),
                    r.rotation_deg(),
                    r.more.len()
                ),
                GOOD,
            ),
            _ => {}
        }
        if r.error.is_none() {
            // the refined spot replaces the rough right click
            if let Some(m) = &r.main {
                self.click.b = Some(m.pb);
            }
        }
        self.click.result = Some(r);
    }

    fn push_undo(&mut self, what: &str) {
        self.undo.push(UndoEntry { what: what.to_string(), cps: self.cps.clone(), result: self.click.result.clone() });
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
    }

    fn undo_last(&mut self) {
        if let Some(u) = self.undo.pop() {
            self.cps = u.cps;
            self.click.result = u.result;
            self.pe.selected = None;
            self.needs_align = true;
            self.set_status(format!("Undone: {}.", u.what), MUTED);
        }
    }

    /// Accept the checked suggestions of the click mode as control points.
    fn accept_click(&mut self, all: bool) {
        let Some(r) = self.click.result.clone() else { return };
        if r.error.is_some() || r.a != self.pe.a || r.b != self.pe.b {
            return;
        }
        self.push_undo("adding the patch click points");
        let mut n = 0;
        for s in r.main.iter().chain(r.more.iter()) {
            if !(all || s.accept) {
                continue;
            }
            let mut c = ControlPoint::new(r.a, r.b, self.to_norm(r.a, s.pa), self.to_norm(r.b, s.pb), s.main);
            c.conf = s.conf;
            c.score = s.score;
            // confidence as weight; the clicked point counts more (like hand-drawn areas)
            c.weight = if s.main { self.click_cfg.manual_weight * (s.conf as f64).max(0.05) } else { (s.conf as f64).max(1e-3) };
            self.cps.push(c);
            n += 1;
        }
        self.click.result = None;
        self.click.a = None;
        self.click.b = None;
        self.needs_align = true;
        self.set_status(format!("{n} points added from the patch click. Align again (step 3) to use them. Ctrl+Z undoes it."), GOOD);
    }

    fn discard_click(&mut self) {
        if self.click.result.is_some() {
            self.push_undo("discarding the patch click suggestions");
        }
        self.click.result = None;
        self.click.a = None;
        self.click.b = None;
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

    fn fit_view(&mut self) {
        let rp = self.render_photos(true);
        stitch::fit_bounds(&mut self.view, &rp);
        self.preview_dirty = true;
    }

    fn auto_crop(&mut self) {
        let rp = self.render_photos(true);
        self.view.crop = stitch::auto_crop(&rp, &self.view);
        self.preview_dirty = true;
    }

    /// Pick a sensible projection from the covered field of view.
    fn choose_projection(&mut self) {
        self.view.projection = Projection::Rectilinear;
        self.view.yaw = 0.0;
        self.view.pitch = 0.0;
        self.view.roll = 0.0;
        self.view.projection = Projection::Equirectangular;
        self.fit_view();
        let (h, v) = self.view.fov_deg();
        self.view.projection = if h < 100.0 && v < 90.0 {
            Projection::Rectilinear
        } else if v > 120.0 {
            Projection::Equirectangular
        } else {
            Projection::Cylindrical
        };
        self.fit_view();
    }

    fn on_matched(&mut self, m: MatchResult) {
        self.cps = m.cps;
        self.matched = true;
        self.needs_align = true;
        self.last_match = Some((m.keypoints, m.detect_ms, m.match_ms, m.pairs.len()));
        let n = self.photos.len();
        let comps = crate::align::components(n, &self.cps);
        let groups = comps.iter().max().map(|v| v + 1).unwrap_or(0);
        let msg = format!(
            "Found {} matching points between {} photo pairs in {}.",
            self.cps.len(),
            m.pairs.len(),
            fmt_ms(m.detect_ms + m.match_ms)
        );
        if groups > 1 {
            self.set_status(format!("{msg} Some photos have no overlap with the rest – see step 3."), WARN);
        } else {
            self.set_status(msg, GOOD);
        }
        if let Some(p) = self.cps.first() {
            self.pe.a = p.a;
            self.pe.b = p.b;
        }
        if self.auto_chain {
            self.run_align(true);
        } else if self.step == Step::Photos {
            self.step = Step::Points;
        }
    }

    fn on_aligned(&mut self, a: AlignResult) {
        if let Some(p) = &a.pairs {
            self.pair_summary = Some(p.clone());
        }
        let pair_note = a.pairs.as_ref().filter(|p| p.ran).map(|p| {
            let added: usize = p.reports.iter().map(|r| r.added).sum();
            let weak = p.reports.iter().filter(|r| r.weak).count();
            format!(
                " Pair check: {} overlapping pairs, +{added} points in {} pairs ({} → {} connected pairs){}.",
                p.overlaps.len(),
                p.reports.iter().filter(|r| r.added > 0).count(),
                p.pairs_before,
                p.pairs_after,
                if weak > 0 { format!(", {weak} still weak – see “Connections”") } else { String::new() }
            )
        });
        self.last_fine = a.fine.clone();
        self.fields = a.fields.clone();
        self.fields.resize(self.photos.len(), None);
        self.align_version += 1;
        self.ov_center = None;
        let active = self.active_indices();
        for (i, p) in self.photos.iter_mut().enumerate() {
            if active.contains(&i) {
                p.rot = a.rots[i];
            }
        }
        self.lenses = a.lenses;
        // keep points of disabled photos, replace the rest
        let mut cps: Vec<ControlPoint> = self.cps.iter().filter(|c| !(active.contains(&c.a) && active.contains(&c.b))).copied().collect();
        cps.extend(a.cps);
        self.cps = cps;
        self.aligned = true;
        self.needs_align = false;
        self.last_align = Some((a.mean_err, a.max_err, a.removed, a.ms));
        let was_chain = self.auto_chain;
        if was_chain || self.view.bounds == PanoView::default().bounds {
            self.choose_projection();
        } else {
            self.fit_view();
        }
        self.update_gains();
        if was_chain {
            self.auto_crop();
            self.step = Step::Frame;
        }
        self.auto_chain = false;
        let groups = image_groups(self.photos.len(), &active, &self.cps);
        self.preview_dirty = true;
        let q = if a.mean_err < 1.5 { "excellent" } else if a.mean_err < 3.0 { "good" } else { "rough – check the points" };
        self.set_status(
            format!(
                "Aligned in {}: average error {:.2} px ({q}){}.{}",
                fmt_ms(a.ms),
                a.mean_err,
                if a.removed > 0 { format!(", {} bad points removed automatically", a.removed) } else { String::new() },
                pair_note.unwrap_or_default()
            ),
            if a.mean_err < 3.0 { GOOD } else { WARN },
        );
        if groups.len() >= 2 && active.len() > 1 {
            // like the original after the assistant (hugin.exe sub_1400557d0): warn with the list
            // of groups and open the point editor with the first photo of the first two groups
            self.set_status(
                format!(
                    "Warning {} unconnected image groups found: {}. Please create control points between unconnected images in step 2, then press “Align photos” again.",
                    groups.len(),
                    groups_text(&groups)
                ),
                WARN,
            );
            self.pe.a = groups[0][0];
            self.pe.b = groups[1][0];
            self.pe.selected = None;
            self.pe.pending = None;
            if was_chain {
                self.step = Step::Points;
            }
        }
    }

    fn create_auto(&mut self) {
        if self.photos.len() < 2 {
            self.set_status("Add at least two overlapping photos first.", WARN);
            return;
        }
        self.run_match(true);
    }

    // ---------------- export ----------------

    fn natural_width(&self) -> f64 {
        let act = self.active_indices();
        let best = act
            .iter()
            .map(|&i| self.lenses[self.photos[i].lens].f * self.photos[i].long())
            .fold(0.0, f64::max);
        let b = self.view.crop_bounds();
        // plane units are ~radians near the centre for all projections
        ((b[1] - b[0]) * best).max(64.0)
    }

    fn default_out_path(&self, stem: &str, ext: &str) -> String {
        let dir = self
            .photos
            .iter()
            .find_map(|p| p.source.path().and_then(|q| q.parent().map(|d| d.to_path_buf())))
            .or_else(|| self.brackets.iter().find_map(|p| p.source.path().and_then(|q| q.parent().map(|d| d.to_path_buf()))))
            .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Pictures")))
            .unwrap_or_else(|| PathBuf::from("."));
        dir.join(format!("{stem}.{ext}")).to_string_lossy().to_string()
    }

    /// (width, height, bytes needed, bytes free) of the panorama file at the chosen size.
    fn render_memory(&self) -> (usize, usize, u64, Option<u64>) {
        let nw = self.natural_width();
        let cb = self.view.crop_bounds();
        let asp = (cb[1] - cb[0]) / (cb[3] - cb[2]);
        let w = (nw * self.out_scale as f64 / 100.0).round().max(16.0) as usize;
        let h = ((w as f64 / asp).round() as usize).max(16);
        (w, h, stitch::estimate_bytes(w, h, self.blend), crate::sysmem::available())
    }

    fn run_render(&mut self) {
        if self.busy() || !self.aligned {
            return;
        }
        let (_, _, need, free) = self.render_memory();
        if let Some(free) = free {
            let usable = (free as f64 * RENDER_MEMORY_SHARE) as u64;
            if need > usable {
                let fit = (self.out_scale as f64 * (usable as f64 / need as f64).sqrt()).floor().max(1.0);
                self.set_status(
                    format!(
                        "This size needs about {:.1} GB of memory, but only {:.1} GB are free. Choose at most {:.0} % (or close other programs).",
                        crate::sysmem::gb(need),
                        crate::sysmem::gb(free),
                        fit
                    ),
                    BAD,
                );
                return;
            }
        }
        if self.out_path.trim().is_empty() {
            self.out_path = self.default_out_path("panorama", self.out_format.ext());
        }
        let path = PathBuf::from(self.out_path.trim());
        let path = if path.extension().is_none() { path.with_extension(self.out_format.ext()) } else { path };
        let active = self.active_indices();
        type Src = (Source, Arc<RgbImage>, u32, u32, M3, Lens, [f32; 3], Option<Arc<fine_align::DisplacementField>>, bool);
        let trusted = self.connected_photos();
        let sources: Vec<Src> = active
            .iter()
            .map(|&i| {
                let p = &self.photos[i];
                (
                    p.source.clone(),
                    p.work.clone(),
                    p.w,
                    p.h,
                    p.rot,
                    self.lenses[p.lens],
                    if self.match_brightness { self.gains[i] } else { [1.0; 3] },
                    self.fields.get(i).cloned().flatten(),
                    trusted[i],
                )
            })
            .collect();
        let view = self.view.clone();
        let out_w = (self.natural_width() * self.out_scale as f64 / 100.0).round() as usize;
        let blend = self.blend;
        let render_opts = stitch::RenderOptions { levels: self.blend_levels, cubic: self.cubic_interp, prefer_detail: self.prefer_detail };
        let fmt = self.out_format;
        let q = self.jpeg_quality;
        self.start_job("Creating the panorama", move |report, cancel| {
            let t0 = Instant::now();
            let cache: Mutex<Option<(usize, Arc<RgbImage>)>> = Mutex::new(None);
            let loader = |i: usize, pixels: bool| -> Result<RenderPhoto, String> {
                let (src, work, w, h, rot, lens, gain, field, trusted) = &sources[i];
                let img = if pixels {
                    let mut c = cache.lock().unwrap();
                    match c.as_ref() {
                        Some((k, im)) if *k == i => im.clone(),
                        _ => {
                            let im = src.load_full()?;
                            *c = Some((i, im.clone()));
                            im
                        }
                    }
                } else {
                    // seams only need the geometry plus where the photo is blown out: the small copy is enough
                    work.clone()
                };
                Ok(RenderPhoto { img, w: *w as f64, h: *h as f64, rot: *rot, lens: *lens, gain: *gain, field: field.clone(), full_long: (*w).max(*h) as f64, trusted: *trusted })
            };
            let prog = stitch::Progress { report, cancel };
            let r = stitch::render_final(sources.len(), &loader, &view, out_w, blend, &render_opts, &prog);
            let res = r.and_then(|(col, cov, w, h)| {
                report(0.98, "Saving the file");
                let rgba = stitch::to_rgba8(&col, &cov);
                save_output(&path, &rgba, w, h, fmt, q).map(|_| (path.clone(), t0.elapsed().as_millis(), w, h))
            });
            JobMsg::Rendered(res)
        });
    }

    // ---------------- exposure blending ----------------

    fn load_brackets(&mut self, items: Vec<(String, Source)>) {
        let n = items.len();
        self.start_job("Loading bracketed photos", move |report, _| {
            use rayon::prelude::*;
            let r = engine::load_pool().install(|| items
                .into_par_iter()
                .map(|(name, src)| {
                    if engine::cancelled() {
                        return Err("cancelled".to_string());
                    }
                    let full = src.load_full()?;
                    let info = match &src {
                        Source::File(p) => imgutil::read_info(p),
                        _ => PhotoInfo::default(),
                    };
                    let work = Arc::new(imgutil::downscale_rgb(&full, 1200));
                    let thumb = imgutil::downscale_rgb(&work, 200);
                    Ok((name, src, work, thumb, info))
                })
                .collect());
            report(1.0, &format!("Loaded {n}"));
            JobMsg::BracketLoaded(r)
        });
    }

    fn load_bracket_sample(&mut self) {
        if self.busy() {
            return;
        }
        self.start_job("Creating sample brackets", |_, _| {
            let shots = sample::bracket_set();
            JobMsg::BracketLoaded(
                shots
                    .into_iter()
                    .map(|s| {
                        let full = Arc::new(s.img);
                        let thumb = imgutil::downscale_rgb(&full, 200);
                        Ok((s.name, Source::Memory(full.clone()), full, thumb, PhotoInfo::default()))
                    })
                    .collect(),
            )
        });
    }

    fn request_fusion_preview(&mut self) {
        if self.fusion_rx.is_some() || self.brackets.len() < 2 {
            return;
        }
        self.fusion_dirty = false;
        let imgs: Vec<RgbImage> = self.brackets.iter().map(|b| (*b.work).clone()).collect();
        let fw = self.fusion_weights;
        let al = self.fusion_align;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let imgs = if al { fusion::align_stack(&imgs) } else { imgs };
            let out = fusion::fuse(&imgs, &fw);
            let _ = tx.send(color_image(&out));
        });
        self.fusion_rx = Some(rx);
    }

    fn run_fusion_export(&mut self) {
        if self.busy() || self.brackets.len() < 2 {
            return;
        }
        if self.fusion_out_path.trim().is_empty() {
            self.fusion_out_path = self.default_out_path("blended", "jpg");
        }
        let path = PathBuf::from(self.fusion_out_path.trim());
        let sources: Vec<Source> = self.brackets.iter().map(|b| b.source.clone()).collect();
        let fw = self.fusion_weights;
        let al = self.fusion_align;
        let q = self.jpeg_quality;
        self.start_job("Blending exposures", move |report, _| {
            let t0 = Instant::now();
            let res = (|| {
                report(0.05, "Loading full-size photos");
                let mut imgs = Vec::new();
                for s in &sources {
                    imgs.push((*s.load_full()?).clone());
                }
                if al {
                    report(0.3, "Aligning handheld shots");
                    imgs = fusion::align_stack(&imgs);
                }
                report(0.55, "Blending");
                let out = fusion::fuse(&imgs, &fw);
                report(0.95, "Saving");
                let rgba: Vec<u8> = out.pixels().flat_map(|p| [p[0], p[1], p[2], 255]).collect();
                let fmt = match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()) {
                    Some(e) if e == "png" => OutFormat::Png,
                    Some(e) if e == "tif" || e == "tiff" => OutFormat::Tiff,
                    _ => OutFormat::Jpeg,
                };
                save_output(&path, &rgba, out.width() as usize, out.height() as usize, fmt, q)?;
                Ok((path.clone(), t0.elapsed().as_millis()))
            })();
            JobMsg::FusedSaved(res)
        });
    }

    // ---------------- project files ----------------

    fn save_project(&mut self, path: PathBuf) {
        let mut photos = Vec::new();
        let photo_dir = path.with_extension("").to_string_lossy().to_string() + "_photos";
        for p in &self.photos {
            let pp = match &p.source {
                Source::File(f) => f.clone(),
                Source::Memory(img) => {
                    let _ = std::fs::create_dir_all(&photo_dir);
                    let f = PathBuf::from(&photo_dir).join(Path::new(&p.name).with_extension("png"));
                    if let Err(e) = img.save(&f) {
                        self.set_status(format!("Could not save a sample photo: {e}"), BAD);
                        return;
                    }
                    f
                }
            };
            photos.push(ProjectPhoto { path: pp, lens: p.lens, rot: p.rot, enabled: p.enabled });
        }
        let pf = ProjectFile {
            version: 1,
            photos,
            lenses: self.lenses.clone(),
            cps: self.cps.clone(),
            view: self.view.clone(),
            aligned: self.aligned,
        };
        match project::save(&path, &pf) {
            Ok(()) => {
                self.set_status(format!("Project saved: {}", path.display()), GOOD);
                self.project_path = Some(path);
            }
            Err(e) => self.set_status(format!("Could not save the project: {e}"), BAD),
        }
    }

    fn open_project(&mut self, path: &Path) {
        if self.busy() {
            return;
        }
        match project::load(path) {
            Ok(pf) => {
                self.clear_all();
                self.mode = Mode::Panorama;
                let items: Vec<(String, Source)> = pf
                    .photos
                    .iter()
                    .map(|p| (p.path.file_name().unwrap_or_default().to_string_lossy().to_string(), Source::File(p.path.clone())))
                    .collect();
                self.project_path = Some(path.to_path_buf());
                self.pending_project = Some(pf);
                self.load_sources(items);
            }
            Err(e) => self.set_status(format!("Could not open the project: {e}"), BAD),
        }
    }

    fn apply_project(&mut self, pf: ProjectFile) {
        if pf.photos.len() != self.photos.len() {
            self.set_status("Some photos of the project could not be found.", WARN);
            return;
        }
        self.lenses = pf.lenses;
        for (p, pp) in self.photos.iter_mut().zip(&pf.photos) {
            p.lens = pp.lens;
            p.rot = pp.rot;
            p.enabled = pp.enabled;
        }
        self.cps = pf.cps;
        self.view = pf.view;
        self.aligned = pf.aligned;
        self.matched = !self.cps.is_empty();
        if self.aligned {
            self.update_gains();
            self.step = Step::Frame;
            self.preview_dirty = true;
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
                self.step = Step::Export;
                self.out_scale = 50.0;
                self.out_path = std::env::current_dir().unwrap_or_default().join("shots").join("demo_render.jpg").to_string_lossy().to_string();
                self.run_render();
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
            let ready = if self.demo.is_some() { settled && self.frames > 120 && (self.demo.as_deref() != Some("render") || self.last_output.is_some()) && (self.aligned || self.mode == Mode::Exposure || self.demo.as_deref() == Some("photos")) } else { self.frames >= 120 };
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

    fn photo_list(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("photos")
            .resizable(true)
            .default_size(250.0)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(egui::Margin::same(10)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Your photos").strong().size(16.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(!self.busy(), egui::Button::new("➕ Add")).clicked() {
                            self.pick_photos();
                        }
                    });
                });
                ui.add_space(4.0);
                let comps = crate::align::components(self.photos.len(), &self.cps);
                let main_group = most_common(&comps);
                let mut per_photo = vec![0usize; self.photos.len()];
                for c in &self.cps {
                    if c.a < per_photo.len() {
                        per_photo[c.a] += 1;
                    }
                    if c.b < per_photo.len() && c.b != c.a {
                        per_photo[c.b] += 1;
                    }
                }
                let mut remove = None;
                let mut toggled = false;
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    if self.photos.is_empty() {
                        ui.add_space(30.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("No photos yet").color(MUTED));
                            ui.label(RichText::new("Drop photos onto the window\nor press ➕ Add").color(MUTED).small());
                        });
                    }
                    for i in 0..self.photos.len() {
                        let p = &mut self.photos[i];
                        let connected = !self.matched || comps.get(i) == Some(&main_group);
                        let pts = per_photo[i];
                        let highlight = self.step == Step::Points && (self.pe.a == i || self.pe.b == i);
                        egui::Frame::new()
                            .fill(if highlight { Color32::from_rgb(38, 50, 68) } else { CARD })
                            .corner_radius(CornerRadius::same(8))
                            .inner_margin(egui::Margin::same(6))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    let tw = 72.0;
                                    let ts = p.thumb.size_vec2();
                                    let th = tw * ts.y / ts.x;
                                    let img = egui::Image::new(egui::load::SizedTexture::new(p.thumb.id(), Vec2::new(tw, th.min(72.0))))
                                        .corner_radius(4.0)
                                        .tint(if p.enabled { Color32::WHITE } else { Color32::from_gray(90) });
                                    ui.add(img);
                                    ui.vertical(|ui| {
                                        ui.label(RichText::new(format!("{}. {}", i + 1, p.name)).strong().color(Color32::WHITE));
                                        ui.label(RichText::new(format!("{} × {}", p.w, p.h)).small().color(MUTED));
                                        if self.matched {
                                            let (t, c) = if !p.enabled {
                                                ("left out".to_string(), MUTED)
                                            } else if connected {
                                                (format!("{pts} points"), GOOD)
                                            } else {
                                                ("no overlap found".to_string(), BAD)
                                            };
                                            ui.label(RichText::new(t).small().color(c));
                                        }
                                        ui.horizontal(|ui| {
                                            if ui.checkbox(&mut p.enabled, RichText::new("use").small()).changed() {
                                                toggled = true;
                                            }
                                            if ui.small_button("🗑").on_hover_text("Remove this photo").clicked() {
                                                remove = Some(i);
                                            }
                                        });
                                    });
                                });
                            });
                        ui.add_space(2.0);
                    }
                });
                if toggled {
                    self.needs_align = true;
                    self.preview_dirty = true;
                }
                if let Some(i) = remove {
                    if !self.busy() {
                        self.remove_photo(i);
                    }
                }
            });
    }

    fn pick_photos(&mut self) {
        let files = rfd::FileDialog::new()
            .add_filter("Photos", &["jpg", "jpeg", "png", "tif", "tiff", "dng", "cr2", "cr3", "nef", "arw", "orf", "rw2", "raf", "pef", "srw"])
            .add_filter("All files", &["*"])
            .pick_files();
        if let Some(f) = files {
            self.add_paths(f);
        }
    }

    // ----- step 1 -----
    fn ui_photos(&mut self, ui: &mut egui::Ui) {
        if self.photos.is_empty() {
            let avail = ui.available_size();
            ui.add_space((avail.y * 0.12).max(10.0));
            ui.vertical_centered(|ui| {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(avail.x.min(720.0), 300.0), Sense::click());
                let p = ui.painter();
                let hovered = resp.hovered() || ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
                p.rect_filled(rect, 18.0, if hovered { Color32::from_rgb(34, 44, 60) } else { Color32::from_rgb(30, 33, 40) });
                dashed_rect(p, rect.shrink(6.0), if hovered { ACCENT } else { Color32::from_rgb(80, 86, 100) });
                // illustration: three overlapping frames
                let c = rect.center() - Vec2::new(0.0, 60.0);
                for (k, dx) in [-46.0f32, 0.0, 46.0].iter().enumerate() {
                    let r = Rect::from_center_size(c + Vec2::new(*dx, (k as f32 - 1.0).abs() * 4.0), Vec2::new(70.0, 50.0));
                    p.rect_filled(r, 6.0, [Color32::from_rgb(52, 92, 140), Color32::from_rgb(64, 120, 180), Color32::from_rgb(52, 92, 140)][k]);
                    p.rect_stroke(r, 6.0, Stroke::new(2.0, Color32::from_rgb(170, 205, 255)), StrokeKind::Inside);
                }
                p.text(rect.center() + Vec2::new(0.0, 10.0), Align2::CENTER_CENTER, "Drop your photos here", FontId::proportional(24.0), Color32::WHITE);
                p.text(
                    rect.center() + Vec2::new(0.0, 42.0),
                    Align2::CENTER_CENTER,
                    "or click to choose them  ·  JPEG, PNG, TIFF and camera RAW",
                    FontId::proportional(15.0),
                    MUTED,
                );
                p.text(
                    rect.center() + Vec2::new(0.0, 74.0),
                    Align2::CENTER_CENTER,
                    "Tip: let neighbouring photos overlap by about a third.",
                    FontId::proportional(13.0),
                    Color32::from_rgb(120, 126, 140),
                );
                if resp.clicked() && !self.busy() {
                    self.pick_photos();
                }
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    let w = 470.0;
                    ui.add_space((ui.available_width() - w).max(0.0) / 2.0);
                    if ui.add_enabled(!self.busy(), egui::Button::new(RichText::new("🖼  Try it with sample photos").size(15.0)).min_size(Vec2::new(230.0, 34.0))).clicked() {
                        self.load_sample();
                    }
                    if ui.add_enabled(!self.busy(), egui::Button::new(RichText::new("📂  Open a saved project").size(15.0)).min_size(Vec2::new(230.0, 34.0))).clicked() {
                        if let Some(p) = rfd::FileDialog::new().add_filter("PanoWeave project", &["pwproj"]).pick_file() {
                            self.open_project(&p);
                        }
                    }
                });
                ui.add_space(26.0);
                ui.horizontal(|ui| {
                    let w = 3.0 * 230.0 + 2.0 * 12.0;
                    ui.add_space((ui.available_width() - w).max(0.0) / 2.0);
                    for (t, d) in [
                        ("1 · Add photos", "Shoot from one spot, turning the camera."),
                        ("2 · Create panorama", "One click finds points, aligns and frames."),
                        ("3 · Fine-tune & save", "Adjust the frame, fix points if needed."),
                    ] {
                        egui::Frame::new().fill(CARD).corner_radius(CornerRadius::same(10)).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                            ui.set_width(206.0);
                            ui.vertical(|ui| {
                                ui.label(RichText::new(t).strong().color(Color32::WHITE));
                                ui.label(RichText::new(d).small().color(MUTED));
                            });
                        });
                    }
                });
            });
            return;
        }
        ui.horizontal(|ui| {
            ui.heading(format!("{} photos", self.photos.len()));
            ui.add_space(10.0);
            if ui.add_enabled(!self.busy(), egui::Button::new("➕ Add more")).clicked() {
                self.pick_photos();
            }
            if ui.add_enabled(!self.busy() && self.photos.len() >= 2, egui::Button::new("🔍  Find points only")).clicked() {
                self.run_match(false);
            }
        });
        ui.label(RichText::new("Everything is ready. Press ✨ Create panorama (top right) for the automatic way, or go through the steps yourself.").color(MUTED));
        ui.add_space(6.0);
        // lenses
        card(ui, |ui| {
            ui.label(RichText::new("Lens").strong());
            ui.label(RichText::new("The field of view is read from the photo information when possible; otherwise it is measured automatically during alignment.").small().color(MUTED));
            let mut changed = false;
            for (li, lens) in self.lenses.iter_mut().enumerate() {
                let users: Vec<String> = self.photos.iter().enumerate().filter(|(_, p)| p.lens == li).map(|(i, _)| (i + 1).to_string()).collect();
                if users.is_empty() {
                    continue;
                }
                ui.horizontal(|ui| {
                    ui.label(format!("Lens {} (photos {}):", li + 1, users.join(", ")));
                    let mut deg = lens.hfov_deg();
                    if ui.add(egui::DragValue::new(&mut deg).range(1.0..=170.0).speed(0.2).suffix("° wide").max_decimals(1)).changed() {
                        *lens = Lens { f: Lens::from_hfov_deg(deg).f, k1: lens.k1, from_exif: true };
                        changed = true;
                    }
                    ui.label(
                        RichText::new(if lens.from_exif { "fixed / from photo info" } else { "will be measured" }).small().color(MUTED),
                    );
                    if lens.from_exif && ui.small_button("measure instead").clicked() {
                        lens.from_exif = false;
                        changed = true;
                    }
                });
            }
            if changed {
                self.needs_align = true;
            }
        });
        ui.add_space(8.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (i, p) in self.photos.iter().enumerate() {
                    egui::Frame::new().fill(CARD).corner_radius(CornerRadius::same(8)).inner_margin(egui::Margin::same(6)).show(ui, |ui| {
                        ui.vertical(|ui| {
                            let s = p.thumb.size_vec2();
                            let w = 200.0;
                            ui.add(egui::Image::new(egui::load::SizedTexture::new(p.thumb.id(), Vec2::new(w, w * s.y / s.x))).corner_radius(4.0));
                            ui.label(RichText::new(format!("{}. {}", i + 1, p.name)).small());
                            let mut info = format!("{} × {}", p.w, p.h);
                            if let Some(f) = p.info.focal_mm {
                                info += &format!(" · {f:.0} mm");
                            }
                            if let Some(t) = p.info.exposure_time {
                                info += &if t < 1.0 { format!(" · 1/{:.0} s", 1.0 / t) } else { format!(" · {t:.1} s") };
                            }
                            ui.label(RichText::new(info).small().color(MUTED));
                        });
                    });
                }
            });
        });
    }

    // ----- step 2 -----
    fn ensure_tex(&mut self, ctx: &egui::Context, i: usize) {
        if self.photos[i].tex.is_none() {
            let img = color_image(&self.photos[i].work);
            self.photos[i].tex = Some(ctx.load_texture(format!("work-{i}-{}", self.photos[i].name), img, egui::TextureOptions::LINEAR));
        }
    }

    fn ui_points(&mut self, ui: &mut egui::Ui) {
        let n = self.photos.len();
        if n < 2 {
            ui.label("Add at least two photos first.");
            return;
        }
        self.pe.a = self.pe.a.min(n - 1);
        self.pe.b = self.pe.b.min(n - 1);
        if self.pe.a == self.pe.b {
            self.pe.b = if self.pe.a + 1 < n { self.pe.a + 1 } else { 0 };
        }
        let ctx = ui.ctx().clone();
        // --- toolbar: all photos ---
        ui.horizontal_wrapped(|ui| {
            if primary_button(ui, if self.matched { "🔍  Find points in all photos again" } else { "🔍  Find points in all photos" }, !self.busy()).clicked() {
                self.run_match(false);
            }
            ui.label("Detail:");
            egui::ComboBox::from_id_salt("density")
                .selected_text(match self.match_settings.points_per_photo {
                    0..=2000 => "Fast",
                    2001..=4000 => "Balanced",
                    _ => "Thorough",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.match_settings.points_per_photo, 1500, "Fast");
                    ui.selectable_value(&mut self.match_settings.points_per_photo, 3500, "Balanced");
                    ui.selectable_value(&mut self.match_settings.points_per_photo, 8000, "Thorough (difficult scenes)");
                });
            ui.checkbox(&mut self.match_settings.ignore_sky, "Ignore sky & clouds")
                .on_hover_text("Clouds move between shots, so points on them pull the photos apart. Turn off only if the sky is the only overlap.");
            ui.label("Points per pair:");
            ui.add(egui::DragValue::new(&mut self.match_settings.points_per_pair).range(8..=300));
            if let Some((k, d, m, pairs)) = self.last_match {
                ui.label(RichText::new(format!("last run: {k} features, {pairs} pairs, {}", fmt_ms(d + m))).small().color(MUTED));
            }
        });
        let (a, b) = (self.pe.a, self.pe.b);
        let (cnt_ab, _, _) = self.pair_stats(a, b);
        // --- toolbar: this pair ---
        ui.horizontal_wrapped(|ui| {
            let label = if cnt_ab == 0 { "🔍  Find points for these two photos" } else { "🔍  Find more points for these two photos" };
            if primary_button(ui, label, !self.busy())
                .on_hover_text("Searches only this pair. Without points it works fully automatically (also for photos with different zoom). With your points it starts from them and fills the overlap.")
                .clicked()
            {
                self.run_pair_points();
            }
            let add_txt = if self.pe.adding { "✔ Done adding" } else { "➕ Add point" };
            if ui
                .add(egui::Button::selectable(self.pe.adding, add_txt))
                .on_hover_text("Click a spot in one photo, then the same spot in the other. Points snap to the exact spot automatically.")
                .clicked()
            {
                self.pe.adding = !self.pe.adding;
                self.pe.pending = None;
            }
            ui.checkbox(&mut self.auto_grow, "then find more automatically");
            if ui
                .add(egui::Button::selectable(self.click.on, "🎯 Patch click"))
                .on_hover_text("Click roughly the same spot left and right: the spot is refined by patch matching (also with different zoom) and more matching patches around it are proposed.")
                .clicked()
            {
                self.click.on = !self.click.on;
                self.pe.adding = false;
                self.pe.pending = None;
            }
            if ui.add_enabled(!self.undo.is_empty(), egui::Button::new("↶ Undo")).on_hover_text(self.undo.last().map(|u| format!("Undo {} (Ctrl+Z)", u.what)).unwrap_or_default()).clicked() {
                self.undo_last();
            }
            if ui.button("🕸 Connections").on_hover_text("Which photos are joined by points, which overlapping pairs have too few").clicked() {
                self.show_graph = !self.show_graph;
            }
            ui.separator();
            ui.label("Match window").on_hover_text("Size of the patch around a point that is compared between the two photos (red frame).");
            if ui.add(egui::DragValue::new(&mut self.tpl_size).range(7..=81).speed(0.5).suffix(" px")).changed() {
                self.tpl_size |= 1;
            }
            ui.label("Search area").on_hover_text("How far around a clicked spot the matching spot is searched (dashed frame), in % of the photo width, at most 500 px.");
            ui.add(egui::DragValue::new(&mut self.search_pct).range(1.0..=50.0).speed(0.2).suffix(" %"));
            if ui.add_enabled(self.pe.selected.is_some(), egui::Button::new("🗑 Delete point")).clicked() {
                self.delete_selected();
            }
            if ui.button("Delete all in this pair").clicked() {
                self.cps.retain(|c| !((c.a == a && c.b == b) || (c.a == b && c.b == a)));
                self.pe.selected = None;
                self.needs_align = true;
            }
            if ui.button("🔍 Check overlap").on_hover_text("Lay the two photos over each other: blend, difference, checkerboard, shift arrows").clicked() {
                self.show_overlap = !self.show_overlap;
            }
            if ui.button("📋 All points (F3)").on_hover_text("List of all control points, sortable by accuracy").clicked() {
                self.show_cp_list = !self.show_cp_list;
            }
            if ui.button("⇄ Swap sides").clicked() {
                self.pe.a = b;
                self.pe.b = a;
                self.pe.views.swap(0, 1);
                self.pe.pending = None;
            }
            ui.separator();
            {
                let on = self.aligned;
                ui.add_enabled(on, egui::Label::new("Remove points worse than"));
                ui.add_enabled(on, egui::DragValue::new(&mut self.err_threshold).range(0.5..=100.0).speed(0.1).suffix(" px"));
                if ui.add_enabled(on, egui::Button::new("Clean up")).clicked() {
                    let before = self.cps.len();
                    let t = self.err_threshold;
                    self.cps.retain(|c| c.err <= t);
                    self.needs_align = true;
                    self.pe.selected = None;
                    let removed = before - self.cps.len();
                    self.set_status(format!("Removed {removed} points. Re-align in step 3 to update."), MUTED);
                }
            }
        });
        let hint = if self.pe.adding {
            match self.pe.pending {
                Some((Pane::A, _)) => "Now click the same spot in the RIGHT photo – it snaps to the exact spot.",
                Some((Pane::B, _)) => "Now click the same spot in the LEFT photo – it snaps to the exact spot.",
                None => "Click a distinctive spot (a corner, a sharp detail) in either photo. Esc cancels.",
            }
        } else {
            "Scroll to zoom, drag the photo to move it, double-click to fit. Click a point to select it: red = match window, dashed white = search area. Drag it to move it (edges are shown, it snaps to the exact spot), Delete removes it."
        };
        if self.click.on {
            self.click_panel(ui);
        } else {
            ui.label(RichText::new(hint).small().color(if self.pe.adding { ACCENT } else { MUTED }));
        }

        self.ensure_tex(&ctx, a);
        self.ensure_tex(&ctx, b);
        self.request_full(a);
        self.request_full(b);
        // drop big textures of photos that are no longer shown
        self.full_tex.retain(|k, _| *k == a || *k == b);

        let avail = ui.available_size();
        let pane_w = ((avail.x - 10.0) / 2.0).max(100.0);
        let header_h = 30.0;
        let pane_h = (avail.y - header_h - 8.0).max(100.0);
        let mut events: Vec<PaneEvent> = Vec::new();
        let mut geo = [PaneGeo::default(); 2];
        let pair_idx: Vec<usize> = (0..self.cps.len()).filter(|&k| (self.cps[k].a == a && self.cps[k].b == b) || (self.cps[k].a == b && self.cps[k].b == a)).collect();
        // positions of the pair's points per pane (work px)
        let pts: Vec<(usize, P2, P2)> = pair_idx
            .iter()
            .map(|&k| {
                let c = &self.cps[k];
                let (pa, pb) = if c.a == a { (c.pa, c.pb) } else { (c.pb, c.pa) };
                (k, self.to_px(a, pa), self.to_px(b, pb))
            })
            .collect();
        let frame = self.selection_frame();
        let mut new_a = a;
        let mut new_b = b;
        ui.horizontal_top(|ui| {
            for pane in [Pane::A, Pane::B] {
                let idx = if pane == Pane::A { a } else { b };
                ui.allocate_ui_with_layout(Vec2::new(pane_w, header_h + pane_h + 8.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_max_width(pane_w);
                    // header with the photo choice
                    ui.horizontal(|ui| {
                        if pane == Pane::A {
                            let total = |i: usize| self.cps.iter().filter(|c| c.a == i || c.b == i).count();
                            egui::ComboBox::from_id_salt("left_photo")
                                .width((pane_w - 190.0).max(120.0))
                                .truncate()
                                .selected_text(format!("{}. {}  —  {} points", a + 1, self.photos[a].name, total(a)))
                                .show_ui(ui, |ui| {
                                    for i in 0..n {
                                        ui.selectable_value(&mut new_a, i, format!("{}. {}  —  {} points", i + 1, self.photos[i].name, total(i)));
                                    }
                                });
                        } else {
                            let label = |j: usize| -> RichText {
                                let (c, mean, mx) = self.pair_stats(a, j);
                                let t = if c == 0 {
                                    format!("{}. {}  —  no points", j + 1, self.photos[j].name)
                                } else if self.aligned {
                                    format!("{}. {}  —  {} points · ⌀ {:.1} px · max {:.1} px", j + 1, self.photos[j].name, c, mean, mx)
                                } else {
                                    format!("{}. {}  —  {} points", j + 1, self.photos[j].name, c)
                                };
                                let col = if c == 0 { MUTED } else if self.aligned { err_color(mx) } else { Color32::WHITE };
                                RichText::new(t).color(col)
                            };
                            egui::ComboBox::from_id_salt("right_photo").width((pane_w - 190.0).max(120.0)).truncate().selected_text(label(b)).show_ui(ui, |ui| {
                                ui.set_min_width(380.0);
                                for j in 0..n {
                                    if j != a {
                                        ui.selectable_value(&mut new_b, j, label(j));
                                    }
                                }
                            });
                        }
                        let v = self.pe.views[pane.i()];
                        if ui.small_button("⤢ Fit").clicked() {
                            events.push(PaneEvent::Fit(pane));
                        }
                        ui.label(RichText::new(format!("{:.0} %", v.zoom.max(0.01) * 100.0)).small().color(MUTED));
                    });
                    let (resp, painter) = ui.allocate_painter(Vec2::new(pane_w, pane_h), Sense::click_and_drag());
                    let rect = resp.rect;
                    let painter = painter.with_clip_rect(rect);
                    let (w, h) = self.work_dims(idx);
                    let key = idx + 1;
                    let fit = (rect.width() / w as f32).min(rect.height() / h as f32);
                    {
                        let v = &mut self.pe.views[pane.i()];
                        if v.key != key {
                            *v = PaneView { zoom: 1.0, center: Vec2::new(w as f32 / 2.0, h as f32 / 2.0), key };
                        }
                    }
                    let v = self.pe.views[pane.i()];
                    let scale = fit * v.zoom;
                    let origin = rect.center() - v.center * scale;
                    geo[pane.i()] = PaneGeo { rect, origin, scale, fit, w: w as f32, h: h as f32 };
                    painter.rect_filled(rect, 8.0, BG);
                    let img_rect = Rect::from_min_size(origin, Vec2::new(w as f32 * scale, h as f32 * scale));
                    let tex_id = self.full_tex.get(&idx).map(|t| t.id()).or_else(|| self.photos[idx].tex.as_ref().map(|t| t.id()));
                    if let Some(t) = tex_id {
                        painter.image(t, img_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    }
                    // edges around the point while it is being moved
                    if let (Some((dk, _)), Some(fr)) = (self.pe.dragging, frame) {
                        if let Some((_, pa, pb)) = pts.iter().find(|t| t.0 == dk) {
                            let p = if pane == Pane::A { *pa } else { *pb };
                            let (sf, sm) = guided::step_sizes(&fr.j);
                            let r = (self.tpl_size / 2) as f64;
                            let half = if pane == fr.fixed {
                                2.0 * r * sf
                            } else {
                                self.local_radius(idx) + r * sm
                            }
                            .clamp(16.0, 220.0)
                                + 6.0;
                            let (img, x0, y0, ew, eh) = edge_image(&self.photos[idx].gray, p, half);
                            let tid = match &mut self.edge_tex[pane.i()] {
                                Some(t) => {
                                    t.set(img, egui::TextureOptions::LINEAR);
                                    t.id()
                                }
                                None => {
                                    let t = ui.ctx().load_texture(format!("edges-{}", pane.i()), img, egui::TextureOptions::LINEAR);
                                    let id = t.id();
                                    self.edge_tex[pane.i()] = Some(t);
                                    id
                                }
                            };
                            let rmin = origin + Vec2::new(x0 as f32 * scale, y0 as f32 * scale);
                            painter.rect_filled(Rect::from_min_size(rmin, Vec2::new(ew as f32 * scale, eh as f32 * scale)), 0.0, Color32::from_black_alpha(70));
                            painter.image(tid, Rect::from_min_size(rmin, Vec2::new(ew as f32 * scale, eh as f32 * scale)), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                        }
                    }
                    // points
                    let to_screen = |p: P2| origin + Vec2::new((p.0 as f32 + 0.5) * scale, (p.1 as f32 + 0.5) * scale);
                    for (num, (k, pa, pb)) in pts.iter().enumerate() {
                        let p = if pane == Pane::A { *pa } else { *pb };
                        let s = to_screen(p);
                        if !rect.expand(10.0).contains(s) {
                            continue;
                        }
                        let c = &self.cps[*k];
                        let col = if self.aligned { err_color(c.err) } else { point_color(num) };
                        let sel = self.pe.selected == Some(*k);
                        if sel {
                            let r = 11.0;
                            painter.circle_stroke(s, r, Stroke::new(3.0, Color32::BLACK));
                            painter.circle_stroke(s, r, Stroke::new(1.6, Color32::WHITE));
                            for d in [Vec2::new(1.0, 0.0), Vec2::new(0.0, 1.0)] {
                                painter.line_segment([s - d * (r + 6.0), s - d * 3.0], Stroke::new(1.4, Color32::WHITE));
                                painter.line_segment([s + d * 3.0, s + d * (r + 6.0)], Stroke::new(1.4, Color32::WHITE));
                            }
                            let info = if self.aligned { format!("#{} · {:.2} px", num + 1, c.err) } else { format!("#{}", num + 1) };
                            painter.text(s + Vec2::new(14.0, -14.0), Align2::LEFT_BOTTOM, info, FontId::proportional(12.5), Color32::WHITE);
                        } else {
                            painter.circle_stroke(s, 6.0, Stroke::new(1.5, Color32::BLACK));
                            if c.kind == CpKind::Point {
                                painter.circle_stroke(s, 5.0, Stroke::new(if c.manual { 2.4 } else { 1.8 }, col));
                            } else {
                                // control area: small square
                                painter.rect_stroke(Rect::from_center_size(s, Vec2::splat(8.0)), 0.0, Stroke::new(1.4, col), StrokeKind::Middle);
                            }
                            if v.zoom > 2.5 {
                                painter.text(s + Vec2::new(7.0, -7.0), Align2::LEFT_BOTTOM, format!("{}", num + 1), FontId::proportional(11.0), col);
                            }
                        }
                    }
                    // matching window and search area of the selected point
                    if let (Some(fr), Some(k)) = (frame, self.pe.selected) {
                        if let Some((_, pa, pb)) = pts.iter().find(|t| t.0 == k) {
                            let p = if pane == Pane::A { *pa } else { *pb };
                            let (sf, sm) = guided::step_sizes(&fr.j);
                            // half size = TemplateSize / 2 (integer), as the original draws it (sub_14006cfe0)
                            let r = (self.tpl_size / 2) as f64;
                            let yellow = TEMPLATE_FRAME_COLOR;
                            let blue = SEARCH_FRAME_COLOR;
                            let quad = |m: &dyn Fn(f64, f64) -> P2| -> Vec<Pos2> {
                                [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0), (-1.0, -1.0)].iter().map(|&(x, y)| to_screen(m(x, y))).collect()
                            };
                            let window = if pane == fr.fixed {
                                let h = r * sf;
                                quad(&|x, y| (p.0 + x * h, p.1 + y * h))
                            } else {
                                // the same patch as it appears in this photo (zoomed / rotated)
                                let h = r * sf;
                                let j = fr.j;
                                quad(&|x, y| (p.0 + (j[0] * x + j[1] * y) * h, p.1 + (j[2] * x + j[3] * y) * h))
                            };
                            painter.line(window.clone(), Stroke::new(3.2, Color32::from_black_alpha(150)));
                            painter.line(window.clone(), Stroke::new(1.6, yellow));
                            let label_at = window.iter().fold(Pos2::new(f32::MAX, f32::MIN), |m, q| Pos2::new(m.x.min(q.x), m.y.max(q.y)));
                            if pane != fr.fixed {
                                let sr = if self.pe.dragging.is_some() {
                                    self.local_radius(idx) + r * sm
                                } else {
                                    // original: half = SearchAreaPercent * image width / 200 (sub_140072d30)
                                    (self.search_pct * self.photos[idx].w as f64 / 200.0).min(guided::SEARCH_AREA_MAX_PX / 2.0) * self.work_per_full(idx)
                                };
                                let outer = quad(&|x, y| (p.0 + x * sr, p.1 + y * sr));
                                painter.extend(egui::Shape::dashed_line(&outer, Stroke::new(3.0, Color32::from_black_alpha(130)), 7.0, 5.0));
                                painter.extend(egui::Shape::dashed_line(&outer, Stroke::new(1.4, blue), 7.0, 5.0));
                                if (outer[2].x - outer[0].x) > 70.0 {
                                    painter.text(outer[0] + Vec2::new(3.0, -2.0), Align2::LEFT_BOTTOM, "search area", FontId::proportional(11.0), blue);
                                }
                            }
                            if (window[2].x - window[0].x).abs() > 50.0 || (window[2].y - window[0].y).abs() > 50.0 {
                                painter.text(label_at + Vec2::new(0.0, 3.0), Align2::LEFT_TOP, "match window", FontId::proportional(11.0), yellow);
                            }
                        }
                    }
                    if self.click.on {
                        // patch click: rough clicks (orange), refined spot and suggestions
                        let orange = Color32::from_rgb(255, 150, 40);
                        let click = if pane == Pane::A { self.click.a } else { self.click.b };
                        if let Some(c) = click {
                            let s = to_screen(c);
                            painter.circle_stroke(s, 10.0, Stroke::new(2.5, orange));
                            painter.line_segment([s - Vec2::new(14.0, 0.0), s + Vec2::new(14.0, 0.0)], Stroke::new(1.0, orange));
                            painter.line_segment([s - Vec2::new(0.0, 14.0), s + Vec2::new(0.0, 14.0)], Stroke::new(1.0, orange));
                        }
                        if let Some(r) = &self.click.result {
                            if r.a == a && r.b == b {
                                for sg in r.main.iter().chain(r.more.iter()) {
                                    let q = if pane == Pane::A { sg.pa } else { sg.pb };
                                    let col = if sg.accept { GOOD } else { Color32::from_gray(150) };
                                    painter.circle_stroke(to_screen(q), if sg.main { 7.0 } else { 4.5 }, Stroke::new(if sg.main { 2.5 } else { 1.5 }, col));
                                }
                            }
                        }
                    }
                    if let Some((pp, pn)) = self.pe.pending {
                        if pp == pane {
                            let s = to_screen(self.to_px(idx, pn));
                            painter.circle_stroke(s, 9.0, Stroke::new(2.5, ACCENT));
                            painter.circle_filled(s, 2.0, ACCENT);
                        }
                    }
                    painter.text(rect.left_top() + Vec2::new(10.0, 8.0), Align2::LEFT_TOP, format!("Photo {}", idx + 1), FontId::proportional(14.0), Color32::WHITE);
                    // loupe while placing points
                    if self.pe.adding && v.zoom < 3.0 {
                        if let (Some(hp), Some(t)) = (resp.hover_pos(), tex_id) {
                            if img_rect.contains(hp) {
                                let lr = Rect::from_min_size(rect.right_bottom() - Vec2::new(170.0, 170.0), Vec2::splat(160.0));
                                let u = (hp - img_rect.min) / img_rect.size();
                                // magnifier shows MAGNIFIER_WIDTH pixels of the full-size photo
                                let full_w = self.photos[idx].w as f32;
                                let full_h = self.photos[idx].h as f32;
                                let hx = 0.5 * MAGNIFIER_WIDTH / full_w;
                                let hy = 0.5 * MAGNIFIER_WIDTH / full_h;
                                painter.rect_filled(lr.expand(3.0), 6.0, Color32::BLACK);
                                painter.image(t, lr, Rect::from_min_max(Pos2::new(u.x - hx, u.y - hy), Pos2::new(u.x + hx, u.y + hy)), Color32::WHITE);
                                painter.line_segment([lr.center() - Vec2::new(10.0, 0.0), lr.center() + Vec2::new(10.0, 0.0)], Stroke::new(1.0, Color32::from_rgb(255, 80, 80)));
                                painter.line_segment([lr.center() - Vec2::new(0.0, 10.0), lr.center() + Vec2::new(0.0, 10.0)], Stroke::new(1.0, Color32::from_rgb(255, 80, 80)));
                            }
                        }
                    }
                    // input
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(if self.pe.adding { egui::CursorIcon::Crosshair } else { egui::CursorIcon::Default });
                        let sc = ui.input(|i| i.smooth_scroll_delta.y);
                        if sc.abs() > 0.1 {
                            if let Some(hp) = resp.hover_pos() {
                                events.push(PaneEvent::Zoom(pane, hp, (sc * 0.0025).exp()));
                            }
                        }
                    }
                    if resp.double_clicked() {
                        events.push(PaneEvent::Fit(pane));
                    } else if resp.clicked() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            events.push(PaneEvent::Click(pane, p));
                        }
                    }
                    if resp.drag_started() {
                        let primary = resp.drag_started_by(egui::PointerButton::Primary);
                        if let Some(p) = resp.interact_pointer_pos() {
                            events.push(PaneEvent::DragStart(pane, p, primary));
                        }
                    }
                    if resp.dragged() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            events.push(PaneEvent::Drag(pane, p, resp.drag_delta()));
                        }
                    }
                    if resp.drag_stopped() {
                        events.push(PaneEvent::DragEnd);
                    }
                });
                if pane == Pane::A {
                    ui.add_space(6.0);
                }
            }
        });
        if new_a != a || new_b != b {
            if new_a != a && new_a == b {
                new_b = a; // keep two different photos
            }
            self.pe.a = new_a;
            self.pe.b = new_b;
            self.pe.selected = None;
            self.pe.pending = None;
            return;
        }
        self.handle_pane_events(events, &geo, &pts);
        if ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)) {
            self.delete_selected();
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.pe.pending = None;
        }
        if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Z)) && !ui.ctx().egui_wants_keyboard_input() {
            self.undo_last();
        }
    }

    /// Strip under the toolbar while the patch click mode is on.
    fn click_panel(&mut self, ui: &mut egui::Ui) {
        let (a, b) = (self.pe.a, self.pe.b);
        let state = match (self.click.a, self.click.b) {
            (None, None) => "Patch click: click roughly the same spot in the LEFT and the RIGHT photo (a corner or a sharp detail).",
            (Some(_), None) => "Now click roughly the same spot in the RIGHT photo.",
            (None, Some(_)) => "Now click roughly the same spot in the LEFT photo.",
            _ => "",
        };
        if !state.is_empty() {
            ui.label(RichText::new(state).small().color(ACCENT));
        }
        let Some(r) = self.click.result.clone() else { return };
        if r.a != a || r.b != b {
            return;
        }
        egui::Frame::new().fill(CARD).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
            if let Some(e) = &r.error {
                ui.label(RichText::new(e).color(BAD));
                return;
            }
            ui.horizontal_wrapped(|ui| {
                if let Some(m) = &r.main {
                    ui.label(RichText::new(format!("Refined: score {:.3}, confidence {:.2}", m.score, m.conf)).strong());
                }
                ui.label(RichText::new(format!("local zoom ×{:.3}, rotation {:.1}° – {}", r.zoom(), r.rotation_deg(), r.j_source)).small().color(MUTED));
            });
            let n_on = r.main.iter().chain(r.more.iter()).filter(|s| s.accept).count();
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new(format!("✔ Accept {n_on} checked")).strong())).clicked() {
                    self.accept_click(false);
                }
                if ui.button("Accept all").clicked() {
                    self.accept_click(true);
                }
                if ui.button("✖ Discard").clicked() {
                    self.discard_click();
                }
                ui.label(RichText::new(format!("{} more pairs around the click (radius {:.0} px)", r.more.len(), self.click_cfg.more_radius)).small().color(MUTED));
            });
            let mut toggles: Vec<(usize, bool)> = Vec::new();
            egui::ScrollArea::vertical().id_salt("click_sugg").max_height(96.0).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (k, sg) in r.main.iter().chain(r.more.iter()).enumerate() {
                        let mut on = sg.accept;
                        let label = if sg.main { format!("clicked point · {:.2}", sg.score) } else { format!("#{k} · {:.2}", sg.score) };
                        if ui.checkbox(&mut on, label).on_hover_text(format!("score {:.3}, confidence {:.3}", sg.score, sg.conf)).changed() {
                            toggles.push((k, on));
                        }
                    }
                });
            });
            if let Some(res) = &mut self.click.result {
                for (k, on) in toggles {
                    if k == 0 && res.main.is_some() {
                        if let Some(m) = &mut res.main {
                            m.accept = on;
                        }
                    } else {
                        let i = if res.main.is_some() { k - 1 } else { k };
                        if let Some(s) = res.more.get_mut(i) {
                            s.accept = on;
                        }
                    }
                }
            }
        });
    }

    fn handle_pane_events(&mut self, events: Vec<PaneEvent>, geo: &[PaneGeo; 2], pts: &[(usize, P2, P2)]) {
        let (a, b) = (self.pe.a, self.pe.b);
        let px_of = |pane: Pane, pos: Pos2| -> P2 {
            let g = &geo[pane.i()];
            (((pos.x - g.origin.x) / g.scale - 0.5) as f64, ((pos.y - g.origin.y) / g.scale - 0.5) as f64)
        };
        let screen_of = |pane: Pane, p: P2| -> Pos2 {
            let g = &geo[pane.i()];
            g.origin + Vec2::new((p.0 as f32 + 0.5) * g.scale, (p.1 as f32 + 0.5) * g.scale)
        };
        let nearest = |pane: Pane, pos: Pos2| -> Option<usize> {
            pts.iter()
                .map(|(k, pa, pb)| (*k, screen_of(pane, if pane == Pane::A { *pa } else { *pb }).distance(pos)))
                .filter(|(_, d)| *d < 11.0)
                .min_by(|x, y| x.1.partial_cmp(&y.1).unwrap())
                .map(|(k, _)| k)
        };
        for ev in events {
            match ev {
                PaneEvent::Fit(pane) => {
                    let g = &geo[pane.i()];
                    self.pe.views[pane.i()].zoom = 1.0;
                    self.pe.views[pane.i()].center = Vec2::new(g.w / 2.0, g.h / 2.0);
                }
                PaneEvent::Zoom(pane, pos, f) => {
                    let g = &geo[pane.i()];
                    let v = &mut self.pe.views[pane.i()];
                    let p_img = (pos - g.origin) / g.scale;
                    v.zoom = (v.zoom * f).clamp(0.5, 80.0);
                    let ns = g.fit * v.zoom;
                    v.center = p_img - (pos - g.rect.center()) / ns;
                    v.center.x = v.center.x.clamp(0.0, g.w);
                    v.center.y = v.center.y.clamp(0.0, g.h);
                }
                PaneEvent::Click(pane, pos) => {
                    if self.click.on {
                        let p = px_of(pane, pos);
                        let (w, h) = (geo[pane.i()].w as f64, geo[pane.i()].h as f64);
                        if p.0 >= 0.0 && p.1 >= 0.0 && p.0 <= w - 1.0 && p.1 <= h - 1.0 {
                            if pane == Pane::A {
                                self.click.a = Some(p);
                            } else {
                                self.click.b = Some(p);
                            }
                            // new clicks replace open suggestions (undo keeps them)
                            if self.click.result.is_some() {
                                self.push_undo("replacing the patch click suggestions");
                                self.click.result = None;
                            }
                            self.run_click();
                        }
                    } else if self.pe.adding {
                        let p = px_of(pane, pos);
                        let (w, h) = (geo[pane.i()].w as f64, geo[pane.i()].h as f64);
                        if p.0 >= -0.5 && p.1 >= -0.5 && p.0 <= w - 0.5 && p.1 <= h - 0.5 {
                            self.add_point_click(pane, p);
                        }
                    } else {
                        self.pe.selected = nearest(pane, pos);
                        if self.pe.selected.is_some() {
                            self.pe.moving = Some(pane);
                        }
                        // show the partner in the other photo
                        if let Some(k) = self.pe.selected {
                            if let Some((_, pa, pb)) = pts.iter().find(|t| t.0 == k) {
                                let other = pane.other();
                                if self.pe.views[other.i()].zoom > 1.2 {
                                    let q = if other == Pane::A { *pa } else { *pb };
                                    self.pe.views[other.i()].center = Vec2::new(q.0 as f32 + 0.5, q.1 as f32 + 0.5);
                                }
                            }
                        }
                    }
                }
                PaneEvent::DragStart(pane, pos, primary) => {
                    self.pe.dragging = None;
                    self.pe.panning = None;
                    if primary {
                        if let Some(k) = nearest(pane, pos) {
                            self.pe.selected = Some(k);
                            self.pe.dragging = Some((k, pane));
                            self.pe.moving = Some(pane);
                            continue;
                        }
                    }
                    self.pe.panning = Some(pane);
                }
                PaneEvent::Drag(pane, pos, delta) => {
                    if let Some((k, dp)) = self.pe.dragging {
                        if dp == pane && k < self.cps.len() {
                            let idx = if pane == Pane::A { a } else { b };
                            let n = self.to_norm(idx, px_of(pane, pos));
                            let c = &mut self.cps[k];
                            if c.a == idx {
                                c.pa = n;
                            } else {
                                c.pb = n;
                            }
                            c.manual = true;
                            self.needs_align = true;
                        }
                    } else if self.pe.panning == Some(pane) {
                        let g = &geo[pane.i()];
                        let v = &mut self.pe.views[pane.i()];
                        v.center -= delta / g.scale;
                        v.center.x = v.center.x.clamp(0.0, g.w);
                        v.center.y = v.center.y.clamp(0.0, g.h);
                    }
                }
                PaneEvent::DragEnd => {
                    if let Some((k, pane)) = self.pe.dragging.take() {
                        self.snap_point(k, pane);
                    }
                    self.pe.panning = None;
                }
            }
        }
    }

    fn delete_selected(&mut self) {
        if let Some(k) = self.pe.selected.take() {
            if k < self.cps.len() {
                self.cps.remove(k);
                self.needs_align = true;
            }
        }
    }

    fn work_dims(&self, i: usize) -> (usize, usize) {
        let (w, h) = self.photos[i].work.dimensions();
        (w as usize, h as usize)
    }

    fn to_px(&self, i: usize, p: [f64; 2]) -> P2 {
        let (w, h) = self.work_dims(i);
        let (x, y) = engine::denorm_coord(p, w, h);
        (x as f64, y as f64)
    }

    fn to_norm(&self, i: usize, p: P2) -> [f64; 2] {
        let (w, h) = self.work_dims(i);
        engine::norm_coord(p.0 as f32, p.1 as f32, w, h)
    }

    /// (number of points, mean error, max error) between two photos.
    fn pair_stats(&self, a: usize, b: usize) -> (usize, f64, f64) {
        let mut n = 0;
        let mut s = 0.0;
        let mut m = 0.0f64;
        for c in &self.cps {
            if (c.a == a && c.b == b) || (c.a == b && c.b == a) {
                n += 1;
                s += c.err;
                m = m.max(c.err);
            }
        }
        (n, if n > 0 { s / n as f64 } else { 0.0 }, m)
    }

    /// How much larger things appear in photo `to` than in photo `from` (work pixels), from the lenses.
    fn scale_prior(&self, from: usize, to: usize) -> f64 {
        let lf = self.work_dims(from);
        let lt = self.work_dims(to);
        let pf = self.lenses[self.photos[from].lens].f * lf.0.max(lf.1) as f64;
        let pt = self.lenses[self.photos[to].lens].f * lt.0.max(lt.1) as f64;
        (pt / pf).clamp(0.05, 20.0)
    }

    /// Point pairs between two photos, in work pixels, oriented from -> to.
    fn pair_seeds(&self, from: usize, to: usize) -> Vec<(P2, P2)> {
        self.cps
            .iter()
            .filter_map(|c| {
                if c.a == from && c.b == to {
                    Some((self.to_px(from, c.pa), self.to_px(to, c.pb)))
                } else if c.a == to && c.b == from {
                    Some((self.to_px(from, c.pb), self.to_px(to, c.pa)))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Where a spot of photo `from` should be in photo `to`, and the local zoom/tilt between them.
    fn predict(&self, from: usize, to: usize, p: P2) -> Option<(P2, [f64; 4])> {
        let seeds = self.pair_seeds(from, to);
        if seeds.len() == 1 {
            // one point: zoom from the lenses, rotation measured at that point
            let ((x, y), (u, v)) = seeds[0];
            let j = self.rotation_guess(from, (x, y), to, (u, v));
            let t = [j[0], j[1], u - j[0] * x - j[1] * y, j[2], j[3], v - j[2] * x - j[3] * y, 0.0, 0.0, 1.0];
            return Some((h_apply(&t, p.0, p.1), j));
        }
        if !seeds.is_empty() {
            let (w, h) = self.work_dims(from);
            let t = guided::estimate(&seeds, self.scale_prior(from, to), w.max(h) as f64)?;
            return Some((h_apply(&t, p.0, p.1), guided::jacobian(&t, p)));
        }
        if self.aligned && !self.needs_align {
            let lf = self.lenses[self.photos[from].lens];
            let lt = self.lenses[self.photos[to].lens];
            let rf = self.photos[from].rot;
            let rt = self.photos[to].rot;
            let map = |q: P2| -> Option<P2> {
                let w = mtv(&rf, crate::align::ray(&lf, self.to_norm(from, q)));
                crate::align::project(&lt, mv(&rt, w)).map(|n| self.to_px(to, n))
            };
            let c = map(p)?;
            let x = map((p.0 + 1.0, p.1))?;
            let y = map((p.0, p.1 + 1.0))?;
            let (w, h) = self.work_dims(to);
            if c.0 < 0.0 || c.1 < 0.0 || c.0 >= w as f64 || c.1 >= h as f64 {
                return None;
            }
            return Some((c, [x.0 - c.0, y.0 - c.0, x.1 - c.1, y.1 - c.1]));
        }
        None
    }

    /// Work pixels per full-size pixel of a photo.
    fn work_per_full(&self, i: usize) -> f64 {
        self.photos[i].work.width() as f64 / self.photos[i].w.max(1) as f64
    }

    /// Search radius (work px) when placing a point: 10 % of the photo width, at most 500 px.
    fn placing_radius(&self, i: usize) -> f64 {
        let win_work = guided::placing_window(self.photos[i].w as f64, self.search_pct) * self.work_per_full(i);
        ((win_work - self.tpl_size as f64) / 2.0).max(2.0)
    }

    /// Matching window of the selected point (cached; kept while the point is being dragged).
    fn selection_frame(&mut self) -> Option<SelFrame> {
        let k = self.pe.selected?;
        if k >= self.cps.len() {
            return None;
        }
        let (a, b) = (self.pe.a, self.pe.b);
        let c = self.cps[k];
        if !((c.a == a && c.b == b) || (c.a == b && c.b == a)) {
            return None;
        }
        let moving = self.pe.moving.unwrap_or(Pane::B);
        let fixed = moving.other();
        let (pa, pb) = if c.a == a { (c.pa, c.pb) } else { (c.pb, c.pa) };
        let (fi, mi, fp, mp) = if fixed == Pane::A { (a, b, pa, pb) } else { (b, a, pb, pa) };
        let key = (
            k,
            fixed == Pane::A,
            (fp[0] * 1e4) as i64,
            (fp[1] * 1e4) as i64,
            (mp[0] * 1e4) as i64,
            (mp[1] * 1e4) as i64,
            self.cps.len(),
        );
        if let Some(f) = self.sel_frame {
            if f.key == key || (self.pe.dragging.is_some() && f.key.0 == k && f.fixed == fixed) {
                return Some(f);
            }
        }
        let pf = self.to_px(fi, fp);
        let pm = self.to_px(mi, mp);
        let j = self.predict(fi, mi, pf).map(|x| x.1).unwrap_or_else(|| self.rotation_guess(fi, pf, mi, pm));
        let f = SelFrame { key, fixed, j };
        self.sel_frame = Some(f);
        Some(f)
    }

    /// Search radius (work px) when snapping a moved point (local search).
    fn local_radius(&self, i: usize) -> f64 {
        let win = guided::LOCAL_SEARCH_WIDTH + guided::TEMPLATE_SIZE as f64;
        (guided::window_radius(win) * self.work_per_full(i)).max(1.5)
    }

    /// Zoom from the lenses plus a measured rotation, for a point pair without any other help.
    fn rotation_guess(&self, fixed: usize, pf: P2, moving: usize, guess: P2) -> [f64; 4] {
        let s = self.scale_prior(fixed, moving);
        let gf = &self.photos[fixed].gray;
        let gm = &self.photos[moving].gray;
        let fs = (s < 1.0 / 1.5).then(|| imgutil::blur(gf, guided::soft_sigma(1.0 / s)));
        let ms = (s > 1.5).then(|| imgutil::blur(gm, guided::soft_sigma(s)));
        let c = guided::Corr { fixed: gf, fixed_soft: fs.as_ref(), moving: gm, moving_soft: ms.as_ref(), tpl_r: self.tpl_size / 2 };
        // the click can be a few pixels off: look around it
        let mut best = ([s, 0.0, 0.0, s], -2.0f32);
        for (dx, dy) in [(0.0, 0.0), (-4.0, 0.0), (4.0, 0.0), (0.0, -4.0), (0.0, 4.0)] {
            let r = guided::best_rotation(&c, pf, s, (guess.0 + dx, guess.1 + dy));
            if r.1 > best.1 {
                best = r;
            }
        }
        best.0
    }

    /// Correlation search for the spot in `moving` that matches `pf` in `fixed`.
    fn finetune(&self, fixed: usize, pf: P2, moving: usize, guess: P2, j: [f64; 4], search: f64) -> Option<(P2, f32)> {
        let s = (j[0] * j[3] - j[1] * j[2]).abs().sqrt();
        let gf = &self.photos[fixed].gray;
        let gm = &self.photos[moving].gray;
        let fs = (s < 1.0 / 1.5).then(|| imgutil::blur(gf, guided::soft_sigma(1.0 / s)));
        let ms = (s > 1.5).then(|| imgutil::blur(gm, guided::soft_sigma(s)));
        let c = guided::Corr { fixed: gf, fixed_soft: fs.as_ref(), moving: gm, moving_soft: ms.as_ref(), tpl_r: self.tpl_size / 2 };
        guided::refine(&c, pf, j, guess, search).map(|(x, y, v)| ((x, y), v))
    }

    fn add_point_click(&mut self, pane: Pane, p: P2) {
        let (a, b) = (self.pe.a, self.pe.b);
        let this = if pane == Pane::A { a } else { b };
        let other = if pane == Pane::A { b } else { a };
        match self.pe.pending {
            Some((q, qn)) if q != pane => {
                // second click: keep the first spot, snap this one onto it
                let fixed = other;
                let pf = self.to_px(fixed, qn);
                let j = match self.predict(fixed, this, pf) {
                    Some(x) => x.1,
                    None => self.rotation_guess(fixed, pf, this, p),
                };
                let r = self.placing_radius(this);
                let snapped = self.finetune(fixed, pf, this, p, j, r).filter(|r| r.1 >= guided::FINETUNE_MIN_NCC);
                let final_p = snapped.map(|r| r.0).unwrap_or(p);
                self.pe.pending = None;
                let n_this = self.to_norm(this, final_p);
                if snapped.is_some() {
                    self.set_status("Point added and snapped to the exact spot.", GOOD);
                } else {
                    self.set_status("Point added where you clicked (no clear detail to snap to).", WARN);
                }
                self.push_manual(q, qn, n_this);
            }
            _ => {
                let pn = self.to_norm(this, p);
                if let Some((guess, j)) = self.predict(this, other, p) {
                    let r = self.placing_radius(other);
                    if let Some((pos, v)) = self.finetune(this, p, other, guess, j, r) {
                        if v >= guided::FINETUNE_MIN_NCC {
                            let on = self.to_norm(other, pos);
                            let o = pane.other();
                            if self.pe.views[o.i()].zoom > 1.2 {
                                self.pe.views[o.i()].center = Vec2::new(pos.0 as f32 + 0.5, pos.1 as f32 + 0.5);
                            }
                            self.set_status("Point added – the matching spot was found and snapped automatically. Drag it if it is off.", GOOD);
                            self.push_manual(pane, pn, on);
                            return;
                        }
                    }
                }
                self.pe.pending = Some((pane, pn));
            }
        }
    }

    /// Store a hand-placed pair; `n1` belongs to pane `p1`, `n2` to the other pane.
    fn push_manual(&mut self, p1: Pane, n1: [f64; 2], n2: [f64; 2]) {
        let (a, b) = (self.pe.a, self.pe.b);
        let (pa, pb) = if p1 == Pane::A { (n1, n2) } else { (n2, n1) };
        self.cps.push(ControlPoint::new(a, b, pa, pb, true));
        self.pe.selected = Some(self.cps.len() - 1);
        self.needs_align = true;
        if self.auto_grow {
            let auto = self.cps.iter().filter(|c| !c.manual && ((c.a == a && c.b == b) || (c.a == b && c.b == a))).count();
            if auto < 10 {
                self.run_pair_points();
            }
        }
    }

    /// After dragging a point: snap it onto its partner with sub-pixel accuracy.
    fn snap_point(&mut self, k: usize, pane: Pane) {
        if k >= self.cps.len() {
            return;
        }
        let (a, b) = (self.pe.a, self.pe.b);
        let c = self.cps[k];
        let (pa, pb) = if c.a == a { (c.pa, c.pb) } else { (c.pb, c.pa) };
        let (fixed, moving, fixed_p, moving_p) = if pane == Pane::A { (b, a, pb, pa) } else { (a, b, pa, pb) };
        let pf = self.to_px(fixed, fixed_p);
        let pm = self.to_px(moving, moving_p);
        // local zoom/tilt from the other points of the pair (without this one)
        let seeds: Vec<(P2, P2)> = self.pair_seeds(fixed, moving).into_iter().filter(|s| (s.0 .0 - pf.0).hypot(s.0 .1 - pf.1) > 0.01).collect();
        let (w, h) = self.work_dims(fixed);
        let j = if seeds.len() >= 2 {
            guided::estimate(&seeds, self.scale_prior(fixed, moving), w.max(h) as f64).map(|t| guided::jacobian(&t, pf))
        } else {
            None
        }
        .unwrap_or_else(|| self.rotation_guess(fixed, pf, moving, pm));
        let r = self.local_radius(moving);
        if let Some((q, v)) = self.finetune(fixed, pf, moving, pm, j, r) {
            if v >= guided::FINETUNE_MIN_NCC {
                let n = self.to_norm(moving, q);
                let cp = &mut self.cps[k];
                if cp.a == moving {
                    cp.pa = n;
                } else {
                    cp.pb = n;
                }
                self.set_status(format!("Point snapped to the exact spot (match {:.0} %).", v * 100.0), GOOD);
            }
        }
    }

    /// Find points for the current pair only (automatic, or growing from existing points).
    fn run_pair_points(&mut self) {
        if self.busy() {
            self.set_status("Please wait until the current task has finished.", WARN);
            return;
        }
        let (a, b) = (self.pe.a, self.pe.b);
        let seeds = self.pair_seeds(a, b);
        let ga = self.photos[a].gray.clone();
        let gb = self.photos[b].gray.clone();
        let sky = self.match_settings.ignore_sky;
        let ra = sky.then(|| self.photos[a].work.clone());
        let rb = sky.then(|| self.photos[b].work.clone());
        let prior = self.scale_prior(a, b);
        let max = self.match_settings.points_per_pair;
        let (wa, ha) = self.work_dims(a);
        let (wb, hb) = self.work_dims(b);
        let mode = if seeds.is_empty() { "automatically" } else { "from your points" };
        self.start_job(&format!("Finding points between photo {} and {}", a + 1, b + 1), move |report, _| {
            let t0 = Instant::now();
            report(0.2, "Searching this photo pair");
            let found = if seeds.is_empty() {
                guided::auto_pair(&ga, &gb, ra.as_deref(), rb.as_deref(), prior, max)
            } else {
                guided::grow(&ga, &gb, ra.as_deref(), rb.as_deref(), &seeds, prior, max)
            };
            let cps = found
                .into_iter()
                .map(|(p, q, _)| ControlPoint {
                    a,
                    b,
                    pa: engine::norm_coord(p.0 as f32, p.1 as f32, wa, ha),
                    pb: engine::norm_coord(q.0 as f32, q.1 as f32, wb, hb),
                    ..ControlPoint::new(0, 0, [0.0; 2], [0.0; 2], false)
                })
                .collect();
            JobMsg::PairPoints { a, b, cps, ms: t0.elapsed().as_millis(), mode }
        });
    }

    fn on_pair_points(&mut self, a: usize, b: usize, cps: Vec<ControlPoint>, ms: u128, mode: &str) {
        if a >= self.photos.len() || b >= self.photos.len() {
            return;
        }
        if cps.len() < 4 {
            self.set_status(
                format!(
                    "No reliable points found {mode} between photo {} and {} ({}). Add a point by hand on a clear detail both photos share.",
                    a + 1,
                    b + 1,
                    fmt_ms(ms)
                ),
                WARN,
            );
            return;
        }
        let sel = self.pe.selected.and_then(|k| self.cps.get(k).copied());
        self.cps.retain(|c| c.manual || !((c.a == a && c.b == b) || (c.a == b && c.b == a)));
        let n = cps.len();
        self.cps.extend(cps);
        self.matched = true;
        self.needs_align = true;
        self.pe.selected = sel.and_then(|s| self.cps.iter().position(|c| c.a == s.a && c.b == s.b && c.pa == s.pa && c.pb == s.pb));
        self.set_status(
            format!("Found {n} points {mode} between photo {} and {} in {}. Align again (step 3) to use them.", a + 1, b + 1, fmt_ms(ms)),
            GOOD,
        );
    }

    fn request_full(&mut self, i: usize) {
        if self.full_tex.contains_key(&i) || self.full_pending.contains(&i) {
            return;
        }
        let long = self.photos[i].w.max(self.photos[i].h);
        if long <= self.photos[i].work.width().max(self.photos[i].work.height()) {
            return; // the working copy is already full size
        }
        self.full_pending.insert(i);
        let src = self.photos[i].source.clone();
        let tx = self.full_tx.clone();
        let name = self.photos[i].name.clone();
        std::thread::spawn(move || {
            if let Ok(img) = src.load_full() {
                // the two full-size photos shown in the point editor share the image-cache bound
                let max_px = (IMAGE_CACHE_UPPER_BOUND / 2 / 4) as f64;
                let px = img.width() as f64 * img.height() as f64;
                let long = img.width().max(img.height()) as f64;
                let side = if px > max_px { (long * (max_px / px).sqrt()) as u32 } else { long as u32 };
                let img = imgutil::downscale_rgb(&img, side.min(8192));
                let _ = tx.send((i, name, color_image(&img)));
            }
        });
    }

    // ----- step 3 -----
    fn ui_align(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if primary_button(ui, "📐  Align photos", !self.busy() && !self.cps.is_empty()).clicked() {
                self.run_align(false);
            }
            if ui
                .selectable_label(self.patch_win.open, "🔬  Patch comparison")
                .on_hover_text("Opens a separate window that shows every patch comparison of the fine alignment – live while it runs and afterwards as a list.")
                .clicked()
            {
                self.patch_win.open = !self.patch_win.open;
            }
            if ui.add_enabled(self.aligned && !self.busy(), egui::Button::new("⟲  Level the horizon")).clicked() {
                let mut rots: Vec<M3> = self.photos.iter().map(|p| p.rot).collect();
                let used: Vec<bool> = self.photos.iter().map(|p| p.enabled).collect();
                crate::align::straighten(&mut rots, &used);
                for (p, r) in self.photos.iter_mut().zip(rots) {
                    p.rot = r;
                }
                self.fit_view();
                self.set_status("Horizon levelled.", GOOD);
            }
            if self.needs_align && self.aligned {
                ui.label(RichText::new("Something changed – align again to update.").color(WARN));
            }
        });
        ui.add_space(6.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            card(ui, |ui| {
                ui.label(RichText::new("Result").strong().size(16.0));
                if let Some((mean, max, removed, ms)) = self.last_align {
                    let (q, c) = if mean < 1.5 { ("Excellent", GOOD) } else if mean < 3.0 { ("Good", GOOD) } else if mean < 6.0 { ("Fair", WARN) } else { ("Poor", BAD) };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(q).size(26.0).strong().color(c));
                        ui.add_space(12.0);
                        ui.vertical(|ui| {
                            ui.label(format!("Average error {mean:.2} px · worst point {max:.1} px"));
                            ui.label(RichText::new(format!("{} points used · {removed} wrong matches removed automatically · {}", self.cps.len(), fmt_ms(ms))).small().color(MUTED));
                        });
                    });
                    if mean >= 3.0 {
                        ui.label(RichText::new("Tip: open step 2, look at the pairs marked red and delete wrong points, or use “Clean up”.").small().color(WARN));
                    }
                } else {
                    ui.label(RichText::new("Not aligned yet. Press “Align photos”.").color(MUTED));
                }
                let comps = crate::align::components(self.photos.len(), &self.cps);
                let main = most_common(&comps);
                let lonely: Vec<String> = (0..self.photos.len()).filter(|&i| self.photos[i].enabled && comps[i] != main).map(|i| (i + 1).to_string()).collect();
                if self.matched && !lonely.is_empty() {
                    ui.label(
                        RichText::new(format!(
                            "⚠ Photo{} {} {} not connected to the rest. Add points by hand in step 2, or untick “use” in the photo list.",
                            if lonely.len() > 1 { "s" } else { "" },
                            lonely.join(", "),
                            if lonely.len() > 1 { "are" } else { "is" }
                        ))
                        .color(WARN),
                    );
                }
            });
            ui.add_space(6.0);
            card(ui, |ui| {
                ui.label(RichText::new("Options").strong());
                ui.checkbox(&mut self.align_settings.fine_tune_fov, "Measure the lens field of view precisely");
                ui.checkbox(&mut self.align_settings.distortion, "Correct lens distortion (barrel / pincushion)");
                ui.checkbox(&mut self.align_settings.remove_outliers, "Remove wrong matches automatically");
                ui.checkbox(&mut self.align_settings.level, "Level the horizon automatically");
            });
            ui.add_space(6.0);
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Pair check (overlapping photos without points)").strong());
                    if ui.button("🕸 Connections").clicked() {
                        self.show_graph = !self.show_graph;
                    }
                });
                ui.checkbox(&mut self.pair_cfg.enabled, "After aligning, search overlapping pairs that have too few points again");
                ui.label(
                    RichText::new("Uses the estimated overlap of every pair: the point finder only inside the overlap, then a search predicted through a third photo, then patch matching. Pairs that still have too few points are reported, nothing is made up. Off = exactly as before.")
                        .small()
                        .color(MUTED),
                );
                ui.add_enabled_ui(self.pair_cfg.enabled, |ui| {
                    egui::Grid::new("pair_grid").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                        let c = &mut self.pair_cfg;
                        ui.label("Overlap from");
                        let mut pct = c.min_overlap * 100.0;
                        let (lo, hi) = crate::pair_check::MIN_OVERLAP_RANGE;
                        if ui.add(egui::DragValue::new(&mut pct).range(lo * 100.0..=hi * 100.0).speed(0.5).suffix(" %")).on_hover_text(format!("share of the smaller photo; {:.0} % = obviously overlapping", crate::pair_check::OBVIOUS_OVERLAP * 100.0)).changed() {
                            c.min_overlap = pct / 100.0;
                        }
                        ui.end_row();
                        ui.label("Too few below");
                        ui.add(egui::DragValue::new(&mut c.min_points).range(1..=100).suffix(" points"));
                        ui.end_row();
                        ui.label("Rounds at most");
                        ui.add(egui::DragValue::new(&mut c.max_rounds).range(1..=5));
                        ui.end_row();
                        ui.label("Steps");
                        ui.vertical(|ui| {
                            ui.checkbox(&mut c.restrict_search, "point finder only inside the overlap");
                            ui.checkbox(&mut c.predict_via_third, "predict through a third photo");
                            ui.checkbox(&mut c.patch_fallback, "patch matching in the overlap");
                        });
                        ui.end_row();
                    });
                });
            });
            ui.add_space(6.0);
            card(ui, |ui| {
                ui.label(RichText::new("Fine alignment (patch matching)").strong());
                ui.checkbox(&mut self.fine_cfg.enabled, "Compare image patches after aligning and refine the alignment (sharper seams)");
                ui.label(
                    RichText::new("Slides small patches of overlapping photos over each other to find the remaining shift with sub-pixel accuracy, refines the points, adds control areas along the seams and re-aligns in rounds. Off = exactly as before.")
                        .small()
                        .color(MUTED),
                );
                ui.add_enabled_ui(self.fine_cfg.enabled, |ui| {
                    ui.checkbox(&mut self.fine_cfg.refine_points, "Refine control points");
                    ui.checkbox(&mut self.fine_cfg.areas, "Add control areas in the overlaps (denser along the seams)");
                    ui.checkbox(&mut self.parallax_cfg.enabled, "Parallax mode: local correction near the seams (for close objects)");
                    egui::CollapsingHeader::new("Advanced settings").id_salt("fine_adv").show(ui, |ui| {
                        egui::Grid::new("fine_grid").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                            let m = &mut self.fine_cfg.matcher;
                            ui.label("Patch size");
                            ui.add(egui::DragValue::new(&mut m.patch_size).range(24..=256).suffix(" px"));
                            ui.end_row();
                            ui.label("Search radius");
                            ui.add(egui::DragValue::new(&mut m.search_radius).range(2.0..=64.0).speed(0.2).suffix(" px"));
                            ui.end_row();
                            ui.label("Measure");
                            egui::ComboBox::from_id_salt("metric").selected_text(m.metric.name()).show_ui(ui, |ui| {
                                for x in Metric::ALL {
                                    ui.selectable_value(&mut m.metric, x, x.name());
                                }
                            });
                            ui.end_row();
                            ui.label("Compare");
                            egui::ComboBox::from_id_salt("fspace").selected_text(m.feature_space.name()).show_ui(ui, |ui| {
                                for x in FeatureSpace::ALL {
                                    ui.selectable_value(&mut m.feature_space, x, x.name());
                                }
                            });
                            ui.end_row();
                            ui.label("Pyramid levels");
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut m.use_pyramid, "");
                                ui.add_enabled(m.use_pyramid, egui::DragValue::new(&mut m.pyramid_levels).range(1..=4));
                            });
                            ui.end_row();
                            ui.label("Min. similarity");
                            ui.add(egui::DragValue::new(&mut m.min_score).range(0.0..=1.0).speed(0.01));
                            ui.end_row();
                            ui.label("Min. uniqueness");
                            ui.add(egui::DragValue::new(&mut m.min_uniqueness).range(1.0..=3.0).speed(0.01));
                            ui.end_row();
                            ui.label("Min. texture");
                            let mut t = m.min_texture * 1e5;
                            if ui.add(egui::DragValue::new(&mut t).range(0.0..=100.0).speed(0.1).suffix(" ×1e-5")).changed() {
                                m.min_texture = t / 1e5;
                            }
                            ui.end_row();
                            ui.label("Forward-backward check");
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut m.forward_backward, "");
                                ui.add_enabled(m.forward_backward, egui::DragValue::new(&mut m.fb_tolerance).range(0.05..=5.0).speed(0.01).suffix(" px"));
                            });
                            ui.end_row();
                            ui.label("Valid pixels needed");
                            ui.add(egui::DragValue::new(&mut m.min_valid_fraction).range(0.3..=1.0).speed(0.01));
                            ui.end_row();
                            ui.label("Pre-blur (sigma)");
                            ui.add(egui::DragValue::new(&mut m.blur_sigma).range(0.0..=3.0).speed(0.01));
                            ui.end_row();
                            let f = &mut self.fine_cfg;
                            ui.label("Point refinement: template");
                            ui.add(egui::DragValue::new(&mut f.point_template).range(9..=101).suffix(" px"));
                            ui.end_row();
                            ui.label("Point refinement: search");
                            ui.add(egui::DragValue::new(&mut f.point_search).range(1.0..=50.0).speed(0.1).suffix(" px"));
                            ui.end_row();
                            ui.label("Point refinement: min. correlation");
                            ui.add(egui::DragValue::new(&mut f.point_min_score).range(0.0..=1.0).speed(0.01));
                            ui.end_row();
                            ui.label("Area grid cell");
                            ui.add(egui::DragValue::new(&mut f.grid_cell).range(16..=256).suffix(" px"));
                            ui.end_row();
                            ui.label("Max. areas per pair");
                            ui.add(egui::DragValue::new(&mut f.max_areas_per_pair).range(10..=2000));
                            ui.end_row();
                            ui.label("Dense band around the seam");
                            ui.add(egui::DragValue::new(&mut f.seam_band).range(0.0..=1000.0).suffix(" px"));
                            ui.end_row();
                            ui.label("Weight of hand-drawn areas");
                            ui.add(egui::DragValue::new(&mut f.manual_area_weight).range(0.1..=10.0).speed(0.05));
                            ui.end_row();
                            ui.label("Areas vs. points (balance)");
                            ui.add(egui::DragValue::new(&mut f.area_weight_scale).range(0.0..=10.0).speed(0.05));
                            ui.end_row();
                            ui.label("Robust loss");
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut f.robust_loss, RobustLoss::Huber, "Huber");
                                ui.radio_value(&mut f.robust_loss, RobustLoss::Cauchy, "Cauchy");
                            });
                            ui.end_row();
                            ui.label("Rounds");
                            ui.add(egui::DragValue::new(&mut f.refine_iterations).range(1..=10));
                            ui.end_row();
                            ui.label("Stop when shift below");
                            ui.add(egui::DragValue::new(&mut f.converge_threshold).range(0.01..=2.0).speed(0.01).suffix(" px"));
                            ui.end_row();
                            ui.label("Points that fail the check");
                            ui.checkbox(&mut f.drop_invalid_points, "remove (otherwise keep with low weight)");
                            ui.end_row();
                            let p = &mut self.parallax_cfg;
                            ui.label("Parallax: largest correction");
                            ui.add(egui::DragValue::new(&mut p.max_shift).range(1.0..=30.0).speed(0.1).suffix(" px"));
                            ui.end_row();
                            ui.label("Parallax: smoothing");
                            ui.add(egui::DragValue::new(&mut p.smooth_cells).range(0.5..=5.0).speed(0.05).suffix(" cells"));
                            ui.end_row();
                        });
                        if ui.button("Reset to defaults").clicked() {
                            self.fine_cfg = crate::fine_align::FineConfig::default();
                            self.parallax_cfg = crate::fine_align::ParallaxConfig::default();
                        }
                    });
                });
                if let Some(st) = &self.last_fine {
                    ui.separator();
                    ui.label(fine_summary(st));
                    if st.rolled_back {
                        ui.label(RichText::new("The last round was undone because it made the result worse.").color(WARN));
                    }
                    egui::CollapsingHeader::new(format!("Why {} patches were not used", st.patches_total - st.patches_valid)).id_salt("fine_reasons").show(ui, |ui| {
                        for (k, v) in &st.reasons {
                            if k != "ok" {
                                ui.label(format!("{v} × {k}"));
                            }
                        }
                    });
                    egui::CollapsingHeader::new("Log").id_salt("fine_log").show(ui, |ui| {
                        for l in &st.log {
                            ui.label(RichText::new(l).small().monospace());
                        }
                    });
                }
            });
            ui.add_space(6.0);
            if self.aligned {
                card(ui, |ui| {
                    ui.label(RichText::new("Camera directions").strong());
                    egui::Grid::new("dirs").striped(true).spacing(Vec2::new(18.0, 4.0)).show(ui, |ui| {
                        for h in ["Photo", "Left/right", "Up/down", "Tilt", "Points", "Avg. error"] {
                            ui.label(RichText::new(h).color(MUTED));
                        }
                        ui.end_row();
                        for (i, p) in self.photos.iter().enumerate() {
                            let (y, pi, r) = geometry::ypr_of(&p.rot);
                            let mine: Vec<&ControlPoint> = self.cps.iter().filter(|c| c.a == i || c.b == i).collect();
                            let avg = if mine.is_empty() { 0.0 } else { mine.iter().map(|c| c.err).sum::<f64>() / mine.len() as f64 };
                            ui.label(format!("{}. {}", i + 1, p.name));
                            ui.label(format!("{:+.1}°", y.to_degrees()));
                            ui.label(format!("{:+.1}°", pi.to_degrees()));
                            ui.label(format!("{:+.1}°", r.to_degrees()));
                            ui.label(format!("{}", mine.len()));
                            ui.label(RichText::new(format!("{avg:.2} px")).color(err_color(avg)));
                            ui.end_row();
                        }
                    });
                    ui.add_space(4.0);
                    for (li, l) in self.lenses.iter().enumerate() {
                        ui.label(RichText::new(format!("Lens {}: {:.1}° wide, distortion {:+.3}", li + 1, l.hfov_deg(), l.k1)).small().color(MUTED));
                    }
                });
            }
        });
    }

    // ----- step 4 -----
    fn ui_frame(&mut self, ui: &mut egui::Ui) {
        if !self.aligned {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("The preview appears once the photos are aligned.").size(16.0).color(MUTED));
                ui.add_space(8.0);
                if primary_button(ui, "✨  Create panorama", self.photos.len() >= 2 && !self.busy()).clicked() {
                    self.create_auto();
                }
            });
            return;
        }
        ui.horizontal(|ui| {
            ui.label("Projection:");
            let before = self.view.projection;
            egui::ComboBox::from_id_salt("proj").width(220.0).selected_text(self.view.projection.name()).show_ui(ui, |ui| {
                for p in Projection::ALL {
                    ui.selectable_value(&mut self.view.projection, p, p.name()).on_hover_text(p.hint());
                }
            });
            if before != self.view.projection {
                self.fit_view();
                self.auto_crop();
            }
            if ui.button("⤢ Fit to photos").on_hover_text("Show all photos, remove the crop").clicked() {
                self.fit_view();
            }
            if ui.button("✂ Auto crop").on_hover_text("Largest rectangle without empty edges").clicked() {
                self.auto_crop();
            }
            if ui.button("No crop").clicked() {
                self.view.crop = [0.0, 1.0, 0.0, 1.0];
                self.preview_dirty = true;
            }
            if ui.button("⟲ Reset view").clicked() {
                self.view.yaw = 0.0;
                self.view.pitch = 0.0;
                self.view.roll = 0.0;
                self.fit_view();
            }
            if ui.checkbox(&mut self.match_brightness, "Match brightness").changed() {
                self.preview_dirty = true;
            }
            ui.checkbox(&mut self.show_outlines, "Show photo borders");
        });
        let (hf, vf) = self.view.fov_deg();
        let nw = self.natural_width();
        let cb = self.view.crop_bounds();
        let nh = nw * (cb[3] - cb[2]) / (cb[1] - cb[0]);
        ui.label(
            RichText::new(format!(
                "{} · view {:.0}° × {:.0}° · full size {:.0} × {:.0} px   —   drag to move, Shift+drag to tilt, scroll to zoom, drag the crop edges to adjust",
                self.view.projection.hint(),
                hf,
                vf,
                nw,
                nh
            ))
            .small()
            .color(MUTED),
        );
        let avail = ui.available_size() - Vec2::new(0.0, 4.0);
        let (resp, painter) = ui.allocate_painter(avail, Sense::click_and_drag());
        painter.rect_filled(resp.rect, 8.0, BG);
        let aspect = self.view.aspect() as f32;
        let mut r = resp.rect.shrink(10.0);
        if r.width() / r.height() > aspect {
            let w = r.height() * aspect;
            r = Rect::from_center_size(r.center(), Vec2::new(w, r.height()));
        } else {
            let h = r.width() / aspect;
            r = Rect::from_center_size(r.center(), Vec2::new(r.width(), h));
        }
        // checkerboard behind (transparent areas)
        painter.rect_filled(r, 0.0, Color32::from_rgb(34, 36, 42));
        if let Some(t) = &self.preview_tex {
            painter.image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
        // crop frame
        let c = self.view.crop;
        let crop_rect = Rect::from_min_max(
            Pos2::new(r.min.x + c[0] as f32 * r.width(), r.min.y + c[2] as f32 * r.height()),
            Pos2::new(r.min.x + c[1] as f32 * r.width(), r.min.y + c[3] as f32 * r.height()),
        );
        painter.rect_stroke(crop_rect, 0.0, Stroke::new(2.0, Color32::WHITE), StrokeKind::Middle);
        for corner in [crop_rect.left_top(), crop_rect.right_top(), crop_rect.left_bottom(), crop_rect.right_bottom()] {
            painter.circle_filled(corner, 4.5, Color32::WHITE);
        }
        // thirds
        for k in 1..3 {
            let x = crop_rect.min.x + crop_rect.width() * k as f32 / 3.0;
            let y = crop_rect.min.y + crop_rect.height() * k as f32 / 3.0;
            painter.line_segment([Pos2::new(x, crop_rect.min.y), Pos2::new(x, crop_rect.max.y)], Stroke::new(0.5, Color32::from_white_alpha(60)));
            painter.line_segment([Pos2::new(crop_rect.min.x, y), Pos2::new(crop_rect.max.x, y)], Stroke::new(0.5, Color32::from_white_alpha(60)));
        }
        if self.show_outlines {
            let rp = self.render_photos(false);
            let m = stitch::Mapper::new(&self.view, self.view.bounds, 1000, 1000);
            let vr = self.view.rotation();
            for (i, p) in rp.iter().enumerate() {
                if !self.photos[i].enabled {
                    continue;
                }
                let pts: Vec<Option<Pos2>> = stitch::photo_outline(p, 30)
                    .into_iter()
                    .map(|d| m.to_px(d, &vr).map(|(x, y)| Pos2::new(r.min.x + x as f32 / 1000.0 * r.width(), r.min.y + y as f32 / 1000.0 * r.height())))
                    .collect();
                let col = hue(i);
                for w in pts.windows(2) {
                    if let (Some(p0), Some(p1)) = (w[0], w[1]) {
                        if (p0.x - p1.x).abs() < r.width() * 0.5 {
                            painter.line_segment([p0, p1], Stroke::new(1.5, col));
                        }
                    }
                }
                let centre = mtv(&p.rot, [0.0, 0.0, 1.0]);
                if let Some((x, y)) = m.to_px(centre, &vr) {
                    let pos = Pos2::new(r.min.x + x as f32 / 1000.0 * r.width(), r.min.y + y as f32 / 1000.0 * r.height());
                    painter.circle_filled(pos, 11.0, Color32::from_black_alpha(160));
                    painter.text(pos, Align2::CENTER_CENTER, format!("{}", i + 1), FontId::proportional(13.0), col);
                }
            }
        }
        // interaction
        let near_edge = |pos: Pos2| -> Option<usize> {
            let d = 7.0;
            let in_y = pos.y > crop_rect.min.y - d && pos.y < crop_rect.max.y + d;
            let in_x = pos.x > crop_rect.min.x - d && pos.x < crop_rect.max.x + d;
            if in_y && (pos.x - crop_rect.min.x).abs() < d {
                Some(0)
            } else if in_y && (pos.x - crop_rect.max.x).abs() < d {
                Some(1)
            } else if in_x && (pos.y - crop_rect.min.y).abs() < d {
                Some(2)
            } else if in_x && (pos.y - crop_rect.max.y).abs() < d {
                Some(3)
            } else {
                None
            }
        };
        if let Some(h) = resp.hover_pos() {
            match near_edge(h) {
                Some(0) | Some(1) => ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal),
                Some(_) => ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical),
                None => ui.ctx().set_cursor_icon(egui::CursorIcon::Grab),
            }
        }
        if resp.drag_started() {
            let pos = resp.interact_pointer_pos().unwrap_or_default();
            self.frame_drag = match near_edge(pos) {
                Some(e) => FrameDrag::Crop(e),
                None if ui.input(|i| i.modifiers.shift) => FrameDrag::Roll,
                None => FrameDrag::Rotate,
            };
        }
        let mut fast = false;
        if resp.dragged() {
            let d = resp.drag_delta();
            let b = self.view.bounds;
            let ux = (b[1] - b[0]) / r.width() as f64;
            let uy = (b[3] - b[2]) / r.height() as f64;
            match self.frame_drag {
                FrameDrag::Rotate => {
                    let s = if self.view.projection == Projection::Rectilinear { 0.6 } else { 1.0 };
                    self.view.yaw -= d.x as f64 * ux * s;
                    self.view.pitch = (self.view.pitch + d.y as f64 * uy * s).clamp(-1.57, 1.57);
                    self.preview_dirty = true;
                    fast = true;
                }
                FrameDrag::Roll => {
                    self.view.roll += d.x as f64 * 0.003;
                    self.preview_dirty = true;
                    fast = true;
                }
                FrameDrag::Crop(e) => {
                    if let Some(p) = resp.interact_pointer_pos() {
                        let fx = ((p.x - r.min.x) / r.width()).clamp(0.0, 1.0) as f64;
                        let fy = ((p.y - r.min.y) / r.height()).clamp(0.0, 1.0) as f64;
                        let c = &mut self.view.crop;
                        match e {
                            0 => c[0] = fx.min(c[1] - 0.02),
                            1 => c[1] = fx.max(c[0] + 0.02),
                            2 => c[2] = fy.min(c[3] - 0.02),
                            _ => c[3] = fy.max(c[2] + 0.02),
                        }
                        self.preview_dirty = true;
                        fast = true;
                    }
                }
                FrameDrag::None => {}
            }
        }
        if resp.drag_stopped() {
            if matches!(self.frame_drag, FrameDrag::Rotate | FrameDrag::Roll) {
                self.update_gains();
            }
            self.frame_drag = FrameDrag::None;
            self.preview_dirty = true;
        }
        if resp.hovered() {
            let s = ui.input(|i| i.smooth_scroll_delta.y);
            if s.abs() > 0.1 {
                let f = (1.0 - s as f64 * 0.0015).clamp(0.8, 1.25);
                let b = &mut self.view.bounds;
                let cx = 0.5 * (b[0] + b[1]);
                let cy = 0.5 * (b[2] + b[3]);
                let lim = self.view.projection.limits();
                b[0] = (cx + (b[0] - cx) * f).max(lim[0]);
                b[1] = (cx + (b[1] - cx) * f).min(lim[1]);
                b[2] = (cy + (b[2] - cy) * f).max(lim[2]);
                b[3] = (cy + (b[3] - cy) * f).min(lim[3]);
                self.preview_dirty = true;
                fast = true;
            }
        }
        if self.preview_dirty || self.preview_tex.is_none() {
            let px_w = (r.width() * ui.ctx().pixels_per_point()) as usize;
            self.request_preview(px_w.max(200), fast);
            ui.ctx().request_repaint();
        }
        if self.preview_rx.is_some() {
            ui.ctx().request_repaint();
        }
    }

    // ----- step 5 -----
    fn ui_export(&mut self, ui: &mut egui::Ui) {
        if !self.aligned {
            ui.label(RichText::new("Align the photos first (or press ✨ Create panorama).").color(MUTED));
            return;
        }
        if self.out_path.is_empty() {
            self.out_path = self.default_out_path("panorama", self.out_format.ext());
        }
        let avail_w = ui.available_width();
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width((avail_w * 0.45).max(380.0));
                card(ui, |ui| {
                    ui.label(RichText::new("Size").strong());
                    let nw = self.natural_width();
                    let cb = self.view.crop_bounds();
                    let asp = (cb[1] - cb[0]) / (cb[3] - cb[2]);
                    ui.add(egui::Slider::new(&mut self.out_scale, 5.0..=100.0).suffix(" %").text("of full detail"));
                    let w = (nw * self.out_scale as f64 / 100.0).round();
                    let h = (w / asp).round();
                    ui.label(format!("{w:.0} × {h:.0} px  ({:.1} megapixels)", w * h / 1e6));
                    {
                        let (_, _, need, free) = self.render_memory();
                        match free {
                            Some(free) if need as f64 > free as f64 * RENDER_MEMORY_SHARE => {
                                let fit = (self.out_scale as f64 * ((free as f64 * RENDER_MEMORY_SHARE) / need as f64).sqrt()).floor().max(1.0);
                                ui.label(
                                    RichText::new(format!(
                                        "Needs about {:.1} GB of memory, only {:.1} GB free – choose at most {:.0} %.",
                                        crate::sysmem::gb(need),
                                        crate::sysmem::gb(free),
                                        fit
                                    ))
                                    .color(BAD),
                                );
                                if ui.button(format!("Use {fit:.0} %")).clicked() {
                                    self.out_scale = fit as f32;
                                }
                            }
                            Some(free) => {
                                ui.label(RichText::new(format!("needs about {:.1} GB of memory ({:.1} GB free)", crate::sysmem::gb(need), crate::sysmem::gb(free))).small().color(MUTED));
                            }
                            None => {}
                        }
                    }
                    ui.horizontal(|ui| {
                        for (t, v) in [("Small", 25.0), ("Medium", 50.0), ("Full", 100.0)] {
                            if ui.button(t).clicked() {
                                self.out_scale = v;
                            }
                        }
                    });
                    if w * h > 150e6 {
                        ui.label(RichText::new("Very large – this needs a lot of memory.").small().color(WARN));
                    }
                });
                ui.add_space(6.0);
                card(ui, |ui| {
                    ui.label(RichText::new("File").strong());
                    ui.horizontal(|ui| {
                        for f in [OutFormat::Jpeg, OutFormat::Png, OutFormat::Tiff] {
                            if ui.add(egui::Button::selectable(self.out_format == f, f.name())).clicked() {
                                self.out_format = f;
                                self.out_path = PathBuf::from(&self.out_path).with_extension(f.ext()).to_string_lossy().to_string();
                            }
                        }
                    });
                    if self.out_format == OutFormat::Jpeg {
                        ui.add(egui::Slider::new(&mut self.jpeg_quality, 50..=100).text("quality"));
                    }
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.out_path).desired_width(ui.available_width() - 90.0));
                        if ui.button("Choose…").clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .add_filter(self.out_format.name(), &[self.out_format.ext()])
                                .set_file_name(format!("panorama.{}", self.out_format.ext()))
                                .save_file()
                            {
                                self.out_path = p.to_string_lossy().to_string();
                            }
                        }
                    });
                });
                ui.add_space(6.0);
                card(ui, |ui| {
                    ui.label(RichText::new("Blending").strong());
                    ui.radio_value(&mut self.blend, Blend::Smooth, "Smooth seams (recommended) – hides seams at every detail level");
                    ui.radio_value(&mut self.blend, Blend::Feather, "Soft fade – faster, can show ghosting with moving subjects");
                    ui.checkbox(&mut self.match_brightness, "Match brightness and colour between photos");
                    ui.horizontal(|ui| {
                        ui.label("Seam transition (detail levels)");
                        egui::ComboBox::from_id_salt("blend_levels")
                            .selected_text(if self.blend_levels == 0 { "Automatic (wide, as before)".to_string() } else { format!("{} (narrow ≈ {} px)", self.blend_levels, 1usize << self.blend_levels) })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.blend_levels, 0, "Automatic (wide, as before)");
                                for l in 2..=7usize {
                                    ui.selectable_value(&mut self.blend_levels, l, format!("{} (≈ {} px)", l, 1usize << l));
                                }
                            });
                    });
                    ui.checkbox(&mut self.cubic_interp, "Sharper resampling (bicubic instead of bilinear)");
                    if ui
                        .checkbox(&mut self.prefer_detail, "Use the sharpest photo where photos overlap")
                        .on_hover_text("Where photos lie on top of each other, the one with the most real detail (zoomed in or higher resolution) is used. Off: seams in the middle of the overlaps, as before.")
                        .changed()
                    {
                        self.preview_dirty = true;
                    }
                });
                ui.add_space(10.0);
                if primary_button(ui, "💾  Create panorama file", !self.busy()).clicked() {
                    self.run_render();
                }
                if let Some(p) = self.last_output.clone() {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("✔ Saved: {}", p.display())).color(GOOD));
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Open picture").clicked() {
                            let _ = open::that(&p);
                        }
                        if ui.button("Show in folder").clicked() {
                            if let Some(d) = p.parent() {
                                let _ = open::that(d);
                            }
                        }
                    });
                }
            });
            ui.add_space(12.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("Preview").strong());
                if let Some(t) = &self.preview_tex {
                    let w = ui.available_width();
                    let c = self.view.crop;
                    let asp = self.view.aspect() as f32 * ((c[1] - c[0]) / (c[3] - c[2])) as f32;
                    let size = Vec2::new(w, w / asp);
                    let uv = Rect::from_min_max(Pos2::new(c[0] as f32, c[2] as f32), Pos2::new(c[1] as f32, c[3] as f32));
                    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
                    ui.painter().image(t.id(), rect, uv, Color32::WHITE);
                } else {
                    ui.label(RichText::new("Open step 4 once to build the preview.").color(MUTED));
                    self.preview_dirty = true;
                    self.request_preview(1400, false);
                }
            });
        });
    }

    // ----- exposure mode -----
    fn ui_exposure(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("brackets")
            .resizable(true)
            .default_size(250.0)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(egui::Margin::same(10)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Exposures").strong().size(16.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(!self.busy(), egui::Button::new("➕ Add")).clicked() {
                            self.pick_photos();
                        }
                    });
                });
                let mut remove = None;
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for (i, b) in self.brackets.iter().enumerate() {
                        egui::Frame::new().fill(CARD).corner_radius(CornerRadius::same(8)).inner_margin(egui::Margin::same(6)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let s = b.thumb.size_vec2();
                                ui.add(egui::Image::new(egui::load::SizedTexture::new(b.thumb.id(), Vec2::new(72.0, 72.0 * s.y / s.x))).corner_radius(4.0));
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(&b.name).strong());
                                    if let Some(t) = b.info.exposure_time {
                                        ui.label(RichText::new(if t < 1.0 { format!("1/{:.0} s", 1.0 / t) } else { format!("{t:.1} s") }).small().color(MUTED));
                                    }
                                    if ui.small_button("🗑").clicked() {
                                        remove = Some(i);
                                    }
                                });
                            });
                        });
                    }
                });
                if let Some(i) = remove {
                    self.brackets.remove(i);
                    self.fusion_dirty = true;
                }
            });
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::same(14))).show(ui, |ui| {
            if self.brackets.is_empty() {
                ui.add_space(60.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Blend a bracketed series into one picture").size(22.0).color(Color32::WHITE));
                    ui.label(RichText::new("Add 2–9 photos of the same view taken with different exposures. Dark areas come from the bright shots, bright areas from the dark shots – no halos, no tone mapping needed.").color(MUTED));
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        ui.add_space((ui.available_width() - 470.0).max(0.0) / 2.0);
                        if primary_button(ui, "➕  Add exposures…", !self.busy()).clicked() {
                            self.pick_photos();
                        }
                        if ui.add_enabled(!self.busy(), egui::Button::new("🖼  Try with sample exposures").min_size(Vec2::new(0.0, 34.0))).clicked() {
                            self.load_bracket_sample();
                        }
                    });
                });
                return;
            }
            ui.horizontal(|ui| {
                let mut ch = false;
                ch |= ui.checkbox(&mut self.fusion_align, "Align handheld shots").changed();
                ui.separator();
                ch |= ui.add(egui::Slider::new(&mut self.fusion_weights.exposure, 0.0..=3.0).text("good exposure")).changed();
                ch |= ui.add(egui::Slider::new(&mut self.fusion_weights.contrast, 0.0..=3.0).text("detail")).changed();
                ch |= ui.add(egui::Slider::new(&mut self.fusion_weights.saturation, 0.0..=3.0).text("colour")).changed();
                if ch {
                    self.fusion_dirty = true;
                }
            });
            ui.horizontal(|ui| {
                if self.fusion_out_path.is_empty() {
                    self.fusion_out_path = self.default_out_path("blended", "jpg");
                }
                ui.add(egui::TextEdit::singleline(&mut self.fusion_out_path).desired_width(420.0));
                if ui.button("Choose…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().add_filter("Pictures", &["jpg", "png", "tif"]).set_file_name("blended.jpg").save_file() {
                        self.fusion_out_path = p.to_string_lossy().to_string();
                    }
                }
                if primary_button(ui, "💾  Save blended picture", !self.busy() && self.brackets.len() >= 2).clicked() {
                    self.run_fusion_export();
                }
                if self.fusion_rx.is_some() {
                    ui.add(egui::Spinner::new());
                    ui.ctx().request_repaint();
                }
            });
            ui.add_space(6.0);
            let avail = ui.available_size();
            if let Some(t) = &self.fusion_tex {
                let s = t.size_vec2();
                let sc = (avail.x / s.x).min((avail.y - 4.0) / s.y);
                let (rect, _) = ui.allocate_exact_size(avail, Sense::hover());
                let r = Rect::from_center_size(rect.center(), s * sc);
                ui.painter().image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            } else if self.brackets.len() < 2 {
                ui.label(RichText::new("Add at least two exposures.").color(MUTED));
            }
        });
    }

    /// Position of a control point side in pixels of the full-size photo.
    fn full_px(&self, i: usize, p: [f64; 2]) -> (f64, f64) {
        let (w, h) = (self.photos[i].w as f64, self.photos[i].h as f64);
        let l = w.max(h);
        (p[0] * l + w * 0.5 - 0.5, p[1] * l + h * 0.5 - 0.5)
    }

    /// Show a point in the editor: open its pair, select it and zoom in on it.
    fn jump_to_point(&mut self, k: usize) {
        let c = self.cps[k];
        self.step = Step::Points;
        self.pe.a = c.a;
        self.pe.b = c.b;
        self.pe.selected = Some(k);
        self.pe.pending = None;
        let pa = self.to_px(c.a, c.pa);
        let pb = self.to_px(c.b, c.pb);
        self.pe.views[0] = PaneView { zoom: 4.0, center: Vec2::new(pa.0 as f32 + 0.5, pa.1 as f32 + 0.5), key: c.a + 1 };
        self.pe.views[1] = PaneView { zoom: 4.0, center: Vec2::new(pb.0 as f32 + 0.5, pb.1 as f32 + 0.5), key: c.b + 1 };
    }

    fn cp_list_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_cp_list;
        let mut jump: Option<usize> = None;
        let mut delete: Vec<usize> = Vec::new();
        egui::Window::new("All control points")
            .open(&mut open)
            .default_pos([300.0, 140.0])
            .default_size([760.0, 520.0])
            .resizable(true)
            .show(ctx, |ui| {
                // filter
                let (pa, pb) = (self.pe.a, self.pe.b);
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut self.cp_only_pair, format!("only photos {} and {}", pa + 1, pb + 1));
                    ui.add_enabled_ui(self.aligned, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("error at least");
                            ui.add(egui::DragValue::new(&mut self.cp_min_err).range(0.0..=1000.0).speed(0.1).suffix(" px"));
                        });
                    });
                });
                let cache_key = format!(
                    "{}-{}-{}-{}-{}-{}-{}-{}",
                    self.cps.len(),
                    self.align_version,
                    self.cp_sort.0 as u8,
                    self.cp_sort.1,
                    self.cp_only_pair,
                    pa,
                    pb,
                    self.cp_min_err
                );
                let cached = self.cp_rows_cache.as_ref().filter(|c| c.0 == cache_key).map(|c| (c.1.clone(), c.2.clone()));
                let (rows, pair_no) = if let Some(c) = cached { c } else {
                let mut rows: Vec<usize> = (0..self.cps.len())
                    .filter(|&k| {
                        let c = &self.cps[k];
                        let pair_ok = !self.cp_only_pair || (c.a == pa && c.b == pb) || (c.a == pb && c.b == pa);
                        let err_ok = !self.aligned || c.err >= self.cp_min_err;
                        pair_ok && err_ok
                    })
                    .collect();
                // number of each point within its photo pair (the original's "P CP#" column)
                let mut pair_no = vec![0usize; self.cps.len()];
                {
                    let mut cnt: std::collections::HashMap<(usize, usize), usize> = std::collections::HashMap::new();
                    for (k, c) in self.cps.iter().enumerate() {
                        let e = cnt.entry((c.a.min(c.b), c.a.max(c.b))).or_insert(0);
                        pair_no[k] = *e;
                        *e += 1;
                    }
                }
                let (col, asc) = self.cp_sort;
                rows.sort_by(|&x, &y| {
                    let (c1, c2) = (&self.cps[x], &self.cps[y]);
                    let o = match col {
                        CpCol::Index => x.cmp(&y),
                        CpCol::PhotoA => (c1.a, c1.b).cmp(&(c2.a, c2.b)),
                        CpCol::PhotoB => (c1.b, c1.a).cmp(&(c2.b, c2.a)),
                        CpCol::Error => c1.err.partial_cmp(&c2.err).unwrap_or(std::cmp::Ordering::Equal),
                        CpCol::Kind => c1.manual.cmp(&c2.manual),
                        CpCol::PairIndex => pair_no[x].cmp(&pair_no[y]),
                        CpCol::Match => c1.score.partial_cmp(&c2.score).unwrap_or(std::cmp::Ordering::Equal),
                    };
                    if asc { o } else { o.reverse() }
                });
                self.cp_rows_cache = Some((cache_key, rows.clone(), pair_no.clone()));
                (rows, pair_no)
                };
                let shown_err: Vec<f64> = rows.iter().map(|&k| self.cps[k].err).collect();
                let mean = if shown_err.is_empty() { 0.0 } else { shown_err.iter().sum::<f64>() / shown_err.len() as f64 };
                ui.label(
                    RichText::new(if self.aligned {
                        format!("{} of {} points shown · average error {:.2} px · click a row to show it in the editor", rows.len(), self.cps.len(), mean)
                    } else {
                        format!("{} of {} points shown · errors appear after aligning (step 3)", rows.len(), self.cps.len())
                    })
                    .small()
                    .color(MUTED),
                );
                ui.horizontal(|ui| {
                    if ui.add_enabled(self.pe.selected.is_some(), egui::Button::new("🗑 Delete selected")).clicked() {
                        if let Some(k) = self.pe.selected {
                            delete.push(k);
                        }
                    }
                    if ui.add_enabled(!rows.is_empty(), egui::Button::new(format!("🗑 Delete all {} listed", rows.len()))).clicked() {
                        delete.extend(rows.iter().copied());
                    }
                    if ui
                        .add_enabled(self.aligned, egui::Button::new("🧹 Remove outliers (mean + 2σ)"))
                        .on_hover_text("Removes points whose error is larger than average + 2 × spread, first per photo pair, then for the whole project.")
                        .clicked()
                    {
                        let rm = engine::statistical_outliers(&self.cps);
                        delete.extend(rm.iter().enumerate().filter(|(_, r)| **r).map(|(k, _)| k));
                    }
                });
                ui.separator();
                // header
                let widths = [60.0f32, 150.0, 150.0, 60.0, 90.0, 70.0, 90.0, 210.0];
                let titles = ["#", "Photo A", "Photo B", "Pair #", "Error", "Match", "Type", "Position A → B (px)"];
                let cols = [CpCol::Index, CpCol::PhotoA, CpCol::PhotoB, CpCol::PairIndex, CpCol::Error, CpCol::Match, CpCol::Kind];
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    for (i, t) in titles.iter().enumerate() {
                        if i < cols.len() {
                            let arrow = if self.cp_sort.0 == cols[i] { if self.cp_sort.1 { " ▲" } else { " ▼" } } else { "" };
                            let b = egui::Button::new(RichText::new(format!("{t}{arrow}")).strong()).frame(false).min_size(Vec2::new(widths[i], 22.0));
                            if ui.add_sized([widths[i], 22.0], b).clicked() {
                                if self.cp_sort.0 == cols[i] {
                                    self.cp_sort.1 = !self.cp_sort.1;
                                } else {
                                    // errors: worst first; everything else ascending
                                    self.cp_sort = (cols[i], cols[i] != CpCol::Error);
                                }
                            }
                        } else {
                            ui.add_sized([widths[i], 22.0], egui::Label::new(RichText::new(*t).strong()));
                        }
                    }
                });
                let row_h = 22.0;
                let total_w: f32 = widths.iter().sum();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_h, rows.len(), |ui, range| {
                    for r in range {
                        let k = rows[r];
                        let c = self.cps[k];
                        let (rect, resp) = ui.allocate_exact_size(Vec2::new(total_w, row_h), Sense::click());
                        let sel = self.pe.selected == Some(k);
                        let p = ui.painter();
                        if sel {
                            p.rect_filled(rect, 3.0, ACCENT_DARK);
                        } else if resp.hovered() {
                            p.rect_filled(rect, 3.0, Color32::from_rgb(48, 52, 61));
                        } else if r % 2 == 1 {
                            p.rect_filled(rect, 0.0, Color32::from_rgb(32, 34, 40));
                        }
                        let fa = self.full_px(c.a, c.pa);
                        let fb = self.full_px(c.b, c.pb);
                        let err_txt = if self.aligned { format!("{:.2} px", c.err) } else { "—".into() };
                        let err_col = if self.aligned { err_color(c.err) } else { MUTED };
                        let cells: [(String, Color32); 8] = [
                            (format!("{}", k + 1), MUTED),
                            (format!("{}. {}", c.a + 1, self.photos[c.a].name), Color32::WHITE),
                            (format!("{}. {}", c.b + 1, self.photos[c.b].name), Color32::WHITE),
                            (format!("{}", pair_no[k] + 1), MUTED),
                            (err_txt, err_col),
                            (
                                if c.score > 0.0 { format!("{:.3}", c.score) } else { "—".into() },
                                if c.score <= 0.0 { MUTED } else if c.score >= 0.8 { GOOD } else { WARN },
                            ),
                            (
                                match (c.kind, c.manual) {
                                    (CpKind::Area, _) => "area".into(),
                                    (CpKind::AreaManual, _) => "area (hand)".into(),
                                    (_, true) => "by hand".into(),
                                    _ => "auto".into(),
                                },
                                if c.manual { ACCENT } else { MUTED },
                            ),
                            (format!("{:.1}, {:.1} → {:.1}, {:.1}", fa.0, fa.1, fb.0, fb.1), MUTED),
                        ];
                        let mut x = rect.min.x + 4.0;
                        for (i, (t, col)) in cells.iter().enumerate() {
                            let clip = Rect::from_min_size(Pos2::new(x, rect.min.y), Vec2::new(widths[i] - 6.0, row_h));
                            p.with_clip_rect(clip).text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, t, FontId::proportional(13.0), *col);
                            x += widths[i];
                        }
                        if resp.clicked() {
                            jump = Some(k);
                        }
                    }
                });
            });
        self.show_cp_list = open;
        if let Some(k) = jump {
            self.jump_to_point(k);
        }
        if !delete.is_empty() {
            delete.sort_unstable();
            delete.dedup();
            for k in delete.into_iter().rev() {
                if k < self.cps.len() {
                    self.cps.remove(k);
                }
            }
            self.pe.selected = None;
            self.needs_align = true;
            self.set_status("Points deleted. Align again (step 3) to update.", MUTED);
        }
    }

    /// Overlap check: photos A and B of the point editor laid over each other in the shared
    /// panorama space (blend with slider, difference heat map, checkerboard, shift arrows).
    fn overlap_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_overlap;
        let (a, b) = (self.pe.a, self.pe.b);
        let mut redraw = false;
        egui::Window::new(format!("Overlap check – photo {} and {}", a + 1, b + 1))
            .open(&mut open)
            .default_size([820.0, 640.0])
            .default_pos([280.0, 110.0])
            .resizable(true)
            .show(ctx, |ui| {
                if !self.aligned || a >= self.photos.len() || b >= self.photos.len() || a == b {
                    ui.label(RichText::new("Align the photos first (step 3).").color(MUTED));
                    return;
                }
                ui.horizontal_wrapped(|ui| {
                    for (m, t) in [(0u8, "Blend"), (1, "Difference"), (2, "Checkerboard")] {
                        if ui.add(egui::Button::selectable(self.ov_mode == m, t)).clicked() {
                            self.ov_mode = m;
                            redraw = true;
                        }
                    }
                    ui.separator();
                    match self.ov_mode {
                        0 => {
                            redraw |= ui.add(egui::Slider::new(&mut self.ov_alpha, 0.0..=1.0).text("A – B")).changed();
                        }
                        2 => {
                            redraw |= ui.add(egui::Slider::new(&mut self.ov_tile, 4..=128).text("tile px")).changed();
                        }
                        _ => {
                            ui.label(RichText::new("bright = the photos differ").small().color(MUTED));
                        }
                    }
                    ui.separator();
                    redraw |= ui.add(egui::Slider::new(&mut self.ov_zoom, 0.25..=8.0).logarithmic(true).text("zoom")).changed();
                    ui.checkbox(&mut self.ov_arrows, "shift arrows");
                    if ui.button("⟲ Centre").clicked() {
                        self.ov_center = None;
                        redraw = true;
                    }
                });
                let geo = |i: usize| crate::fine_align::Geo {
                    lens: self.lenses[self.photos[i].lens],
                    rot: self.photos[i].rot,
                    w: self.photos[i].work.width() as f64,
                    h: self.photos[i].work.height() as f64,
                };
                let (ga, gb) = (geo(a), geo(b));
                // centre: middle of the pair's control points (as seen from A)
                if self.ov_center.map(|c| c.0 != (a, b)).unwrap_or(true) {
                    let dirs: Vec<V3> = self
                        .cps
                        .iter()
                        .filter(|c| (c.a == a && c.b == b) || (c.a == b && c.b == a))
                        .map(|c| if c.a == a { ga.px_to_dir(self.to_px(a, c.pa)) } else { ga.px_to_dir(self.to_px(a, c.pb)) })
                        .collect();
                    let d = if dirs.is_empty() {
                        let ca = mtv(&ga.rot, [0.0, 0.0, 1.0]);
                        let cb = mtv(&gb.rot, [0.0, 0.0, 1.0]);
                        normalize([ca[0] + cb[0], ca[1] + cb[1], ca[2] + cb[2]])
                    } else {
                        let s = dirs.iter().fold([0.0; 3], |m, d| [m[0] + d[0], m[1] + d[1], m[2] + d[2]]);
                        normalize(s)
                    };
                    self.ov_center = Some(((a, b), d));
                    redraw = true;
                }
                let center = self.ov_center.unwrap().1;
                let scale = ga.pixel_angle().max(gb.pixel_angle()) / self.ov_zoom as f64;
                let plane = crate::fine_align::Plane::new(center, &ga.rot, scale);
                let avail = ui.available_size() - Vec2::new(0.0, 96.0);
                let ppp = ui.ctx().pixels_per_point();
                let (rw, rh) = (((avail.x * ppp) as usize).clamp(64, 1600), ((avail.y.max(120.0) * ppp) as usize).clamp(64, 1200));
                let key = format!("{a}-{b}-{}-{:.3}-{}-{:.3}-{:?}-{rw}x{rh}-{}", self.ov_mode, self.ov_alpha, self.ov_tile, self.ov_zoom, center, self.align_version);
                if redraw || key != self.ov_key || self.ov_tex.is_none() {
                    let rp: Vec<RenderPhoto> = [a, b]
                        .iter()
                        .map(|&i| RenderPhoto {
                            img: self.photos[i].work.clone(),
                            w: self.photos[i].work.width() as f64,
                            h: self.photos[i].work.height() as f64,
                            rot: self.photos[i].rot,
                            lens: self.lenses[self.photos[i].lens],
                            gain: if self.match_brightness { self.gains.get(i).copied().unwrap_or([1.0; 3]) } else { [1.0; 3] },
                            field: self.fields.get(i).cloned().flatten(),
                            full_long: self.photos[i].long(),
                            trusted: true,
                        })
                        .collect();
                    let (mode, alpha, tile) = (self.ov_mode, self.ov_alpha, self.ov_tile.max(1));
                    let mut rgba = vec![0u8; rw * rh * 4];
                    use rayon::prelude::*;
                    rgba.par_chunks_mut(rw * 4).enumerate().for_each(|(y, row)| {
                        for x in 0..rw {
                            let d = plane.dir(x as f64 - rw as f64 * 0.5, y as f64 - rh as f64 * 0.5);
                            let s = |p: &RenderPhoto| -> Option<[f32; 3]> {
                                let (px, py, _) = p.lookup(d)?;
                                let c = imgutil::sample_rgb_linear(&p.img, px, py);
                                Some([c[0] * p.gain[0], c[1] * p.gain[1], c[2] * p.gain[2]])
                            };
                            let (ca, cb) = (s(&rp[0]), s(&rp[1]));
                            let lin = match (ca, cb) {
                                (Some(u), Some(v)) => match mode {
                                    0 => [u[0] * (1.0 - alpha) + v[0] * alpha, u[1] * (1.0 - alpha) + v[1] * alpha, u[2] * (1.0 - alpha) + v[2] * alpha],
                                    1 => {
                                        let g = |c: [f32; 3]| imgutil::linear_to_srgb(0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]);
                                        let t = ((g(u) - g(v)).abs() * 4.0).min(1.0);
                                        // heat map: black - red - yellow - white
                                        let r = (t * 3.0).min(1.0);
                                        let gg = (t * 3.0 - 1.0).clamp(0.0, 1.0);
                                        let bb = (t * 3.0 - 2.0).clamp(0.0, 1.0);
                                        let o = x * 4;
                                        row[o] = (r * 255.0) as u8;
                                        row[o + 1] = (gg * 255.0) as u8;
                                        row[o + 2] = (bb * 255.0) as u8;
                                        row[o + 3] = 255;
                                        continue;
                                    }
                                    _ => {
                                        if ((x / tile) + (y / tile)) % 2 == 0 { u } else { v }
                                    }
                                },
                                (Some(u), None) | (None, Some(u)) => [u[0] * 0.35, u[1] * 0.35, u[2] * 0.35],
                                (None, None) => [0.02, 0.02, 0.025],
                            };
                            let o = x * 4;
                            for k in 0..3 {
                                row[o + k] = (imgutil::linear_to_srgb(lin[k]) * 255.0) as u8;
                            }
                            row[o + 3] = 255;
                        }
                    });
                    let img = egui::ColorImage::from_rgba_unmultiplied([rw, rh], &rgba);
                    match &mut self.ov_tex {
                        Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                        None => self.ov_tex = Some(ui.ctx().load_texture("overlap", img, egui::TextureOptions::LINEAR)),
                    }
                    self.ov_key = key;
                }
                let size = Vec2::new(rw as f32 / ppp, rh as f32 / ppp);
                let (resp, painter) = ui.allocate_painter(size, Sense::drag());
                let r = resp.rect;
                if let Some(t) = &self.ov_tex {
                    painter.image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                }
                // drag to move around
                if resp.dragged() {
                    let dlt = resp.drag_delta() * ppp;
                    let nd = plane.dir(-dlt.x as f64, -dlt.y as f64);
                    self.ov_center = Some(((a, b), nd));
                }
                // arrows: measured remaining shift of points and areas (×10), colour = confidence
                if self.ov_arrows {
                    for c in self.cps.iter().filter(|c| (c.a == a && c.b == b) || (c.a == b && c.b == a)) {
                        if c.conf <= 0.0 && c.shift == [0.0, 0.0] {
                            continue;
                        }
                        let pa = if c.a == a { c.pa } else { c.pb };
                        let d = ga.px_to_dir(self.to_px(a, pa));
                        let Some((u, v)) = plane.coords(d) else { continue };
                        let p = r.min + Vec2::new(((u + rw as f64 * 0.5) / ppp as f64) as f32, ((v + rh as f64 * 0.5) / ppp as f64) as f32);
                        if !r.contains(p) {
                            continue;
                        }
                        let sh = Vec2::new(c.shift[0], c.shift[1]) * (10.0 * self.ov_zoom / ppp);
                        let t = c.conf.clamp(0.0, 1.0);
                        let col = Color32::from_rgb((255.0 * (1.0 - t)) as u8, (255.0 * t) as u8, 60);
                        painter.arrow(p, sh, Stroke::new(1.5, col));
                        if c.kind != CpKind::Point {
                            painter.rect_stroke(Rect::from_center_size(p, Vec2::splat(5.0)), 0.0, Stroke::new(1.0, col), StrokeKind::Middle);
                        }
                    }
                }
                ui.label(RichText::new("Drag to move · arrows show the remaining shift ×10 (green = sure, red = unsure) · squares = control areas").small().color(MUTED));
                if let Some(st) = &self.last_fine {
                    ui.label(RichText::new(fine_summary(st)).small());
                }
            });
        self.show_overlap = open;
    }

    /// Connection view: graph of the photos (edges = pairs with points) and a warning list.
    fn graph_window(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let n = self.photos.len();
        let active = self.active_indices();
        let act_cps: Vec<ControlPoint> = self.cps.iter().filter(|c| active.contains(&c.a) && active.contains(&c.b)).copied().collect();
        let counts = crate::pair_check::pair_counts(&act_cps);
        let ovs: Vec<crate::pair_check::PairOverlap> = self.pair_summary.as_ref().map(|s| s.overlaps.clone()).unwrap_or_default();
        let min_pts = self.pair_cfg.min_points;
        let enough = self.pair_cfg.enough_points.max(1);
        // edges: pairs with points, and overlapping pairs
        let mut edges: Vec<(usize, usize, usize, Option<f32>)> = counts.iter().map(|(&(a, b), &c)| (a, b, c, None)).collect();
        for o in &ovs {
            if !active.contains(&o.a) || !active.contains(&o.b) {
                continue;
            }
            match edges.iter_mut().find(|e| e.0 == o.a && e.1 == o.b) {
                Some(e) => e.3 = Some(o.share),
                None => edges.push((o.a, o.b, 0, Some(o.share))),
            }
        }
        edges.sort_by_key(|e| (e.0, e.1));
        let linked: Vec<bool> = (0..n).map(|i| counts.keys().any(|&(a, b)| a == i || b == i)).collect();
        // every photo counts, a photo without points is a group of its own (as in the original)
        let groups = image_groups(n, &active, &act_cps);
        let mut action: Option<(u8, usize, usize)> = None;
        egui::Window::new("Connections between the photos")
            .open(&mut open)
            .default_size([760.0, 640.0])
            .frame(egui::Frame::window(&ctx.global_style()).fill(PANEL))
            .show(ctx, |ui| {
            if !self.aligned {
                ui.label(RichText::new("Not aligned yet: edges show pairs with points only. After aligning, overlapping pairs without enough points are shown red and dashed.").small().color(WARN));
            }
            if groups.len() > 1 {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("⚠ Warning {} unconnected image groups found: {}", groups.len(), groups_text(&groups))).color(BAD).strong());
                    if ui
                        .button(format!("Join photos {} and {}", groups[0][0] + 1, groups[1][0] + 1))
                        .on_hover_text("Opens the first photo of the first two groups in the point editor with the patch click mode (the original opens the same pair)")
                        .clicked()
                    {
                        action = Some((2, groups[0][0], groups[1][0]));
                    }
                });
            }
            // --- graph ---
            let size = Vec2::new(ui.available_width(), 330.0);
            let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 6.0, Color32::from_rgb(24, 26, 31));
            let inner = rect.shrink(28.0);
            // positions: from the alignment (direction of each photo's centre), else a circle
            let main_group: Vec<usize> = groups.iter().max_by_key(|g| g.len()).cloned().unwrap_or_default();
            let pos: Vec<Pos2> = if self.aligned && main_group.len() >= 2 {
                // the biggest group where the alignment puts it; all other photos in a row below
                let row_y = inner.max.y;
                let graph = Rect::from_min_max(inner.min, Pos2::new(inner.max.x, inner.max.y - 36.0));
                let dirs: Vec<(f64, f64)> = (0..n)
                    .map(|i| {
                        let d = mtv(&self.photos[i].rot, [0.0, 0.0, 1.0]);
                        (d[0].atan2(d[2]), d[1].clamp(-1.0, 1.0).asin())
                    })
                    .collect();
                let md: Vec<(f64, f64)> = main_group.iter().map(|&i| dirs[i]).collect();
                let (x0, x1) = md.iter().fold((f64::MAX, f64::MIN), |m, d| (m.0.min(d.0), m.1.max(d.0)));
                let (y0, y1) = md.iter().fold((f64::MAX, f64::MIN), |m, d| (m.0.min(d.1), m.1.max(d.1)));
                let sx = (x1 - x0).max(1e-6);
                let sy = (y1 - y0).max(1e-6);
                // spread horizontally and vertically on their own (photos of a row would sit on one line)
                let (kx, ky) = (graph.width() as f64 / sx, graph.height() as f64 / sy.max(sx * 0.15));
                let others: Vec<usize> = (0..n).filter(|i| !main_group.contains(i)).collect();
                (0..n)
                    .map(|i| {
                        if let Some(k) = others.iter().position(|&o| o == i) {
                            return Pos2::new(inner.min.x + (k as f32 + 0.5) * inner.width() / others.len().max(1) as f32, row_y);
                        }
                        let d = dirs[i];
                        // zig-zag neighbours vertically when the photos lie in one row
                        let jitter = if sy < sx * 0.15 { if main_group.iter().position(|&m| m == i).unwrap_or(0) % 2 == 0 { -0.25 } else { 0.25 } } else { 0.0 };
                        graph.center() + Vec2::new(((d.0 - (x0 + x1) * 0.5) * kx) as f32, ((d.1 - (y0 + y1) * 0.5) * ky) as f32 + jitter * graph.height())
                    })
                    .collect()
            } else {
                (0..n)
                    .map(|i| {
                        let t = i as f32 / n.max(1) as f32 * std::f32::consts::TAU;
                        inner.center() + Vec2::new(t.cos() * inner.width() * 0.45, t.sin() * inner.height() * 0.45)
                    })
                    .collect()
            };
            for &(a, b, c, ov) in &edges {
                let (pa, pb) = (pos[a], pos[b]);
                let weak = ov.map(|o| o >= self.pair_cfg.min_overlap).unwrap_or(false) && c < min_pts;
                if weak {
                    painter.extend(egui::Shape::dashed_line(&[pa, pb], Stroke::new(2.0, BAD), 6.0, 4.0));
                } else {
                    let col = if c >= enough { GOOD } else { WARN };
                    painter.line_segment([pa, pb], Stroke::new(if c >= enough { 2.0 } else { 1.5 }, col));
                }
                let mid = pa + (pb - pa) * 0.5;
                if pa.distance(pb) < GRAPH_LABEL_MIN_LEN {
                    continue;
                }
                let label = match ov {
                    Some(o) => format!("{c} · {:.0}%", o * 100.0),
                    None => c.to_string(),
                };
                painter.text(mid, Align2::CENTER_CENTER, label, FontId::proportional(10.0), Color32::from_gray(220));
            }
            for &i in &active {
                let p = pos[i];
                let col = if linked[i] { ACCENT_DARK } else { BAD };
                painter.circle_filled(p, 13.0, col);
                if !linked[i] {
                    painter.circle_stroke(p, 17.0, Stroke::new(2.5, BAD));
                }
                if self.graph_photo == Some(i) {
                    painter.circle_stroke(p, 15.0, Stroke::new(2.0, Color32::WHITE));
                }
                painter.text(p, Align2::CENTER_CENTER, (i + 1).to_string(), FontId::proportional(12.0), Color32::WHITE);
            }
            if resp.clicked() {
                if let Some(hp) = resp.interact_pointer_pos() {
                    let node = active.iter().copied().filter(|&i| pos[i].distance(hp) < 15.0).min_by(|&x, &y| pos[x].distance(hp).partial_cmp(&pos[y].distance(hp)).unwrap());
                    if let Some(i) = node {
                        self.graph_photo = Some(i);
                    } else {
                        // nearest edge
                        let dist = |p: Pos2, a: Pos2, b: Pos2| -> f32 {
                            let ab = b - a;
                            let t = ((p - a).dot(ab) / ab.length_sq().max(1e-6)).clamp(0.0, 1.0);
                            (a + ab * t).distance(p)
                        };
                        if let Some(e) = edges.iter().filter(|e| dist(hp, pos[e.0], pos[e.1]) < 6.0).min_by(|x, y| dist(hp, pos[x.0], pos[x.1]).partial_cmp(&dist(hp, pos[y.0], pos[y.1])).unwrap()) {
                            action = Some((0, e.0, e.1));
                        }
                    }
                }
            }
            ui.label(RichText::new(format!("green: {enough}+ points · yellow: fewer · red dashed: overlap {:.0} %+ but fewer than {min_pts} points · red ring: no points at all. Click a line to open the pair, a photo to see it.", self.pair_cfg.min_overlap * 100.0)).small().color(MUTED));
            if let Some(i) = self.graph_photo {
                ui.horizontal(|ui| {
                    let t = &self.photos[i].thumb;
                    let s = t.size_vec2();
                    let k = (120.0 / s.y.max(1.0)).min(220.0 / s.x.max(1.0));
                    ui.image((t.id(), s * k));
                    ui.vertical(|ui| {
                        ui.label(RichText::new(format!("{}. {}", i + 1, self.photos[i].name)).strong());
                        let partners: Vec<String> = counts.iter().filter(|(k, _)| k.0 == i || k.1 == i).map(|(k, c)| format!("{} ({c})", if k.0 == i { k.1 + 1 } else { k.0 + 1 })).collect();
                        ui.label(if partners.is_empty() { "no points to any photo".to_string() } else { format!("points with: {}", partners.join(", ")) });
                    });
                });
            }
            ui.separator();
            // --- warning list ---
            ui.label(RichText::new("Warnings").strong());
            egui::ScrollArea::vertical().id_salt("graph_warn").max_height(200.0).show(ui, |ui| {
                let mut any = false;
                for &i in &active {
                    if !linked[i] {
                        any = true;
                        ui.label(RichText::new(format!("Photo {} ({}) has no points to any other photo.", i + 1, self.photos[i].name)).color(BAD));
                    }
                }
                for &(a, b, c, ov) in &edges {
                    let Some(o) = ov else { continue };
                    if o < self.pair_cfg.min_overlap || c >= min_pts {
                        continue;
                    }
                    any = true;
                    let tried = self
                        .pair_summary
                        .as_ref()
                        .and_then(|s| s.reports.iter().find(|r| r.a == a && r.b == b))
                        .map(|r| if r.steps.is_empty() { "nothing tried yet".to_string() } else { r.steps.iter().map(|s| format!("{} → {}", s.step, s.points)).collect::<Vec<_>>().join(", ") })
                        .unwrap_or_else(|| "nothing tried yet".into());
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format!("Photos {} – {}: overlap {:.0} %, {c} points · tried: {tried}", a + 1, b + 1, o * 100.0));
                        if ui.add_enabled(!self.busy(), egui::Button::new("Search again")).clicked() {
                            action = Some((1, a, b));
                        }
                        if ui.button("Manual").on_hover_text("Opens the pair with the patch click mode").clicked() {
                            action = Some((2, a, b));
                        }
                    });
                }
                if !any {
                    ui.label(RichText::new("No warnings: every photo has points, and every overlapping pair has enough.").color(GOOD));
                }
            });
        });
        if !open {
            self.show_graph = false;
        }
        match action {
            Some((0, a, b)) | Some((2, a, b)) => {
                let manual = matches!(action, Some((2, _, _)));
                self.pe.a = a;
                self.pe.b = b;
                self.pe.selected = None;
                self.pe.pending = None;
                self.step = Step::Points;
                if manual {
                    self.click.on = true;
                    self.click.a = None;
                    self.click.b = None;
                    self.click.result = None;
                    self.pe.adding = false;
                }
            }
            Some((1, a, b)) => self.run_pair_recheck(a, b),
            _ => {}
        }
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_help;
        egui::Window::new("How PanoWeave works").open(&mut open).collapsible(false).default_width(520.0).show(ctx, |ui| {
            ui.label(RichText::new("Panorama").strong());
            ui.label("1. Photos – add overlapping photos taken from one spot (JPEG, PNG, TIFF, camera RAW).");
            ui.label("2. Matching points – spots that appear in two photos. Found automatically; you can add, move or delete them. Clicking a spot in the left photo finds the partner spot for you.");
            ui.label("3. Align – works out where each photo was pointing, the lens field of view and distortion. Wrong matches are removed automatically.");
            ui.label("4. Frame & crop – choose the projection, drag to move the view, scroll to zoom, drag the white frame to crop.");
            ui.label("5. Save – choose size and file type. Smooth seams blend every detail level separately so seams disappear.");
            ui.add_space(6.0);
            ui.label(RichText::new("Exposure blend").strong());
            ui.label("Merge photos of the same view taken at different exposures into one evenly lit picture.");
            ui.add_space(6.0);
            ui.label(RichText::new("Projects (.pwproj) keep your photos, points, alignment and framing.").small().color(MUTED));
        });
        self.show_help = open;
    }
}

fn save_output(path: &Path, rgba: &[u8], w: usize, h: usize, fmt: OutFormat, quality: u8) -> Result<(), String> {
    if let Some(d) = path.parent() {
        if !d.as_os_str().is_empty() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
    }
    match fmt {
        OutFormat::Jpeg => {
            let rgb: Vec<u8> = rgba.chunks(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
            let f = std::fs::File::create(path).map_err(|e| e.to_string())?;
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(f), quality);
            enc.encode(&rgb, w as u32, h as u32, image::ExtendedColorType::Rgb8).map_err(|e| e.to_string())
        }
        OutFormat::Png | OutFormat::Tiff => {
            image::save_buffer(path, rgba, w as u32, h as u32, image::ColorType::Rgba8).map_err(|e| e.to_string())
        }
    }
}

/// Edge picture (Sobel) of a square around `c` in a grey photo, as a see-through overlay.
/// Returns the image and its position/size in photo pixels.
fn edge_image(g: &Gray, c: P2, half: f64) -> (egui::ColorImage, usize, usize, usize, usize) {
    let x0 = (c.0 - half).floor().max(0.0) as usize;
    let y0 = (c.1 - half).floor().max(0.0) as usize;
    let x1 = ((c.0 + half).ceil() as usize).min(g.w.saturating_sub(1)).max(x0);
    let y1 = ((c.1 + half).ceil() as usize).min(g.h.saturating_sub(1)).max(y0);
    let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut mag = vec![0.0f32; w * h];
    let mut mx = 1e-6f32;
    for y in 0..h {
        for x in 0..w {
            let (gx_, gy_) = ((x0 + x) as isize, (y0 + y) as isize);
            let p = |dx: isize, dy: isize| g.at_clamped(gx_ + dx, gy_ + dy);
            let sx = (p(1, -1) + 2.0 * p(1, 0) + p(1, 1)) - (p(-1, -1) + 2.0 * p(-1, 0) + p(-1, 1));
            let sy = (p(-1, 1) + 2.0 * p(0, 1) + p(1, 1)) - (p(-1, -1) + 2.0 * p(0, -1) + p(1, -1));
            let m = (sx * sx + sy * sy).sqrt();
            mag[y * w + x] = m;
            mx = mx.max(m);
        }
    }
    let mut rgba = vec![0u8; w * h * 4];
    for (i, m) in mag.iter().enumerate() {
        let v = (m / mx).powf(0.7);
        let a = if v < 0.12 { 0.0 } else { v };
        rgba[i * 4] = 60;
        rgba[i * 4 + 1] = 255;
        rgba[i * 4 + 2] = 255;
        rgba[i * 4 + 3] = (a * 255.0) as u8;
    }
    (egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba), x0, y0, w, h)
}

/// One-line summary of a fine-alignment run.
fn fine_summary(st: &fine_align::FineStats) -> String {
    format!(
        "Fine alignment: {} of {} patches usable · {} control areas · {} points fine-tuned, {} not updated (low correlation) · {} round(s) · {}\nRemaining shift {:.2} → {:.2} px · overlap difference {:.1} → {:.1} · point error mean/median/95 % {:.2}/{:.2}/{:.2} → {:.2}/{:.2}/{:.2} px",
        st.patches_valid,
        st.patches_total,
        st.areas,
        st.points_refined,
        st.points_not_updated,
        st.rounds,
        fmt_ms(st.ms),
        st.shift_before,
        st.shift_after,
        st.diff_before,
        st.diff_after,
        st.err_before[0],
        st.err_before[1],
        st.err_before[2],
        st.err_after[0],
        st.err_after[1],
        st.err_after[2]
    )
}

fn mean_luma(img: &RgbImage) -> f64 {
    let n = (img.width() * img.height()).max(1) as f64;
    img.pixels().map(|p| p[0] as f64 + p[1] as f64 + p[2] as f64).sum::<f64>() / n
}

fn most_common(v: &[usize]) -> usize {
    let mut counts = std::collections::HashMap::new();
    for x in v {
        *counts.entry(*x).or_insert(0) += 1;
    }
    counts.into_iter().max_by_key(|(_, c)| *c).map(|(k, _)| k).unwrap_or(0)
}

fn hue(i: usize) -> Color32 {
    let h = (i as f32 * 0.618_034).fract();
    let c = egui::ecolor::Hsva::new(h, 0.75, 1.0, 1.0);
    c.into()
}

fn dashed_rect(p: &egui::Painter, r: Rect, col: Color32) {
    let dash = 10.0;
    let gap = 7.0;
    let seg = |a: Pos2, b: Pos2| {
        let len = a.distance(b);
        let dir = (b - a) / len;
        let mut t = 0.0;
        while t < len {
            let e = (t + dash).min(len);
            p.line_segment([a + dir * t, a + dir * e], Stroke::new(2.0, col));
            t += dash + gap;
        }
    };
    seg(r.left_top(), r.right_top());
    seg(r.right_top(), r.right_bottom());
    seg(r.right_bottom(), r.left_bottom());
    seg(r.left_bottom(), r.left_top());
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

#[cfg(test)]
mod group_tests {
    use super::*;

    #[test]
    fn groups_are_listed_like_the_original() {
        let g = vec![vec![0, 1, 2, 3, 5], vec![4, 6]];
        assert_eq!(groups_text(&g), "[1-4, 6], [5, 7]");
        let mut cps = vec![ControlPoint::new(0, 1, [0.0; 2], [0.0; 2], false), ControlPoint::new(2, 3, [0.0; 2], [0.0; 2], false)];
        cps.push(ControlPoint::new(1, 2, [0.0; 2], [0.0; 2], false));
        let gs = image_groups(6, &[0, 1, 2, 3, 4, 5], &cps);
        assert_eq!(gs, vec![vec![0, 1, 2, 3], vec![4], vec![5]]);
    }
}
