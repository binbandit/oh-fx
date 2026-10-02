const MAX_ENTRIES: usize = 100;
const MAX_RETAINED_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) start: usize,
    pub(crate) removed: String,
    pub(crate) inserted: String,
    pub(crate) cursor_before: usize,
    pub(crate) cursor_after: usize,
}

impl Entry {
    fn retained_bytes(&self) -> usize {
        self.removed.len().saturating_add(self.inserted.len())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Prepared {
    Record(Entry),
    Boundary,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EditHistory {
    undo_stack: Vec<Entry>,
    redo_stack: Vec<Entry>,
    retained_bytes: usize,
}

impl EditHistory {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn prepare(
        start: usize,
        removed: &str,
        inserted: &str,
        cursor_before: usize,
        cursor_after: usize,
    ) -> Prepared {
        if removed.len().saturating_add(inserted.len()) > MAX_RETAINED_BYTES {
            return Prepared::Boundary;
        }
        Prepared::Record(Entry {
            start,
            removed: removed.to_owned(),
            inserted: inserted.to_owned(),
            cursor_before,
            cursor_after,
        })
    }

    pub(crate) fn commit(&mut self, prepared: Prepared) {
        let Prepared::Record(entry) = prepared else {
            self.reset();
            return;
        };
        self.drop_redo();
        self.retained_bytes = self.retained_bytes.saturating_add(entry.retained_bytes());
        self.undo_stack.push(entry);
        while self.undo_stack.len() > MAX_ENTRIES || self.retained_bytes > MAX_RETAINED_BYTES {
            let dropped = self.undo_stack.remove(0);
            self.retained_bytes = self.retained_bytes.saturating_sub(dropped.retained_bytes());
        }
    }

    pub(crate) fn peek_undo(&self) -> Option<&Entry> {
        self.undo_stack.last()
    }

    pub(crate) fn peek_redo(&self) -> Option<&Entry> {
        self.redo_stack.last()
    }

    pub(crate) fn commit_undo(&mut self) {
        if let Some(entry) = self.undo_stack.pop() {
            self.redo_stack.push(entry);
        }
    }

    pub(crate) fn commit_redo(&mut self) {
        if let Some(entry) = self.redo_stack.pop() {
            self.undo_stack.push(entry);
        }
    }

    fn drop_redo(&mut self) {
        for entry in self.redo_stack.drain(..) {
            self.retained_bytes = self.retained_bytes.saturating_sub(entry.retained_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_edit_history_moves_owned_deltas_between_undo_and_redo() {
        let mut history = EditHistory::default();
        history.commit(EditHistory::prepare(2, "old", "new", 5, 5));
        assert_eq!(history.peek_undo().unwrap().removed, "old");

        history.commit_undo();
        assert_eq!(history.peek_undo(), None);
        assert_eq!(history.peek_redo().unwrap().inserted, "new");

        history.commit_redo();
        assert_eq!(history.peek_redo(), None);
        assert_eq!(history.undo_stack.len(), 1);
    }

    #[test]
    fn structured_boundaries_discard_both_edit_directions() {
        let mut history = EditHistory::default();
        history.commit(EditHistory::prepare(0, "", "a", 0, 1));
        history.commit_undo();
        history.commit(Prepared::Boundary);
        assert_eq!(history.peek_undo(), None);
        assert_eq!(history.peek_redo(), None);
        assert_eq!(history.retained_bytes, 0);
    }

    #[test]
    fn history_keeps_only_the_newest_hundred_entries() {
        let mut history = EditHistory::default();
        for index in 0..=MAX_ENTRIES {
            history.commit(EditHistory::prepare(index, "", "x", index, index + 1));
        }
        assert_eq!(history.undo_stack.len(), MAX_ENTRIES);
        assert_eq!(history.undo_stack[0].start, 1);
        assert_eq!(history.retained_bytes, MAX_ENTRIES);
    }

    #[test]
    fn oversized_edits_become_history_boundaries() {
        let large = "x".repeat(MAX_RETAINED_BYTES + 1);
        assert_eq!(
            EditHistory::prepare(0, "", &large, 0, 0),
            Prepared::Boundary
        );
    }
}
