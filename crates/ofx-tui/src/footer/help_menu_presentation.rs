use ofx_text::visible_width;

use crate::footer::picker_presentation::inline_menu_band;
use crate::row_text::{Paint, Row, single_line_ellipsized};
use crate::shell::SlashCommandSpec;
use crate::shell::help_menu::HelpMenu;
use crate::theme::Theme;

const HEADER_ROWS: usize = 2;
const MAX_VISIBLE_ITEMS: usize = 20;
pub(crate) const MAX_INLINE_ROWS: usize = HEADER_ROWS + MAX_VISIBLE_ITEMS;
const COLUMN_GAP: usize = 4;
const TAB_GAP: usize = 2;
const ALL_CATEGORIES: &str = "All";
const PACKED_TABS: &str = "…";
const NO_COMMANDS: &str = "No commands found.";
pub(crate) const HELP_MENU_HINTS: [&str; 5] = [
    "↑↓ navigate     tab category     enter open     esc close",
    "↑↓ navigate  tab category  enter open  esc close",
    "↑↓ move  tab category  enter  esc",
    "tab category  enter open  esc",
    "tab enter esc",
];

#[derive(Debug, Clone, Copy)]
pub(crate) struct HelpMenuView<'a> {
    pub(crate) specs: &'a [SlashCommandSpec],
    pub(crate) labels: &'a [String],
    pub(crate) matches: &'a [usize],
    pub(crate) menu: HelpMenu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MenuLayout {
    show_header: bool,
    first_item: usize,
    visible_items: usize,
}

impl MenuLayout {
    fn build(view: &HelpMenuView<'_>, budget: usize) -> Self {
        let show_header = budget > HEADER_ROWS;
        let body_rows = if show_header {
            budget - HEADER_ROWS
        } else {
            budget
        };
        let count = view.matches.len();
        if count == 0 {
            return Self {
                show_header,
                first_item: 0,
                visible_items: 0,
            };
        }
        let selected = view.menu.selected() % count;
        let capacity = body_rows.min(MAX_VISIBLE_ITEMS);
        let first_item = view
            .menu
            .window_start()
            .min(selected)
            .max((selected + 1).saturating_sub(capacity));
        Self {
            show_header,
            first_item,
            visible_items: (count - first_item).min(capacity),
        }
    }
}

pub(crate) fn visible_item_rows(view: &HelpMenuView<'_>, budget: usize) -> usize {
    MenuLayout::build(view, budget).visible_items.max(1)
}

pub(crate) fn help_menu_band(
    view: &HelpMenuView<'_>,
    budget: usize,
    width: usize,
    theme: &Theme,
) -> Vec<Row> {
    inline_menu_band(help_menu_rows(view, budget, width, theme))
}

fn help_menu_rows(view: &HelpMenuView<'_>, budget: usize, width: usize, theme: &Theme) -> Vec<Row> {
    if budget == 0 || width == 0 {
        return Vec::new();
    }
    let layout = MenuLayout::build(view, budget);
    let mut rows = Vec::with_capacity(HEADER_ROWS + layout.visible_items.max(1));
    if layout.show_header {
        rows.push(header_row(view, width, theme));
        rows.push(Row::new());
    }
    if view.matches.is_empty() {
        rows.push(Row::styled(
            &single_line_ellipsized(NO_COMMANDS, width),
            theme.dim,
        ));
        return rows;
    }
    let indent = if width <= 2 { 0 } else { 2 };
    let widest = view
        .matches
        .iter()
        .map(|index| visible_width(&view.specs[*index].command))
        .max()
        .unwrap_or_default();
    let columns = Columns {
        indent,
        description: (indent + widest + COLUMN_GAP).min(width),
        width,
    };
    let selected = view.menu.selected() % view.matches.len();
    let shown = layout.first_item..layout.first_item + layout.visible_items;
    for (position, index) in view.matches[shown.clone()].iter().zip(shown) {
        let paint = if index == selected {
            theme.selected_completion
        } else {
            theme.dim
        };
        rows.push(command_row(&view.specs[*position], paint, columns));
    }
    rows
}

#[derive(Debug, Clone, Copy)]
struct Columns {
    indent: usize,
    description: usize,
    width: usize,
}

fn command_row(spec: &SlashCommandSpec, paint: Paint, columns: Columns) -> Row {
    let gutter = if columns.description >= columns.indent + COLUMN_GAP {
        COLUMN_GAP
    } else {
        0
    };
    let mut row = Row::new();
    row.push_spaces(columns.indent);
    row.push(
        &single_line_ellipsized(
            &spec.command,
            columns.description.saturating_sub(columns.indent + gutter),
        ),
        paint,
    );
    row.pad_to_column(columns.description);
    row.push(
        &single_line_ellipsized(
            &spec.description,
            columns.width.saturating_sub(columns.description),
        ),
        paint,
    );
    row
}

fn header_row(view: &HelpMenuView<'_>, width: usize, theme: &Theme) -> Row {
    let tabs = view.labels.len() + 1;
    let active = view.menu.category().map_or(0, |category| category + 1);
    let title = || {
        Row::styled(
            &format!("Commands {}", view.matches.len()),
            theme.selected_completion,
        )
    };
    let tab = |row: &mut Row, index: usize| {
        let label = index
            .checked_sub(1)
            .and_then(|category| view.labels.get(category))
            .map_or(ALL_CATEGORIES, String::as_str);
        if index == active {
            row.push_fmt(format_args!("[{label}]"), theme.selected_completion);
        } else {
            row.push(label, theme.dim);
        }
    };
    let spaced_tab = |row: &mut Row, index: usize| {
        row.push_spaces(TAB_GAP);
        tab(row, index);
    };
    let mut wide = title();
    for index in 0..tabs {
        spaced_tab(&mut wide, index);
    }
    if wide.width() <= width {
        return wide;
    }
    for shown in (1..tabs).rev() {
        let mut packed = title();
        for index in 0..shown {
            spaced_tab(&mut packed, index);
        }
        packed.push_spaces(TAB_GAP);
        packed.push(PACKED_TABS, theme.dim);
        if active >= shown {
            spaced_tab(&mut packed, active);
        }
        if packed.width() <= width {
            return packed;
        }
    }
    let mut compact = title();
    spaced_tab(&mut compact, active);
    if compact.width() <= width {
        return compact;
    }
    let mut only = Row::new();
    tab(&mut only, active);
    only.clipped(width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::footer::picker_presentation::menu_hint_row;
    use crate::shell::help_menu::help_matches;

    fn theme() -> Theme {
        Theme::builtin(false, false, true)
    }

    fn labels() -> Vec<String> {
        [
            "General",
            "Session",
            "Account",
            "Model",
            "Appearance",
            "Security",
            "Workspace",
            "Media",
            "Agents",
            "Extensions",
            "Product",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    fn spec(command: &str, description: &str, category: usize) -> SlashCommandSpec {
        SlashCommandSpec {
            command: command.to_owned(),
            aliases: Vec::new(),
            description: description.to_owned(),
            help_entry: command.to_owned(),
            takes_arguments: false,
            category,
            compacts: false,
        }
    }

    fn upstream_specs() -> Vec<SlashCommandSpec> {
        vec![
            spec("/help", "show available slash commands", 0),
            spec(
                "/paste",
                "attach an image from the clipboard when supported",
                7,
            ),
            spec("/status", "show runtime configuration", 0),
        ]
    }

    fn rows(
        specs: &[SlashCommandSpec],
        menu: HelpMenu,
        query: &str,
        budget: usize,
        width: usize,
    ) -> Vec<Row> {
        let labels = labels();
        let matches = help_matches(specs, &labels, menu.category(), query);
        let view = HelpMenuView {
            specs,
            labels: &labels,
            matches: &matches,
            menu,
        };
        help_menu_rows(&view, budget, width, &theme())
    }

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(Row::text).collect()
    }

    fn category(index: usize) -> HelpMenu {
        let mut menu = HelpMenu::default();
        menu.cycle_category((index + 1).cast_signed(), labels().len());
        menu
    }

    #[test]
    fn descriptions_follow_the_widest_matching_command_in_category_order() {
        let rows = rows(&upstream_specs(), HelpMenu::default(), "", 20, 160);
        assert_eq!(
            texts(&rows),
            [
                "Commands 3  [All]  General  Session  Account  Model  Appearance  Security  Workspace  Media  Agents  Extensions  Product",
                "",
                "  /help      show available slash commands",
                "  /status    show runtime configuration",
                "  /paste     attach an image from the clipboard when supported",
            ]
        );
        let header = rows[0].segments();
        assert_eq!(
            (header[0].text.as_str(), header[0].paint),
            ("Commands 3", theme().selected_completion)
        );
        assert_eq!(
            (header[2].text.as_str(), header[2].paint),
            ("[All]", theme().selected_completion)
        );
        assert_eq!(
            (header[4].text.as_str(), header[4].paint),
            ("General", theme().dim)
        );
        let selected = rows[2].segments();
        assert_eq!(selected[1].paint, theme().selected_completion);
        assert_eq!(selected[3].paint, theme().selected_completion);
        assert_eq!(rows[3].segments()[3].paint, theme().dim);
        let narrow = rows_at(22);
        assert_eq!(narrow[2], "  /help      show ava…");
        assert!(narrow.iter().all(|row| row.chars().count() <= 22));
    }

    fn rows_at(width: usize) -> Vec<String> {
        texts(&rows(&upstream_specs(), HelpMenu::default(), "", 20, width))
    }

    #[test]
    fn rows_keep_a_four_column_gutter_and_ellipsize_what_does_not_fit() {
        let specs = vec![
            spec("/permissions", "choose permission behavior", 5),
            spec("/help", "show available slash commands", 5),
        ];
        let at = |width| texts(&rows(&specs, HelpMenu::default(), "", 12, width))[2..].to_vec();
        assert_eq!(
            at(160),
            [
                "  /permissions    choose permission behavior",
                "  /help           show available slash commands"
            ]
        );
        assert_eq!(
            at(24),
            ["  /permissions    choos…", "  /help           show …"]
        );
        assert_eq!(at(10), ["  /pe…    ", "  /he…    "]);
        let multiline = vec![spec("/x", "first\nsecond", 0)];
        assert_eq!(
            texts(&rows(&multiline, HelpMenu::default(), "", 12, 40))[2],
            "  /x    first second"
        );
    }

    #[test]
    fn a_search_shows_only_matching_commands_or_says_none_were_found() {
        let found = rows(&upstream_specs(), HelpMenu::default(), "clipboard", 12, 80);
        assert_eq!(
            texts(&found)[1..],
            [
                "",
                "  /paste    attach an image from the clipboard when supported"
            ]
        );
        assert!(texts(&found)[0].starts_with("Commands 1  [All]"));
        let missing = rows(
            &upstream_specs(),
            HelpMenu::default(),
            "no command can match this",
            12,
            80,
        );
        assert_eq!(texts(&missing)[1..], ["", "No commands found."]);
        let tight = rows(&upstream_specs(), HelpMenu::default(), "no match", 2, 80);
        assert_eq!(texts(&tight), ["No commands found."]);
    }

    #[test]
    fn the_header_packs_tabs_and_keeps_a_far_active_category_visible() {
        let many: Vec<SlashCommandSpec> = (0..37)
            .map(|index| spec("/help", "show", if index == 36 { 10 } else { 0 }))
            .collect();
        let header = |menu: HelpMenu, count: usize, width: usize| {
            texts(&rows(&many[..count], menu, "", 8, width))[0].clone()
        };
        let all = HelpMenu::default();
        assert_eq!(
            header(all, 2, 100),
            "Commands 2  [All]  General  Session  Account  Model  Appearance  Security  Workspace  Media  …"
        );
        assert_eq!(
            header(all, 37, 100),
            "Commands 37  [All]  General  Session  Account  Model  Appearance  Security  Workspace  Media  …"
        );
        assert_eq!(
            header(all, 11, 80),
            "Commands 11  [All]  General  Session  Account  Model  Appearance  Security  …"
        );
        assert_eq!(header(all, 11, 24), "Commands 11  [All]  …");
        assert_eq!(header(all, 11, 18), "Commands 11  [All]");
        assert_eq!(header(all, 11, 4), "[All");
        assert_eq!(
            header(category(10), 37, 100),
            "Commands 1  All  General  Session  Account  Model  Appearance  Security  Workspace  …  [Product]"
        );
        assert_eq!(header(category(10), 37, 22), "Commands 1  [Product]");
        assert_eq!(header(category(0), 37, 6), "[Gener");
    }

    #[test]
    fn the_window_follows_the_selection_and_small_budgets_drop_the_header() {
        let specs: Vec<SlashCommandSpec> = (0..30)
            .map(|index| spec(&format!("/c{index:02}"), "run", 0))
            .collect();
        let mut menu = HelpMenu::default();
        menu.move_selection(-1, 30, 20);
        let shown = texts(&rows(&specs, menu, "", 22, 40));
        assert_eq!(shown.len(), 22);
        assert_eq!(shown[2], "  /c10    run");
        assert_eq!(shown[21], "  /c29    run");
        let labels = labels();
        let matches = help_matches(&specs, &labels, None, "");
        let view = HelpMenuView {
            specs: &specs,
            labels: &labels,
            matches: &matches,
            menu,
        };
        assert_eq!(visible_item_rows(&view, 40), 20);
        assert_eq!(visible_item_rows(&view, 5), 3);
        assert_eq!(visible_item_rows(&view, 2), 2);
        assert_eq!(
            texts(&rows(&specs, menu, "", 2, 40)),
            ["  /c28    run", "  /c29    run"]
        );
        assert_eq!(texts(&help_menu_band(&view, 3, 40, &theme())).len(), 5);
        assert!(help_menu_band(&view, 0, 40, &theme()).is_empty());
    }

    #[test]
    fn the_hint_row_shortens_with_the_width() {
        let hint = |width| menu_hint_row(&theme(), width, false, &HELP_MENU_HINTS).text();
        assert_eq!(
            hint(80),
            "↑↓ navigate     tab category     enter open     esc close"
        );
        assert_eq!(hint(48), "↑↓ navigate  tab category  enter open  esc close");
        assert_eq!(hint(34), "↑↓ move  tab category  enter  esc");
        assert_eq!(hint(30), "tab category  enter open  esc");
        assert_eq!(hint(14), "tab enter esc");
        assert_eq!(hint(5), "tab e");
    }
}
