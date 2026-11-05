use super::viewport_window::WinSpec;
use super::*;

/// Groups of photos joined by control points (each sorted, groups sorted by their first photo);
/// a photo without points is a group of its own.
pub(super) fn image_groups(n: usize, active: &[usize], cps: &[ControlPoint]) -> Vec<Vec<usize>> {
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

/// The original's way of listing groups: every group in brackets,
/// runs of consecutive numbers written as a range: "[1-4, 6], [5, 7]" (photo numbers as shown).
pub(super) fn groups_text(groups: &[Vec<usize>]) -> String {
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
pub(super) const GRAPH_LABEL_MIN_LEN: f32 = 70.0;

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

impl PanoApp {
    /// Position of a control point side in pixels of the full-size photo.
    pub(super) fn full_px(&self, i: usize, p: [f64; 2]) -> (f64, f64) {
        let (w, h) = (self.photos[i].w as f64, self.photos[i].h as f64);
        let l = w.max(h);
        (p[0] * l + w * 0.5 - 0.5, p[1] * l + h * 0.5 - 0.5)
    }

    /// Show a point in the editor: open its pair, select it and zoom in on it.
    pub(super) fn jump_to_point(&mut self, k: usize) {
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

    /// The points lying on top of each other, cached until the points or the settings change.
    fn duplicates(&mut self) -> Vec<crate::duplicate_points::Duplicate> {
        if !self.dup_cfg.enabled {
            return Vec::new();
        }
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.cps.len().hash(&mut h);
        self.dup_cfg.tolerance_px.to_bits().hash(&mut h);
        for c in &self.cps {
            (c.a, c.b, c.manual, c.kind as u8, c.pa[0].to_bits(), c.pa[1].to_bits(), c.pb[0].to_bits(), c.pb[1].to_bits(), c.err.to_bits()).hash(&mut h);
        }
        let key = h.finish();
        if let Some((k, d)) = &self.dup_cache {
            if *k == key {
                return d.clone();
            }
        }
        let d = crate::duplicate_points::find(&self.cps, &self.dup_cfg);
        self.dup_cache = Some((key, d.clone()));
        d
    }

    pub(super) fn cp_list_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_cp_list;
        let mut jump: Option<usize> = None;
        let mut delete: Vec<usize> = Vec::new();
        let spec = WinSpec { key: "cp_list", title: "All control points".into(), size: [760.0, 520.0], min_size: [420.0, 260.0], fill: None };
        let mut vp = std::mem::take(&mut self.vp);
        let force = self.demo.is_some();
        vp.show(ctx, &spec, &mut open, force, |ui| {
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
                // points on top of each other (recomputed only when the points or the settings changed)
                let dups = self.duplicates();
                let dup_of: HashMap<usize, crate::duplicate_points::Duplicate> = dups.iter().map(|d| (d.point, *d)).collect();
                let removable = crate::duplicate_points::removable(&self.cps, &dups);
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut self.dup_cfg.enabled, "find points on top of each other");
                    ui.add_enabled_ui(self.dup_cfg.enabled, |ui| {
                        ui.label("closer than");
                        ui.add(egui::DragValue::new(&mut self.dup_cfg.tolerance_px).range(0.1..=20.0).speed(0.05).suffix(" px"));
                        ui.checkbox(&mut self.cp_only_dups, "only those");
                    });
                });
                if self.dup_cfg.enabled {
                    let (same, contra) = (dups.iter().filter(|d| d.kind == crate::duplicate_points::DupKind::Same).count(), dups.iter().filter(|d| d.kind == crate::duplicate_points::DupKind::Contradicts).count());
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(format!("{same} twice on the same spot · {contra} on the same spot in one photo but not in the other")).small().color(if dups.is_empty() { MUTED } else { WARN }));
                        if ui
                            .add_enabled(!removable.is_empty(), egui::Button::new(format!("🗑 Remove {} duplicates", removable.len())))
                            .on_hover_text("Removes the second point of every pair that lies on the same spot in both photos. The better point stays (placed by hand first, then the smaller error). Points placed by hand are never removed, and points that only agree in one photo are only marked.")
                            .clicked()
                        {
                            delete.extend(removable.iter().copied());
                        }
                    });
                }
                let cache_key = format!(
                    "{}-{}-{}-{}-{}-{}-{}-{}-{}-{}",
                    self.cps.len(),
                    self.align_version,
                    self.cp_sort.0 as u8,
                    self.cp_sort.1,
                    self.cp_only_pair,
                    pa,
                    pb,
                    self.cp_min_err,
                    self.cp_only_dups && self.dup_cfg.enabled,
                    self.dup_cache.as_ref().map_or(0, |c| c.0)
                );
                let cached = self.cp_rows_cache.as_ref().filter(|c| c.0 == cache_key).map(|c| (c.1.clone(), c.2.clone()));
                let (rows, pair_no) = if let Some(c) = cached { c } else {
                let mut rows: Vec<usize> = (0..self.cps.len())
                    .filter(|&k| {
                        let c = &self.cps[k];
                        let pair_ok = !self.cp_only_pair || (c.a == pa && c.b == pb) || (c.a == pb && c.b == pa);
                        let err_ok = !self.aligned || c.err >= self.cp_min_err;
                        let dup_ok = !(self.cp_only_dups && self.dup_cfg.enabled) || dup_of.contains_key(&k);
                        pair_ok && err_ok && dup_ok
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
                        let dup = dup_of.get(&k);
                        if sel {
                            p.rect_filled(rect, 3.0, ACCENT_DARK);
                        } else if resp.hovered() {
                            p.rect_filled(rect, 3.0, Color32::from_rgb(48, 52, 61));
                        } else if dup.is_some() {
                            p.rect_filled(rect, 0.0, Color32::from_rgb(64, 48, 24));
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
                        if let Some(d) = dup {
                            let what = match d.kind {
                                crate::duplicate_points::DupKind::Same => "lies on the same spot as",
                                crate::duplicate_points::DupKind::Contradicts => "lies on the same spot in one photo, but not in the other, as",
                            };
                            resp.clone().on_hover_text(format!("Point {} {} point {}", k + 1, what, d.other + 1));
                        }
                        if resp.clicked() {
                            jump = Some(k);
                        }
                    }
                });
            });
        self.vp = vp;
        self.show_cp_list = open;
        if let Some(k) = jump {
            self.jump_to_point(k);
        }
        if !delete.is_empty() {
            delete.sort_unstable();
            delete.dedup();
            self.push_undo(if delete.len() == 1 { "deleting a control point" } else { "deleting control points" });
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
    pub(super) fn overlap_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_overlap;
        let (a, b) = (self.pe.a, self.pe.b);
        let mut redraw = false;
        let spec = WinSpec { key: "overlap", title: "Overlap check".into(), size: [820.0, 640.0], min_size: [420.0, 320.0], fill: None };
        let mut vp = std::mem::take(&mut self.vp);
        let force = self.demo.is_some();
        vp.show(ctx, &spec, &mut open, force, |ui| {
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
                            gain_map: self.gain_map_of(i),
                            field: self.fields.get(i).cloned().flatten(),
                            full_long: self.photos[i].long(),
                            trusted: true,
                            mask: self.photos[i].mask.buf.clone(),
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
                                let g = p.gain_at(px, py);
                                Some([c[0] * g[0], c[1] * g[1], c[2] * g[2]])
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
        self.vp = vp;
        self.show_overlap = open;
    }

    /// Connection view: graph of the photos (edges = pairs with points) and a warning list.
    pub(super) fn graph_window(&mut self, ctx: &egui::Context) {
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
        let mut photo_choice = None;
        let spec = WinSpec { key: "graph", title: "Connections between the photos".into(), size: [760.0, 640.0], min_size: [420.0, 320.0], fill: Some(PANEL) };
        let mut vp = std::mem::take(&mut self.vp);
        let force = self.demo.is_some();
        vp.show(ctx, &spec, &mut open, force, |ui| {
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
                    let thumb = ui.add(egui::Image::new((t.id(), s * k)).sense(Sense::click()));
                    photo_choice = self.photo_menu(&thumb, i);
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
        self.vp = vp;
        if !open {
            self.show_graph = false;
        }
        if let Some((i, a)) = photo_choice {
            self.apply_photo_action(i, a);
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
                    self.pe.tool = PointTool::PatchClick;
                    self.click.a = None;
                    self.click.b = None;
                    self.click.result = None;
                }
            }
            Some((1, a, b)) => self.run_pair_recheck(a, b),
            _ => {}
        }
    }

    pub(super) fn help_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_help;
        let spec = WinSpec { key: "help", title: "How PanoWeave works".into(), size: [520.0, 420.0], min_size: [360.0, 240.0], fill: None };
        let mut vp = std::mem::take(&mut self.vp);
        let force = self.demo.is_some();
        vp.show(ctx, &spec, &mut open, force, |ui| {
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
        self.vp = vp;
        self.show_help = open;
    }
}
