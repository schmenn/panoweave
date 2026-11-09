//! `--accuracy <photos>`: headless measurement of alignment accuracy, for judging changes to the
//! alignment honestly (the "average error" of the points also drops when points are removed). Runs the same
//! chain as "Align photos" in the app (align, pair check, fine alignment) several times in a row,
//! each pass starting from the previous result like pressing "Align" again, and after every pass
//! prints the independent quality check (`fine_align::quality`: remaining shift on spots that are
//! neither points nor control areas) next to the point error the app shows.
//!
//! Environment (all optional, for comparing variants):
//! - `PW_ACC_PASSES` passes (default 3)
//! - `PW_FINE_GRID` control-area grid cell (default 64), `PW_FINE_AREAS` areas per pair (200),
//!   `PW_FINE_ITER` rounds (4), `PW_FINE_CONV` stop threshold in px (0.1)
//! - `PW_ACC_NO_OUTLIERS=1` passes after the first keep every point (no outlier removal)
//! - `PW_ACC_PAIRS=1` also prints the check per photo pair (worst first, with the lens of each photo)
//! - `PW_ACC_FINE_SIZE` long side of the images the fine alignment works on (default 1600 = the
//!   working images), `PW_ACC_CHECK_SIZE` the same for the quality check (default 3200: on the
//!   1600 px images a good part of the result is the inaccuracy of the check itself – Fischerbastei
//!   0.40 px at 1600 against 0.27 px at 3200 – so small differences between settings would be lost).
//!   Shifts are always printed in pixels of the 1600 px working image.

use crate::{align, engine, fine_align, pair_check};
use std::path::PathBuf;

/// Long side of the images the quality check measures on by default (see the module comment).
const CHECK_SIZE: u32 = 3200;

fn env<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn radius_text(q: &fine_align::Quality) -> String {
    let [a, b, c] = q.by_radius;
    format!("<0.25: {:.3} ({}), 0.25-0.4: {:.3} ({}), >0.4: {:.3} ({})", a.1, a.0, b.1, b.0, c.1, c.0)
}

/// Photos, points and lenses of a headless run: from a project file (points, protected ones included, lenses and
/// lens assignment as they are) or from photos (points found, lenses from EXIF like the app).
pub struct Setup {
    pub files: Vec<PathBuf>,
    pub photos: Vec<engine::LoadedPhoto>,
    pub cps: Vec<align::ControlPoint>,
    pub lens_of: Vec<usize>,
    pub lenses: Vec<align::Lens>,
    pub long: Vec<f64>,
    pub ms: engine::MatchSettings,
}

pub fn setup(files_in: &[PathBuf]) -> Setup {
    use rayon::prelude::*;
    let t = std::time::Instant::now();
    // a project file instead of photos: its photos, lenses and points (protected ones included) are used as they are
    let proj = match files_in {
        [f] if f.extension().is_some_and(|e| e.eq_ignore_ascii_case("pwproj")) => Some(crate::project::load(f).expect("project")),
        _ => None,
    };
    let files: Vec<PathBuf> = match &proj {
        Some(p) => p.photos.iter().map(|ph| ph.path.clone()).collect(),
        None => files_in.to_vec(),
    };
    let photos: Vec<engine::LoadedPhoto> = files
        .par_iter()
        .map(|f| engine::load(engine::Source::File(f.clone()), f.file_name().unwrap().to_string_lossy().to_string()).unwrap())
        .collect();
    println!("loaded {} photos {:?}", photos.len(), t.elapsed());
    let grays: Vec<_> = photos.iter().map(|p| p.gray.clone()).collect();
    let works: Vec<_> = photos.iter().map(|p| p.work.clone()).collect();
    let ms = engine::MatchSettings::default();
    let m = match &proj {
        Some(p) => engine::MatchResult { cps: p.cps.clone(), pairs: Vec::new(), detect_ms: 0, match_ms: 0, keypoints: 0, sky_removed: 0 },
        None => engine::find_points(&works, &grays, &[], ms, &|_, _| {}),
    };
    println!("points: {} cps in {} pairs, {} protected", m.cps.len(), m.pairs.len(), m.cps.iter().filter(|c| c.locked).count());
    // same lens grouping as the app
    let mut keys: Vec<(Option<u64>, u32)> = Vec::new();
    let mut lenses = Vec::new();
    let lens_of: Vec<usize> = photos
        .iter()
        .map(|p| {
            let key = (p.info.focal_35mm.map(|f| f as u64), p.w.max(p.h));
            if let Some(i) = keys.iter().position(|k| *k == key) {
                return i;
            }
            keys.push(key);
            lenses.push(match engine::exif_hfov(&p.info) {
                Some(h) => align::Lens { from_exif: true, ..align::Lens::from_hfov_deg(h) },
                None => align::Lens::from_hfov_deg(50.0),
            });
            lenses.len() - 1
        })
        .collect();
    let long: Vec<f64> = photos.iter().map(|p| p.w.max(p.h) as f64).collect();
    let (lens_of, lenses) = match &proj {
        Some(p) => (p.photos.iter().map(|ph| ph.lens).collect::<Vec<_>>(), p.lenses.clone()),
        None => (lens_of, lenses),
    };
    Setup { files, photos, cps: m.cps, lens_of, lenses, long, ms }
}

pub fn run(files_in: &[PathBuf]) {
    use rayon::prelude::*;
    let Setup { files, photos, cps: cps0, lens_of, mut lenses, long, ms } = setup(files_in);
    let files = &files[..];
    let fa_at = |size: u32| -> Vec<fine_align::FaPhoto> {
        if size == engine::WORK_SIZE {
            return photos.iter().map(|p| fine_align::FaPhoto { gray: p.gray.clone(), rgb: p.work.clone() }).collect();
        }
        files
            .par_iter()
            .map(|f| {
                let full = engine::Source::File(f.clone()).load_full().unwrap();
                let rgb = crate::imgutil::downscale_rgb(&full, size);
                fine_align::FaPhoto { gray: std::sync::Arc::new(crate::imgutil::to_gray(&rgb)), rgb: std::sync::Arc::new(rgb) }
            })
            .collect()
    };
    let fine_size: u32 = env("PW_ACC_FINE_SIZE", engine::WORK_SIZE);
    let check_size: u32 = env("PW_ACC_CHECK_SIZE", CHECK_SIZE);
    let fa = fa_at(engine::WORK_SIZE);
    let fa_fine = if fine_size == engine::WORK_SIZE { fa.clone() } else { fa_at(fine_size) };
    let fa_check = if check_size == fine_size { fa_fine.clone() } else if check_size == engine::WORK_SIZE { fa.clone() } else { fa_at(check_size) };
    // shifts in check-image pixels -> working-image pixels
    let to_work = photos[0].gray.w.max(photos[0].gray.h) as f32 / fa_check[0].gray.w.max(fa_check[0].gray.h) as f32;
    let scaled = |mut q: fine_align::Quality| {
        q.mean *= to_work;
        q.median *= to_work;
        q.p90 *= to_work;
        for b in q.by_radius.iter_mut() {
            b.1 *= to_work;
        }
        q
    };
    let d = fine_align::FineConfig::default();
    let fine_cfg = fine_align::FineConfig {
        grid_cell: env("PW_FINE_GRID", d.grid_cell),
        max_areas_per_pair: env("PW_FINE_AREAS", d.max_areas_per_pair),
        refine_iterations: env("PW_FINE_ITER", d.refine_iterations),
        converge_threshold: env("PW_FINE_CONV", d.converge_threshold),
        record_patches: false,
        locked_weight: env("PW_ACC_LOCKED_WEIGHT", d.locked_weight),
        ..d
    };
    let passes: usize = env("PW_ACC_PASSES", 3);
    let keep_points = std::env::var("PW_ACC_NO_OUTLIERS").is_ok_and(|v| v == "1");
    let pc_cfg = pair_check::PairCheckConfig::default();
    let used = vec![true; photos.len()];
    // the quality check: a small search (the alignment is already close) on fixed spots
    let qcfg = crate::patch_matcher::MatchConfig { search_radius: 6.0, ..fine_cfg.matcher.clone() };
    let mut cps = cps0;
    let mut spots = Vec::new();
    println!(
        "settings: passes {passes}, fine on {fine_size} px, check on {check_size} px, grid {}, areas/pair {}, rounds {}, stop {} px, outliers after pass 1 {}",
        fine_cfg.grid_cell, fine_cfg.max_areas_per_pair, fine_cfg.refine_iterations, fine_cfg.converge_threshold, if keep_points { "kept" } else { "removed" }
    );
    for pass in 0..passes {
        let t = std::time::Instant::now();
        let s = engine::AlignSettings { remove_outliers: pass == 0 || !keep_points, locked_weight: fine_cfg.locked_weight, ..Default::default() };
        let r0 = engine::align_all(&lens_of, &lenses, &long, &cps, &s, &|_, _| {});
        let (mut r, psum) = pair_check::run_rounds(&fa, &lens_of, &lenses, &long, r0, &s, &used, &pc_cfg, &fine_cfg.matcher, ms, &|_, _| {});
        let removed = r.removed;
        {
            // overlapping pairs (estimated from the geometry) that still have too few points
            let counts = pair_check::pair_counts(&r.cps);
            let weak: Vec<String> = psum
                .overlaps
                .iter()
                .filter(|o| counts.get(&(o.a, o.b)).copied().unwrap_or(0) < pc_cfg.min_points)
                .map(|o| format!("{}-{} ({:.0} %, {} pts)", o.a, o.b, o.share * 100.0, counts.get(&(o.a, o.b)).copied().unwrap_or(0)))
                .collect();
            println!("  pair check: {} overlapping pairs, {} with fewer than {} points: {}", psum.overlaps.len(), weak.len(), pc_cfg.min_points, weak.join(", "));
        }
        if spots.is_empty() {
            let mut pairs: Vec<(usize, usize)> = r.cps.iter().map(|c| (c.a.min(c.b), c.a.max(c.b))).collect();
            pairs.sort();
            pairs.dedup();
            spots = fine_align::quality_spots(&fa_check, &lens_of, &r.lenses, &r.rots, &pairs, 48.0 / to_work as f64, 60);
            let q = scaled(fine_align::quality(&fa_check, &lens_of, &r.lenses, &r.rots, &spots, &qcfg));
            println!(
                "without fine alignment: CHECK {} of {} spots valid, shift mean {:.3} median {:.3} p90 {:.3} px, diff {:.2} | median by radius {}",
                q.valid, q.spots, q.mean, q.median, q.p90, q.diff, radius_text(&q)
            );
        }
        let (rots, lenses2, cps2, st) = fine_align::run(&fa_fine, &lens_of, &long, &r.lenses, &r.rots, &r.cps, s.fine_tune_fov, s.distortion, &fine_cfg, &|_, _| {});
        r.rots = rots;
        r.lenses = lenses2;
        r.cps = cps2;
        let errs: Vec<f64> = r.cps.iter().filter(|c| c.kind == align::CpKind::Point).map(|c| c.err).collect();
        let mean_err = errs.iter().sum::<f64>() / errs.len().max(1) as f64;
        let q = scaled(fine_align::quality(&fa_check, &lens_of, &r.lenses, &r.rots, &spots, &qcfg));
        println!(
            "pass {}: {:.1} s | points {} (removed {removed}), point error {mean_err:.3} px | fine {} rounds, areas {} | CHECK {} of {} spots valid, shift mean {:.3} median {:.3} p90 {:.3} px, diff {:.2} | median by radius {}",
            pass + 1,
            t.elapsed().as_secs_f32(),
            errs.len(),
            st.rounds,
            st.areas,
            q.valid,
            q.spots,
            q.mean,
            q.median,
            q.p90,
            q.diff,
            radius_text(&q)
        );
        // protected points: how well the alignment fits them (px of the original photos), per pair
        {
            let mut per: std::collections::BTreeMap<(usize, usize), Vec<f64>> = Default::default();
            for c in r.cps.iter().filter(|c| c.locked) {
                per.entry((c.a.min(c.b), c.a.max(c.b))).or_default().push(c.err);
            }
            let all: Vec<f64> = per.values().flatten().copied().collect();
            if !all.is_empty() {
                let mut sorted = all.clone();
                sorted.sort_by(f64::total_cmp);
                println!(
                    "  protected points: {} in {} pairs, error mean {:.2} median {:.2} max {:.2} px",
                    all.len(),
                    per.len(),
                    all.iter().sum::<f64>() / all.len() as f64,
                    sorted[sorted.len() / 2],
                    sorted[sorted.len() - 1]
                );
                if std::env::var("PW_ACC_PAIRS").is_ok_and(|v| v == "1") {
                    for ((a, b), v) in &per {
                        // the best two photos alone can do with only these points (lens as found): what is left is
                        // the limit of the model (parallax, hand points that contradict each other), not of the weighting
                        let own: Vec<align::ControlPoint> = r.cps.iter().filter(|c| c.locked && (c.a.min(c.b), c.a.max(c.b)) == (*a, *b)).copied().collect();
                        let inp = align::AlignInput { n: photos.len(), lens_of: &lens_of, long_side: &long };
                        let mut ls = r.lenses.clone();
                        let mut rots = vec![crate::geometry::IDENTITY; photos.len()];
                        rots[*a] = r.rots[*a];
                        rots[*b] = r.rots[*b];
                        align::optimize(&inp, &mut rots, &mut ls, &own, *a, &align::OptimizeOptions { locked_weight: 1.0, ..Default::default() }, 60);
                        let mut own = own;
                        align::cp_errors(&inp, &rots, &ls, &mut own);
                        let alone = own.iter().map(|c| c.err).sum::<f64>() / own.len() as f64;
                        println!("    pair {:2}-{:2}: {} protected, mean {:.2} max {:.2} px (alone, these points only: {alone:.2} px)", a, b, v.len(), v.iter().sum::<f64>() / v.len() as f64, v.iter().cloned().fold(0.0, f64::max));
                    }
                }
            }
        }
        if std::env::var("PW_ACC_PAIRS").is_ok_and(|v| v == "1") {
            let mut pairs: Vec<(usize, usize)> = spots.iter().map(|s| (s.0, s.1)).collect();
            pairs.dedup();
            let mut rows: Vec<(f32, String)> = Vec::new();
            for (a, b) in pairs {
                let sp: Vec<_> = spots.iter().copied().filter(|s| s.0 == a && s.1 == b).collect();
                let q = scaled(fine_align::quality(&fa_check, &lens_of, &r.lenses, &r.rots, &sp, &qcfg));
                let mixed = if lens_of[a] == lens_of[b] { "same" } else { "MIXED" };
                rows.push((q.median, format!("pair {:2}-{:2} lenses {}/{} {mixed}: {} of {} valid, median {:.3} p90 {:.3}", a, b, lens_of[a] + 1, lens_of[b] + 1, q.valid, q.spots, q.median, q.p90)));
            }
            rows.sort_by(|x, y| y.0.total_cmp(&x.0));
            for (_, t) in rows {
                println!("{t}");
            }
        }
        cps = r.cps;
        lenses = r.lenses;
        for (i, l) in lenses.iter().enumerate() {
            println!("lens {}: {:.2}° wide, k1 {:+.4}", i + 1, l.hfov_deg(), l.k1);
        }
    }
}
