use std::path::PathBuf;

use super::entity_spans::Span;
use super::pasted_blocks::{PastedBlock, registered_placeholder_span_starting_at};
use super::text_boundaries::is_word_character;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntityKind {
    Paste,
    Skill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entity {
    pub(crate) kind: EntityKind,
    pub(crate) index: usize,
    pub(crate) span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillToken {
    pub(crate) span: Span,
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) owns_trailing_separator: bool,
}

impl SkillToken {
    fn is_valid(&self, input: &str) -> bool {
        self.span.is_valid(input.len())
            && input
                .get(self.span.raw_start..self.span.raw_end)
                .and_then(|raw| raw.strip_prefix('$'))
                == Some(self.name.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entities {
    pub(crate) pasted_blocks: Vec<PastedBlock>,
    pub(crate) skill_tokens: Vec<SkillToken>,
    pub(crate) pending_separator: Option<PendingSeparator>,
    pub(crate) next_paste_id: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingSeparator {
    raw_offset: usize,
    token_start: usize,
}

impl Default for Entities {
    fn default() -> Self {
        Self {
            pasted_blocks: Vec::new(),
            skill_tokens: Vec::new(),
            pending_separator: None,
            next_paste_id: 1,
        }
    }
}

impl Entities {
    pub(crate) fn register_pasted_block(&mut self, block: PastedBlock) {
        let index = self
            .pasted_blocks
            .iter()
            .take_while(|existing| existing.span.raw_start < block.span.raw_start)
            .count();
        self.pasted_blocks.insert(index, block);
    }

    pub(crate) fn next_paste_id_after(&self, blocks: &[PastedBlock]) -> Option<usize> {
        let mut next_id = self.next_paste_id;
        for block in blocks {
            if block.id >= next_id {
                next_id = block.id.checked_add(1)?;
            }
        }
        Some(next_id)
    }

    pub(crate) fn entity_starting_at(&self, input: &str, raw_start: usize) -> Option<Entity> {
        if let Some(index) = self
            .pasted_blocks
            .iter()
            .position(|block| block.span.raw_start == raw_start)
        {
            let span =
                registered_placeholder_span_starting_at(input, raw_start, &self.pasted_blocks)?;
            return Some(Entity {
                kind: EntityKind::Paste,
                index,
                span,
            });
        }
        let index = self
            .skill_tokens
            .iter()
            .position(|token| token.span.raw_start == raw_start)?;
        self.valid_skill_entity(input, index)
    }

    pub(crate) fn entity_ending_at(&self, input: &str, raw_end: usize) -> Option<Entity> {
        if let Some(index) = self
            .pasted_blocks
            .iter()
            .position(|block| block.span.raw_end == raw_end)
        {
            return self.registered_entity(input, index);
        }
        let index = self
            .skill_tokens
            .iter()
            .position(|token| token.span.raw_end == raw_end)?;
        self.valid_skill_entity(input, index)
    }

    pub(crate) fn entity_containing(&self, input: &str, raw_offset: usize) -> Option<Entity> {
        if let Some(index) = self
            .pasted_blocks
            .iter()
            .position(|block| block.span.contains(raw_offset))
        {
            return self.registered_entity(input, index);
        }
        let index = self
            .skill_tokens
            .iter()
            .position(|token| token.span.contains(raw_offset))?;
        self.valid_skill_entity(input, index)
    }

    pub(crate) fn entity_overlapping(&self, raw_start: usize, raw_end: usize) -> Option<Entity> {
        let edit = Span::new(raw_start, raw_end);
        let paste = self
            .pasted_blocks
            .iter()
            .position(|block| block.span.overlaps(edit))
            .map(|index| Entity {
                kind: EntityKind::Paste,
                index,
                span: self.pasted_blocks[index].span,
            });
        paste.or_else(|| {
            self.skill_tokens
                .iter()
                .position(|token| token.span.overlaps(edit))
                .map(|index| Entity {
                    kind: EntityKind::Skill,
                    index,
                    span: self.skill_tokens[index].span,
                })
        })
    }

    pub(crate) fn entity_ending_at_or_containing(
        &self,
        input: &str,
        raw_offset: usize,
    ) -> Option<Entity> {
        self.entity_ending_at(input, raw_offset)
            .or_else(|| self.entity_containing(input, raw_offset))
    }

    pub(crate) fn entity_starting_at_or_containing(
        &self,
        input: &str,
        raw_offset: usize,
    ) -> Option<Entity> {
        self.entity_starting_at(input, raw_offset)
            .or_else(|| self.entity_containing(input, raw_offset))
    }

    pub(crate) fn atomic_forward_delete_end(
        &self,
        input: &str,
        cursor: usize,
        entity: Entity,
    ) -> usize {
        let end = entity.span.raw_end;
        if cursor != entity.span.raw_start || entity.kind != EntityKind::Skill {
            return end;
        }
        if self.skill_tokens[entity.index].owns_trailing_separator
            && input.as_bytes().get(end) == Some(&b' ')
        {
            end + 1
        } else {
            end
        }
    }

    pub(crate) fn remove(&mut self, entity: Entity) {
        match entity.kind {
            EntityKind::Paste => {
                self.pasted_blocks.remove(entity.index);
            }
            EntityKind::Skill => {
                self.skill_tokens.remove(entity.index);
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.pasted_blocks.clear();
        self.skill_tokens.clear();
        self.pending_separator = None;
    }

    pub(crate) fn discard_pending_separator(&mut self) {
        self.pending_separator = None;
    }

    pub(crate) fn remove_skill_token_containing(&mut self, raw_offset: usize) {
        self.skill_tokens
            .retain(|token| !token.span.contains(raw_offset));
    }

    pub(crate) fn shift_for_insert(&mut self, raw_offset: usize, byte_len: usize) {
        if byte_len == 0 {
            return;
        }
        self.skill_tokens.retain_mut(|token| {
            if raw_offset == token.span.raw_end {
                token.owns_trailing_separator = false;
            }
            match token.span.after_insert(raw_offset, byte_len) {
                Some(span) => {
                    token.span = span;
                    true
                }
                None => false,
            }
        });
        self.pasted_blocks.retain_mut(|block| {
            match block.span.after_insert(raw_offset, byte_len) {
                Some(span) => {
                    block.span = span;
                    true
                }
                None => false,
            }
        });
    }

    pub(crate) fn adjust_for_delete(&mut self, start: usize, end: usize) {
        self.skill_tokens.retain_mut(|token| {
            if start <= token.span.raw_end && end > token.span.raw_end {
                token.owns_trailing_separator = false;
            }
            match token.span.after_delete(start, end) {
                Some(span) => {
                    token.span = span;
                    true
                }
                None => false,
            }
        });
        self.pasted_blocks
            .retain_mut(|block| match block.span.after_delete(start, end) {
                Some(span) => {
                    block.span = span;
                    true
                }
                None => false,
            });
    }

    pub(crate) fn register_skill_token(&mut self, token: SkillToken) {
        let index = self
            .skill_tokens
            .iter()
            .take_while(|existing| existing.span.raw_start < token.span.raw_start)
            .count();
        if token.owns_trailing_separator {
            self.pending_separator = Some(PendingSeparator {
                raw_offset: token.span.raw_end,
                token_start: token.span.raw_start,
            });
        }
        self.skill_tokens.insert(index, token);
    }

    pub(crate) fn claim_pending_separator(
        &mut self,
        input: &str,
        cursor: usize,
        text: &str,
    ) -> bool {
        let Some(pending) = self.pending_separator.take() else {
            return false;
        };
        if text != " "
            || input.as_bytes().get(pending.raw_offset) != Some(&b' ')
            || cursor != pending.raw_offset + 1
        {
            return false;
        }
        let Some(token) = self
            .skill_tokens
            .iter_mut()
            .find(|token| token.span.raw_start == pending.token_start)
        else {
            return false;
        };
        if token.span.raw_end != pending.raw_offset || !token.owns_trailing_separator {
            return false;
        }
        token.owns_trailing_separator = false;
        true
    }

    fn registered_entity(&self, input: &str, index: usize) -> Option<Entity> {
        let span = self.pasted_blocks[index].span;
        registered_placeholder_span_starting_at(input, span.raw_start, &self.pasted_blocks)?;
        Some(Entity {
            kind: EntityKind::Paste,
            index,
            span,
        })
    }

    fn valid_skill_entity(&self, input: &str, index: usize) -> Option<Entity> {
        let token = &self.skill_tokens[index];
        token.is_valid(input).then_some(Entity {
            kind: EntityKind::Skill,
            index,
            span: token.span,
        })
    }
}

pub(crate) fn appends_skill_separator(input: &str, replace_end: usize) -> bool {
    input[replace_end..]
        .chars()
        .next()
        .is_none_or(is_word_character)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_entity_lookup_accepts_only_canonical_registered_bytes() {
        let placeholder = "[Pasted text #1, 2 lines]";
        let mut entities = Entities::default();
        entities.register_pasted_block(PastedBlock {
            id: 1,
            text: "first\nsecond".to_owned(),
            line_count: 2,
            span: Span::new(0, placeholder.len()),
        });
        assert!(entities.entity_starting_at(placeholder, 0).is_some());
        assert!(
            entities
                .entity_starting_at("[Pasted text #1, 3 lines]", 0)
                .is_none()
        );
    }

    #[test]
    fn registered_entity_projections_shift_external_edits_and_drop_interior_edits() {
        let mut entities = Entities::default();
        entities.register_pasted_block(PastedBlock {
            id: 1,
            text: "backing".to_owned(),
            line_count: 1,
            span: Span::new(4, 11),
        });
        entities.shift_for_insert(2, 3);
        assert_eq!(entities.pasted_blocks[0].span, Span::new(7, 14));
        entities.adjust_for_delete(8, 9);
        assert!(entities.pasted_blocks.is_empty());
    }

    #[test]
    fn next_paste_id_skips_recalled_block_ids() {
        let entities = Entities::default();
        let blocks = [PastedBlock {
            id: 4,
            text: String::new(),
            line_count: 1,
            span: Span::default(),
        }];
        assert_eq!(entities.next_paste_id_after(&blocks), Some(5));
        assert_eq!(entities.next_paste_id_after(&[]), Some(1));
    }
}
