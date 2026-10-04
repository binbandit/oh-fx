use ofx_contract::{QuestionRequest, RequestId};

use crate::composer::text_boundaries::{
    logical_line_end, logical_line_start, next_character_end, next_word_delete_end, next_word_end,
    previous_character_start, previous_whitespace_delimited_token_start, previous_word_start,
};
use crate::footer::question_freeform_layout::{self, Direction};
use crate::row_text::terminal_safe;

pub(crate) const FREEFORM_OPTION_LABEL: &str = "Other";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptOption {
    pub(crate) label: String,
    pub(crate) description: Option<String>,
    pub(crate) freeform: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Draft {
    pub(crate) text: String,
    pub(crate) cursor: usize,
    preferred_column: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptEntry {
    question: String,
    options: Vec<PromptOption>,
    choice: usize,
    answer: Option<String>,
    draft: Draft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EntryView<'a> {
    pub(crate) question: &'a str,
    pub(crate) options: &'a [PromptOption],
    pub(crate) choice: usize,
    pub(crate) draft: &'a Draft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PromptView<'a> {
    pub(crate) entry: Option<EntryView<'a>>,
    pub(crate) index: usize,
    pub(crate) count: usize,
}

impl PromptView<'_> {
    pub(crate) fn freeform_selected(&self) -> bool {
        self.entry.is_some_and(|entry| {
            entry
                .options
                .get(entry.choice)
                .is_some_and(|option| option.freeform)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Pending,
    AllDecided,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Insertion {
    Inserted,
    Inactive,
    LimitExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FreeformEdit {
    CursorLeft,
    CursorRight,
    CursorHome,
    CursorEnd,
    CursorWordLeft,
    CursorWordRight,
    DeleteNext,
    DeleteWordLeft,
    DeleteWhitespaceWordLeft,
    DeleteWordRight,
    DeleteToLineStart,
    DeleteToLineEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QuestionPrompt {
    pub(crate) request_id: RequestId,
    entries: Vec<PromptEntry>,
    current: usize,
    limit_rejected: bool,
}

impl QuestionPrompt {
    pub(crate) fn new(request: QuestionRequest) -> Self {
        let entries = request
            .entries
            .into_iter()
            .map(|entry| {
                let mut options: Vec<PromptOption> = entry
                    .options
                    .into_iter()
                    .map(|option| PromptOption {
                        label: terminal_safe(&option.label).into_owned(),
                        description: option
                            .description
                            .map(|description| terminal_safe(&description).into_owned()),
                        freeform: false,
                    })
                    .collect();
                options.push(PromptOption {
                    label: FREEFORM_OPTION_LABEL.to_owned(),
                    description: None,
                    freeform: true,
                });
                PromptEntry {
                    question: terminal_safe(&entry.question).into_owned(),
                    options,
                    choice: 0,
                    answer: None,
                    draft: Draft::default(),
                }
            })
            .collect();
        Self {
            request_id: request.id,
            entries,
            current: 0,
            limit_rejected: false,
        }
    }

    pub(crate) fn view(&self) -> PromptView<'_> {
        PromptView {
            entry: self.entries.get(self.current).map(|entry| EntryView {
                question: &entry.question,
                options: &entry.options,
                choice: entry.choice,
                draft: &entry.draft,
            }),
            index: self.current,
            count: self.entries.len(),
        }
    }

    pub(crate) fn freeform_selected(&self) -> bool {
        self.view().freeform_selected()
    }

    pub(crate) fn draft_len(&self) -> usize {
        self.selected_draft().map_or(0, |draft| draft.text.len())
    }

    pub(crate) fn clear_draft(&mut self) {
        self.limit_rejected = false;
        if let Some(draft) = self.selected_draft_mut() {
            *draft = Draft::default();
        }
    }

    pub(crate) fn move_choice(&mut self, step: isize) {
        self.limit_rejected = false;
        let Some(entry) = self.entries.get_mut(self.current) else {
            return;
        };
        let count = entry.options.len();
        if count == 0 {
            return;
        }
        entry.draft.preferred_column = None;
        entry.choice = (entry.choice + count).saturating_add_signed(step) % count;
    }

    pub(crate) fn next_entry(&mut self) {
        self.limit_rejected = false;
        if self.entries.is_empty() {
            return;
        }
        self.forget_preferred_column();
        self.current = (self.current + 1) % self.entries.len();
        self.forget_preferred_column();
    }

    pub(crate) fn retreat(&mut self) -> bool {
        self.limit_rejected = false;
        if self.current == 0 || self.current > self.entries.len() {
            return false;
        }
        self.forget_preferred_column();
        self.current -= 1;
        self.forget_preferred_column();
        true
    }

    pub(crate) fn select_ordinal(&mut self, index: usize) -> Decision {
        self.limit_rejected = false;
        let Some(entry) = self.entries.get_mut(self.current) else {
            return Decision::Pending;
        };
        let Some(option) = entry.options.get(index) else {
            return Decision::Pending;
        };
        let freeform = option.freeform;
        entry.draft.preferred_column = None;
        entry.choice = index;
        if freeform {
            return Decision::Pending;
        }
        self.submit()
    }

    pub(crate) fn submit(&mut self) -> Decision {
        self.limit_rejected = false;
        let answered = self.current;
        let Some(entry) = self.entries.get_mut(answered) else {
            return Decision::Pending;
        };
        let Some(option) = entry.options.get(entry.choice) else {
            return Decision::Pending;
        };
        entry.answer = Some(if option.freeform {
            entry.draft.text.clone()
        } else {
            option.label.clone()
        });
        entry.draft.preferred_column = None;
        let count = self.entries.len();
        for offset in 1..count {
            let candidate = (answered + offset) % count;
            if self.entries[candidate].answer.is_none() {
                self.current = candidate;
                self.forget_preferred_column();
                return Decision::Pending;
            }
        }
        self.current = count;
        Decision::AllDecided
    }

    pub(crate) fn answers(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.answer.clone().unwrap_or_default())
            .collect()
    }

    pub(crate) fn resolutions(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .map(|entry| {
                (
                    entry.question.clone(),
                    entry.answer.clone().unwrap_or_default(),
                )
            })
            .collect()
    }

    pub(crate) fn insert(&mut self, text: &str, max_len: usize) -> Insertion {
        let Some(draft) = self.selected_draft_mut() else {
            return Insertion::Inactive;
        };
        let inserted = draft.insert(text, max_len);
        if inserted == Insertion::Inserted {
            self.limit_rejected = false;
        }
        inserted
    }

    pub(crate) fn note_limit_rejection(&mut self) -> bool {
        !std::mem::replace(&mut self.limit_rejected, true)
    }

    pub(crate) fn backspace(&mut self) -> bool {
        self.limit_rejected = false;
        let Some(draft) = self.selected_draft_mut() else {
            return false;
        };
        draft.backspace();
        true
    }

    pub(crate) fn edit(&mut self, edit: FreeformEdit) -> bool {
        self.limit_rejected = false;
        let Some(draft) = self.selected_draft_mut() else {
            return false;
        };
        draft.edit(edit);
        true
    }

    pub(crate) fn move_freeform_vertical(&mut self, direction: Direction, cols: usize) -> bool {
        let Some(entry) = self.entries.get_mut(self.current) else {
            return false;
        };
        if !entry
            .options
            .get(entry.choice)
            .is_some_and(|option| option.freeform)
        {
            return false;
        }
        let draft = &mut entry.draft;
        let movement = question_freeform_layout::move_cursor(
            &draft.text,
            draft.cursor,
            question_freeform_layout::content_width(entry.choice, cols),
            direction,
            draft.preferred_column,
        );
        draft.cursor = movement.cursor;
        draft.preferred_column = Some(movement.preferred_column);
        movement.moved
    }

    fn selected_draft(&self) -> Option<&Draft> {
        self.freeform_selected()
            .then(|| &self.entries[self.current].draft)
    }

    fn selected_draft_mut(&mut self) -> Option<&mut Draft> {
        if !self.freeform_selected() {
            return None;
        }
        Some(&mut self.entries[self.current].draft)
    }

    fn forget_preferred_column(&mut self) {
        if let Some(entry) = self.entries.get_mut(self.current) {
            entry.draft.preferred_column = None;
        }
    }
}

impl Draft {
    pub(crate) fn insert(&mut self, text: &str, max_len: usize) -> Insertion {
        if text.is_empty() {
            return Insertion::Inserted;
        }
        if self.text.len() > max_len || text.len() > max_len - self.text.len() {
            return Insertion::LimitExceeded;
        }
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.preferred_column = None;
        Insertion::Inserted
    }

    pub(crate) fn backspace(&mut self) {
        self.preferred_column = None;
        let start = previous_character_start(&self.text, self.cursor);
        self.delete(start, self.cursor);
    }

    pub(crate) fn edit(&mut self, edit: FreeformEdit) {
        self.preferred_column = None;
        let text = self.text.as_str();
        let cursor = self.cursor;
        match edit {
            FreeformEdit::CursorLeft => self.cursor = previous_character_start(text, cursor),
            FreeformEdit::CursorRight => self.cursor = next_character_end(text, cursor),
            FreeformEdit::CursorHome => self.cursor = 0,
            FreeformEdit::CursorEnd => self.cursor = text.len(),
            FreeformEdit::CursorWordLeft => self.cursor = previous_word_start(text, cursor),
            FreeformEdit::CursorWordRight => self.cursor = next_word_end(text, cursor),
            FreeformEdit::DeleteNext => {
                let end = next_character_end(text, cursor);
                self.delete(cursor, end);
            }
            FreeformEdit::DeleteWordLeft => {
                let start = previous_word_start(text, cursor);
                self.delete(start, cursor);
            }
            FreeformEdit::DeleteWhitespaceWordLeft => {
                let line_start = logical_line_start(text, cursor);
                let start = previous_whitespace_delimited_token_start(text, cursor, line_start);
                self.delete(start, cursor);
            }
            FreeformEdit::DeleteWordRight => {
                let end = next_word_delete_end(text, cursor);
                self.delete(cursor, end);
            }
            FreeformEdit::DeleteToLineStart => {
                let start = logical_line_start(text, cursor);
                self.delete(start, cursor);
            }
            FreeformEdit::DeleteToLineEnd => {
                let line_end = logical_line_end(text, cursor);
                let end = if cursor == line_end && line_end < text.len() {
                    line_end + 1
                } else {
                    line_end
                };
                self.delete(cursor, end);
            }
        }
    }

    fn delete(&mut self, start: usize, end: usize) {
        let start = start.min(self.text.len());
        let end = end.clamp(start, self.text.len());
        if start == end {
            return;
        }
        self.text.replace_range(start..end, "");
        if self.cursor >= end {
            self.cursor -= end - start;
        } else if self.cursor > start {
            self.cursor = start;
        }
    }
}

#[cfg(test)]
mod tests {
    use ofx_contract::{QuestionBatchEntry, QuestionOption};

    use super::*;

    fn option(label: &str, description: Option<&str>) -> QuestionOption {
        QuestionOption {
            label: label.to_owned(),
            description: description.map(str::to_owned),
        }
    }

    fn prompt(questions: &[&str], labels: &[&str]) -> QuestionPrompt {
        QuestionPrompt::new(QuestionRequest {
            id: RequestId::new(7),
            entries: questions
                .iter()
                .map(|question| QuestionBatchEntry {
                    question: (*question).to_owned(),
                    options: labels.iter().map(|label| option(label, None)).collect(),
                })
                .collect(),
        })
    }

    fn proceed() -> QuestionPrompt {
        QuestionPrompt::new(QuestionRequest {
            id: RequestId::new(1),
            entries: vec![QuestionBatchEntry {
                question: "Should we proceed?".to_owned(),
                options: vec![
                    option("Yes", Some("go ahead")),
                    option("No", None),
                    option("Maybe", Some("decide later")),
                ],
            }],
        })
    }

    fn draft(prompt: &QuestionPrompt) -> (String, usize) {
        let draft = prompt.view().entry.unwrap().draft.clone();
        (draft.text, draft.cursor)
    }

    #[test]
    fn every_entry_gains_a_trailing_freeform_slot() {
        let prompt = prompt(&["Q0?", "Q1?"], &["One", "Two"]);
        let view = prompt.view();
        let labels: Vec<&str> = view
            .entry
            .unwrap()
            .options
            .iter()
            .map(|option| option.label.as_str())
            .collect();
        assert_eq!(labels, ["One", "Two", "Other"]);
        assert!(view.entry.unwrap().options[2].freeform);
        assert_eq!((view.index, view.count), (0, 2));
        assert!(!prompt.freeform_selected());
    }

    #[test]
    fn moving_the_choice_wraps_and_reaches_the_freeform_slot() {
        let mut prompt = proceed();
        prompt.move_choice(-1);
        assert!(prompt.freeform_selected());
        prompt.move_choice(1);
        assert_eq!(prompt.view().entry.unwrap().choice, 0);
    }

    #[test]
    fn typed_choice_and_freeform_edits_change_only_the_selected_draft() {
        let mut prompt = proceed();
        assert_eq!(prompt.insert("x", usize::MAX), Insertion::Inactive);
        assert!(!prompt.backspace());
        assert!(!prompt.edit(FreeformEdit::CursorLeft));
        prompt.move_choice(-1);
        assert_eq!(prompt.insert("a", usize::MAX), Insertion::Inserted);
        assert_eq!(prompt.insert("b", usize::MAX), Insertion::Inserted);
        assert!(prompt.edit(FreeformEdit::CursorLeft));
        assert!(prompt.edit(FreeformEdit::DeleteNext));
        assert_eq!(draft(&prompt), ("a".to_owned(), 1));
        prompt.move_choice(1);
        prompt.move_choice(-1);
        assert_eq!(draft(&prompt), ("a".to_owned(), 1));
    }

    #[test]
    fn ordinals_commit_predefined_options_and_select_the_freeform_slot() {
        let mut prompt = proceed();
        assert_eq!(prompt.select_ordinal(9), Decision::Pending);
        assert_eq!(prompt.select_ordinal(3), Decision::Pending);
        assert!(prompt.freeform_selected());
        assert_eq!(prompt.select_ordinal(1), Decision::AllDecided);
        assert_eq!(prompt.answers(), ["No"]);
        assert!(prompt.view().entry.is_none());
    }

    #[test]
    fn submitting_advances_to_the_next_unanswered_entry() {
        let mut prompt = prompt(&["Q0?", "Q1?", "Q2?"], &["One", "Two"]);
        prompt.next_entry();
        assert_eq!(prompt.submit(), Decision::Pending);
        assert_eq!(prompt.view().index, 2);
        prompt.move_choice(1);
        assert_eq!(prompt.submit(), Decision::Pending);
        assert_eq!(prompt.view().index, 0);
        assert_eq!(prompt.submit(), Decision::AllDecided);
        assert_eq!(prompt.answers(), ["One", "One", "Two"]);
        assert_eq!(
            prompt.resolutions(),
            [
                ("Q0?".to_owned(), "One".to_owned()),
                ("Q1?".to_owned(), "One".to_owned()),
                ("Q2?".to_owned(), "Two".to_owned()),
            ]
        );
    }

    #[test]
    fn paging_keeps_drafts_and_answers_and_retreating_stops_at_the_first_entry() {
        let mut prompt = prompt(&["Q0?", "Q1?"], &["One", "Two"]);
        assert!(!prompt.retreat());
        prompt.move_choice(-1);
        prompt.insert("draft", usize::MAX);
        prompt.next_entry();
        assert_eq!(prompt.view().index, 1);
        prompt.next_entry();
        assert_eq!(draft(&prompt), ("draft".to_owned(), 5));
        prompt.next_entry();
        assert!(prompt.retreat());
        assert_eq!(prompt.view().index, 0);
        assert_eq!(prompt.submit(), Decision::Pending);
        assert_eq!(prompt.submit(), Decision::AllDecided);
        assert_eq!(prompt.answers(), ["draft", "One"]);
    }

    #[test]
    fn an_empty_freeform_answer_is_an_answer() {
        let mut prompt = proceed();
        prompt.move_choice(-1);
        assert_eq!(prompt.submit(), Decision::AllDecided);
        assert_eq!(prompt.answers(), [""]);
    }

    #[test]
    fn freeform_word_and_line_deletions_follow_the_composer_contract() {
        let mut prompt = proceed();
        prompt.move_choice(-1);
        prompt.insert("one two-three\nfour five", usize::MAX);
        assert!(prompt.edit(FreeformEdit::DeleteWordLeft));
        assert_eq!(draft(&prompt).0, "one two-three\nfour ");
        assert!(prompt.edit(FreeformEdit::DeleteWhitespaceWordLeft));
        assert_eq!(draft(&prompt).0, "one two-three\n");
        assert!(prompt.edit(FreeformEdit::DeleteWhitespaceWordLeft));
        assert_eq!(draft(&prompt).0, "one two-three\n");
        assert!(prompt.backspace());
        assert!(prompt.edit(FreeformEdit::CursorWordLeft));
        assert_eq!(draft(&prompt), ("one two-three".to_owned(), 8));
        assert!(prompt.edit(FreeformEdit::DeleteToLineEnd));
        assert_eq!(draft(&prompt).0, "one two-");
        assert!(prompt.edit(FreeformEdit::CursorHome));
        assert!(prompt.edit(FreeformEdit::DeleteWordRight));
        assert_eq!(draft(&prompt), ("two-".to_owned(), 0));
        assert!(prompt.edit(FreeformEdit::CursorEnd));
        assert!(prompt.edit(FreeformEdit::DeleteToLineStart));
        assert_eq!(draft(&prompt), (String::new(), 0));
    }

    #[test]
    fn line_end_deletion_at_a_break_joins_the_next_line() {
        let mut prompt = proceed();
        prompt.move_choice(-1);
        prompt.insert("ab\ncd", usize::MAX);
        prompt.edit(FreeformEdit::CursorHome);
        prompt.edit(FreeformEdit::CursorWordRight);
        assert!(prompt.edit(FreeformEdit::DeleteToLineEnd));
        assert_eq!(draft(&prompt), ("abcd".to_owned(), 2));
    }

    #[test]
    fn freeform_edits_keep_graphemes_whole() {
        let mut prompt = proceed();
        prompt.move_choice(-1);
        prompt.insert("e\u{301}👍🏽", usize::MAX);
        assert!(prompt.backspace());
        assert_eq!(draft(&prompt), ("e\u{301}".to_owned(), 3));
        assert!(prompt.edit(FreeformEdit::CursorLeft));
        assert_eq!(draft(&prompt).1, 0);
        assert!(prompt.edit(FreeformEdit::DeleteNext));
        assert_eq!(draft(&prompt), (String::new(), 0));
    }

    #[test]
    fn bounded_insertion_rejects_without_changing_the_draft() {
        let mut prompt = proceed();
        prompt.move_choice(-1);
        assert_eq!(prompt.insert("abc", 4), Insertion::Inserted);
        assert_eq!(prompt.insert("de", 4), Insertion::LimitExceeded);
        assert_eq!(draft(&prompt), ("abc".to_owned(), 3));
        assert_eq!(prompt.insert("d", 4), Insertion::Inserted);
        prompt.clear_draft();
        assert_eq!(prompt.draft_len(), 0);
    }

    #[test]
    fn limit_rejections_are_reported_once_until_an_edit_lands() {
        let mut prompt = proceed();
        prompt.move_choice(-1);
        assert!(prompt.note_limit_rejection());
        assert!(!prompt.note_limit_rejection());
        assert_eq!(prompt.insert("a", 4), Insertion::Inserted);
        assert!(prompt.note_limit_rejection());
        prompt.move_choice(1);
        assert!(prompt.note_limit_rejection());
    }

    #[test]
    fn vertical_moves_walk_the_wrapped_draft_and_report_its_edges() {
        let mut prompt = proceed();
        prompt.move_choice(-1);
        prompt.insert("first line\nsecond", usize::MAX);
        assert!(prompt.move_freeform_vertical(Direction::Up, 40));
        assert_eq!(draft(&prompt).1, 6);
        assert!(!prompt.move_freeform_vertical(Direction::Up, 40));
        assert!(prompt.move_freeform_vertical(Direction::Down, 40));
        assert_eq!(draft(&prompt).1, 17);
        prompt.move_choice(1);
        assert!(!prompt.move_freeform_vertical(Direction::Up, 40));
    }

    #[test]
    fn hostile_request_text_is_shown_escaped() {
        let prompt = prompt(&["Q\u{1b}[2J?"], &["\u{202e}evil", "ok"]);
        let entry = prompt.view().entry.unwrap();
        assert_eq!(entry.question, "Q\\x1b[2J?");
        assert_eq!(entry.options[0].label, "\\u{202e}evil");
    }
}
