//! Strip ("band") planning for the memory-saving export.
//!
//! The panorama is cut into horizontal strips over the full width. Only the photos that touch a
//! strip (plus a margin for the multi-band blend) are needed for it, so the memory need depends on
//! the width and the strip height, not on the whole area. This module only plans (budget, strip
//! heights, which photos touch which strip); it renders nothing.
//!
//! Memory per strip (documented formula, all in bytes):
//!
//! `strip_bytes = width * (strip_rows + 2 * margin) * (blend_bytes_per_px + roi_bytes_per_px)`
//!
//! * `blend_bytes_per_px` is the per-pixel need of the blend itself, the same figures as
//!   [`stitch::estimate_bytes`] (Smooth 50 B, Feather 26 B).
//! * `roi_bytes_per_px` is the working copy of one photo region (colour 12 B, valid 4 B, mask 4 B,
//!   Smooth only). Photos are processed one after the other, so only one is counted.
//! * `margin` is the extra rows above and below the strip so that the multi-band blend gives the
//!   same result as on the whole image (see [`margin_rows`]).

use crate::stitch::{self, Blend};

/// Settings of the strip planner. `StripConfig::default()` gives the documented defaults.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StripConfig {
    /// the new export path is used only when this is on (off = earlier behaviour)
    pub enabled: bool,
    /// smallest strip height (inner rows, without margin) the planner will use
    pub min_rows: usize,
    /// largest strip height
    pub max_rows: usize,
    /// share of the free system memory the export may use (0..1)
    pub budget_share: f64,
    /// extra bytes per pixel of the strip for the working copy of one photo (Smooth only)
    pub roi_bytes_per_px: u64,
    /// margin factor: margin = `margin_factor << levels` rows
    pub margin_factor: usize,
    /// extra push-pull levels (beyond the multi-band levels) for filling holes in a photo region;
    /// the fill reaches `2^(levels + fill_extra_levels)` rows
    pub fill_extra_levels: usize,
    /// share of the memory budget for the cache of decoded photos (0..0.9)
    pub cache_share: f64,
    /// a strip should be at least this many times higher than the margin (otherwise a lot of
    /// work is done twice)
    pub min_strip_to_margin: usize,
}

impl Default for StripConfig {
    fn default() -> Self {
        StripConfig {
            enabled: false,
            min_rows: 64,
            max_rows: 4096,
            budget_share: 0.75,
            roi_bytes_per_px: 20,
            margin_factor: 4,
            fill_extra_levels: 2,
            cache_share: 0.3,
            min_strip_to_margin: 1,
        }
    }
}

/// Result of the planning.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StripPlan {
    /// inner rows per strip (the last strip may be lower)
    pub rows: usize,
    /// margin rows above and below each strip
    pub margin: usize,
    /// number of strips
    pub count: usize,
    /// planned peak memory of one strip in bytes
    pub peak_bytes: u64,
}

/// Margin rows needed above and below a strip for `levels` multi-band levels. The lowest level
/// covers `2^levels` rows of the output and the Gaussian filter reaches a few rows further, so
/// `margin_factor << levels` leaves some room; the hole fill adds its own reach of
/// `2^(levels + fill_extra_levels)` rows. Feather needs no margin (weights are local).
pub fn margin_rows(levels: usize, blend: Blend, cfg: &StripConfig) -> usize {
    match blend {
        Blend::Smooth => (cfg.margin_factor << levels.min(10)) + (1usize << (levels + cfg.fill_extra_levels).min(14)),
        Blend::Feather => 0,
    }
}

/// Planned memory of one strip with `rows` inner rows.
pub fn strip_bytes(width: usize, rows: usize, margin: usize, blend: Blend, cfg: &StripConfig) -> u64 {
    let total_rows = rows as u64 + 2 * margin as u64;
    let px = width as u64 * total_rows;
    let roi = match blend {
        Blend::Smooth => cfg.roi_bytes_per_px,
        Blend::Feather => 0,
    };
    // `estimate_bytes` is linear in the pixel count, so one row of width `width` gives the per-pixel figure
    stitch::estimate_bytes(width, 1, blend) * total_rows + px * roi
}

/// The memory budget in bytes: `budget_share` of the free memory, or an explicit `override_bytes`
/// (the "maximum RAM for the export" setting). `None` if neither is known.
pub fn budget_bytes(free: Option<u64>, override_bytes: Option<u64>, cfg: &StripConfig) -> Option<u64> {
    override_bytes.or_else(|| free.map(|f| (f as f64 * cfg.budget_share.clamp(0.0, 1.0)) as u64))
}

/// Plans the strips for a panorama of `width` x `height` pixels. The strip height is the largest
/// one that stays within `budget` (but at most `max_rows` and at most the image height). An error
/// message (instead of a crash) comes back if even the smallest strip does not fit.
pub fn plan(width: usize, height: usize, levels: usize, blend: Blend, budget: u64, cfg: &StripConfig) -> Result<StripPlan, String> {
    if width == 0 || height == 0 {
        return Err("the panorama has no pixels".into());
    }
    let margin = margin_rows(levels, blend, cfg);
    // strips start on the grid of the coarsest hole-fill level, so the fill (and with it the
    // result) is the same in every strip; the margin is a multiple of that grid too
    let align = match blend {
        Blend::Smooth => 1usize << (levels + cfg.fill_extra_levels).min(14),
        Blend::Feather => 1,
    };
    let margin = margin.div_ceil(align) * align;
    let per_row = strip_bytes(width, 1, margin, blend, cfg) - strip_bytes(width, 0, margin, blend, cfg);
    let fixed = strip_bytes(width, 0, margin, blend, cfg);
    let min_rows = (cfg.min_rows.max(margin * cfg.min_strip_to_margin / 4).max(1)).div_ceil(align) * align;
    let min_rows = min_rows.min(height);
    let need_min = fixed + per_row * min_rows as u64;
    if need_min > budget {
        return Err(format!(
            "the export needs at least {:.2} GB for one strip of {} rows at this width, but only {:.2} GB are allowed – choose a smaller width or allow more memory",
            need_min as f64 / 1073741824.0,
            min_rows,
            budget as f64 / 1073741824.0
        ));
    }
    let fit = ((budget - fixed) / per_row.max(1)) as usize;
    let mut rows = fit.min(cfg.max_rows.max(min_rows)).min(height).max(min_rows);
    if rows < height {
        rows = ((rows / align) * align).max(min_rows);
    }
    Ok(StripPlan { rows, margin, count: height.div_ceil(rows), peak_bytes: strip_bytes(width, rows, margin, blend, cfg) })
}

/// Splits the export memory budget into the part for the strips and the part for the photo cache.
pub fn split_budget(total: u64, cfg: &StripConfig) -> (u64, u64) {
    let cache = (total as f64 * cfg.cache_share.clamp(0.0, 0.9)) as u64;
    (total - cache, cache)
}

/// The inner row ranges `[y0, y1)` of all strips.
pub fn strips(height: usize, rows: usize) -> Vec<(usize, usize)> {
    let rows = rows.max(1);
    (0..height).step_by(rows).map(|y0| (y0, (y0 + rows).min(height))).collect()
}

/// The rows a strip is computed over: the inner range plus the margin, clipped to the image.
pub fn padded(range: (usize, usize), margin: usize, height: usize) -> (usize, usize) {
    (range.0.saturating_sub(margin), (range.1 + margin).min(height))
}

/// Indices of the photos whose row range (from `stitch::roi_rows`) touches `[y0, y1)`,
/// sorted by their first row so that photos needed for the next strip come next.
pub fn photos_for(photo_rows: &[(usize, usize)], range: (usize, usize)) -> Vec<usize> {
    let mut v: Vec<usize> = (0..photo_rows.len()).filter(|&i| photo_rows[i].0 < range.1 && photo_rows[i].1 > range.0).collect();
    v.sort_by_key(|&i| (photo_rows[i].0, i));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;

    #[test]
    fn strips_cover_the_image_without_gaps() {
        for (h, rows) in [(1000, 256), (1024, 256), (1, 64), (65, 64), (7, 3)] {
            let s = strips(h, rows);
            assert_eq!(s.first().unwrap().0, 0);
            assert_eq!(s.last().unwrap().1, h);
            for w in s.windows(2) {
                assert_eq!(w[0].1, w[1].0);
            }
            assert!(s.iter().all(|&(a, b)| b > a && b - a <= rows));
        }
    }

    #[test]
    fn padded_range_is_clipped_to_the_image() {
        assert_eq!(padded((0, 100), 30, 1000), (0, 130));
        assert_eq!(padded((900, 1000), 30, 1000), (870, 1000));
        assert_eq!(padded((300, 400), 30, 1000), (270, 430));
    }

    #[test]
    fn photos_are_selected_by_row_overlap() {
        let rows = [(0, 100), (90, 200), (300, 400), (0, 1000)];
        assert_eq!(photos_for(&rows, (0, 50)), vec![0, 3]);
        assert_eq!(photos_for(&rows, (95, 120)), vec![0, 3, 1]);
        assert_eq!(photos_for(&rows, (200, 300)), vec![3]);
        assert_eq!(photos_for(&rows, (399, 500)), vec![3, 2]);
        assert!(photos_for(&[(0, 10)], (10, 20)).is_empty());
    }

    #[test]
    fn peak_memory_does_not_depend_on_the_height() {
        let cfg = StripConfig::default();
        let a = plan(20_000, 5_000, 6, Blend::Smooth, 8 * GB, &cfg).unwrap();
        let b = plan(20_000, 100_000, 6, Blend::Smooth, 8 * GB, &cfg).unwrap();
        assert_eq!(a.margin, b.margin);
        assert!(a.peak_bytes <= 8 * GB && b.peak_bytes <= 8 * GB);
        // the tall panorama needs more strips but not more memory per strip
        assert!(b.count > a.count);
        assert!(b.peak_bytes <= 8 * GB);
    }

    #[test]
    fn plan_stays_within_the_budget_and_limits() {
        let cfg = StripConfig::default();
        for budget in [2 * GB, 4 * GB, 32 * GB] {
            let p = plan(30_000, 20_000, 5, Blend::Smooth, budget, &cfg).unwrap();
            assert!(p.peak_bytes <= budget, "budget {budget}: {p:?}");
            assert!(p.rows >= cfg.min_rows && p.rows <= cfg.max_rows);
            assert_eq!(p.count, 20_000usize.div_ceil(p.rows));
        }
    }

    #[test]
    fn a_bigger_budget_gives_higher_strips() {
        let cfg = StripConfig::default();
        let small = plan(30_000, 20_000, 5, Blend::Smooth, 2 * GB, &cfg).unwrap();
        let big = plan(30_000, 20_000, 5, Blend::Smooth, 8 * GB, &cfg).unwrap();
        assert!(big.rows > small.rows);
    }

    #[test]
    fn too_small_a_budget_is_an_error_message() {
        let cfg = StripConfig::default();
        let e = plan(60_000, 20_000, 7, Blend::Smooth, 100 << 20, &cfg).unwrap_err();
        assert!(e.contains("smaller width"), "{e}");
    }

    #[test]
    fn feather_has_no_margin_and_a_small_image_is_one_strip() {
        let cfg = StripConfig::default();
        let p = plan(1000, 500, 4, Blend::Feather, GB, &cfg).unwrap();
        assert_eq!(p.margin, 0);
        assert_eq!(p.count, 1);
        assert_eq!(p.rows, 500);
        assert_eq!(margin_rows(5, Blend::Smooth, &cfg), (4 << 5) + (1 << 7));
    }

    #[test]
    fn budget_comes_from_the_override_or_the_free_memory() {
        let cfg = StripConfig::default();
        assert_eq!(budget_bytes(Some(8 * GB), None, &cfg), Some(6 * GB));
        assert_eq!(budget_bytes(Some(10 * GB), Some(GB), &cfg), Some(GB));
        assert_eq!(budget_bytes(None, None, &cfg), None);
    }

    #[test]
    fn auto_levels_matches_the_blend_levels() {
        assert_eq!(stitch::auto_levels(4000, 1000), 6);
        assert_eq!(stitch::auto_levels(100, 100), 3);
        assert_eq!(stitch::auto_levels(100_000, 100_000), 7);
    }
}
