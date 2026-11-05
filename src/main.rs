#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod align;
mod crashlog;
mod app;
mod engine;
mod features;
mod fine_align;
mod fusion;
mod gainmap;
mod gainmap_solve;
mod geometry;
mod guided;
mod click_match;
mod pair_check;
mod patch_matcher;
mod patch_store;
mod patch_viz;
mod patch_window;
mod photo_cache;
mod imgutil;
mod project;
mod sample;
#[allow(dead_code)] // the painting tool in the UI uses the rest
mod masks;
mod seams;
mod stitch;
mod strip;
mod strip_write;
mod sysmem;
#[cfg(test)]
mod testutil;

use std::path::PathBuf;

pub struct Args {
    pub screenshot: Option<PathBuf>,
    pub demo: Option<String>,
    pub files: Vec<PathBuf>,
}

fn parse_args() -> (Args, bool) {
    let mut a = Args { screenshot: None, demo: None, files: vec![] };
    let mut selftest = false;
    let mut it = std::env::args().skip(1);
    while let Some(s) = it.next() {
        match s.as_str() {
            "--screenshot" => a.screenshot = it.next().map(PathBuf::from),
            "--demo" => a.demo = it.next(),
            "--selftest" => selftest = true,
            "--loadtest" => {
                a.demo = Some("loadtest".into());
                selftest = true;
            }
            "--kpdebug" => {
                a.demo = Some("kpdebug".into());
                selftest = true;
            }
            "--pairtest" => {
                a.demo = Some("pairtest".into());
                selftest = true;
            }
            "--paircheck" => {
                a.demo = Some("paircheck".into());
                selftest = true;
            }
            _ => a.files.push(PathBuf::from(s)),
        }
    }
    (a, selftest)
}

/// `--imgdiff <a> <b> [diff.png]`: compares two pictures pixel by pixel (max/mean difference, number
/// of differing pixels, where the biggest difference is) and optionally writes an amplified difference picture.
fn imgdiff(args: &[String]) {
    let (a, b) = (image::open(&args[0]).unwrap().to_rgba8(), image::open(&args[1]).unwrap().to_rgba8());
    if a.dimensions() != b.dimensions() {
        println!("different sizes: {:?} and {:?}", a.dimensions(), b.dimensions());
        return;
    }
    let (w, h) = a.dimensions();
    let mut out = image::RgbImage::new(w, h);
    let (mut max, mut sum, mut cnt, mut big, mut at) = (0i32, 0f64, 0u64, 0u64, (0u32, 0u32));
    let mut hist = [0u64; 5];
    for (x, y, pa) in a.enumerate_pixels() {
        let pb = b.get_pixel(x, y);
        let d = (0..4).map(|k| (pa[k] as i32 - pb[k] as i32).abs()).max().unwrap();
        if d > max {
            max = d;
            at = (x, y);
        }
        sum += d as f64;
        if d > 0 {
            cnt += 1;
        }
        if d > 8 {
            big += 1;
        }
        hist[match d {
            0 => 0,
            1 => 1,
            2..=4 => 2,
            5..=16 => 3,
            _ => 4,
        }] += 1;
        let v = (d * 16).min(255) as u8;
        out.put_pixel(x, y, image::Rgb([v, v, v]));
    }
    let n = (w as u64 * h as u64) as f64;
    println!("{w}x{h}: max {max} at {at:?}, mean {:.4}, differing {cnt} ({:.2} %), over 8: {big} ({:.3} %)", sum / n, cnt as f64 * 100.0 / n, big as f64 * 100.0 / n);
    println!("difference 0 / 1 / 2-4 / 5-16 / over 16: {hist:?}");
    if let Some(p) = args.get(2) {
        out.save(p).unwrap();
    }
    // 4th argument: a sheet of the 3 places with the most big differences (picture A | picture B | difference x16), 2x zoom
    if let Some(p) = args.get(3) {
        const CELL: u32 = 128;
        let (cw, ch) = (w.div_ceil(CELL), h.div_ceil(CELL));
        let mut counts = vec![0u32; (cw * ch) as usize];
        for (x, y, v) in out.enumerate_pixels() {
            if v[0] > 16 * 2 {
                counts[((y / CELL) * cw + x / CELL) as usize] += 1;
            }
        }
        let mut cells: Vec<usize> = (0..counts.len()).filter(|&i| counts[i] > 0).collect();
        cells.sort_by_key(|&i| std::cmp::Reverse(counts[i]));
        cells.truncate(3);
        let zoom = 2u32;
        let side = CELL * zoom;
        let mut sheet = image::RgbaImage::new(side * 3 + 8, (side + 4) * cells.len().max(1) as u32);
        for (row, &c) in cells.iter().enumerate() {
            let (x0, y0) = ((c as u32 % cw) * CELL, (c as u32 / cw) * CELL);
            for dy in 0..side {
                for dx in 0..side {
                    let (x, y) = (x0 + dx / zoom, y0 + dy / zoom);
                    if x >= w || y >= h {
                        continue;
                    }
                    let v = out.get_pixel(x, y)[0];
                    let oy = row as u32 * (side + 4) + dy;
                    sheet.put_pixel(dx, oy, *a.get_pixel(x, y));
                    sheet.put_pixel(side + 4 + dx, oy, *b.get_pixel(x, y));
                    sheet.put_pixel(2 * side + 8 + dx, oy, image::Rgba([v, v, v, 255]));
                }
            }
            println!("sheet row {row}: cell at x {x0}, y {y0}, {} pixels over 2 steps", counts[c]);
        }
        sheet.save(p).unwrap();
    }
}

/// Graphics setup. On Windows only DirectX 12 is started (unless `WGPU_BACKEND` says otherwise): with the default
/// choice (Vulkan, DX12 and OpenGL side by side) about every fourth run of v0.1.0 died with a wgpu panic
/// ("Failed to create staging buffer") on the same adapter; with one fixed backend 0 of 24 test runs did.
fn wgpu_configuration() -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew};
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    if cfg!(windows) && eframe::wgpu::Backends::from_env().is_none() {
        setup.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
    }
    WgpuConfiguration { wgpu_setup: WgpuSetup::CreateNew(setup), ..Default::default() }
}

fn main() -> eframe::Result {
    // the window program (no console in release builds) tells the person about a crash; headless runs just log it
    crashlog::install(!std::env::args().any(|a| a == "--selftest" || a == "--loadtest" || a == "--imgdiff" || a == "--kpdebug" || a == "--pairtest" || a == "--paircheck"));
    {
        let raw: Vec<String> = std::env::args().skip(1).collect();
        if raw.first().map(|s| s.as_str()) == Some("--imgdiff") && raw.len() >= 3 {
            imgdiff(&raw[1..]);
            return Ok(());
        }
    }
    let (args, selftest) = parse_args();
    if selftest && args.demo.as_deref() == Some("loadtest") {
        loadtest(&args.files);
        return Ok(());
    }
    if selftest && args.demo.as_deref() == Some("kpdebug") {
        kpdebug(&args.files);
        return Ok(());
    }
    if selftest && args.demo.as_deref() == Some("paircheck") {
        paircheck_test(&args.files);
        return Ok(());
    }
    if selftest && args.demo.as_deref() == Some("pairtest") {
        pairtest(&args.files);
        return Ok(());
    }
    if selftest {
        selftest_run(args.files);
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("PanoWeave – panorama studio")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([960.0, 600.0]),
        wgpu_options: wgpu_configuration(),
        ..Default::default()
    };
    eframe::run_native("PanoWeave", options, Box::new(move |cc| {
        if let Some(rs) = cc.wgpu_render_state.as_ref() {
            let i = rs.adapter.get_info();
            crashlog::log_gpu(&format!("{} | backend {:?} | type {:?} | driver {} {}", i.name, i.backend, i.device_type, i.driver, i.driver_info));
        }
        Ok(Box::new(app::PanoApp::new(cc, args)))
    }))
}

use eframe::egui;

/// Headless check of the whole pipeline on the sample photos.
/// Load many photos like the app does and report time and memory, then find points.
fn loadtest(files: &[PathBuf]) {
    use rayon::prelude::*;
    let t = std::time::Instant::now();
    let photos: Vec<engine::LoadedPhoto> = engine::load_pool().install(|| {
        files.par_iter().filter_map(|f| engine::load(engine::Source::File(f.clone()), f.file_name().unwrap().to_string_lossy().to_string()).ok()).collect()
    });
    let m = sysmem::process().unwrap_or((0, 0));
    println!("loaded {} photos in {:?}; memory now {:.2} GB, peak {:.2} GB; free {:.2} GB", photos.len(), t.elapsed(), sysmem::gb(m.0), sysmem::gb(m.1), sysmem::gb(sysmem::available().unwrap_or(0)));
    let grays: Vec<_> = photos.iter().map(|p| p.gray.clone()).collect();
    let works: Vec<_> = photos.iter().map(|p| p.work.clone()).collect();
    if std::env::var("PW_CANCEL_TEST").is_ok() {
        // press "Cancel" after 3 s and see how fast the work stops
        let t = std::time::Instant::now();
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(3));
            engine::CANCEL.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let _ = engine::find_points(&works, &grays, &[], engine::MatchSettings::default(), &|_, _| {});
        println!("cancel test: point finding stopped after {:?}", t.elapsed());
        engine::CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
        let lens_of = vec![0usize; photos.len()];
        let long: Vec<f64> = photos.iter().map(|p| p.w.max(p.h) as f64).collect();
        let m = engine::find_points(&works, &grays, &[], engine::MatchSettings::default(), &|_, _| {});
        let t = std::time::Instant::now();
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(2));
            engine::CANCEL.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let r = engine::align_all(&lens_of, &[align::Lens::from_hfov_deg(50.0)], &long, &m.cps, &engine::AlignSettings::default(), &|_, _| {});
        let fa: Vec<fine_align::FaPhoto> = photos.iter().map(|p| fine_align::FaPhoto { gray: p.gray.clone(), rgb: p.work.clone() }).collect();
        let _ = fine_align::run(&fa, &lens_of, &long, &r.lenses, &r.rots, &r.cps, true, true, &fine_align::FineConfig::default(), &|_, _| {});
        println!("cancel test: alignment + fine alignment stopped after {:?}", t.elapsed());
        return;
    }
    let t = std::time::Instant::now();
    let r = engine::find_points(&works, &grays, &[], engine::MatchSettings::default(), &|_, _| {});
    let m = sysmem::process().unwrap_or((0, 0));
    println!("points: {} cps, {} pairs in {:?}; memory now {:.2} GB, peak {:.2} GB", r.cps.len(), r.pairs.len(), t.elapsed(), sysmem::gb(m.0), sysmem::gb(m.1));
}

/// Draw detected points on each photo (shots/kp_<name>.png) and print response statistics.
fn kpdebug(files: &[PathBuf]) {
    for f in files {
        let p = engine::load(engine::Source::File(f.clone()), f.file_name().unwrap().to_string_lossy().to_string()).unwrap();
        let mut feats = features::detect(&p.gray, 3500);
        let mask = features::sky_mask(&p.work, &p.gray);
        features::remove_masked(&mut feats, &mask);
        let mut sc: Vec<f32> = feats.kps.iter().map(|k| k.score).collect();
        sc.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |x: f64| sc.get(((sc.len() as f64 - 1.0) * x) as usize).copied().unwrap_or(0.0);
        println!("{}: {} points, response p5 {:.2e} p25 {:.2e} p50 {:.2e} p90 {:.2e}", p.name, sc.len(), q(0.05), q(0.25), q(0.5), q(0.9));
        let mut img = (*p.work).clone();
        for k in &feats.kps {
            let c = if k.score < 1.5e-3 { image::Rgb([255, 0, 0]) } else if k.score < 3e-3 { image::Rgb([255, 255, 0]) } else { image::Rgb([0, 255, 0]) };
            for d in -3i32..=3 {
                for (x, y) in [(k.x as i32 + d, k.y as i32), (k.x as i32, k.y as i32 + d)] {
                    if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
                        img.put_pixel(x as u32, y as u32, c);
                    }
                }
            }
        }
        let _ = img.save(format!("shots/kp_{}.png", p.name));
    }
}

fn pairtest(files: &[PathBuf]) {
    use std::sync::Arc;
    let photos: Vec<engine::LoadedPhoto> = files
        .iter()
        .map(|f| engine::load(engine::Source::File(f.clone()), f.file_name().unwrap().to_string_lossy().to_string()).unwrap())
        .collect();
    let (pa, pb) = (&photos[0], &photos[1]);
    let fov = |p: &engine::LoadedPhoto| engine::exif_hfov(&p.info).unwrap_or(50.0);
    let f = |p: &engine::LoadedPhoto| align::Lens::from_hfov_deg(fov(p)).f * p.work.width().max(p.work.height()) as f64;
    let prior = f(pb) / f(pa);
    println!("{} -> {}: zoom ratio {:.2}", pa.name, pb.name, prior);
    let t = std::time::Instant::now();
    let auto = guided::auto_pair(&pa.gray, &pb.gray, Some(&pa.work), Some(&pb.work), prior, engine::MatchSettings::default().points_per_pair);
    println!("automatic: {} points in {:?}", auto.len(), t.elapsed());
    let model = |v: &Vec<(guided::P2, guided::P2, f32)>| {
        let (s, d): (Vec<_>, Vec<_>) = v.iter().map(|x| (x.0, x.1)).unzip();
        let h = geometry::fit_homography(&s, &d).unwrap();
        let r: Vec<f64> = v.iter().map(|x| { let p = geometry::h_apply(&h, x.0 .0, x.0 .1); (p.0 - x.1 .0).hypot(p.1 - x.1 .1) }).collect();
        (r.iter().sum::<f64>() / r.len() as f64, r.iter().cloned().fold(0.0, f64::max))
    };
    if auto.len() >= 8 {
        let (m, x) = model(&auto);
        println!("  consistency: mean {:.2} px, max {:.2} px (work px of second photo)", m, x);
        // simulate a hand-placed point, clicked 3 px off, then snapped and grown
        let s0 = auto[auto.len() / 2];
        let c = guided::Corr { fixed: &pa.gray, fixed_soft: None, moving: &pb.gray, moving_soft: None, tpl_r: guided::TPL_R };
        let soft = imgutil::blur(&pb.gray, guided::soft_sigma(prior));
        let c = guided::Corr { moving_soft: if prior > 1.5 { Some(&soft) } else { None }, ..c };
        let snapped = guided::refine(&c, s0.0, [prior, 0.0, 0.0, prior], (s0.1 .0 + 3.0, s0.1 .1 - 2.0), (guided::window_radius(guided::placing_window(pb.w as f64, guided::SEARCH_AREA_PERCENT)) * pb.work.width() as f64 / pb.w as f64).max(2.0));
        println!("  snap of a click 3.6 px off: {:?} (true {:?})", snapped, s0.1);
        for n in [1usize, 2, 3] {
            let seeds: Vec<_> = auto.iter().step_by(auto.len() / n).take(n).map(|x| (x.0, x.1)).collect();
            let t = std::time::Instant::now();
            let g = guided::grow(&pa.gray, &pb.gray, Some(&pa.work), Some(&pb.work), &seeds, prior, engine::MatchSettings::default().points_per_pair);
            let (m, x) = if g.len() >= 8 { model(&g) } else { (0.0, 0.0) };
            println!("  grown from {n} hand point(s): {} points in {:?}, consistency mean {:.2} max {:.2}", g.len(), t.elapsed(), m, x);
        }
    }
    let _ = Arc::new(0);
}

/// Render options for the self-test: PW_SHARP=0 gives the earlier behaviour.
fn render_opts() -> stitch::RenderOptions {
    if std::env::var("PW_SHARP").map(|v| v == "0").unwrap_or(false) {
        stitch::RenderOptions::default()
    } else {
        stitch::RenderOptions { levels: 4, cubic: true, prefer_detail: true, seams: seams::SeamConfig::for_export(false) }
    }
}

/// Sharpness (variance of the Laplacian) and mean seam difference are measured on the final image.
fn sharpness(col: &[[f32; 3]], cov: &[f32], w: usize, h: usize) -> f64 {
    let g = |x: usize, y: usize| { let c = col[y * w + x]; (0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]) as f64 };
    let mut v = Vec::new();
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            if cov[y * w + x] > 0.0 && cov[y * w + x - 1] > 0.0 && cov[y * w + x + 1] > 0.0 && cov[(y - 1) * w + x] > 0.0 && cov[(y + 1) * w + x] > 0.0 {
                v.push(4.0 * g(x, y) - g(x - 1, y) - g(x + 1, y) - g(x, y - 1) - g(x, y + 1));
            }
        }
    }
    let m = v.iter().sum::<f64>() / v.len().max(1) as f64;
    v.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / v.len().max(1) as f64
}

fn selftest_run(files: Vec<PathBuf>) {
    use std::sync::Arc;
    let t = std::time::Instant::now();
    let shots = sample::panorama_set();
    println!("sample render {:?}", t.elapsed());
    let photos: Vec<engine::LoadedPhoto> = if files.is_empty() {
        shots.into_iter().map(|s| engine::load(engine::Source::Memory(Arc::new(s.img)), s.name).unwrap()).collect()
    } else {
        use rayon::prelude::*;
        files
            .par_iter()
            .map(|f| engine::load(engine::Source::File(f.clone()), f.file_name().unwrap().to_string_lossy().to_string()).unwrap())
            .collect()
    };
    println!("loaded {} photos {:?}", photos.len(), t.elapsed());
    let grays: Vec<_> = photos.iter().map(|p| p.gray.clone()).collect();
    let t = std::time::Instant::now();
    let works: Vec<_> = photos.iter().map(|p| p.work.clone()).collect();
    for p in &photos {
        let m = features::sky_mask(&p.work, &p.gray);
        println!("  sky mask {}: {:.0}% of photo", p.name, m.fraction() * 100.0);
        if p.name == "sample_03.jpg" || !files.is_empty() {
            let mut im = (*p.work).clone();
            for (x, y, px) in im.enumerate_pixels_mut() {
                if m.contains(x as f32, y as f32) {
                    px[0] = (px[0] as u16 / 2 + 127) as u8;
                    px[1] /= 2;
                    px[2] /= 2;
                }
            }
            let _ = im.save(format!("shots/mask_{}.png", p.name));
        }
    }
    let mut ms = engine::MatchSettings::default();
    ms.ignore_sky = std::env::var("PW_SKY").map(|v| v != "0").unwrap_or(true);
    let m = engine::find_points(&works, &grays, &[], ms, &|_, _| {});
    let masks: Vec<_> = photos.iter().map(|p| features::sky_mask(&p.work, &p.gray)).collect();
    let on_sky = m
        .cps
        .iter()
        .filter(|c| {
            let (w, h) = (photos[c.a].work.width() as usize, photos[c.a].work.height() as usize);
            let (x, y) = engine::denorm_coord(c.pa, w, h);
            masks[c.a].contains(x, y)
        })
        .count();
    println!("control points on sky/clouds: {on_sky} of {}", m.cps.len());
    println!("sky/cloud keypoints removed: {}", m.sky_removed);
    println!("points: {} keypoints, {} cps, {} pairs, detect {} ms, match {} ms, total {:?}", m.keypoints, m.cps.len(), m.pairs.len(), m.detect_ms, m.match_ms, t.elapsed());
    for p in &m.pairs {
        println!("  pair {}-{}: {}", p.a, p.b, p.points);
    }
    // same lens grouping as the app: by focal length and photo size
    let mut lens_keys: Vec<(Option<u64>, u32)> = Vec::new();
    let mut lenses = Vec::new();
    let lens_of: Vec<usize> = photos
        .iter()
        .map(|p| {
            let key = (p.info.focal_35mm.map(|f| f as u64), p.w.max(p.h));
            if let Some(i) = lens_keys.iter().position(|k| *k == key) {
                return i;
            }
            lens_keys.push(key);
            lenses.push(match engine::exif_hfov(&p.info) {
                Some(h) => align::Lens { from_exif: true, ..align::Lens::from_hfov_deg(h) },
                None => align::Lens::from_hfov_deg(50.0),
            });
            lenses.len() - 1
        })
        .collect();
    let long: Vec<f64> = photos.iter().map(|p| p.w.max(p.h) as f64).collect();
    let mut r = engine::align_all(&lens_of, &lenses, &long, &m.cps, &engine::AlignSettings::default(), &|_, _| {});
    // fine alignment (patch matching), switchable with PW_FINE=0
    if std::env::var("PW_FINE").map(|v| v != "0").unwrap_or(true) {
        let fa: Vec<fine_align::FaPhoto> = photos.iter().map(|p| fine_align::FaPhoto { gray: p.gray.clone(), rgb: p.work.clone() }).collect();
        let cfg = fine_align::FineConfig::default();
        let (rots, lenses2, cps2, st) = fine_align::run(&fa, &lens_of, &long, &r.lenses, &r.rots, &r.cps, true, true, &cfg, &|_, _| {});
        println!(
            "fine: {} ms, {} rounds, patches {} valid of {}, areas {}, shift {:.3} -> {:.3} px, diff {:.2} -> {:.2}, err mean/med/p95 {:.2}/{:.2}/{:.2} -> {:.2}/{:.2}/{:.2}{}",
            st.ms, st.rounds, st.patches_valid, st.patches_total, st.areas, st.shift_before, st.shift_after, st.diff_before, st.diff_after,
            st.err_before[0], st.err_before[1], st.err_before[2], st.err_after[0], st.err_after[1], st.err_after[2],
            if st.rolled_back { " (last round undone)" } else { "" }
        );
        for l in &st.log {
            println!("  {l}");
        }
        for (k, v) in &st.reasons {
            println!("  reason {k}: {v}");
        }
        // patch comparison list: must match the statistics of the last measuring round
        {
            let info = patch_store::STORE.info();
            let (_, _, states) = patch_store::STORE.snapshot();
            let limit = states.iter().filter(|s| s.reason == patch_store::REASON_AREA_LIMIT).count();
            let used = states.iter().filter(|s| s.cp.is_some() && s.status == patch_store::Status::Valid && s.kind_area()).count();
            println!(
                "patch list: {} patches ({} valid + {} valid but over the area limit, {} rejected), {} areas used, data {:.1} MB, process {:.0} MB",
                info.done, info.valid, limit, info.rejected - limit, used, info.bytes as f64 / 1048576.0, sysmem::process().map(|p| p.0).unwrap_or(0) as f64 / 1048576.0
            );
            println!("fine stats of the same round: {} measured, {} valid, {} areas", st.list_total, st.list_valid, st.list_areas);
            println!("check: {}", if info.done == st.list_total && info.valid + limit == st.list_valid && used == st.list_areas { "list matches the optimisation statistics" } else { "MISMATCH" });
            drop(states);
            // restart with changes: 5 patches disabled, one control area shifted by hand
            {
                use patch_store::{Change, Status, STORE};
                let (_, d, s) = STORE.snapshot();
                let dis: Vec<u32> = d.iter().zip(&s).filter(|(_, s)| s.status == Status::Valid).take(5).map(|(d, _)| d.id).collect();
                STORE.edit(&dis, |_, st| {
                    st.prev_status = st.status;
                    st.prev_reason = st.reason;
                    st.status = Status::DisabledManual;
                    st.changed = Some(Change::Disabled);
                });
                let mv: Vec<u32> = d.iter().zip(&s).filter(|(d, s)| s.status == Status::Valid && d.kind == align::CpKind::Area && !dis.contains(&d.id)).take(1).map(|(d, _)| d.id).collect();
                STORE.edit(&mv, |d, st| {
                    st.manual = true;
                    st.dx += 1.0;
                    st.cp = fine_align::manual_cp(d, st, st.dx, st.dy, 1.0);
                    st.changed = Some(Change::Moved);
                });
                let keys: Vec<patch_store::Key> = d.iter().filter(|d| dis.contains(&d.id) || mv.contains(&d.id)).map(|d| d.key).collect();
                let t = std::time::Instant::now();
                let (_, _, cps3, st3) = fine_align::reoptimize(&fa, &lens_of, &long, &lenses2, &rots, &cps2, true, true, &cfg, &|_, _| {});
                let (_, d2, s2) = STORE.snapshot();
                let n_dis = d2.iter().zip(&s2).filter(|(d, s)| keys.contains(&d.key) && s.status == Status::DisabledManual).count();
                let n_man = d2.iter().zip(&s2).filter(|(d, s)| keys.contains(&d.key) && s.manual).count();
                println!(
                    "restart: {} ms, {} pairs used, err {:.2} -> {:.2}, new list {} patches, disabled kept {n_dis}/{}, shifted kept {n_man}/{}",
                    t.elapsed().as_millis(), cps3.len(), st3.err_before[0], st3.err_after[0], d2.len(), dis.len(), mv.len()
                );
            }
            patch_store::STORE.free_after_export();
            println!("after export: patch data {:.1} MB, process {:.0} MB", patch_store::STORE.info().bytes as f64 / 1048576.0, sysmem::process().map(|p| p.0).unwrap_or(0) as f64 / 1048576.0);
        }
        r.rots = rots;
        r.lenses = lenses2;
        r.cps = cps2;
        if std::env::var("PW_PARALLAX").map(|v| v == "1").unwrap_or(false) {
            let pc = fine_align::ParallaxConfig { enabled: true, ..Default::default() };
            r.fields = fine_align::parallax_fields(&fa, &lens_of, &r.lenses, &r.rots, &r.cps, &cfg, &pc);
            println!("parallax fields: {} photos", r.fields.iter().filter(|f| f.is_some()).count());
        }
    }
    let fields = r.fields.clone();
    println!(
        "align: {} ms, mean {:.2}px max {:.2}px removed {}, hfov {:.2} (true 62) k1 {:.4} (true -0.04)",
        r.ms, r.mean_err, r.max_err, r.removed, r.lenses[0].hfov_deg(), r.lenses[0].k1
    );
    for (i, rot) in r.rots.iter().enumerate() {
        let (y, p, ro) = geometry::ypr_of(rot);
        println!("  photo {i}: yaw {:.2} pitch {:.2} roll {:.2}", y.to_degrees(), p.to_degrees(), ro.to_degrees());
    }
    let mut rp: Vec<stitch::RenderPhoto> = photos
        .iter()
        .enumerate()
        .map(|(i, p)| stitch::RenderPhoto {
            img: p.work.clone(),
            w: p.work.width() as f64,
            h: p.work.height() as f64,
            rot: r.rots[i],
            lens: r.lenses[lens_of[i]],
            gain: [1.0; 3],
            gain_map: None,
            field: fields.get(i).cloned().flatten(),
            full_long: p.w.max(p.h) as f64,
            trusted: true,
            mask: None,
        })
        .collect();
    // seam cost: mean absolute grey difference of the two photos around every control pair,
    // sampled through the same lookup the renderer uses (includes parallax fields)
    {
        let mut sum = 0.0f64;
        let mut cnt = 0usize;
        for c in r.cps.iter().filter(|c| c.kind == align::CpKind::Point) {
            let g = |i: usize| fine_align::Geo { lens: r.lenses[lens_of[i]], rot: r.rots[i], w: photos[i].work.width() as f64, h: photos[i].work.height() as f64 };
            let ga = g(c.a);
            let l = ga.long();
            let pa = (c.pa[0] * l + ga.w * 0.5 - 0.5, c.pa[1] * l + ga.h * 0.5 - 0.5);
            let plane = fine_align::Plane::new(ga.px_to_dir(pa), &ga.rot, ga.pixel_angle());
            for y in -12..=12 {
                for x in -12..=12 {
                    let d = plane.dir(x as f64, y as f64);
                    if let (Some((xa, ya, _)), Some((xb, yb, _))) = (rp[c.a].lookup(d), rp[c.b].lookup(d)) {
                        let va = imgutil::sample_rgb_linear(&rp[c.a].img, xa, ya);
                        let vb = imgutil::sample_rgb_linear(&rp[c.b].img, xb, yb);
                        let ga_ = imgutil::linear_to_srgb(0.3 * va[0] + 0.59 * va[1] + 0.11 * va[2]);
                        let gb_ = imgutil::linear_to_srgb(0.3 * vb[0] + 0.59 * vb[1] + 0.11 * vb[2]);
                        sum += ((ga_ - gb_).abs() * 255.0) as f64;
                        cnt += 1;
                    }
                }
            }
        }
        println!("seam cost (mean |A-B| around control pairs, 0..255): {:.2}", sum / cnt.max(1) as f64);
    }
    let mut view = stitch::PanoView::default();
    stitch::fit_bounds(&mut view, &rp);
    let gains = stitch::compute_gains(&rp, &view);
    println!("gains {:?}", gains);
    for (p, g) in rp.iter_mut().zip(&gains) {
        p.gain = *g;
    }
    view.crop = stitch::auto_crop(&rp, &view);
    println!("bounds {:?} crop {:?} fov {:?}", view.bounds, view.crop, view.fov_deg());
    let t = std::time::Instant::now();
    let w = 1200;
    let h = (w as f64 / view.aspect()) as usize;
    let px = stitch::render_preview(&rp, &view, w, h, true, true);
    println!("preview {:?}", t.elapsed());
    image::save_buffer("shots/selftest_preview.png", &px, w as u32, h as u32, image::ColorType::Rgba8).unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let prog = stitch::Progress { report: &|_, _| {}, cancel: &cancel };
    let t = std::time::Instant::now();
    let loader = |i: usize, _px: bool| Ok(rp[i].clone());
    let (col, cov, w, h) = stitch::render_final(rp.len(), &loader, &view, 2400, stitch::Blend::Smooth, &render_opts(), &prog).unwrap();
    println!("final smooth {}x{} {:?}, sharpness (Laplacian variance x1e6) {:.2}", w, h, t.elapsed(), sharpness(&col, &cov, w, h) * 1e6);
    let bytes = stitch::to_rgba8(&col, &cov);
    image::save_buffer("shots/selftest_final.png", &bytes, w as u32, h as u32, image::ColorType::Rgba8).unwrap();
    let br = sample::bracket_set();
    let imgs: Vec<_> = br.into_iter().map(|s| s.img).collect();
    let t = std::time::Instant::now();
    let al = fusion::align_stack(&imgs);
    let fused = fusion::fuse(&al, &fusion::FusionWeights::default());
    println!("fusion {:?}", t.elapsed());
    fused.save("shots/selftest_fused.png").unwrap();
    imgs[1].save("shots/selftest_bracket_normal.png").unwrap();
}

/// Acceptance test of the geometric pair check on real photos:
/// relations before/after, triangle test, a pair that can only be joined through a third photo,
/// and "switched off = exactly as before".
fn paircheck_test(files: &[PathBuf]) {
    use rayon::prelude::*;
    let photos: Vec<engine::LoadedPhoto> = files
        .par_iter()
        .map(|f| engine::load(engine::Source::File(f.clone()), f.file_name().unwrap().to_string_lossy().to_string()).unwrap())
        .collect();
    let grays: Vec<_> = photos.iter().map(|p| p.gray.clone()).collect();
    let works: Vec<_> = photos.iter().map(|p| p.work.clone()).collect();
    let mut ms = engine::MatchSettings::default();
    if let Some(v) = std::env::var("PW_PPP").ok().and_then(|v| v.parse().ok()) {
        ms.points_per_photo = v;
    }
    let m = engine::find_points(&works, &grays, &[], ms, &|_, _| {});
    let mut lens_keys: Vec<(Option<u64>, u32)> = Vec::new();
    let mut lenses = Vec::new();
    let lens_of: Vec<usize> = photos
        .iter()
        .map(|p| {
            let key = (p.info.focal_35mm.map(|f| f as u64), p.w.max(p.h));
            if let Some(i) = lens_keys.iter().position(|k| *k == key) {
                return i;
            }
            lens_keys.push(key);
            lenses.push(match engine::exif_hfov(&p.info) {
                Some(h) => align::Lens { from_exif: true, ..align::Lens::from_hfov_deg(h) },
                None => align::Lens::from_hfov_deg(50.0),
            });
            lenses.len() - 1
        })
        .collect();
    let long: Vec<f64> = photos.iter().map(|p| p.w.max(p.h) as f64).collect();
    let fa: Vec<fine_align::FaPhoto> = photos.iter().map(|p| fine_align::FaPhoto { gray: p.gray.clone(), rgb: p.work.clone() }).collect();
    let used = vec![true; photos.len()];
    let s = engine::AlignSettings::default();
    let matcher = fine_align::FineConfig::default().matcher;
    let r0 = engine::align_all(&lens_of, &lenses, &long, &m.cps, &s, &|_, _| {});
    // switched off: the alignment is exactly the one without the check
    let r_off = engine::align_all(&lens_of, &lenses, &long, &m.cps, &s, &|_, _| {});
    let same = r0.cps.len() == r_off.cps.len() && r0.rots.iter().zip(&r_off.rots).all(|(a, b)| a == b);
    println!("switched off: {}", if same { "identical to before" } else { "DIFFERENT" });
    let cfg = pair_check::PairCheckConfig::default();
    let pairs_before = pair_check::pair_counts(&r0.cps);
    let t = std::time::Instant::now();
    let (r1, sum) = pair_check::run_rounds(&fa, &lens_of, &lenses, &long, r0, &s, &used, &cfg, &matcher, ms, &|_, _| {});
    println!(
        "real data ({} photos): pairs with points {} -> {}, overlapping pairs {}, searched {}, {} rounds, {:?}, error {:.2} px",
        photos.len(),
        pairs_before.len(),
        sum.pairs_after,
        sum.overlaps.len(),
        sum.reports.len(),
        sum.rounds,
        t.elapsed(),
        r1.mean_err
    );
    for l in &sum.log {
        println!("  {l}");
    }
    let counts = pair_check::pair_counts(&r1.cps);
    // triangle: three photos that overlap each other by 30 % or more
    let big: Vec<&pair_check::PairOverlap> = sum.overlaps.iter().filter(|o| o.share >= pair_check::OBVIOUS_OVERLAP).collect();
    let has = |a: usize, b: usize| big.iter().any(|o| (o.a, o.b) == (a.min(b), a.max(b)));
    let mut tri = None;
    'outer: for o in &big {
        for c in 0..photos.len() {
            if c != o.a && c != o.b && has(o.a, c) && has(o.b, c) {
                tri = Some((o.a, o.b, c));
                break 'outer;
            }
        }
    }
    let Some((a, b, c)) = tri else {
        println!("no triangle with 30 % overlap in this data set");
        return;
    };
    // remove every point between A and B, then check (as if the finder had missed the pair)
    let gap: Vec<align::ControlPoint> = r1.cps.iter().filter(|x| (x.a.min(x.b), x.a.max(x.b)) != (a.min(b), a.max(b))).copied().collect();
    let r2 = engine::align_all(&lens_of, &lenses, &long, &gap, &s, &|_, _| {});
    let (r3, sum3) = pair_check::run_rounds(&fa, &lens_of, &lenses, &long, r2, &s, &used, &cfg, &matcher, ms, &|_, _| {});
    let n_ab = pair_check::pair_counts(&r3.cps).get(&(a.min(b), a.max(b))).copied().unwrap_or(0);
    let rep = sum3.reports.iter().find(|x| (x.a, x.b) == (a.min(b), a.max(b)));
    println!(
        "triangle {}-{}-{} (overlaps {:.0} %/{:.0} %/{:.0} %): {}-{} had {} points, removed; after the check {} points ({}) -> {}",
        a + 1,
        b + 1,
        c + 1,
        overlap_of(&sum.overlaps, a, b) * 100.0,
        overlap_of(&sum.overlaps, a, c) * 100.0,
        overlap_of(&sum.overlaps, b, c) * 100.0,
        a + 1,
        b + 1,
        counts.get(&(a.min(b), a.max(b))).copied().unwrap_or(0),
        n_ab,
        rep.and_then(|r| r.success).unwrap_or("-"),
        if n_ab >= cfg.min_points { "PASS" } else { "FAIL" }
    );
    // only through a third photo: the plain finder step switched off
    let cfg_p = pair_check::PairCheckConfig { restrict_search: false, ..cfg.clone() };
    let r4 = engine::align_all(&lens_of, &lenses, &long, &gap, &s, &|_, _| {});
    let (r5, sum5) = pair_check::run_rounds(&fa, &lens_of, &lenses, &long, r4, &s, &used, &cfg_p, &matcher, ms, &|_, _| {});
    let n5 = pair_check::pair_counts(&r5.cps).get(&(a.min(b), a.max(b))).copied().unwrap_or(0);
    let rep5 = sum5.reports.iter().find(|x| (x.a, x.b) == (a.min(b), a.max(b)));
    println!(
        "through a third photo only: {}-{} -> {} points, solved by: {} -> {}",
        a + 1,
        b + 1,
        n5,
        rep5.and_then(|r| r.success).unwrap_or("-"),
        if n5 >= cfg.min_points && rep5.and_then(|r| r.success) == Some(pair_check::STEP_PREDICT) { "PASS" } else { "FAIL" }
    );
    if let Some(r) = rep5 {
        for st in &r.steps {
            println!("  {} -> {} ({})", st.step, st.points, st.note);
        }
    }
}

fn overlap_of(o: &[pair_check::PairOverlap], a: usize, b: usize) -> f32 {
    o.iter().find(|x| (x.a, x.b) == (a.min(b), a.max(b))).map(|x| x.share).unwrap_or(0.0)
}
