//! Project files (.pwproj, JSON): photos, lenses, points, alignment and framing.

use crate::align::{ControlPoint, Lens};
use crate::fine_align::{DisplacementField, ParallaxConfig};
use crate::geometry::M3;
use crate::masks::MaskStroke;
use crate::stitch::PanoView;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
pub struct ProjectPhoto {
    pub path: PathBuf,
    pub lens: usize,
    pub rot: M3,
    pub enabled: bool,
    /// local parallax correction of this photo (computed by the alignment), if there is one
    #[serde(default)]
    pub field: Option<DisplacementField>,
    /// what the person painted on the photo to steer the seams (strokes and polygons)
    #[serde(default)]
    pub masks: Vec<MaskStroke>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ProjectFile {
    pub version: u32,
    pub photos: Vec<ProjectPhoto>,
    pub lenses: Vec<Lens>,
    pub cps: Vec<ControlPoint>,
    pub view: PanoView,
    pub aligned: bool,
    /// settings of the parallax correction (old files: off)
    #[serde(default)]
    pub parallax: Option<ParallaxConfig>,
}

/// Newest project file version this program reads.
pub const VERSION: u32 = 1;

/// Saves the project. Values that only describe the last measurement (error, weight, confidence, score, shift of a
/// point) are written as 0 when they are not finite, because JSON cannot hold them and the file would not load again;
/// the number of such values is returned. A non-finite value in the geometry (point position, rotation, lens, view,
/// parallax field) is an error: nothing is written. The old file is only replaced once the new one is complete.
pub fn save(path: &std::path::Path, p: &ProjectFile) -> Result<usize, String> {
    let mut p = p.clone();
    let mut fixed = 0;
    for c in p.cps.iter_mut() {
        for v in [&mut c.err, &mut c.weight] {
            if !v.is_finite() {
                *v = 0.0;
                fixed += 1;
            }
        }
        let [s0, s1] = &mut c.shift;
        for v in [&mut c.conf, &mut c.score, s0, s1] {
            if !v.is_finite() {
                *v = 0.0;
                fixed += 1;
            }
        }
    }
    if let Some(e) = non_finite(&p) {
        return Err(format!("{e} has an invalid value (not a number) – the project was not saved"));
    }
    let s = serde_json::to_string_pretty(&p).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("pwproj.tmp");
    std::fs::write(&tmp, s).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(fixed)
}

/// The first value of the geometry that is not a finite number, named for the message.
fn non_finite(p: &ProjectFile) -> Option<String> {
    let bad = |v: &[f64]| v.iter().any(|x| !x.is_finite());
    for (i, ph) in p.photos.iter().enumerate() {
        if ph.rot.iter().any(|r| bad(r)) {
            return Some(format!("the rotation of photo {}", i + 1));
        }
        if ph.field.as_ref().is_some_and(|f| f.d.iter().flatten().any(|x| !x.is_finite())) {
            return Some(format!("the parallax field of photo {}", i + 1));
        }
    }
    if let Some(i) = p.lenses.iter().position(|l| !l.f.is_finite() || !l.k1.is_finite()) {
        return Some(format!("lens {}", i + 1));
    }
    if let Some(k) = p.cps.iter().position(|c| bad(&c.pa) || bad(&c.pb)) {
        return Some(format!("point {}", k + 1));
    }
    let v = &p.view;
    if bad(&[v.yaw, v.pitch, v.roll]) || bad(&v.bounds) || bad(&v.crop) {
        return Some("the view".into());
    }
    None
}

/// Checks that the numbers in a loaded project refer to things that exist, so a damaged or hand-edited file is
/// reported when it is opened instead of crashing later in the alignment or the export.
pub fn check(p: &ProjectFile) -> Result<(), String> {
    if p.version > VERSION {
        return Err(format!("the file was made by a newer PanoWeave (project version {}, this one reads up to {VERSION})", p.version));
    }
    if let Some(i) = p.photos.iter().position(|ph| ph.lens >= p.lenses.len()) {
        return Err(format!("photo {} refers to lens {}, but the file has {} lenses", i + 1, p.photos[i].lens + 1, p.lenses.len()));
    }
    let n = p.photos.len();
    if let Some(k) = p.cps.iter().position(|c| c.a >= n || c.b >= n) {
        return Err(format!("point {} refers to photo {} or {}, but the file has {n} photos", k + 1, p.cps[k].a + 1, p.cps[k].b + 1));
    }
    Ok(())
}

pub fn load(path: &std::path::Path) -> Result<ProjectFile, String> {
    let s = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let p: ProjectFile = serde_json::from_str(&s).map_err(|e| e.to_string())?;
    check(&p)?;
    Ok(p)
}

/// Leaves out the photos with `found[i] == false` (they could not be loaded): their points go, the photo numbers
/// of the other points move up. Returns the reduced project and the number of points left out.
pub fn keep_photos(mut p: ProjectFile, found: &[bool]) -> (ProjectFile, usize) {
    let mut new_idx = vec![None; p.photos.len()];
    let mut next = 0;
    for (i, &f) in found.iter().enumerate().take(p.photos.len()) {
        if f {
            new_idx[i] = Some(next);
            next += 1;
        }
    }
    let photos = std::mem::take(&mut p.photos);
    p.photos = photos.into_iter().zip(found.iter().chain(std::iter::repeat(&false))).filter(|(_, f)| **f).map(|(ph, _)| ph).collect();
    let before = p.cps.len();
    p.cps = p
        .cps
        .into_iter()
        .filter_map(|mut c| {
            c.a = new_idx.get(c.a).copied().flatten()?;
            c.b = new_idx.get(c.b).copied().flatten()?;
            Some(c)
        })
        .collect();
    let dropped = before - p.cps.len();
    (p, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::IDENTITY;

    fn sample() -> ProjectFile {
        ProjectFile {
            version: 1,
            photos: vec![
                ProjectPhoto { path: PathBuf::from("a.jpg"), lens: 0, rot: IDENTITY, enabled: true, field: None, masks: Vec::new() },
                ProjectPhoto { path: PathBuf::from("b.jpg"), lens: 0, rot: crate::geometry::rodrigues([0.0, 0.2, 0.0]), enabled: false, field: Some(DisplacementField { gw: 2, gh: 2, d: vec![[0.001, -0.002], [0.0, 0.0], [0.003, 0.004], [-0.005, 0.0]] }), masks: vec![MaskStroke { kind: crate::masks::MaskKind::Negative, points: vec![[0.2, 0.3], [0.4, 0.5]], radius: 0.02, polygon: false }] },
            ],
            lenses: vec![Lens::from_hfov_deg(60.0)],
            cps: vec![ControlPoint::new(0, 1, [0.1, 0.2], [-0.3, 0.05], true)],
            view: PanoView::default(),
            aligned: true,
            parallax: Some(ParallaxConfig { enabled: true, max_shift: 9.0, ..Default::default() }),
        }
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("panoweave_test_{}_{}", std::process::id(), name))
    }

    #[test]
    fn save_load_roundtrip() {
        let p = sample();
        let path = tmp("roundtrip.pwproj");
        save(&path, &p).unwrap();
        let q = load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(q.version, p.version);
        assert_eq!(q.photos.len(), 2);
        assert_eq!(q.photos[1].path, PathBuf::from("b.jpg"));
        assert!(!q.photos[1].enabled);
        // serde_json without `float_roundtrip` may differ in the last digit
        for (qr, pr) in q.photos[1].rot.iter().zip(&p.photos[1].rot) {
            for (a, b) in qr.iter().zip(pr) {
                assert!((a - b).abs() < 1e-12);
            }
        }
        assert_eq!(q.lenses, p.lenses);
        assert_eq!(q.cps.len(), 1);
        assert_eq!(q.cps[0].pb, [-0.3, 0.05]);
        assert!(q.cps[0].manual);
        assert!(q.aligned);
        // the parallax correction survives saving and loading
        assert!(q.photos[0].field.is_none());
        let f = q.photos[1].field.as_ref().unwrap();
        assert_eq!((f.gw, f.gh, f.d.len()), (2, 2, 4));
        assert!((f.d[2][1] - 0.004).abs() < 1e-7);
        // the masks too
        assert!(q.photos[0].masks.is_empty());
        assert_eq!(q.photos[1].masks, p.photos[1].masks);
        let pc = q.parallax.unwrap();
        assert!(pc.enabled && pc.max_shift == 9.0);
    }

    #[test]
    fn old_project_files_without_parallax_still_load() {
        // a file saved before the parallax keys existed: cut the new keys out of a saved one
        let mut v: serde_json::Value = serde_json::to_value(sample()).unwrap();
        v.as_object_mut().unwrap().remove("parallax");
        v["photos"][1].as_object_mut().unwrap().remove("field");
        v["photos"][1].as_object_mut().unwrap().remove("masks");
        let pf: ProjectFile = serde_json::from_value(v).unwrap();
        assert!(pf.parallax.is_none() && pf.photos[1].field.is_none() && pf.photos[1].masks.is_empty());
    }

    #[test]
    fn old_control_points_get_defaults() {
        // fields added later (weight, kind, conf, ...) must be optional in old files
        let js = r#"{"a":0,"b":1,"pa":[0.0,0.0],"pb":[0.1,0.1]}"#;
        let cp: ControlPoint = serde_json::from_str(js).unwrap();
        assert_eq!(cp.weight, 1.0);
        assert_eq!(cp.kind, crate::align::CpKind::Point);
        assert!(!cp.manual);
    }

    #[test]
    fn load_errors_are_reported() {
        assert!(load(&tmp("does_not_exist.pwproj")).is_err());
        let path = tmp("garbage.pwproj");
        std::fs::write(&path, "not json").unwrap();
        let r = load(&path);
        let _ = std::fs::remove_file(&path);
        assert!(r.is_err());
    }

    #[test]
    fn damaged_numbers_are_reported_when_opening() {
        let mut p = sample();
        p.photos[1].lens = 3;
        assert!(check(&p).unwrap_err().contains("photo 2 refers to lens 4"));
        let mut p = sample();
        p.cps.push(ControlPoint::new(0, 5, [0.0; 2], [0.0; 2], false));
        assert!(check(&p).unwrap_err().contains("point 2"));
        let mut p = sample();
        p.version = VERSION + 1;
        assert!(check(&p).unwrap_err().contains("newer PanoWeave"));
        // and through `load`
        let path = tmp("damaged.pwproj");
        let mut p = sample();
        p.photos[0].lens = 9;
        std::fs::write(&path, serde_json::to_string(&p).unwrap()).unwrap();
        let r = load(&path);
        let _ = std::fs::remove_file(&path);
        assert!(r.is_err());
        assert!(check(&sample()).is_ok());
    }

    #[test]
    fn invalid_values_never_make_an_unreadable_file() {
        let path = tmp("nan.pwproj");
        // measurement values: written as 0, the file loads again
        let mut p = sample();
        p.cps[0].err = f64::NAN;
        p.cps[0].conf = f32::INFINITY;
        assert_eq!(save(&path, &p).unwrap(), 2);
        let q = load(&path).unwrap();
        assert_eq!((q.cps[0].err, q.cps[0].conf), (0.0, 0.0));
        // geometry: refused, the good file stays as it was
        let mut bad = sample();
        bad.photos[1].rot[0][0] = f64::NAN;
        let e = save(&path, &bad).unwrap_err();
        assert!(e.contains("rotation of photo 2"), "{e}");
        assert!(load(&path).is_ok(), "the old file is untouched");
        assert!(!path.with_extension("pwproj.tmp").exists(), "no temporary file is left");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_photos_are_left_out_with_their_points() {
        let mut p = sample();
        p.photos.push(ProjectPhoto { path: PathBuf::from("c.jpg"), lens: 0, rot: IDENTITY, enabled: true, field: None, masks: Vec::new() });
        p.cps = vec![
            ControlPoint::new(0, 1, [0.1; 2], [0.2; 2], false),
            ControlPoint::new(1, 2, [0.3; 2], [0.4; 2], false),
            ControlPoint::new(0, 2, [0.5; 2], [0.6; 2], true),
        ];
        let (q, dropped) = keep_photos(p.clone(), &[true, false, true]);
        assert_eq!(dropped, 2);
        assert_eq!(q.photos.iter().map(|ph| ph.path.to_string_lossy().to_string()).collect::<Vec<_>>(), ["a.jpg", "c.jpg"]);
        assert_eq!((q.cps.len(), q.cps[0].a, q.cps[0].b, q.cps[0].pa, q.cps[0].manual), (1, 0, 1, [0.5; 2], true));
        assert!(check(&q).is_ok());
        let (all, none) = keep_photos(p.clone(), &[true, true, true]);
        assert_eq!((all.photos.len(), all.cps.len(), none), (3, 3, 0));
    }
}
