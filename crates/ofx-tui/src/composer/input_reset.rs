use super::Composer;
use super::kill_ring::KillRing;
use super::registered_entities::Entities;

impl Composer {
    pub(crate) fn clear(&mut self) {
        self.vertical.reset();
        self.edit.discard_selection();
        self.limit_rejection.clear();
        self.edit.clear();
        self.entities.pasted_blocks.clear();
        self.prompt_history.reset_navigation();
        self.edit_history.reset();
    }

    pub(crate) fn reset_for_session(&mut self) {
        self.kill_ring = KillRing::default();
        if let Some(next_paste_id) =
            Entities::default().next_paste_id_after(&self.entities.pasted_blocks)
        {
            self.entities.next_paste_id = next_paste_id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::entity_spans::Span;
    use super::super::input_limit_rejection::LimitRejection;
    use super::super::kill_ring::YankResult;
    use super::super::pasted_blocks::PastedBlock;
    use super::super::test_fixture::{replace_text, select};
    use super::super::{HistoryNavigation, InsertResult};
    use super::*;
    use crate::input::TextOwner;

    fn pasted_block(id: usize, text: &str, span: Span) -> PastedBlock {
        PastedBlock {
            id,
            text: text.to_owned(),
            line_count: 1,
            span,
        }
    }

    #[test]
    fn clearing_the_current_input_keeps_the_kill_ring_and_paste_ids() {
        let mut composer = Composer::new();
        replace_text(&mut composer, "draft");
        composer.kill_ring.text.push_str("killed");
        composer.entities.next_paste_id = 3;

        composer.clear();

        assert!(composer.is_empty());
        assert_eq!(composer.cursor(), 0);
        assert_eq!(composer.kill_ring_text(), "killed");
        assert_eq!(composer.entities.next_paste_id, 3);
    }

    #[test]
    fn session_input_reset_clears_transient_state_and_preserves_prompt_history() {
        let mut composer = Composer::new();
        composer.prompt_history.record(usize::MAX, "one", &[]);
        composer.prompt_history.record(usize::MAX, "two", &[]);
        replace_text(&mut composer, "draft");
        assert_eq!(
            composer.navigate_history(-1, 4096),
            HistoryNavigation::Moved
        );
        assert_eq!(composer.insert_text("!", 4096), InsertResult::Inserted);
        select(&mut composer, 0, "two!".len());
        composer
            .entities
            .register_pasted_block(pasted_block(7, "pasted backing", Span::new(0, 3)));
        composer.kill_ring.text.push_str("killed draft");
        composer.entities.next_paste_id = 8;
        assert!(composer.limit_rejection.begin(TextOwner::Composer));
        composer.vertical.preferred_column = Some(4);
        assert!(composer.selection().is_some());
        assert!(composer.edit_history.peek_undo().is_some());

        composer.clear();
        composer.reset_for_session();

        assert_eq!(composer.text(), "");
        assert_eq!(composer.selection(), None);
        assert!(composer.entities.pasted_blocks.is_empty());
        assert_eq!(composer.kill_ring_text(), "");
        assert_eq!(composer.edit_history.peek_undo(), None);
        assert_eq!(composer.entities.next_paste_id, 1);
        assert_eq!(composer.limit_rejection, LimitRejection::default());
        assert_eq!(composer.preferred_column(), None);
        assert_eq!(composer.prompt_history.active_index(), None);
        assert_eq!(
            composer.navigate_history(-1, 4096),
            HistoryNavigation::Moved
        );
        assert_eq!(composer.text(), "two");
        assert_eq!(
            composer.navigate_history(-1, 4096),
            HistoryNavigation::Moved
        );
        assert_eq!(composer.text(), "one");
        assert_eq!(
            composer.navigate_history(-1, 4096),
            HistoryNavigation::Unchanged
        );
    }

    #[test]
    fn a_session_reset_keeps_the_draft_and_restarts_paste_ids() {
        let mut composer = Composer::new();
        replace_text(&mut composer, "draft");
        composer.kill_ring.text.push_str("killed");
        composer.entities.next_paste_id = 3;

        composer.reset_for_session();

        assert_eq!(composer.kill_ring_text(), "");
        assert_eq!(composer.yank(usize::MAX), YankResult::Inactive);
        assert_eq!(composer.text(), "draft");
        assert_eq!(composer.entities.next_paste_id, 1);
    }

    #[test]
    fn a_session_reset_numbers_new_pastes_after_those_still_in_the_draft() {
        let mut composer = Composer::new();
        let placeholder = "[Pasted text #4, 2 lines]";
        replace_text(&mut composer, placeholder);
        composer.entities.register_pasted_block(PastedBlock {
            line_count: 2,
            ..pasted_block(4, "one\ntwo", Span::new(0, placeholder.len()))
        });
        composer.entities.next_paste_id = 5;

        composer.reset_for_session();

        assert_eq!(composer.expanded_text(), "one\ntwo");
        assert_eq!(composer.entities.next_paste_id, 5);
    }
}
