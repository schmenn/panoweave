use super::*;

/// Magnifier: /CPEditorPanel/MagnifierWidth default 61 px (hugin.exe sub_14019f160).
pub(super) const MAGNIFIER_WIDTH: f32 = 61.0;
/// Frame colours of the original's point editor (sub_14006cfe0): template RED, search area WHITE.
pub(super) const TEMPLATE_FRAME_COLOR: Color32 = Color32::from_rgb(255, 60, 60);
pub(super) const SEARCH_FRAME_COLOR: Color32 = Color32::WHITE;
/// Point colours in the order the original's point editor uses them (sub_140067330):
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
/// (hugin.exe sub_1400550e0, sub_14019f160). Used here for the full-size photos of the point editor.
pub(super) const IMAGE_CACHE_UPPER_BOUND: u64 = 268435456; // 0x10000000
/// Share of the free memory the final render may use.
/// Memory budget of the low-memory export when the free memory of the computer is unknown (2 GB).
pub(super) const FALLBACK_EXPORT_BUDGET: u64 = 2 << 30;
pub(super) const ACCENT: Color32 = Color32::from_rgb(70, 160, 255);
pub(super) const ACCENT_DARK: Color32 = Color32::from_rgb(34, 92, 160);
pub(super) const GOOD: Color32 = Color32::from_rgb(90, 200, 120);
pub(super) const WARN: Color32 = Color32::from_rgb(240, 190, 70);
pub(super) const BAD: Color32 = Color32::from_rgb(235, 90, 80);
pub(super) const PANEL: Color32 = Color32::from_rgb(28, 30, 35);
pub(super) const BG: Color32 = Color32::from_rgb(20, 21, 25);
pub(super) const CARD: Color32 = Color32::from_rgb(36, 39, 46);
pub(super) const MUTED: Color32 = Color32::from_rgb(150, 156, 168);

/// Steps kept for undo.
pub(super) const UNDO_DEPTH: usize = 30;

/// Automated screenshots give up waiting after this long.
pub(super) const DEMO_TIMEOUT_S: u64 = 300;
