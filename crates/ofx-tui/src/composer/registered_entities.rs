use super::entity_spans::Span;
use super::pasted_blocks::{PastedBlock, registered_placeholder_span_starting_at};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entity {
    pub(crate) index: usize,
    pub(crate) span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entities {
    pub(crate) pasted_blocks: Vec<PastedBlock>,
    pub(crate) next_paste_id: usize,
}

impl Default for Entities {
    fn default() -> Self {
        Self {
            pasted_blocks: Vec::new(),
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
        let index = self
            .pasted_blocks
            .iter()
            .position(|block| block.span.raw_start == raw_start)?;
        let span = registered_placeholder_span_starting_at(input, raw_start, &self.pasted_blocks)?;
        Some(Entity { index, span })
    }

    pub(crate) fn entity_ending_at(&self, input: &str, raw_end: usize) -> Option<Entity> {
        let index = self
            .pasted_blocks
            .iter()
            .position(|block| block.span.raw_end == raw_end)?;
        self.registered_entity(input, index)
    }

    pub(crate) fn entity_containing(&self, input: &str, raw_offset: usize) -> Option<Entity> {
        let index = self
            .pasted_blocks
            .iter()
            .position(|block| block.span.contains(raw_offset))?;
        self.registered_entity(input, index)
    }

    pub(crate) fn entity_overlapping(&self, raw_start: usize, raw_end: usize) -> Option<Entity> {
        let edit = Span::new(raw_start, raw_end);
        self.pasted_blocks
            .iter()
            .position(|block| block.span.overlaps(edit))
            .map(|index| Entity {
                index,
                span: self.pasted_blocks[index].span,
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

    pub(crate) fn remove(&mut self, entity: Entity) {
        self.pasted_blocks.remove(entity.index);
    }

    pub(crate) fn shift_for_insert(&mut self, raw_offset: usize, byte_len: usize) {
        if byte_len == 0 {
            return;
        }
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
        self.pasted_blocks
            .retain_mut(|block| match block.span.after_delete(start, end) {
                Some(span) => {
                    block.span = span;
                    true
                }
                None => false,
            });
    }

    fn registered_entity(&self, input: &str, index: usize) -> Option<Entity> {
        let span = self.pasted_blocks[index].span;
        registered_placeholder_span_starting_at(input, span.raw_start, &self.pasted_blocks)?;
        Some(Entity { index, span })
    }
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
