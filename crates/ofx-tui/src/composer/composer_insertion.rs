use super::Composer;
use super::edit_history::{EditHistory, Prepared};
use super::editor_state::{InsertResult, can_insert, can_replace};
use super::pasted_blocks::{expanded_len, expanded_range_len};

impl Composer {
    pub(crate) fn can_insert(&self, inserted_len: usize, max_len: usize) -> bool {
        expanded_len(&self.edit.input, &self.entities.pasted_blocks)
            .is_some_and(|current_len| can_insert(current_len, inserted_len, max_len))
    }

    pub(crate) fn can_replace_range(
        &self,
        replace_start: usize,
        replace_end: usize,
        inserted_len: usize,
        max_len: usize,
    ) -> bool {
        let blocks = &self.entities.pasted_blocks;
        let Some(current_len) = expanded_len(&self.edit.input, blocks) else {
            return false;
        };
        expanded_range_len(&self.edit.input, blocks, replace_start, replace_end).is_some_and(
            |replaced_len| can_replace(current_len, replaced_len, inserted_len, max_len),
        )
    }

    pub(crate) fn insert_slice(&mut self, text: &str) {
        if text.is_empty() || self.claim_pending_separator(text) {
            return;
        }
        let insert_start = self.edit.cursor;
        let prepared = if self
            .entities
            .entity_containing(&self.edit.input, insert_start)
            .is_some()
        {
            Prepared::Boundary
        } else {
            EditHistory::prepare(
                insert_start,
                "",
                text,
                self.edit.cursor,
                insert_start + text.len(),
            )
        };
        self.vertical.reset();
        self.insert_slice_without_history(text);
        self.edit_history.commit(prepared);
    }

    pub(crate) fn insert_slice_bounded(&mut self, text: &str, max_len: usize) -> InsertResult {
        if !self.can_insert(text.len(), max_len) {
            let pending = self.entities.pending_separator;
            if self.claim_pending_separator(text) {
                self.limit_rejection.clear();
                return InsertResult::Inserted;
            }
            self.entities.pending_separator = pending;
            return InsertResult::LimitExceeded;
        }
        self.insert_slice(text);
        self.limit_rejection.clear();
        InsertResult::Inserted
    }

    pub(crate) fn insert_slice_without_history(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let insert_start = self.edit.cursor;
        self.entities.discard_pending_separator();
        self.entities.remove_skill_token_containing(insert_start);
        self.edit.insert_str(text);
        self.entities.shift_for_insert(insert_start, text.len());
    }

    fn claim_pending_separator(&mut self, text: &str) -> bool {
        if !self
            .entities
            .claim_pending_separator(&self.edit.input, self.edit.cursor, text)
        {
            return false;
        }
        self.vertical.reset();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::entity_spans::Span;
    use super::super::pasted_blocks::PastedBlock;
    use super::super::test_fixture::replace_text;
    use super::*;
    use crate::input::TextOwner;

    #[test]
    fn bounded_insertion_rejects_before_applying_the_successful_transition() {
        let placeholder = "[Pasted text #1, 1 line]";
        let mut composer = Composer::new();
        replace_text(&mut composer, placeholder);
        composer.entities.register_pasted_block(PastedBlock {
            id: 1,
            text: "expanded".to_owned(),
            line_count: 1,
            span: Span::new(0, placeholder.len()),
        });
        composer.vertical.preferred_column = Some(7);
        composer.note_limit_rejection(TextOwner::Composer);

        assert_eq!(
            composer.insert_slice_bounded("!", "expanded".len()),
            InsertResult::LimitExceeded
        );
        assert_eq!(composer.text(), placeholder);
        assert_eq!(composer.preferred_column(), Some(7));
        assert!(!composer.note_limit_rejection(TextOwner::Composer));

        assert_eq!(
            composer.insert_slice_bounded("!", "expanded!".len()),
            InsertResult::Inserted
        );
        assert_eq!(composer.text(), format!("{placeholder}!"));
        assert_eq!(composer.preferred_column(), None);
        assert!(composer.note_limit_rejection(TextOwner::Composer));
        assert_eq!(composer.expanded_len(), Some("expanded!".len()));
    }

    #[test]
    fn composer_insertion_inserts_at_cursor_and_advances() {
        let mut composer = Composer::new();
        composer.insert_slice("abc");
        assert_eq!(composer.text(), "abc");
        assert_eq!(composer.cursor(), 3);
        composer.edit.set_cursor(2);
        composer.insert_slice("xy");
        assert_eq!(composer.text(), "abxyc");
        assert_eq!(composer.cursor(), 4);
    }
}
