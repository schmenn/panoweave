use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Step {
    Photos,
    Points,
    Align,
    Frame,
    Export,
}

impl Step {
    pub(super) const ALL: [Step; 5] = [Step::Photos, Step::Points, Step::Align, Step::Frame, Step::Export];
    pub(super) fn title(&self) -> &'static str {
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
pub(super) enum Mode {
    Panorama,
    Exposure,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum OutFormat {
    Jpeg,
    Png,
    Tiff,
}

impl OutFormat {
    pub(super) fn ext(&self) -> &'static str {
        match self {
            OutFormat::Jpeg => "jpg",
            OutFormat::Png => "png",
            OutFormat::Tiff => "tif",
        }
    }
    pub(super) fn name(&self) -> &'static str {
        match self {
            OutFormat::Jpeg => "JPEG",
            OutFormat::Png => "PNG (with transparency)",
            OutFormat::Tiff => "TIFF (with transparency)",
        }
    }
}

pub(super) struct Photo {
    pub(super) name: String,
    pub(super) source: Source,
    pub(super) w: u32,
    pub(super) h: u32,
    pub(super) work: Arc<RgbImage>,
    pub(super) gray: Arc<Gray>,
    pub(super) info: PhotoInfo,
    pub(super) thumb: egui::TextureHandle,
    pub(super) tex: Option<egui::TextureHandle>,
    pub(super) lens: usize,
    pub(super) rot: M3,
    pub(super) enabled: bool,
    /// what the person painted on the photo to steer the seams
    pub(super) mask: super::masks_state::PhotoMask,
}

impl Photo {
    pub(super) fn long(&self) -> f64 {
        self.w.max(self.h) as f64
    }
}

pub(super) enum JobMsg {
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

pub(super) struct Job {
    pub(super) title: String,
    pub(super) rx: Receiver<JobMsg>,
    pub(super) progress: Arc<Mutex<(f32, String)>>,
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) started: Instant,
}

pub(super) struct Bracket {
    pub(super) name: String,
    pub(super) source: Source,
    pub(super) work: Arc<RgbImage>,
    pub(super) thumb: egui::TextureHandle,
    pub(super) info: PhotoInfo,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Pane {
    A,
    B,
}

impl Pane {
    pub(super) fn i(self) -> usize {
        match self {
            Pane::A => 0,
            Pane::B => 1,
        }
    }
    pub(super) fn other(self) -> Pane {
        match self {
            Pane::A => Pane::B,
            Pane::B => Pane::A,
        }
    }
}

/// Zoom and position of one photo half in the point editor.
#[derive(Clone, Copy, Default)]
pub(super) struct PaneView {
    /// 1 = whole photo fits
    pub(super) zoom: f32,
    /// photo pixel shown in the middle
    pub(super) center: Vec2,
    /// photo shown + 1 (0 = nothing yet)
    pub(super) key: usize,
}

#[derive(Clone, Copy)]
pub(super) struct PaneGeo {
    pub(super) rect: Rect,
    pub(super) origin: Pos2,
    pub(super) scale: f32,
    pub(super) fit: f32,
    pub(super) w: f32,
    pub(super) h: f32,
}

impl Default for PaneGeo {
    fn default() -> Self {
        PaneGeo { rect: Rect::NOTHING, origin: Pos2::ZERO, scale: 1.0, fit: 1.0, w: 1.0, h: 1.0 }
    }
}

pub(super) enum PaneEvent {
    Fit(Pane),
    Zoom(Pane, Pos2, f32),
    Click(Pane, Pos2),
    DragStart(Pane, Pos2, bool),
    Drag(Pane, Pos2, Vec2),
    DragEnd,
}

#[derive(Default)]
pub(super) struct PointEditor {
    pub(super) a: usize,
    pub(super) b: usize,
    pub(super) selected: Option<usize>,
    pub(super) adding: bool,
    pub(super) pending: Option<(Pane, [f64; 2])>,
    pub(super) dragging: Option<(usize, Pane)>,
    pub(super) panning: Option<Pane>,
    pub(super) views: [PaneView; 2],
    /// photo half whose point gets searched (the other one keeps its spot)
    pub(super) moving: Option<Pane>,
}

/// Matching window of the selected point: which half stays, and the local zoom/rotation
/// from the fixed photo to the other one (working-image pixels).
#[derive(Clone, Copy, PartialEq)]
pub(super) struct SelFrame {
    pub(super) key: (usize, bool, i64, i64, i64, i64, usize),
    pub(super) fixed: Pane,
    pub(super) j: [f64; 4],
}

/// Columns of the control point list.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CpCol {
    Index,
    PhotoA,
    PhotoB,
    PairIndex,
    Error,
    Match,
    Kind,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum FrameDrag {
    None,
    Rotate,
    Roll,
    Crop(usize),
}

pub struct PanoApp {
    pub(super) mode: Mode,
    pub(super) step: Step,
    pub(super) photos: Vec<Photo>,
    pub(super) lenses: Vec<Lens>,
    pub(super) cps: Vec<ControlPoint>,
    pub(super) gains: Vec<[f32; 3]>,
    /// smooth brightness correction per photo (not saved: worked out again from the photos)
    pub(super) gain_maps: Vec<Option<Arc<GainMap>>>,
    pub(super) gainmap_cfg: crate::gainmap::GainMapConfig,
    pub(super) view: PanoView,
    pub(super) aligned: bool,
    pub(super) matched: bool,
    pub(super) needs_align: bool,
    pub(super) match_settings: MatchSettings,
    pub(super) align_settings: AlignSettings,
    pub(super) match_brightness: bool,
    pub(super) last_match: Option<(usize, u128, u128, usize)>,
    pub(super) last_align: Option<(f64, f64, usize, u128)>,
    pub(super) job: Option<Job>,
    pub(super) auto_chain: bool,
    pub(super) status: String,
    pub(super) status_color: Color32,
    pub(super) pe: PointEditor,
    pub(super) err_threshold: f64,
    // preview
    pub(super) preview_tex: Option<egui::TextureHandle>,
    pub(super) preview_size: [usize; 2],
    pub(super) preview_gen: u64,
    pub(super) preview_shown_gen: u64,
    pub(super) preview_rx: Option<Receiver<(u64, egui::ColorImage, PanoView)>>,
    pub(super) preview_view: Option<PanoView>,
    pub(super) preview_dirty: bool,
    pub(super) show_outlines: bool,
    pub(super) frame_drag: FrameDrag,
    // export
    pub(super) out_scale: f32,
    pub(super) out_format: OutFormat,
    pub(super) jpeg_quality: u8,
    pub(super) export_opts: export::ExportOptions,
    pub(super) blend: Blend,
    pub(super) out_path: String,
    pub(super) last_output: Option<PathBuf>,
    // exposure mode
    pub(super) brackets: Vec<Bracket>,
    pub(super) fusion_weights: FusionWeights,
    pub(super) fusion_align: bool,
    pub(super) fusion_tex: Option<egui::TextureHandle>,
    pub(super) fusion_dirty: bool,
    pub(super) fusion_rx: Option<Receiver<egui::ColorImage>>,
    pub(super) fusion_out_path: String,
    // project
    pub(super) project_path: Option<PathBuf>,
    pub(super) pending_project: Option<ProjectFile>,
    // automation
    pub(super) screenshot: Option<PathBuf>,
    pub(super) shot_data: Arc<Mutex<Option<Arc<egui::ColorImage>>>>,
    pub(super) shot_requested: bool,
    pub(super) frames: u64,
    pub(super) demo: Option<String>,
    pub(super) demo_started: bool,
    pub(super) show_help: bool,
    /// list of all control points (like the original's F3 list)
    pub(super) show_cp_list: bool,
    /// fine alignment (patch matching) settings and last result
    pub(super) fine_cfg: crate::fine_align::FineConfig,
    pub(super) last_fine: Option<crate::fine_align::FineStats>,
    /// final render: multi-band levels (0 = automatic as before) and bicubic sampling
    pub(super) blend_levels: usize,
    pub(super) cubic_interp: bool,
    /// use the most detailed photo where photos overlap
    pub(super) prefer_detail: bool,
    /// parallax correction fields per photo (from the last alignment)
    pub(super) fields: Vec<Option<Arc<fine_align::DisplacementField>>>,
    pub(super) parallax_cfg: fine_align::ParallaxConfig,
    /// overlap check window
    pub(super) show_overlap: bool,
    pub(super) ov_mode: u8,
    pub(super) ov_alpha: f32,
    pub(super) ov_tile: usize,
    pub(super) ov_zoom: f32,
    pub(super) ov_arrows: bool,
    pub(super) ov_filter: bool,
    pub(super) ov_center: Option<((usize, usize), V3)>,
    pub(super) ov_tex: Option<egui::TextureHandle>,
    pub(super) ov_key: String,
    /// counts alignments (to refresh views)
    pub(super) align_version: u64,
    pub(super) cp_sort: (CpCol, bool),
    pub(super) cp_only_pair: bool,
    pub(super) cp_min_err: f64,
    /// sorted rows of the control point list and the number within each pair (cached)
    pub(super) cp_rows_cache: Option<(String, Vec<usize>, Vec<usize>)>,
    pub(super) render_started: bool,
    pub(super) second_render_started: bool,
    pub(super) auto_grow: bool,
    pub(super) full_tex: HashMap<usize, egui::TextureHandle>,
    pub(super) full_pending: HashSet<usize>,
    pub(super) full_tx: Sender<(usize, String, egui::ColorImage)>,
    pub(super) full_rx: Receiver<(usize, String, egui::ColorImage)>,
    /// matching window size (working-image pixels, odd)
    pub(super) tpl_size: i32,
    /// search area in % of the photo width
    pub(super) search_pct: f64,
    pub(super) sel_frame: Option<SelFrame>,
    pub(super) edge_tex: [Option<egui::TextureHandle>; 2],
    /// patch comparison window (its own OS window; the records live in patch_store)
    pub(super) patch_win: crate::patch_window::PatchWindow,
    /// geometric pair check (part of the alignment) and its last result
    pub(super) pair_cfg: crate::pair_check::PairCheckConfig,
    pub(super) pair_summary: Option<crate::pair_check::PairSummary>,
    /// connection view (graph + warning list)
    pub(super) show_graph: bool,
    pub(super) graph_photo: Option<usize>,
    /// patch click mode of the point editor
    pub(super) click: ClickState,
    pub(super) click_cfg: crate::click_match::ClickConfig,
    pub(super) undo: Vec<UndoEntry>,
    /// mask painting window
    pub(super) mk: super::masks_ui::MaskUi,
    /// secondary windows as real OS windows (position and size remembered)
    pub(super) vp: super::viewport_window::ViewportWindows,
}

/// Patch click mode: the two rough clicks and the last result (suggestions).
#[derive(Default)]
pub(super) struct ClickState {
    pub(super) on: bool,
    pub(super) a: Option<P2>,
    pub(super) b: Option<P2>,
    pub(super) result: Option<crate::click_match::ClickResult>,
}

/// One step that can be undone in the point editor (points and the open suggestions).
pub(super) struct UndoEntry {
    pub(super) what: String,
    pub(super) cps: Vec<ControlPoint>,
    pub(super) result: Option<crate::click_match::ClickResult>,
}
