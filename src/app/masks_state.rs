//! The masks of one photo as the app keeps them: the strokes, the small picture made from them
//! (rebuilt after every change) and the undo / redo lists. No window code in here, so it can be tested.

use crate::masks::{self, MaskBuf, MaskStroke};
use std::sync::Arc;

/// How many steps can be taken back per photo.
pub(super) const MASK_UNDO_DEPTH: usize = 50;

#[derive(Default)]
pub(super) struct PhotoMask {
    /// the strokes in painting order (later ones paint over earlier ones)
    pub(super) strokes: Vec<MaskStroke>,
    /// the strokes painted into a picture; `None` when nothing is painted
    pub(super) buf: Option<Arc<MaskBuf>>,
    /// counts changes (to refresh the overlay picture and the seam preview)
    pub(super) rev: u64,
    undo: Vec<Vec<MaskStroke>>,
    redo: Vec<Vec<MaskStroke>>,
}

impl PhotoMask {
    fn rebuild(&mut self, w: usize, h: usize) {
        self.buf = masks::rasterize(&self.strokes, w, h).map(Arc::new);
        self.rev += 1;
    }

    /// Remembers the current strokes so the next change can be taken back.
    fn snapshot(&mut self) {
        self.undo.push(self.strokes.clone());
        if self.undo.len() > MASK_UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Adds a finished stroke (the photo is `w` x `h` pixels). Strokes without points are ignored.
    pub(super) fn commit(&mut self, stroke: MaskStroke, w: usize, h: usize) -> bool {
        if stroke.points.is_empty() {
            return false;
        }
        self.snapshot();
        self.strokes.push(stroke);
        self.rebuild(w, h);
        true
    }

    /// Replaces the automatic strokes (`auto`, proposed by the program) by `new` ones; strokes painted by hand stay.
    /// One step to take back. Returns how many automatic strokes were taken away when `new` is empty, else how many
    /// were added.
    pub(super) fn replace_auto(&mut self, new: Vec<MaskStroke>, w: usize, h: usize) -> usize {
        let old = self.strokes.iter().filter(|s| s.auto).count();
        if old == 0 && new.is_empty() {
            return 0;
        }
        self.snapshot();
        self.strokes.retain(|s| !s.auto);
        let added = new.len();
        // in front: strokes painted by hand later paint over them
        self.strokes.splice(0..0, new);
        self.rebuild(w, h);
        if added == 0 { old } else { added }
    }

    /// Removes everything painted on this photo (can be taken back).
    pub(super) fn clear(&mut self, w: usize, h: usize) -> bool {
        if self.strokes.is_empty() {
            return false;
        }
        self.snapshot();
        self.strokes.clear();
        self.rebuild(w, h);
        true
    }

    /// Takes strokes from a project file; this is no step to take back.
    pub(super) fn load(&mut self, strokes: Vec<MaskStroke>, w: usize, h: usize) {
        self.strokes = strokes;
        self.undo.clear();
        self.redo.clear();
        self.rebuild(w, h);
    }

    pub(super) fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub(super) fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub(super) fn undo(&mut self, w: usize, h: usize) -> bool {
        let Some(prev) = self.undo.pop() else { return false };
        self.redo.push(std::mem::replace(&mut self.strokes, prev));
        self.rebuild(w, h);
        true
    }

    pub(super) fn redo(&mut self, w: usize, h: usize) -> bool {
        let Some(next) = self.redo.pop() else { return false };
        self.undo.push(std::mem::replace(&mut self.strokes, next));
        self.rebuild(w, h);
        true
    }

    /// True if something is painted (for the marks in the photo list).
    pub(super) fn any(&self) -> bool {
        self.buf.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::masks::MaskKind;

    fn dab(kind: MaskKind, x: f32) -> MaskStroke {
        MaskStroke { kind, points: vec![[x, 0.5]], radius: 0.05, polygon: false, auto: false }
    }

    #[test]
    fn commit_undo_redo_walk_through_the_steps() {
        let mut m = PhotoMask::default();
        assert!(!m.any() && !m.can_undo() && !m.can_redo());
        assert!(m.commit(dab(MaskKind::Positive, 0.3), 200, 100));
        assert!(m.commit(dab(MaskKind::Negative, 0.7), 200, 100));
        assert_eq!(m.strokes.len(), 2);
        assert!(m.any());
        assert!(m.undo(200, 100));
        assert_eq!(m.strokes.len(), 1);
        assert!(m.can_redo());
        assert!(m.undo(200, 100));
        assert!(m.strokes.is_empty() && m.buf.is_none(), "all taken back: no mask picture");
        assert!(!m.undo(200, 100));
        assert!(m.redo(200, 100) && m.redo(200, 100));
        assert_eq!(m.strokes.len(), 2);
        assert!(!m.redo(200, 100));
        // a new stroke after undo drops the redo list
        m.undo(200, 100);
        m.commit(dab(MaskKind::Positive, 0.1), 200, 100);
        assert!(!m.can_redo());
    }

    #[test]
    fn clear_can_be_taken_back_and_empty_strokes_are_ignored() {
        let mut m = PhotoMask::default();
        assert!(!m.commit(MaskStroke { kind: MaskKind::Positive, points: vec![], radius: 0.02, polygon: false, auto: false }, 100, 100));
        assert!(!m.clear(100, 100), "nothing to clear");
        m.commit(dab(MaskKind::Positive, 0.5), 100, 100);
        assert!(m.clear(100, 100));
        assert!(m.buf.is_none());
        assert!(m.undo(100, 100));
        assert!(m.buf.is_some());
    }

    #[test]
    fn undo_depth_is_limited_and_loading_is_no_undo_step() {
        let mut m = PhotoMask::default();
        for i in 0..(MASK_UNDO_DEPTH + 10) {
            m.commit(dab(MaskKind::Positive, (i % 10) as f32 / 10.0), 100, 100);
        }
        let mut steps = 0;
        while m.undo(100, 100) {
            steps += 1;
        }
        assert_eq!(steps, MASK_UNDO_DEPTH);
        let mut l = PhotoMask::default();
        l.load(vec![dab(MaskKind::Negative, 0.5)], 100, 100);
        assert!(l.any() && !l.can_undo());
    }

    #[test]
    fn revision_counts_every_change() {
        let mut m = PhotoMask::default();
        let r0 = m.rev;
        m.commit(dab(MaskKind::Positive, 0.5), 100, 100);
        let r1 = m.rev;
        m.undo(100, 100);
        assert!(r1 > r0 && m.rev > r1);
    }
}
