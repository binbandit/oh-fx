use super::Shell;
use super::help_menu::{HelpMenu, help_matches};
use crate::footer::help_menu_presentation::{
    HelpMenuView, MAX_INLINE_ROWS, help_menu_band, visible_item_rows,
};
use crate::input::COMPOSER_INPUT_LIMIT_BYTES;
use crate::row_text::Row;

impl Shell<'_> {
    pub(super) fn open_help_menu(&mut self) {
        if self.model_menu.is_some() || self.model_draft.is_some() || self.picker.is_some() {
            return;
        }
        self.skills_menu = None;
        self.help_menu = Some(HelpMenu::default());
        self.invalidate();
    }

    pub(super) fn help_menu_band(&self) -> Option<Vec<Row>> {
        let menu = self.help_menu?;
        let matches = self.help_matches(menu);
        Some(help_menu_band(
            &self.help_view(menu, &matches),
            self.menu_budget(MAX_INLINE_ROWS),
            self.cols(),
            &self.theme,
        ))
    }

    pub(super) fn move_help_menu(&mut self, delta: isize) -> bool {
        let Some(mut menu) = self.help_menu else {
            return false;
        };
        let matches = self.help_matches(menu);
        let visible = visible_item_rows(
            &self.help_view(menu, &matches),
            self.menu_budget(MAX_INLINE_ROWS),
        );
        let moved = menu.move_selection(delta, matches.len(), visible);
        self.help_menu = Some(menu);
        moved
    }

    pub(super) fn cycle_help_menu_category(&mut self, delta: isize) -> bool {
        let categories = self.options.command_categories.len();
        let Some(menu) = &mut self.help_menu else {
            return false;
        };
        menu.cycle_category(delta, categories);
        true
    }

    pub(super) fn cancel_help_menu(&mut self) -> bool {
        if self.help_menu.take().is_none() {
            return false;
        }
        self.composer.clear();
        true
    }

    pub(super) fn submit_help_menu_selection(&mut self) -> bool {
        let Some(menu) = self.help_menu else {
            return false;
        };
        if self.composer_names_a_command() {
            self.help_menu = None;
            return false;
        }
        let Some(index) = menu.selected_match(&self.help_matches(menu)) else {
            return true;
        };
        let spec = &self.options.commands[index];
        let (command, takes_arguments) = (spec.command.clone(), spec.takes_arguments);
        if command.len() + usize::from(takes_arguments) > COMPOSER_INPUT_LIMIT_BYTES {
            return true;
        }
        self.help_menu = None;
        self.composer.clear();
        self.insert(&command);
        if takes_arguments {
            self.insert(" ");
        } else {
            self.submit();
        }
        true
    }

    pub(super) fn help_menu_edited(&mut self) {
        if let Some(menu) = &mut self.help_menu {
            menu.reset_for_query();
        }
    }

    fn composer_names_a_command(&self) -> bool {
        let text = self.composer.text();
        let command = if text.ends_with([' ', '\t']) {
            text.trim_end_matches([' ', '\t'])
        } else {
            text
        };
        self.options.commands.iter().any(|spec| {
            spec.command == command || spec.aliases.iter().any(|alias| alias == command)
        })
    }

    fn help_matches(&self, menu: HelpMenu) -> Vec<usize> {
        help_matches(
            &self.options.commands,
            &self.options.command_categories,
            menu.category(),
            self.composer.text(),
        )
    }

    fn help_view<'a>(&'a self, menu: HelpMenu, matches: &'a [usize]) -> HelpMenuView<'a> {
        HelpMenuView {
            specs: &self.options.commands,
            labels: &self.options.command_categories,
            matches,
            menu,
        }
    }
}

#[cfg(test)]
mod tests;
