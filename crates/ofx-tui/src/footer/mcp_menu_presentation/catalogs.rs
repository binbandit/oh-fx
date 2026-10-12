use ofx_contract::{McpMenuPrompt, McpMenuResource};
use ofx_text::contains_ignore_case;

use super::{McpMenuView, TEXT_INDENT, safe_line, text_row, visible_window_start};
use crate::row_text::Row;
use crate::shell::mcp_menu::{LoadState, Section};
use crate::theme::Theme;

const LOADING: &str = "Loading MCP catalog…";
const NO_MATCHES: &str = "No matching MCP items.";
const WIDE_ROW: usize = 76;
const IDENTITY_COLUMNS: usize = 34;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct McpMenuCatalogs {
    pub(crate) tools: Vec<String>,
    pub(crate) resources: Vec<McpMenuResource>,
    pub(crate) prompts: Vec<McpMenuPrompt>,
    pub(crate) resources_server: Option<String>,
    pub(crate) prompts_server: Option<String>,
}

impl McpMenuCatalogs {
    pub(crate) fn count(&self, section: Section, query: &str, server: Option<&str>) -> usize {
        self.listed(section, server)
            .filter(|item| item.matches(query))
            .count()
    }

    pub(crate) fn item(
        &self,
        section: Section,
        query: &str,
        index: usize,
        server: Option<&str>,
    ) -> Option<Item<'_>> {
        self.listed(section, server)
            .filter(|item| item.matches(query))
            .nth(index)
    }

    fn row(
        &self,
        section: Section,
        query: &str,
        index: usize,
        server: Option<&str>,
    ) -> Option<(&str, &str)> {
        self.item(section, query, index, server).map(Item::columns)
    }

    fn listed<'a>(
        &'a self,
        section: Section,
        server: Option<&str>,
    ) -> impl Iterator<Item = Item<'a>> + use<'a> {
        let tools = (section == Section::Tools)
            .then(|| self.tools.iter().map(|tool| Item::Tool(tool.as_str())));
        let resources = (section == Section::Resources
            && self.resources_server.as_deref() == server)
            .then(|| self.resources.iter().map(Item::Resource));
        let prompts = (section == Section::Prompts && self.prompts_server.as_deref() == server)
            .then(|| self.prompts.iter().map(Item::Prompt));
        tools
            .into_iter()
            .flatten()
            .chain(resources.into_iter().flatten())
            .chain(prompts.into_iter().flatten())
    }
}

pub(crate) enum Item<'a> {
    Tool(&'a str),
    Resource(&'a McpMenuResource),
    Prompt(&'a McpMenuPrompt),
}

impl<'a> Item<'a> {
    fn matches(&self, query: &str) -> bool {
        match self {
            Item::Tool(name) => matches(name, query),
            Item::Resource(resource) => {
                matches(&resource.uri, query)
                    || matches(&resource.name, query)
                    || optional_matches(resource.title.as_deref(), query)
                    || optional_matches(resource.description.as_deref(), query)
            }
            Item::Prompt(prompt) => {
                matches(&prompt.name, query)
                    || optional_matches(prompt.title.as_deref(), query)
                    || optional_matches(prompt.description.as_deref(), query)
            }
        }
    }

    fn columns(self) -> (&'a str, &'a str) {
        match self {
            Item::Tool(name) => (name, "Tool"),
            Item::Resource(resource) => (
                &resource.uri,
                resource
                    .title
                    .as_deref()
                    .or(resource.description.as_deref())
                    .unwrap_or(if resource.template {
                        "Template"
                    } else {
                        &resource.name
                    }),
            ),
            Item::Prompt(prompt) => (
                prompt.title.as_deref().unwrap_or(&prompt.name),
                prompt.description.as_deref().unwrap_or("Prompt"),
            ),
        }
    }
}

fn matches(text: &str, query: &str) -> bool {
    query.is_empty() || contains_ignore_case(text, query)
}

fn optional_matches(text: Option<&str>, query: &str) -> bool {
    text.is_some_and(|text| matches(text, query))
}

pub(super) fn catalog_row(
    theme: &Theme,
    view: McpMenuView<'_>,
    body_index: usize,
    width: usize,
    visible_rows: usize,
    feedback_in_header: bool,
) -> Row {
    let state = view.state;
    if state.load_state == LoadState::Loading {
        if !view.loading_visible {
            return Row::new();
        }
        return first_row(body_index, || {
            text_row(LOADING, width, theme.dim, TEXT_INDENT)
        });
    }
    if state.load_state == LoadState::Failed
        && let Some(feedback) = view.feedback
    {
        if feedback_in_header {
            return Row::new();
        }
        return first_row(body_index, || {
            text_row(feedback, width, theme.warning, TEXT_INDENT)
        });
    }
    let count = view.item_count();
    if count == 0 {
        let text = if state.query.is_empty() {
            match state.section {
                Section::Tools => "No MCP tools available.",
                Section::Resources => "No MCP resources available.",
                Section::Servers | Section::Prompts => "No MCP prompts available.",
            }
        } else {
            NO_MATCHES
        };
        return first_row(body_index, || text_row(text, width, theme.dim, TEXT_INDENT));
    }
    let visible = visible_rows.min(count).max(1);
    let index =
        visible_window_start(state.window_start, state.selected_index, count, visible) + body_index;
    view.catalogs
        .row(
            state.section,
            &state.query,
            index,
            view.selected_server().map(|server| server.name.as_str()),
        )
        .map_or_else(Row::new, |(identity, description)| {
            item_row(
                theme,
                identity,
                description,
                index == state.selected_index,
                width,
            )
        })
}

fn first_row(body_index: usize, row: impl FnOnce() -> Row) -> Row {
    if body_index == 0 { row() } else { Row::new() }
}

fn item_row(theme: &Theme, identity: &str, description: &str, selected: bool, width: usize) -> Row {
    let paint = if selected {
        theme.selected_completion
    } else {
        theme.dim
    };
    let mut row = Row::new();
    if width > 4 {
        row.push_spaces(2);
    }
    if width >= WIDE_ROW {
        let identity_width = IDENTITY_COLUMNS.min(width - 4);
        row.push(&safe_line(identity, identity_width), paint);
        row.pad_to_column(identity_width + 4);
        row.push(&safe_line(description, width - identity_width - 4), paint);
    } else {
        row.push(&safe_line(identity, width.saturating_sub(2)), paint);
    }
    row
}
