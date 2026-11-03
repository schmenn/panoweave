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
