use ofx_contract::{CatalogRetry, ModelCatalogSource, ModelOption};

use super::picker_state::matches_query;
use crate::list_window::update_edge_start;

pub(crate) const VENDOR_TABS: [&str; 6] = ["All", "Anthropic", "OpenAI", "xAI", "Z.AI", "Others"];
const VENDOR_KEYS: [&str; 4] = ["anthropic", "openai", "xai", "zai"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum CatalogLoad {
    #[default]
    Idle,
    Loading,
    Listed {
        models: Vec<ModelOption>,
        source: ModelCatalogSource,
    },
    Failed(Option<CatalogRetry>),
}

impl CatalogLoad {
    pub(crate) fn models(&self) -> &[ModelOption] {
        match self {
            Self::Listed { models, .. } => models,
            _ => &[],
        }
    }

    pub(crate) fn find(&self, id: &str) -> Option<&ModelOption> {
        self.models().iter().find(|option| option.id == id)
    }
}

pub(crate) fn vendor(id: &str) -> &str {
    id.split_once('/').map_or("", |(vendor, _)| vendor)
}

fn vendor_tab(id: &str) -> usize {
    let vendor = vendor(id);
    VENDOR_KEYS
        .iter()
        .position(|key| vendor.eq_ignore_ascii_case(key))
        .map_or(VENDOR_TABS.len() - 1, |position| position + 1)
}

pub(crate) fn available_tabs(models: &[ModelOption]) -> Vec<usize> {
    let mut seen = [false; VENDOR_TABS.len()];
    for option in models {
        seen[vendor_tab(&option.id)] = true;
    }
    let vendors = seen[1..].iter().filter(|seen| **seen).count();
    (0..VENDOR_TABS.len())
        .filter(|tab| *tab == 0 || (vendors > 1 && seen[*tab]))
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ModelMenu {
    pub(crate) tab: usize,
    pub(crate) selected: usize,
    pub(crate) window_start: usize,
    query: String,
}

impl ModelMenu {
    pub(crate) fn matches(&self, models: &[ModelOption]) -> Vec<usize> {
        models
            .iter()
            .enumerate()
            .filter(|(_, option)| self.tab == 0 || vendor_tab(&option.id) == self.tab)
            .filter(|(_, option)| {
                matches_query(&option.id, &self.query)
                    || matches_query(vendor(&option.id), &self.query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    pub(crate) fn set_query(&mut self, query: &str) {
        if self.query != query {
            query.clone_into(&mut self.query);
            self.reset_selection();
        }
    }

    pub(crate) fn reset_selection(&mut self) {
        self.selected = 0;
        self.window_start = 0;
    }

    pub(crate) fn move_selection(&mut self, models: &[ModelOption], delta: isize, visible: usize) {
        let count = self.matches(models).len();
        if count == 0 {
            return;
        }
        let next = (self.selected % count).checked_add_signed(delta);
        self.selected = match next {
            Some(next) if next < count => next,
            Some(_) => 0,
            None => count - 1,
        };
        self.window_start =
            update_edge_start(self.window_start, count, self.selected, visible.max(1));
    }

    pub(crate) fn move_tab(&mut self, models: &[ModelOption], delta: isize) {
        let tabs = available_tabs(models);
        let current = tabs.iter().position(|tab| *tab == self.tab).unwrap_or(0);
        let next = (current + tabs.len()).saturating_add_signed(delta.signum()) % tabs.len();
        if tabs[next] != self.tab {
            self.tab = tabs[next];
            self.reset_selection();
        }
    }

    pub(crate) fn selected_id<'a>(&self, models: &'a [ModelOption]) -> Option<&'a str> {
        let matches = self.matches(models);
        let index = matches.get(self.selected % matches.len().max(1))?;
        Some(&models[*index].id)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use ofx_contract::ModelCapabilities;

    use super::*;

    pub(crate) fn option(id: &str) -> ModelOption {
        ModelOption {
            id: id.to_owned(),
            capabilities: ModelCapabilities::default(),
            max_output_tokens: None,
        }
    }

    fn catalog(ids: &[&str]) -> Vec<ModelOption> {
        ids.iter().map(|id| option(id)).collect()
    }

    fn ids<'a>(menu: &ModelMenu, models: &'a [ModelOption]) -> Vec<&'a str> {
        menu.matches(models)
            .into_iter()
            .map(|index| models[index].id.as_str())
            .collect()
    }

    #[test]
    fn vendor_tabs_appear_only_for_the_vendors_of_a_mixed_catalog() {
        let mixed = catalog(&["anthropic/claude", "openai/gpt", "deepseek/v3"]);
        assert_eq!(available_tabs(&mixed), [0, 1, 2, 5]);
        assert_eq!(
            available_tabs(&catalog(&["gpt-5.6-sol", "gpt-5.4-mini"])),
            [0]
        );
        assert_eq!(available_tabs(&catalog(&["openai/a", "OpenAI/b"])), [0]);
        assert_eq!(available_tabs(&[]), [0]);
    }

    #[test]
    fn the_query_matches_ids_and_vendors_and_resets_the_selection() {
        let models = catalog(&["anthropic/claude", "openai/gpt-5", "openai/gpt-4o"]);
        let mut menu = ModelMenu::default();
        menu.move_selection(&models, 2, 6);
        assert_eq!(menu.selected, 2);
        menu.set_query(" OPENAI ");
        assert_eq!(menu.selected, 0);
        assert_eq!(ids(&menu, &models), ["openai/gpt-5", "openai/gpt-4o"]);
        menu.set_query("4o");
        assert_eq!(menu.selected_id(&models), Some("openai/gpt-4o"));
        menu.set_query("none");
        assert_eq!(menu.selected_id(&models), None);
    }

    #[test]
    fn selection_wraps_and_tabs_cycle_through_available_vendors() {
        let models = catalog(&["anthropic/a", "openai/b", "openai/c", "x/d"]);
        let mut menu = ModelMenu::default();
        menu.move_selection(&models, -1, 2);
        assert_eq!((menu.selected, menu.window_start), (3, 2));
        menu.move_selection(&models, 1, 2);
        assert_eq!((menu.selected, menu.window_start), (0, 0));
        menu.move_tab(&models, 1);
        assert_eq!((menu.tab, ids(&menu, &models)), (1, vec!["anthropic/a"]));
        menu.move_tab(&models, 1);
        assert_eq!(ids(&menu, &models), ["openai/b", "openai/c"]);
        menu.move_selection(&models, 1, 2);
        menu.move_tab(&models, 1);
        assert_eq!((menu.tab, menu.selected), (5, 0));
        menu.move_tab(&models, 1);
        assert_eq!(menu.tab, 0);
        menu.move_tab(&models, -1);
        assert_eq!(menu.tab, 5);
        let single = catalog(&["a", "b"]);
        let mut menu = ModelMenu::default();
        menu.move_tab(&single, 1);
        assert_eq!(menu.tab, 0);
    }
}
