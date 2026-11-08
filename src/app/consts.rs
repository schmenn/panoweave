use super::*;

/// Magnifier: default width 61 px.
pub(super) const MAGNIFIER_WIDTH: f32 = 61.0;
/// Frame colours of the original's point editor: template RED, search area WHITE.
pub(super) const TEMPLATE_FRAME_COLOR: Color32 = Color32::from_rgb(255, 60, 60);
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
pub(super) const ACCENT: Color32 = Color32::from_rgb(70, 160, 255);
pub(super) const ACCENT_DARK: Color32 = Color32::from_rgb(34, 92, 160);
pub(super) const GOOD: Color32 = Color32::from_rgb(90, 200, 120);
pub(super) const WARN: Color32 = Color32::from_rgb(240, 190, 70);
pub(super) const BAD: Color32 = Color32::from_rgb(235, 90, 80);
pub(super) const PANEL: Color32 = Color32::from_rgb(28, 30, 35);
pub(super) const BG: Color32 = Color32::from_rgb(20, 21, 25);
pub(super) const CARD: Color32 = Color32::from_rgb(36, 39, 46);
pub(super) const MUTED: Color32 = Color32::from_rgb(150, 156, 168);

/// Steps kept for undo (and again for redo). Every step holds a copy of all control points
/// (~100 bytes each, up to ~12 000 points), so 20 steps stay below ~25 MB per stack.
pub(super) const UNDO_DEPTH: usize = 20;

/// Automated screenshots give up waiting after this long.
pub(super) const DEMO_TIMEOUT_S: u64 = 300;
