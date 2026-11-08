//! Finding control points that sit on sky or clouds.
//!
//! The point finder already keeps points off the sky (`features::sky_mask`, `features::cloud_score`), but
//! points from patch click, from growing, from old projects or placed by hand can still lie there. Clouds
//! move between shots, so such points pull the alignment off. This module only decides which points are on
//! sky or cloud and which of them may be removed; the caller shows them and removes on request.
//! Points placed by hand and protected points are reported but never offered for removal.

use crate::align::{ControlPoint, CpKind};
use crate::engine;
use crate::features::{self, SkyMask};

/// Lowest and highest value of the threshold slider.
pub const THRESHOLD_RANGE: (f32, f32) = (0.3, 0.8);

/// Settings of the check.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SkyConfig {
    /// switched off = nothing is computed, reported or offered
    pub enabled: bool,
    /// a point is on sky or cloud when the share of sky around it in one of its two photos is above this
    pub threshold: f32,
    /// also offer points placed by hand for removal (protected points stay in any case)
    pub include_manual: bool,
}

impl Default for SkyConfig {
    fn default() -> Self {
        SkyConfig { enabled: false, threshold: features::CLOUD_THRESHOLD, include_manual: false }
    }
}

/// A point on sky or cloud, with the higher of the two scores.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SkyPoint {
    pub point: usize,
    pub score: f32,
}

/// What the check needs to know about one photo: its sky mask and the size of the working image the mask
/// was made for.
pub struct PhotoSky<'a> {
    pub mask: &'a SkyMask,
    pub w: usize,
    pub h: usize,
}

/// Sky mask of a working image (the same call the point finder makes).
pub fn mask_of(work: &image::RgbImage) -> SkyMask {
    features::sky_mask(work, &crate::imgutil::to_gray(work))
}

/// Share of sky around the point in photo `i` (None when that photo has no mask yet).
fn score_in(photos: &[Option<PhotoSky>], i: usize, p: [f64; 2]) -> Option<f32> {
    let s = photos.get(i)?.as_ref()?;
    let (x, y) = engine::denorm_coord(p, s.w, s.h);
    Some(features::cloud_score(s.mask, x, y, s.w.max(s.h) as f32))
}

/// Points (not control areas) whose surroundings are sky or cloud in photo A or photo B. A point whose
/// photo has no mask is judged by the other photo only. The result is ordered by point index.
pub fn find(cps: &[ControlPoint], photos: &[Option<PhotoSky>], cfg: &SkyConfig) -> Vec<SkyPoint> {
    if !cfg.enabled {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (k, c) in cps.iter().enumerate() {
        if c.kind != CpKind::Point {
            continue;
        }
        let best = match (score_in(photos, c.a, c.pa), score_in(photos, c.b, c.pb)) {
            (Some(x), Some(y)) => x.max(y),
            (Some(x), None) | (None, Some(x)) => x,
            (None, None) => continue,
        };
        if best > cfg.threshold {
            out.push(SkyPoint { point: k, score: best });
        }
    }
    out
}

/// What a removal would do with the found points.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Removal {
    /// indices that may be removed
    pub remove: Vec<usize>,
    /// found, but placed by hand
    pub skipped_manual: usize,
    /// found, but protected
    pub skipped_protected: usize,
}

/// Splits the found points into those that may be removed and those that stay: protected points
/// always, points placed by hand unless `include_manual`.
pub fn removable(cps: &[ControlPoint], found: &[SkyPoint], include_manual: bool) -> Removal {
    let mut r = Removal::default();
    for s in found {
        let Some(c) = cps.get(s.point) else { continue };
        if c.locked {
            r.skipped_protected += 1;
        } else if c.manual && !include_manual {
            r.skipped_manual += 1;
        } else {
            r.remove.push(s.point);
        }
    }
    r
}

/// Text of the remove button: "Remove n", or why there is nothing to remove ("0 to remove (2 by hand, 1 protected)").
pub fn button_text(r: &Removal) -> String {
    if !r.remove.is_empty() {
        return format!("🗑 Remove {}", r.remove.len());
    }
    let mut why = Vec::new();
    if r.skipped_manual > 0 {
        why.push(format!("{} by hand", r.skipped_manual));
    }
    if r.skipped_protected > 0 {
        why.push(format!("{} protected", r.skipped_protected));
    }
    if why.is_empty() { "0 to remove".to_string() } else { format!("0 to remove ({})", why.join(", ")) }
}

/// Text for the status line after a removal.
pub fn removal_note(r: &Removal) -> String {
    let mut s = String::new();
    if r.skipped_protected > 0 {
        s += &format!(" {} protected on sky or cloud kept (protected points are never removed).", r.skipped_protected);
    }
    if r.skipped_manual > 0 {
        s += &format!(" {} placed by hand kept (tick \"include points placed by hand\" to remove them too).", r.skipped_manual);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{solid, texture};

    const W: u32 = 192;
    const H: u32 = 128;

    fn cp(a: usize, b: usize, pa: (f32, f32), pb: (f32, f32)) -> ControlPoint {
        ControlPoint {
            a,
            b,
            pa: engine::norm_coord(pa.0, pa.1, W as usize, H as usize),
            pb: engine::norm_coord(pb.0, pb.1, W as usize, H as usize),
            err: 0.0,
            manual: false,
            weight: 1.0,
            kind: CpKind::Point,
            conf: 0.0,
            shift: [0.0; 2],
            score: 0.0,
            locked: false,
        }
    }

    fn on() -> SkyConfig {
        SkyConfig { enabled: true, ..SkyConfig::default() }
    }

    /// top 40 rows blue sky or white cloud, below textured ground
    fn scene(sky: [u8; 3], seed: u64) -> image::RgbImage {
        let mut img = texture(W, H, seed);
        for y in 0..40 {
            for x in 0..W {
                img.put_pixel(x, y, image::Rgb(sky));
            }
        }
        img
    }

    fn masks(imgs: &[&image::RgbImage]) -> Vec<SkyMask> {
        imgs.iter().map(|i| mask_of(i)).collect()
    }

    fn view(m: &[SkyMask]) -> Vec<Option<PhotoSky<'_>>> {
        m.iter().map(|m| Some(PhotoSky { mask: m, w: W as usize, h: H as usize })).collect()
    }

    #[test]
    fn blue_sky_cloud_and_ground_points() {
        for (name, sky) in [("blue sky", [90u8, 140, 220]), ("cloud", [215, 215, 220])] {
            let (a, b) = (scene(sky, 5), scene(sky, 6));
            let m = masks(&[&a, &b]);
            let cps = vec![cp(0, 1, (60.0, 12.0), (70.0, 12.0)), cp(0, 1, (60.0, 100.0), (70.0, 100.0))];
            let found = find(&cps, &view(&m), &on());
            assert_eq!(found.iter().map(|s| s.point).collect::<Vec<_>>(), vec![0], "{name}");
        }
    }

    #[test]
    fn sky_in_only_one_photo_is_enough() {
        let a = scene([90, 140, 220], 5);
        let b = texture(W, H, 6); // ground everywhere
        let m = masks(&[&a, &b]);
        let cps = vec![cp(0, 1, (60.0, 12.0), (70.0, 12.0))];
        assert_eq!(find(&cps, &view(&m), &on()).len(), 1);
        // swapped sides give the same answer
        let cps = vec![cp(1, 0, (70.0, 12.0), (60.0, 12.0))];
        assert_eq!(find(&cps, &view(&m), &on()).len(), 1);
    }

    #[test]
    fn threshold_decides_at_the_cloud_edge() {
        let (a, b) = (scene([90, 140, 220], 5), scene([90, 140, 220], 6));
        let m = masks(&[&a, &b]);
        // near the sky edge (y ~ 40): part of the circle around the point lies on the ground
        let cps = vec![cp(0, 1, (60.0, 42.0), (60.0, 42.0))];
        let ph = view(&m);
        let low = find(&cps, &ph, &SkyConfig { enabled: true, threshold: 0.0, ..SkyConfig::default() });
        assert_eq!(low.len(), 1, "edge point must have a non-zero score");
        let sc = low[0].score;
        assert!(sc > 0.0 && sc < 1.0, "score {sc}");
        assert!(find(&cps, &ph, &SkyConfig { enabled: true, threshold: sc + 0.01, ..SkyConfig::default() }).is_empty());
        assert_eq!(find(&cps, &ph, &SkyConfig { enabled: true, threshold: sc - 0.01, ..SkyConfig::default() }).len(), 1);
    }

    #[test]
    fn disabled_finds_nothing_and_default_is_off() {
        assert!(!SkyConfig::default().enabled);
        assert_eq!(SkyConfig::default().threshold, features::CLOUD_THRESHOLD);
        let a = solid(W, H, [90, 140, 220]);
        let m = masks(&[&a, &a]);
        let cps = vec![cp(0, 1, (60.0, 12.0), (70.0, 12.0))];
        assert!(find(&cps, &view(&m), &SkyConfig::default()).is_empty());
    }

    #[test]
    fn missing_mask_uses_the_other_photo_or_skips() {
        let a = scene([90, 140, 220], 5);
        let m = masks(&[&a]);
        let ph = vec![Some(PhotoSky { mask: &m[0], w: W as usize, h: H as usize }), None];
        let cps = vec![cp(0, 1, (60.0, 12.0), (70.0, 100.0)), cp(0, 1, (60.0, 100.0), (70.0, 12.0))];
        let found = find(&cps, &ph, &on());
        assert_eq!(found.iter().map(|s| s.point).collect::<Vec<_>>(), vec![0]);
        assert!(find(&cps, &[None, None], &on()).is_empty());
    }

    #[test]
    fn control_areas_are_not_judged() {
        let a = solid(W, H, [90, 140, 220]);
        let m = masks(&[&a, &a]);
        let mut c = cp(0, 1, (60.0, 12.0), (70.0, 12.0));
        c.kind = CpKind::Area;
        assert!(find(&[c], &view(&m), &on()).is_empty());
    }

    #[test]
    fn hand_placed_and_protected_points_are_never_removable() {
        let a = solid(W, H, [90, 140, 220]);
        let m = masks(&[&a, &a]);
        let mut cps: Vec<ControlPoint> = (0..4).map(|i| cp(0, 1, (30.0 + 20.0 * i as f32, 20.0), (30.0 + 20.0 * i as f32, 20.0))).collect();
        cps[1].manual = true;
        cps[2].locked = true;
        cps[3].manual = true;
        cps[3].locked = true;
        let found = find(&cps, &view(&m), &on());
        assert_eq!(found.len(), 4);
        let r = removable(&cps, &found, false);
        assert_eq!(r.remove, vec![0]);
        assert_eq!(r.skipped_manual, 1);
        assert_eq!(r.skipped_protected, 2);
        let note = removal_note(&r);
        assert!(note.contains("2 protected") && note.contains("1 placed by hand"), "{note}");
        assert_eq!(removal_note(&Removal::default()), "");
        // with "include points placed by hand" the hand point goes too, protected ones still stay
        let r = removable(&cps, &found, true);
        assert_eq!((r.remove, r.skipped_manual, r.skipped_protected), (vec![0, 1], 0, 2));
    }

    #[test]
    fn button_says_why_nothing_can_be_removed() {
        assert_eq!(button_text(&Removal { remove: vec![3, 4], ..Default::default() }), "🗑 Remove 2");
        assert_eq!(button_text(&Removal::default()), "0 to remove");
        assert_eq!(button_text(&Removal { remove: vec![], skipped_manual: 2, skipped_protected: 1 }), "0 to remove (2 by hand, 1 protected)");
        assert_eq!(button_text(&Removal { remove: vec![], skipped_manual: 0, skipped_protected: 1 }), "0 to remove (1 protected)");
    }

    #[test]
    fn config_reads_old_settings_without_the_field() {
        let c: SkyConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(c, SkyConfig::default());
    }
}
