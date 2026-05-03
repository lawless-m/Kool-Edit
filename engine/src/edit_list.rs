//! Generic append-only edit list with a history pointer for undo/redo.
//!
//! Per `03-data-model.md` this drives both the per-source destructive history
//! and (later) the project-level multitrack history. The semantics are:
//!
//! - `apply(op)` truncates anything beyond `pointer` (the redo branch is
//!   discarded if you start a new operation after undoing) and appends.
//! - `undo` decrements `pointer`, saturating at 0.
//! - `redo` increments `pointer`, saturating at `len()`.
//! - `flatten` is signalled by `truncate_history`; the caller is responsible
//!   for actually rendering samples — this struct just owns the operation
//!   journal.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EditList<Op> {
    ops: Vec<Op>,
    pointer: usize,
}

impl<Op> Default for EditList<Op> {
    fn default() -> Self {
        Self {
            ops: Vec::new(),
            pointer: 0,
        }
    }
}

impl<Op> EditList<Op> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Number of ops currently in effect (i.e. before the history pointer).
    pub fn pointer(&self) -> usize {
        self.pointer
    }

    pub fn can_undo(&self) -> bool {
        self.pointer > 0
    }

    pub fn can_redo(&self) -> bool {
        self.pointer < self.ops.len()
    }

    /// Iterate over the ops currently in effect, oldest first.
    pub fn active(&self) -> impl Iterator<Item = &Op> {
        self.ops[..self.pointer].iter()
    }

    /// Append a new op, discarding any redo branch.
    pub fn apply(&mut self, op: Op) {
        self.ops.truncate(self.pointer);
        self.ops.push(op);
        self.pointer = self.ops.len();
    }

    /// Move the pointer back one step. Returns the op that was undone, if any.
    pub fn undo(&mut self) -> Option<&Op> {
        if !self.can_undo() {
            return None;
        }
        self.pointer -= 1;
        self.ops.get(self.pointer)
    }

    /// Move the pointer forward one step. Returns the op that was redone.
    pub fn redo(&mut self) -> Option<&Op> {
        if !self.can_redo() {
            return None;
        }
        let op = &self.ops[self.pointer];
        self.pointer += 1;
        Some(op)
    }

    /// Discard the entire history. Used by `flatten` once samples have been
    /// baked into a new base file.
    pub fn truncate_history(&mut self) {
        self.ops.clear();
        self.pointer = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_appends_and_advances_pointer() {
        let mut list: EditList<u32> = EditList::new();
        list.apply(1);
        list.apply(2);
        list.apply(3);
        assert_eq!(list.len(), 3);
        assert_eq!(list.pointer(), 3);
        assert_eq!(list.active().copied().collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn undo_then_redo_restores_state() {
        let mut list: EditList<u32> = EditList::new();
        list.apply(1);
        list.apply(2);
        assert_eq!(list.undo(), Some(&2));
        assert_eq!(list.pointer(), 1);
        assert_eq!(list.redo(), Some(&2));
        assert_eq!(list.pointer(), 2);
        assert!(!list.can_redo());
    }

    #[test]
    fn apply_after_undo_truncates_redo_branch() {
        let mut list: EditList<u32> = EditList::new();
        list.apply(1);
        list.apply(2);
        list.apply(3);
        list.undo();
        list.undo();
        assert_eq!(list.pointer(), 1);
        list.apply(99);
        assert_eq!(list.len(), 2);
        assert_eq!(list.pointer(), 2);
        assert_eq!(list.active().copied().collect::<Vec<_>>(), vec![1, 99]);
        assert!(!list.can_redo());
    }

    #[test]
    fn undo_past_start_is_noop() {
        let mut list: EditList<u32> = EditList::new();
        assert_eq!(list.undo(), None);
        list.apply(1);
        list.undo();
        assert_eq!(list.undo(), None);
        assert_eq!(list.pointer(), 0);
    }

    #[test]
    fn redo_past_end_is_noop() {
        let mut list: EditList<u32> = EditList::new();
        list.apply(1);
        assert_eq!(list.redo(), None);
        assert_eq!(list.pointer(), 1);
    }

    #[test]
    fn truncate_history_clears_everything() {
        let mut list: EditList<u32> = EditList::new();
        list.apply(1);
        list.apply(2);
        list.undo();
        list.truncate_history();
        assert_eq!(list.len(), 0);
        assert_eq!(list.pointer(), 0);
        assert!(!list.can_undo());
        assert!(!list.can_redo());
    }
}
