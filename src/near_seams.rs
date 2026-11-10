//! Near objects in the export: a car 3 m in front of the camera is seen from slightly different places in
//! neighbouring photos, so where two photos are blended it shows twice or blurred. It has to come from exactly one
//! photo, with the seam running round it.
//!
//! The seam finder is steered by masks (green = "use this photo here", red = "do not use it here"), so this module
//! only *proposes* masks: per near object (found by `depth.rs`) one photo that keeps it - the one that shows it whole
//! (no cell on the picture edge) and nearest to its middle - gets green, every other photo that sees it gets red, as
//! filled polygons round the object's cells. Nothing is applied until the person accepts (`app/masks_near.rs`); the
//! strokes carry `auto = true`, are shown and can be changed in the mask editor like any other, and one button removes
//! them all again.

use crate::depth::{CoarseMask, Layers, GRID};
use crate::masks::{MaskKind, MaskStroke};

/// Settings of the proposal.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct NearSeamConfig {
    /// off = nothing is proposed
    pub enabled: bool,
    /// an object smaller than this (cells of the 16 x 16 grid of its photo) is not worth a mask
    pub min_cells: usize,
    /// also paint the other photos red where they see the object (a red spot is left out completely, so a stray cell
    /// can let another photo's different content in: off, green on the keeper alone steers the seam)
    pub avoid_others: bool,
    /// an object that shifts less than this between the photos (working-image pixels at 1600 px, the largest of its
    /// regions) only blurs a little and is not worth a mask
    pub min_shift_px: f32,
    /// an object larger than this share of a photo is no single object (a whole hillside shifting a little): not proposed
    pub max_share: f32,
}

impl Default for NearSeamConfig {
    fn default() -> Self {
        NearSeamConfig { enabled: true, min_cells: 3, avoid_others: false, min_shift_px: 12.0, max_share: 0.25 }
    }
}

impl NearSeamConfig {
    /// Environment `PW_NEAR_SEAMS=0` switches the proposal off (experiments).
    pub fn with_env(mut self) -> Self {
        if std::env::var("PW_NEAR_SEAMS").is_ok_and(|v| v == "0") {
            self.enabled = false;
        }
        if std::env::var("PW_NEAR_RED").is_ok_and(|v| v == "1") {
            self.avoid_others = true;
        }
        self
    }
}

/// One near object: where it is in every photo that sees it, and the photo proposed to keep it.
#[derive(Clone, Debug, PartialEq)]
pub struct NearObject {
    /// photo and the cells the object covers in it
    pub photos: Vec<(usize, CoarseMask)>,
    /// the photo that keeps it, or None with `why_not` set
    pub keeper: Option<usize>,
    pub why_not: &'static str,
    /// the largest shift of its regions (working-image pixels at 1600 px)
    pub shift_px: f32,
}

impl NearObject {
    pub fn text(&self) -> String {
        let list = self.photos.iter().map(|(p, _)| (p + 1).to_string()).collect::<Vec<_>>().join(", ");
        match self.keeper {
            Some(k) => format!("Near object in photos {list}: keep it from photo {}.", k + 1),
            None => format!("Near object in photos {list}: not proposed ({}).", self.why_not),
        }
    }
}

struct Uf(Vec<usize>);

impl Uf {
    fn find(&mut self, mut x: usize) -> usize {
        while self.0[x] != x {
            self.0[x] = self.0[self.0[x]];
            x = self.0[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.0[b] = a;
        }
    }
}

/// Two regions that share a photo belong to the same object when their cells in that photo overlap by at least this
/// share of the larger one (a huge region must not swallow every small one that lies inside it).
const SAME_OBJECT_OVERLAP: f32 = 0.5;

/// Groups the regions of `layers` into objects and proposes a keeper for each. A region says that the same thing lies
/// on its cells in both photos of a pair; two regions that share a photo are the same object when their cells in that
/// photo overlap enough (a car seen in photos 8, 10 and 14 gives regions 8-10 and 8-14 that overlap in photo 8).
pub fn plan(layers: &Layers, n_photos: usize, cfg: &NearSeamConfig) -> Vec<NearObject> {
    if !cfg.enabled || layers.is_empty() {
        return Vec::new();
    }
    // a region that covers more of a photo than one object can is a wide shift (a hillside, a badly matched pair), not an
    // object: it would also tie unrelated objects together, so it is left out
    let limit = (cfg.max_share * (GRID * GRID) as f32) as usize;
    let regions: Vec<&crate::depth::NearRegion> = layers.regions.iter().filter(|r| r.a < n_photos && r.b < n_photos && r.mask_a.count() <= limit && r.mask_b.count() <= limit).collect();
    let mut uf = Uf((0..regions.len()).collect());
    for i in 0..regions.len() {
        for j in i + 1..regions.len() {
            let sides = |r: &crate::depth::NearRegion| [(r.a, r.mask_a), (r.b, r.mask_b)];
            let same = sides(regions[i]).iter().any(|(p, m)| sides(regions[j]).iter().any(|(q, n)| p == q && m.overlap(n) as f32 >= SAME_OBJECT_OVERLAP * m.count().max(n.count()) as f32));
            if same {
                uf.union(i, j);
            }
        }
    }
    // cells of every cluster in every photo
    let mut clusters: std::collections::BTreeMap<usize, (f32, Vec<(usize, CoarseMask)>)> = Default::default();
    for (i, r) in regions.iter().enumerate() {
        let (shift, v) = clusters.entry(uf.find(i)).or_default();
        *shift = shift.max(r.shift_px);
        for (p, m) in [(r.a, r.mask_a), (r.b, r.mask_b)] {
            match v.iter_mut().find(|(q, _)| *q == p) {
                Some((_, u)) => u.merge(&m),
                None => v.push((p, m)),
            }
        }
    }
    let mut out: Vec<NearObject> = clusters
        .into_values()
        .map(|(shift_px, mut photos)| {
            photos.sort_by_key(|p| p.0);
            judge(photos, shift_px, cfg)
        })
        // small or hardly shifting things are not worth listing
        .filter(|o| !matches!(o.why_not, "too small" | "shifts too little to matter"))
        .collect();
    out.sort_by_key(|o| o.photos.first().map(|p| p.0));
    out
}

/// Whether the object reaches the edge of the picture (then it may be cut off there).
fn clipped(m: &CoarseMask) -> bool {
    m.cells().iter().any(|&(x, y)| x == 0 || y == 0 || x == GRID - 1 || y == GRID - 1)
}

/// How far the middle of the cells is from the middle of the picture (0 = in the middle, about 0.7 = in a corner).
fn off_centre(m: &CoarseMask) -> f32 {
    let c = m.cells();
    let n = c.len().max(1) as f32;
    let (mx, my) = (c.iter().map(|p| p.0 as f32 + 0.5).sum::<f32>() / n / GRID as f32, c.iter().map(|p| p.1 as f32 + 0.5).sum::<f32>() / n / GRID as f32);
    ((mx - 0.5).powi(2) + (my - 0.5).powi(2)).sqrt()
}

fn judge(photos: Vec<(usize, CoarseMask)>, shift_px: f32, cfg: &NearSeamConfig) -> NearObject {
    let biggest = photos.iter().map(|(_, m)| m.cells().len()).max().unwrap_or(0);
    let why = if biggest < cfg.min_cells {
        Some("too small")
    } else if shift_px < cfg.min_shift_px {
        Some("shifts too little to matter")
    } else if biggest as f32 > cfg.max_share * (GRID * GRID) as f32 {
        Some("too large for one object")
    } else {
        None
    };
    // the keeper shows it whole (no cell on the edge) and nearest to the middle; ties go to the lower number
    let keeper = match why {
        Some(_) => None,
        None => photos.iter().filter(|(_, m)| !clipped(m)).min_by(|x, y| off_centre(&x.1).total_cmp(&off_centre(&y.1))).map(|p| p.0),
    };
    let why_not = why.unwrap_or(if keeper.is_none() { "no photo shows it whole" } else { "" });
    NearObject { photos, keeper, why_not, shift_px }
}

/// The masks proposed for one object, as (photo, stroke): green on the keeper, red on every other photo, one filled
/// rectangle per run of cells in a row (the object's cells, so a few cells wider than its points).
pub fn strokes(obj: &NearObject, cfg: &NearSeamConfig) -> Vec<(usize, MaskStroke)> {
    let Some(keeper) = obj.keeper else { return Vec::new() };
    let mut out = Vec::new();
    for (p, m) in &obj.photos {
        if *p != keeper && !cfg.avoid_others {
            continue;
        }
        let kind = if *p == keeper { MaskKind::Positive } else { MaskKind::Negative };
        for cy in 0..GRID {
            let mut cx = 0;
            while cx < GRID {
                if !m.get(cx, cy) {
                    cx += 1;
                    continue;
                }
                let x0 = cx;
                while cx < GRID && m.get(cx, cy) {
                    cx += 1;
                }
                let g = GRID as f32;
                let (l, r, t, b) = (x0 as f32 / g, cx as f32 / g, cy as f32 / g, (cy + 1) as f32 / g);
                out.push((*p, MaskStroke { kind, points: vec![[l, t], [r, t], [r, b], [l, b]], radius: 0.0, polygon: true, auto: true }));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depth::{NearRegion, Source};

    const DIMS: (usize, usize) = (1600, 1200);

    fn mask(cells: &[(usize, usize)]) -> CoarseMask {
        let mut m = CoarseMask::default();
        for &(x, y) in cells {
            m.set(x, y);
        }
        m
    }

    fn block(x0: usize, y0: usize, w: usize, h: usize) -> CoarseMask {
        let v: Vec<(usize, usize)> = (y0..y0 + h).flat_map(|y| (x0..x0 + w).map(move |x| (x, y))).collect();
        mask(&v)
    }

    fn region(a: usize, b: usize, ma: CoarseMask, mb: CoarseMask) -> NearRegion {
        NearRegion { a, b, mask_a: ma, mask_b: mb, dims_a: DIMS, dims_b: DIMS, shift_px: 20.0, count: 30, source: Source::Residuals }
    }

    #[test]
    fn one_object_seen_in_three_photos_is_kept_from_the_most_central_whole_view() {
        // photo 1: the car in the middle; photo 0: at the right edge (cut); photo 2: left but whole
        let (c0, c1, c2) = (block(13, 6, 3, 3), block(6, 6, 3, 3), block(1, 6, 3, 3));
        let layers = Layers { regions: vec![region(0, 1, c0, c1), region(1, 2, c1, c2)] };
        let o = plan(&layers, 3, &NearSeamConfig::default());
        assert_eq!(o.len(), 1, "{o:?}");
        assert_eq!(o[0].photos.iter().map(|p| p.0).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(o[0].keeper, Some(1));
        let red = NearSeamConfig { avoid_others: true, ..Default::default() };
        let s = strokes(&o[0], &red);
        // by default only the keeper gets green
        assert!(strokes(&o[0], &NearSeamConfig::default()).iter().all(|(p, st)| *p == 1 && st.kind == MaskKind::Positive));
        assert!(s.iter().filter(|(p, _)| *p == 1).all(|(_, st)| st.kind == MaskKind::Positive && st.auto && st.polygon));
        assert!(s.iter().filter(|(p, _)| *p != 1).all(|(_, st)| st.kind == MaskKind::Negative && st.auto));
        // one rectangle per row of cells and photo: 3 rows x 3 photos
        assert_eq!(s.len(), 9);
        // every stroke is a rectangle inside the picture
        assert!(s.iter().all(|(_, st)| st.points.len() == 4 && st.points.iter().all(|p| (0.0..=1.0).contains(&p[0]) && (0.0..=1.0).contains(&p[1]))));
    }

    #[test]
    fn regions_that_only_share_a_photo_but_not_its_cells_are_different_objects() {
        // photo 1 shows two things: a car on the left (seen with photo 0) and a railing on the right (seen with photo 2)
        let layers = Layers { regions: vec![region(0, 1, block(8, 6, 3, 3), block(1, 6, 3, 3)), region(1, 2, block(10, 8, 3, 3), block(2, 8, 3, 3))] };
        let o = plan(&layers, 3, &NearSeamConfig::default());
        assert_eq!(o.len(), 2, "{o:?}");
        // the two regions of one pair that overlap are one object
        let one = Layers { regions: vec![region(0, 1, block(5, 5, 3, 3), block(5, 5, 3, 3)), region(0, 1, block(6, 5, 3, 3), block(6, 5, 3, 3))] };
        assert_eq!(plan(&one, 2, &NearSeamConfig::default()).len(), 1);
    }

    #[test]
    fn two_separate_objects_stay_two() {
        let layers = Layers {
            regions: vec![region(0, 1, block(2, 2, 3, 3), block(3, 2, 3, 3)), region(0, 1, block(10, 10, 3, 3), block(11, 10, 3, 3))],
        };
        let o = plan(&layers, 2, &NearSeamConfig::default());
        assert_eq!(o.len(), 2);
        assert!(o.iter().all(|x| x.keeper.is_some()));
    }

    #[test]
    fn objects_that_are_cut_off_everywhere_or_too_big_or_too_small_are_not_proposed() {
        let cut = Layers { regions: vec![region(0, 1, block(13, 6, 3, 3), block(0, 6, 3, 3))] };
        let o = plan(&cut, 2, &NearSeamConfig::default());
        assert_eq!((o.len(), o[0].keeper, o[0].why_not), (1, None, "no photo shows it whole"));
        assert!(strokes(&o[0], &NearSeamConfig::default()).is_empty());
        let big = Layers { regions: vec![region(0, 1, block(2, 2, 12, 8), block(2, 2, 12, 8))] };
        // a region that large is a wide shift, not an object: it is dropped before objects are formed
        assert!(plan(&big, 2, &NearSeamConfig::default()).is_empty());
        let small = Layers { regions: vec![region(0, 1, block(5, 5, 1, 1), block(5, 5, 1, 1))] };
        assert!(plan(&small, 2, &NearSeamConfig::default()).is_empty(), "too small to list");
        let slight = Layers { regions: vec![NearRegion { shift_px: 5.0, ..region(0, 1, block(5, 5, 3, 3), block(5, 5, 3, 3)) }] };
        assert!(plan(&slight, 2, &NearSeamConfig::default()).is_empty(), "shifts too little to list");
    }

    #[test]
    fn switched_off_or_nothing_found_proposes_nothing() {
        let layers = Layers { regions: vec![region(0, 1, block(5, 5, 3, 3), block(5, 5, 3, 3))] };
        assert!(plan(&layers, 2, &NearSeamConfig { enabled: false, ..Default::default() }).is_empty());
        assert!(plan(&Layers::default(), 2, &NearSeamConfig::default()).is_empty());
    }
}
