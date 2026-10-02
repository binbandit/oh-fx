use super::Composer;
use super::edit_history::Entry;
use super::registered_entities::Entities;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Undo,
    Redo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Transition {
    start: usize,
    end: usize,
    replacement: String,
    cursor_after: usize,
}

fn plan(
    input: &str,
    entities: &Entities,
    entry: &Entry,
    direction: Direction,
) -> Option<Transition> {
    let (expected, replacement, cursor_after) = match direction {
        Direction::Undo => (&entry.inserted, &entry.removed, entry.cursor_before),
        Direction::Redo => (&entry.removed, &entry.inserted, entry.cursor_after),
    };
    let end = entry.start.checked_add(expected.len())?;
    if input.get(entry.start..end) != Some(expected.as_str()) {
        return None;
    }
    let structured = if entry.start < end {
        entities.entity_overlapping(entry.start, end).is_some()
    } else {
        entities.entity_containing(input, entry.start).is_some()
    };
    if structured {
        return None;
    }
    Some(Transition {
        start: entry.start,
        end,
        replacement: replacement.clone(),
        cursor_after,
    })
}

impl Composer {
    pub(crate) fn undo(&mut self) -> bool {
        self.step_history(Direction::Undo)
    }

    pub(crate) fn redo(&mut self) -> bool {
        self.step_history(Direction::Redo)
    }

    fn step_history(&mut self, direction: Direction) -> bool {
        let entry = match direction {
            Direction::Undo => self.edit_history.peek_undo(),
            Direction::Redo => self.edit_history.peek_redo(),
        };
        let Some(entry) = entry else {
            return false;
        };
        let Some(transition) = plan(&self.edit.input, &self.entities, entry, direction) else {
            self.edit_history.reset();
            return false;
        };
        self.apply_history_transition(&transition);
        match direction {
            Direction::Undo => self.edit_history.commit_undo(),
            Direction::Redo => self.edit_history.commit_redo(),
        }
        true
    }

    fn apply_history_transition(&mut self, transition: &Transition) {
        self.entities.discard_pending_separator();
        if transition.start < transition.end {
            self.entities
                .adjust_for_delete(transition.start, transition.end);
            self.edit
                .delete_text_range(transition.start, transition.end);
        }
        self.edit.set_cursor(transition.start);
        self.insert_slice_without_history(&transition.replacement);
        self.edit.set_cursor(transition.cursor_after);
        self.vertical.reset();
        self.limit_rejection.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::super::edit_history::EditHistory;
    use super::super::entity_spans::Span;
    use super::super::pasted_blocks::PastedBlock;
    use super::super::test_fixture::{replace_text, select};
    use super::*;
    use crate::input::TextOwner;

    fn record(
        composer: &mut Composer,
        start: usize,
        removed: &str,
        inserted: &str,
        cursor_before: usize,
        cursor_after: usize,
    ) {
        composer.edit_history.commit(EditHistory::prepare(
            start,
            removed,
            inserted,
            cursor_before,
            cursor_after,
        ));
    }

    #[test]
    fn composer_undo_and_redo_apply_the_recorded_transition_and_reset_edit_policy() {
        let mut composer = Composer::new();
        replace_text(&mut composer, "ab ");
        record(&mut composer, 1, "", "b", 1, 2);
        composer.vertical.preferred_column = Some(1);
        composer.note_limit_rejection(TextOwner::Composer);

        assert!(composer.undo());
        assert_eq!(composer.text(), "a ");
        assert_eq!(composer.cursor(), 1);
        assert_eq!(composer.edit_history.peek_undo(), None);
        assert!(composer.edit_history.peek_redo().is_some());
        assert_eq!(composer.preferred_column(), None);
        assert!(composer.note_limit_rejection(TextOwner::Composer));

        assert!(composer.redo());
        assert_eq!(composer.text(), "ab ");
        assert_eq!(composer.cursor(), 2);
        assert!(composer.edit_history.peek_undo().is_some());
        assert_eq!(composer.edit_history.peek_redo(), None);
    }

    #[test]
    fn composer_undo_rejects_stale_bytes_and_clears_both_history_directions() {
        let mut composer = Composer::new();
        replace_text(&mut composer, "changed");
        record(&mut composer, 0, "", "recorded", 0, "recorded".len());
        assert!(!composer.undo());
        assert_eq!(composer.text(), "changed");
        assert_eq!(composer.edit_history.peek_undo(), None);
        assert_eq!(composer.edit_history.peek_redo(), None);
    }

    #[test]
    fn composer_undo_rejects_registered_entities_without_mutating_text() {
        let placeholder = "[Pasted text #1, 1 line]";
        let mut composer = Composer::new();
        replace_text(&mut composer, placeholder);
        record(&mut composer, 0, "", placeholder, 0, placeholder.len());
        composer.entities.register_pasted_block(PastedBlock {
            id: 1,
            text: "pasted".to_owned(),
            line_count: 1,
            span: Span::new(0, placeholder.len()),
        });
        assert!(!composer.undo());
        assert_eq!(composer.text(), placeholder);
        assert_eq!(composer.entities.pasted_blocks.len(), 1);
        assert_eq!(composer.edit_history.peek_undo(), None);
        assert_eq!(composer.edit_history.peek_redo(), None);
    }

    #[test]
    fn text_edits_undo_and_redo_without_snapshotting_structured_state() {
        let mut composer = Composer::new();
        composer.insert_slice("a");
        composer.insert_slice("b");
        assert!(composer.undo());
        assert_eq!(composer.text(), "a");
        assert!(composer.redo());
        assert_eq!(composer.text(), "ab");

        select(&mut composer, 0, 2);
        assert!(composer.delete_selection());
        assert!(composer.undo());
        assert_eq!(composer.text(), "ab");
    }
}
