//! Detection of control points that sit on top of each other.
//!
//! Two points of the same photo pair that lie on the same spot in both photos say the same thing twice
//! (the optimiser counts the spot twice, the list gets cluttered). Two points that lie on the same spot in
//! one photo but on different spots in the other contradict each other. Both cases are only reported:
//! the caller decides what to remove, and points placed by hand are never offered for removal.
//! Control areas (grid patches) are not looked at, they overlap by design.

use crate::align::{ControlPoint, CpKind};
use crate::engine::WORK_SIZE;

/// Settings of the check.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DupConfig {
    /// switched off = nothing is reported
    pub enabled: bool,
    /// two positions closer than this count as the same spot (pixels of the ~1600 px working image)
    pub tolerance_px: f64,
}

impl Default for DupConfig {
    fn default() -> Self {
        DupConfig { enabled: true, tolerance_px: 2.0 }
    }
}

/// What the second point of a [`Duplicate`] is to the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DupKind {
    /// same spot in both photos
    Same,
    /// same spot in one photo, a different spot in the other
    Contradicts,
}

/// `point` lies on the spot of `other` (the better of the two, which stays).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Duplicate {
    pub point: usize,
    pub other: usize,
    pub kind: DupKind,
}

/// Position of a point in the lower-numbered photo and in the higher-numbered one, so that points of the
/// same pair compare no matter which way round they were stored.
fn oriented(c: &ControlPoint) -> ((usize, usize), [f64; 2], [f64; 2]) {
    if c.a <= c.b { ((c.a, c.b), c.pa, c.pb) } else { ((c.b, c.a), c.pb, c.pa) }
}

fn close(p: [f64; 2], q: [f64; 2], tol: f64) -> bool {
    (p[0] - q[0]).abs() <= tol && (p[1] - q[1]).abs() <= tol
}

/// Finds points on top of each other. Within a pair the better point of a group stays: placed by hand first,
/// then the smaller alignment error, the higher match score and weight, the earlier one in the list. The
/// result is ordered by `point`.
pub fn find(cps: &[ControlPoint], cfg: &DupConfig) -> Vec<Duplicate> {
    if !cfg.enabled {
        return Vec::new();
    }
    let tol = cfg.tolerance_px.max(0.0) / WORK_SIZE as f64;
    let mut pairs: std::collections::HashMap<(usize, usize), Vec<usize>> = std::collections::HashMap::new();
    for (k, c) in cps.iter().enumerate() {
        if c.kind == CpKind::Point {
            pairs.entry(oriented(c).0).or_default().push(k);
        }
    }
    let mut out = Vec::new();
    for mut idx in pairs.into_values() {
        if idx.len() < 2 {
            continue;
        }
        // best first, so the first point of a group is the one that stays
        idx.sort_by(|&x, &y| {
            let (p, q) = (&cps[x], &cps[y]);
            q.manual
                .cmp(&p.manual)
                .then(p.err.total_cmp(&q.err))
                .then(q.score.total_cmp(&p.score))
                .then(q.weight.total_cmp(&p.weight))
                .then(x.cmp(&y))
        });
        let mut kept: Vec<usize> = Vec::new();
        for &k in &idx {
            let (_, pl, ph) = oriented(&cps[k]);
            let mut hit: Option<Duplicate> = None;
            for &o in &kept {
                let (_, ol, oh) = oriented(&cps[o]);
                let (l, h) = (close(pl, ol, tol), close(ph, oh, tol));
                let kind = match (l, h) {
                    (true, true) => DupKind::Same,
                    (true, false) | (false, true) => DupKind::Contradicts,
                    _ => continue,
                };
                // a duplicate of something counts for more than a contradiction
                if hit.is_none() || kind == DupKind::Same {
                    hit = Some(Duplicate { point: k, other: o, kind });
                }
                if kind == DupKind::Same {
                    break;
                }
            }
            match hit {
                Some(d) => out.push(d),
                None => kept.push(k),
            }
        }
    }
    out.sort_by_key(|d| d.point);
    out
}

/// The points that can be removed without losing information: exact duplicates that were not placed by hand.
/// Sorted, without repeats.
pub fn removable(cps: &[ControlPoint], dups: &[Duplicate]) -> Vec<usize> {
    let mut v: Vec<usize> = dups.iter().filter(|d| d.kind == DupKind::Same && !cps[d.point].manual).map(|d| d.point).collect();
    v.sort_unstable();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(a: usize, b: usize, pa: [f64; 2], pb: [f64; 2], manual: bool) -> ControlPoint {
        ControlPoint::new(a, b, pa, pb, manual)
    }

    const PX: f64 = 1.0 / WORK_SIZE as f64;

    #[test]
    fn the_same_spot_twice_is_one_duplicate_and_the_first_stays() {
        let cps = [pt(0, 1, [0.1, 0.1], [-0.3, 0.1], false), pt(0, 1, [0.1 + 0.5 * PX, 0.1], [-0.3, 0.1 - 0.5 * PX], false)];
        let d = find(&cps, &DupConfig::default());
        assert_eq!(d, vec![Duplicate { point: 1, other: 0, kind: DupKind::Same }]);
        assert_eq!(removable(&cps, &d), vec![1]);
    }

    #[test]
    fn the_stored_direction_does_not_matter() {
        let cps = [pt(0, 1, [0.1, 0.1], [-0.3, 0.1], false), pt(1, 0, [-0.3, 0.1], [0.1, 0.1], false)];
        assert_eq!(find(&cps, &DupConfig::default()).len(), 1);
    }

    #[test]
    fn other_pairs_and_far_points_are_not_reported() {
        let cps = [
            pt(0, 1, [0.1, 0.1], [-0.3, 0.1], false),
            pt(0, 2, [0.1, 0.1], [-0.3, 0.1], false), // same spot in photo 0, but another partner: a normal triple point
            pt(0, 1, [0.1 + 5.0 * PX, 0.1], [-0.3 + 5.0 * PX, 0.1], false), // 5 px away
        ];
        assert!(find(&cps, &DupConfig::default()).is_empty());
    }

    #[test]
    fn one_end_on_the_same_spot_is_a_contradiction_and_not_removable() {
        let cps = [pt(0, 1, [0.1, 0.1], [-0.3, 0.1], false), pt(0, 1, [0.1, 0.1], [-0.2, 0.0], false)];
        let d = find(&cps, &DupConfig::default());
        assert_eq!(d, vec![Duplicate { point: 1, other: 0, kind: DupKind::Contradicts }]);
        assert!(removable(&cps, &d).is_empty());
    }

    #[test]
    fn a_hand_placed_point_stays_and_is_never_offered_for_removal() {
        let (auto, hand) = (pt(0, 1, [0.1, 0.1], [-0.3, 0.1], false), pt(0, 1, [0.1, 0.1], [-0.3, 0.1], true));
        // the hand-placed one wins although it comes later
        let cps = [auto, hand];
        let d = find(&cps, &DupConfig::default());
        assert_eq!(d, vec![Duplicate { point: 0, other: 1, kind: DupKind::Same }]);
        assert_eq!(removable(&cps, &d), vec![0]);
        // two hand-placed ones: reported, but nothing is offered for removal
        let cps = [hand, hand];
        let d = find(&cps, &DupConfig::default());
        assert_eq!(d.len(), 1);
        assert!(removable(&cps, &d).is_empty());
    }

    #[test]
    fn the_smaller_error_stays_and_three_on_one_spot_leave_one() {
        let mut cps = vec![pt(0, 1, [0.1, 0.1], [-0.3, 0.1], false); 3];
        cps[0].err = 2.0;
        cps[1].err = 0.5;
        cps[2].err = 1.0;
        let d = find(&cps, &DupConfig::default());
        assert_eq!(removable(&cps, &d), vec![0, 2]);
        assert!(d.iter().all(|x| x.other == 1));
    }

    #[test]
    fn control_areas_and_the_switch_are_respected() {
        let mut area = pt(0, 1, [0.1, 0.1], [-0.3, 0.1], false);
        area.kind = CpKind::Area;
        let cps = [area, area];
        assert!(find(&cps, &DupConfig::default()).is_empty());
        let cps = [pt(0, 1, [0.1, 0.1], [-0.3, 0.1], false); 2];
        assert!(find(&cps, &DupConfig { enabled: false, ..DupConfig::default() }).is_empty());
        assert_eq!(find(&cps, &DupConfig::default()).len(), 1);
    }

    #[test]
    fn the_tolerance_is_in_working_pixels() {
        let cps = [pt(0, 1, [0.1, 0.1], [-0.3, 0.1], false), pt(0, 1, [0.1 + 3.0 * PX, 0.1], [-0.3 + 3.0 * PX, 0.1], false)];
        assert!(find(&cps, &DupConfig { tolerance_px: 2.0, ..DupConfig::default() }).is_empty());
        let d = find(&cps, &DupConfig { tolerance_px: 4.0, ..DupConfig::default() });
        assert_eq!(d.len(), 1);
    }
}
