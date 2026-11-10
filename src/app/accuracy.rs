//! Step 3, "Maximum accuracy": tries every way the program has to place the photos and rates each with the
//! independent check (see `crate::accuracy_search`). The result is only a proposal: it is applied when the person
//! presses "Apply best" (or "Apply" on another row); "Keep current" leaves the alignment as it is.
use super::*;
use crate::accuracy_search::{self, Outcome, SearchConfig, SearchReport};

#[derive(Default)]
pub(super) struct AccState {
    pub(super) cfg: SearchConfig,
    pub(super) report: Option<Box<SearchReport>>,
    /// the row that was applied, for the mark in the table
    pub(super) applied: Option<usize>,
}

impl PanoApp {
    pub(super) fn run_accuracy_search(&mut self) {
        if self.busy() || self.photos.len() < 2 {
            return;
        }
        let active = self.active_indices();
        let cps: Vec<ControlPoint> = self.cps.iter().filter(|c| active.contains(&c.a) && active.contains(&c.b)).copied().collect();
        if cps.is_empty() {
            self.set_status("There are no control points between the active photos. Find or add points first (step 2).", BAD);
            return;
        }
        let lens_of: Vec<usize> = self.photos.iter().map(|p| p.lens).collect();
        let long: Vec<f64> = self.photos.iter().map(|p| p.long()).collect();
        let lenses = self.lenses.clone();
        let s = self.align_settings;
        let fine_cfg = fine_align::FineConfig { locked_weight: s.locked_weight, layers: self.layers.clone(), ..self.fine_cfg.clone() };
        let fa: Vec<fine_align::FaPhoto> = self.photos.iter().map(|p| fine_align::FaPhoto { gray: p.gray.clone(), rgb: p.work.clone() }).collect();
        let sources: Vec<Source> = self.photos.iter().map(|p| p.source.clone()).collect();
        let current = if self.aligned { Some((self.photos.iter().map(|p| p.rot).collect::<Vec<M3>>(), self.lenses.clone())) } else { None };
        let cfg = self.acc.cfg.clone();
        self.acc.report = None;
        self.acc.applied = None;
        self.start_job("Maximum accuracy", move |report, _| {
            report(0.0, "Loading the photos for the check");
            let (check, note) = accuracy_search::load_check_photos(&sources, cfg.check_size, &fa);
            let inp = accuracy_search::PhotoInputs { fa: &fa, check: &check, lens_of: &lens_of, lenses: &lenses, long: &long, cps: &cps, settings: s, fine_cfg, current };
            let mut rep = accuracy_search::search_photos(&inp, &cfg, report);
            if let Some(n) = note {
                rep.notes.push(n);
            }
            JobMsg::AccuracySearch(Box::new(rep))
        });
    }

    pub(super) fn on_accuracy_search(&mut self, rep: Box<SearchReport>) {
        eprintln!("maximum accuracy:\n{}", rep.table());
        let done = rep.rows.iter().filter(|r| r.outcome == Outcome::Done).count();
        let text = match rep.best {
            Some(b) => format!("Maximum accuracy: {done} ways rated in {:.0} s. Best: {}. Nothing was changed yet – see the table in step 3.", rep.secs, rep.rows[b].spec.label()),
            None => "Maximum accuracy: no way could be rated – nothing was changed.".to_string(),
        };
        self.set_status(text, if rep.best.is_some() { GOOD } else { WARN });
        self.acc.report = Some(rep);
        self.acc.applied = None;
    }

    /// Apply the alignment of row `i` (only after the person pressed the button).
    fn apply_accuracy_row(&mut self, i: usize) {
        let Some(rep) = &self.acc.report else { return };
        let Some(result) = rep.rows.get(i).and_then(|r| r.result.clone()) else { return };
        self.on_aligned(result);
        self.acc.applied = Some(i);
    }

    pub(super) fn ui_accuracy(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.label(RichText::new("Maximum accuracy (tries every way)").strong());
            ui.label(
                RichText::new("Optional and slow. Places the photos in every way it has – all together, a frame from the protected points first, photo by photo from several start photos (every new photo can pull the earlier ones right), each with and without the fine alignment – and rates each with the independent check on the same spots. It only proposes: nothing changes until you press Apply.")
                    .small()
                    .color(MUTED),
            );
            ui.horizontal_wrapped(|ui| {
                ui.label("Start photos");
                ui.add(egui::DragValue::new(&mut self.acc.cfg.max_starts).range(1..=10)).on_hover_text("How many photos a photo-by-photo chain starts from (the ones with the most protected points). Every start adds two ways.");
                ui.label("Time budget");
                let mut min = (self.acc.cfg.time_budget_s / 60) as u32;
                if ui.add(egui::DragValue::new(&mut min).range(1..=600).suffix(" min")).on_hover_text("No new way is begun after this; ways left out are listed with the reason.").changed() {
                    self.acc.cfg.time_budget_s = min as u64 * 60;
                }
                ui.checkbox(&mut self.acc.cfg.with_fine, "also with fine alignment");
            });
            ui.horizontal_wrapped(|ui| {
                if primary_button(ui, "🔎  Try all ways", !self.busy() && !self.cps.is_empty() && self.photos.len() >= 2).clicked() {
                    self.run_accuracy_search();
                }
                if self.acc.report.is_some() && ui.button("Keep current").on_hover_text("Close the table and leave the alignment as it is.").clicked() {
                    self.acc.report = None;
                    self.acc.applied = None;
                }
            });
            let mut apply: Option<usize> = None;
            if let Some(rep) = &self.acc.report {
                ui.add_space(4.0);
                ui.label(RichText::new(format!("{} fixed spots, {} measured in every way (the ranking uses these). \"Lost\" = spots another way could measure; \"Mean incl. lost\" counts such a spot as 6 px. Shifts in pixels of the 1600 px working image; smaller is better.", rep.spots, rep.common_spots)).small().color(MUTED));
                egui::ScrollArea::horizontal().id_salt("acc_scroll").show(ui, |ui| {
                    egui::Grid::new("acc_grid").striped(true).spacing(Vec2::new(14.0, 4.0)).show(ui, |ui| {
                        for h in ["", "Way", "Median", "Mean", "90 %", "Spots", "Lost", "Mean incl. lost", "Protected points", "Time", ""] {
                            ui.label(RichText::new(h).color(MUTED));
                        }
                        ui.end_row();
                        for (i, r) in rep.rows.iter().enumerate() {
                            let best = Some(i) == rep.best;
                            ui.label(if best {
                                RichText::new("best").color(GOOD).strong()
                            } else if self.acc.applied == Some(i) {
                                RichText::new("applied").color(MUTED)
                            } else {
                                RichText::new("")
                            });
                            ui.label(r.spec.label());
                            match &r.outcome {
                                Outcome::Done => {
                                    ui.label(format!("{:.3}", r.common.median));
                                    ui.label(format!("{:.3}", r.common.mean));
                                    ui.label(format!("{:.3}", r.common.p90));
                                    ui.label(format!("{}", r.common.n));
                                    ui.label(format!("{}", r.lost));
                                    ui.label(format!("{:.3}", r.mean_with_lost));
                                    ui.label(r.protected.map(|(n, m, x)| format!("{m:.1} / {x:.1} px ({n})")).unwrap_or_else(|| "–".into()));
                                    ui.label(format!("{:.0} s", r.secs));
                                    if r.result.is_some() && ui.small_button(if best { "Apply best" } else { "Apply" }).clicked() {
                                        apply = Some(i);
                                    }
                                }
                                Outcome::SameAs(b) => {
                                    ui.label(RichText::new(format!("same result as way {}", b + 1)).color(MUTED));
                                }
                                Outcome::Skipped(why) => {
                                    ui.label(RichText::new(format!("not run: {why}")).color(WARN));
                                }
                            }
                            ui.end_row();
                        }
                    });
                });
                for n in &rep.notes {
                    ui.label(RichText::new(n).small().color(WARN));
                }
                if rep.cancelled {
                    ui.label(RichText::new("Cancelled – the table is incomplete.").small().color(WARN));
                }
            }
            if let Some(i) = apply {
                self.apply_accuracy_row(i);
            }
        });
    }
}
