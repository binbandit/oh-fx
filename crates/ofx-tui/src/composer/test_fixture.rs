use super::Composer;
use super::editor_state::EditorState;

pub(super) fn replace_text(composer: &mut Composer, text: &str) {
    composer.vertical.reset();
    composer.edit.discard_selection();
    composer.entities.pasted_blocks.clear();
    composer.edit.swap_input(&mut text.to_owned());
    composer.edit_history.reset();
    composer.limit_rejection.clear();
}

pub(super) fn select(composer: &mut Composer, anchor: usize, cursor: usize) {
    composer.vertical.reset();
    select_in(&mut composer.edit, anchor, cursor);
}

pub(super) fn select_in(state: &mut EditorState, anchor: usize, cursor: usize) {
    let clamp = |offset: usize| {
        state
            .input
            .floor_char_boundary(offset.min(state.input.len()))
    };
    let (anchor, cursor) = (clamp(anchor), clamp(cursor));
    state.selection_anchor = Some(anchor);
    state.cursor = cursor;
}
