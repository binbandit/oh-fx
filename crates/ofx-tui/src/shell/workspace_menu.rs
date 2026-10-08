use ofx_contract::{WorkspaceMenu, WorkspaceMenuEntry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceAction {
    Add,
    Remove(usize),
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceMenuState {
    pub(crate) menu: WorkspaceMenu,
    selected: usize,
}

impl WorkspaceMenuState {
    pub(crate) fn open(menu: WorkspaceMenu) -> Self {
        Self { menu, selected: 0 }
    }

    pub(crate) fn moved(&mut self, delta: isize) {
        let count = action_count(&self.menu.entries);
        let current = self.selected % count;
        let step = delta.unsigned_abs() % count;
        self.selected = if delta < 0 {
            (current + count - step) % count
        } else {
            (current + step) % count
        };
    }

    pub(crate) fn selected_action(&self) -> WorkspaceAction {
        let selected = self.selected % action_count(&self.menu.entries);
        if selected == 0 {
            return WorkspaceAction::Add;
        }
        saved_indices(&self.menu.entries)
            .nth(selected - 1)
            .map_or(WorkspaceAction::Clear, WorkspaceAction::Remove)
    }

    pub(crate) fn selected_row(&self) -> usize {
        match self.selected_action() {
            WorkspaceAction::Add => 0,
            WorkspaceAction::Remove(index) => index + 1,
            WorkspaceAction::Clear => self.menu.entries.len() + 1,
        }
    }
}

pub(crate) fn action_count(entries: &[WorkspaceMenuEntry]) -> usize {
    let saved = saved_indices(entries).count();
    1 + saved + usize::from(saved > 0)
}

pub(crate) fn row_count(entries: &[WorkspaceMenuEntry]) -> usize {
    1 + entries.len() + usize::from(saved_indices(entries).next().is_some())
}

fn saved_indices(entries: &[WorkspaceMenuEntry]) -> impl Iterator<Item = usize> + '_ {
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.saved)
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ofx_contract::DirectoryAccess;

    use super::*;

    fn entry(path: &str, saved: bool, command_line: bool) -> WorkspaceMenuEntry {
        WorkspaceMenuEntry {
            path: PathBuf::from(path),
            saved,
            command_line,
            access: DirectoryAccess::Active,
        }
    }

    fn opened(entries: Vec<WorkspaceMenuEntry>) -> WorkspaceMenuState {
        WorkspaceMenuState::open(WorkspaceMenu {
            primary: PathBuf::from("/tmp/project"),
            saved_suppressed: false,
            limit: 16,
            entries,
        })
    }

    #[test]
    fn the_menu_maps_compact_rows_to_existing_command_actions() {
        let mut state = opened(vec![
            entry("/tmp/saved-a", true, false),
            entry("/tmp/saved-b", true, false),
        ]);
        assert_eq!(state.selected_action(), WorkspaceAction::Add);
        for expected in [
            WorkspaceAction::Remove(0),
            WorkspaceAction::Remove(1),
            WorkspaceAction::Clear,
            WorkspaceAction::Add,
        ] {
            state.moved(1);
            assert_eq!(state.selected_action(), expected);
        }
        state.moved(-1);
        assert_eq!(state.selected_action(), WorkspaceAction::Clear);
        let empty = opened(Vec::new());
        assert_eq!(action_count(&empty.menu.entries), 1);
        assert_eq!(empty.selected_action(), WorkspaceAction::Add);
    }

    #[test]
    fn the_menu_shows_launch_roots_without_offering_invalid_remove_actions() {
        let mut state = opened(vec![
            entry("/tmp/launch-only", false, true),
            entry("/tmp/saved", true, false),
        ]);
        assert_eq!(action_count(&state.menu.entries), 3);
        assert_eq!(row_count(&state.menu.entries), 4);
        state.moved(1);
        assert_eq!(state.selected_action(), WorkspaceAction::Remove(1));
        assert_eq!(state.selected_row(), 2);
        state.moved(1);
        assert_eq!(state.selected_action(), WorkspaceAction::Clear);
        assert_eq!(state.selected_row(), 3);
        state.moved(-5);
        assert_eq!(state.selected_action(), WorkspaceAction::Add);
    }
}
