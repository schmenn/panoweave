//! "Snap selected": refines the selected points with the same correlation search as after
//! dragging a point (`snap_point`), as a job on a worker thread.
//!
//! The right-hand photo of the pair on screen is searched, the left one stays (for points of
//! other pairs: the photo `b` of the point). A result is only taken when the match reaches
//! `guided::FINETUNE_MIN_NCC`. Protected points are skipped. The job works on copies; the result
//! is applied only to points that are still exactly as they were when the job started.

use super::*;
use crate::imgutil::Gray;
use super::points_predict::rotation_guess_with;
use rayon::prelude::*;
use std::collections::HashMap;

/// One point to refine, as it was when the job started.
pub(super) struct SnapItem {
    pub(super) k: usize,
    /// (photo a, photo b, position a, position b) of the point at the start
    pub(super) before: (usize, usize, [f64; 2], [f64; 2]),
    pub(super) fixed: usize,
    pub(super) moving: usize,
    /// work-image pixels
    pub(super) pf: P2,
    pub(super) pm: P2,
}

/// What a photo pair contributes (copied, so the worker never touches the app).
pub(super) struct SnapPair {
    pub(super) gf: Arc<Gray>,
    pub(super) gm: Arc<Gray>,
    /// the other points of the pair, fixed -> moving, work pixels
    pub(super) seeds: Vec<(P2, P2)>,
    pub(super) scale: f64,
    /// longer side of the fixed work image
    pub(super) long_side: f64,
    pub(super) radius: f64,
    pub(super) tpl_size: i32,
}

pub(super) struct SnapPlan {
    pub(super) items: Vec<SnapItem>,
    pub(super) pairs: HashMap<(usize, usize), SnapPair>,
    pub(super) protected: usize,
    /// `points_select::order_fingerprint` of the points at the start
    pub(super) fp: u64,
}

pub(super) struct SnapOutcome {
    pub(super) k: usize,
    pub(super) before: (usize, usize, [f64; 2], [f64; 2]),
    pub(super) moving: usize,
    /// new position in work pixels and the match, `None` = match too low / flat area
    pub(super) new: Option<(P2, f32)>,
}

pub(super) struct SnapDone {
    pub(super) fp: u64,
    pub(super) protected: usize,
    pub(super) outcomes: Vec<SnapOutcome>,
}

/// Refines one point. Same steps as `snap_point`: local zoom/tilt from the other points of the
/// pair (else a measured rotation), then the correlation search; the result needs NCC >= `guided::FINETUNE_MIN_NCC`.
pub(super) fn snap_one(pair: &SnapPair, soft: &(Option<Gray>, Option<Gray>), it: &SnapItem) -> Option<(P2, f32)> {
    let seeds: Vec<(P2, P2)> = pair.seeds.iter().copied().filter(|s| (s.0 .0 - it.pf.0).hypot(s.0 .1 - it.pf.1) > 0.01).collect();
    let c = guided::Corr { fixed: &pair.gf, fixed_soft: soft.0.as_ref(), moving: &pair.gm, moving_soft: soft.1.as_ref(), tpl_r: pair.tpl_size / 2 };
    let j = if seeds.len() >= 2 { guided::estimate(&seeds, pair.scale, pair.long_side).map(|t| guided::jacobian(&t, it.pf)) } else { None }
        .unwrap_or_else(|| rotation_guess_with(&c, pair.scale, it.pf, it.pm));
    guided::refine(&c, it.pf, j, it.pm, pair.radius).map(|(x, y, v)| ((x, y), v)).filter(|r| r.1 >= guided::FINETUNE_MIN_NCC)
}

/// Runs the plan. `None` = cancelled (nothing to apply).
pub(super) fn run_snap(plan: SnapPlan, report: &(dyn Fn(f32, &str) + Sync), cancel: &AtomicBool) -> Option<SnapDone> {
    let stop = || cancel.load(Ordering::Relaxed) || engine::cancelled();
    // the soft copies for photos of very different zoom: once per pair, not once per point
    let softs: HashMap<(usize, usize), (Option<Gray>, Option<Gray>)> = plan
        .pairs
        .iter()
        .map(|(key, p)| {
            let s = p.scale;
            let fs = (s < 1.0 / 1.5).then(|| imgutil::blur(&p.gf, guided::soft_sigma(1.0 / s)));
            let ms = (s > 1.5).then(|| imgutil::blur(&p.gm, guided::soft_sigma(s)));
            (*key, (fs, ms))
        })
        .collect();
    let n = plan.items.len().max(1);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let outcomes: Vec<Option<SnapOutcome>> = plan
        .items
        .par_iter()
        .map(|it| {
            if stop() {
                return None;
            }
            let key = (it.fixed, it.moving);
            let new = snap_one(&plan.pairs[&key], &softs[&key], it);
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            report(d as f32 / n as f32, &format!("Snapping point {d} of {n}"));
            Some(SnapOutcome { k: it.k, before: it.before, moving: it.moving, new })
        })
        .collect();
    if stop() || outcomes.iter().any(|o| o.is_none()) {
        return None;
    }
    Some(SnapDone { fp: plan.fp, protected: plan.protected, outcomes: outcomes.into_iter().flatten().collect() })
}

/// The text after a snap run.
pub(super) fn snap_message(snapped: usize, low: usize, protected: usize, stale: usize) -> String {
    let mut s = format!("{snapped} snapped, {low} unchanged (match too low), {protected} protected skipped.");
    if stale > 0 {
        s.push_str(&format!(" {stale} changed while the job ran and were left alone."));
    }
    s
}

impl PanoApp {
    /// Collects everything the worker needs for the points `idx` (cheap, no image work).
    pub(super) fn plan_snap(&self, idx: &[usize]) -> SnapPlan {
        let mut plan = SnapPlan { items: Vec::new(), pairs: HashMap::new(), protected: 0, fp: points_select::order_fingerprint(&self.cps) };
        let (sa, sb) = (self.pe.a, self.pe.b);
        for &k in idx {
            let Some(c) = self.cps.get(k).copied() else { continue };
            if c.locked {
                plan.protected += 1;
                continue;
            }
            let moving = if (c.a == sa && c.b == sb) || (c.a == sb && c.b == sa) { sb } else { c.b };
            let fixed = if moving == c.b { c.a } else { c.b };
            let (pf_n, pm_n) = if fixed == c.a { (c.pa, c.pb) } else { (c.pb, c.pa) };
            plan.items.push(SnapItem { k, before: (c.a, c.b, c.pa, c.pb), fixed, moving, pf: self.to_px(fixed, pf_n), pm: self.to_px(moving, pm_n) });
            plan.pairs.entry((fixed, moving)).or_insert_with(|| {
                let (w, h) = self.work_dims(fixed);
                SnapPair {
                    gf: self.photos[fixed].gray.clone(),
                    gm: self.photos[moving].gray.clone(),
                    seeds: self.pair_seeds(fixed, moving),
                    scale: self.scale_prior(fixed, moving),
                    long_side: w.max(h) as f64,
                    radius: self.local_radius(moving),
                    tpl_size: self.tpl_size,
                }
            });
        }
        plan
    }

    /// Button "Snap selected": starts the job for the selected points.
    pub(super) fn snap_selected(&mut self) {
        if self.busy() {
            self.set_status("Please wait until the current task has finished.", WARN);
            return;
        }
        let idx = self.selection_indices();
        if idx.is_empty() {
            return;
        }
        let plan = self.plan_snap(&idx);
        if plan.items.is_empty() {
            self.set_status(snap_message(0, 0, plan.protected, 0), WARN);
            return;
        }
        self.start_job("Snapping points", move |report, cancel| match run_snap(plan, report, cancel) {
            Some(d) => JobMsg::Snapped(Box::new(d)),
            None => JobMsg::Cancelled,
        });
    }

    /// The job's result: moves the points that are still as they were, as one undo step.
    pub(super) fn on_snapped(&mut self, done: SnapDone) {
        if done.fp != points_select::order_fingerprint(&self.cps) {
            self.set_status("Snap selected: the points were changed while it ran, so nothing was applied.", WARN);
            return;
        }
        let (mut snapped, mut low, mut stale) = (0usize, 0usize, 0usize);
        let n = done.outcomes.len();
        self.edit_cps(&format!("snapping {n} points"), |s| {
            for o in &done.outcomes {
                let still = s.cps.get(o.k).is_some_and(|c| !c.locked && (c.a, c.b, c.pa, c.pb) == o.before);
                if !still {
                    stale += 1;
                    continue;
                }
                match o.new {
                    Some((p, _)) => {
                        let nn = s.to_norm(o.moving, p);
                        let c = &mut s.cps[o.k];
                        if c.a == o.moving {
                            c.pa = nn;
                        } else {
                            c.pb = nn;
                        }
                        snapped += 1;
                    }
                    None => low += 1,
                }
            }
        });
        self.set_status(format!("{} Ctrl+Z undoes it; align again (step 3) to update.", snap_message(snapped, low, done.protected, stale)), if snapped > 0 { GOOD } else { WARN });
    }
}

#[cfg(test)]
mod tests {
    use super::super::masks_tests::app_with_photos;
    use super::*;

    fn pair_of(g: Gray) -> SnapPair {
        let g = Arc::new(g);
        let long_side = g.w.max(g.h) as f64;
        SnapPair { gf: g.clone(), gm: g, seeds: vec![], scale: 1.0, long_side, radius: 8.0, tpl_size: 21 }
    }

    fn item(pf: P2, pm: P2) -> SnapItem {
        SnapItem { k: 0, before: (0, 1, [0.0; 2], [0.0; 2]), fixed: 0, moving: 1, pf, pm }
    }

    #[test]
    fn snap_one_pulls_a_displaced_point_back_and_rejects_flat_areas() {
        let g = imgutil::to_gray(&crate::testutil::texture(300, 300, 7));
        let pair = pair_of(g);
        let none = (None, None);
        let r = snap_one(&pair, &none, &item((150.0, 150.0), (153.0, 152.0))).expect("textured spot snaps");
        assert!((r.0 .0 - 150.0).hypot(r.0 .1 - 150.0) < 0.3, "got {:?}", r);
        assert!(r.1 >= guided::FINETUNE_MIN_NCC);
        // a flat photo: nothing to lock onto, so the point stays
        let flat = pair_of(Gray { w: 100, h: 100, data: vec![0.5; 100 * 100] });
        assert!(snap_one(&flat, &none, &item((50.0, 50.0), (52.0, 50.0))).is_none());
    }

    #[test]
    fn run_snap_reports_and_honours_cancel() {
        let g = imgutil::to_gray(&crate::testutil::texture(300, 300, 7));
        let mk = || {
            let mut pairs = HashMap::new();
            pairs.insert((0, 1), pair_of(g.clone()));
            SnapPlan { items: vec![item((150.0, 150.0), (152.0, 151.0)), item((100.0, 120.0), (101.0, 120.0))], pairs, protected: 3, fp: 0 }
        };
        let seen = Mutex::new(0usize);
        let report = |_: f32, _: &str| *seen.lock().unwrap() += 1;
        let d = run_snap(mk(), &report, &AtomicBool::new(false)).expect("not cancelled");
        assert_eq!(d.outcomes.len(), 2);
        assert_eq!(d.protected, 3);
        assert_eq!(*seen.lock().unwrap(), 2);
        assert!(run_snap(mk(), &report, &AtomicBool::new(true)).is_none());
    }

    fn app() -> PanoApp {
        let mut app = app_with_photos(2);
        app.cps = (0..6).map(|i| ControlPoint::new(0, 1, [i as f64 * 0.01, 0.1], [0.2, i as f64 * 0.02], i == 2)).collect();
        app.cps[1].locked = true;
        app.needs_align = false;
        app.pe.a = 0;
        app.pe.b = 1;
        app
    }

    #[test]
    fn plan_skips_protected_and_keeps_hand_placed_points() {
        let a = app();
        let plan = a.plan_snap(&[0, 1, 2, 99]);
        assert_eq!(plan.protected, 1);
        assert_eq!(plan.items.iter().map(|i| i.k).collect::<Vec<_>>(), vec![0, 2]);
        assert!(plan.items.iter().all(|i| i.fixed == 0 && i.moving == 1));
    }

    fn mk(a: &PanoApp, k: usize, new: Option<(P2, f32)>) -> SnapOutcome {
        let c = a.cps[k];
        SnapOutcome { k, before: (c.a, c.b, c.pa, c.pb), moving: 1, new }
    }

    #[test]
    fn result_is_one_undo_step_and_stale_points_are_left_alone() {
        let mut a = app();
        let fp = points_select::order_fingerprint(&a.cps);
        let outcomes = vec![mk(&a, 0, Some(((10.0, 12.0), 0.9))), mk(&a, 2, None), mk(&a, 3, Some(((20.0, 20.0), 0.95)))];
        // point 3 is dragged while the job runs
        a.cps[3].pb = [0.9, 0.9];
        let moved = a.cps[3].pb;
        let before0 = a.cps[0].pb;
        a.on_snapped(SnapDone { fp, protected: 1, outcomes });
        assert_ne!(a.cps[0].pb, before0);
        assert_eq!(a.cps[3].pb, moved);
        assert!(a.status.contains("1 snapped, 1 unchanged (match too low), 1 protected skipped"), "{}", a.status);
        assert!(a.status.contains("1 changed while"), "{}", a.status);
        assert!(a.needs_align);
        a.undo_last();
        assert_eq!(a.cps[0].pb, before0);
        assert!(!a.undo.can_undo(), "exactly one step");
        // the list changed while the job ran: everything is dropped
        let fp_old = fp.wrapping_add(1);
        let snapshot = a.cps.clone();
        a.on_snapped(SnapDone { fp: fp_old, protected: 0, outcomes: vec![mk(&a, 0, Some(((1.0, 1.0), 0.9)))] });
        assert!(a.cps.iter().zip(&snapshot).all(|(x, y)| x.pb == y.pb));
        assert!(a.status.contains("nothing was applied"));
    }
}
