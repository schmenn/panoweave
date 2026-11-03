//! Project files (.pwproj, JSON): photos, lenses, points, alignment and framing.

use crate::align::{ControlPoint, Lens};
use crate::geometry::M3;
use crate::stitch::PanoView;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
pub struct ProjectPhoto {
    pub path: PathBuf,
    pub lens: usize,
    pub rot: M3,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ProjectFile {
    pub version: u32,
    pub photos: Vec<ProjectPhoto>,
    pub lenses: Vec<Lens>,
    pub cps: Vec<ControlPoint>,
    pub view: PanoView,
    pub aligned: bool,
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
                ProjectPhoto { path: PathBuf::from("a.jpg"), lens: 0, rot: IDENTITY, enabled: true },
                ProjectPhoto { path: PathBuf::from("b.jpg"), lens: 0, rot: crate::geometry::rodrigues([0.0, 0.2, 0.0]), enabled: false },
            ],
            lenses: vec![Lens::from_hfov_deg(60.0)],
            cps: vec![ControlPoint::new(0, 1, [0.1, 0.2], [-0.3, 0.05], true)],
            view: PanoView::default(),
            aligned: true,
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
