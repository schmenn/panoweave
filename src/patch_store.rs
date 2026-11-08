//! Patch comparison records: the data model and the event bus behind the patch comparison
//! window. No GUI code here; the matching (fine_align) writes into [`STORE`], the window reads.
//!
//! Rules (from the person's specification):
//! - every measured patch pair of the CURRENT round is kept in RAM with its image cut-outs and
//!   score field, also while the window is closed (so it can show everything later);
//! - a new matching round replaces the list; after a panorama export the whole store is freed;
//! - live events are only produced while the window listens (window open), so a closed window
//!   costs nothing beyond storing the records;
//! - cut-outs are stored as 8-bit grey (0 = invalid pixel), score fields as 16-bit floats.

use crate::align::{ControlPoint, CpKind};
use crate::fine_align::{Geo, Plane};
use crate::geometry::V3;
use crate::patch_matcher::{LevelImages, MatchConfig, MatchDetail, MatchResult, Region, SearchTrace};
use half::f16;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Instant;

/// Extra pixels kept around patch A (pre-blur and gradient need a border).
pub const PATCH_MARGIN: usize = 4;
/// Events waiting for the window at most; further live frames are dropped (skipped).
pub const EVENT_QUEUE: usize = 4096;
/// Animation: every N-th tested offset is shown (default and range).
pub const SAMPLE_EVERY_DEFAULT: u32 = 10;
pub const SAMPLE_EVERY_RANGE: (u32, u32) = (1, 200);
/// Animation: waiting time per shown frame in ms (default and range).
pub const FRAME_DELAY_DEFAULT_MS: u32 = 0;
pub const FRAME_DELAY_RANGE_MS: (u32, u32) = (0, 500);
/// The worker checks for Cancel at least this often while it waits for the display (ms).
const DELAY_SLICE_MS: u64 = 10;
/// Confidence given to patches shifted by hand (they enter the optimisation with this weight).
pub const MANUAL_CONFIDENCE_DEFAULT: f32 = 1.0;
/// The window warns when the patch data take more than this.
pub const MEMORY_WARN_BYTES: usize = 1 << 30;
/// Reason of valid automatic areas that were not used because of the per-pair limit.
pub const REASON_AREA_LIMIT: &str = "not used (area limit for this pair)";
pub const REASON_OK: &str = "ok";
pub const REASON_B_SKIPPED: &str = "photo B not sampled (the template had too little texture, so the search was skipped)";

/// Identity of a patch across rounds: the two photos and the spot in photo A.
pub type Key = (usize, usize, i64, i64);

pub fn key_of(a: usize, b: usize, pa: [f64; 2]) -> Key {
    (a, b, (pa[0] * 1e7).round() as i64, (pa[1] * 1e7).round() as i64)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, serde::Serialize, serde::Deserialize)]
pub enum Status {
    Valid,
    Rejected,
    DisabledManual,
}

impl Status {
    pub fn name(&self) -> &'static str {
        match self {
            Status::Valid => "valid",
            Status::Rejected => "rejected",
            Status::DisabledManual => "disabled by hand",
        }
    }
}

/// Encoded grey image: 0 = invalid, 1..255 = value 0..1.
fn enc(v: f32, valid: bool) -> u8 {
    if !valid { 0 } else { 1 + (v.clamp(0.0, 1.0) * 254.0).round() as u8 }
}
fn dec(b: u8) -> (f32, bool) {
    if b == 0 { (0.0, false) } else { ((b - 1) as f32 / 254.0, true) }
}

/// Everything measured for one patch pair (never changed afterwards).
pub struct PatchData {
    pub id: u32,
    pub image_a: usize,
    pub image_b: usize,
    pub key: Key,
    /// spot in photo A (normalised coordinates, like control points)
    pub pa: [f64; 2],
    pub kind: CpKind,
    /// the point was placed by hand in the point editor (its photo-B spot is never moved)
    pub hand_point: bool,
    /// centre direction (world), for the position in the panorama
    pub dir: V3,
    /// parameters used (patch_size, search_radius, metric, feature_space and the checks)
    pub cfg: MatchConfig,
    /// the matcher's own result
    pub auto: MatchResult,
    pub uniqueness: f32,
    pub texture: f32,
    /// mean absolute grey difference before matching (0..255), NaN if not measured
    pub diff_before: f32,
    /// patch A with a margin (a_size x a_size, encoded)
    pub a_size: usize,
    pub a: Vec<u8>,
    /// the search region in photo B (b_size x b_size, encoded); None when it was not sampled
    pub b_size: usize,
    pub b: Option<Vec<u8>>,
    /// score field of every tested shift at finest resolution ((2 r + 1)^2, NaN = not tested);
    /// coarse pyramid levels fill blocks, finer levels overwrite them
    pub map_radius: isize,
    pub map: Vec<f16>,
    /// pyramid levels used below the finest
    pub levels: usize,
    pub plane: Plane,
    pub geo_a: Geo,
    pub geo_b: Geo,
}

impl PatchData {
    /// Bytes of the arrays (shown in the window).
    pub fn bytes(&self) -> usize {
        self.a.len() + self.b.as_ref().map(|b| b.len()).unwrap_or(0) + self.map.len() * 2 + std::mem::size_of::<PatchData>()
    }
    /// Region of photo A as the matcher saw it (patch A embedded in a region of B's size).
    pub fn region_a(&self) -> Region {
        let s = if self.b.is_some() { self.b_size } else { self.a_size };
        let mut r = Region::new(s);
        let off = (s / 2) as isize - (self.a_size / 2) as isize;
        for y in 0..self.a_size {
            for x in 0..self.a_size {
                let (xx, yy) = (x as isize + off, y as isize + off);
                if xx < 0 || yy < 0 || xx >= s as isize || yy >= s as isize {
                    continue;
                }
                let (v, m) = dec(self.a[y * self.a_size + x]);
                let i = yy as usize * s + xx as usize;
                r.data[i] = v;
                r.mask[i] = m;
            }
        }
        r
    }
    pub fn region_b(&self) -> Option<Region> {
        let b = self.b.as_ref()?;
        let s = self.b_size;
        let mut r = Region::new(s);
        for i in 0..s * s {
            let (v, m) = dec(b[i]);
            r.data[i] = v;
            r.mask[i] = m;
        }
        Some(r)
    }
    pub fn map_f32(&self) -> Vec<f32> {
        self.map.iter().map(|v| v.to_f32()).collect()
    }
    /// half size of the template (score window is 2 h + 1 pixels wide)
    pub fn half(&self) -> usize {
        self.cfg.patch_size / 2
    }
}

/// The editable part of a record (status, manual shift). Kept in the store, not in the window.
#[derive(Clone)]
pub struct PatchState {
    pub status: Status,
    pub reason: &'static str,
    /// status before it was disabled by hand (restored when it is enabled again)
    pub prev_status: Status,
    pub prev_reason: &'static str,
    /// shifted by hand
    pub manual: bool,
    pub dx: f32,
    pub dy: f32,
    pub score: f32,
    pub confidence: f32,
    /// the control pair that goes into the optimisation (None = not used)
    pub cp: Option<ControlPoint>,
    /// the control pair as the matching made it (for "reset to the automatic result")
    pub cp_auto: Option<ControlPoint>,
    /// changed in the window but not applied yet
    pub changed: Option<Change>,
    /// status of the automatic result (restored by "reset to the automatic result")
    pub auto_status: Status,
    pub auto_reason: &'static str,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    Disabled,
    Enabled,
    Moved,
    Reset,
}

impl Change {
    pub fn name(&self) -> &'static str {
        match self {
            Change::Disabled => "disabled",
            Change::Enabled => "enabled again",
            Change::Moved => "shifted",
            Change::Reset => "reset",
        }
    }
}

impl PatchState {
    pub fn magnitude(&self) -> f32 {
        (self.dx * self.dx + self.dy * self.dy).sqrt()
    }
    pub fn active(&self) -> bool {
        self.status != Status::DisabledManual
    }
    /// the control pair is an automatic control area
    pub fn kind_area(&self) -> bool {
        self.cp.map(|c| c.kind == CpKind::Area).unwrap_or(false)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Phase {
    #[default]
    Empty,
    Running,
    Finished,
    Cancelled,
    /// freed after a panorama export
    Freed,
}

#[derive(Default)]
struct Round {
    slots: Vec<Option<(Arc<PatchData>, PatchState)>>,
    total: usize,
    done: usize,
    valid: usize,
    rejected: usize,
    bytes: usize,
    phase: Phase,
    started: Option<Instant>,
    elapsed_ms: u128,
    label: String,
    number: u64,
    /// patches of this round are kept for the window (`FineConfig::record_patches`)
    recording: bool,
}

/// One frame of the live preview: patch A at one tested offset over the search area.
pub struct LiveFrame {
    pub id: u32,
    pub image_a: usize,
    pub image_b: usize,
    pub level: usize,
    pub levels: usize,
    pub images: Arc<LevelImages>,
    /// tested offset in level units
    pub offset: (f32, f32),
    pub score: f32,
    /// |A - B| at this offset over the template ((2 half + 1)^2, NaN = invalid)
    pub diff: Vec<f32>,
    /// the final frame (the found optimum, sub-pixel, finest level)
    pub last: bool,
}

pub enum Event {
    MatchingStarted,
    PatchStarted { id: u32, levels: usize, image_a: usize, image_b: usize },
    PatchProgress(Box<LiveFrame>),
    PatchFinished,
    MatchingFinished,
    MatchingCancelled,
    Freed,
}

/// Summary of the current round (cheap to copy).
#[derive(Clone, Default)]
pub struct RoundInfo {
    pub total: usize,
    pub done: usize,
    pub valid: usize,
    pub rejected: usize,
    pub bytes: usize,
    pub phase: Phase,
    pub elapsed_ms: u128,
    pub label: String,
    pub number: u64,
    /// false: the round ran with patch recording switched off, the window has nothing to show
    pub recording: bool,
}

/// Kept for a patch when the optimisation is restarted from the window.
#[derive(Clone)]
#[allow(clippy::large_enum_variant)] // one per patch at most, only while the window restarts an optimisation
pub enum Override {
    Disabled,
    /// shifted by hand: kept as it is (not matched again)
    Manual(Arc<PatchData>, PatchState),
}

pub struct Store {
    round: RwLock<Round>,
    version: AtomicU64,
    next_id: AtomicU32,
    listening: AtomicBool,
    animate: AtomicBool,
    sample_every: AtomicU32,
    delay_ms: AtomicU32,
    focus: AtomicU32,
    tx: Mutex<Option<SyncSender<Event>>>,
    rounds: AtomicU64,
}

pub static STORE: LazyLock<Store> = LazyLock::new(|| Store {
    round: RwLock::new(Round::default()),
    version: AtomicU64::new(1),
    next_id: AtomicU32::new(1),
    listening: AtomicBool::new(false),
    animate: AtomicBool::new(true),
    sample_every: AtomicU32::new(SAMPLE_EVERY_DEFAULT),
    delay_ms: AtomicU32::new(FRAME_DELAY_DEFAULT_MS),
    focus: AtomicU32::new(0),
    tx: Mutex::new(None),
    rounds: AtomicU64::new(0),
});

impl Store {
    // ---------------- window side ----------------

    /// The window opens: events start flowing (thread-safe queue, read in the GUI thread).
    pub fn subscribe(&self) -> Receiver<Event> {
        let (tx, rx) = sync_channel(EVENT_QUEUE);
        *self.tx.lock().unwrap() = Some(tx);
        self.listening.store(true, Ordering::Release);
        rx
    }
    /// The window closes: no more events.
    pub fn unsubscribe(&self) {
        self.listening.store(false, Ordering::Release);
        *self.tx.lock().unwrap() = None;
        self.focus.store(0, Ordering::Release);
    }
    pub fn set_live(&self, animate: bool, sample_every: u32, delay_ms: u32) {
        self.animate.store(animate, Ordering::Relaxed);
        self.sample_every.store(sample_every.clamp(SAMPLE_EVERY_RANGE.0, SAMPLE_EVERY_RANGE.1), Ordering::Relaxed);
        self.delay_ms.store(delay_ms.clamp(FRAME_DELAY_RANGE_MS.0, FRAME_DELAY_RANGE_MS.1), Ordering::Relaxed);
    }
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
    pub fn info(&self) -> RoundInfo {
        let r = self.round.read().unwrap();
        let elapsed_ms = match (r.phase, r.started) {
            (Phase::Running, Some(t)) => t.elapsed().as_millis(),
            _ => r.elapsed_ms,
        };
        RoundInfo {
            total: r.total,
            done: r.done,
            valid: r.valid,
            rejected: r.rejected,
            bytes: r.bytes,
            phase: r.phase,
            elapsed_ms,
            label: r.label.clone(),
            number: r.number,
            recording: r.recording,
        }
    }
    /// All finished records of the current round (data shared, state copied).
    pub fn snapshot(&self) -> (u64, Vec<Arc<PatchData>>, Vec<PatchState>) {
        let v = self.version();
        let r = self.round.read().unwrap();
        let mut d = Vec::with_capacity(r.done);
        let mut s = Vec::with_capacity(r.done);
        for (pd, st) in r.slots.iter().flatten() {
            d.push(pd.clone());
            s.push(st.clone());
        }
        (v, d, s)
    }
    /// Change the editable part of records (by id). Returns how many were changed.
    pub fn edit(&self, ids: &[u32], f: impl Fn(&PatchData, &mut PatchState)) -> usize {
        let mut n = 0;
        {
            let mut r = self.round.write().unwrap();
            for slot in r.slots.iter_mut().flatten() {
                if ids.contains(&slot.0.id) {
                    f(&slot.0, &mut slot.1);
                    n += 1;
                }
            }
        }
        self.recount();
        n
    }
    fn recount(&self) {
        let mut r = self.round.write().unwrap();
        let (mut v, mut j) = (0, 0);
        for (_, st) in r.slots.iter().flatten() {
            match st.status {
                Status::Valid => v += 1,
                Status::Rejected => j += 1,
                Status::DisabledManual => {}
            }
        }
        r.valid = v;
        r.rejected = j;
        drop(r);
        self.version.fetch_add(1, Ordering::AcqRel);
    }
    /// Free everything (after a panorama export): lists cleared, arrays dropped.
    pub fn free_after_export(&self) {
        {
            let mut r = self.round.write().unwrap();
            let number = r.number;
            *r = Round { phase: Phase::Freed, number, ..Default::default() };
        }
        self.version.fetch_add(1, Ordering::AcqRel);
        self.emit(Event::Freed);
    }
    /// Disabled / shifted records, to be kept when the optimisation is restarted.
    pub fn overrides(&self) -> HashMap<Key, Override> {
        let r = self.round.read().unwrap();
        let mut m = HashMap::new();
        for (d, s) in r.slots.iter().flatten() {
            if s.status == Status::DisabledManual {
                m.insert(d.key, Override::Disabled);
            } else if s.manual {
                m.insert(d.key, Override::Manual(d.clone(), s.clone()));
            }
        }
        m
    }
    pub fn pending_changes(&self) -> HashMap<&'static str, usize> {
        let r = self.round.read().unwrap();
        let mut m = HashMap::new();
        for (_, s) in r.slots.iter().flatten() {
            if let Some(c) = s.changed {
                *m.entry(c.name()).or_insert(0) += 1;
            }
        }
        m
    }

    // ---------------- matching side ----------------

    fn emit(&self, e: Event) {
        if !self.listening.load(Ordering::Acquire) {
            return;
        }
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            match tx.try_send(e) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }
    /// A new matching round starts: the previous list is replaced. With `recording` off no patch
    /// is stored (the matching itself is the same), the window then says why it is empty.
    pub fn begin_round(&self, total: usize, label: &str, recording: bool) {
        {
            let mut r = self.round.write().unwrap();
            let number = self.rounds.fetch_add(1, Ordering::AcqRel) + 1;
            *r = Round {
                slots: Vec::with_capacity(total),
                total,
                phase: Phase::Running,
                started: Some(Instant::now()),
                label: label.to_string(),
                number,
                recording,
                ..Default::default()
            };
        }
        self.next_id.store(1, Ordering::Release);
        self.focus.store(0, Ordering::Release);
        self.version.fetch_add(1, Ordering::AcqRel);
        self.emit(Event::MatchingStarted);
    }
    pub fn end_round(&self, cancelled: bool) {
        {
            let mut r = self.round.write().unwrap();
            if r.phase != Phase::Running {
                return;
            }
            r.phase = if cancelled { Phase::Cancelled } else { Phase::Finished };
            r.elapsed_ms = r.started.map(|t| t.elapsed().as_millis()).unwrap_or(0);
        }
        self.version.fetch_add(1, Ordering::AcqRel);
        self.emit(if cancelled { Event::MatchingCancelled } else { Event::MatchingFinished });
    }
    /// A patch starts: returns its id and whether the live preview follows it.
    pub fn start_patch(&self, levels: usize, image_a: usize, image_b: usize) -> (u32, bool) {
        let id = self.next_id.fetch_add(1, Ordering::AcqRel);
        if !self.listening.load(Ordering::Acquire) {
            return (id, false);
        }
        // only one patch at a time is followed (the matching runs on many threads)
        let focused = self.focus.compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire).is_ok();
        if focused {
            self.emit(Event::PatchStarted { id, levels, image_a, image_b });
        }
        (id, focused)
    }
    pub fn wants_frames(&self) -> bool {
        self.listening.load(Ordering::Acquire) && self.animate.load(Ordering::Relaxed)
    }
    pub fn finish_patch(&self, data: PatchData, state: PatchState, focused: bool) {
        let id = data.id;
        {
            let mut r = self.round.write().unwrap();
            if r.phase != Phase::Running {
                return;
            }
            r.bytes += data.bytes();
            r.done += 1;
            match state.status {
                Status::Valid => r.valid += 1,
                Status::Rejected => r.rejected += 1,
                Status::DisabledManual => {}
            }
            let i = id as usize - 1;
            if r.slots.len() <= i {
                r.slots.resize_with(i + 1, || None);
            }
            r.slots[i] = Some((Arc::new(data), state));
        }
        self.version.fetch_add(1, Ordering::AcqRel);
        if focused {
            self.focus.store(0, Ordering::Release);
        }
        self.emit(Event::PatchFinished);
    }
    /// After the selection step of a round: which control pair a patch became.
    /// Id 0 means the patch was not recorded: nothing to do.
    pub fn finalize(&self, id: u32, cp: Option<ControlPoint>, rejected_reason: Option<&'static str>) {
        let Some(i) = (id as usize).checked_sub(1) else {
            return;
        };
        let mut r = self.round.write().unwrap();
        let mut delta = (0isize, 0isize);
        if let Some(Some((_, st))) = r.slots.get_mut(i) {
            st.cp = cp;
            st.cp_auto = cp;
            if let Some(reason) = rejected_reason
                && st.auto_status == Status::Valid {
                    st.auto_status = Status::Rejected;
                    st.auto_reason = reason;
                    st.prev_status = Status::Rejected;
                    st.prev_reason = reason;
                    if st.status == Status::Valid {
                        st.status = Status::Rejected;
                        st.reason = reason;
                        delta = (-1, 1);
                    }
                }
        }
        r.valid = (r.valid as isize + delta.0) as usize;
        r.rejected = (r.rejected as isize + delta.1) as usize;
    }
    /// Re-insert a record that was shifted by hand (not matched again) under a new id.
    pub fn keep_manual(&self, d: &PatchData, s: &PatchState) -> u32 {
        let (id, _) = self.start_patch(d.levels, d.image_a, d.image_b);
        let data = PatchData {
            id,
            image_a: d.image_a,
            image_b: d.image_b,
            key: d.key,
            pa: d.pa,
            kind: d.kind,
            hand_point: d.hand_point,
            dir: d.dir,
            cfg: d.cfg.clone(),
            auto: d.auto.clone(),
            uniqueness: d.uniqueness,
            texture: d.texture,
            diff_before: d.diff_before,
            a_size: d.a_size,
            a: d.a.clone(),
            b_size: d.b_size,
            b: d.b.clone(),
            map_radius: d.map_radius,
            map: d.map.clone(),
            levels: d.levels,
            plane: d.plane,
            geo_a: d.geo_a,
            geo_b: d.geo_b,
        };
        let mut st = s.clone();
        st.changed = None;
        self.finish_patch(data, st, false);
        id
    }

    /// Replays a finished search for the live preview.
    ///
    /// The matcher computes all offsets of a level in one go (like OpenCV's matchTemplate), so
    /// the frames are rebuilt AFTER the search from what it tested: the overlay is moved over the
    /// tested offsets of each pyramid level in the order they were computed (scan order), every
    /// N-th one is shown, and the difference image for a shown offset is computed from patch A
    /// and the matching part of B. The matching itself is not slowed down unless a waiting time
    /// per frame is set (then the worker sleeps so the person can follow it).
    pub fn play_trace(&self, id: u32, image_a: usize, image_b: usize, tr: &SearchTrace, res: &MatchResult) {
        if !self.wants_frames() || tr.images.is_empty() {
            return;
        }
        let every = self.sample_every.load(Ordering::Relaxed).max(1) as usize;
        let delay = self.delay_ms.load(Ordering::Relaxed) as u64;
        let imgs: Vec<Arc<LevelImages>> = tr.images.iter().cloned().map(Arc::new).collect();
        let levels = tr.levels;
        let send = |level: usize, off: (f32, f32), score: f32, last: bool| -> bool {
            if !self.wants_frames() || crate::engine::cancelled() {
                return false;
            }
            let im = &imgs[level.min(imgs.len() - 1)];
            let diff = level_diff(im, off);
            self.emit(Event::PatchProgress(Box::new(LiveFrame { id, image_a, image_b, level, levels, images: im.clone(), offset: off, score, diff, last })));
            if delay > 0 {
                let mut left = delay;
                while left > 0 {
                    if crate::engine::cancelled() || !self.wants_frames() {
                        return false;
                    }
                    let s = left.min(DELAY_SLICE_MS);
                    std::thread::sleep(std::time::Duration::from_millis(s));
                    left -= s;
                }
            }
            true
        };
        // per level (coarse to fine) the animation starts again with that level's search area
        for level in (0..=levels).rev() {
            let list: Vec<&(u8, i16, i16, f32)> = tr.tested.iter().filter(|t| t.0 as usize == level).collect();
            for (k, t) in list.iter().enumerate() {
                if k % every != 0 && k + 1 != list.len() {
                    continue;
                }
                if !send(level, (t.1 as f32, t.2 as f32), t.3, false) {
                    return;
                }
            }
        }
        let _ = send(0, (res.dx, res.dy), res.score, true);
    }
}

/// |A - B| at a (fractional) offset on one pyramid level, over the template window.
pub fn level_diff(im: &LevelImages, off: (f32, f32)) -> Vec<f32> {
    let s = im.size as isize;
    let c = s / 2;
    let h = im.half;
    let n = (2 * h + 1) as usize;
    let mut out = vec![f32::NAN; n * n];
    let sample = |x: f32, y: f32| -> Option<f32> {
        let (x0, y0) = (x.floor() as isize, y.floor() as isize);
        let (tx, ty) = (x - x0 as f32, y - y0 as f32);
        if x0 < 0 || y0 < 0 || x0 + 1 >= s || y0 + 1 >= s {
            return None;
        }
        let i = (y0 * s + x0) as usize;
        let su = s as usize;
        if !(im.mb[i] && im.mb[i + 1] && im.mb[i + su] && im.mb[i + su + 1]) {
            return None;
        }
        let v = &im.b;
        Some((v[i] * (1.0 - tx) + v[i + 1] * tx) * (1.0 - ty) + (v[i + su] * (1.0 - tx) + v[i + su + 1] * tx) * ty)
    };
    for dy in -h..=h {
        for dx in -h..=h {
            let ia = ((c + dy) * s + c + dx) as usize;
            if !im.ma[ia] {
                continue;
            }
            if let Some(vb) = sample((c + dx) as f32 + off.0, (c + dy) as f32 + off.1) {
                out[((dy + h) as usize) * n + (dx + h) as usize] = (im.a[ia] - vb).abs();
            }
        }
    }
    out
}

/// Inputs to build a record (from the fine alignment).
pub struct RecordInput<'a> {
    pub id: u32,
    pub a: usize,
    pub b: usize,
    pub pa: [f64; 2],
    pub kind: CpKind,
    pub hand_point: bool,
    pub dir: V3,
    pub cfg: &'a MatchConfig,
    pub ra: &'a Region,
    pub rb: Option<&'a Region>,
    pub trace: &'a SearchTrace,
    pub result: &'a MatchResult,
    pub detail: MatchDetail,
    pub diff_before: f32,
    pub plane: Plane,
    pub geo_a: Geo,
    pub geo_b: Geo,
}

pub fn build_record(inp: RecordInput) -> (PatchData, PatchState) {
    let cfg = inp.cfg;
    let s = inp.ra.size;
    let c = s / 2;
    let hh = cfg.patch_size / 2 + PATCH_MARGIN;
    let a_size = 2 * hh + 1;
    let mut a = vec![0u8; a_size * a_size];
    for y in 0..a_size {
        for x in 0..a_size {
            let (xx, yy) = (c as isize + x as isize - hh as isize, c as isize + y as isize - hh as isize);
            if xx < 0 || yy < 0 || xx >= s as isize || yy >= s as isize {
                continue;
            }
            let i = yy as usize * s + xx as usize;
            a[y * a_size + x] = enc(inp.ra.data[i], inp.ra.mask[i]);
        }
    }
    let b = inp.rb.map(|rb| rb.data.iter().zip(&rb.mask).map(|(&v, &m)| enc(v, m)).collect::<Vec<u8>>());
    // score field at the finest resolution from everything the search tested
    let r = cfg.search_radius.ceil() as isize;
    let n = (2 * r + 1) as usize;
    let mut map = vec![f32::NAN; n * n];
    for &(l, ox, oy, v) in &inp.trace.tested {
        let k = 1isize << l;
        let (cx, cy) = (ox as isize * k, oy as isize * k);
        let (lo, hi) = (-(k / 2), k - k / 2 - 1);
        for yy in cy + lo..=cy + hi {
            for xx in cx + lo..=cx + hi {
                if xx.abs() <= r && yy.abs() <= r {
                    map[(yy + r) as usize * n + (xx + r) as usize] = v;
                }
            }
        }
    }
    let res = inp.result;
    let status = if res.valid { Status::Valid } else { Status::Rejected };
    let reason = if res.valid { REASON_OK } else if inp.rb.is_none() && res.reason == "too little texture" { "too little texture" } else { res.reason };
    let data = PatchData {
        id: inp.id,
        image_a: inp.a,
        image_b: inp.b,
        key: key_of(inp.a, inp.b, inp.pa),
        pa: inp.pa,
        kind: inp.kind,
        hand_point: inp.hand_point,
        dir: inp.dir,
        cfg: cfg.clone(),
        auto: res.clone(),
        uniqueness: inp.detail.uniqueness,
        texture: inp.detail.texture,
        diff_before: inp.diff_before,
        a_size,
        a,
        b_size: s,
        b,
        map_radius: r,
        map: map.into_iter().map(f16::from_f32).collect(),
        levels: inp.trace.levels,
        plane: inp.plane,
        geo_a: inp.geo_a,
        geo_b: inp.geo_b,
    };
    let state = PatchState {
        status,
        reason,
        prev_status: status,
        prev_reason: reason,
        manual: false,
        dx: res.dx,
        dy: res.dy,
        score: res.score,
        confidence: res.confidence,
        cp: None,
        cp_auto: None,
        changed: None,
        auto_status: status,
        auto_reason: reason,
    };
    (data, state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_keeps_invalid_pixels() {
        assert!(!dec(enc(0.5, false)).1);
        let (v, m) = dec(enc(0.5, true));
        assert!(m && (v - 0.5).abs() < 0.01);
        let (v, _) = dec(enc(0.0, true));
        assert!(v.abs() < 1e-6);
    }

    /// With recording off every patch has id 0; finishing it must be a no-op, not an underflow.
    #[test]
    fn finalize_of_an_unrecorded_patch_does_nothing() {
        STORE.finalize(0, None, Some(REASON_AREA_LIMIT));
    }
}
