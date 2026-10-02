mod composer_deletion;
mod composer_history;
mod composer_insertion;
mod composer_kill_ring;
mod composer_line_continuation;
mod composer_replacement;
mod composer_selection;
mod composer_undo;
mod edit_history;
mod editor_state;
mod entity_spans;
mod horizontal_navigation;
mod input_limit_rejection;
mod input_paste_runtime;
mod input_reset;
mod kill_ring;
mod pasted_blocks;
mod registered_entities;
#[cfg(test)]
mod test_fixture;
mod text_boundaries;
mod vertical_navigation;
mod visual_layout;

use std::borrow::Cow;

pub(crate) use composer_deletion::DeletionKind;
pub(crate) use composer_history::HistoryNavigation;
pub(crate) use editor_state::{InsertResult, SelectionRange};
pub(crate) use visual_layout::{
    LayoutEvent, UnitKind, VisualLayout, terminal_column, visible_window,
};

use composer_history::PromptHistory;
use edit_history::EditHistory;
use editor_state::EditorState;
use input_limit_rejection::LimitRejection;
use kill_ring::KillRing;
use registered_entities::Entities;
use vertical_navigation::VerticalNavigation;

#[derive(Debug, Clone, Default)]
pub(crate) struct Composer {
    edit: EditorState,
    entities: Entities,
    edit_history: EditHistory,
    kill_ring: KillRing,
    prompt_history: PromptHistory,
    vertical: VerticalNavigation,
    limit_rejection: LimitRejection,
}

impl Composer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn text(&self) -> &str {
        &self.edit.input
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.edit.input.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn cursor(&self) -> usize {
        self.edit.cursor
    }

    pub(crate) fn selection(&self) -> Option<SelectionRange> {
        self.edit.selection_range()
    }

    pub(crate) fn expanded_text(&self) -> Cow<'_, str> {
        pasted_blocks::expand(&self.edit.input, &self.entities.pasted_blocks)
    }

    #[cfg(test)]
    pub(crate) fn expanded_len(&self) -> Option<usize> {
        pasted_blocks::expanded_len(&self.edit.input, &self.entities.pasted_blocks)
    }

    #[cfg(test)]
    pub(crate) fn preferred_column(&self) -> Option<usize> {
        self.vertical.preferred_column()
    }

    pub(crate) fn visual_layout(&self, terminal_cols: u16) -> VisualLayout<'_> {
        VisualLayout::new(
            &self.edit.input,
            self.edit.cursor,
            terminal_cols,
            &self.entities.pasted_blocks,
        )
    }
}
