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
//!   working images), `PW_ACC_CHECK_SIZE` the same for the quality check (default 1600). Shifts are
//!   always printed in pixels of the 1600 px working image.

use crate::{align, engine, fine_align, pair_check};
use std::path::PathBuf;

fn env<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn radius_text(q: &fine_align::Quality) -> String {
    let [a, b, c] = q.by_radius;
    format!("<0.25: {:.3} ({}), 0.25-0.4: {:.3} ({}), >0.4: {:.3} ({})", a.1, a.0, b.1, b.0, c.1, c.0)
}

pub fn run(files: &[PathBuf]) {
    use rayon::prelude::*;
    let t = std::time::Instant::now();
    let photos: Vec<engine::LoadedPhoto> = files
        .par_iter()
        .map(|f| engine::load(engine::Source::File(f.clone()), f.file_name().unwrap().to_string_lossy().to_string()).unwrap())
        .collect();
    println!("loaded {} photos {:?}", photos.len(), t.elapsed());
    let grays: Vec<_> = photos.iter().map(|p| p.gray.clone()).collect();
    let works: Vec<_> = photos.iter().map(|p| p.work.clone()).collect();
    let ms = engine::MatchSettings::default();
    let m = engine::find_points(&works, &grays, &[], ms, &|_, _| {});
    println!("points: {} cps in {} pairs", m.cps.len(), m.pairs.len());
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
    let check_size: u32 = env("PW_ACC_CHECK_SIZE", engine::WORK_SIZE);
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
        ..d
    };
    let passes: usize = env("PW_ACC_PASSES", 3);
    let keep_points = std::env::var("PW_ACC_NO_OUTLIERS").is_ok_and(|v| v == "1");
    let pc_cfg = pair_check::PairCheckConfig::default();
    let used = vec![true; photos.len()];
    // the quality check: a small search (the alignment is already close) on fixed spots
    let qcfg = crate::patch_matcher::MatchConfig { search_radius: 6.0, ..fine_cfg.matcher.clone() };
    let mut cps = m.cps.clone();
    let mut spots = Vec::new();
    println!(
        "settings: passes {passes}, fine on {fine_size} px, check on {check_size} px, grid {}, areas/pair {}, rounds {}, stop {} px, outliers after pass 1 {}",
        fine_cfg.grid_cell, fine_cfg.max_areas_per_pair, fine_cfg.refine_iterations, fine_cfg.converge_threshold, if keep_points { "kept" } else { "removed" }
    );
    for pass in 0..passes {
        let t = std::time::Instant::now();
        let s = engine::AlignSettings { remove_outliers: pass == 0 || !keep_points, ..Default::default() };
        let r0 = engine::align_all(&lens_of, &lenses, &long, &cps, &s, &|_, _| {});
        let (mut r, _) = pair_check::run_rounds(&fa, &lens_of, &lenses, &long, r0, &s, &used, &pc_cfg, &fine_cfg.matcher, ms, &|_, _| {});
        let removed = r.removed;
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
