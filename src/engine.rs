//! The panorama pipeline glue: loading photos, finding points, aligning.

use crate::align::{self, AlignInput, ControlPoint, Lens, OptimizeOptions};
use crate::features;
use crate::geometry::M3;
use crate::imgutil::{self, downscale_rgb, to_gray, Gray, PhotoInfo};
use image::RgbImage;
use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

pub const WORK_SIZE: u32 = 1600;

/// Set by the Cancel button; long computations look at it and stop early.
/// (Only one job runs at a time, so one flag is enough.)
pub static CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[inline]
pub fn cancelled() -> bool {
    CANCEL.load(std::sync::atomic::Ordering::Relaxed)
}

/// Up to this many photos every pair is matched; above, only promising pairs (see `candidate_pairs`).
pub const ALL_PAIRS_LIMIT: usize = 30;
/// Partners kept per photo by the quick pre-check.
pub const PRESELECT_PARTNERS: usize = 8;
/// Descriptors per photo used by the quick pre-check.
pub const PRESELECT_SAMPLE: usize = 400;
/// Quick-check matches a pair needs to be considered.
pub const PRESELECT_MIN: usize = 6;
/// Neighbours in file order that are always tried (photos are usually shot in order).
pub const PRESELECT_NEIGHBOURS: usize = 2;

/// Which photo pairs to match. With many photos, matching all n·(n−1)/2 pairs takes very long,
/// so a quick check on a small sample of descriptors picks the most promising partners
/// of every photo, plus its neighbours in file order.
pub fn candidate_pairs(feats: &[features::Features]) -> Vec<(usize, usize)> {
    let n = feats.len();
    if n <= ALL_PAIRS_LIMIT {
        return (0..n).flat_map(|i| (i + 1..n).map(move |j| (i, j))).collect();
    }
    let sample: Vec<features::Features> = feats
        .iter()
        .map(|f| {
            let k = f.kps.len();
            let step = (k as f64 / PRESELECT_SAMPLE as f64).max(1.0);
            let idx: Vec<usize> = (0..k.min(PRESELECT_SAMPLE)).map(|i| (i as f64 * step) as usize).collect();
            features::Features { w: f.w, h: f.h, kps: idx.iter().map(|&i| f.kps[i]).collect(), desc: idx.iter().map(|&i| f.desc[i]).collect() }
        })
        .collect();
    let all: Vec<(usize, usize)> = (0..n).flat_map(|i| (i + 1..n).map(move |j| (i, j))).collect();
    let scores: Vec<usize> = all
        .par_iter()
        .map(|&(i, j)| if cancelled() { 0 } else { features::match_descriptors(&sample[i], &sample[j], 0.8).iter().filter(|m| m.2 < 64).count() })
        .collect();
    let mut keep = std::collections::BTreeSet::new();
    let mut per: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    for (k, &(i, j)) in all.iter().enumerate() {
        per[i].push((scores[k], j));
        per[j].push((scores[k], i));
    }
    for i in 0..n {
        per[i].sort_by_key(|a| std::cmp::Reverse(a.0));
        for &(s, j) in per[i].iter().take(PRESELECT_PARTNERS) {
            if s >= PRESELECT_MIN {
                keep.insert((i.min(j), i.max(j)));
            }
        }
        for d in 1..=PRESELECT_NEIGHBOURS {
            if i + d < n {
                keep.insert((i, i + d));
            }
        }
    }
    keep.into_iter().collect()
}

#[derive(Clone)]
pub enum Source {
    File(PathBuf),
    Memory(Arc<RgbImage>),
}

impl Source {
    pub fn load_full(&self) -> Result<Arc<RgbImage>, String> {
        match self {
            Source::File(p) => imgutil::load_photo(p).map(Arc::new),
            Source::Memory(m) => Ok(m.clone()),
        }
    }
    pub fn path(&self) -> Option<&PathBuf> {
        match self {
            Source::File(p) => Some(p),
            Source::Memory(_) => None,
        }
    }
}

pub struct LoadedPhoto {
    pub name: String,
    pub source: Source,
    pub w: u32,
    pub h: u32,
    pub work: Arc<RgbImage>,
    pub gray: Arc<Gray>,
    pub thumb: RgbImage,
    pub info: PhotoInfo,
}

/// Photos decoded at the same time: each full-size decode needs a lot of memory
/// (a 24 MP photo is ~150 MB while decoding), so loading uses a small pool.
pub const LOAD_THREADS: usize = 4;

pub fn load_pool() -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new().num_threads(LOAD_THREADS).build().expect("thread pool")
}

pub fn load(source: Source, name: String) -> Result<LoadedPhoto, String> {
    let full = source.load_full()?;
    let info = match &source {
        Source::File(p) => imgutil::read_info(p),
        Source::Memory(_) => PhotoInfo::default(),
    };
    let work = Arc::new(downscale_rgb(&full, WORK_SIZE));
    let gray = Arc::new(to_gray(&work));
    let thumb = downscale_rgb(&work, 220);
    Ok(LoadedPhoto { name, source, w: full.width(), h: full.height(), work, gray, thumb, info })
}

/// Field of view (long side, degrees) from photo information, if possible.
pub fn exif_hfov(info: &PhotoInfo) -> Option<f64> {
    let f35 = info.focal_35mm?;
    Some((2.0 * (36.0 / (2.0 * f35)).atan()).to_degrees())
}

#[derive(Clone, Copy, Debug)]
pub struct MatchSettings {
    pub points_per_photo: usize,
    pub points_per_pair: usize,
    /// leave out sky and clouds (clouds move between shots)
    pub ignore_sky: bool,
    /// spread the kept points evenly over the whole overlap (false = former row by row order, top rows first)
    pub even_thinning: bool,
}

impl Default for MatchSettings {
    fn default() -> Self {
        // points per pair: default 20
        MatchSettings { points_per_photo: 3500, points_per_pair: 20, ignore_sky: true, even_thinning: true }
    }
}

pub struct PairStat {
    pub a: usize,
    pub b: usize,
    pub points: usize,
}

pub struct MatchResult {
    pub cps: Vec<ControlPoint>,
    pub pairs: Vec<PairStat>,
    pub detect_ms: u128,
    pub match_ms: u128,
    pub keypoints: usize,
    pub sky_removed: usize,
}

/// Normalised coordinate of a work-image pixel.
#[inline]
pub fn norm_coord(x: f32, y: f32, w: usize, h: usize) -> [f64; 2] {
    let l = w.max(h) as f64;
    [((x as f64 + 0.5) - w as f64 * 0.5) / l, ((y as f64 + 0.5) - h as f64 * 0.5) / l]
}

#[inline]
pub fn denorm_coord(p: [f64; 2], w: usize, h: usize) -> (f32, f32) {
    let l = w.max(h) as f64;
    ((p[0] * l + w as f64 * 0.5 - 0.5) as f32, (p[1] * l + h as f64 * 0.5 - 0.5) as f32)
}

/// Matches taken from a pair before checking them one by one.
pub const VERIFY_POOL: usize = 200;
/// Correlation a match must reach in the check.
pub const VERIFY_MIN_NCC: f32 = 0.75;
/// A pair needs at least this many checked matches to count as connected.
pub const MIN_VERIFIED: usize = 6;

/// Check each match by comparing the image patches around both points (with the local zoom and
/// rotation of the pair): drops matches on plain surfaces, ambiguous spots and wrong matches, and
/// moves the good ones to sub-pixel accuracy.
pub fn verify_matches(ga: &Gray, gb: &Gray, h: &[f64; 9], corrs: &[features::Corr]) -> Vec<features::Corr> {
    use crate::guided;
    let mid = (ga.w as f64 / 2.0, ga.h as f64 / 2.0);
    let j0 = guided::jacobian(h, mid);
    let s = (j0[0] * j0[3] - j0[1] * j0[2]).abs().sqrt();
    let fs = (s < 1.0 / 1.5).then(|| crate::imgutil::blur(ga, guided::soft_sigma(1.0 / s)));
    let ms = (s > 1.5).then(|| crate::imgutil::blur(gb, guided::soft_sigma(s)));
    let c = guided::Corr { fixed: ga, fixed_soft: fs.as_ref(), moving: gb, moving_soft: ms.as_ref(), tpl_r: guided::TPL_R };
    corrs
        .par_iter()
        .filter_map(|m| {
            let pa = (m.ax as f64, m.ay as f64);
            let j = guided::jacobian(h, pa);
            let (x, y, v) = guided::refine(&c, pa, j, (m.bx as f64, m.by as f64), 2.0 * s.max(1.0))?;
            (v >= VERIFY_MIN_NCC).then_some(features::Corr { bx: x as f32, by: y as f32, ..*m })
        })
        .collect()
}

pub fn find_points(
    works: &[Arc<RgbImage>],
    grays: &[Arc<Gray>],
    keep_manual: &[ControlPoint],
    settings: MatchSettings,
    report: &(dyn Fn(f32, &str) + Sync),
) -> MatchResult {
    let n = grays.len();
    report(0.02, "Detecting points in every photo");
    let t0 = Instant::now();
    let detected: Vec<(features::Features, usize)> = grays
        .par_iter()
        .zip(works.par_iter())
        .map(|(g, w)| {
            if cancelled() {
                return (features::Features { w: g.w, h: g.h, kps: vec![], desc: vec![] }, 0);
            }
            let mut f = features::detect(g, settings.points_per_photo);
            let mut removed = 0;
            if settings.ignore_sky {
                let mask = features::sky_mask(w, g);
                removed = features::remove_masked(&mut f, &mask);
            }
            (f, removed)
        })
        .collect();
    let sky_removed: usize = detected.iter().map(|d| d.1).sum();
    let feats: Vec<features::Features> = detected.into_iter().map(|d| d.0).collect();
    let detect_ms = t0.elapsed().as_millis();
    let keypoints = feats.iter().map(|f| f.kps.len()).sum();
    let t1 = Instant::now();
    if n > ALL_PAIRS_LIMIT {
        report(0.08, "Picking promising photo pairs");
    }
    let pairs = candidate_pairs(&feats);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let total = pairs.len().max(1);
    let results: Vec<(usize, usize, Vec<ControlPoint>)> = pairs
        .par_iter()
        .map(|&(i, j)| {
            if cancelled() {
                return (i, j, Vec::new());
            }
            // take a generous set, check every match by correlation, then keep the best spread
            let r = features::match_pair(&feats[i], &feats[j], VERIFY_POOL, (i * 7919 + j * 104729) as u64 + 1).and_then(|mut pm| {
                let checked = verify_matches(&grays[i], &grays[j], &pm.h, &pm.corrs);
                if checked.len() < MIN_VERIFIED {
                    return None;
                }
                pm.corrs = features::thin_mode(checked, grays[i].w, grays[i].h, settings.points_per_pair, settings.even_thinning);
                Some(pm)
            });
            let k = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            report(0.1 + 0.9 * k as f32 / total as f32, &format!("Matching photo pairs ({k} of {total})"));
            let cps = match r {
                Some(pm) => pm
                    .corrs
                    .iter()
                    .map(|c| ControlPoint {
                        a: i,
                        b: j,
                        pa: norm_coord(c.ax, c.ay, grays[i].w, grays[i].h),
                        pb: norm_coord(c.bx, c.by, grays[j].w, grays[j].h),
                        ..ControlPoint::new(0, 0, [0.0; 2], [0.0; 2], false)
                    })
                    .collect(),
                None => vec![],
            };
            (i, j, cps)
        })
        .collect();
    let mut cps: Vec<ControlPoint> = keep_manual.iter().filter(|c| c.manual || c.locked).copied().collect();
    let mut stats = Vec::new();
    for (a, b, c) in results {
        if !c.is_empty() {
            stats.push(PairStat { a, b, points: c.len() });
            cps.extend(c);
        }
    }
    MatchResult { cps, pairs: stats, detect_ms, match_ms: t1.elapsed().as_millis(), keypoints, sky_removed }
}

/// Statistical clean-up factor: remove points worse than mean + OUTLIER_SIGMA * standard deviation
/// (the original uses 2.0 as well, per photo pair and over the whole project).
pub const OUTLIER_SIGMA: f64 = 2.0;

fn mean_sd(v: &[f64]) -> (f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    let m = v.iter().sum::<f64>() / v.len() as f64;
    let var = v.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / v.len() as f64;
    (m, var.sqrt())
}

/// Which points the statistical clean-up removes: worse than mean + 2 sigma within their photo
/// pair, then worse than mean + 2 sigma of all remaining points. Points placed by hand are kept,
/// and a pair never drops below 3 points (PanoWeave's own safety rules).
pub fn statistical_outliers(cps: &[ControlPoint]) -> Vec<bool> {
    statistical_outliers_ex(cps, false)
}

/// `statistical_outliers`; with `skip_areas` the automatic control areas are neither measured nor removed
/// (fine alignment builds them anew, so removing them only inflates the count). The result still has one
/// entry per point.
pub fn statistical_outliers_ex(cps: &[ControlPoint], skip_areas: bool) -> Vec<bool> {
    if skip_areas && cps.iter().any(|c| c.kind == crate::align::CpKind::Area) {
        let idx: Vec<usize> = (0..cps.len()).filter(|&k| cps[k].kind != crate::align::CpKind::Area).collect();
        let sub: Vec<ControlPoint> = idx.iter().map(|&k| cps[k]).collect();
        let rm_sub = statistical_outliers_ex(&sub, false);
        let mut rm = vec![false; cps.len()];
        for (j, &k) in idx.iter().enumerate() {
            rm[k] = rm_sub[j];
        }
        return rm;
    }
    let n = cps.len();
    let mut rm = vec![false; n];
    let mut pairs: std::collections::HashMap<(usize, usize), Vec<usize>> = std::collections::HashMap::new();
    for (k, c) in cps.iter().enumerate() {
        pairs.entry((c.a.min(c.b), c.a.max(c.b))).or_default().push(k);
    }
    for idx in pairs.values() {
        if idx.len() < 3 {
            continue;
        }
        let errs: Vec<f64> = idx.iter().map(|&k| cps[k].err).collect();
        let (m, sd) = mean_sd(&errs);
        let lim = m + OUTLIER_SIGMA * sd;
        for &k in idx {
            if cps[k].err > lim && !cps[k].manual && !cps[k].locked {
                rm[k] = true;
            }
        }
    }
    let rest: Vec<f64> = (0..n).filter(|&k| !rm[k]).map(|k| cps[k].err).collect();
    let (m, sd) = mean_sd(&rest);
    let lim = m + OUTLIER_SIGMA * sd;
    for k in 0..n {
        if !rm[k] && cps[k].err > lim && !cps[k].manual && !cps[k].locked {
            rm[k] = true;
        }
    }
    // keep at least 3 points per pair
    for idx in pairs.values() {
        let left = idx.iter().filter(|&&k| !rm[k]).count();
        let mut back: Vec<usize> = idx.iter().copied().filter(|&k| rm[k]).collect();
        back.sort_by(|&x, &y| cps[x].err.partial_cmp(&cps[y].err).unwrap_or(std::cmp::Ordering::Equal));
        // give back the best removed points until the pair has 3 again
        for k in back.into_iter().take(3usize.saturating_sub(left)) {
            rm[k] = false;
        }
    }
    rm
}

const IDENTITY_ROT: M3 = crate::geometry::IDENTITY;

#[derive(Clone)]
pub struct AlignResult {
    pub rots: Vec<M3>,
    pub lenses: Vec<Lens>,
    pub cps: Vec<ControlPoint>,
    pub removed: usize,
    /// in how many alignment passes points were removed (the check runs again after new points are added)
    pub removal_passes: usize,
    pub ms: u128,
    pub mean_err: f64,
    pub max_err: f64,
    /// statistics of the fine alignment (patch matching), if it ran
    pub fine: Option<crate::fine_align::FineStats>,
    /// parallax correction fields per photo (parallax mode)
    pub fields: Vec<Option<std::sync::Arc<crate::fine_align::DisplacementField>>>,
    /// geometric pair check (overlaps, searched pairs), if it ran
    pub pairs: Option<crate::pair_check::PairSummary>,
}

#[derive(Clone, Copy)]
pub struct AlignSettings {
    pub fine_tune_fov: bool,
    pub distortion: bool,
    pub remove_outliers: bool,
    pub level: bool,
    /// weight factor of protected points in the optimisation (`align::LOCKED_WEIGHT`; 1.0 = like any point)
    pub locked_weight: f64,
    /// leave the automatic control areas out of the outlier clean-up (fine alignment rebuilds them anyway)
    pub outliers_skip_areas: bool,
}

impl Default for AlignSettings {
    fn default() -> Self {
        AlignSettings { fine_tune_fov: true, distortion: true, remove_outliers: true, level: true, locked_weight: align::LOCKED_WEIGHT, outliers_skip_areas: true }
    }
}

pub fn align_all(
    lens_of: &[usize],
    lenses_in: &[Lens],
    long_side: &[f64],
    cps_in: &[ControlPoint],
    s: &AlignSettings,
    report: &(dyn Fn(f32, &str) + Sync),
) -> AlignResult {
    align_all_from(lens_of, lenses_in, long_side, cps_in, s, None, report)
}

/// Like `align_all`; with `start` the optimisation starts from these rotations instead of the closed-form
/// spanning-tree placement (used by the "Maximum accuracy" search, which tries other ways of placing the photos).
/// `None` = exactly `align_all`.
pub fn align_all_from(
    lens_of: &[usize],
    lenses_in: &[Lens],
    long_side: &[f64],
    cps_in: &[ControlPoint],
    s: &AlignSettings,
    start: Option<Vec<M3>>,
    report: &(dyn Fn(f32, &str) + Sync),
) -> AlignResult {
    let t0 = Instant::now();
    let n = lens_of.len();
    let inp = AlignInput { n, lens_of, long_side };
    let mut lenses = lenses_in.to_vec();
    let mut cps = cps_in.to_vec();
    // anchor = photo with most points
    let mut cnt = vec![0usize; n];
    for c in &cps {
        cnt[c.a] += 1;
        cnt[c.b] += 1;
    }
    let anchor = (0..n).max_by_key(|&i| cnt[i]).unwrap_or(0);
    report(0.1, "Estimating the lens");
    align::guess_fov(&inp, &mut lenses, &cps, anchor);
    if cancelled() {
        return AlignResult { rots: vec![IDENTITY_ROT; n], lenses, cps, removed: 0, removal_passes: 0, ms: 0, mean_err: 0.0, max_err: 0.0, fine: None, fields: Vec::new(), pairs: None };
    }
    report(0.3, "Placing photos");
    let mut rots = match start {
        Some(r) if r.len() == n => r,
        _ => align::initial_rotations_ex(&inp, &lenses, &cps, anchor, s.locked_weight > 1.0),
    };
    align::optimize(&inp, &mut rots, &mut lenses, &cps, anchor, &OptimizeOptions { locked_weight: s.locked_weight, ..Default::default() }, 25);
    report(0.5, "Fine-tuning");
    let opts = OptimizeOptions { focal: s.fine_tune_fov, distortion: s.distortion, cauchy: false, locked_weight: s.locked_weight };
    align::optimize(&inp, &mut rots, &mut lenses, &cps, anchor, &opts, 60);
    let mut removed = 0;
    if s.remove_outliers {
        // the original's "Clean control points": points worse than mean + 2 sigma,
        // first within each photo pair, then over the whole project
        align::cp_errors(&inp, &rots, &lenses, &mut cps);
        let rm = statistical_outliers_ex(&cps, s.outliers_skip_areas);
        let before = cps.len();
        cps = cps.iter().zip(&rm).filter(|(_, r)| !**r).map(|(c, _)| *c).collect();
        removed = before - cps.len();
        if removed > 0 {
            report(0.7, "Removing bad points and refining");
            align::optimize(&inp, &mut rots, &mut lenses, &cps, anchor, &opts, 40);
        }
    }
    align::cp_errors(&inp, &rots, &lenses, &mut cps);
    if s.level {
        let used: Vec<bool> = (0..n).map(|i| cps.iter().any(|c| c.a == i || c.b == i) || n == 1).collect();
        align::straighten(&mut rots, &used);
    }
    let mean_err = if cps.is_empty() { 0.0 } else { cps.iter().map(|c| c.err).sum::<f64>() / cps.len() as f64 };
    let max_err = cps.iter().map(|c| c.err).fold(0.0, f64::max);
    AlignResult { rots, lenses, cps, removed, removal_passes: usize::from(removed > 0), ms: t0.elapsed().as_millis(), mean_err, max_err, fine: None, fields: Vec::new(), pairs: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_areas_are_left_out_of_the_outlier_clean_up() {
        use crate::align::CpKind;
        let mut cps: Vec<ControlPoint> = (0..20)
            .map(|i| {
                let mut c = ControlPoint::new(0, 1, [0.01 * i as f64, 0.0], [0.0; 2], false);
                c.err = 0.5;
                c
            })
            .collect();
        // two bad points and two bad areas
        for k in [3usize, 4] {
            cps[k].err = 30.0;
        }
        for k in [10usize, 11] {
            cps[k].err = 30.0;
            cps[k].kind = CpKind::Area;
        }
        let all = statistical_outliers_ex(&cps, false);
        assert!(all[3] && all[4] && all[10] && all[11]);
        let rm = statistical_outliers_ex(&cps, true);
        assert!(rm[3] && rm[4], "bad real points are still removed");
        assert!(!rm[10] && !rm[11], "areas are kept");
        assert_eq!(rm.len(), cps.len());
    }
    use crate::geometry::{mm, transpose};
    use crate::testutil::{crop, texture};

    #[test]
    fn coordinates_are_normalised_by_long_side() {
        // u = (x + 0.5 - w/2) / L, v = (y + 0.5 - h/2) / L, L = max(w, h)
        let p = norm_coord(0.0, 0.0, 400, 300);
        assert!((p[0] - (0.5 - 200.0) / 400.0).abs() < 1e-12);
        assert!((p[1] - (0.5 - 150.0) / 400.0).abs() < 1e-12);
        // the centre of the image is the origin
        let c = norm_coord(199.5, 149.5, 400, 300);
        assert!(c[0].abs() < 1e-12 && c[1].abs() < 1e-12);
        // portrait: the long side is the height
        let q = norm_coord(0.0, 0.0, 300, 400);
        assert!((q[0] - (0.5 - 150.0) / 400.0).abs() < 1e-12);
        // resolution independent: same relative position gives the same value
        let a = norm_coord(99.5, 74.5, 200, 150);
        let b = norm_coord(199.5, 149.5, 400, 300);
        assert!((a[0] - b[0]).abs() < 1e-12 && (a[1] - b[1]).abs() < 1e-12);
    }

    #[test]
    fn denorm_inverts_norm() {
        for &(x, y, w, h) in &[(0.0f32, 0.0f32, 400usize, 300usize), (123.0, 77.0, 400, 300), (50.0, 399.0, 300, 400)] {
            let (bx, by) = denorm_coord(norm_coord(x, y, w, h), w, h);
            assert!((bx - x).abs() < 1e-3 && (by - y).abs() < 1e-3);
        }
    }

    #[test]
    fn hfov_from_35mm_focal_length() {
        let info = PhotoInfo { focal_35mm: Some(36.0), ..Default::default() };
        // 36 mm sensor side with a 36 mm lens -> 2 * atan(0.5) = 53.13 degrees
        assert!((exif_hfov(&info).unwrap() - 53.130102).abs() < 1e-4);
        let wide = PhotoInfo { focal_35mm: Some(18.0), ..Default::default() };
        assert!(exif_hfov(&wide).unwrap() > exif_hfov(&info).unwrap());
        assert!(exif_hfov(&PhotoInfo::default()).is_none());
    }

    #[test]
    fn match_settings_defaults() {
        let s = MatchSettings::default();
        assert_eq!(s.points_per_pair, 20);
        assert!(s.points_per_photo >= s.points_per_pair);
        assert!(s.ignore_sky);
    }

    fn empty_feats(n: usize) -> Vec<features::Features> {
        (0..n).map(|_| features::Features { w: 10, h: 10, kps: vec![], desc: vec![] }).collect()
    }

    #[test]
    fn few_photos_try_every_pair() {
        assert!(candidate_pairs(&empty_feats(0)).is_empty());
        assert!(candidate_pairs(&empty_feats(1)).is_empty());
        assert_eq!(candidate_pairs(&empty_feats(2)), vec![(0, 1)]);
        let p = candidate_pairs(&empty_feats(5));
        assert_eq!(p.len(), 10);
        assert!(p.iter().all(|&(i, j)| i < j));
        let at_limit = candidate_pairs(&empty_feats(ALL_PAIRS_LIMIT));
        assert_eq!(at_limit.len(), ALL_PAIRS_LIMIT * (ALL_PAIRS_LIMIT - 1) / 2);
    }

    #[test]
    fn many_photos_keep_neighbours() {
        let n = ALL_PAIRS_LIMIT + 5;
        let p = candidate_pairs(&empty_feats(n));
        assert!(p.len() < n * (n - 1) / 2);
        for i in 0..n - 1 {
            assert!(p.contains(&(i, i + 1)), "neighbour {i}");
        }
        assert!(p.iter().all(|&(i, j)| i < j && j < n));
    }

    fn err_cp(a: usize, b: usize, err: f64, manual: bool) -> ControlPoint {
        let mut c = ControlPoint::new(a, b, [0.0; 2], [0.0; 2], manual);
        c.err = err;
        c
    }

    #[test]
    fn outliers_are_flagged_per_pair() {
        let mut cps: Vec<ControlPoint> = (0..10).map(|_| err_cp(0, 1, 1.0, false)).collect();
        cps.push(err_cp(0, 1, 50.0, false));
        let rm = statistical_outliers(&cps);
        assert!(rm[10]);
        assert!(rm[..10].iter().all(|r| !r));
    }

    #[test]
    fn manual_points_are_never_removed() {
        let mut cps: Vec<ControlPoint> = (0..10).map(|_| err_cp(0, 1, 1.0, false)).collect();
        cps.push(err_cp(0, 1, 50.0, true));
        assert!(statistical_outliers(&cps).iter().all(|r| !r));
    }

    #[test]
    fn protected_points_are_never_flagged_as_outliers() {
        let mut cps: Vec<ControlPoint> = (0..10).map(|_| err_cp(0, 1, 1.0, false)).collect();
        let mut bad = err_cp(0, 1, 50.0, false);
        bad.locked = true;
        cps.push(bad);
        assert!(statistical_outliers(&cps).iter().all(|r| !r));
    }

    #[test]
    fn small_pairs_keep_at_least_three_points() {
        let mut cps: Vec<ControlPoint> = (0..30).map(|_| err_cp(1, 2, 1.0, false)).collect();
        cps.extend([err_cp(0, 1, 1.0, false), err_cp(0, 1, 1.0, false), err_cp(0, 1, 100.0, false)]);
        let rm = statistical_outliers(&cps);
        let kept = (30..33).filter(|&k| !rm[k]).count();
        assert!(kept >= 3);
    }

    #[test]
    fn equal_errors_remove_nothing() {
        let cps: Vec<ControlPoint> = (0..8).map(|_| err_cp(0, 1, 2.0, false)).collect();
        assert!(statistical_outliers(&cps).iter().all(|r| !r));
        assert!(statistical_outliers(&[]).is_empty());
    }

    #[test]
    fn mean_sd_values() {
        assert_eq!(mean_sd(&[]), (0.0, 0.0));
        let (m, sd) = mean_sd(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]);
        assert!((m - 5.0).abs() < 1e-12 && (sd - 2.0).abs() < 1e-12);
    }

    #[test]
    fn align_without_points_does_not_panic() {
        let lens = Lens { f: 1.0, k1: 0.0, from_exif: true };
        let r = align_all(&[0, 0], &[lens], &[100.0, 100.0], &[], &AlignSettings::default(), &|_, _| {});
        assert_eq!(r.rots.len(), 2);
        assert_eq!(r.mean_err, 0.0);
        assert_eq!(r.removed, 0);
    }

    #[test]
    fn rotation_free_points_align_to_identity() {
        let lens = Lens::from_hfov_deg(60.0);
        let cps: Vec<ControlPoint> = (0..15)
            .map(|i| {
                let p = [-0.4 + 0.05 * i as f64, 0.2 - 0.025 * i as f64];
                ControlPoint::new(0, 1, p, p, false)
            })
            .collect();
        let s = AlignSettings { fine_tune_fov: false, distortion: false, remove_outliers: false, level: false, ..Default::default() };
        let lens = Lens { from_exif: true, ..lens };
        let r = align_all(&[0, 0], &[lens], &[1000.0, 1000.0], &cps, &s, &|_, _| {});
        for i in 0..2 {
            for a in 0..3 {
                for b in 0..3 {
                    let id = if a == b { 1.0 } else { 0.0 };
                    assert!((r.rots[i][a][b] - id).abs() < 1e-6);
                }
            }
        }
        assert!(r.max_err < 1e-4, "max_err {}", r.max_err);
    }

    fn rotation_angle(a: &M3, b: &M3) -> f64 {
        let r = mm(a, &transpose(b));
        let tr = r[0][0] + r[1][1] + r[2][2];
        ((tr - 1.0) / 2.0).clamp(-1.0, 1.0).acos()
    }

    /// End to end: cut a synthetic texture into three overlapping views with a known shift,
    /// find the points, align and compare with the truth.
    #[test]
    fn end_to_end_three_overlapping_crops() {
        let big = texture(520, 260, 77);
        // 320 x 260 views, shifted by 100 px each
        let views: Vec<RgbImage> = (0..3).map(|i| crop(&big, i * 100, 0, 320, 260)).collect();
        let works: Vec<Arc<RgbImage>> = views.iter().cloned().map(Arc::new).collect();
        let grays: Vec<Arc<Gray>> = views.iter().map(|v| Arc::new(to_gray(v))).collect();

        let mr = find_points(&works, &grays, &[], MatchSettings { points_per_photo: 800, points_per_pair: 40, ignore_sky: false, even_thinning: true }, &|_, _| {});
        assert!(mr.keypoints > 100);
        assert!(mr.pairs.iter().any(|p| (p.a, p.b) == (0, 1)), "pairs {:?}", mr.pairs.iter().map(|p| (p.a, p.b, p.points)).collect::<Vec<_>>());
        assert!(mr.pairs.iter().any(|p| (p.a, p.b) == (1, 2)));
        // every found point must agree with the true shift: x_b = x_a - 100 * (b - a)
        let mut bad = 0;
        for cp in &mr.cps {
            let (ax, ay) = denorm_coord(cp.pa, 320, 260);
            let (bx, by) = denorm_coord(cp.pb, 320, 260);
            let dx = 100.0 * (cp.b as f32 - cp.a as f32);
            if (bx - (ax - dx)).abs() > 2.0 || (by - ay).abs() > 2.0 {
                bad += 1;
            }
        }
        assert!(!mr.cps.is_empty());
        assert!(bad * 20 <= mr.cps.len(), "{bad} of {} points off the true shift", mr.cps.len());

        let lens_of = [0usize; 3];
        let long = [320.0; 3];
        let lens = Lens::from_hfov_deg(50.0);
        let s = AlignSettings { fine_tune_fov: false, distortion: false, remove_outliers: true, level: false, ..Default::default() };
        let r = align_all(&lens_of, &[lens], &long, &mr.cps, &s, &|_, _| {});
        assert!(r.mean_err < 2.0, "mean error {} px", r.mean_err);
        // equal shifts mean equal rotations between neighbours, and about twice that between 0 and 2
        let a01 = rotation_angle(&r.rots[0], &r.rots[1]);
        let a12 = rotation_angle(&r.rots[1], &r.rots[2]);
        let a02 = rotation_angle(&r.rots[0], &r.rots[2]);
        assert!(a01 > 0.05 && a12 > 0.05);
        assert!((a01 - a12).abs() < 0.25 * a01.max(a12), "{a01} vs {a12}");
        assert!((a02 - (a01 + a12)).abs() < 0.15 * a02, "{a02} vs {}", a01 + a12);
    }

    /// `FineConfig::record_patches` only decides whether the patch window gets data: with it off
    /// the fine alignment must give exactly the same rotations, lenses and points.
    #[test]
    fn fine_alignment_is_the_same_without_patch_recording() {
        use crate::fine_align::{self, FaPhoto, FineConfig};
        let big = texture(520, 260, 91);
        let views: Vec<RgbImage> = (0..3).map(|i| crop(&big, i * 100, 0, 320, 260)).collect();
        let works: Vec<Arc<RgbImage>> = views.iter().cloned().map(Arc::new).collect();
        let grays: Vec<Arc<Gray>> = views.iter().map(|v| Arc::new(to_gray(v))).collect();
        let mr = find_points(&works, &grays, &[], MatchSettings { points_per_photo: 800, points_per_pair: 40, ignore_sky: false, even_thinning: true }, &|_, _| {});
        let lens_of = [0usize; 3];
        let long = [320.0; 3];
        let s = AlignSettings { fine_tune_fov: false, distortion: false, remove_outliers: true, level: false, ..Default::default() };
        let r = align_all(&lens_of, &[Lens::from_hfov_deg(50.0)], &long, &mr.cps, &s, &|_, _| {});
        let fa: Vec<FaPhoto> = views.iter().zip(&grays).map(|(v, g)| FaPhoto { gray: g.clone(), rgb: Arc::new(v.clone()) }).collect();
        let run = |record: bool| {
            let cfg = FineConfig { record_patches: record, ..FineConfig::default() };
            fine_align::run(&fa, &lens_of, &long, &r.lenses, &r.rots, &r.cps, false, false, &cfg, &|_, _| {})
        };
        let (rots_on, lenses_on, cps_on, st_on) = run(true);
        let (rots_off, lenses_off, cps_off, st_off) = run(false);
        assert!(st_on.patches_total > 0, "the test must measure patches");
        assert_eq!(st_on.patches_total, st_off.patches_total);
        assert_eq!(st_on.patches_valid, st_off.patches_valid);
        // parallel sums may round differently in the last bits, so compare with a tolerance
        let close = |x: f64, y: f64| (x - y).abs() < 1e-9;
        for (a, b) in rots_on.iter().zip(&rots_off) {
            assert!((0..3).all(|i| (0..3).all(|j| close(a[i][j], b[i][j]))), "{a:?} vs {b:?}");
        }
        assert_eq!(lenses_on.len(), lenses_off.len());
        for (a, b) in lenses_on.iter().zip(&lenses_off) {
            assert!(close(a.f, b.f) && close(a.k1, b.k1));
        }
        // the area selection walks a HashMap, so the order of the points may differ: compare sorted
        let sorted = |v: &[ControlPoint]| {
            let mut v = v.to_vec();
            v.sort_by(|x, y| (x.a, x.b, x.pa[0], x.pa[1]).partial_cmp(&(y.a, y.b, y.pa[0], y.pa[1])).unwrap());
            v
        };
        let (cps_on, cps_off) = (sorted(&cps_on), sorted(&cps_off));
        assert_eq!(cps_on.len(), cps_off.len());
        for (a, b) in cps_on.iter().zip(&cps_off) {
            assert_eq!((a.a, a.b), (b.a, b.b));
            assert!((0..2).all(|i| close(a.pa[i], b.pa[i]) && close(a.pb[i], b.pb[i])), "{:?} vs {:?}", (a.pa, a.pb), (b.pa, b.pb));
        }
    }

    #[test]
    fn fine_alignment_leaves_protected_points_alone_and_reports_far_ones() {
        use crate::align::CpKind;
        use crate::fine_align::{self, FaPhoto, FineConfig};
        let big = texture(520, 260, 91);
        let views: Vec<RgbImage> = (0..3).map(|i| crop(&big, i * 100, 0, 320, 260)).collect();
        let works: Vec<Arc<RgbImage>> = views.iter().cloned().map(Arc::new).collect();
        let grays: Vec<Arc<Gray>> = views.iter().map(|v| Arc::new(to_gray(v))).collect();
        let mr = find_points(&works, &grays, &[], MatchSettings { points_per_photo: 800, points_per_pair: 40, ignore_sky: false, even_thinning: true }, &|_, _| {});
        let lens_of = [0usize; 3];
        let long = [320.0; 3];
        let s = AlignSettings { fine_tune_fov: false, distortion: false, remove_outliers: true, level: false, ..Default::default() };
        let r = align_all(&lens_of, &[Lens::from_hfov_deg(50.0)], &long, &mr.cps, &s, &|_, _| {});
        let fa: Vec<FaPhoto> = views.iter().zip(&grays).map(|(v, g)| FaPhoto { gray: g.clone(), rgb: Arc::new(v.clone()) }).collect();
        let cfg = FineConfig { drop_invalid_points: true, ..FineConfig::default() };
        let run = |cps: &[ControlPoint]| fine_align::run(&fa, &lens_of, &long, &r.lenses, &r.rots, cps, false, false, &cfg, &|_, _| {});
        // all points protected and where they belong: none moved, dropped or reported
        let mut all = r.cps.clone();
        for c in all.iter_mut() {
            c.locked = true;
        }
        let (_, _, out, st) = run(&all);
        for p in all.iter().filter(|c| c.kind == CpKind::Point) {
            let q = out.iter().find(|c| c.locked && (c.a, c.b, c.pa) == (p.a, p.b, p.pa)).expect("a protected point is never dropped");
            assert_eq!((q.a, q.b, q.pb, q.weight), (p.a, p.b, p.pb, p.weight), "a protected point is never moved or re-weighted");
        }
        assert_eq!(st.locked_far, 0, "points in place are not reported: {:?}", st.log);
        // two of the measured ones put 5 px and 15 px off in photo B: both reported, both left as they are
        let measured: Vec<usize> = (0..r.cps.len()).filter(|&k| r.cps[k].kind == CpKind::Point && out.iter().any(|c| (c.a, c.b, c.pa) == (r.cps[k].a, r.cps[k].b, r.cps[k].pa) && c.conf > 0.5)).collect();
        assert!(measured.len() >= 2);
        let mut cps = r.cps.clone();
        let (k5, k15) = (measured[0], measured[measured.len() - 1]);
        cps[k5].pb[0] += 5.0 / 320.0;
        cps[k15].pb[0] += 15.0 / 320.0;
        cps[k5].locked = true;
        cps[k15].locked = true;
        let (_, _, out, st) = run(&cps);
        for k in [k5, k15] {
            let q = out.iter().find(|c| c.locked && (c.a, c.b, c.pa) == (cps[k].a, cps[k].b, cps[k].pa)).expect("kept");
            assert_eq!(q.pb, cps[k].pb, "left where it is");
        }
        assert_eq!(st.locked_far, 2, "{:?}", st.log);
        assert!((st.locked_far_max - 15.0).abs() < 1.5, "the largest is about 15 px: {}", st.locked_far_max);

        // without control areas no round can be judged: nothing changes, and it is said
        let cfg = FineConfig { areas: false, ..FineConfig::default() };
        let (rots, _, _, st) = fine_align::run(&fa, &lens_of, &long, &r.lenses, &r.rots, &r.cps, false, false, &cfg, &|_, _| {});
        assert!(st.no_test_spots, "{:?}", st.log);
        assert_eq!(rots, r.rots);
        assert!(!st.log.iter().any(|l| l.contains("below the threshold")), "{:?}", st.log);
    }
}
