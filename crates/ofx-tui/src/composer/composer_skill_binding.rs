use std::path::Path;

use ofx_contract::SkillBinding;

use super::Composer;
use super::entity_spans::Span;
use super::registered_entities::{SkillToken, appends_skill_separator};

impl Composer {
    pub(crate) fn skill_token_inserted_len(&self, replace_end: usize, name: &str) -> usize {
        1 + name.len() + usize::from(appends_skill_separator(&self.edit.input, replace_end))
    }

    pub(crate) fn bind_skill_token(
        &mut self,
        replace_start: usize,
        replace_end: usize,
        name: &str,
        path: &Path,
    ) -> bool {
        if replace_start > replace_end || replace_end > self.edit.input.len() {
            return false;
        }
        let separator = appends_skill_separator(&self.edit.input, replace_end);
        let token_end = replace_start + 1 + name.len();
        let mut raw = format!("${name}");
        if separator {
            raw.push(' ');
        }
        self.edit.discard_selection();
        self.entities.discard_pending_separator();
        if replace_start < replace_end {
            self.entities.adjust_for_delete(replace_start, replace_end);
            self.edit.delete_text_range(replace_start, replace_end);
        }
        self.edit.set_cursor(replace_start);
        self.entities.remove_skill_token_containing(replace_start);
        self.edit.insert_str(&raw);
        self.entities.shift_for_insert(replace_start, raw.len());
        self.entities.register_skill_token(SkillToken {
            span: Span::new(replace_start, token_end),
            name: name.to_owned(),
            path: path.to_path_buf(),
            owns_trailing_separator: separator,
        });
        self.edit.set_cursor(replace_start + raw.len());
        self.edit_history.reset();
        self.vertical.reset();
        self.limit_rejection.clear();
        true
    }

    pub(crate) fn skill_bindings(&self) -> Vec<SkillBinding> {
        let mut bindings: Vec<SkillBinding> = Vec::new();
        for token in &self.entities.skill_tokens {
            if token.name.is_empty()
                || token.path.as_os_str().is_empty()
                || bindings.iter().any(|binding| binding.path == token.path)
            {
                continue;
            }
            bindings.push(SkillBinding {
                name: token.name.clone(),
                path: token.path.clone(),
            });
        }
        bindings
    }

    pub(crate) fn skill_token_name(&self, index: usize) -> &str {
        &self.entities.skill_tokens[index].name
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::DeletionKind;
    use super::super::test_fixture::replace_text;
    use super::*;
    use crate::input::{MoveIntent, MoveKind};

    const LIMIT: usize = 1024;

    fn bound(text: &str, start: usize, end: usize) -> Composer {
        let mut composer = Composer::new();
        replace_text(&mut composer, text);
        assert!(composer.bind_skill_token(start, end, "review", Path::new("/skills/review")));
        composer
    }

    #[test]
    fn binding_replaces_the_range_and_owns_a_separator_before_a_word() {
        let mut composer = Composer::new();
        composer.insert_text("revnext", LIMIT);
        composer.edit.set_cursor(3);
        assert!(composer.bind_skill_token(0, 3, "review", Path::new("/skills/review")));
        assert_eq!(composer.text(), "$review next");
        assert_eq!(composer.cursor(), "$review ".len());
        let token = &composer.entities.skill_tokens[0];
        assert_eq!(token.span, Span::new(0, "$review".len()));
        assert!(token.owns_trailing_separator);
        assert!(composer.entities.pending_separator.is_some());
        assert!(!composer.undo());
        assert_eq!(composer.skill_token_inserted_len(3, "review"), 8);
    }

    #[test]
    fn binding_before_punctuation_or_space_adds_no_separator() {
        let composer = bound("rev, then", 0, 3);
        assert_eq!(composer.text(), "$review, then");
        assert!(!composer.entities.skill_tokens[0].owns_trailing_separator);
        let composer = bound("rev then", 0, 3);
        assert_eq!(composer.text(), "$review then");
        assert_eq!(composer.cursor(), "$review".len());
    }

    #[test]
    fn a_typed_space_right_after_the_owned_separator_is_swallowed_once() {
        let mut composer = bound("", 0, 0);
        assert_eq!(composer.text(), "$review ");
        composer.insert_text(" ", LIMIT);
        assert_eq!(composer.text(), "$review ");
        assert!(!composer.entities.skill_tokens[0].owns_trailing_separator);
        composer.insert_text(" ", LIMIT);
        assert_eq!(composer.text(), "$review  ");
    }

    #[test]
    fn any_other_edit_drops_the_pending_separator() {
        let mut composer = bound("", 0, 0);
        composer.insert_text("x", LIMIT);
        composer.delete(DeletionKind::CharacterLeft);
        composer.insert_text(" ", LIMIT);
        assert_eq!(composer.text(), "$review  ");
    }

    #[test]
    fn deleting_backward_removes_the_whole_token() {
        let mut composer = bound("", 0, 0);
        composer.move_cursor(MoveIntent {
            kind: MoveKind::CharacterLeft,
            extend_selection: false,
        });
        assert_eq!(composer.cursor(), "$review".len());
        assert!(composer.delete(DeletionKind::CharacterLeft));
        assert_eq!(composer.text(), " ");
        assert!(composer.entities.skill_tokens.is_empty());
    }

    #[test]
    fn deleting_forward_from_the_token_start_takes_its_owned_separator() {
        let mut composer = bound("", 0, 0);
        composer.insert_text("next", LIMIT);
        assert_eq!(composer.text(), "$review next");
        composer.move_cursor(MoveIntent {
            kind: MoveKind::LineStart,
            extend_selection: false,
        });
        assert!(composer.delete(DeletionKind::CharacterRight));
        assert_eq!(composer.text(), "next");
        let mut composer = bound("rev then", 0, 3);
        composer.move_cursor(MoveIntent {
            kind: MoveKind::LineStart,
            extend_selection: false,
        });
        assert!(composer.delete(DeletionKind::WordRight));
        assert_eq!(composer.text(), " then");
    }

    #[test]
    fn typing_inside_a_token_turns_it_back_into_text() {
        let mut composer = bound("", 0, 0);
        composer.edit.set_cursor(3);
        composer.insert_text("x", LIMIT);
        assert_eq!(composer.text(), "$rexview ");
        assert!(composer.entities.skill_tokens.is_empty());
        assert!(composer.skill_bindings().is_empty());
    }

    #[test]
    fn bindings_list_each_bound_path_once_in_prompt_order() {
        let mut composer = bound("", 0, 0);
        composer.insert_text("and ", LIMIT);
        let end = composer.text().len();
        assert!(composer.bind_skill_token(end, end, "review", Path::new("/skills/review")));
        let end = composer.text().len();
        assert!(composer.bind_skill_token(end, end, "deploy", Path::new("/skills/deploy")));
        assert_eq!(composer.text(), "$review and $review $deploy ");
        assert_eq!(
            composer.skill_bindings(),
            [
                SkillBinding {
                    name: "review".to_owned(),
                    path: PathBuf::from("/skills/review"),
                },
                SkillBinding {
                    name: "deploy".to_owned(),
                    path: PathBuf::from("/skills/deploy"),
                },
            ]
        );
        composer.clear();
        assert!(composer.skill_bindings().is_empty());
        assert!(!composer.bind_skill_token(1, 0, "x", Path::new("/x")));
    }
}
