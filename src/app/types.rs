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
            Step::Export => "Export",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Panorama,
    Exposure,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    /// Whether a file extension (any case, also "jpeg"/"tiff") belongs to this format.
    pub(super) fn has_ext(&self, ext: &str) -> bool {
        let e = ext.to_ascii_lowercase();
        match self {
            OutFormat::Jpeg => e == "jpg" || e == "jpeg",
            OutFormat::Png => e == "png",
            OutFormat::Tiff => e == "tif" || e == "tiff",
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

#[allow(clippy::large_enum_variant)] // a few messages per job; boxing would not save anything that matters
pub(super) enum JobMsg {
    Loaded(Vec<Result<LoadedPhoto, String>>),
    Matched(MatchResult),
    Aligned(AlignResult),
    Rendered(Result<(PathBuf, u128, usize, usize), String>),
    BracketLoaded(Vec<Result<(String, Source, Arc<RgbImage>, RgbImage, PhotoInfo), String>>),
    FusedSaved(Result<(PathBuf, u128), String>),
    /// sky masks of the photos that had none: (key of the working image, the image, its mask)
    SkyMasks(Vec<(usize, Arc<RgbImage>, Arc<crate::features::SkyMask>)>),
    PairPoints { a: usize, b: usize, cps: Vec<ControlPoint>, ms: u128, mode: &'static str },
    /// "Search again" for one pair from the connection view
    PairChecked { a: usize, b: usize, cps: Vec<ControlPoint>, reports: Vec<crate::pair_check::PairReport> },
    /// patch click mode result
    ClickMatched(Box<crate::click_match::ClickResult>),
    /// "Snap selected" finished (see `points_snap.rs`)
    Snapped(Box<super::points_snap::SnapDone>),
    /// the person pressed Cancel: the job's result is thrown away
    Cancelled,
    /// the job stopped with an error that was not expected (a panic in the worker); the text says what happened
    Failed(String),
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
    /// pane, position, modifier keys held during the click (Shift/Ctrl = multiple selection)
    Click(Pane, Pos2, egui::Modifiers),
    /// pane, position, primary button, modifier keys
    DragStart(Pane, Pos2, bool, egui::Modifiers),
    Drag(Pane, Pos2, Vec2),
    DragEnd,
}

/// The one tool of the point editor: what a click into a photo does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) enum PointTool {
    /// Select / drag points.
    #[default]
    Edit,
    /// Place a point by clicking the same spot in both photos.
    Add,
    /// Rough click pair refined by patch matching.
    PatchClick,
    /// Drag a rectangle: selects the points inside.
    SelectRect,
    /// Drag a free outline: selects the points inside.
    SelectLasso,
    /// Paint over points to select them (Ctrl = take them out again).
    SelectBrush,
}

impl PointTool {
    /// The tool after a tool button was pressed: the active one again leads back to `Edit`,
    /// another one replaces it (never two at once).
    pub(super) fn toggle(current: PointTool, pressed: PointTool) -> PointTool {
        if current == pressed { PointTool::Edit } else { pressed }
    }

    /// Tools that draw a selection shape with the mouse.
    pub(super) fn is_select(self) -> bool {
        matches!(self, PointTool::SelectRect | PointTool::SelectLasso | PointTool::SelectBrush)
    }
}

/// A selection shape that is being dragged (photo pixels of the pane it started in).
pub(super) struct SelDrag {
    pub(super) pane: Pane,
    pub(super) path: Vec<P2>,
    /// keys held when the drag started
    pub(super) mods: egui::Modifiers,
}

#[derive(Default)]
pub(super) struct PointEditor {
    pub(super) a: usize,
    pub(super) b: usize,
    pub(super) selected: Option<usize>,
    /// Further selected points (Shift/Ctrl+click); indices into the point list, see `points_select.rs`.
    pub(super) sel: std::collections::HashSet<usize>,
    /// order fingerprint of the points when `sel` was last checked
    pub(super) sel_fp: u64,
    /// selection shape being dragged (rectangle, lasso, brush)
    pub(super) sel_drag: Option<SelDrag>,
    /// brush radius in screen pixels (0 = `select_shapes::BRUSH_PX_DEFAULT`)
    pub(super) brush_px: f32,
    /// What a click into a photo does (radio-button style, see `PointTool::toggle`).
    pub(super) tool: PointTool,
    pub(super) pending: Option<(Pane, [f64; 2])>,
    /// the point the add tool just placed (while it is still the selected one, a further click
    /// moves its clicked half instead of starting a second point)
    pub(super) fresh: Option<usize>,
    /// Alt was held when the current drag started: no snapping at its end
    pub(super) drag_no_snap: bool,
    pub(super) dragging: Option<(usize, Pane)>,
    pub(super) panning: Option<Pane>,
    pub(super) views: [PaneView; 2],
    /// photo half whose point gets searched (the other one keeps its spot)
    pub(super) moving: Option<Pane>,
    /// the points as they were when the current drag of a point started (one undo step per drag)
    pub(super) drag_before: Option<CpSnapshot>,
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
    /// response curve and vignetting per photo (from the model of its lens; not saved: worked out again)
    pub(super) lens_models: Vec<Option<crate::lensphoto::LensModel>>,
    pub(super) lens_cfg: crate::lensphoto::LensPhotoConfig,
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
    /// the logo in the top bar (made on first use)
    pub(super) logo_tex: Option<egui::TextureHandle>,
    pub(super) preview_size: [usize; 2],
    pub(super) preview_gen: u64,
    pub(super) preview_shown_gen: u64,
    /// preview worker: (number, picture, view, share of the crop that masks leave empty)
    pub(super) preview_rx: Option<Receiver<(u64, egui::ColorImage, PanoView, f32)>>,
    /// share of the cropped panorama that red masks leave without any photo (see `stitch::mask_holes`)
    pub(super) mask_hole_share: f32,
    /// fingerprint of all masks when the preview was last asked for (a change makes a new preview)
    pub(super) mask_sig: u64,
    /// brightness update being worked out on a worker thread (`update_gains`), with its number
    pub(super) gains_rx: Option<Receiver<(u64, super::pipeline::GainsUpdate)>>,
    pub(super) gains_gen: u64,
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
    pub(super) ov_tone: bool,
    pub(super) ov_center: Option<((usize, usize), V3)>,
    pub(super) ov_tex: Option<egui::TextureHandle>,
    pub(super) ov_key: String,
    /// counts alignments (to refresh views)
    pub(super) align_version: u64,
    pub(super) cp_sort: (CpCol, bool),
    pub(super) cp_only_pair: bool,
    pub(super) cp_min_err: f64,
    /// marking of control points that lie on top of each other (list window)
    pub(super) dup_cfg: crate::duplicate_points::DupConfig,
    pub(super) cp_only_dups: bool,
    /// list filter: only protected points
    pub(super) cp_only_locked: bool,
    /// result of the duplicate check with a fingerprint of the points it was made for
    pub(super) dup_cache: Option<(u64, Vec<crate::duplicate_points::Duplicate>)>,
    /// points on sky or cloud: settings, list filter, sky masks per photo (made on a worker thread, not saved
    /// in the project; keyed by the working image they belong to) and the last result with its fingerprint
    pub(super) sky_cfg: crate::sky_points::SkyConfig,
    pub(super) cp_only_sky: bool,
    pub(super) sky_masks: HashMap<usize, (Arc<RgbImage>, Arc<crate::features::SkyMask>)>,
    pub(super) sky_version: u64,
    pub(super) sky_cache: Option<(u64, Vec<crate::sky_points::SkyPoint>)>,
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
    /// undo / redo of the control points (every window feeds the same one)
    pub(super) undo: super::history::History<CpSnapshot>,
    /// mask painting window
    pub(super) mk: super::masks_ui::MaskUi,
    /// secondary windows as real OS windows (position and size remembered)
    pub(super) vp: super::viewport_window::ViewportWindows,
}

/// Patch click mode: the two rough clicks and the last result (suggestions).
#[derive(Default)]
pub(super) struct ClickState {
    /// the photo pair the clicks belong to; another pair on screen drops them
    pub(super) pair: (usize, usize),
    pub(super) a: Option<P2>,
    pub(super) b: Option<P2>,
    pub(super) result: Option<crate::click_match::ClickResult>,
}

/// The state of the control points (and the open patch click suggestions) at one moment.
pub(super) struct CpSnapshot {
    pub(super) cps: Vec<ControlPoint>,
    pub(super) result: Option<crate::click_match::ClickResult>,
}

#[cfg(test)]
mod point_tool_tests {
    use super::PointTool::{self, *};

    #[test]
    fn toggle_is_radio_like() {
        assert_eq!(PointTool::toggle(Edit, Add), Add);
        assert_eq!(PointTool::toggle(Add, PatchClick), PatchClick);
        assert_eq!(PointTool::toggle(PatchClick, Add), Add);
        assert_eq!(PointTool::toggle(Add, Add), Edit);
        assert_eq!(PointTool::toggle(PatchClick, PatchClick), Edit);
        assert_eq!(PointTool::default(), Edit);
    }
}
