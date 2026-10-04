use ofx_text::contains_ignore_case;

use super::SlashCommandSpec;
use crate::list_window::update_edge_start;

const QUERY_SEPARATORS: [char; 4] = [' ', '\t', '\r', '\n'];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HelpMenu {
    category: Option<usize>,
    selected: usize,
    window_start: usize,
}

impl HelpMenu {
    pub(crate) fn category(self) -> Option<usize> {
        self.category
    }

    pub(crate) fn selected(self) -> usize {
        self.selected
    }

    pub(crate) fn window_start(self) -> usize {
        self.window_start
    }

    pub(crate) fn selected_match(self, matches: &[usize]) -> Option<usize> {
        if matches.is_empty() {
            return None;
        }
        Some(matches[self.selected % matches.len()])
    }

    pub(crate) fn reset_for_query(&mut self) {
        self.selected = 0;
        self.window_start = 0;
    }

    pub(crate) fn cycle_category(&mut self, delta: isize, categories: usize) {
        let tabs = (categories + 1).cast_signed();
        let current = self
            .category
            .map_or(0, |category| category + 1)
            .cast_signed();
        let next = (current + delta).rem_euclid(tabs).cast_unsigned();
        self.category = next.checked_sub(1);
        self.reset_for_query();
    }

    pub(crate) fn move_selection(&mut self, delta: isize, count: usize, visible: usize) -> bool {
        if count == 0 {
            return false;
        }
        let next = (self.selected % count).cast_signed() + delta;
        self.selected = if next < 0 {
            count - 1
        } else if next.cast_unsigned() >= count {
            0
        } else {
            next.cast_unsigned()
        };
        self.window_start =
            update_edge_start(self.window_start, count, self.selected, visible.max(1));
        true
    }
}

pub(crate) fn help_matches(
    specs: &[SlashCommandSpec],
    labels: &[String],
    category: Option<usize>,
    query: &str,
) -> Vec<usize> {
    let categories = match category {
        Some(category) => category..category + 1,
        None => 0..labels.len(),
    };
    categories
        .flat_map(|category| {
            specs
                .iter()
                .enumerate()
                .filter(move |(_, spec)| spec.category == category)
        })
        .filter(|(_, spec)| {
            let label = labels.get(spec.category).map_or("", String::as_str);
            spec_matches(spec, label, query)
        })
        .map(|(index, _)| index)
        .collect()
}

fn spec_matches(spec: &SlashCommandSpec, label: &str, query: &str) -> bool {
    query
        .split(QUERY_SEPARATORS)
        .filter(|token| !token.is_empty())
        .all(|token| {
            [
                spec.command.as_str(),
                spec.help_entry.as_str(),
                spec.description.as_str(),
                label,
            ]
            .into_iter()
            .chain(spec.aliases.iter().map(String::as_str))
            .any(|text| contains_ignore_case(text, token))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(
        command: &str,
        help_entry: &str,
        description: &str,
        category: usize,
    ) -> SlashCommandSpec {
        SlashCommandSpec {
            command: command.to_owned(),
            aliases: if command == "/quit" {
                vec!["/exit".to_owned()]
            } else {
                Vec::new()
            },
            description: description.to_owned(),
            help_entry: help_entry.to_owned(),
            takes_arguments: help_entry != command,
            category,
            compacts: false,
        }
    }

    fn specs() -> Vec<SlashCommandSpec> {
        vec![
            spec("/help", "/help", "show available slash commands", 0),
            spec("/new", "/new", "start a fresh session", 1),
            spec("/model", "/model <id-or-query>", "choose a model", 2),
            spec("/status", "/status", "show runtime configuration", 0),
            spec("/quit", "/quit", "exit the interactive shell", 0),
        ]
    }

    fn labels() -> Vec<String> {
        ["General", "Session", "Model"].map(str::to_owned).to_vec()
    }

    #[test]
    fn all_lists_the_commands_in_category_order_and_a_category_only_its_own() {
        assert_eq!(help_matches(&specs(), &labels(), None, ""), [0, 3, 4, 1, 2]);
        assert_eq!(help_matches(&specs(), &labels(), Some(0), ""), [0, 3, 4]);
        assert_eq!(help_matches(&specs(), &labels(), Some(2), ""), [2]);
        assert!(help_matches(&specs(), &labels(), Some(7), "").is_empty());
    }

    #[test]
    fn every_query_token_must_match_a_command_entry_description_label_or_alias() {
        let found = |query: &str| help_matches(&specs(), &labels(), None, query);
        assert_eq!(found("SESSION"), [1]);
        assert_eq!(found("id-or"), [2]);
        assert_eq!(found("show runtime"), [3]);
        assert!(found("show nothing").is_empty());
        assert_eq!(found("exit"), [4]);
        assert_eq!(found("  \t"), [0, 3, 4, 1, 2]);
    }

    #[test]
    fn categories_cycle_through_all_and_back_and_reset_the_selection() {
        let mut menu = HelpMenu::default();
        menu.move_selection(1, 5, 5);
        menu.cycle_category(1, 3);
        assert_eq!((menu.category(), menu.selected()), (Some(0), 0));
        menu.cycle_category(3, 3);
        assert_eq!(menu.category(), None);
        menu.cycle_category(-1, 3);
        assert_eq!(menu.category(), Some(2));
    }

    #[test]
    fn the_selection_wraps_keeps_the_window_on_it_and_picks_from_the_matches() {
        let mut menu = HelpMenu::default();
        assert!(!menu.move_selection(1, 0, 3));
        assert!(menu.move_selection(-1, 30, 3));
        assert_eq!((menu.selected(), menu.window_start()), (29, 27));
        assert_eq!(menu.selected_match(&[4, 9]), Some(9));
        assert_eq!(menu.selected_match(&[]), None);
        menu.move_selection(1, 30, 3);
        assert_eq!((menu.selected(), menu.window_start()), (0, 0));
    }
}
