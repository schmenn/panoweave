use super::*;

/// Magnifier: default width 61 px.
pub(super) const MAGNIFIER_WIDTH: f32 = 61.0;
/// Frame colours of the original's point editor: template RED, search area WHITE.
pub(super) const TEMPLATE_FRAME_COLOR: Color32 = MARK_RED;
pub(super) const SEARCH_FRAME_COLOR: Color32 = Color32::WHITE;
/// Point colours in the order the original's point editor uses them:
/// BLUE, GREEN, CYAN, GOLD, NAVY, DARK TURQUOISE, SALMON, MAROON, KHAKI
/// (RGB as defined by the wxWidgets colour database).
pub(super) const POINT_COLORS: [Color32; 9] = [
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

pub(super) fn point_color(i: usize) -> Color32 {
    POINT_COLORS[i % POINT_COLORS.len()]
}

/// Image-cache bound of the original: /ImageCache/UpperBound default 0x10000000 = 256 MiB
/// Used here for the full-size photos of the point editor.
pub(super) const IMAGE_CACHE_UPPER_BOUND: u64 = 268435456; // 0x10000000
/// Share of the free memory the final render may use.
/// Memory budget of the low-memory export when the free memory of the computer is unknown (2 GB).
pub(super) const FALLBACK_EXPORT_BUDGET: u64 = 2 << 30;
// ---- Central style values (used by `setup_style`, `card`, `primary_button` and every hand-made frame) ----
/// Corner radius of buttons, fields, combo boxes, checkboxes and thumbnails (small, so controls look flat and tidy).
pub(crate) const RADIUS: u8 = 3;
/// Corner radius of cards, tiles and windows.
pub(crate) const RADIUS_CARD: u8 = 6;
/// Corner radius of the big image surfaces (point editor panes, preview, mask canvas): square, so a zoomed
/// photo that fills the surface does not poke its square corners over a rounded edge.
pub(crate) const RADIUS_CANVAS: u8 = 0;
/// Gap between neighbouring elements (`item_spacing.x`); also the unit for padding inside tiles.
pub(super) const GAP: f32 = 8.0;
/// Vertical gap between rows (`item_spacing.y`).
pub(super) const GAP_Y: f32 = 7.0;
/// Height of normal buttons and fields (`interact_size.y`).
pub(super) const ROW_H: f32 = 26.0;
/// Height of big buttons (primary actions, start-page buttons, mode tabs).
pub(super) const BIG_H: f32 = 34.0;
/// Font size of big buttons.
pub(super) const BIG_FONT: f32 = 15.0;
/// Inner margin of cards.
pub(super) const CARD_MARGIN: i8 = 14;
/// Inner margin of small tiles (photo thumbnails, hints inside panels).
pub(super) const TILE_MARGIN: i8 = 8;
/// Width of a photo tile in the overview of step 1 (thumbnail width; the grid makes as many columns as fit).
pub(super) const PHOTO_TILE_W: f32 = 200.0;
// ---- Colours --------------------------------------------------------------------------------------
// Every colour of the UI comes from here (a test rejects colours written as numbers elsewhere).
// Neutral greys for the surfaces and a violet accent that matches the program icon
// (`assets/logo.svg`: #4B3BA8, #6F5BD8, #9C8CFF).

/// Window background behind everything (top bar, empty canvas).
pub(crate) const BG: Color32 = Color32::from_rgb(20, 20, 21);
/// Side panels and the main panel.
pub(crate) const PANEL: Color32 = Color32::from_rgb(28, 28, 30);
/// Cards, windows, menus.
pub(crate) const CARD: Color32 = Color32::from_rgb(38, 38, 40);
/// A card or list row under the mouse.
pub(crate) const CARD_HOVER: Color32 = Color32::from_rgb(46, 46, 49);
/// Every second row of a table, the inside of drop areas.
pub(crate) const FAINT: Color32 = Color32::from_rgb(33, 33, 35);
/// Behind pictures (frame preview, connection graph, patch views).
pub(crate) const CANVAS: Color32 = Color32::from_rgb(24, 24, 26);
/// Buttons and other widgets at rest / under the mouse.
pub(crate) const WIDGET: Color32 = Color32::from_rgb(50, 50, 53);
pub(crate) const WIDGET_HOVER: Color32 = Color32::from_rgb(63, 63, 67);
/// Lines and frames that must stay visible (dashed drop area, steps not reached yet).
pub(crate) const BORDER_STRONG: Color32 = Color32::from_rgb(80, 80, 84);
/// Normal text drawn by hand (labels in pictures, step names).
pub(crate) const TEXT: Color32 = Color32::from_rgb(220, 221, 222);
/// Less important text.
pub(crate) const MUTED: Color32 = Color32::from_rgb(153, 153, 153);
/// Tint for the thumbnail of a photo that is switched off.
pub(crate) const DIMMED_TINT: Color32 = Color32::from_rgb(90, 90, 90);
/// Accent: light (text, lines, active things), middle, dark (filled buttons, selection).
pub(crate) const ACCENT: Color32 = Color32::from_rgb(156, 140, 255);
pub(crate) const ACCENT_MID: Color32 = Color32::from_rgb(111, 91, 216);
pub(crate) const ACCENT_DARK: Color32 = Color32::from_rgb(75, 59, 168);
/// Background of a selected or highlighted row / card.
pub(crate) const ACCENT_SOFT: Color32 = Color32::from_rgb(44, 38, 72);
/// Status colours (meaning stays: green fine, yellow check, red problem).
pub(crate) const GOOD: Color32 = Color32::from_rgb(90, 200, 120);
pub(crate) const WARN: Color32 = Color32::from_rgb(240, 190, 70);
pub(crate) const BAD: Color32 = Color32::from_rgb(235, 90, 80);
/// Background of a row that needs attention (duplicate point).
pub(crate) const WARN_SOFT: Color32 = Color32::from_rgb(66, 54, 28);
/// Background of a row with a point on sky or cloud.
pub(crate) const SKY_ROW: Color32 = Color32::from_rgb(28, 44, 66);
/// Marks drawn over photos: a hand click waiting for its partner, the red cross of the magnifier,
/// the search spot of a patch, photo B over A in the patch window.
pub(crate) const MARK_CLICK: Color32 = Color32::from_rgb(255, 150, 40);
pub(crate) const MARK_RED: Color32 = Color32::from_rgb(255, 60, 60);
pub(crate) const MARK_SPOT: Color32 = Color32::from_rgb(90, 200, 255);
pub(crate) const PATCH_B_TINT: Color32 = Color32::from_rgba_unmultiplied_const(255, 200, 120, 140);
/// Mask colours: "use here" and "do not use here".
pub(crate) const MASK_GREEN: Color32 = Color32::from_rgb(60, 200, 90);
pub(crate) const MASK_RED: Color32 = Color32::from_rgb(235, 70, 60);

/// Colour between `a` (t = 0) and `b` (t = 1), for scales such as bad → good.
pub(crate) fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// Steps kept for undo (and again for redo). Every step holds a copy of all control points
/// (~100 bytes each, up to ~12 000 points), so 20 steps stay below ~25 MB per stack.
pub(super) const UNDO_DEPTH: usize = 20;

/// Automated screenshots give up waiting after this long.
pub(super) const DEMO_TIMEOUT_S: u64 = 300;

/// The user guide (docs/GUIDE.md), opened from the help window.
pub(super) const GUIDE_URL: &str = "https://github.com/schmenn/panoweave/blob/main/docs/GUIDE.md";

/// The program icon (`assets/logo.svg` rendered at 256 px), for the main window and every
/// secondary window, so the task bar shows PanoWeave's icon and not the toolkit's default one.
pub(crate) fn window_icon() -> std::sync::Arc<eframe::egui::IconData> {
    static ICON: std::sync::OnceLock<std::sync::Arc<eframe::egui::IconData>> = std::sync::OnceLock::new();
    ICON.get_or_init(|| std::sync::Arc::new(eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon-256.png")).expect("assets/icon-256.png is a PNG")))
        .clone()
}

/// The logo for the top bar (64 px, drawn at 30 px).
pub(super) fn logo_image() -> eframe::egui::ColorImage {
    let img = image::load_from_memory(include_bytes!("../../assets/icon-64.png")).expect("assets/icon-64.png is a PNG").to_rgba8();
    eframe::egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw())
}
