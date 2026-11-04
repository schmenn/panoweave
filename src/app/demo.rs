use super::*;

fn demo_clock() -> &'static Instant {
    static T: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T.get_or_init(Instant::now)
}

impl PanoApp {
    /// Screenshot and demo automation of the headless drivers (`--screenshot`, `--demo`).
    pub(super) fn automation(&mut self, ctx: &egui::Context) {
        self.frames += 1;
        if let Some(d) = self.demo.clone() {
            self.demo_drive(ctx, &d);
        }
        self.screenshot_drive(ctx);
    }

    fn demo_drive(&mut self, ctx: &egui::Context, d: &str) {
        self.demo_start(ctx, d);
        // after the sample photos are in, continue the chain
        if self.auto_chain && !self.busy() && !self.photos.is_empty() && !self.matched {
            self.create_auto();
        }
        self.demo_render(d);
        self.demo_select_point(d);
        self.demo_goto_step(d);
    }

    /// First frame: prepare the window for the chosen demo and load the sample photos.
    fn demo_start(&mut self, ctx: &egui::Context, d: &str) {
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
            match d {
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
    }

    /// `render` demo: export once (twice with PW_OUT2).
    fn demo_render(&mut self, d: &str) {
        if d == "render" && !self.busy() && self.aligned && self.last_output.is_none() && !self.render_started {
            self.render_started = true;
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
                if let Some(mb) = std::env::var("PW_EXPORT_MB2").ok().and_then(|v| v.parse::<u64>().ok()) {
                    // SAFETY: the demo runs on the UI thread, the worker threads do not read this variable
                    unsafe { std::env::set_var("PW_EXPORT_MB", mb.to_string()) };
                }
                self.out_path = o2;
                self.last_output = None;
                self.run_render();
            }
        }
    }

    /// `pointsel` / `pointdrag` demos: show a selected point zoomed in.
    fn demo_select_point(&mut self, d: &str) {
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
    }

    /// Jump to the step the demo shows.
    fn demo_goto_step(&mut self, d: &str) {
        if !self.busy() && self.aligned {
            match d {
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

    /// Take the screenshot once everything has settled, then close the window.
    fn screenshot_drive(&mut self, ctx: &egui::Context) {
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
}
