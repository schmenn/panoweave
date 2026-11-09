use super::*;

impl PanoApp {
    pub fn new(cc: &eframe::CreationContext<'_>, args: crate::Args) -> Self {
        Self::with_context(&cc.egui_ctx, args)
    }

    /// The same without a window (the tests build the app from a bare `egui::Context`).
    pub(super) fn with_context(ctx: &egui::Context, args: crate::Args) -> Self {
        setup_style(ctx);
        let (full_tx, full_rx) = channel();
        let mut app = PanoApp {
            mode: Mode::Panorama,
            step: Step::Photos,
            photos: vec![],
            lenses: vec![],
            cps: vec![],
            gains: vec![],
            gain_maps: vec![],
            gainmap_cfg: crate::gainmap::GainMapConfig::default(),
            lens_models: vec![],
            lens_cfg: crate::lensphoto::LensPhotoConfig::default(),
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
            logo_tex: None,
            preview_size: [0, 0],
            preview_gen: 0,
            preview_shown_gen: 0,
            preview_rx: None,
            mask_hole_share: 0.0,
            mask_sig: 0,
            gains_rx: None,
            gains_gen: 0,
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
            blend_levels: 0,
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
            ov_filter: true,
            ov_tone: true,
            ov_center: None,
            ov_tex: None,
            ov_key: String::new(),
            align_version: 0,
            cp_sort: (CpCol::Index, true),
            cp_only_pair: false,
            cp_min_err: 0.0,
            dup_cfg: crate::duplicate_points::DupConfig::default(),
            cp_only_dups: false,
            cp_only_locked: false,
            dup_cache: None,
            sky_cfg: crate::sky_points::SkyConfig::default(),
            cp_only_sky: false,
            sky_masks: HashMap::new(),
            sky_version: 0,
            sky_cache: None,
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
            undo: super::history::History::new(UNDO_DEPTH),
            mk: Default::default(),
            vp: Default::default(),
        };
        app.patch_win.no_save = app.screenshot.is_some();
        app.vp.no_save = app.screenshot.is_some();
        app.patch_win.demo_scroll = std::env::var("PW_SCROLL").ok().and_then(|v| v.parse().ok());
        if !args.files.is_empty() {
            app.add_paths(args.files);
        }
        app
    }

    pub(super) fn set_status(&mut self, s: impl Into<String>, c: Color32) {
        self.status = s.into();
        self.status_color = c;
    }

    pub(super) fn busy(&self) -> bool {
        self.job.is_some()
    }

    pub(super) fn start_job(&mut self, title: &str, f: impl FnOnce(&(dyn Fn(f32, &str) + Sync), &AtomicBool) -> JobMsg + Send + 'static) {
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
            // a panic in the job must not leave the program "busy" for ever: it becomes a message
            let msg = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&report, &c2))) {
                Ok(m) => m,
                Err(e) => JobMsg::Failed(panic_text(e.as_ref())),
            };
            let _ = tx.send(msg);
        });
        self.job = Some(Job { title: title.into(), rx, progress, cancel, started: Instant::now() });
    }
}

/// The text of a panic payload (`panic!("...")` gives a `&str` or a `String`).
pub(super) fn panic_text(e: &(dyn std::any::Any + Send)) -> String {
    e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown error".into())
}

/// What a receiver of a worker has: a result, nothing yet, or nothing ever (the worker ended without sending).
pub(super) enum Ready<T> {
    Value(T),
    Empty,
    Gone,
}

pub(super) fn ready<T>(rx: &Receiver<T>) -> Ready<T> {
    match rx.try_recv() {
        Ok(v) => Ready::Value(v),
        Err(std::sync::mpsc::TryRecvError::Empty) => Ready::Empty,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => Ready::Gone,
    }
}
