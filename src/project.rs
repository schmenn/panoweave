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

pub fn save(path: &std::path::Path, p: &ProjectFile) -> Result<(), String> {
    let s = serde_json::to_string_pretty(p).map_err(|e| e.to_string())?;
    std::fs::write(path, s).map_err(|e| e.to_string())
}

pub fn load(path: &std::path::Path) -> Result<ProjectFile, String> {
    let s = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&s).map_err(|e| e.to_string())
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
}
