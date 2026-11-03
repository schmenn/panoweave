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
        per[i].sort_by(|a, b| b.0.cmp(&a.0));
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
}

impl Default for MatchSettings {
    fn default() -> Self {
        // points per pair: /Assistant/nControlPoints default 20 (hugin.exe sub_14019f160)
        MatchSettings { points_per_photo: 3500, points_per_pair: 20, ignore_sky: true }
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
                pm.corrs = features::thin(checked, grays[i].w, grays[i].h, settings.points_per_pair);
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
    let mut cps: Vec<ControlPoint> = keep_manual.iter().filter(|c| c.manual).copied().collect();
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
/// (hugin.exe sub_140223410 passes 2.0, f64 at 0x14038cb40, to getCPoutsideLimit_pair / getCPoutsideLimit).
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
            if cps[k].err > lim && !cps[k].manual {
                rm[k] = true;
            }
        }
    }
    let rest: Vec<f64> = (0..n).filter(|&k| !rm[k]).map(|k| cps[k].err).collect();
    let (m, sd) = mean_sd(&rest);
    let lim = m + OUTLIER_SIGMA * sd;
    for k in 0..n {
        if !rm[k] && cps[k].err > lim && !cps[k].manual {
            rm[k] = true;
        }
    }
    // keep at least 3 points per pair
    for idx in pairs.values() {
        let mut left = idx.iter().filter(|&&k| !rm[k]).count();
        let mut back: Vec<usize> = idx.iter().copied().filter(|&k| rm[k]).collect();
        back.sort_by(|&x, &y| cps[x].err.partial_cmp(&cps[y].err).unwrap_or(std::cmp::Ordering::Equal));
        for k in back {
            if left >= 3 {
                break;
            }
            rm[k] = false;
            left += 1;
        }
    }
    rm
}

const IDENTITY_ROT: M3 = crate::geometry::IDENTITY;

pub struct AlignResult {
    pub rots: Vec<M3>,
    pub lenses: Vec<Lens>,
    pub cps: Vec<ControlPoint>,
    pub removed: usize,
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

pub struct AlignSettings {
    pub fine_tune_fov: bool,
    pub distortion: bool,
    pub remove_outliers: bool,
    pub level: bool,
}

impl Default for AlignSettings {
    fn default() -> Self {
        AlignSettings { fine_tune_fov: true, distortion: true, remove_outliers: true, level: true }
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
        return AlignResult { rots: vec![IDENTITY_ROT; n], lenses, cps, removed: 0, ms: 0, mean_err: 0.0, max_err: 0.0, fine: None, fields: Vec::new(), pairs: None };
    }
    report(0.3, "Placing photos");
    let mut rots = align::initial_rotations(&inp, &lenses, &cps, anchor);
    align::optimize(&inp, &mut rots, &mut lenses, &cps, anchor, &OptimizeOptions { focal: false, distortion: false, cauchy: false }, 25);
    report(0.5, "Fine-tuning");
    let opts = OptimizeOptions { focal: s.fine_tune_fov, distortion: s.distortion, cauchy: false };
    align::optimize(&inp, &mut rots, &mut lenses, &cps, anchor, &opts, 60);
    let mut removed = 0;
    if s.remove_outliers {
        // the original's "Clean control points": points worse than mean + 2 sigma,
        // first within each photo pair, then over the whole project (hugin.exe sub_140223410)
        align::cp_errors(&inp, &rots, &lenses, &mut cps);
        let rm = statistical_outliers(&cps);
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
    AlignResult { rots, lenses, cps, removed, ms: t0.elapsed().as_millis(), mean_err, max_err, fine: None, fields: Vec::new(), pairs: None }
}
