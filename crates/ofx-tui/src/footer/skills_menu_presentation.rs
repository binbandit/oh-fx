use std::borrow::Cow;

use ofx_text::{prefix_by_width, visible_width};

use crate::list_window::update_edge_start;
use crate::row_text::{Paint, Row, terminal_safe};
use crate::shell::skills_menu::{SOURCE_FILTERS, SkillsMenu, filter_label};
use crate::theme::Theme;

const MAX_MENU_ROWS: usize = 8;
const FIXED_FOOTER_ROWS: usize = 5;
const MINIMUM_TRANSCRIPT_ROWS: usize = 5;
const HEADER_ROWS: usize = 2;
const COLUMN_GAP: usize = 4;
const CTRL_C_EXIT_HINT: &str = "press ctrl+c again to exit";
const HINTS: [&str; 5] = [
    "↑↓ navigate     tab source     enter use     esc close",
    "↑↓ navigate  tab source  enter use  esc close",
    "↑↓ move  tab source  enter  esc",
    "enter use  esc close",
    "enter esc",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MenuLayout {
    show_header: bool,
    visible_items: usize,
    row_count: usize,
}

impl MenuLayout {
    fn build(match_count: usize, budget: usize) -> Self {
        if budget == 0 {
            return Self {
                show_header: false,
                visible_items: 0,
                row_count: 0,
            };
        }
        if match_count == 0 {
            let show_header = budget > HEADER_ROWS;
            return Self {
                show_header,
                visible_items: 0,
                row_count: if show_header { HEADER_ROWS + 1 } else { 1 },
            };
        }
        if budget <= HEADER_ROWS {
            let visible_items = match_count.min(budget);
            return Self {
                show_header: false,
                visible_items,
                row_count: visible_items,
            };
        }
        let visible_items = match_count.min(budget - HEADER_ROWS);
        Self {
            show_header: true,
            visible_items,
            row_count: HEADER_ROWS + visible_items,
        }
    }
}

pub(crate) fn menu_row_budget(
    terminal_rows: usize,
    input_extra: usize,
    banner_rows: usize,
) -> usize {
    let available = terminal_rows
        .saturating_sub(FIXED_FOOTER_ROWS + input_extra + banner_rows)
        .saturating_sub(MINIMUM_TRANSCRIPT_ROWS);
    MAX_MENU_ROWS.min(available.max(1))
}

pub(crate) fn visible_item_rows(menu: &SkillsMenu, budget: usize) -> usize {
    MenuLayout::build(menu.matches().len(), budget).visible_items
}

pub(crate) fn skills_menu_band(
    menu: &SkillsMenu,
    budget: usize,
    width: usize,
    theme: &Theme,
) -> Vec<Row> {
    let rows = skills_menu_rows(menu, budget, width, theme);
    if rows.is_empty() {
        return rows;
    }
    let mut band = Vec::with_capacity(rows.len() + 2);
    band.push(Row::new());
    band.extend(rows);
    band.push(Row::new());
    band
}

fn skills_menu_rows(menu: &SkillsMenu, budget: usize, width: usize, theme: &Theme) -> Vec<Row> {
    let matches = menu.matches();
    let layout = MenuLayout::build(matches.len(), budget);
    let mut rows = Vec::with_capacity(layout.row_count);
    if layout.show_header {
        rows.push(header_row(menu, matches.len(), width, theme));
        rows.push(Row::new());
    }
    if matches.is_empty() {
        if layout.row_count > 0 {
            rows.push(empty_row(menu, width, theme));
        }
        return rows;
    }
    let selected = menu.selected() % matches.len();
    let window_start = update_edge_start(
        menu.window_start(),
        matches.len(),
        selected,
        layout.visible_items,
    );
    let name_column = matches
        .iter()
        .map(|index| visible_width(&terminal_safe(&menu.item(*index).name)))
        .max()
        .unwrap_or_default();
    let visible = &matches[window_start..window_start + layout.visible_items];
    let scope_column = visible
        .iter()
        .map(|index| visible_width(&menu.item(*index).scope))
        .max()
        .unwrap_or_default();
    for (offset, index) in visible.iter().enumerate() {
        let item = menu.item(*index);
        let paint = if window_start + offset == selected {
            theme.selected_completion
        } else {
            theme.dim
        };
        rows.push(item_row(
            &item.name,
            &item.scope,
            Columns {
                name: name_column,
                scope: scope_column,
            },
            paint,
            width,
        ));
    }
    rows
}

pub(crate) fn skills_menu_hint_row(theme: &Theme, width: usize, ctrl_c_pending: bool) -> Row {
    if ctrl_c_pending {
        return Row::styled(CTRL_C_EXIT_HINT, theme.statusline).clipped(width);
    }
    let hint = HINTS
        .into_iter()
        .find(|hint| visible_width(hint) <= width)
        .unwrap_or(HINTS[HINTS.len() - 1]);
    Row::styled(hint, theme.dim).clipped(width)
}

fn header_row(menu: &SkillsMenu, count: usize, width: usize, theme: &Theme) -> Row {
    let title = format!("Skills {count}");
    let active = menu.filter();
    let tab = |row: &mut Row, filter| {
        if filter == active {
            row.push_fmt(
                format_args!("[{}]", filter_label(filter)),
                theme.selected_completion,
            );
        } else {
            row.push(filter_label(filter), theme.dim);
        }
    };
    let mut wide = Row::styled(&title, theme.selected_completion);
    for filter in SOURCE_FILTERS {
        wide.push_spaces(2);
        tab(&mut wide, filter);
    }
    if wide.width() <= width {
        return wide;
    }
    let mut labelled = Row::styled(&title, theme.selected_completion);
    labelled.push_spaces(4);
    labelled.push("Source ", theme.dim);
    tab(&mut labelled, active);
    if labelled.width() <= width {
        return labelled;
    }
    let mut compact = Row::styled(&title, theme.selected_completion);
    compact.push_spaces(2);
    tab(&mut compact, active);
    if compact.width() <= width {
        return compact;
    }
    let mut only = Row::new();
    tab(&mut only, active);
    only.clipped(width)
}

fn empty_row(menu: &SkillsMenu, width: usize, theme: &Theme) -> Row {
    let text: Cow<'_, str> = if menu.catalog_is_empty() {
        Cow::Borrowed("No skills available.")
    } else {
        match menu.filter() {
            None => Cow::Borrowed("No skills found."),
            filter => Cow::Owned(format!("No {} skills found.", filter_label(filter))),
        }
    };
    Row::styled(&single_line_ellipsized(&text, width), theme.dim)
}

#[derive(Debug, Clone, Copy)]
struct Columns {
    name: usize,
    scope: usize,
}

fn item_row(name: &str, scope: &str, columns: Columns, paint: Paint, width: usize) -> Row {
    let mut row = Row::new();
    if width > 4 {
        row.push_spaces(2);
    }
    let prefix = row.width();
    let scope_width = columns.scope.max(visible_width(scope));
    let content_width = width.saturating_sub(1);
    let show_scope =
        scope_width > 0 && content_width >= prefix + columns.name + COLUMN_GAP + scope_width;
    let name_budget = if show_scope {
        columns.name
    } else {
        width.saturating_sub(prefix)
    };
    row.push(
        &single_line_ellipsized(&terminal_safe(name), name_budget),
        paint,
    );
    if show_scope {
        let scope_start = prefix + columns.name + COLUMN_GAP;
        row.push(&" ".repeat(scope_start.saturating_sub(row.width())), paint);
        row.push(scope, paint);
    }
    row
}

fn single_line_ellipsized(text: &str, width: usize) -> Cow<'_, str> {
    let text = if text.contains(['\n', '\r']) {
        Cow::Owned(text.replace(['\n', '\r'], " "))
    } else {
        Cow::Borrowed(text)
    };
    if visible_width(&text) <= width {
        return text;
    }
    match width {
        0 => Cow::Borrowed(""),
        1 => Cow::Borrowed("…"),
        _ => Cow::Owned(format!("{}…", prefix_by_width(&text, width - 1))),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ofx_contract::{SkillMenuFocus, SkillMenuGroup, SkillMenuItem, SkillMenuSource};

    use super::*;

    fn theme() -> Theme {
        Theme::builtin(false, false, true)
    }

    fn item(name: &str, source: SkillMenuSource, scope: &str) -> SkillMenuItem {
        SkillMenuItem {
            name: name.to_owned(),
            description: String::new(),
            path: PathBuf::from(format!("/skills/{name}")),
            source,
            group: SkillMenuGroup::Workspace,
            scope: scope.to_owned(),
            source_label: String::new(),
        }
    }

    fn menu(items: Vec<SkillMenuItem>) -> SkillsMenu {
        SkillsMenu::open(items, &SkillMenuFocus::Start).unwrap()
    }

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(Row::text).collect()
    }

    fn two() -> SkillsMenu {
        menu(vec![
            item("managed", SkillMenuSource::OhFx, "oh-fx · Global"),
            item("workspace", SkillMenuSource::Codex, "Codex · Workspace"),
        ])
    }

    #[test]
    fn the_menu_shows_source_tabs_a_gap_and_one_row_per_skill() {
        let mut menu = two();
        menu.move_selection(1, 6);
        let rows = skills_menu_rows(&menu, 8, 120, &theme());
        assert_eq!(
            texts(&rows),
            [
                "Skills 2  [All]  oh-fx  Workspace  Claude  Codex  Agents  OpenCode  Claw",
                "",
                "  managed      oh-fx · Global",
                "  workspace    Codex · Workspace",
            ]
        );
        assert_eq!(rows[2].segments()[1].paint, theme().dim);
        assert_eq!(rows[3].segments()[1].paint, theme().selected_completion);
    }

    #[test]
    fn narrow_headers_keep_the_active_source_visible() {
        let mut menu = two();
        for _ in 0..6 {
            menu.cycle_filter(1, 6);
        }
        let header = |width| texts(&skills_menu_rows(&menu, 3, width, &theme()))[0].clone();
        assert_eq!(header(40), "Skills 0    Source [OpenCode]");
        assert_eq!(header(25), "Skills 0  [OpenCode]");
        assert_eq!(header(8), "[OpenCod");
        assert_eq!(
            texts(&skills_menu_rows(&menu, 3, 40, &theme()))[2],
            "No OpenCode skills found."
        );
    }

    #[test]
    fn small_budgets_drop_the_header_and_windows_follow_the_selection() {
        let mut menu = two();
        assert_eq!(texts(&skills_menu_rows(&menu, 1, 40, &theme())).len(), 1);
        menu.move_selection(1, 1);
        assert_eq!(
            texts(&skills_menu_rows(&menu, 1, 40, &theme())),
            ["  workspace    Codex · Workspace"]
        );
        assert_eq!(visible_item_rows(&menu, 3), 1);
        assert_eq!(visible_item_rows(&menu, 8), 2);
        assert_eq!(menu_row_budget(24, 0, 0), 8);
        assert_eq!(menu_row_budget(16, 1, 0), 5);
        assert_eq!(menu_row_budget(8, 0, 0), 1);
    }

    #[test]
    fn narrow_rows_drop_the_scope_and_ellipsize_the_name() {
        let menu = two();
        let rows = skills_menu_rows(&menu, 8, 20, &theme());
        assert_eq!(texts(&rows)[2..], ["  managed", "  workspace"]);
        let rows = skills_menu_rows(&menu, 8, 8, &theme());
        assert_eq!(texts(&rows)[3], "  works…");
    }

    #[test]
    fn names_with_bidi_or_control_characters_keep_rows_within_the_width() {
        let menu = menu(vec![
            item("a\u{202e}b", SkillMenuSource::OhFx, "oh-fx · Global"),
            item("plain", SkillMenuSource::Codex, "Codex · Workspace"),
        ]);
        for width in [12, 30, 80] {
            for row in &skills_menu_rows(&menu, 8, width, &theme())[2..] {
                assert!(row.width() <= width, "{width}: {:?}", row.text());
            }
        }
        let rows = texts(&skills_menu_rows(&menu, 8, 80, &theme()));
        let scope = |row: &str| row.find(" · ").unwrap();
        assert_eq!(scope(&rows[2]), scope(&rows[3]), "{rows:?}");
    }

    #[test]
    fn empty_catalogs_and_queries_say_so() {
        let empty = menu(Vec::new());
        assert_eq!(
            texts(&skills_menu_rows(&empty, 8, 80, &theme())),
            [
                "Skills 0  [All]  oh-fx  Workspace  Claude  Codex  Agents  OpenCode  Claw",
                "",
                "No skills available."
            ]
        );
        let mut missing = two();
        missing.set_query("zzz", 6);
        assert_eq!(
            texts(&skills_menu_rows(&missing, 2, 80, &theme())),
            ["No skills found."]
        );
    }

    #[test]
    fn the_hint_row_shortens_with_the_width() {
        let hint = |width| skills_menu_hint_row(&theme(), width, false).text();
        assert_eq!(
            hint(80),
            "↑↓ navigate     tab source     enter use     esc close"
        );
        assert_eq!(hint(40), "↑↓ move  tab source  enter  esc");
        assert_eq!(hint(5), "enter");
        assert_eq!(
            skills_menu_hint_row(&theme(), 80, true).text(),
            "press ctrl+c again to exit"
        );
    }
}
