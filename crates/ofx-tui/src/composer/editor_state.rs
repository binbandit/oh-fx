#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InsertResult {
    Inserted,
    Inactive,
    LimitExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SelectionRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionEdge {
    Start,
    End,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EditorState {
    pub(crate) input: String,
    pub(crate) cursor: usize,
    pub(crate) selection_anchor: Option<usize>,
}

impl EditorState {
    pub(crate) fn clear(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.selection_anchor = None;
    }

    pub(crate) fn swap_input(&mut self, other: &mut String) {
        std::mem::swap(&mut self.input, other);
        self.cursor = self.input.len();
        self.selection_anchor = None;
    }

    pub(crate) fn clear_selection(&mut self) -> bool {
        self.selection_anchor.take().is_some()
    }

    pub(crate) fn selection_range(&self) -> Option<SelectionRange> {
        let anchor = self.selection_anchor?;
        (anchor != self.cursor).then(|| SelectionRange {
            start: anchor.min(self.cursor),
            end: anchor.max(self.cursor),
        })
    }

    pub(crate) fn selected_text(&self) -> Option<&str> {
        let selection = self.selection_range()?;
        self.input.get(selection.start..selection.end)
    }

    pub(crate) fn discard_selection(&mut self) {
        self.selection_anchor = None;
    }

    pub(crate) fn collapse_selection(&mut self, edge: SelectionEdge) -> bool {
        let Some(selection) = self.selection_range() else {
            self.selection_anchor = None;
            return false;
        };
        self.cursor = match edge {
            SelectionEdge::Start => selection.start,
            SelectionEdge::End => selection.end,
        };
        self.selection_anchor = None;
        true
    }

    pub(crate) fn set_cursor(&mut self, raw_offset: usize) -> bool {
        let offset = self.clamp(raw_offset);
        let changed = self.cursor != offset || self.selection_anchor.is_some();
        self.cursor = offset;
        self.selection_anchor = None;
        changed
    }

    pub(crate) fn move_cursor_to(&mut self, raw_offset: usize, extend_selection: bool) -> bool {
        if !extend_selection {
            return self.set_cursor(raw_offset);
        }
        let offset = self.clamp(raw_offset);
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        }
        if self.cursor == offset {
            return false;
        }
        self.cursor = offset;
        true
    }

    pub(crate) fn select_all(&mut self) -> bool {
        if self.input.is_empty() {
            return self.clear_selection();
        }
        let changed = self.selection_anchor != Some(0) || self.cursor != self.input.len();
        self.selection_anchor = Some(0);
        self.cursor = self.input.len();
        changed
    }

    pub(crate) fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.selection_anchor = None;
    }

    pub(crate) fn delete_text_range(&mut self, start: usize, end: usize) -> bool {
        let start = start.min(self.input.len());
        let end = end.max(start).min(self.input.len());
        if start == end {
            return false;
        }
        if let Some(anchor) = self.selection_anchor {
            self.selection_anchor = Some(offset_after_delete(anchor, start, end));
        }
        delete_range(&mut self.input, &mut self.cursor, start, end)
    }

    pub(crate) fn replace_char_before_cursor(&mut self, expected: char, replacement: char) -> bool {
        let Some(index) = self.cursor.checked_sub(expected.len_utf8()) else {
            return false;
        };
        if !self.input[index..].starts_with(expected)
            || replacement.len_utf8() != expected.len_utf8()
        {
            return false;
        }
        self.input
            .replace_range(index..self.cursor, replacement.encode_utf8(&mut [0; 4]));
        self.selection_anchor = None;
        true
    }

    fn clamp(&self, raw_offset: usize) -> usize {
        self.input
            .floor_char_boundary(raw_offset.min(self.input.len()))
    }
}

pub(crate) fn can_insert(current_len: usize, inserted_len: usize, max_len: usize) -> bool {
    current_len <= max_len && inserted_len <= max_len - current_len
}

pub(crate) fn can_replace(
    current_len: usize,
    replaced_len: usize,
    inserted_len: usize,
    max_len: usize,
) -> bool {
    debug_assert!(replaced_len <= current_len);
    can_insert(current_len - replaced_len, inserted_len, max_len)
}

pub(crate) fn delete_range(
    buffer: &mut String,
    cursor: &mut usize,
    start: usize,
    end: usize,
) -> bool {
    let start = start.min(buffer.len());
    let end = end.max(start).min(buffer.len());
    if start == end {
        return false;
    }
    buffer.replace_range(start..end, "");
    let count = end - start;
    if *cursor >= end {
        *cursor -= count;
    } else if *cursor > start {
        *cursor = start;
    }
    true
}

fn offset_after_delete(offset: usize, start: usize, end: usize) -> usize {
    if offset >= end {
        offset - (end - start)
    } else if offset > start {
        start
    } else {
        offset
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_fixture::select_in;
    use super::*;

    fn state(text: &str) -> EditorState {
        EditorState {
            input: text.to_owned(),
            cursor: text.len(),
            selection_anchor: None,
        }
    }

    #[test]
    fn collapsing_an_empty_selection_clears_its_transient_anchor() {
        let mut state = state("abc");
        select_in(&mut state, 0, 0);
        assert!(!state.collapse_selection(SelectionEdge::Start));
        assert_eq!(state.selection_anchor, None);

        let anchor = state.input.len();
        select_in(&mut state, anchor, anchor);
        assert!(!state.collapse_selection(SelectionEdge::End));
        assert_eq!(state.selection_anchor, None);
    }

    #[test]
    fn selection_extension_keeps_a_fixed_anchor_while_crossing_it() {
        let mut state = state("abcd");
        state.set_cursor(2);
        assert!(state.move_cursor_to(1, true));
        assert_eq!(state.selection_anchor, Some(2));
        assert!(state.move_cursor_to(3, true));
        assert_eq!(state.selection_anchor, Some(2));
        assert_eq!(
            state.selection_range(),
            Some(SelectionRange { start: 2, end: 3 })
        );

        assert!(state.move_cursor_to(0, false));
        assert_eq!(state.selection_anchor, None);
        assert!(state.select_all());
        assert_eq!(
            state.selection_range(),
            Some(SelectionRange { start: 0, end: 4 })
        );
    }

    #[test]
    fn editor_state_insertion_and_deletion_keep_cursor_and_selection_consistent() {
        let mut state = state("alpha beta");
        state.set_cursor(5);
        state.insert_str("!");
        assert_eq!(state.input, "alpha! beta");
        assert_eq!(state.cursor, 6);

        select_in(&mut state, 5, 7);
        assert!(state.delete_text_range(5, 7));
        assert_eq!(state.input, "alphabeta");
        assert_eq!(state.cursor, 5);
        assert_eq!(state.selection_anchor, Some(5));
        assert_eq!(state.selection_range(), None);
    }

    #[test]
    fn bounded_edit_admission_handles_inserts_and_replacements() {
        assert!(can_insert(4095, 1, 4096));
        assert!(!can_insert(4095, 2, 4096));
        assert!(!can_insert(5000, 0, 4096));
        assert!(can_replace(4094, 8, 10, 4096));
        assert!(!can_replace(4094, 8, 11, 4096));
    }

    #[test]
    fn range_deletion_clamps_offsets_and_preserves_cursor_position() {
        let mut buffer = "alpha beta".to_owned();
        let mut cursor = buffer.len();
        assert!(delete_range(&mut buffer, &mut cursor, 5, 6));
        assert_eq!(buffer, "alphabeta");
        assert_eq!(cursor, 9);

        cursor = 3;
        assert!(delete_range(&mut buffer, &mut cursor, 1, 5));
        assert_eq!(buffer, "abeta");
        assert_eq!(cursor, 1);

        assert!(!delete_range(&mut buffer, &mut cursor, 99, 100));
        assert_eq!(buffer, "abeta");
    }

    #[test]
    fn cursor_offsets_snap_to_character_boundaries() {
        let mut state = state("a\u{e9}b");
        assert!(state.set_cursor(2));
        assert_eq!(state.cursor, 1);
        assert!(state.set_cursor(99));
        assert_eq!(state.cursor, state.input.len());
    }

    #[test]
    fn replacing_the_character_before_the_cursor_requires_an_exact_match() {
        let mut state = state("x \\");
        assert!(!state.replace_char_before_cursor('/', '\n'));
        assert!(state.replace_char_before_cursor('\\', '\n'));
        assert_eq!(state.input, "x \n");
        assert_eq!(state.cursor, 3);
    }
}
