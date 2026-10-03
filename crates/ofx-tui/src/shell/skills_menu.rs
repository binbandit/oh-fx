use ofx_contract::{SkillMenuFocus, SkillMenuGroup, SkillMenuItem, SkillMenuSource};
use ofx_text::contains_ignore_case;

use crate::list_window::update_edge_start;

const MATCH_RANKS: usize = 3;
const QUERY_TRIM: [char; 5] = [' ', '\t', '\r', '\n', '/'];

pub(crate) const SOURCE_FILTERS: [Option<SkillMenuSource>; 8] = [
    None,
    Some(SkillMenuSource::OhFx),
    Some(SkillMenuSource::Workspace),
    Some(SkillMenuSource::Claude),
    Some(SkillMenuSource::Codex),
    Some(SkillMenuSource::Agents),
    Some(SkillMenuSource::OpenCode),
    Some(SkillMenuSource::Claw),
];

pub(crate) fn filter_label(filter: Option<SkillMenuSource>) -> &'static str {
    match filter {
        None => "All",
        Some(SkillMenuSource::OhFx) => "oh-fx",
        Some(SkillMenuSource::Workspace) => "Workspace",
        Some(SkillMenuSource::OpenCode) => "OpenCode",
        Some(SkillMenuSource::Codex) => "Codex",
        Some(SkillMenuSource::Claude) => "Claude",
        Some(SkillMenuSource::Agents) => "Agents",
        Some(SkillMenuSource::Claw) => "Claw",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MentionAnchor {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillsMenu {
    items: Vec<SkillMenuItem>,
    filter: usize,
    query: String,
    selected: usize,
    window_start: usize,
    mention: Option<MentionAnchor>,
}

impl SkillsMenu {
    pub(crate) fn open(items: Vec<SkillMenuItem>, focus: &SkillMenuFocus) -> Option<Self> {
        let mut menu = Self {
            items,
            filter: 0,
            query: String::new(),
            selected: 0,
            window_start: 0,
            mention: None,
        };
        match focus {
            SkillMenuFocus::Start => {}
            SkillMenuFocus::Query(query) => query.clone_into(&mut menu.query),
            SkillMenuFocus::Item(index) => {
                let source = menu.items.get(*index)?.source;
                menu.filter = SOURCE_FILTERS
                    .iter()
                    .position(|filter| *filter == Some(source))?;
                menu.selected = menu.matches().iter().position(|item| item == index)?;
            }
        }
        Some(menu)
    }

    pub(crate) fn mention(items: Vec<SkillMenuItem>, anchor: MentionAnchor, query: &str) -> Self {
        Self {
            items,
            filter: 0,
            query: query.to_owned(),
            selected: 0,
            window_start: 0,
            mention: Some(anchor),
        }
    }

    pub(crate) fn mention_anchor(&self) -> Option<MentionAnchor> {
        self.mention
    }

    pub(crate) fn set_mention_end(&mut self, end: usize) {
        if let Some(anchor) = &mut self.mention {
            anchor.end = end;
        }
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.mention.is_none() || self.matches_any_source()
    }

    fn matches_any_source(&self) -> bool {
        let query = self.query.trim_matches(QUERY_TRIM);
        self.items
            .iter()
            .any(|item| match_rank(item, query).is_some())
    }

    pub(crate) fn filter(&self) -> Option<SkillMenuSource> {
        SOURCE_FILTERS[self.filter]
    }

    pub(crate) fn catalog_is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn window_start(&self) -> usize {
        self.window_start
    }

    pub(crate) fn item(&self, index: usize) -> &SkillMenuItem {
        &self.items[index]
    }

    pub(crate) fn matches(&self) -> Vec<usize> {
        let query = self.query.trim_matches(QUERY_TRIM);
        let filter = self.filter();
        let mut ranked: Vec<(usize, SkillMenuGroup, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| filter.is_none_or(|source| item.source == source))
            .filter_map(|(index, item)| {
                match_rank(item, query).map(|rank| (rank, item.group, index))
            })
            .collect();
        ranked.sort_unstable();
        ranked.into_iter().map(|(_, _, index)| index).collect()
    }

    pub(crate) fn selected_item(&self) -> Option<&SkillMenuItem> {
        let matches = self.matches();
        let index = *matches.get(self.selected % matches.len().max(1))?;
        Some(&self.items[index])
    }

    pub(crate) fn set_query(&mut self, query: &str, visible_rows: usize) {
        if self.query == query {
            return;
        }
        query.clone_into(&mut self.query);
        self.clamp(visible_rows);
    }

    pub(crate) fn move_selection(&mut self, delta: isize, visible_rows: usize) -> bool {
        let count = self.matches().len();
        if count == 0 {
            return false;
        }
        let current = self.selected % count;
        self.selected = current.saturating_add_signed(delta).min(count - 1);
        self.window_start =
            update_edge_start(self.window_start, count, self.selected, visible_rows.max(1));
        true
    }

    pub(crate) fn cycle_filter(&mut self, delta: isize, visible_rows: usize) {
        let last = SOURCE_FILTERS.len() - 1;
        self.filter = match self.filter.checked_add_signed(delta) {
            None => last,
            Some(next) if next > last => 0,
            Some(next) => next,
        };
        self.selected = 0;
        self.window_start = 0;
        self.clamp(visible_rows);
    }

    fn clamp(&mut self, visible_rows: usize) {
        let count = self.matches().len();
        if count == 0 {
            self.selected = 0;
            self.window_start = 0;
            return;
        }
        self.selected = self.selected.min(count - 1);
        self.window_start =
            update_edge_start(self.window_start, count, self.selected, visible_rows);
    }
}

fn match_rank(item: &SkillMenuItem, query: &str) -> Option<usize> {
    if query.is_empty()
        || item
            .name
            .as_bytes()
            .get(..query.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(query.as_bytes()))
    {
        return Some(0);
    }
    if contains_ignore_case(&item.name, query) {
        return Some(1);
    }
    let elsewhere = contains_ignore_case(&item.description, query)
        || contains_ignore_case(&item.source_label, query)
        || contains_ignore_case(item.path.as_os_str().as_encoded_bytes(), query);
    elsewhere.then_some(MATCH_RANKS - 1)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn item(name: &str, source: SkillMenuSource, group: SkillMenuGroup) -> SkillMenuItem {
        SkillMenuItem {
            name: name.to_owned(),
            description: format!("{name} helps"),
            path: PathBuf::from(format!("/skills/{name}")),
            source,
            group,
            scope: "oh-fx · Workspace".to_owned(),
            source_label: "workspace .oh-fx".to_owned(),
        }
    }

    fn names(menu: &SkillsMenu) -> Vec<&str> {
        menu.matches()
            .into_iter()
            .map(|index| menu.item(index).name.as_str())
            .collect()
    }

    fn catalog() -> Vec<SkillMenuItem> {
        vec![
            item(
                "zeta",
                SkillMenuSource::Claude,
                SkillMenuGroup::Compatibility,
            ),
            item("review", SkillMenuSource::OhFx, SkillMenuGroup::Workspace),
            item("managed", SkillMenuSource::OhFx, SkillMenuGroup::Managed),
            item(
                "prereview",
                SkillMenuSource::Codex,
                SkillMenuGroup::Compatibility,
            ),
        ]
    }

    #[test]
    fn matches_order_by_match_quality_then_source_group_then_discovery() {
        let mut menu = SkillsMenu::open(catalog(), &SkillMenuFocus::Start).unwrap();
        assert_eq!(names(&menu), ["managed", "review", "zeta", "prereview"]);
        menu.set_query("REV", 4);
        assert_eq!(names(&menu), ["review", "prereview"]);
        menu.set_query("/helps ", 4);
        assert_eq!(names(&menu), ["managed", "review", "zeta", "prereview"]);
        menu.set_query("skills/zeta", 4);
        assert_eq!(names(&menu), ["zeta"]);
        menu.set_query("nothing", 4);
        assert!(menu.selected_item().is_none());
    }

    #[test]
    fn source_tabs_cycle_in_upstream_order_and_reset_the_selection() {
        let mut menu = SkillsMenu::open(catalog(), &SkillMenuFocus::Start).unwrap();
        assert!(menu.move_selection(2, 4));
        menu.cycle_filter(1, 4);
        assert_eq!(menu.filter(), Some(SkillMenuSource::OhFx));
        assert_eq!(menu.selected(), 0);
        assert_eq!(names(&menu), ["managed", "review"]);
        menu.cycle_filter(1, 4);
        menu.cycle_filter(1, 4);
        assert_eq!(menu.filter(), Some(SkillMenuSource::Claude));
        assert_eq!(names(&menu), ["zeta"]);
        menu.cycle_filter(-1, 4);
        menu.cycle_filter(-1, 4);
        menu.cycle_filter(-1, 4);
        assert_eq!(menu.filter(), None);
        menu.cycle_filter(-1, 4);
        assert_eq!(menu.filter(), Some(SkillMenuSource::Claw));
        menu.cycle_filter(1, 4);
        assert_eq!(menu.filter(), None);
        menu.cycle_filter(-1, 4);
        assert!(!menu.move_selection(1, 4));
        assert_eq!(
            SOURCE_FILTERS.map(filter_label),
            [
                "All",
                "oh-fx",
                "Workspace",
                "Claude",
                "Codex",
                "Agents",
                "OpenCode",
                "Claw"
            ]
        );
    }

    #[test]
    fn selection_clamps_at_both_ends_and_scrolls_its_window() {
        let mut menu = SkillsMenu::open(catalog(), &SkillMenuFocus::Start).unwrap();
        assert!(menu.move_selection(-1, 2));
        assert_eq!(menu.selected(), 0);
        assert!(menu.move_selection(3, 2));
        assert_eq!((menu.selected(), menu.window_start()), (3, 2));
        assert!(menu.move_selection(9, 2));
        assert_eq!(menu.selected_item().unwrap().name, "prereview");
        assert!(menu.move_selection(-2, 2));
        assert_eq!((menu.selected(), menu.window_start()), (1, 1));
        menu.set_query("rev", 2);
        assert_eq!(menu.selected(), 1);
        assert_eq!(menu.selected_item().unwrap().name, "prereview");
    }

    #[test]
    fn focusing_an_item_opens_its_source_tab_on_it() {
        let menu = SkillsMenu::open(catalog(), &SkillMenuFocus::Item(1)).unwrap();
        assert_eq!(menu.filter(), Some(SkillMenuSource::OhFx));
        assert_eq!(menu.selected_item().unwrap().name, "review");
        let menu = SkillsMenu::open(catalog(), &SkillMenuFocus::Query("rev".to_owned())).unwrap();
        assert_eq!(names(&menu), ["review", "prereview"]);
        assert!(SkillsMenu::open(catalog(), &SkillMenuFocus::Item(9)).is_none());
        assert!(
            SkillsMenu::open(Vec::new(), &SkillMenuFocus::Start)
                .unwrap()
                .catalog_is_empty()
        );
    }
}
