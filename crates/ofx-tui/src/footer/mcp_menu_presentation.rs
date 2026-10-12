use ofx_contract::{
    McpMenuAddTransport, McpMenuAdmission, McpMenuServer, McpMenuSnapshot, McpMenuSource,
    McpMenuStatus, McpMenuTransport,
};
use ofx_text::{encode_terminal_safe_inline, visible_width};

use crate::row_text::{Paint, Row, single_line_ellipsized};
use crate::shell::mcp_menu::{
    Action, LoadState, McpMenuState, Screen, Section, server_action_available,
};
use crate::theme::Theme;

mod catalogs;

pub(crate) use catalogs::McpMenuCatalogs;

const HEADER_ROWS: usize = 2;
const TEXT_INDENT: usize = 2;
const FACT_VALUE_COLUMN: usize = 22;
const SAFE_TEXT_BYTES: usize = 4096;
const CTRL_C_EXIT_HINT: &str = "press ctrl+c again to exit";
const NO_SERVERS: &str = "No MCP servers configured.";
const NO_AVAILABLE_SERVERS: &str = "No MCP servers available; configuration errors need attention.";
const SERVER_GONE: &str = "MCP server is no longer available.";
const PROJECT_CONFIG: &str = "<workspace>/.mcp.json";
const INFO_HINTS: [&str; 4] = [
    "servers: a add · r reload · enter inspect",
    "project trust: p approve all · z reset",
    "details: enter sign in · l logout · d remove",
    "project details: a approve · x reject",
];
const ROOT_HINTS: [&str; 3] = [
    "↑↓ move  tab section  enter inspect  a add  r reload  c help  esc close",
    "↑↓ move  tab section  enter  a add  r reload  c help  esc",
    "tab enter a r c esc",
];
const CATALOG_HINTS: [&str; 3] = [
    "↑↓ navigate     tab section     / filter     esc back",
    "↑↓ move  tab section  / filter  esc",
    "tab / esc",
];
const SECTIONS: [(Section, &str); 4] = [
    (Section::Servers, "Servers"),
    (Section::Tools, "Tools"),
    (Section::Resources, "Resources"),
    (Section::Prompts, "Prompts"),
];
const ADD_HINTS: [&str; 3] = [
    "type field     enter next/save     tab transport     esc cancel",
    "type  enter next/save  tab transport  esc",
    "enter tab esc",
];
const CONFIRM_HINTS: [&str; 3] = [
    "enter confirm     esc cancel",
    "enter confirm  esc",
    "enter esc",
];
const DETAILS_ACTIONS: [(Action, &str); 5] = [
    (Action::Authenticate, "enter sign in  "),
    (Action::TrustApprove, "a approve  "),
    (Action::TrustReject, "x reject  "),
    (Action::Remove, "d remove  "),
    (Action::Logout, "l logout  "),
];
const INFO_ESCAPES: [&str; 2] = ["esc back", "esc"];
pub(crate) const MAX_INLINE_ROWS: usize = 12;

#[derive(Debug, Clone, Copy)]
pub(crate) struct McpMenuView<'a> {
    pub(crate) state: &'a McpMenuState,
    pub(crate) snapshot: &'a McpMenuSnapshot,
    pub(crate) feedback: Option<&'a str>,
    pub(crate) add: AddFields<'a>,
    pub(crate) catalogs: &'a McpMenuCatalogs,
    pub(crate) loading_visible: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AddFields<'a> {
    pub(crate) name: &'a str,
    pub(crate) target: &'a str,
    pub(crate) arguments: &'a str,
    pub(crate) draft: &'a str,
}

impl<'a> McpMenuView<'a> {
    pub(crate) fn item_count(self) -> usize {
        match self.state.section {
            Section::Servers => self.snapshot.servers.len(),
            section => self.catalogs.count(section, &self.state.query),
        }
    }

    pub(crate) fn selected_server(self) -> Option<&'a McpMenuServer> {
        self.snapshot.servers.get(self.state.selected_server_index)
    }
}

pub(crate) fn menu_row_count(view: McpMenuView<'_>, max_rows: usize) -> usize {
    if max_rows == 0 {
        return 0;
    }
    let header_rows = if max_rows > 2 { HEADER_ROWS } else { 0 };
    let body_budget = max_rows - header_rows;
    if body_budget == 0 {
        return header_rows;
    }
    let body_rows = match view.state.screen {
        Screen::Browse => view.item_count().max(1),
        Screen::Details => view
            .selected_server()
            .map_or(1, |server| 7 + usize::from(server.failure.is_some())),
        Screen::Add if view.state.add_transport == McpMenuAddTransport::Local => 5,
        Screen::Add => 4,
        Screen::Info => 2 + INFO_HINTS.len(),
        Screen::Confirm => 1,
    };
    header_rows + body_rows.min(body_budget)
}

pub(crate) fn visible_items_for_budget(row_budget: usize) -> usize {
    let header_rows = if row_budget > 2 { HEADER_ROWS } else { 0 };
    row_budget.saturating_sub(header_rows).max(1)
}

pub(crate) fn mcp_menu_rows(
    theme: &Theme,
    view: McpMenuView<'_>,
    width: usize,
    row_count: usize,
) -> Vec<Row> {
    (0..row_count)
        .map(|row_index| compose_row(theme, view, row_index, width, row_count))
        .collect()
}

pub(crate) fn mcp_menu_hint_row(
    theme: &Theme,
    view: McpMenuView<'_>,
    width: usize,
    ctrl_c_pending: bool,
) -> Row {
    if ctrl_c_pending {
        return Row::styled(CTRL_C_EXIT_HINT, theme.statusline).clipped(width);
    }
    let details = details_hint(view);
    let variants: [&str; 3] = match view.state.screen {
        Screen::Browse if view.state.section == Section::Servers => ROOT_HINTS,
        Screen::Browse => CATALOG_HINTS,
        Screen::Details => [&details, "c help  esc back", "c esc"],
        Screen::Add => ADD_HINTS,
        Screen::Info => [INFO_ESCAPES[0], INFO_ESCAPES[1], INFO_ESCAPES[1]],
        Screen::Confirm => CONFIRM_HINTS,
    };
    let hint = variants
        .into_iter()
        .find(|candidate| visible_width(candidate) <= width)
        .unwrap_or(variants[variants.len() - 1]);
    Row::styled(hint, theme.dim).clipped(width)
}

fn details_hint(view: McpMenuView<'_>) -> String {
    let mut hint = String::new();
    if let Some(server) = view.selected_server() {
        for (action, label) in DETAILS_ACTIONS {
            if server_action_available(action, server) {
                hint.push_str(label);
            }
        }
    }
    hint.push_str("c help  esc back");
    hint
}

fn compose_row(
    theme: &Theme,
    view: McpMenuView<'_>,
    row_index: usize,
    width: usize,
    row_count: usize,
) -> Row {
    if width == 0 || row_index >= row_count {
        return Row::new();
    }
    let show_header = row_count > 2;
    let body_start = if show_header { HEADER_ROWS } else { 0 };
    if show_header && row_index == 0 {
        return header_row(theme, view, width);
    }
    if show_header && row_index == 1 {
        if let Some(feedback) = view.feedback {
            let paint = if view.state.load_state == LoadState::Failed {
                theme.warning
            } else {
                theme.dim
            };
            return text_row(feedback, width, paint, TEXT_INDENT);
        }
        if !view.state.query.is_empty() {
            return text_row(
                &format!("Filter: {}", view.state.query),
                width,
                theme.dim,
                TEXT_INDENT,
            );
        }
        match view.state.section {
            Section::Servers
                if view.state.screen == Screen::Browse
                    && !view.snapshot.configuration_issues.is_empty() =>
            {
                return configuration_issue_row(theme, &view.snapshot.configuration_issues, width);
            }
            Section::Resources | Section::Prompts => {
                if let Some(server) = view.selected_server() {
                    return fact_row(theme, "Server", &server.name, width);
                }
            }
            Section::Tools => {
                return text_row("All available servers", width, theme.dim, TEXT_INDENT);
            }
            Section::Servers => {}
        }
    }
    if row_index < body_start {
        return Row::new();
    }
    let body_index = row_index - body_start;
    match view.state.screen {
        Screen::Browse if view.state.section != Section::Servers => catalogs::catalog_row(
            theme,
            view,
            body_index,
            width,
            row_count - body_start,
            show_header && view.feedback.is_some(),
        ),
        Screen::Browse => browse_row(theme, view, body_index, width, row_count - body_start),
        Screen::Details => details_row(theme, view, body_index, width),
        Screen::Add => add_row(theme, view, body_index, width),
        Screen::Info => info_row(theme, view, body_index, width),
        Screen::Confirm => text_row(
            confirmation_text(view.state.confirmation_action),
            width,
            theme.warning,
            0,
        ),
    }
}

fn confirmation_text(action: Option<Action>) -> &'static str {
    match action {
        Some(Action::Remove) => "Remove this profile MCP server? press enter to confirm.",
        Some(Action::Logout) => "Log out of this MCP server? press enter to confirm.",
        Some(Action::TrustReject) => "Reject this project MCP server? press enter to confirm.",
        Some(Action::TrustApproveAll) => {
            "Approve all pending project MCP servers? press enter to confirm."
        }
        Some(Action::TrustReset) => "Reset all project MCP choices? press enter to confirm.",
        _ => "Confirm this MCP action before continuing.",
    }
}

fn add_row(theme: &Theme, view: McpMenuView<'_>, row_index: usize, width: usize) -> Row {
    let local = view.state.add_transport == McpMenuAddTransport::Local;
    let add = view.add;
    let field = |index: usize, label: &str, stored| {
        if view.state.add_field_index == index {
            (format!("> {label}"), add.draft)
        } else {
            (label.to_owned(), stored)
        }
    };
    let (label, value) = match (row_index, local) {
        (0, _) => ("Transport".to_owned(), if local { "Local" } else { "HTTP" }),
        (1, _) => field(0, "Name", add.name),
        (2, true) => field(1, "Command", add.target),
        (2, false) => field(1, "URL", add.target),
        (3, true) => field(2, "Arguments", add.arguments),
        (3, false) | (4, true) => ("Scope".to_owned(), "Profile"),
        _ => return Row::new(),
    };
    fact_row(theme, &label, value, width)
}

fn header_row(theme: &Theme, view: McpMenuView<'_>, width: usize) -> Row {
    let mut row = Row::new();
    row.push_fmt(
        format_args!("MCP {}", view.snapshot.servers.len()),
        theme.selected_completion,
    );
    let mut compact = row.clone();
    for (section, label) in SECTIONS {
        row.push_spaces(2);
        if section == view.state.section {
            row.push_fmt(format_args!("[{label}]"), theme.selected_completion);
            compact.push_spaces(2);
            compact.push_fmt(format_args!("[{label}]"), theme.selected_completion);
        } else {
            row.push(label, theme.dim);
        }
    }
    if row.width() <= width {
        row
    } else {
        compact.clipped(width)
    }
}

fn browse_row(
    theme: &Theme,
    view: McpMenuView<'_>,
    body_index: usize,
    width: usize,
    visible_rows: usize,
) -> Row {
    let count = view.item_count();
    if count == 0 {
        if body_index > 0 {
            return Row::new();
        }
        let text = if view.snapshot.configuration_issues.is_empty() {
            NO_SERVERS
        } else {
            NO_AVAILABLE_SERVERS
        };
        return text_row(text, width, theme.dim, TEXT_INDENT);
    }
    let visible = visible_rows.min(count).max(1);
    let start = visible_window_start(
        view.state.window_start,
        view.state.selected_index,
        count,
        visible,
    );
    let index = start + body_index;
    view.snapshot
        .servers
        .get(index)
        .map_or_else(Row::new, |server| {
            server_row(theme, server, index == view.state.selected_index, width)
        })
}

fn visible_window_start(requested: usize, selected: usize, count: usize, visible: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let visible = visible.min(count).max(1);
    let selected = selected.min(count - 1);
    let mut start = requested.min(count - visible);
    if selected < start {
        start = selected;
    }
    if selected >= start + visible {
        start = selected + 1 - visible;
    }
    start.min(count - visible)
}

fn server_row(theme: &Theme, server: &McpMenuServer, selected: bool, width: usize) -> Row {
    let mut row = Row::new();
    if width > 4 {
        row.push_spaces(2);
    }
    let paint = if selected {
        theme.selected_completion
    } else {
        theme.dim
    };
    let name_width = if width >= 80 {
        20
    } else if width >= 56 {
        16
    } else {
        width.saturating_sub(2)
    };
    row.push(&safe_line(&server.name, name_width), paint);
    if width > name_width + 4 {
        row.pad_to_column(name_width + 4);
        let state_width = if width >= 80 {
            20
        } else {
            width - name_width - 4
        };
        row.push(&safe_line(state_label(server), state_width), paint);
    }
    if width >= 80 {
        row.pad_to_column(46);
        row.push(&safe_line(&metadata(server), width - 46), paint);
    }
    row
}

fn state_label(server: &McpMenuServer) -> &'static str {
    if server.reloading {
        return "Reloading";
    }
    if server.admission == Some(McpMenuAdmission::Pending) {
        return "Pending trust";
    }
    match server.status {
        McpMenuStatus::NeedsAuth => "Needs authentication",
        McpMenuStatus::Unavailable => "Disconnected",
        McpMenuStatus::Disabled => "Disabled",
        McpMenuStatus::Connecting => "Connecting",
        McpMenuStatus::Ready => "Ready",
        McpMenuStatus::Failed => "Failed",
    }
}

fn metadata(server: &McpMenuServer) -> String {
    let transport = match server.transport {
        McpMenuTransport::Stdio => "stdio",
        McpMenuTransport::Http => "HTTP",
        McpMenuTransport::Sse => "SSE",
    };
    let source = match server.source {
        McpMenuSource::Profile => "Profile",
        McpMenuSource::Project => "Project",
        McpMenuSource::Acp => "ACP",
    };
    format!("{transport} · {source}")
}

fn details_row(theme: &Theme, view: McpMenuView<'_>, row_index: usize, width: usize) -> Row {
    let Some(server) = view.selected_server() else {
        return text_row(SERVER_GONE, width, theme.warning, TEXT_INDENT);
    };
    let source;
    let capabilities;
    let (label, value) = match row_index {
        0 => ("Server", server.name.as_str()),
        1 => ("State", state_label(server)),
        2 => {
            source = source_detail(server, view.snapshot.profile_config.as_deref());
            ("Source", source.as_str())
        }
        3 => (
            "Transport",
            match server.transport {
                McpMenuTransport::Stdio => "stdio",
                McpMenuTransport::Http => "http",
                McpMenuTransport::Sse => "sse",
            },
        ),
        4 => (
            "Policy",
            if server.required {
                "required"
            } else {
                "optional"
            },
        ),
        5 => (
            "Protocol",
            server.protocol_version.as_deref().unwrap_or("unavailable"),
        ),
        6 => {
            capabilities = capability_summary(server);
            ("Capabilities", capabilities.as_str())
        }
        7 => match &server.failure {
            Some(failure) => ("Error", failure.as_str()),
            None => return Row::new(),
        },
        _ => return Row::new(),
    };
    fact_row(theme, label, value, width)
}

fn source_detail(server: &McpMenuServer, profile_config: Option<&str>) -> String {
    match (server.source, profile_config) {
        (McpMenuSource::Profile, Some(path)) => format!("Profile · {path}"),
        (McpMenuSource::Profile, None) => "Profile".to_owned(),
        (McpMenuSource::Project, _) => "Project · .mcp.json".to_owned(),
        (McpMenuSource::Acp, _) => "ACP session".to_owned(),
    }
}

fn capability_summary(server: &McpMenuServer) -> String {
    let count =
        |value: Option<usize>| value.map_or_else(|| "?".to_owned(), |count| count.to_string());
    let counts = server.counts;
    format!(
        "tools={} resources={} templates={} prompts={}",
        count(counts.tools),
        count(counts.resources),
        count(counts.resource_templates),
        count(counts.prompts)
    )
}

fn info_row(theme: &Theme, view: McpMenuView<'_>, row_index: usize, width: usize) -> Row {
    match row_index {
        0 => fact_row(
            theme,
            "Profile config",
            view.snapshot
                .profile_config
                .as_deref()
                .unwrap_or("mcp.json"),
            width,
        ),
        1 => fact_row(theme, "Project config", PROJECT_CONFIG, width),
        index => INFO_HINTS.get(index - 2).map_or_else(Row::new, |hint| {
            text_row(hint, width, theme.dim, TEXT_INDENT)
        }),
    }
}

fn fact_row(theme: &Theme, label: &str, value: &str, width: usize) -> Row {
    let mut row = Row::new();
    if width > 4 {
        row.push_spaces(2);
    }
    let value_column = FACT_VALUE_COLUMN.min(width);
    row.push(&safe_line(label, value_column.saturating_sub(2)), theme.dim);
    row.pad_to_column(value_column);
    row.push(
        &safe_line(value, width.saturating_sub(value_column)),
        theme.selected_completion,
    );
    row.clipped(width)
}

fn text_row(text: &str, width: usize, paint: Paint, indent: usize) -> Row {
    let mut row = Row::new();
    row.push_spaces(indent.min(width));
    row.push(&safe_line(text, width.saturating_sub(indent)), paint);
    row
}

fn configuration_issue_row(theme: &Theme, issues: &[String], width: usize) -> Row {
    let indent = TEXT_INDENT.min(width);
    let content_width = width - indent;
    let summary = if issues.len() == 1 {
        "1 project MCP configuration error. Run oh-fx mcp list for details. ".to_owned()
    } else {
        format!(
            "{} project MCP configuration errors. Run oh-fx mcp list for details. ",
            issues.len()
        )
    };
    let mut row = Row::new();
    row.push_spaces(indent);
    let summary_width = visible_width(&summary);
    if summary_width >= content_width {
        row.push(&safe_line(&summary, content_width), theme.warning);
    } else {
        row.push(&summary, theme.warning);
        row.push(
            &safe_line(&issues[0], content_width - summary_width),
            theme.warning,
        );
    }
    row
}

fn safe_line(text: &str, width: usize) -> String {
    let safe = encode_terminal_safe_inline(text.as_bytes(), SAFE_TEXT_BYTES).text;
    single_line_ellipsized(&safe, width).into_owned()
}

#[cfg(test)]
mod tests;
