use super::*;

pub(super) fn setup_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = Vec2::new(GAP, GAP_Y);
        s.spacing.button_padding = Vec2::new(10.0, 5.0);
        s.spacing.interact_size.y = ROW_H;
        let v = &mut s.visuals;
        v.panel_fill = PANEL;
        v.window_fill = CARD;
        v.extreme_bg_color = BG;
        v.faint_bg_color = Color32::from_rgb(32, 34, 40);
        v.selection.bg_fill = ACCENT_DARK;
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.hyperlink_color = ACCENT;
        for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.corner_radius = CornerRadius::same(RADIUS);
        }
        v.menu_corner_radius = CornerRadius::same(RADIUS_CARD);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(48, 52, 61);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(60, 65, 77);
        v.widgets.active.weak_bg_fill = ACCENT_DARK;
        v.window_corner_radius = CornerRadius::same(RADIUS_CARD);
        s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.5));
        s.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.5));
        s.text_styles.insert(egui::TextStyle::Heading, FontId::proportional(21.0));
        s.text_styles.insert(egui::TextStyle::Small, FontId::proportional(12.0));
    });
}

pub(super) fn color_image(img: &RgbImage) -> egui::ColorImage {
    egui::ColorImage::from_rgb([img.width() as usize, img.height() as usize], img.as_raw())
}

pub(super) fn primary_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).color(Color32::WHITE).strong().size(BIG_FONT))
            .fill(ACCENT_DARK)
            .stroke(Stroke::new(1.0, ACCENT))
            .min_size(Vec2::new(0.0, BIG_H)),
    )
}

/// How many tiles of width `tile` fit next to each other into `avail` with `gap` between them (at least one).
pub(super) fn tiles_per_row(avail: f32, tile: f32, gap: f32) -> usize {
    (((avail + gap) / (tile + gap)).floor() as usize).max(1)
}

pub(super) fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .corner_radius(CornerRadius::same(RADIUS_CARD))
        .inner_margin(egui::Margin::same(CARD_MARGIN))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

pub(super) fn err_color(e: f64) -> Color32 {
    if e < 2.0 {
        GOOD
    } else if e < 5.0 {
        WARN
    } else {
        BAD
    }
}

pub(super) fn fmt_ms(ms: u128) -> String {
    if ms < 1000 { format!("{ms} ms") } else { format!("{:.1} s", ms as f64 / 1000.0) }
}

/// Edge picture (Sobel) of a square around `c` in a grey photo, as a see-through overlay.
/// Returns the image and its position/size in photo pixels.
pub(super) fn edge_image(g: &Gray, c: P2, half: f64) -> (egui::ColorImage, usize, usize, usize, usize) {
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
pub(super) fn fine_summary(st: &fine_align::FineStats) -> String {
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

pub(super) fn mean_luma(img: &RgbImage) -> f64 {
    let n = (img.width() * img.height()).max(1) as f64;
    img.pixels().map(|p| p[0] as f64 + p[1] as f64 + p[2] as f64).sum::<f64>() / n
}

pub(super) fn most_common(v: &[usize]) -> usize {
    let mut counts = std::collections::HashMap::new();
    for x in v {
        *counts.entry(*x).or_insert(0) += 1;
    }
    counts.into_iter().max_by_key(|(_, c)| *c).map(|(k, _)| k).unwrap_or(0)
}

pub(super) fn hue(i: usize) -> Color32 {
    let h = (i as f32 * 0.618_034).fract();
    let c = egui::ecolor::Hsva::new(h, 0.75, 1.0, 1.0);
    c.into()
}

pub(super) fn dashed_rect(p: &egui::Painter, r: Rect, col: Color32) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_uses_central_constants() {
        let ctx = egui::Context::default();
        setup_style(&ctx);
        let st = ctx.style_of(egui::Theme::Dark);
        let w = &st.visuals.widgets;
        let want = CornerRadius::same(RADIUS);
        for (name, r) in [
            ("noninteractive", w.noninteractive.corner_radius),
            ("inactive", w.inactive.corner_radius),
            ("hovered", w.hovered.corner_radius),
            ("active", w.active.corner_radius),
            ("open", w.open.corner_radius),
        ] {
            assert_eq!(r, want, "widget state {name}");
        }
        assert_eq!(st.visuals.window_corner_radius, CornerRadius::same(RADIUS_CARD));
        assert_eq!(st.visuals.menu_corner_radius, CornerRadius::same(RADIUS_CARD));
        assert_eq!(st.spacing.item_spacing, Vec2::new(GAP, GAP_Y));
        assert_eq!(st.spacing.interact_size.y, ROW_H);
        const { assert!(RADIUS < RADIUS_CARD && BIG_H > ROW_H) };
    }

    /// The arguments of the call that starts at `open` (the index just after its `(`), split at top-level commas.
    fn call_args(src: &str, open: usize) -> Vec<String> {
        let (mut depth, mut cur, mut out) = (0i32, String::new(), Vec::new());
        for ch in src[open..].chars() {
            match ch {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' if depth == 0 => break,
                ')' | ']' | '}' => depth -= 1,
                ',' if depth == 0 => {
                    out.push(std::mem::take(&mut cur).trim().to_string());
                    continue;
                }
                _ => {}
            }
            cur.push(ch);
        }
        out.push(cur.trim().to_string());
        out
    }

    /// Every corner radius in the UI code comes from `RADIUS`, `RADIUS_CARD` or `RADIUS_CANVAS`; a number written
    /// into a painter call (other than 0) is what made the corners uneven before.
    #[test]
    fn no_hand_written_corner_radius_in_the_ui() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files: Vec<_> = std::fs::read_dir(root.join("app")).unwrap().map(|e| e.unwrap().path()).collect();
        files.extend(["patch_window.rs", "patch_viz.rs"].map(|f| root.join(f)));
        // (call, index of the radius argument)
        let calls = [("rect_filled(", 1), ("rect_stroke(", 1), ("CornerRadius::same(", 0), (".corner_radius(", 0)];
        let mut bad = Vec::new();
        for f in files.iter().filter(|f| f.extension().is_some_and(|e| e == "rs")) {
            let all = std::fs::read_to_string(f).unwrap();
            // test code draws nothing
            let src = all.split("#[cfg(test)]").next().unwrap_or_default();
            for (call, arg) in calls {
                for (at, _) in src.match_indices(call) {
                    let Some(a) = call_args(src, at + call.len()).into_iter().nth(arg) else { continue };
                    let num = a.trim_end_matches("f32").trim_end_matches("u8").trim_end_matches('_');
                    if num.parse::<f32>().is_ok_and(|v| v != 0.0) {
                        let line = src[..at].lines().count();
                        bad.push(format!("{}:{line}: {call}… {a}", f.file_name().unwrap().to_string_lossy()));
                    }
                }
            }
        }
        assert!(bad.is_empty(), "use RADIUS / RADIUS_CARD / RADIUS_CANVAS instead of:
{}", bad.join("
"));
    }

    #[test]
    fn tiles_wrap_into_rows_that_fit() {
        assert_eq!(tiles_per_row(1000.0, 216.0, 8.0), 4); // 4*216 + 3*8 = 888
        assert_eq!(tiles_per_row(888.0, 216.0, 8.0), 4);
        assert_eq!(tiles_per_row(887.0, 216.0, 8.0), 3);
        assert_eq!(tiles_per_row(100.0, 216.0, 8.0), 1, "a narrow window still shows one per row");
        assert_eq!(tiles_per_row(0.0, 216.0, 8.0), 1);
    }

    #[test]
    fn the_radius_check_finds_a_written_number() {
        let src = "p.rect_filled(Rect::from_min_max(a, b), 8.0, fill);";
        let at = src.find("rect_filled(").unwrap() + "rect_filled(".len();
        assert_eq!(call_args(src, at), ["Rect::from_min_max(a, b)", "8.0", "fill"]);
    }
}
