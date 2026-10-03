use super::Composer;
use super::pasted_blocks::{PastedBlock, expanded_len};
use super::registered_entities::{Entities, SkillToken};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryNavigation {
    Unchanged,
    LimitExceeded(usize),
    Moved,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Snapshot {
    text: String,
    pasted_blocks: Vec<PastedBlock>,
    skill_tokens: Vec<SkillToken>,
}

impl Snapshot {
    fn of(text: &str, entities: &Entities) -> Self {
        Self {
            text: text.to_owned(),
            pasted_blocks: entities.pasted_blocks.clone(),
            skill_tokens: entities.skill_tokens.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavigationTarget {
    Unchanged,
    Entry(usize),
    Draft,
}

fn navigation_target(index: Option<usize>, entry_count: usize, delta: i32) -> NavigationTarget {
    if entry_count == 0 || index.is_some_and(|current| current >= entry_count) {
        return NavigationTarget::Unchanged;
    }
    if delta < 0 {
        return match index {
            Some(0) => NavigationTarget::Unchanged,
            Some(current) => NavigationTarget::Entry(current - 1),
            None => NavigationTarget::Entry(entry_count - 1),
        };
    }
    match index {
        None => NavigationTarget::Unchanged,
        Some(current) if current + 1 < entry_count => NavigationTarget::Entry(current + 1),
        Some(_) => NavigationTarget::Draft,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PromptHistory {
    entries: Vec<Snapshot>,
    draft: Option<Snapshot>,
    index: Option<usize>,
}

impl PromptHistory {
    #[cfg(test)]
    pub(crate) fn active_index(&self) -> Option<usize> {
        self.index
    }

    #[cfg(test)]
    pub(crate) fn draft_text(&self) -> Option<&str> {
        self.draft.as_ref().map(|draft| draft.text.as_str())
    }

    fn install_text_entries(&mut self, entries: Vec<String>) {
        self.entries = entries
            .into_iter()
            .map(|text| Snapshot {
                text,
                ..Snapshot::default()
            })
            .collect();
        self.reset_navigation();
    }

    fn record(&mut self, max_entries: usize, entry: Snapshot) {
        if self.entries.last() == Some(&entry) {
            return;
        }
        self.entries.push(entry);
        let excess = self.entries.len().saturating_sub(max_entries);
        self.entries.drain(..excess);
    }

    pub(crate) fn record_text(&mut self, max_entries: usize, text: &str) {
        self.record(max_entries, Snapshot::of(text, &Entities::default()));
    }

    pub(crate) fn reset_navigation(&mut self) {
        self.index = None;
        self.draft = None;
    }
}

impl Composer {
    pub(crate) fn navigate_history(&mut self, delta: i32, max_len: usize) -> HistoryNavigation {
        let history = &self.prompt_history;
        let target = navigation_target(history.index, history.entries.len(), delta);
        let entering_history = history.index.is_none();
        let prepared = match target {
            NavigationTarget::Unchanged => return HistoryNavigation::Unchanged,
            NavigationTarget::Entry(index) => history.entries[index].clone(),
            NavigationTarget::Draft => match &history.draft {
                Some(draft) => draft.clone(),
                None => return HistoryNavigation::Unchanged,
            },
        };

        let recalled_len =
            expanded_len(&prepared.text, &prepared.pasted_blocks).unwrap_or(usize::MAX);
        if recalled_len > max_len {
            return HistoryNavigation::LimitExceeded(recalled_len);
        }
        let Some(next_paste_id) = self.entities.next_paste_id_after(&prepared.pasted_blocks) else {
            return HistoryNavigation::Unchanged;
        };

        if entering_history {
            self.prompt_history.draft = Some(Snapshot::of(&self.edit.input, &self.entities));
        }
        self.replace_active_composer(prepared);
        self.entities.next_paste_id = next_paste_id;
        self.edit_history.reset();

        match target {
            NavigationTarget::Entry(index) => self.prompt_history.index = Some(index),
            NavigationTarget::Draft | NavigationTarget::Unchanged => {
                self.prompt_history.reset_navigation();
            }
        }
        HistoryNavigation::Moved
    }

    pub(crate) fn browsing_history(&self) -> bool {
        self.prompt_history.index.is_some()
    }

    pub(crate) fn restore_text(&mut self, text: String) {
        self.replace_active_composer(Snapshot {
            text,
            ..Snapshot::default()
        });
        self.edit_history.reset();
    }

    pub(crate) fn install_history(&mut self, entries: Vec<String>) {
        self.prompt_history.install_text_entries(entries);
    }

    pub(crate) fn record_history(&mut self, max_entries: usize) {
        self.prompt_history
            .record(max_entries, Snapshot::of(&self.edit.input, &self.entities));
    }

    pub(crate) fn record_text_history(&mut self, max_entries: usize, text: &str) {
        self.prompt_history.record_text(max_entries, text);
    }

    fn replace_active_composer(&mut self, snapshot: Snapshot) {
        self.vertical.reset();
        self.edit.discard_selection();
        let mut text = snapshot.text;
        self.edit.swap_input(&mut text);
        self.entities.pasted_blocks = snapshot.pasted_blocks;
        self.entities.skill_tokens = snapshot.skill_tokens;
        self.entities.discard_pending_separator();
        self.limit_rejection.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::super::entity_spans::Span;
    use super::super::pasted_blocks::{LARGE_PASTE_CHAR_THRESHOLD, format_placeholder};
    use super::super::test_fixture::replace_text;
    use super::*;
    use crate::input::TextOwner;

    fn entry(text: &str, pasted_blocks: &[PastedBlock]) -> Snapshot {
        Snapshot {
            text: text.to_owned(),
            pasted_blocks: pasted_blocks.to_vec(),
            skill_tokens: Vec::new(),
        }
    }

    fn paste_block(id: usize, backing: &str, start: usize) -> PastedBlock {
        PastedBlock {
            id,
            text: backing.to_owned(),
            line_count: 1,
            span: Span::new(start, start + format_placeholder(id, 1).len()),
        }
    }

    fn navigate(composer: &mut Composer, delta: i32) -> HistoryNavigation {
        composer.navigate_history(delta, usize::MAX)
    }

    fn install(composer: &mut Composer, entries: &[&str]) {
        for text in entries {
            composer.prompt_history.record(usize::MAX, entry(text, &[]));
        }
    }

    fn entry_texts(history: &PromptHistory) -> Vec<&str> {
        history
            .entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect()
    }

    #[test]
    fn navigation_target_follows_bounded_older_and_newer_history_laws() {
        assert_eq!(navigation_target(None, 0, -1), NavigationTarget::Unchanged);
        assert_eq!(navigation_target(None, 3, -1), NavigationTarget::Entry(2));
        assert_eq!(
            navigation_target(Some(0), 3, -1),
            NavigationTarget::Unchanged
        );
        assert_eq!(navigation_target(Some(0), 3, 1), NavigationTarget::Entry(1));
        assert_eq!(navigation_target(Some(2), 3, 1), NavigationTarget::Draft);
        assert_eq!(navigation_target(None, 3, 1), NavigationTarget::Unchanged);
        assert_eq!(
            navigation_target(Some(3), 3, -1),
            NavigationTarget::Unchanged
        );
    }

    #[test]
    fn record_deduplicates_adjacent_entries_and_prunes_the_oldest() {
        let mut history = PromptHistory::default();
        history.record(2, entry("one", &[]));
        history.record(2, entry("one", &[]));
        history.record(2, entry("two", &[]));
        history.record(2, entry("three", &[]));
        assert_eq!(entry_texts(&history), ["two", "three"]);
    }

    #[test]
    fn navigation_recalls_entries_and_restores_the_unsent_draft() {
        let mut composer = Composer::new();
        install(&mut composer, &["older", "newer"]);
        replace_text(&mut composer, "unsent draft");
        composer.vertical.preferred_column = Some(4);
        composer.note_limit_rejection(TextOwner::Composer);

        assert_eq!(navigate(&mut composer, -1), HistoryNavigation::Moved);
        assert_eq!(composer.text(), "newer");
        assert_eq!(composer.prompt_history.draft_text(), Some("unsent draft"));
        assert_eq!(composer.prompt_history.index, Some(1));
        assert_eq!(composer.preferred_column(), None);
        assert!(composer.note_limit_rejection(TextOwner::Composer));

        assert_eq!(navigate(&mut composer, 1), HistoryNavigation::Moved);
        assert_eq!(composer.text(), "unsent draft");
        assert_eq!(composer.prompt_history.draft_text(), None);
        assert_eq!(composer.prompt_history.index, None);
    }

    #[test]
    fn oversized_recall_leaves_active_composer_and_history_position_unchanged() {
        let mut composer = Composer::new();
        install(&mut composer, &["oversized"]);
        replace_text(&mut composer, "draft");
        composer.vertical.preferred_column = Some(2);
        assert_eq!(
            composer.navigate_history(-1, 4),
            HistoryNavigation::LimitExceeded("oversized".len())
        );
        assert_eq!(composer.text(), "draft");
        assert_eq!(composer.prompt_history.index, None);
        assert_eq!(composer.prompt_history.draft_text(), None);
        assert_eq!(composer.preferred_column(), Some(2));
    }

    #[test]
    fn prompt_history_recalls_previous_inputs_with_up_and_down() {
        let mut composer = Composer::new();
        composer
            .prompt_history
            .record(100, entry("first prompt", &[]));
        composer
            .prompt_history
            .record(100, entry("second prompt", &[]));
        replace_text(&mut composer, "draft");

        navigate(&mut composer, -1);
        assert_eq!(composer.text(), "second prompt");
        navigate(&mut composer, -1);
        assert_eq!(composer.text(), "first prompt");
        navigate(&mut composer, 1);
        assert_eq!(composer.text(), "second prompt");
        navigate(&mut composer, 1);
        assert_eq!(composer.text(), "draft");
    }

    #[test]
    fn prompt_history_recall_restores_compact_paste_backing() {
        let placeholder = format_placeholder(1, 1);
        let backing = "x".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1);
        let mut composer = Composer::new();
        replace_text(&mut composer, &placeholder);
        composer
            .entities
            .register_pasted_block(paste_block(1, &backing, 0));
        composer.record_history(100);
        composer.clear();

        navigate(&mut composer, -1);
        assert_eq!(composer.text(), placeholder);
        assert_eq!(composer.entities.pasted_blocks.len(), 1);
        assert_eq!(composer.entities.pasted_blocks[0].text, backing);
        assert_eq!(composer.expanded_text(), backing);
        assert_eq!(composer.entities.next_paste_id, 2);
    }

    #[test]
    fn prompt_history_semantic_dedupe_distinguishes_paste_provenance() {
        let placeholder = format_placeholder(4, 1);
        let mut history = PromptHistory::default();
        history.record(
            100,
            entry(&placeholder, &[paste_block(4, "first backing", 0)]),
        );
        history.record(
            100,
            entry(&placeholder, &[paste_block(4, "first backing", 0)]),
        );
        history.record(
            100,
            entry(&placeholder, &[paste_block(4, "second backing", 0)]),
        );
        let backings: Vec<&str> = history
            .entries
            .iter()
            .map(|entry| entry.pasted_blocks[0].text.as_str())
            .collect();
        assert_eq!(backings, ["first backing", "second backing"]);
    }

    #[test]
    fn prompt_history_restores_the_saved_draft_after_editing_a_recalled_entry() {
        let mut composer = Composer::new();
        composer
            .prompt_history
            .record(100, entry("historical prompt", &[]));
        replace_text(&mut composer, "unsent draft");

        navigate(&mut composer, -1);
        composer.insert_slice("_edited");
        assert_eq!(composer.text(), "historical prompt_edited");
        assert_eq!(composer.prompt_history.index, Some(0));
        assert_eq!(composer.prompt_history.draft_text(), Some("unsent draft"));

        navigate(&mut composer, 1);
        assert_eq!(composer.text(), "unsent draft");
        assert_eq!(composer.prompt_history.index, None);
        assert_eq!(composer.prompt_history.draft, None);
    }

    #[test]
    fn prompt_history_restores_the_complete_saved_semantic_draft() {
        let placeholder = format_placeholder(3, 1);
        let mut composer = Composer::new();
        composer.prompt_history.record(100, entry("entry", &[]));
        replace_text(&mut composer, &format!("draft {placeholder}"));
        composer
            .entities
            .register_pasted_block(paste_block(3, "draft backing", "draft ".len()));

        navigate(&mut composer, -1);
        assert_eq!(composer.text(), "entry");
        assert!(composer.entities.pasted_blocks.is_empty());
        navigate(&mut composer, 1);
        assert_eq!(composer.text(), format!("draft {placeholder}"));
        assert_eq!(composer.expanded_text(), "draft draft backing");
    }

    #[test]
    fn prompt_history_limit_rejection_preserves_active_and_saved_drafts() {
        let mut composer = Composer::new();
        composer.prompt_history.record(100, entry("oversized", &[]));
        composer.prompt_history.record(100, entry("new", &[]));
        replace_text(&mut composer, "unsent draft");

        assert_eq!(
            composer.navigate_history(-1, 4096),
            HistoryNavigation::Moved
        );
        assert_eq!(composer.text(), "new");
        assert_eq!(composer.prompt_history.index, Some(1));
        assert_eq!(composer.prompt_history.draft_text(), Some("unsent draft"));

        composer.vertical.preferred_column = Some(7);
        assert_eq!(
            composer.navigate_history(-1, 4),
            HistoryNavigation::LimitExceeded("oversized".len())
        );
        assert_eq!(composer.text(), "new");
        assert_eq!(composer.prompt_history.index, Some(1));
        assert_eq!(composer.prompt_history.draft_text(), Some("unsent draft"));
        assert_eq!(composer.preferred_column(), Some(7));
    }

    #[test]
    fn prompt_history_limit_counts_registered_paste_backing_text() {
        let placeholder = format_placeholder(1, 1);
        let backing = "x".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1);
        let mut composer = Composer::new();
        composer
            .prompt_history
            .record(100, entry(&placeholder, &[paste_block(1, &backing, 0)]));
        replace_text(&mut composer, "draft");
        assert_eq!(
            composer.navigate_history(-1, backing.len() - 1),
            HistoryNavigation::LimitExceeded(backing.len())
        );
        assert_eq!(composer.text(), "draft");
        assert_eq!(composer.prompt_history.index, None);
    }

    #[test]
    fn installed_entries_replace_history_and_end_navigation() {
        let mut composer = Composer::new();
        install(&mut composer, &["session"]);
        replace_text(&mut composer, "draft");
        navigate(&mut composer, -1);
        composer.install_history(vec!["older".to_owned(), "newer".to_owned()]);
        assert_eq!(entry_texts(&composer.prompt_history), ["older", "newer"]);
        assert_eq!(composer.prompt_history.index, None);
        assert_eq!(composer.prompt_history.draft, None);
        composer.record_text_history(100, "newer");
        composer.record_text_history(100, "/help");
        assert_eq!(
            entry_texts(&composer.prompt_history),
            ["older", "newer", "/help"]
        );
    }

    #[test]
    fn recording_the_composer_captures_text_and_paste_backing() {
        let placeholder = format_placeholder(1, 1);
        let mut composer = Composer::new();
        replace_text(&mut composer, &placeholder);
        composer
            .entities
            .register_pasted_block(paste_block(1, "backing", 0));
        composer.record_history(100);
        composer.record_history(100);
        assert_eq!(
            entry_texts(&composer.prompt_history),
            [placeholder.as_str()]
        );
    }
}
