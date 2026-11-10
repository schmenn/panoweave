//! Near objects in the export: a car 3 m in front of the camera is seen from slightly different places in
//! neighbouring photos, so where two photos are blended it shows twice or blurred. The seam must not cut through it,
//! it has to run round it.
//!
//! This module only *proposes*: per near object (found by `depth.rs`) a tight outline in every photo that sees it,
//! built from the matched points of the object (a small disc round each point, so things standing beside it, such as a
//! van that moved away between two shots, are not inside). Applied (`app/masks_near.rs`) the outline becomes mask
//! strokes of the kind `Guard`: the seam finder (`seams.rs`) gets a high cost for every seam edge there (so the seam
//! goes round) but is otherwise free: which photo shows which part is decided as before. Only objects that are sure
//! enough are proposed (several photo pairs agree or many points, a clear shift, points that lie together).

use crate::depth::{Layers, NearRegion, Source};
use crate::masks::{MaskKind, MaskStroke};

/// Settings of the proposal.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct NearSeamConfig {
    /// off = nothing is proposed
    pub enabled: bool,
    /// a region needs at least this many points (found from errors, with outline points)
    pub min_points: usize,
    /// an object that shifts less than this between the photos (working-image pixels at 1600 px, the largest of its
    /// regions) only blurs a little and is not worth a mask
    pub min_shift_px: f32,
    /// an object must be found in this many photo pairs ...
    pub min_regions: usize,
    /// ... or have at least this many points in all (one pair with very many points is sure enough)
    pub strong_points: usize,
    /// a region that covers more than this share of a photo (cells of its coarse mask) is no single object
    pub max_share: f32,
    /// radius of the disc round every point of the outline, as a share of the long side of the photo
    pub outline_radius: f32,
    /// a point belongs to the outline only when another point lies within this many radii (strays stay out)
    pub neighbour_reach: f32,
}

impl Default for NearSeamConfig {
    fn default() -> Self {
        NearSeamConfig { enabled: true, min_points: 12, min_shift_px: 12.0, min_regions: 2, strong_points: 40, max_share: 0.25, outline_radius: 0.02, neighbour_reach: 3.0 }
    }
}

impl NearSeamConfig {
    /// Environment `PW_NEAR_SEAMS=0` switches the proposal off, `PW_NEAR_RADIUS=<share>` sets the outline radius
    /// (experiments).
    pub fn with_env(mut self) -> Self {
        if std::env::var("PW_NEAR_SEAMS").is_ok_and(|v| v == "0") {
            self.enabled = false;
        }
        if let Some(r) = std::env::var("PW_NEAR_RADIUS").ok().and_then(|v| v.parse().ok()) {
            self.outline_radius = r;
        }
        self
    }
}

/// One near object: its outline points (shares of the picture) in every photo that sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct NearObject {
    pub photos: Vec<(usize, Vec<[f32; 2]>)>,
    /// the largest shift of its regions (working-image pixels at 1600 px)
    pub shift_px: f32,
    /// photo pairs that found it, and points in all
    pub regions: usize,
    pub points: usize,
}

impl NearObject {
    pub fn text(&self) -> String {
        let list = self.photos.iter().map(|(p, _)| (p + 1).to_string()).collect::<Vec<_>>().join(", ");
        format!("Near object in photos {list} (about {:.0} px shift, {} photo pair{}, {} points): seams will not cut through it.", self.shift_px, self.regions, if self.regions == 1 { "" } else { "s" }, self.points)
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

/// Points closer than this (share of the picture) count as one when the outline is made.
const THIN: f32 = 0.006;

/// Groups the regions of `layers` into objects and keeps the sure ones. A region says that the same thing lies on its
/// points in both photos of a pair; two regions that share a photo are the same object when their cells in that photo
/// overlap enough (a car seen in photos 8, 10 and 14 gives regions 8-10 and 8-14 that overlap in photo 8).
pub fn plan(layers: &Layers, n_photos: usize, cfg: &NearSeamConfig) -> Vec<NearObject> {
    if !cfg.enabled || layers.is_empty() {
        return Vec::new();
    }
    let limit = (cfg.max_share * (crate::depth::GRID * crate::depth::GRID) as f32) as usize;
    let regions: Vec<&NearRegion> = layers
        .regions
        .iter()
        .filter(|r| r.source == Source::Residuals && r.a < n_photos && r.b < n_photos && r.pts_a.len() >= cfg.min_points && r.pts_b.len() >= cfg.min_points && r.mask_a.count() <= limit && r.mask_b.count() <= limit)
        .collect();
    let mut uf = Uf((0..regions.len()).collect());
    for i in 0..regions.len() {
        for j in i + 1..regions.len() {
            let sides = |r: &NearRegion| [(r.a, r.mask_a), (r.b, r.mask_b)];
            let same = sides(regions[i]).iter().any(|(p, m)| sides(regions[j]).iter().any(|(q, n)| p == q && m.overlap(n) as f32 >= SAME_OBJECT_OVERLAP * m.count().max(n.count()) as f32));
            if same {
                uf.union(i, j);
            }
        }
    }
    struct Cluster {
        shift: f32,
        regions: usize,
        points: usize,
        photos: Vec<(usize, Vec<[f32; 2]>)>,
    }
    let mut clusters: std::collections::BTreeMap<usize, Cluster> = Default::default();
    for (i, r) in regions.iter().enumerate() {
        let c = clusters.entry(uf.find(i)).or_insert(Cluster { shift: 0.0, regions: 0, points: 0, photos: Vec::new() });
        c.shift = c.shift.max(r.shift_px);
        c.regions += 1;
        c.points += r.count;
        for (p, pts) in [(r.a, &r.pts_a), (r.b, &r.pts_b)] {
            match c.photos.iter_mut().find(|(q, _)| *q == p) {
                Some((_, v)) => v.extend_from_slice(pts),
                None => c.photos.push((p, pts.clone())),
            }
        }
    }
    let mut out = Vec::new();
    for c in clusters.into_values() {
        let sure = c.shift >= cfg.min_shift_px && (c.regions >= cfg.min_regions || c.points >= cfg.strong_points);
        if !sure {
            continue;
        }
        let mut photos: Vec<(usize, Vec<[f32; 2]>)> = c.photos.into_iter().map(|(p, v)| (p, outline_points(v, cfg))).filter(|(_, v)| !v.is_empty()).collect();
        photos.sort_by_key(|p| p.0);
        if photos.len() >= 2 {
            out.push(NearObject { photos, shift_px: c.shift, regions: c.regions, points: c.points });
        }
    }
    out.sort_by_key(|o| o.photos.first().map(|p| p.0));
    out
}

/// The points that make the outline: thinned out, and without strays (a point needs a neighbour within
/// `neighbour_reach` radii), so the discs round them cover the object and not what stands beside it.
fn outline_points(pts: Vec<[f32; 2]>, cfg: &NearSeamConfig) -> Vec<[f32; 2]> {
    let mut thin: Vec<[f32; 2]> = Vec::new();
    for p in pts {
        if !thin.iter().any(|q| (q[0] - p[0]).hypot(q[1] - p[1]) < THIN) {
            thin.push(p);
        }
    }
    let reach = cfg.neighbour_reach * cfg.outline_radius;
    let keep: Vec<[f32; 2]> = thin.iter().copied().filter(|p| thin.iter().any(|q| q != p && (q[0] - p[0]).hypot(q[1] - p[1]) <= reach)).collect();
    keep
}

/// The mask strokes of an object, as (photo, stroke): a disc of `Guard` round every outline point of every photo that
/// sees it (a seam must not cut through it in any of them).
pub fn strokes(obj: &NearObject, cfg: &NearSeamConfig) -> Vec<(usize, MaskStroke)> {
    let mut out = Vec::new();
    for (p, pts) in &obj.photos {
        for pt in pts {
            out.push((*p, MaskStroke { kind: MaskKind::Guard, points: vec![*pt], radius: cfg.outline_radius, polygon: false, auto: true }));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depth::CoarseMask;

    const DIMS: (usize, usize) = (1600, 1200);

    fn cells(x0: usize, y0: usize, w: usize, h: usize) -> CoarseMask {
        let mut m = CoarseMask::default();
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                m.set(x, y);
            }
        }
        m
    }

    /// points on a grid of 0.03 spacing inside a box (shares of the picture)
    fn pts(x0: f32, y0: f32, n: usize) -> Vec<[f32; 2]> {
        (0..n).map(|i| [x0 + 0.03 * (i % 5) as f32, y0 + 0.03 * (i / 5) as f32]).collect()
    }

    fn region(a: usize, b: usize, ca: CoarseMask, cb: CoarseMask, pa: Vec<[f32; 2]>, pb: Vec<[f32; 2]>, shift: f32) -> NearRegion {
        NearRegion { a, b, mask_a: ca, mask_b: cb, dims_a: DIMS, dims_b: DIMS, shift_px: shift, count: pa.len(), source: Source::Residuals, pts_a: pa, pts_b: pb }
    }

    fn car() -> Layers {
        // a car seen in photos 0, 1, 2: regions 0-1 and 1-2 overlap in photo 1
        Layers {
            regions: vec![
                region(0, 1, cells(10, 6, 3, 3), cells(5, 6, 3, 3), pts(0.64, 0.4, 15), pts(0.33, 0.4, 15), 20.0),
                region(1, 2, cells(5, 6, 3, 3), cells(1, 6, 3, 3), pts(0.33, 0.4, 15), pts(0.07, 0.4, 15), 20.0),
            ],
        }
    }

    #[test]
    fn a_car_seen_in_three_photos_is_one_object_with_a_tight_outline() {
        let o = plan(&car(), 3, &NearSeamConfig::default());
        assert_eq!(o.len(), 1, "{o:?}");
        assert_eq!(o[0].photos.iter().map(|p| p.0).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!((o[0].regions, o[0].points), (2, 30));
        let s = strokes(&o[0], &NearSeamConfig::default());
        assert!(s.iter().all(|(_, st)| st.kind == MaskKind::Guard && st.auto && st.points.len() == 1 && st.radius == 0.02));
        // the outline lies on the points, not on a neighbour 0.2 away
        assert!(s.iter().filter(|(p, _)| *p == 1).all(|(_, st)| (0.33..=0.5).contains(&st.points[0][0]) && (0.4..=0.5).contains(&st.points[0][1])));
    }

    #[test]
    fn strays_far_from_the_group_stay_out_of_the_outline() {
        let mut l = car();
        l.regions[0].pts_a.push([0.1, 0.9]);
        l.regions[0].count += 1;
        let o = plan(&l, 3, &NearSeamConfig::default());
        assert!(o[0].photos[0].1.iter().all(|p| p[1] < 0.6), "{:?}", o[0].photos[0].1);
    }

    #[test]
    fn only_sure_objects_are_proposed() {
        let cfg = NearSeamConfig::default();
        // a single pair with few points: not sure
        let one = Layers { regions: vec![car().regions.remove(0)] };
        assert!(plan(&one, 3, &cfg).is_empty());
        // the same pair with very many points is sure enough
        let mut strong = car().regions.remove(0);
        strong.count = 60;
        assert_eq!(plan(&Layers { regions: vec![strong] }, 3, &cfg).len(), 1);
        // two pairs but a slight shift (a moving tree): not proposed
        let mut slight = car();
        slight.regions.iter_mut().for_each(|r| r.shift_px = 6.0);
        assert!(plan(&slight, 3, &cfg).is_empty());
        // too few points per region
        let mut few = car();
        few.regions.iter_mut().for_each(|r| {
            r.pts_a.truncate(5);
            r.pts_b.truncate(5);
        });
        assert!(plan(&few, 3, &cfg).is_empty());
        // regions from rejected matches carry no points and are not used
        let mut m = car();
        m.regions.iter_mut().for_each(|r| r.source = Source::Matches);
        assert!(plan(&m, 3, &cfg).is_empty());
    }

    #[test]
    fn regions_that_only_share_a_photo_but_not_its_cells_are_different_objects() {
        // photo 1 shows two things: a car on the left (with photo 0) and a railing on the right (with photo 2)
        let mut l = car();
        l.regions[1].mask_a = cells(11, 9, 3, 3);
        l.regions[1].pts_a = pts(0.7, 0.6, 15);
        let o = plan(&l, 3, &NearSeamConfig { min_regions: 1, ..Default::default() });
        assert_eq!(o.len(), 2, "{o:?}");
    }

    #[test]
    fn huge_regions_switched_off_and_nothing_found_propose_nothing() {
        let mut l = car();
        l.regions[0].mask_a = cells(0, 0, 14, 8);
        l.regions[1].mask_a = cells(0, 0, 14, 8);
        assert!(plan(&l, 3, &NearSeamConfig::default()).is_empty());
        assert!(plan(&car(), 3, &NearSeamConfig { enabled: false, ..Default::default() }).is_empty());
        assert!(plan(&Layers::default(), 3, &NearSeamConfig::default()).is_empty());
    }
}
