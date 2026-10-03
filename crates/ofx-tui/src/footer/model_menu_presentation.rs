use ofx_contract::{CatalogRetry, ModelCatalogSource, ModelOption};
use ofx_text::visible_width;

use crate::footer::picker_presentation::{middle_ellipsized, single_line_ellipsized};
use crate::list_window::update_edge_start;
use crate::row_text::{Row, terminal_safe};
use crate::shell::model_menu::{CatalogLoad, ModelMenu, VENDOR_TABS, available_tabs};
use crate::theme::Theme;

pub(crate) const MAX_INLINE_ROWS: usize = 23;
const MAX_VISIBLE_ITEMS: usize = 20;
const FIRST_ITEM_ROW: usize = 2;
const MINIMUM_ID_WIDTH: usize = 8;
const FACT_SEPARATOR: &str = " · ";
const OVERFLOW: &str = "…";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Layout {
    visible_items: usize,
    first_item_row: usize,
    show_header: bool,
    state_row: Option<usize>,
    status_row: Option<usize>,
    row_count: usize,
}

impl Layout {
    fn build(ready: bool, match_count: usize, note: bool, budget: usize) -> Self {
        if budget == 0 {
            return Self::default();
        }
        if !ready || match_count == 0 {
            let state_row = budget.min(3) - 1;
            let status_row = (ready && note && budget > state_row + 2).then_some(state_row + 2);
            return Self {
                show_header: budget > 1,
                state_row: Some(state_row),
                status_row,
                row_count: status_row.unwrap_or(state_row) + 1,
                ..Self::default()
            };
        }
        if budget <= 2 {
            return Self {
                visible_items: 1,
                first_item_row: budget - 1,
                show_header: budget == 2,
                row_count: budget,
                ..Self::default()
            };
        }
        let item_budget = budget - FIRST_ITEM_ROW;
        let visible_items = match_count.min(item_budget).min(MAX_VISIBLE_ITEMS);
        let status_row = (note && item_budget - visible_items >= 2)
            .then_some(FIRST_ITEM_ROW + visible_items + 1);
        Self {
            visible_items,
            first_item_row: FIRST_ITEM_ROW,
            show_header: true,
            state_row: None,
            status_row,
            row_count: status_row.map_or(FIRST_ITEM_ROW + visible_items, |row| row + 1),
        }
    }
}

pub(crate) fn visible_items(menu: &ModelMenu, catalog: &CatalogLoad, budget: usize) -> usize {
    layout(menu, catalog, budget).visible_items
}

fn layout(menu: &ModelMenu, catalog: &CatalogLoad, budget: usize) -> Layout {
    let ready = matches!(catalog, CatalogLoad::Listed { .. });
    let matches = menu.matches(catalog.models()).len();
    Layout::build(ready, matches, note(catalog).is_some(), budget)
}

pub(crate) fn model_menu_band(
    menu: &ModelMenu,
    catalog: &CatalogLoad,
    budget: usize,
    width: usize,
    theme: &Theme,
) -> Vec<Row> {
    let layout = layout(menu, catalog, budget);
    let mut band = vec![Row::new(); layout.row_count + 2];
    let rows = &mut band[1..=layout.row_count];
    let models = catalog.models();
    let matches = menu.matches(models);
    if layout.show_header {
        rows[0] = header_row(menu, models, matches.len(), width, theme);
    }
    if let Some(row) = layout.state_row {
        rows[row] = dimmed(state_text(catalog), width, theme);
    }
    if let (Some(row), Some(text)) = (layout.status_row, note(catalog)) {
        rows[row] = dimmed(text, width, theme);
    }
    if layout.visible_items > 0 && !matches.is_empty() {
        let selected = menu.selected % matches.len();
        let start = update_edge_start(
            menu.window_start,
            matches.len(),
            selected,
            layout.visible_items,
        );
        let column = facts_column(models, width);
        for offset in 0..layout.visible_items.min(matches.len() - start) {
            let option = &models[matches[start + offset]];
            rows[layout.first_item_row + offset] =
                title_row(option, start + offset == selected, column, width, theme);
        }
    }
    band
}

fn state_text(catalog: &CatalogLoad) -> &'static str {
    match catalog {
        CatalogLoad::Idle | CatalogLoad::Loading => "Loading models…",
        CatalogLoad::Failed(retry) => retry.map_or("Unable to load models.", retry_text),
        CatalogLoad::Listed { models, .. } if models.is_empty() => "No models available.",
        CatalogLoad::Listed { .. } => "No models found.",
    }
}

fn retry_text(retry: CatalogRetry) -> &'static str {
    match retry {
        CatalogRetry::RateLimited => "AI Gateway rate limited model discovery; retry /model.",
        CatalogRetry::Unreachable => "Could not reach AI Gateway; retry /model.",
    }
}

fn note(catalog: &CatalogLoad) -> Option<&'static str> {
    match catalog {
        CatalogLoad::Listed {
            source: ModelCatalogSource::ProfileSettings,
            ..
        } => Some(
            "Models from profile settings; explicit model IDs do not require catalog discovery.",
        ),
        CatalogLoad::Listed {
            source: ModelCatalogSource::Subscription,
            ..
        } => Some("Codex catalog: authenticated with a subscription."),
        _ => None,
    }
}

fn dimmed(text: &str, width: usize, theme: &Theme) -> Row {
    Row::styled(&single_line_ellipsized(text, width), theme.dim)
}

fn header_row(
    menu: &ModelMenu,
    models: &[ModelOption],
    count: usize,
    width: usize,
    theme: &Theme,
) -> Row {
    let title = format!("Models {count}");
    let tabs = available_tabs(models);
    let active = tabs.iter().position(|tab| *tab == menu.tab).unwrap_or(0);
    let tab_width = |position: usize| {
        visible_width(VENDOR_TABS[tabs[position]]) + if position == active { 2 } else { 0 }
    };
    let range_width = |start: usize, end: usize| {
        let gaps = 2 * (end - start - 1);
        let markers = 3 * (usize::from(start > 0) + usize::from(end < tabs.len()));
        (start..end).map(tab_width).sum::<usize>() + gaps + markers
    };
    let budget = width.saturating_sub(visible_width(&title) + 2);
    let (mut start, mut end) = (active, active + 1);
    if tab_width(active) <= budget {
        loop {
            let mut expanded = false;
            if end < tabs.len() && range_width(start, end + 1) <= budget {
                end += 1;
                expanded = true;
            }
            if start > 0 && range_width(start - 1, end) <= budget {
                start -= 1;
                expanded = true;
            }
            if !expanded {
                break;
            }
        }
    }
    let mut row = Row::styled(&title, theme.selected_completion);
    row.push_spaces(2);
    let fits = tab_width(active) <= budget;
    if fits && start > 0 {
        row.push(OVERFLOW, theme.dim);
        row.push_spaces(2);
    }
    for position in start..end {
        if position > start {
            row.push_spaces(2);
        }
        let label = VENDOR_TABS[tabs[position]];
        if position == active {
            row.push_fmt(format_args!("[{label}]"), theme.selected_completion);
        } else {
            row.push(label, theme.dim);
        }
    }
    if fits && end < tabs.len() {
        row.push_spaces(2);
        row.push(OVERFLOW, theme.dim);
    }
    row.clipped(width)
}

fn facts(option: &ModelOption) -> String {
    let mut facts = Vec::new();
    if let Some(tokens) = option.capabilities.context_window {
        facts.push(token_fact(tokens, "context"));
    }
    if let Some(tokens) = option.max_output_tokens {
        facts.push(token_fact(tokens, "output"));
    }
    if option.capabilities.supports_fast_mode {
        facts.push("Fast".to_owned());
    }
    facts.join(FACT_SEPARATOR)
}

fn token_fact(tokens: u32, suffix: &str) -> String {
    if tokens >= 1_000_000 && tokens.is_multiple_of(1_000_000) {
        format!("{}M {suffix}", tokens / 1_000_000)
    } else if tokens >= 1_000 && tokens.is_multiple_of(1_000) {
        format!("{}K {suffix}", tokens / 1_000)
    } else {
        format!("{tokens} {suffix}")
    }
}

fn indent(width: usize) -> usize {
    if width <= 2 { 0 } else { 2 }
}

fn facts_column(models: &[ModelOption], width: usize) -> Option<usize> {
    let facts_width = models
        .iter()
        .map(|option| visible_width(&facts(option)))
        .max()
        .unwrap_or_default();
    let longest = models
        .iter()
        .map(|option| visible_width(&terminal_safe(&option.id)))
        .fold(MINIMUM_ID_WIDTH, usize::max);
    let indent = indent(width);
    if facts_width == 0 || width < indent + MINIMUM_ID_WIDTH + 2 + facts_width {
        return None;
    }
    Some((indent + longest + 2).min(width - facts_width))
}

fn title_row(
    option: &ModelOption,
    selected: bool,
    column: Option<usize>,
    width: usize,
    theme: &Theme,
) -> Row {
    let indent = indent(width);
    let mut row = Row::new();
    row.push_spaces(indent);
    let facts = facts(option);
    let facts_start = column.unwrap_or(width);
    let show_facts = !facts.is_empty() && facts_start >= indent + MINIMUM_ID_WIDTH + 2;
    let id_budget = if show_facts {
        facts_start - indent - 2
    } else {
        width.saturating_sub(indent)
    };
    let paint = if selected {
        theme.selected_completion
    } else {
        theme.dim
    };
    row.push(
        &middle_ellipsized(&terminal_safe(&option.id), id_budget),
        paint,
    );
    if show_facts {
        row.push_spaces(facts_start.saturating_sub(row.width()));
        row.push(
            &single_line_ellipsized(&facts, width - facts_start),
            theme.dim,
        );
    }
    row
}

#[cfg(test)]
mod tests {
    use ofx_contract::ModelCapabilities;

    use super::*;
    use crate::shell::model_menu::tests::option;

    fn theme() -> Theme {
        Theme::builtin(false, true, true)
    }

    fn listed(models: Vec<ModelOption>, source: ModelCatalogSource) -> CatalogLoad {
        CatalogLoad::Listed { models, source }
    }

    fn rich(id: &str, window: u32, output: Option<u32>, fast: bool) -> ModelOption {
        ModelOption {
            id: id.to_owned(),
            capabilities: ModelCapabilities {
                reasoning_efforts: vec!["high".to_owned()],
                supports_fast_mode: fast,
                context_window: Some(window),
            },
            max_output_tokens: output,
        }
    }

    fn texts(menu: &ModelMenu, catalog: &CatalogLoad, budget: usize, width: usize) -> Vec<String> {
        model_menu_band(menu, catalog, budget, width, &theme())
            .iter()
            .map(Row::text)
            .collect()
    }

    #[test]
    fn the_menu_shows_vendor_tabs_compact_facts_and_the_catalog_note() {
        let catalog = listed(
            vec![
                rich("anthropic/claude-opus-4.8", 1_000_000, Some(128_000), true),
                option("openai/gpt-5"),
            ],
            ModelCatalogSource::Subscription,
        );
        let rows = texts(&ModelMenu::default(), &catalog, 23, 120);
        assert_eq!(
            rows,
            [
                "",
                "Models 2  [All]  Anthropic  OpenAI",
                "",
                "  anthropic/claude-opus-4.8  1M context · 128K output · Fast",
                "  openai/gpt-5",
                "",
                "Codex catalog: authenticated with a subscription.",
                "",
            ]
        );
        let band = model_menu_band(&ModelMenu::default(), &catalog, 23, 120, &theme());
        assert_eq!(band[3].segments()[1].paint, theme().selected_completion);
        assert_eq!(band[4].segments()[1].paint, theme().dim);
        assert_eq!(band[3].segments()[3].paint, theme().dim);
    }

    #[test]
    fn facts_share_one_column_capped_by_the_width() {
        let catalog = listed(
            vec![
                rich("tiny-provider/model", 1_000_000, None, false),
                rich("much-longer-provider/model", 256_000, Some(128_000), false),
            ],
            ModelCatalogSource::ProfileSettings,
        );
        let menu = ModelMenu::default();
        let column = 2 + "much-longer-provider/model".len() + 2;
        for width in [80, 160] {
            let rows = texts(&menu, &catalog, 23, width);
            assert_eq!(rows[3].find("1M context"), Some(column), "{width}");
            assert_eq!(rows[4].find("256K context"), Some(column), "{width}");
        }
        let narrow = texts(&menu, &catalog, 23, 40);
        assert!(
            narrow[4].ends_with("256K context · 128K output"),
            "{narrow:?}"
        );
        assert!(narrow.iter().all(|row| visible_width(row) <= 40));
        assert_eq!(narrow[6], "Models from profile settings; explicit …");
    }

    #[test]
    fn shortened_ids_keep_both_ends_and_never_start_their_tail_with_a_mark() {
        let catalog = listed(
            vec![
                option("provider/very-long-shared-family-production-reasoning-alpha"),
                option("provider/very-long-shared-family-production-reasoning-beta"),
                option("vendor/cafe\u{301}-and-much-more-text-afterwards-e\u{301}x"),
            ],
            ModelCatalogSource::Subscription,
        );
        let rows = texts(&ModelMenu::default(), &catalog, 23, 40);
        assert!(rows[3].starts_with("  provider/very-lo"), "{rows:?}");
        assert!(rows[3].ends_with("reasoning-alpha"), "{rows:?}");
        assert!(rows[4].ends_with("reasoning-beta"), "{rows:?}");
        for row in &rows {
            assert!(visible_width(row) <= 40, "{row:?}");
            assert!(!row.contains("…\u{301}"), "{row:?}");
        }
        let marked = middle_ellipsized("abcdefgh\u{301}ij", 6);
        assert_eq!(marked, "abc…ij");
    }

    #[test]
    fn narrow_headers_keep_the_active_vendor_visible() {
        let catalog = listed(
            vec![
                option("anthropic/a"),
                option("openai/b"),
                option("xai/c"),
                option("zai/d"),
                option("other/e"),
            ],
            ModelCatalogSource::Subscription,
        );
        let mut menu = ModelMenu::default();
        menu.tab = 5;
        let header = |width| texts(&menu, &catalog, 3, width)[1].clone();
        assert_eq!(
            header(80),
            "Models 1  All  Anthropic  OpenAI  xAI  Z.AI  [Others]"
        );
        assert_eq!(header(36), "Models 1  …  xAI  Z.AI  [Others]");
        assert_eq!(header(18), "Models 1  …  [Othe");
        assert_eq!(header(12), "Models 1  [O");
    }

    #[test]
    fn loading_failure_and_empty_states_fill_the_state_row() {
        let menu = ModelMenu::default();
        assert_eq!(
            texts(&menu, &CatalogLoad::Loading, 10, 80),
            ["", "Models 0  [All]", "", "Loading models…", ""]
        );
        let failed = |retry| texts(&menu, &CatalogLoad::Failed(retry), 10, 80)[3].clone();
        assert_eq!(
            failed(Some(CatalogRetry::Unreachable)),
            "Could not reach AI Gateway; retry /model."
        );
        assert_eq!(
            failed(Some(CatalogRetry::RateLimited)),
            "AI Gateway rate limited model discovery; retry /model."
        );
        assert_eq!(failed(None), "Unable to load models.");
        let empty = listed(Vec::new(), ModelCatalogSource::Subscription);
        assert_eq!(
            texts(&menu, &empty, 10, 80),
            [
                "",
                "Models 0  [All]",
                "",
                "No models available.",
                "",
                "Codex catalog: authenticated with a subscription.",
                ""
            ]
        );
        let mut missing = ModelMenu::default();
        missing.set_query("zzz");
        let catalog = listed(vec![option("a/b")], ModelCatalogSource::Subscription);
        assert_eq!(
            texts(&missing, &catalog, 2, 80),
            ["", "Models 0  [All]", "No models found.", ""]
        );
    }

    #[test]
    fn small_budgets_show_the_selection_first_and_inline_browse_caps_at_twenty() {
        let mut models: Vec<ModelOption> = (0..25)
            .map(|index| option(&format!("p/model-{index}")))
            .collect();
        models[24].id = "p/selected-model".to_owned();
        let catalog = listed(models, ModelCatalogSource::Subscription);
        let mut menu = ModelMenu::default();
        menu.selected = 24;
        assert_eq!(visible_items(&menu, &catalog, 40), 20);
        assert_eq!(texts(&menu, &catalog, 40, 120).len(), 26);
        assert_eq!(
            texts(&menu, &catalog, 1, 40),
            ["", "  p/selected-model", ""]
        );
        let two = texts(&menu, &catalog, 2, 40);
        assert_eq!(two[1], "Models 25  [All]");
        assert_eq!(two[2], "  p/selected-model");
        assert_eq!(visible_items(&menu, &catalog, 7), 5);
    }
}
