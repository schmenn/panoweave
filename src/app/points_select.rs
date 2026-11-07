//! Multiple selection of control points (Shift/Ctrl+click in the photos and in the list).
//!
//! `pe.selected` stays the one "focus" point (matching window, lock button); `pe.sel` holds the
//! extra points of a multiple selection. Indices point into `self.cps`, so the set is dropped
//! whenever the list of points changes in a way that moves them (see `sync_selection`).

use super::*;

/// Cheap fingerprint of the order of the points (photo numbers only, not positions): dragging or
/// locking a point keeps it, deleting, finding or re-ordering points changes it.
pub(super) fn order_fingerprint(cps: &[ControlPoint]) -> u64 {
    cps.iter().fold(cps.len() as u64, |h, c| h.wrapping_mul(0x100000001b3).wrapping_add(((c.a as u64) << 32) ^ c.b as u64 ^ 0x9e3779b97f4a7c15))
}

/// The rows between `anchor` and `k` (both included) in the order the list shows them.
/// Without a usable anchor it is just `k`.
pub(super) fn rows_between(rows: &[usize], anchor: Option<usize>, k: usize) -> Vec<usize> {
    let pos_k = rows.iter().position(|&r| r == k);
    let pos_a = anchor.and_then(|a| rows.iter().position(|&r| r == a));
    match (pos_a, pos_k) {
        (Some(a), Some(b)) => rows[a.min(b)..=a.max(b)].to_vec(),
        _ => vec![k],
    }
}

/// Indices of the points in `idx` that are protected, sorted.
pub(super) fn protected_in(cps: &[ControlPoint], idx: &[usize]) -> Vec<usize> {
    let mut v: Vec<usize> = idx.iter().copied().filter(|&k| cps.get(k).is_some_and(|c| c.locked)).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// "#3, #5" (1-based, as in the list); long lists are cut.
pub(super) fn index_list(idx: &[usize]) -> String {
    let mut s: Vec<String> = idx.iter().take(8).map(|k| format!("#{}", k + 1)).collect();
    if idx.len() > 8 {
        s.push(format!("… (+{})", idx.len() - 8));
    }
    s.join(", ")
}

impl PanoApp {
    /// All selected points: the multiple selection plus the focus point, sorted, valid indices only.
    pub(super) fn selection_indices(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.pe.sel.iter().copied().chain(self.pe.selected).filter(|&k| k < self.cps.len()).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Never let the selection point at other points than the user picked: drops it when the
    /// order of the points changed behind its back (find, clean up, undo, opening a project …).
    pub(super) fn sync_selection(&mut self) {
        let fp = order_fingerprint(&self.cps);
        if fp != self.pe.sel_fp {
            self.pe.sel_fp = fp;
            if !self.pe.sel.is_empty() {
                self.pe.sel.clear();
                self.set_status("The points changed, so the multiple selection was cleared.", MUTED);
            }
        }
        let n = self.cps.len();
        self.pe.sel.retain(|&k| k < n);
    }

    /// Shift/Ctrl+click on point `k`: adds it to the selection or takes it out again.
    pub(super) fn toggle_in_selection(&mut self, k: usize) {
        if k >= self.cps.len() {
            return;
        }
        if self.pe.sel.is_empty() {
            if let Some(s) = self.pe.selected.filter(|&s| s < self.cps.len()) {
                self.pe.sel.insert(s);
            }
        }
        if !self.pe.sel.remove(&k) {
            self.pe.sel.insert(k);
            self.pe.selected = Some(k);
        } else if self.pe.selected == Some(k) {
            self.pe.selected = self.pe.sel.iter().copied().min();
        }
    }

    /// Shift+click in the list: everything between the last clicked row and `k`.
    pub(super) fn select_rows_between(&mut self, rows: &[usize], k: usize) {
        let range = rows_between(rows, self.pe.selected, k);
        self.pe.sel.extend(self.pe.selected.filter(|&s| s < self.cps.len()));
        self.pe.sel.extend(range);
        self.pe.selected = Some(k);
    }

    /// Deletes the whole selection as one undo step; protected points stay and the message names them.
    pub(super) fn delete_selection(&mut self) {
        let idx = self.selection_indices();
        if idx.is_empty() {
            return;
        }
        let prot = protected_in(&self.cps, &idx);
        let n = idx.len() - prot.len();
        let what = if n == 1 { "deleting a point".to_string() } else { format!("deleting {n} selected points") };
        let kept = self.delete_points(idx, &what);
        let mut msg = if n == 0 { "Nothing deleted.".to_string() } else { format!("Deleted {n} point{}. Ctrl+Z brings them back.", if n == 1 { "" } else { "s" }) };
        if kept > 0 {
            msg.push_str(&format!(" {kept} protected point{} kept ({}): unlock with L to delete.", if kept == 1 { "" } else { "s" }, index_list(&prot)));
        }
        self.set_status(msg, if kept > 0 { WARN } else { MUTED });
    }
}

#[cfg(test)]
mod tests {
    use super::super::masks_tests::app_with_photos;
    use super::*;

    fn app(n: usize) -> PanoApp {
        let mut app = app_with_photos(2);
        app.cps = (0..n).map(|i| ControlPoint::new(0, 1, [i as f64 * 0.01, 0.1], [0.2, i as f64 * 0.02], false)).collect();
        app.sync_selection();
        app
    }

    #[test]
    fn rows_between_follows_list_order() {
        let rows = [5, 3, 9, 1];
        assert_eq!(rows_between(&rows, Some(3), 1), vec![3, 9, 1]);
        assert_eq!(rows_between(&rows, Some(1), 5), vec![5, 3, 9, 1]);
        assert_eq!(rows_between(&rows, None, 9), vec![9]);
        assert_eq!(rows_between(&rows, Some(7), 9), vec![9]);
    }

    #[test]
    fn toggle_builds_and_shrinks_the_set() {
        let mut a = app(6);
        a.pe.selected = Some(1);
        a.toggle_in_selection(3);
        a.toggle_in_selection(4);
        assert_eq!(a.selection_indices(), vec![1, 3, 4]);
        a.toggle_in_selection(3);
        assert_eq!(a.selection_indices(), vec![1, 4]);
        a.toggle_in_selection(99); // out of range: ignored
        assert_eq!(a.selection_indices(), vec![1, 4]);
    }

    #[test]
    fn delete_selection_is_one_undo_step_and_keeps_protected() {
        let mut a = app(8);
        a.cps[3].locked = true;
        let orig = a.cps.clone();
        a.pe.selected = Some(1);
        for k in [3, 5, 6] {
            a.toggle_in_selection(k);
        }
        a.delete_selection();
        assert_eq!(a.cps.len(), 5);
        assert!(a.cps.iter().any(|c| c.locked));
        assert!(a.status.contains("#4"), "message names the protected point: {}", a.status);
        assert!(a.pe.sel.is_empty());
        a.undo_last();
        assert_eq!(a.cps.len(), 8);
        assert!(a.cps.iter().zip(&orig).all(|(x, y)| x.pa == y.pa && x.locked == y.locked));
        assert!(!a.undo.can_undo(), "exactly one step");
    }

    #[test]
    fn selection_never_points_at_other_points_after_changes() {
        let mut a = app(6);
        a.pe.selected = Some(0);
        a.toggle_in_selection(4);
        a.sync_selection();
        assert_eq!(a.pe.sel.len(), 2);
        // positions change (drag): stays
        a.cps[4].pa = [0.5, 0.5];
        a.sync_selection();
        assert_eq!(a.pe.sel.len(), 2);
        // the list changes behind the selection's back: dropped
        a.cps.remove(1);
        a.sync_selection();
        assert!(a.pe.sel.is_empty());
        // undo drops it as well
        a.pe.selected = Some(0);
        a.toggle_in_selection(2);
        a.delete_points(vec![4], "deleting a point");
        assert!(a.pe.sel.is_empty());
        a.pe.selected = Some(0);
        a.toggle_in_selection(2);
        a.undo_last();
        assert!(a.pe.sel.is_empty());
    }
}
