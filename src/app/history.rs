//! Undo / redo bookkeeping without any window code (so it can be tested), and the one place that
//! reads the undo keys. Used for the control points; the masks keep their own history.

use eframe::egui::{Context, Key, Modifiers};

/// What the person asked for with the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UndoKey {
    Undo,
    Redo,
}

/// Reads and consumes Ctrl+Z (undo) and Ctrl+Y / Ctrl+Shift+Z (redo) in the viewport of `ctx`.
/// Returns `None` while a text field or drag value has the keyboard focus, so their own text undo
/// keeps working. Every secondary window calls this for its own viewport; the app collects the
/// requests and performs at most one step per frame.
pub(crate) fn read_undo_key(ctx: &Context) -> Option<UndoKey> {
    if ctx.egui_wants_keyboard_input() {
        return None;
    }
    ctx.input_mut(|i| {
        if i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || i.consume_key(Modifiers::COMMAND, Key::Y) {
            Some(UndoKey::Redo)
        } else if i.consume_key(Modifiers::COMMAND, Key::Z) {
            Some(UndoKey::Undo)
        } else {
            None
        }
    })
}

/// Two stacks of labelled states. `T` is the state to go back to (for the points: the list before
/// the change). A new step empties the redo stack; the depth limits both stacks.
pub(super) struct History<T> {
    undo: Vec<(String, T)>,
    redo: Vec<(String, T)>,
    depth: usize,
}

impl<T> History<T> {
    pub(super) fn new(depth: usize) -> Self {
        Self { undo: Vec::new(), redo: Vec::new(), depth: depth.max(1) }
    }

    /// A change has happened: `before` is the state to return to.
    pub(super) fn push(&mut self, what: &str, before: T) {
        self.undo.push((what.to_string(), before));
        Self::trim(&mut self.undo, self.depth);
        self.redo.clear();
    }

    /// Goes back one step. `current` is the state now; it is kept for redo.
    pub(super) fn undo(&mut self, current: T) -> Option<(String, T)> {
        let (what, prev) = self.undo.pop()?;
        self.redo.push((what.clone(), current));
        Self::trim(&mut self.redo, self.depth);
        Some((what, prev))
    }

    /// Goes forward again. `current` is the state now; it is kept for undo.
    pub(super) fn redo(&mut self, current: T) -> Option<(String, T)> {
        let (what, next) = self.redo.pop()?;
        self.undo.push((what.clone(), current));
        Self::trim(&mut self.undo, self.depth);
        Some((what, next))
    }

    pub(super) fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub(super) fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub(super) fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|e| e.0.as_str())
    }

    pub(super) fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|e| e.0.as_str())
    }

    /// Forget everything (the indices the states refer to are no longer valid).
    pub(super) fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    fn trim(v: &mut Vec<(String, T)>, depth: usize) {
        if v.len() > depth {
            let extra = v.len() - depth;
            v.drain(..extra);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_and_redo_walk_through_the_steps() {
        let mut h = History::new(10);
        assert!(!h.can_undo() && !h.can_redo());
        h.push("deleting", vec![1, 2, 3]);
        let (what, prev) = h.undo(vec![1]).unwrap();
        assert_eq!((what.as_str(), &prev), ("deleting", &vec![1, 2, 3]));
        assert!(h.can_redo() && !h.can_undo());
        let (_, next) = h.redo(prev).unwrap();
        assert_eq!(next, vec![1]);
        assert!(h.can_undo() && !h.can_redo());
        assert!(h.undo(vec![]).is_some() && h.undo(vec![]).is_none());
    }

    #[test]
    fn a_new_step_empties_redo() {
        let mut h = History::new(10);
        h.push("a", 1);
        h.undo(2);
        assert!(h.can_redo());
        h.push("b", 1);
        assert!(!h.can_redo());
        assert!(h.redo(0).is_none());
    }

    #[test]
    fn depth_is_limited_and_oldest_goes_first() {
        let mut h = History::new(3);
        for i in 0..10 {
            h.push("x", i);
        }
        let mut seen = vec![];
        let mut cur = 100;
        while let Some((_, s)) = h.undo(cur) {
            seen.push(s);
            cur = s;
        }
        assert_eq!(seen, vec![9, 8, 7]);
        let mut n = 0;
        while h.redo(0).is_some() {
            n += 1;
        }
        assert_eq!(n, 3);
    }

    #[test]
    fn labels_follow_the_top_step() {
        let mut h = History::new(5);
        h.push("one", 1);
        h.push("two", 2);
        assert_eq!(h.undo_label(), Some("two"));
        h.undo(3);
        assert_eq!((h.undo_label(), h.redo_label()), (Some("one"), Some("two")));
        h.clear();
        assert!(!h.can_undo() && !h.can_redo());
    }
}
