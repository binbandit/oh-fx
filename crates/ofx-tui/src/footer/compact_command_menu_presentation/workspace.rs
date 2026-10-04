use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ofx_contract::{DirectoryAccess, WorkspaceMenu, WorkspaceMenuEntry};
use ofx_text::{encode_terminal_safe, prefix_by_width, visible_width};

use crate::row_text::{Row, single_line_ellipsized};
use crate::shell::workspace_menu::row_count;
use crate::theme::Theme;

const PINNED_ROWS: usize = 5;
const COLUMN_GAP: usize = 4;
const TITLE: &str = "Workspace";
const PRIMARY_LABEL: &str = "Primary";
const SUMMARY_LABEL: &str = "Additional directories";
const ADD_LABEL: &str = "Add directory…";
const ADD_INFO: &str = "Grant access to another directory";
const CLEAR_LABEL: &str = "Clear saved directories";
const CLEAR_INFO: &str = "Remove every saved additional directory";
const HINTS: [&str; 3] = [
    "↑↓ navigate     enter use     esc close",
    "↑↓ move  enter use  esc",
    "enter esc",
];

pub(crate) fn workspace_desired_row_count(menu: &WorkspaceMenu) -> usize {
    PINNED_ROWS + row_count(&menu.entries)
}

pub(crate) fn workspace_menu_rows(
    theme: &Theme,
    menu: &WorkspaceMenu,
    selected_row: usize,
    visible_rows: usize,
    width: usize,
) -> Vec<Row> {
    if width == 0 {
        return vec![Row::new(); visible_rows];
    }
    let choices = Choices {
        theme,
        menu,
        selected_row,
        width,
    };
    (0..visible_rows)
        .map(|row_index| {
            if visible_rows < PINNED_ROWS + 1 {
                let header_rows = usize::from(visible_rows > 1);
                return match row_index {
                    0 if header_rows == 1 => title_row(theme, width),
                    index if index < header_rows => Row::new(),
                    index => choices.row(index - header_rows, visible_rows - header_rows),
                };
            }
            match row_index {
                0 => title_row(theme, width),
                2 => label_value_row(
                    theme,
                    PRIMARY_LABEL,
                    &safe_path(&menu.primary),
                    summary_value_column(width),
                    width,
                ),
                3 => label_value_row(
                    theme,
                    SUMMARY_LABEL,
                    &summary(menu),
                    summary_value_column(width),
                    width,
                ),
                1 | 4 => Row::new(),
                index => choices.row(index - PINNED_ROWS, visible_rows - PINNED_ROWS),
            }
        })
        .collect()
}

pub(crate) fn workspace_menu_hint_row(theme: &Theme, width: usize) -> Row {
    let hint = HINTS
        .into_iter()
        .find(|hint| visible_width(hint) <= width)
        .unwrap_or(HINTS[HINTS.len() - 1]);
    Row::styled(hint, theme.dim).clipped(width)
}

struct Choices<'a> {
    theme: &'a Theme,
    menu: &'a WorkspaceMenu,
    selected_row: usize,
    width: usize,
}

impl Choices<'_> {
    fn row(&self, display_row: usize, choice_rows: usize) -> Row {
        let entries = &self.menu.entries;
        let count = row_count(entries);
        if choice_rows == 0 {
            return Row::new();
        }
        let selected = self.selected_row;
        let window_start = selected
            .saturating_sub(choice_rows - 1)
            .min(count.saturating_sub(choice_rows));
        let index = window_start + display_row;
        if index >= count {
            return Row::new();
        }
        let info_column = info_column(entries, self.width);
        let action = |label: &str, info: &str, selected: bool| {
            action_row(self.theme, label, info, selected, info_column, self.width)
        };
        if index == 0 {
            return action(ADD_LABEL, ADD_INFO, selected == 0);
        }
        match entries.get(index - 1) {
            Some(entry) => action(
                &safe_path(&entry.path),
                &entry_status(entry),
                entry.saved && index == selected,
            ),
            None => action(CLEAR_LABEL, CLEAR_INFO, index == selected),
        }
    }
}

fn title_row(theme: &Theme, width: usize) -> Row {
    Row::styled(
        &single_line_ellipsized(TITLE, width),
        theme.selected_completion,
    )
}

fn summary(menu: &WorkspaceMenu) -> String {
    let (count, limit) = (menu.entries.len(), menu.limit);
    if menu.saved_suppressed {
        format!("{count} / {limit} · Saved roots suppressed")
    } else {
        format!("{count} / {limit}")
    }
}

fn entry_status(entry: &WorkspaceMenuEntry) -> String {
    let availability = match entry.access {
        DirectoryAccess::Unavailable => "Unavailable",
        DirectoryAccess::Active => "Active",
        DirectoryAccess::Inactive => "Inactive",
    };
    let source = match (entry.saved, entry.command_line) {
        (true, true) => "Saved + launch",
        (true, false) => "Saved",
        (false, true) => "Launch only",
        (false, false) => "Session",
    };
    format!("{availability} · {source}")
}

fn safe_path(path: &Path) -> String {
    encode_terminal_safe(path.as_os_str().as_bytes(), usize::MAX).text
}

fn indent(width: usize) -> usize {
    if width <= 2 { 0 } else { 2 }
}

fn info_column(entries: &[WorkspaceMenuEntry], width: usize) -> Option<usize> {
    let mut longest_label = visible_width(ADD_LABEL).max(visible_width(CLEAR_LABEL));
    let mut widest_info = visible_width(ADD_INFO).max(visible_width(CLEAR_INFO));
    for entry in entries {
        longest_label = longest_label.max(visible_width(&safe_path(&entry.path)));
        widest_info = widest_info.max(visible_width(&entry_status(entry)));
    }
    if width < indent(width) + 8 + COLUMN_GAP + widest_info {
        return None;
    }
    Some((indent(width) + longest_label + COLUMN_GAP).min(width - widest_info))
}

fn summary_value_column(width: usize) -> usize {
    (indent(width) + visible_width(SUMMARY_LABEL) + COLUMN_GAP).min(width)
}

fn action_row(
    theme: &Theme,
    label: &str,
    info: &str,
    selected: bool,
    info_column: Option<usize>,
    width: usize,
) -> Row {
    let paint = if selected {
        theme.system_notice_label
    } else {
        theme.dim
    };
    let mut row = Row::styled(
        prefix_by_width(if selected { "❯ " } else { "  " }, width),
        paint,
    );
    let used_prefix = row.width();
    if used_prefix >= width {
        return row;
    }
    let label_room = match info_column {
        Some(info_start) if info_start > used_prefix + 2 => info_start - used_prefix - 1,
        _ => width - used_prefix,
    };
    row.push(&single_line_ellipsized(label, label_room), paint);
    let Some(info_start) = info_column.filter(|start| *start > used_prefix + 2) else {
        return row;
    };
    if row.width() >= info_start {
        return row;
    }
    row.push_spaces(info_start - row.width());
    row.push(&single_line_ellipsized(info, width - info_start), theme.dim);
    row
}

fn label_value_row(
    theme: &Theme,
    label: &str,
    value: &str,
    value_column: usize,
    width: usize,
) -> Row {
    let mut row = Row::styled(prefix_by_width("  ", width), theme.system_notice_label);
    let prefix_used = row.width();
    let target = value_column.max(prefix_used + 2).min(width);
    row.push(
        &single_line_ellipsized(label, target.saturating_sub(prefix_used + 1)),
        theme.system_notice_label,
    );
    let used = row.width();
    if used >= width {
        return row;
    }
    let target = target.max(used + 1);
    if target >= width {
        return row;
    }
    row.push_spaces(target - used);
    row.push(&single_line_ellipsized(value, width - target), theme.dim);
    row
}

#[cfg(test)]
mod tests;
