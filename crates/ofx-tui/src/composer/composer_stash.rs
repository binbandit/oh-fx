use std::mem;

use super::Composer;
use super::composer_history::Navigation;
use super::edit_history::EditHistory;
use super::editor_state::EditorState;
use super::registered_entities::Entities;
use super::vertical_navigation::VerticalNavigation;

#[derive(Debug, Default)]
pub(crate) struct ComposerStash {
    edit: EditorState,
    entities: Entities,
    edit_history: EditHistory,
    vertical: VerticalNavigation,
    navigation: Navigation,
}

impl Composer {
    pub(crate) fn stash(&mut self) -> ComposerStash {
        let revision = self.edit.revision.wrapping_add(1);
        let stash = ComposerStash {
            edit: mem::take(&mut self.edit),
            entities: mem::take(&mut self.entities),
            edit_history: mem::take(&mut self.edit_history),
            vertical: mem::take(&mut self.vertical),
            navigation: self.prompt_history.take_navigation(),
        };
        self.edit.revision = revision;
        self.settle_borrowed_state();
        stash
    }

    pub(crate) fn restore(&mut self, stash: ComposerStash) {
        let revision = self.edit.revision.wrapping_add(1);
        self.edit = stash.edit;
        self.edit.revision = revision;
        self.entities = stash.entities;
        self.edit_history = stash.edit_history;
        self.vertical = stash.vertical;
        self.prompt_history.restore_navigation(stash.navigation);
        self.settle_borrowed_state();
    }

    fn settle_borrowed_state(&mut self) {
        self.auto_separator = None;
        self.limit_rejection.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::super::HistoryNavigation;
    use super::super::test_fixture::{replace_text, select};
    use super::*;

    #[test]
    fn a_stash_round_trip_returns_the_draft_with_its_cursor_selection_and_entities() {
        let mut composer = Composer::new();
        composer.insert_paste(&"line\n".repeat(300), usize::MAX);
        composer.insert_text(" tail", usize::MAX);
        select(&mut composer, 2, 6);
        let draft = composer.text().to_owned();
        let blocks = composer.entities.pasted_blocks.clone();
        assert_eq!(blocks.len(), 1);
        let before = composer.edit_revision();
        let stash = composer.stash();
        assert!(composer.is_empty());
        assert!(composer.entities.pasted_blocks.is_empty());
        assert_ne!(composer.edit_revision(), before);
        composer.insert_text("/model gpt", usize::MAX);
        composer.restore(stash);
        assert_eq!(composer.text(), draft);
        assert_eq!(composer.cursor(), 6);
        assert_eq!(composer.selection().map(|range| range.start), Some(2));
        assert_eq!(composer.entities.pasted_blocks, blocks);
        assert!(composer.expanded_text().contains("line\nline"));
    }

    #[test]
    fn replacing_the_text_drops_the_entities_it_held() {
        let mut composer = Composer::new();
        composer.insert_paste(&"line\n".repeat(300), usize::MAX);
        composer.bind_skill_token(0, 0, "review", std::path::Path::new("/skills/review"));
        assert!(!composer.entities.pasted_blocks.is_empty());
        composer.replace_text("/model gpt-5 ");
        assert_eq!(composer.expanded_text(), "/model gpt-5 ");
        assert!(composer.entities.pasted_blocks.is_empty());
        assert!(composer.entities.skill_tokens.is_empty());
        assert!(composer.skill_bindings().is_empty());
    }

    #[test]
    fn a_stashed_draft_keeps_its_undo_history_and_history_position() {
        let mut composer = Composer::new();
        composer.install_history(vec!["older".to_owned(), "newer".to_owned()]);
        replace_text(&mut composer, "draft");
        assert_eq!(
            composer.navigate_history(-1, usize::MAX),
            HistoryNavigation::Moved
        );
        composer.insert_text("!", usize::MAX);
        let stash = composer.stash();
        assert_eq!(
            composer.navigate_history(1, usize::MAX),
            HistoryNavigation::Unchanged
        );
        composer.restore(stash);
        assert_eq!(composer.text(), "newer!");
        assert!(composer.undo());
        assert_eq!(composer.text(), "newer");
        assert_eq!(
            composer.navigate_history(1, usize::MAX),
            HistoryNavigation::Moved
        );
        assert_eq!(composer.text(), "draft");
    }
}
