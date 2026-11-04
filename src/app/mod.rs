//! The window: a guided, step-by-step panorama studio plus an exposure-blending mode.

mod align;
mod chrome;
mod connections;
mod consts;
mod demo;
mod export;
mod exposure;
mod frame;
mod jobs;
mod masks_canvas;
mod masks_seams;
mod masks_state;
#[cfg(test)]
mod masks_tests;
mod masks_tool;
mod masks_ui;
mod photo_menu;
#[cfg(test)]
mod photo_menu_tests;
mod photos;
mod pipeline;
mod points;
mod points_click;
mod points_edit;
mod points_find;
mod points_panes;
mod points_predict;
mod state;
mod types;
mod ui_helpers;
mod viewport_window;

pub use self::types::PanoApp;
use self::{connections::*, consts::*, export::*, points_panes::*, types::*, ui_helpers::*};

use crate::align::{ControlPoint, Lens};
use crate::engine::{self, AlignResult, AlignSettings, LoadedPhoto, MatchResult, MatchSettings, Source};
use crate::fusion::{self, FusionWeights};
use crate::gainmap::GainMap;
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
        if self.mk.open && self.mode == Mode::Panorama {
            self.mask_window(&ctx);
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
