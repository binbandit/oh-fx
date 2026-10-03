use ofx_contract::{UiCommand, UiEvent};

use super::Shell;
use super::picker_state::{Cursor, matches_query, strip_prefix_ignore_case};
use crate::composer::projected_anchor_column;
use crate::footer::picker_presentation::{OptionList, list_picker_rows, option_picker_band};
use crate::list_window::advance_selection;
use crate::row_text::{Row, terminal_safe};

const PROVIDER_PREFIXES: [&str; 3] = ["/provider ", "/login ", "/setup "];
const CURRENT: &str = "current";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ProviderColumn {
    cursor: Cursor,
    dismissed: bool,
}

struct ProviderQuery<'a> {
    prefix: &'a str,
    query: &'a str,
    token_start: usize,
}

fn raw_provider_query(text: &str) -> Option<ProviderQuery<'_>> {
    let lead = text.len() - text.trim_start_matches([' ', '\t']).len();
    let trimmed = &text[lead..];
    PROVIDER_PREFIXES.iter().find_map(|prefix| {
        let query = strip_prefix_ignore_case(trimmed, prefix)?;
        Some(ProviderQuery {
            prefix: &trimmed[..prefix.len()],
            query,
            token_start: lead + prefix.len(),
        })
    })
}

impl Shell<'_> {
    fn provider_query(&self) -> Option<ProviderQuery<'_>> {
        if self.provider_column.dismissed
            || self.skills_menu.is_some()
            || self.model_menu.is_some()
            || self.picker.is_some()
            || self.model_query().is_some()
        {
            return None;
        }
        raw_provider_query(self.composer.text())
    }

    fn provider_busy(&self) -> bool {
        self.working() || !self.outstanding.is_empty()
    }

    fn provider_options(&self, query: &str) -> Vec<&str> {
        self.options
            .providers
            .iter()
            .map(String::as_str)
            .filter(|name| matches_query(name, query))
            .collect()
    }

    fn selected_provider(&self) -> Option<String> {
        let query = self.provider_query()?;
        let options = self.provider_options(query.query);
        let wanted = query.query.trim_matches([' ', '\t']);
        let exact = options
            .iter()
            .find(|name| !wanted.is_empty() && name.eq_ignore_ascii_case(wanted));
        let index = self.provider_column.cursor.index % options.len().max(1);
        exact.or(options.get(index)).map(|name| (*name).to_owned())
    }

    pub(super) fn provider_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::ModelCatalog { provider, catalog } => {
                self.catalog_received(&provider, catalog);
            }
            UiEvent::ProviderPicker { prefix, providers } => {
                self.open_provider_column(&prefix, providers);
            }
            UiEvent::ProviderSelected { provider } => self.provider_selected(provider),
            _ => {}
        }
    }

    fn open_provider_column(&mut self, prefix: &str, providers: Vec<String>) {
        self.options.providers = providers;
        if self.model_menu.is_some() || self.model_draft.is_some() || self.picker.is_some() {
            return;
        }
        self.skills_menu = None;
        self.composer.replace_text(prefix);
        self.provider_column = ProviderColumn::default();
        self.invalidate();
    }

    pub(super) fn provider_column_after_edit(&mut self) {
        let column = &mut self.provider_column;
        column.cursor = Cursor::default();
        if column.dismissed && raw_provider_query(self.composer.text()).is_none() {
            column.dismissed = false;
        }
    }

    pub(super) fn provider_column_band(&self, input_extra: usize, banner_rows: usize) -> Vec<Row> {
        if self.provider_busy() {
            return Vec::new();
        }
        let Some(query) = self.provider_query() else {
            return Vec::new();
        };
        let options = self.provider_options(query.query);
        let labels: Vec<String> = options
            .iter()
            .map(|name| terminal_safe(name).into_owned())
            .collect();
        let annotations: Vec<&str> = options
            .iter()
            .map(|name| {
                if *name == self.options.provider {
                    CURRENT
                } else {
                    ""
                }
            })
            .collect();
        let summary = self
            .composer
            .visual_layout(self.layout.cols)
            .summary(Some(query.token_start));
        let cursor = self.provider_column.cursor;
        option_picker_band(
            &self.theme,
            &OptionList {
                labels: &labels,
                annotations: &annotations,
                cursor: (cursor.index, cursor.window_start),
                empty: "no matching providers",
                start_col: usize::from(projected_anchor_column(summary, self.layout.cols)),
                cols: self.cols(),
                rows: list_picker_rows(usize::from(self.layout.rows), input_extra, banner_rows),
            },
        )
    }

    pub(super) fn navigate_provider_column(&mut self, delta: i32) -> bool {
        let Some(query) = self.provider_query() else {
            return false;
        };
        if !self.provider_busy() {
            let count = self.provider_options(query.query).len();
            let cursor = &mut self.provider_column.cursor;
            advance_selection(&mut cursor.index, &mut cursor.window_start, count, delta);
        }
        true
    }

    pub(super) fn autocomplete_provider_column(&mut self) -> bool {
        let Some(query) = self.provider_query() else {
            return false;
        };
        if self.provider_busy() {
            return true;
        }
        let prefix = query.prefix.to_owned();
        if let Some(name) = self.selected_provider() {
            self.composer.replace_text(&format!("{prefix}{name}"));
        }
        true
    }

    pub(super) fn submit_provider_column(&mut self) -> bool {
        if self.provider_query().is_none() {
            return false;
        }
        let selected = self.selected_provider();
        if self.provider_busy() {
            self.send(UiCommand::SelectProvider {
                provider: selected.unwrap_or_default(),
            });
            return true;
        }
        let Some(provider) = selected else {
            return false;
        };
        self.composer.clear();
        self.provider_column = ProviderColumn::default();
        self.send(UiCommand::SelectProvider { provider });
        true
    }

    pub(super) fn choose_provider_at_end(&mut self) -> bool {
        self.composer.cursor() == self.composer.text().len()
            && self.composer.selection().is_none()
            && !self.provider_busy()
            && self.submit_provider_column()
    }

    pub(super) fn dismiss_provider_column(&mut self) -> bool {
        if self.provider_busy() || self.provider_query().is_none() {
            return false;
        }
        self.provider_column.dismissed = true;
        true
    }
}

#[cfg(test)]
mod tests;
