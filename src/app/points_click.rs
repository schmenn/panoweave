use super::*;

impl PanoApp {
    /// Patch click mode: both rough clicks are there, refine them in a worker thread.
    pub(super) fn run_click(&mut self) {
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
        // what is known besides the clicks: the points of this pair and the alignment at the click
        let align = if self.aligned && !self.needs_align { self.predict_from_alignment(a, b, ca).map(|(_, j)| j) } else { None };
        let priors = crate::click_match::ClickPriors { seeds, align };
        self.start_job("Patch click: refining", move |_, _| JobMsg::ClickMatched(Box::new(crate::click_match::run(a, b, &ga, &gb, ca, cb, hint, &priors, s0, &cfg))));
    }

    pub(super) fn on_click_matched(&mut self, r: crate::click_match::ClickResult) {
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

    /// Accept the checked suggestions of the click mode as control points.
    pub(super) fn accept_click(&mut self, all: bool) {
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

    pub(super) fn discard_click(&mut self) {
        if self.click.result.is_some() {
            self.push_undo("discarding the patch click suggestions");
        }
        self.click.result = None;
        self.click.a = None;
        self.click.b = None;
    }

    /// Strip under the toolbar while the patch click mode is on.
    pub(super) fn click_panel(&mut self, ui: &mut egui::Ui) {
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
        egui::Frame::new().fill(CARD).corner_radius(CornerRadius::same(RADIUS_CARD)).inner_margin(egui::Margin::same(TILE_MARGIN)).show(ui, |ui| {
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
}
