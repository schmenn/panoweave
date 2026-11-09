//! "Maximum accuracy": tries every way the program has to place the photos instead of only one and
//! rates each with the independent check (`fine_align::quality` / `--accuracy`), then proposes the best.
//! Nothing is applied here; the caller (the app asks the person, `--accuracy-search` prints a table) decides.
//!
//! The ways (`Strategy`), each with and without the fine alignment (`PathSpec::fine`):
//! - `Joint`: all photos together, weighted – what "Align photos" does.
//! - `Scaffold`: first a frame from the protected points alone, then the other photos are attached to it.
//! - `Chain`: from a start photo, photo by photo (strongest link first, or outward in shooting order).
//!   Every new photo is placed from its best link and then **all photos placed so far are optimised
//!   together again**, so a later photo that shows an earlier one sitting crooked pulls it back
//!   ("forward and backward like git"). After the last photo the usual full optimisation (lens, outlier
//!   removal) runs from that result.
//! - `Weighted(w)`: `Joint` with another weight of the protected points (comparison, e.g. PR 69: 1 against 30).
//! - `Current`: the alignment the project has now (only when the caller gives it), so "keep" can be compared.
//!
//! **Like for like.** Each way is measured on the *same* fixed spots (made once from the joint result).
//! The ranking uses only the spots that every finished way could measure (`Stats` "common"); a way
//! that loses spots the others could measure is shown in `lost`. Medians over different spot sets would
//! not be comparable (PR 69 had 2675 against 1605 spots).
//!
//! The number of ways is limited by `SearchConfig` (start photos, time budget); ways that were not run are
//! listed as skipped with the reason, nothing is dropped silently. Parallax mode is not scored: the check
//! measures the rotation model only. The foliage filter is a hook (`foliage_points`) until that feature exists.

use crate::align::{self, AlignInput, ControlPoint, CpKind, Lens, OptimizeOptions};
use crate::engine::{self, AlignResult, AlignSettings};
use crate::fine_align::{self, FaPhoto, FineStats};
use crate::geometry::{M3, horn_rotation, mm, orthonormalize};
use rayon::prelude::*;
use std::time::Instant;

/// Shift a lost spot (no valid match: the check searches 6 px around) counts as in `Row::mean_with_lost`.
pub const LOST_PX: f32 = 6.0;

/// Settings of the search (the app shows the first three).
#[derive(Clone, Debug)]
pub struct SearchConfig {
    /// photos used as the start of a chain (the ones with the most protected / all points)
    pub max_starts: usize,
    /// no new way is begun after this many seconds (the one running is finished)
    pub time_budget_s: u64,
    /// also try every way with the fine alignment
    pub with_fine: bool,
    /// iterations of the joint re-optimisation after each attached photo
    pub retro_iterations: usize,
    /// long side of the images the check measures on (3200 = like `--accuracy`; falls back to the working images when memory is short)
    pub check_size: u32,
    /// more weights of the protected points to compare as extra ways (empty = none)
    pub extra_weights: Vec<f64>,
    /// leave out the `Chain` ways (comparison runs)
    pub chains: bool,
}

impl Default for SearchConfig {
    fn default() -> Self {
        SearchConfig { max_starts: 3, time_budget_s: 1800, with_fine: true, retro_iterations: 12, check_size: 3200, extra_weights: Vec::new(), chains: true }
    }
}

/// Order in which a chain takes the next photo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// always the photo with the most points to the ones already placed (grows in both directions)
    Strongest,
    /// start+1, start-1, start+2, start-2 ... (shooting order, outward in both directions)
    Outward,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Strategy {
    Current,
    Joint,
    Weighted(f64),
    Scaffold,
    Chain { start: usize, order: Order },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PathSpec {
    pub strategy: Strategy,
    pub fine: bool,
    /// foliage filter on (hook, see `foliage_points`)
    pub foliage: bool,
}

impl PathSpec {
    pub fn label(&self) -> String {
        let base = match &self.strategy {
            Strategy::Current => "Current alignment".to_string(),
            Strategy::Joint => "All photos together".to_string(),
            Strategy::Weighted(w) => format!("All photos together, protected x{w}"),
            Strategy::Scaffold => "Protected points first, rest attached".to_string(),
            Strategy::Chain { start, order: Order::Strongest } => format!("Chain from photo {}, strongest link", start + 1),
            Strategy::Chain { start, order: Order::Outward } => format!("Chain from photo {}, outward in order", start + 1),
        };
        let mut s = base;
        if self.strategy != Strategy::Current {
            s.push_str(if self.fine { " + fine alignment" } else { "" });
        }
        if self.foliage {
            s.push_str(" + foliage filter");
        }
        s
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Done,
    /// same rotations as the way with this row number (not measured twice)
    SameAs(usize),
    Skipped(String),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub n: usize,
    pub mean: f32,
    pub median: f32,
    pub p90: f32,
}

impl Stats {
    pub fn of(mut v: Vec<f32>) -> Stats {
        v.sort_by(f32::total_cmp);
        if v.is_empty() {
            return Stats { n: 0, mean: f32::NAN, median: f32::NAN, p90: f32::NAN };
        }
        let at = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
        Stats { n: v.len(), mean: v.iter().sum::<f32>() / v.len() as f32, median: at(0.5), p90: at(0.9) }
    }
}

pub struct Row {
    pub spec: PathSpec,
    pub outcome: Outcome,
    pub secs: f32,
    /// what it would apply (None for skipped / failed / same-as rows and for `Current`, which is already applied)
    pub result: Option<AlignResult>,
    /// shift at every fixed spot (working-image px), `None` = no valid match there
    pub shifts: Vec<Option<f32>>,
    /// over the spots this way could measure
    pub own: Stats,
    /// over the spots every finished way could measure (the ranking)
    pub common: Stats,
    /// spots another way could measure but this one could not
    pub lost: usize,
    /// mean shift over every spot that any way could measure, a lost spot counting `LOST_PX`: unlike `common` it does
    /// not look better for a way that loses the spots it fits worst
    pub mean_with_lost: f32,
    /// protected points: (count, mean error, max error) in original photo pixels
    pub protected: Option<(usize, f64, f64)>,
}

pub struct SearchReport {
    pub rows: Vec<Row>,
    pub spots: usize,
    pub common_spots: usize,
    /// row with the smallest median on the common spots
    pub best: Option<usize>,
    pub cancelled: bool,
    pub secs: f32,
    pub notes: Vec<String>,
}

/// What the search needs to know about the project; the measuring is passed in so the search can be tested
/// without photos.
pub struct Env<'a> {
    pub lens_of: &'a [usize],
    pub lenses: &'a [Lens],
    pub long: &'a [f64],
    pub cps: &'a [ControlPoint],
    pub settings: AlignSettings,
    /// shift per fixed spot for an alignment (spots are fixed on the first call, which is the joint result)
    pub measure: &'a (dyn Fn(&[M3], &[Lens]) -> Vec<Option<f32>> + Sync),
    /// the fine alignment: rotations, lenses, points, protected weight -> improved ones; `None` = not available
    pub fine: Option<&'a FineFn<'a>>,
    /// the alignment the project has now (rotations, lenses), if the caller has one
    pub current: Option<(Vec<M3>, Vec<Lens>)>,
}

pub type FineFn<'a> = dyn Fn(&[M3], &[Lens], &[ControlPoint], f64) -> (Vec<M3>, Vec<Lens>, Vec<ControlPoint>, FineStats) + Sync + 'a;

/// Hook for the foliage filter (feature/foliage): return the points with foliage left out or down-weighted, or
/// `None` while the filter does not exist. Every way would then also be tried with it.
pub fn foliage_points(_cps: &[ControlPoint]) -> Option<Vec<ControlPoint>> {
    None
}

// ---------------------------------------------------------------------------------------------
// placing the photos
// ---------------------------------------------------------------------------------------------

/// Link strengths between photos: weighted point counts (protected points count `locked_weight` times) and
/// the number of protected points.
struct Links {
    w: Vec<Vec<f64>>,
    all: Vec<Vec<usize>>,
    locked: Vec<Vec<usize>>,
}

impl Links {
    fn new(n: usize, cps: &[ControlPoint], locked_weight: f64) -> Links {
        let mut l = Links { w: vec![vec![0.0; n]; n], all: vec![vec![0; n]; n], locked: vec![vec![0; n]; n] };
        for c in cps.iter().filter(|c| c.a < n && c.b < n && c.a != c.b) {
            let w = if c.locked { locked_weight.max(1.0) } else { 1.0 };
            for (i, j) in [(c.a, c.b), (c.b, c.a)] {
                l.w[i][j] += w;
                l.all[i][j] += 1;
                if c.locked {
                    l.locked[i][j] += 1;
                }
            }
        }
        l
    }
}

/// Attach the photos not `done` yet, one by one, to the placed ones (see the module comment). `rots` holds a
/// placement for the photos that never get connected. Returns false when cancelled.
#[allow(clippy::too_many_arguments)]
fn attach_rest(inp: &AlignInput, lenses: &[Lens], cps: &[ControlPoint], rots: &mut [M3], done: &mut [bool], anchor: usize, start: usize, order: Order, iters: usize, s: &AlignSettings) -> bool {
    let n = inp.n;
    let links = Links::new(n, cps, s.locked_weight);
    // alternating start+1, start-1, start+2, ...
    let outward: Vec<usize> = {
        let mut v = Vec::new();
        for d in 1..n {
            if start + d < n {
                v.push(start + d);
            }
            if start >= d {
                v.push(start - d);
            }
        }
        v
    };
    let opts = OptimizeOptions { focal: false, distortion: false, cauchy: false, locked_weight: s.locked_weight };
    loop {
        if engine::cancelled() {
            return false;
        }
        let score = |j: usize| -> f64 { (0..n).filter(|&i| done[i]).map(|i| links.w[j][i]).sum() };
        let pick = match order {
            Order::Outward => outward.iter().copied().find(|&j| !done[j] && score(j) > 0.0),
            Order::Strongest => None,
        }
        .or_else(|| (0..n).filter(|&j| !done[j] && score(j) > 0.0).max_by(|&a, &b| score(a).total_cmp(&score(b))));
        let Some(j) = pick else { return true };
        // the placed photo with the strongest link; a pair with enough protected points uses only those
        let i = (0..n)
            .filter(|&i| done[i] && links.all[j][i] > 0)
            .max_by_key(|&i| (links.locked[j][i] >= align::LOCKED_START_MIN, links.all[j][i]))
            .expect("score > 0");
        let only_locked = links.locked[j][i] >= align::LOCKED_START_MIN;
        let mut pairs = Vec::new();
        for c in cps.iter().filter(|c| !only_locked || c.locked) {
            if c.a == i && c.b == j {
                pairs.push((align::ray(&lenses[inp.lens_of[i]], c.pa), align::ray(&lenses[inp.lens_of[j]], c.pb)));
            } else if c.a == j && c.b == i {
                pairs.push((align::ray(&lenses[inp.lens_of[i]], c.pb), align::ray(&lenses[inp.lens_of[j]], c.pa)));
            }
        }
        let rel = horn_rotation(&pairs);
        rots[j] = orthonormalize(&mm(&rel, &rots[i]));
        done[j] = true;
        // backward: everything placed so far is optimised together again with the new photo
        let placed: Vec<ControlPoint> = cps.iter().filter(|c| done[c.a] && done[c.b]).copied().collect();
        let mut ls = lenses.to_vec();
        align::optimize(inp, rots, &mut ls, &placed, anchor, &opts, iters);
    }
}

/// The start rotations of a way (`None` = the usual placement of "Align photos").
fn place(strategy: &Strategy, env: &Env, lenses: &[Lens], cfg: &SearchConfig, s: &AlignSettings) -> Result<Option<Vec<M3>>, String> {
    let n = env.lens_of.len();
    let inp = AlignInput { n, lens_of: env.lens_of, long_side: env.long };
    match strategy {
        Strategy::Current | Strategy::Joint | Strategy::Weighted(_) => Ok(None),
        Strategy::Scaffold => {
            let locked: Vec<ControlPoint> = env.cps.iter().filter(|c| c.locked).copied().collect();
            if locked.is_empty() {
                return Err("no protected points".into());
            }
            // the largest group of photos connected by protected points is the frame
            let comp = align::components(n, &locked);
            let touched: Vec<usize> = (0..n).filter(|&i| locked.iter().any(|c| c.a == i || c.b == i)).collect();
            let mut size = std::collections::HashMap::new();
            for &i in &touched {
                *size.entry(comp[i]).or_insert(0usize) += 1;
            }
            let (&big, _) = size.iter().max_by_key(|(c, k)| (**k, std::cmp::Reverse(**c))).expect("touched not empty");
            let frame: Vec<ControlPoint> = locked.iter().filter(|c| comp[c.a] == big).copied().collect();
            let mut cnt = vec![0usize; n];
            for c in &frame {
                cnt[c.a] += 1;
                cnt[c.b] += 1;
            }
            let anchor = (0..n).max_by_key(|&i| cnt[i]).unwrap_or(0);
            let mut rots = align::initial_rotations_ex(&inp, lenses, env.cps, anchor, s.locked_weight > 1.0);
            let framed = align::initial_rotations_ex(&inp, lenses, &frame, anchor, true);
            let mut done = vec![false; n];
            for i in 0..n {
                if frame.iter().any(|c| c.a == i || c.b == i) {
                    rots[i] = framed[i];
                    done[i] = true;
                }
            }
            let mut ls = lenses.to_vec();
            let opts = OptimizeOptions { focal: false, distortion: false, cauchy: false, locked_weight: s.locked_weight };
            align::optimize(&inp, &mut rots, &mut ls, &frame, anchor, &opts, 40);
            if !attach_rest(&inp, lenses, env.cps, &mut rots, &mut done, anchor, anchor, Order::Strongest, cfg.retro_iterations, s) {
                return Err("cancelled".into());
            }
            Ok(Some(rots))
        }
        Strategy::Chain { start, order } => {
            let mut rots = align::initial_rotations_ex(&inp, lenses, env.cps, *start, s.locked_weight > 1.0);
            let mut done = vec![false; n];
            done[*start] = true;
            rots[*start] = crate::geometry::IDENTITY;
            if !attach_rest(&inp, lenses, env.cps, &mut rots, &mut done, *start, *start, *order, cfg.retro_iterations, s) {
                return Err("cancelled".into());
            }
            Ok(Some(rots))
        }
    }
}

/// Photos to start chains from: the ones with the most protected points, then the most points.
pub fn start_photos(n: usize, cps: &[ControlPoint], max: usize) -> Vec<usize> {
    let mut score = vec![(0usize, 0usize); n];
    for c in cps.iter().filter(|c| c.a < n && c.b < n) {
        for i in [c.a, c.b] {
            score[i].1 += 1;
            if c.locked {
                score[i].0 += 1;
            }
        }
    }
    let mut idx: Vec<usize> = (0..n).filter(|&i| score[i].1 > 0).collect();
    idx.sort_by(|&a, &b| score[b].cmp(&score[a]).then(a.cmp(&b)));
    idx.truncate(max);
    idx
}

/// The ways without fine alignment, in the order they are run (the first is always the joint one).
pub fn specs(n: usize, cps: &[ControlPoint], cfg: &SearchConfig, have_current: bool) -> Vec<PathSpec> {
    let sp = |strategy| PathSpec { strategy, fine: false, foliage: false };
    let mut v = Vec::new();
    if have_current {
        v.push(sp(Strategy::Current));
    }
    v.push(sp(Strategy::Joint));
    for &w in &cfg.extra_weights {
        v.push(sp(Strategy::Weighted(w)));
    }
    if cps.iter().any(|c| c.locked) {
        v.push(sp(Strategy::Scaffold));
    }
    if cfg.chains {
        for start in start_photos(n, cps, cfg.max_starts) {
            for order in [Order::Strongest, Order::Outward] {
                v.push(sp(Strategy::Chain { start, order }));
            }
        }
    }
    v
}

fn protected_of(cps: &[ControlPoint]) -> Option<(usize, f64, f64)> {
    let e: Vec<f64> = cps.iter().filter(|c| c.locked && c.kind == CpKind::Point).map(|c| c.err).collect();
    if e.is_empty() {
        return None;
    }
    Some((e.len(), e.iter().sum::<f64>() / e.len() as f64, e.iter().cloned().fold(0.0, f64::max)))
}

fn rots_close(a: &[M3], b: &[M3]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (0..3).all(|i| (0..3).all(|j| (x[i][j] - y[i][j]).abs() < 1e-6)))
}

fn weight_of(strategy: &Strategy, s: &AlignSettings) -> f64 {
    match strategy {
        Strategy::Weighted(w) => *w,
        _ => s.locked_weight,
    }
}

/// Run the way's alignment (without fine alignment).
fn run_path(spec: &PathSpec, env: &Env, lenses: &[Lens], cfg: &SearchConfig) -> Result<AlignResult, String> {
    let s = AlignSettings { locked_weight: weight_of(&spec.strategy, &env.settings), ..env.settings };
    let start = place(&spec.strategy, env, lenses, cfg, &s)?;
    let r = engine::align_all_from(env.lens_of, lenses, env.long, env.cps, &s, start, &|_, _| {});
    if engine::cancelled() { Err("cancelled".into()) } else { Ok(r) }
}

fn with_fine(base: &AlignResult, spec: &PathSpec, env: &Env) -> Option<AlignResult> {
    let fine = env.fine?;
    let w = weight_of(&spec.strategy, &env.settings);
    let t = Instant::now();
    let (rots, lenses, cps, st) = fine(&base.rots, &base.lenses, &base.cps, w);
    let errs: Vec<f64> = cps.iter().filter(|c| c.kind == CpKind::Point).map(|c| c.err).collect();
    let (mean_err, max_err) = if errs.is_empty() { (base.mean_err, base.max_err) } else { (errs.iter().sum::<f64>() / errs.len() as f64, errs.iter().cloned().fold(0.0, f64::max)) };
    Some(AlignResult { rots, lenses, cps, removed: base.removed, removal_passes: base.removal_passes, ms: base.ms + t.elapsed().as_millis(), mean_err, max_err, fine: Some(st), fields: Vec::new(), pairs: None })
}

fn row(spec: PathSpec, outcome: Outcome) -> Row {
    Row { spec, outcome, secs: 0.0, result: None, shifts: Vec::new(), own: Stats::default(), common: Stats::default(), lost: 0, mean_with_lost: f32::NAN, protected: None }
}

/// Run the search. `report(fraction, text)` is called as the work goes on (way x of y).
pub fn search(env: &Env, cfg: &SearchConfig, report: &(dyn Fn(f32, &str) + Sync)) -> SearchReport {
    let t0 = Instant::now();
    let n = env.lens_of.len();
    let inp = AlignInput { n, lens_of: env.lens_of, long_side: env.long };
    // the lens is estimated once, so every way starts from the same one
    let mut lenses = env.lenses.to_vec();
    let mut cnt = vec![0usize; n];
    for c in env.cps {
        cnt[c.a] += 1;
        cnt[c.b] += 1;
    }
    let anchor = (0..n).max_by_key(|&i| cnt[i]).unwrap_or(0);
    align::guess_fov(&inp, &mut lenses, env.cps, anchor);

    let base_specs = specs(n, env.cps, cfg, env.current.is_some());
    let fine_ok = cfg.with_fine && env.fine.is_some();
    let total = base_specs.len() * if fine_ok { 2 } else { 1 };
    let mut notes = Vec::new();
    if cfg.with_fine && env.fine.is_none() {
        notes.push("Fine alignment is not available here, ways with it were not tried.".to_string());
    }
    if !env.cps.iter().any(|c| c.locked) {
        notes.push("No protected points: the frame way (protected points first) was not tried.".to_string());
    }
    if foliage_points(env.cps).is_none() {
        notes.push("Foliage filter: not available yet, not tried. Parallax mode is not scored (the check measures the rotation model only).".to_string());
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut done_count = 0usize;
    let progress = |k: usize, text: &str| report(k as f32 / total.max(1) as f32, &format!("Way {} of {total}: {text}", (k + 1).min(total)));
    let mut cancelled = false;

    // phase 1: every way without fine alignment. The joint way first (it fixes the spots), the rest in parallel.
    let joint_pos = base_specs.iter().position(|s| s.strategy == Strategy::Joint).expect("joint way");
    let mut results: Vec<Option<Result<AlignResult, String>>> = (0..base_specs.len()).map(|_| None).collect();
    progress(done_count, &base_specs[joint_pos].label());
    let tj = Instant::now();
    let joint = run_path(&base_specs[joint_pos], env, &lenses, cfg);
    let joint_secs = tj.elapsed().as_secs_f32();
    let joint_shifts = joint.as_ref().ok().map(|r| (env.measure)(&r.rots, &r.lenses));
    let mut secs = vec![0.0f32; base_specs.len()];
    secs[joint_pos] = joint_secs;
    results[joint_pos] = Some(joint);
    done_count += 1;
    if !engine::cancelled() {
        let todo: Vec<usize> = (0..base_specs.len()).filter(|&i| i != joint_pos && base_specs[i].strategy != Strategy::Current).collect();
        let counter = std::sync::atomic::AtomicUsize::new(done_count);
        let out: Vec<(usize, Result<AlignResult, String>, f32)> = todo
            .par_iter()
            .map(|&i| {
                let t = Instant::now();
                let k = counter.load(std::sync::atomic::Ordering::Relaxed);
                progress(k, &base_specs[i].label());
                let r = if engine::cancelled() { Err("cancelled".to_string()) } else { run_path(&base_specs[i], env, &lenses, cfg) };
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                (i, r, t.elapsed().as_secs_f32())
            })
            .collect();
        done_count = counter.load(std::sync::atomic::Ordering::Relaxed);
        for (i, r, t) in out {
            results[i] = Some(r);
            secs[i] = t;
        }
    }
    if engine::cancelled() {
        cancelled = true;
    }

    // rows of phase 1 (measured one after the other: the check is parallel inside)
    let mut base_rows: Vec<usize> = Vec::new(); // row index of each base spec
    for (i, spec) in base_specs.iter().enumerate() {
        let mut r = row(spec.clone(), Outcome::Done);
        r.secs = secs[i];
        if spec.strategy == Strategy::Current {
            let (rots, ls) = env.current.clone().expect("current");
            let mut cps = env.cps.to_vec();
            align::cp_errors(&inp, &rots, &ls, &mut cps);
            r.protected = protected_of(&cps);
            if !cancelled {
                r.shifts = (env.measure)(&rots, &ls);
            }
            r.result = None;
        } else {
            match results[i].take() {
                Some(Ok(res)) => {
                    r.protected = protected_of(&res.cps);
                    r.shifts = if i == joint_pos { joint_shifts.clone().unwrap_or_default() } else if cancelled { Vec::new() } else { (env.measure)(&res.rots, &res.lenses) };
                    r.result = Some(res);
                }
                Some(Err(e)) => r.outcome = if e == "cancelled" { Outcome::Skipped("cancelled".into()) } else { Outcome::Skipped(e) },
                None => r.outcome = Outcome::Skipped("cancelled".into()),
            }
        }
        base_rows.push(rows.len());
        rows.push(r);
    }
    // identical results are measured once
    for a in 0..rows.len() {
        if rows[a].outcome != Outcome::Done || rows[a].result.is_none() {
            continue;
        }
        for b in 0..a {
            if rows[b].outcome == Outcome::Done && rows[b].result.is_some() && rows[a].spec.strategy != Strategy::Current {
                let same = {
                    let (ra, rb) = (rows[a].result.as_ref().unwrap(), rows[b].result.as_ref().unwrap());
                    rots_close(&ra.rots, &rb.rots) && ra.lenses == rb.lenses
                };
                if same {
                    rows[a].outcome = Outcome::SameAs(b);
                    rows[a].result = None;
                    rows[a].shifts = Vec::new();
                    break;
                }
            }
        }
    }

    // phase 2: with fine alignment, best-looking bases first, as long as the time budget lasts
    if fine_ok && !cancelled {
        let mut order: Vec<usize> = (0..base_specs.len()).filter(|&i| base_specs[i].strategy != Strategy::Current).collect();
        let med = |i: usize| {
            let r = &rows[base_rows[i]];
            let m = Stats::of(r.shifts.iter().flatten().copied().collect()).median;
            if m.is_nan() { f32::MAX } else { m }
        };
        order.sort_by(|&a, &b| med(a).total_cmp(&med(b)));
        let mut fine_of: Vec<Option<usize>> = vec![None; base_specs.len()];
        for &i in &order {
            let spec = PathSpec { fine: true, ..base_specs[i].clone() };
            let base_row = base_rows[i];
            let mut r = row(spec.clone(), Outcome::Done);
            match &rows[base_row].outcome {
                Outcome::SameAs(b) => {
                    // the base is a copy: so is its fine version
                    let twin = (0..base_specs.len()).find(|&k| base_rows[k] == *b);
                    r.outcome = match twin.and_then(|k| fine_of[k]) {
                        Some(row_idx) => Outcome::SameAs(row_idx),
                        None => Outcome::Skipped("same as another way, whose fine version was not run".into()),
                    };
                }
                Outcome::Done => {
                    if cancelled || engine::cancelled() {
                        cancelled = true;
                        r.outcome = Outcome::Skipped("cancelled".into());
                    } else if t0.elapsed().as_secs() >= cfg.time_budget_s {
                        r.outcome = Outcome::Skipped(format!("time budget of {} s used up", cfg.time_budget_s));
                    } else {
                        progress(done_count, &spec.label());
                        let t = Instant::now();
                        let res = with_fine(rows[base_row].result.as_ref().expect("base result"), &spec, env);
                        r.secs = t.elapsed().as_secs_f32();
                        done_count += 1;
                        match res {
                            Some(res) if !engine::cancelled() => {
                                r.shifts = (env.measure)(&res.rots, &res.lenses);
                                r.protected = protected_of(&res.cps);
                                r.result = Some(res);
                            }
                            _ => {
                                cancelled = engine::cancelled();
                                r.outcome = Outcome::Skipped(if cancelled { "cancelled".into() } else { "fine alignment not available".into() });
                            }
                        }
                    }
                }
                other => r.outcome = Outcome::Skipped(format!("its base way was not run ({other:?})")),
            }
            fine_of[i] = Some(rows.len());
            rows.push(r);
        }
    }

    finish(rows, cancelled, t0.elapsed().as_secs_f32(), notes)
}

/// Statistics over the common spots and the best row.
pub fn finish(mut rows: Vec<Row>, cancelled: bool, secs: f32, mut notes: Vec<String>) -> SearchReport {
    let spots = rows.iter().map(|r| r.shifts.len()).max().unwrap_or(0);
    let finished: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].outcome == Outcome::Done && rows[i].shifts.len() == spots && spots > 0).collect();
    let common: Vec<usize> = (0..spots).filter(|&k| finished.iter().all(|&i| rows[i].shifts[k].is_some())).collect();
    for &i in &finished {
        let own = Stats::of(rows[i].shifts.iter().flatten().copied().collect());
        let on_common = Stats::of(common.iter().filter_map(|&k| rows[i].shifts[k]).collect());
        let lost = (0..spots).filter(|&k| rows[i].shifts[k].is_none() && finished.iter().any(|&j| j != i && rows[j].shifts[k].is_some())).count();
        let any: Vec<usize> = (0..spots).filter(|&k| finished.iter().any(|&j| rows[j].shifts[k].is_some())).collect();
        rows[i].mean_with_lost = if any.is_empty() { f32::NAN } else { any.iter().map(|&k| rows[i].shifts[k].unwrap_or(LOST_PX)).sum::<f32>() / any.len() as f32 };
        rows[i].own = own;
        rows[i].common = on_common;
        rows[i].lost = lost;
    }
    if !finished.is_empty() && common.len() < 20 {
        notes.push(format!("Only {} spots could be measured in every way: the comparison is rough.", common.len()));
    }
    let best = finished
        .iter()
        .copied()
        .filter(|&i| !rows[i].common.median.is_nan())
        .min_by(|&a, &b| rows[a].common.median.total_cmp(&rows[b].common.median).then(rows[a].common.p90.total_cmp(&rows[b].common.p90))
                .then(rows[a].common.mean.total_cmp(&rows[b].common.mean)));
    SearchReport { rows, spots, common_spots: common.len(), best, cancelled, secs, notes }
}

impl SearchReport {
    /// The table (plain text, also for the status line / logs): which way was how good.
    pub fn table(&self) -> String {
        let mut s = format!(
            "{} fixed spots, {} measurable in every finished way (median/mean/p90/n and the ranking use these). Shifts in px of the 1600 px working image. lost = spots another way could measure; mean+L = mean over all spots any way could measure, a lost spot counting {LOST_PX} px.\n",
            self.spots, self.common_spots
        );
        s.push_str(&format!("{:>3}  {:<58} {:>7} {:>7} {:>7} {:>6} | {:>7} {:>6} {:>5} {:>7} | {:>13} | {:>6}\n", "#", "way", "median", "mean", "p90", "n", "own med", "valid", "lost", "mean+L", "protected", "time"));
        for (i, r) in self.rows.iter().enumerate() {
            let mark = if Some(i) == self.best { "*" } else { " " };
            let prot = r.protected.map(|(n, m, x)| format!("{m:.1}/{x:.1} ({n})")).unwrap_or_else(|| "-".into());
            match &r.outcome {
                Outcome::Done => s.push_str(&format!(
                    "{:>3}{mark} {:<58} {:>7.3} {:>7.3} {:>7.3} {:>6} | {:>7.3} {:>6} {:>5} {:>7.3} | {:>13} | {:>5.0}s\n",
                    i + 1,
                    r.spec.label(),
                    r.common.median,
                    r.common.mean,
                    r.common.p90,
                    r.common.n,
                    r.own.median,
                    r.own.n,
                    r.lost,
                    r.mean_with_lost,
                    prot,
                    r.secs
                )),
                Outcome::SameAs(b) => s.push_str(&format!("{:>3}  {:<58} same result as way {}\n", i + 1, r.spec.label(), b + 1)),
                Outcome::Skipped(why) => s.push_str(&format!("{:>3}  {:<58} skipped: {why}\n", i + 1, r.spec.label())),
            }
        }
        for n in &self.notes {
            s.push_str(&format!("note: {n}\n"));
        }
        if self.cancelled {
            s.push_str("The search was cancelled, the table is incomplete.\n");
        }
        s
    }
}

// ---------------------------------------------------------------------------------------------
// the check on real photos
// ---------------------------------------------------------------------------------------------

/// Everything `search_photos` needs from a project.
pub struct PhotoInputs<'a> {
    /// working images (what the fine alignment runs on)
    pub fa: &'a [FaPhoto],
    /// images the check measures on (`load_check_photos`); may be the same as `fa`
    pub check: &'a [FaPhoto],
    pub lens_of: &'a [usize],
    pub lenses: &'a [Lens],
    pub long: &'a [f64],
    pub cps: &'a [ControlPoint],
    pub settings: AlignSettings,
    pub fine_cfg: fine_align::FineConfig,
    pub current: Option<(Vec<M3>, Vec<Lens>)>,
}

/// Images for the check at `size` (long side) from the photos' files, or the working images when the memory
/// that is free is not enough. Returns the images and a note when the fallback was used.
pub fn load_check_photos(sources: &[engine::Source], size: u32, work: &[FaPhoto]) -> (Vec<FaPhoto>, Option<String>) {
    if size == engine::WORK_SIZE || sources.is_empty() {
        return (work.to_vec(), None);
    }
    // grey + colour of every photo, and some room for the photos being decoded
    let each = size as u64 * (size as u64 * 2 / 3) * 4;
    let need = each * sources.len() as u64 + 2_000_000_000;
    if let Some(free) = crate::sysmem::available()
        && free < need
    {
        return (work.to_vec(), Some(format!("Not enough free memory for the check at {size} px ({:.1} GB needed), measuring on the working images: less exact.", need as f64 / 1e9)));
    }
    let pool = engine::load_pool();
    let loaded: Vec<Result<FaPhoto, String>> = pool.install(|| {
        sources
            .par_iter()
            .map(|s| {
                if engine::cancelled() {
                    return Err("cancelled".to_string());
                }
                let full = s.load_full()?;
                let rgb = crate::imgutil::downscale_rgb(&full, size);
                Ok(FaPhoto { gray: std::sync::Arc::new(crate::imgutil::to_gray(&rgb)), rgb: std::sync::Arc::new(rgb) })
            })
            .collect()
    });
    match loaded.into_iter().collect::<Result<Vec<_>, _>>() {
        Ok(v) => (v, None),
        Err(e) => (work.to_vec(), Some(format!("Could not load the photos for the check ({e}), measuring on the working images: less exact."))),
    }
}

/// The search with the real check (patch matching on fixed spots) and the real fine alignment.
pub fn search_photos(inp: &PhotoInputs, cfg: &SearchConfig, report: &(dyn Fn(f32, &str) + Sync)) -> SearchReport {
    let to_work = inp.fa[0].gray.w.max(inp.fa[0].gray.h) as f32 / inp.check[0].gray.w.max(inp.check[0].gray.h) as f32;
    let qcfg = crate::patch_matcher::MatchConfig { search_radius: 6.0, ..inp.fine_cfg.matcher.clone() };
    let spots: std::sync::OnceLock<Vec<(usize, usize, (f64, f64))>> = std::sync::OnceLock::new();
    let mut pairs: Vec<(usize, usize)> = inp.cps.iter().map(|c| (c.a.min(c.b), c.a.max(c.b))).collect();
    pairs.sort();
    pairs.dedup();
    let measure = |rots: &[M3], lenses: &[Lens]| -> Vec<Option<f32>> {
        let sp = spots.get_or_init(|| fine_align::quality_spots(inp.check, inp.lens_of, lenses, rots, &pairs, 48.0 / to_work as f64, 60));
        fine_align::spot_shifts(inp.check, inp.lens_of, lenses, rots, sp, &qcfg).into_iter().map(|v| v.map(|x| x * to_work)).collect()
    };
    let fine = |rots: &[M3], lenses: &[Lens], cps: &[ControlPoint], w: f64| {
        let cfg = fine_align::FineConfig { locked_weight: w, record_patches: false, ..inp.fine_cfg.clone() };
        fine_align::run(inp.fa, inp.lens_of, inp.long, lenses, rots, cps, inp.settings.fine_tune_fov, inp.settings.distortion, &cfg, &|_, _| {})
    };
    let env = Env {
        lens_of: inp.lens_of,
        lenses: inp.lenses,
        long: inp.long,
        cps: inp.cps,
        settings: inp.settings,
        measure: &measure,
        fine: if inp.fine_cfg.enabled { Some(&fine) } else { None },
        current: inp.current.clone(),
    };
    search(&env, cfg, report)
}

/// `--accuracy-search <photos|.pwproj>`: runs the search headless and prints the table. Environment (all optional):
/// `PW_ACCS_STARTS` start photos (3), `PW_ACCS_BUDGET` seconds (1800), `PW_ACCS_FINE=0` without fine alignment,
/// `PW_ACCS_WEIGHTS=1,30` extra protected weights to compare, `PW_ACCS_CHAINS=0` without chains,
/// `PW_ACCS_CHECK_SIZE` (3200), `PW_ACCS_LOCKED_WEIGHT` weight of the joint way (30).
pub fn run_cli(files_in: &[std::path::PathBuf]) {
    let env = |k: &str| std::env::var(k).ok();
    let setup = crate::accuracy_test::setup(files_in);
    let d = SearchConfig::default();
    let cfg = SearchConfig {
        max_starts: env("PW_ACCS_STARTS").and_then(|v| v.parse().ok()).unwrap_or(d.max_starts),
        time_budget_s: env("PW_ACCS_BUDGET").and_then(|v| v.parse().ok()).unwrap_or(d.time_budget_s),
        with_fine: env("PW_ACCS_FINE").is_none_or(|v| v != "0"),
        check_size: env("PW_ACCS_CHECK_SIZE").and_then(|v| v.parse().ok()).unwrap_or(d.check_size),
        extra_weights: env("PW_ACCS_WEIGHTS").map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect()).unwrap_or_default(),
        chains: env("PW_ACCS_CHAINS").is_none_or(|v| v != "0"),
        ..d
    };
    let fa: Vec<FaPhoto> = setup.photos.iter().map(|p| FaPhoto { gray: p.gray.clone(), rgb: p.work.clone() }).collect();
    let sources: Vec<engine::Source> = setup.files.iter().map(|f| engine::Source::File(f.clone())).collect();
    let (check, note) = load_check_photos(&sources, cfg.check_size, &fa);
    if let Some(n) = note {
        println!("{n}");
    }
    let locked_weight = env("PW_ACCS_LOCKED_WEIGHT").and_then(|v| v.parse().ok()).unwrap_or(align::LOCKED_WEIGHT);
    let inp = PhotoInputs {
        fa: &fa,
        check: &check,
        lens_of: &setup.lens_of,
        lenses: &setup.lenses,
        long: &setup.long,
        cps: &setup.cps,
        settings: AlignSettings { locked_weight, ..Default::default() },
        fine_cfg: fine_align::FineConfig { locked_weight, ..Default::default() },
        current: None,
    };
    println!("settings: {cfg:?}");
    let last = std::sync::Mutex::new(String::new());
    let rep = search_photos(&inp, &cfg, &|f, t| {
        let mut l = last.lock().unwrap();
        if *l != t {
            println!("[{:3.0} %] {t}", f * 100.0);
            *l = t.to_string();
        }
    });
    println!("{}", rep.table());
    println!("total {:.0} s", rep.secs);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Rng, camera_from_ypr, mtv, normalize};
    use std::sync::Mutex;

    /// the cancel flag is global: tests that use it must not run at the same time
    static CANCEL_LOCK: Mutex<()> = Mutex::new(());

    fn synth(rots: &[M3], lens: &Lens, pairs: &[(usize, usize)], per_pair: usize, seed: u64) -> Vec<ControlPoint> {
        let mut rng = Rng(seed);
        let mut cps = Vec::new();
        for &(a, b) in pairs {
            let mut k = 0;
            let mut guard = 0;
            while k < per_pair && guard < 100_000 {
                guard += 1;
                let ca = mtv(&rots[a], [0.0, 0.0, 1.0]);
                let cb = mtv(&rots[b], [0.0, 0.0, 1.0]);
                let mid = normalize([ca[0] + cb[0], ca[1] + cb[1], ca[2] + cb[2]]);
                let d = normalize([mid[0] + 0.4 * rng.gauss(), mid[1] + 0.3 * rng.gauss(), mid[2] + 0.4 * rng.gauss()]);
                let (Some(pa), Some(pb)) = (align::project(lens, crate::geometry::mv(&rots[a], d)), align::project(lens, crate::geometry::mv(&rots[b], d))) else { continue };
                let inside = |p: [f64; 2]| p[0].abs() < 0.5 && p[1].abs() < 0.375;
                if inside(pa) && inside(pb) {
                    cps.push(ControlPoint::new(a, b, pa, pb, false));
                    k += 1;
                }
            }
            assert_eq!(k, per_pair);
        }
        cps
    }

    /// error of each photo's rotation against the truth, relative to photo 0 (the frame does not matter): three
    /// numbers per photo (the off-diagonal parts of the remaining rotation, in thousandths of a radian)
    fn errors(rots: &[M3], truth: &[M3]) -> Vec<Option<f32>> {
        let rel = |r: &[M3], i: usize| mm(&r[i], &crate::geometry::transpose(&r[0]));
        let mut v = Vec::new();
        for i in 1..rots.len() {
            let d = mm(&rel(rots, i), &crate::geometry::transpose(&rel(truth, i)));
            for (a, b) in [(0, 1), (0, 2), (1, 2)] {
                v.push(Some(((d[a][b] - d[b][a]).abs() * 500.0) as f32));
            }
        }
        v
    }

    /// three cameras in a row: exact protected points plus many wrong ones that agree with each other
    fn trap() -> (Vec<M3>, Lens, Vec<ControlPoint>) {
        let lens = Lens { from_exif: true, ..Lens::from_hfov_deg(50.0) };
        let truth: Vec<M3> = vec![camera_from_ypr(0.0, 0.0, 0.0), camera_from_ypr(0.35, 0.02, 0.0), camera_from_ypr(0.7, -0.03, 0.01), camera_from_ypr(1.05, 0.0, 0.0)];
        let mut cps = synth(&truth, &lens, &[(0, 1), (1, 2), (2, 3)], 8, 21);
        for c in cps.iter_mut() {
            c.locked = true;
            c.manual = true;
        }
        let mut wrong = synth(&truth, &lens, &[(0, 1), (1, 2), (2, 3)], 120, 22);
        let mut rng = Rng(23);
        for c in wrong.iter_mut() {
            c.pb[0] += 0.02 + 0.004 * rng.gauss();
            c.pb[1] += 0.012 + 0.004 * rng.gauss();
        }
        cps.extend(wrong);
        (truth, lens, cps)
    }

    fn scene() -> (Vec<M3>, Lens, Vec<ControlPoint>) {
        // the lens is known (like one from the photos' EXIF data): the test is about the placing
        let lens = Lens { from_exif: true, ..Lens::from_hfov_deg(50.0) };
        let truth: Vec<M3> = (0..6).map(|i| camera_from_ypr(0.3 * i as f64, 0.01 * (i % 3) as f64, 0.0)).collect();
        let mut cps = synth(&truth, &lens, &[(0, 1), (1, 2), (2, 3), (3, 4), (4, 5), (0, 2), (3, 5)], 12, 5);
        // protected hand points: exact
        for c in cps.iter_mut().filter(|c| (c.a, c.b) == (1, 2) || (c.a, c.b) == (3, 4)).take(12) {
            c.locked = true;
            c.manual = true;
        }
        // a wrong, well-supported cluster between 4 and 5 (a moving car): 60 points shifted by ~25 px
        let mut wrong = synth(&truth, &lens, &[(4, 5)], 60, 6);
        for c in wrong.iter_mut() {
            c.pb[0] += 0.025;
            c.pb[1] += 0.01;
        }
        cps.extend(wrong);
        (truth, lens, cps)
    }

    fn run_scene(cfg: &SearchConfig) -> (SearchReport, Vec<M3>) {
        let (truth, lens, cps) = scene();
        let lens_of = vec![0usize; 6];
        let long = vec![1000.0; 6];
        let measure = |rots: &[M3], _: &[Lens]| errors(rots, &truth);
        let env = Env {
            lens_of: &lens_of,
            lenses: std::slice::from_ref(&lens),
            long: &long,
            cps: &cps,
            settings: AlignSettings { fine_tune_fov: false, distortion: false, remove_outliers: true, level: false, locked_weight: 30.0, outliers_skip_areas: true },
            measure: &measure,
            fine: None,
            current: None,
        };
        (search(&env, cfg, &|_, _| {}), truth)
    }

    #[test]
    fn every_way_is_tried_and_rated_on_the_same_spots() {
        let _g = CANCEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        engine::CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
        let cfg = SearchConfig { with_fine: false, ..Default::default() };
        let (rep, _) = run_scene(&cfg);
        println!("{}", rep.table());
        // joint, frame, 3 starts x 2 orders
        assert_eq!(rep.rows.len(), 1 + 1 + 3 * 2);
        assert_eq!(rep.rows[0].spec.strategy, Strategy::Joint);
        assert!(rep.rows.iter().any(|r| r.spec.strategy == Strategy::Scaffold));
        // all measured on the very same 5 spots, the ranking on the common ones
        assert_eq!(rep.spots, 15);
        assert_eq!(rep.common_spots, 15);
        for r in rep.rows.iter().filter(|r| r.outcome == Outcome::Done) {
            assert_eq!(r.common.n, rep.common_spots);
        }
        // best = smallest common median, and nothing is better
        let best = rep.best.expect("a best way");
        let m = rep.rows[best].common.median;
        assert!(rep.rows.iter().filter(|r| r.outcome == Outcome::Done).all(|r| r.common.median >= m));
        // exact protected points: the frame fits them
        let sc = rep.rows.iter().find(|r| r.spec.strategy == Strategy::Scaffold).unwrap();
        assert!(sc.protected.unwrap().1 < 1.0, "protected points fit: {:?}", sc.protected);
        // the table names every way
        assert_eq!(rep.table().lines().filter(|l| l.contains("Chain from photo")).count(), 6);
    }

    #[test]
    fn a_way_can_be_better_than_the_joint_one() {
        let _g = CANCEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        engine::CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
        // protected points count like any point (weight 1): the joint way is pulled away by the 360 wrong points that
        // agree with each other; the same way with the protected weight 30 is not - and the table shows it
        let (truth, lens, cps) = trap();
        let lens_of = vec![0usize; 4];
        let long = vec![1000.0; 4];
        let measure = |rots: &[M3], _: &[Lens]| errors(rots, &truth);
        let env = Env {
            lens_of: &lens_of,
            lenses: std::slice::from_ref(&lens),
            long: &long,
            cps: &cps,
            settings: AlignSettings { fine_tune_fov: false, distortion: false, remove_outliers: false, level: false, locked_weight: 1.0, outliers_skip_areas: true },
            measure: &measure,
            fine: None,
            current: None,
        };
        let cfg = SearchConfig { with_fine: false, max_starts: 4, extra_weights: vec![30.0], ..Default::default() };
        let rep = search(&env, &cfg, &|_, _| {});
        println!("{}", rep.table());
        let joint = rep.rows[0].common.mean;
        let w = rep.rows.iter().find(|r| r.spec.strategy == Strategy::Weighted(30.0)).unwrap();
        assert_eq!(w.outcome, Outcome::Done);
        assert!(w.common.mean < joint * 0.5, "weight 30: {} against {joint}", w.common.mean);
        // and the search proposes a way that is not the joint one with weight 1
        assert_ne!(rep.best, Some(0));
        assert!(rep.rows[rep.best.unwrap()].common.mean < joint * 0.5);
    }

    #[test]
    fn without_protected_points_there_is_no_frame_way_and_it_is_said_so() {
        let _g = CANCEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        engine::CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
        let (truth, lens, mut cps) = scene();
        for c in cps.iter_mut() {
            c.locked = false;
        }
        let lens_of = vec![0usize; 6];
        let long = vec![1000.0; 6];
        let measure = |rots: &[M3], _: &[Lens]| errors(rots, &truth);
        let env = Env {
            lens_of: &lens_of,
            lenses: std::slice::from_ref(&lens),
            long: &long,
            cps: &cps,
            settings: AlignSettings { fine_tune_fov: false, distortion: false, remove_outliers: true, level: false, locked_weight: 30.0, outliers_skip_areas: true },
            measure: &measure,
            fine: None,
            current: None,
        };
        let rep = search(&env, &SearchConfig { with_fine: false, ..Default::default() }, &|_, _| {});
        assert!(!rep.rows.iter().any(|r| r.spec.strategy == Strategy::Scaffold));
        assert!(rep.notes.iter().any(|n| n.contains("No protected points")));
    }

    #[test]
    fn fine_versions_follow_and_the_time_budget_skips_them_with_a_reason() {
        let _g = CANCEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        engine::CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
        let (truth, lens, cps) = scene();
        let lens_of = vec![0usize; 6];
        let long = vec![1000.0; 6];
        let measure = |rots: &[M3], _: &[Lens]| errors(rots, &truth);
        let fine = |r: &[M3], l: &[Lens], c: &[ControlPoint], _w: f64| (r.to_vec(), l.to_vec(), c.to_vec(), FineStats::default());
        let env = Env {
            lens_of: &lens_of,
            lenses: std::slice::from_ref(&lens),
            long: &long,
            cps: &cps,
            settings: AlignSettings { fine_tune_fov: false, distortion: false, remove_outliers: true, level: false, locked_weight: 30.0, outliers_skip_areas: true },
            measure: &measure,
            fine: Some(&fine),
            current: None,
        };
        let rep = search(&env, &SearchConfig::default(), &|_, _| {});
        let n_fine = rep.rows.iter().filter(|r| r.spec.fine).count();
        assert_eq!(n_fine, rep.rows.iter().filter(|r| !r.spec.fine).count());
        // budget 0: no fine way is begun, each says why
        let rep0 = search(&env, &SearchConfig { time_budget_s: 0, ..Default::default() }, &|_, _| {});
        let fines: Vec<&Row> = rep0.rows.iter().filter(|r| r.spec.fine).collect();
        assert!(!fines.is_empty());
        assert!(fines.iter().all(|r| matches!(&r.outcome, Outcome::SameAs(_)) || matches!(&r.outcome, Outcome::Skipped(w) if w.contains("time budget"))), "{:?}", fines.iter().map(|r| &r.outcome).collect::<Vec<_>>());
        assert!(fines.iter().any(|r| matches!(&r.outcome, Outcome::Skipped(w) if w.contains("time budget"))));
        assert!(rep0.table().contains("skipped"));
    }

    #[test]
    fn cancelling_stops_the_search_and_says_so() {
        let _g = CANCEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (truth, lens, cps) = scene();
        let lens_of = vec![0usize; 6];
        let long = vec![1000.0; 6];
        let measure = |rots: &[M3], _: &[Lens]| errors(rots, &truth);
        let env = Env {
            lens_of: &lens_of,
            lenses: std::slice::from_ref(&lens),
            long: &long,
            cps: &cps,
            settings: AlignSettings { fine_tune_fov: false, distortion: false, remove_outliers: true, level: false, locked_weight: 30.0, outliers_skip_areas: true },
            measure: &measure,
            fine: None,
            current: None,
        };
        engine::CANCEL.store(true, std::sync::atomic::Ordering::Relaxed);
        let rep = search(&env, &SearchConfig::default(), &|_, _| {});
        engine::CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(rep.cancelled);
        assert!(rep.best.is_none());
        assert!(rep.table().contains("cancelled"));
    }

    #[test]
    fn the_current_alignment_is_a_way_to_compare_with() {
        let _g = CANCEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        engine::CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
        let (truth, lens, cps) = scene();
        let lens_of = vec![0usize; 6];
        let long = vec![1000.0; 6];
        let measure = |rots: &[M3], _: &[Lens]| errors(rots, &truth);
        let env = Env {
            lens_of: &lens_of,
            lenses: std::slice::from_ref(&lens),
            long: &long,
            cps: &cps,
            settings: AlignSettings { fine_tune_fov: false, distortion: false, remove_outliers: true, level: false, locked_weight: 30.0, outliers_skip_areas: true },
            measure: &measure,
            fine: None,
            // a crooked current alignment
            current: Some((truth.iter().enumerate().map(|(i, r)| mm(&camera_from_ypr(0.05 * i as f64, 0.03 * i as f64, 0.0), r)).collect(), vec![lens])),
        };
        let rep = search(&env, &SearchConfig { with_fine: false, ..Default::default() }, &|_, _| {});
        assert_eq!(rep.rows[0].spec.strategy, Strategy::Current);
        assert!(rep.rows[0].result.is_none(), "current is already applied");
        assert_ne!(rep.best, Some(0), "a crooked current alignment must not win");
    }

    #[test]
    fn chain_keeps_every_photo_and_start_photos_prefer_protected_points() {
        let (_, _, cps) = scene();
        let starts = start_photos(6, &cps, 2);
        assert_eq!(starts.len(), 2);
        // protected points are between 1-2 and 3-4
        assert!(starts.iter().all(|s| (1..=4).contains(s)), "{starts:?}");
        assert!(start_photos(6, &cps, 99).len() <= 6);
        // the foliage hook does nothing yet, so it adds no way
        assert!(foliage_points(&cps).is_none());
    }

    #[test]
    fn identical_results_are_measured_once() {
        let _g = CANCEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        engine::CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
        // exact points only: every way finds the same solution
        let lens = Lens { from_exif: true, ..Lens::from_hfov_deg(50.0) };
        let truth: Vec<M3> = (0..4).map(|i| camera_from_ypr(0.3 * i as f64, 0.0, 0.0)).collect();
        let cps = synth(&truth, &lens, &[(0, 1), (1, 2), (2, 3), (0, 2)], 15, 9);
        let lens_of = vec![0usize; 4];
        let long = vec![1000.0; 4];
        let measure = |rots: &[M3], _: &[Lens]| errors(rots, &truth);
        let env = Env {
            lens_of: &lens_of,
            lenses: std::slice::from_ref(&lens),
            long: &long,
            cps: &cps,
            settings: AlignSettings { fine_tune_fov: false, distortion: false, remove_outliers: false, level: false, locked_weight: 30.0, outliers_skip_areas: true },
            measure: &measure,
            fine: None,
            current: None,
        };
        let rep = search(&env, &SearchConfig { with_fine: false, ..Default::default() }, &|_, _| {});
        let same = rep.rows.iter().filter(|r| matches!(r.outcome, Outcome::SameAs(_))).count();
        assert!(same >= 1, "{}", rep.table());
    }
}
