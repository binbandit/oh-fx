use ofx_contract::{
    ModelCapabilities, ModelCatalog, Notice, NoticeTone, ReasoningEffort, UiCommand,
};

use super::Shell;
use super::model_menu::{CatalogLoad, ModelMenu};
use super::picker_state::{
    Cursor, ExplicitModel, FAST_OPTIONS, MODEL_PREFIX, ModelFlow, ModelQuery, ModelStage,
    effort_at, effort_index, effort_options, explicit_model, is_bare_model_command, matches_query,
};
use crate::composer::projected_anchor_column;
use crate::footer::model_menu_presentation::{MAX_INLINE_ROWS, visible_items};
use crate::footer::picker_presentation::{OptionList, list_picker_rows, option_picker_band};
use crate::input::{InputEvent, ShortcutAction};
use crate::list_window::{DEFAULT_MAX_PICKER_ROWS, advance_selection, update_edge_start};
use crate::render_engine::transcript_blocks::Entry;
use crate::row_text::{Row, terminal_safe};

const MODEL_USAGE: &str = "usage: /model <id> <effort> [normal|fast]";
const MAX_MODEL_COMPLETIONS: usize = 32;

struct Column {
    stage: ModelStage,
    values: Vec<String>,
    cursor: Cursor,
    token_start: usize,
    exact: Option<String>,
}

impl Column {
    fn selected(&self) -> Option<String> {
        self.exact.clone().or_else(|| {
            let count = self.values.len();
            (count > 0).then(|| self.values[self.cursor.index % count].clone())
        })
    }
}

impl Shell<'_> {
    pub(super) fn model_query(&self) -> Option<ModelQuery<'_>> {
        if self.skills_menu_visible()
            || self.help_menu.is_some()
            || self.model_menu.is_some()
            || self.picker.is_some()
            || self.settings_menu.is_some()
        {
            return None;
        }
        self.model_flow.query(self.composer.text())
    }

    pub(super) fn bare_model_command(&self) -> bool {
        is_bare_model_command(self.composer.text(), self.composer.cursor())
    }

    pub(super) fn ensure_catalog(&mut self) {
        if self.model_query().is_some() {
            self.request_catalog(false);
        }
    }

    pub(super) fn request_catalog(&mut self, after_failure: bool) {
        let wanted = match self.catalog {
            CatalogLoad::Idle => true,
            CatalogLoad::Failed(_) => after_failure,
            CatalogLoad::Loading | CatalogLoad::Listed { .. } => false,
        };
        if wanted {
            self.catalog = CatalogLoad::Loading;
            self.send(UiCommand::ListModels);
        }
    }

    pub(super) fn catalog_received(&mut self, provider: &str, catalog: ModelCatalog) {
        if provider != self.options.provider {
            return;
        }
        self.catalog = match catalog {
            ModelCatalog::Listed { models, source } => CatalogLoad::Listed { models, source },
            ModelCatalog::Failed { retry } => CatalogLoad::Failed(retry),
        };
        if let Some(menu) = &mut self.model_menu {
            menu.restart();
        }
    }

    pub(super) fn open_model_menu(&mut self) {
        self.skills_menu = None;
        self.model_flow = ModelFlow::default();
        self.composer.clear();
        self.model_menu = Some(ModelMenu::default());
        self.request_catalog(true);
    }

    pub(super) fn toggle_model_shortcut(&mut self) {
        if self.exit_model_shortcut()
            || self.cancel_model_menu()
            || self.skills_menu_visible()
            || self.help_menu.is_some()
            || self.picker.is_some()
            || self.settings_menu.is_some()
        {
            return;
        }
        self.model_draft = Some(self.composer.stash());
        self.open_model_menu();
    }

    pub(super) fn exit_model_shortcut(&mut self) -> bool {
        if self.model_draft.is_none() {
            return false;
        }
        self.model_menu = None;
        self.restore_model_draft();
        true
    }

    pub(super) fn cancel_model_menu(&mut self) -> bool {
        if self.model_menu.take().is_none() {
            return false;
        }
        self.composer.clear();
        self.restore_model_draft();
        true
    }

    pub(super) fn provider_selected(&mut self, provider: String) {
        self.options.provider = provider;
        self.catalog = CatalogLoad::Idle;
        if let Some(menu) = &mut self.model_menu {
            *menu = ModelMenu::default();
            menu.set_query(self.composer.text());
        }
        if self.model_menu.is_some() || self.model_query().is_some() {
            self.request_catalog(false);
        }
    }

    fn restore_model_draft(&mut self) {
        if let Some(draft) = self.model_draft.take() {
            self.model_flow = ModelFlow::default();
            self.composer.restore(draft);
        }
    }

    pub(super) fn settle_model_draft(&mut self) {
        if self.model_menu.is_none() && self.model_query().is_none() {
            self.restore_model_draft();
        }
    }

    pub(super) fn move_model_menu(&mut self, delta: isize) -> bool {
        let budget = self.menu_budget(MAX_INLINE_ROWS);
        let Some(menu) = &mut self.model_menu else {
            return false;
        };
        let visible = visible_items(menu, &self.catalog, budget);
        menu.move_selection(self.catalog.models(), delta, visible);
        true
    }

    pub(super) fn cycle_model_menu_vendor(&mut self, delta: isize) -> bool {
        let Some(menu) = &mut self.model_menu else {
            return false;
        };
        menu.move_tab(self.catalog.models(), delta);
        true
    }

    pub(super) fn sync_model_menu(&mut self, edited: bool) {
        if edited && let Some(menu) = &mut self.model_menu {
            menu.set_query(self.composer.text());
        }
    }

    pub(super) fn submit_model_menu(&mut self) -> bool {
        let Some(menu) = &self.model_menu else {
            return false;
        };
        let Some(model) = menu.selected_id(self.catalog.models()).map(str::to_owned) else {
            return true;
        };
        self.model_menu = None;
        self.composer.clear();
        self.begin_model_options(&model);
        true
    }

    pub(super) fn open_current_model_column(&mut self) {
        self.rewrite_model_text(MODEL_PREFIX);
        self.model_flow.anchor_current = true;
        self.request_catalog(true);
    }

    pub(super) fn model_edit_preserved(&self, event: &InputEvent) -> bool {
        let Some(query) = self.model_query() else {
            return false;
        };
        if query.stage == ModelStage::Model
            || self.composer.cursor() != self.composer.text().len()
            || self.composer.selection().is_some()
        {
            return false;
        }
        let deletes = |shortcut: Option<ShortcutAction>| {
            shortcut == Some(ShortcutAction::DeleteBackward) && !query.query.is_empty()
        };
        match event {
            InputEvent::Text(_) => true,
            InputEvent::Raw(raw) => (33..127).contains(&raw.byte) || deletes(raw.composer_shortcut),
            InputEvent::Action(decoded) => deletes(decoded.composer_shortcut),
            InputEvent::TextDropped(_)
            | InputEvent::Paste(_)
            | InputEvent::NativeClearProbe
            | InputEvent::NativeClearDetected => false,
        }
    }

    pub(super) fn model_column_after_edit(&mut self, preserved: bool) {
        let flow = &mut self.model_flow;
        if flow.owned_revision == Some(self.composer.edit_revision()) {
            return;
        }
        if !preserved {
            flow.clear();
        }
        flow.reset_active_index();
        if flow.dismissed && flow.raw_query(self.composer.text()).is_none() {
            flow.dismissed = false;
        }
    }

    pub(super) fn dismiss_model_column(&mut self) -> bool {
        if self.model_query().is_none() {
            return false;
        }
        self.model_flow.dismissed = true;
        true
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        self.catalog
            .find(model)
            .map(|option| option.capabilities.clone())
            .unwrap_or_default()
    }

    fn model_column(&self) -> Option<Column> {
        let query = self.model_query()?;
        let flow = &self.model_flow;
        let wanted = query.query.trim_matches([' ', '\t']);
        let (values, cursor, exact) = match query.stage {
            ModelStage::Model => {
                let values = self.model_completions(query.query);
                let cursor = self.model_cursor(&values);
                let exact = values
                    .iter()
                    .find(|id| !wanted.is_empty() && id.eq_ignore_ascii_case(wanted))
                    .cloned();
                (values, cursor, exact)
            }
            ModelStage::Effort => {
                let efforts = self.capabilities(&flow.pending).reasoning_efforts;
                let options = effort_options(&efforts);
                let exact = ReasoningEffort::parse(wanted)
                    .filter(|effort| options.contains(effort))
                    .map(|effort| effort.display_label().to_owned());
                let values: Vec<String> = options
                    .iter()
                    .map(ReasoningEffort::display_label)
                    .filter(|label| matches_query(label, query.query))
                    .map(str::to_owned)
                    .collect();
                (values, flow.effort, exact)
            }
            ModelStage::Fast => {
                let values: Vec<String> = FAST_OPTIONS
                    .iter()
                    .filter(|label| matches_query(label, query.query))
                    .map(|label| (*label).to_owned())
                    .collect();
                let exact = FAST_OPTIONS
                    .iter()
                    .find(|label| label.eq_ignore_ascii_case(wanted))
                    .map(|label| (*label).to_owned());
                (values, flow.fast, exact)
            }
        };
        Some(Column {
            stage: query.stage,
            values,
            cursor,
            token_start: query.token_start,
            exact,
        })
    }

    fn model_completions(&self, query: &str) -> Vec<String> {
        let mut ids: Vec<String> = self
            .catalog
            .models()
            .iter()
            .filter(|option| matches_query(&option.id, query))
            .take(MAX_MODEL_COMPLETIONS)
            .map(|option| option.id.clone())
            .collect();
        let current = &self.options.model;
        if self.model_flow.anchor_current
            && query.is_empty()
            && !ids.contains(current)
            && self.catalog.find(current).is_some()
        {
            if ids.len() == MAX_MODEL_COMPLETIONS {
                ids.pop();
            }
            ids.push(current.clone());
        }
        ids
    }

    fn model_cursor(&self, values: &[String]) -> Cursor {
        let flow = &self.model_flow;
        let anchored = flow
            .anchor_current
            .then(|| values.iter().position(|id| *id == self.options.model))
            .flatten();
        let index = anchored.unwrap_or(flow.model.index % values.len().max(1));
        Cursor {
            index,
            window_start: if flow.anchor_current {
                update_edge_start(0, values.len(), index, DEFAULT_MAX_PICKER_ROWS)
            } else {
                flow.model.window_start
            },
        }
    }

    pub(super) fn model_column_band(&self, input_extra: usize, banner_rows: usize) -> Vec<Row> {
        let Some(column) = self.model_column() else {
            return Vec::new();
        };
        let empty = match (column.stage, &self.catalog) {
            (ModelStage::Model, CatalogLoad::Idle | CatalogLoad::Loading) => "loading models...",
            (ModelStage::Model, CatalogLoad::Failed(_)) => "unable to load models",
            (ModelStage::Model, CatalogLoad::Listed { .. }) => "no matching models",
            (ModelStage::Effort, _) => "no matching effort",
            (ModelStage::Fast, _) => "no matching mode",
        };
        let labels: Vec<String> = column
            .values
            .iter()
            .map(|value| terminal_safe(value).into_owned())
            .collect();
        let summary = self
            .composer
            .visual_layout(self.layout.cols)
            .summary(Some(column.token_start));
        option_picker_band(
            &self.theme,
            &OptionList {
                labels: &labels,
                annotations: &[],
                cursor: (column.cursor.index, column.cursor.window_start),
                empty,
                start_col: usize::from(projected_anchor_column(summary, self.layout.cols)),
                cols: self.cols(),
                rows: list_picker_rows(usize::from(self.layout.rows), input_extra, banner_rows),
            },
        )
    }

    pub(super) fn navigate_model_column(&mut self, delta: i32) -> bool {
        let Some(column) = self.model_column() else {
            return false;
        };
        let flow = &mut self.model_flow;
        if column.stage == ModelStage::Model {
            flow.model = column.cursor;
            flow.anchor_current = false;
        }
        let cursor = flow.active_cursor();
        advance_selection(
            &mut cursor.index,
            &mut cursor.window_start,
            column.values.len(),
            delta,
        );
        true
    }

    pub(super) fn autocomplete_model_column(&mut self) {
        let Some(column) = self.model_column() else {
            return;
        };
        let Some(value) = column.selected() else {
            return;
        };
        let pending = self.model_flow.pending.clone();
        let efforts = self.capabilities(&pending).reasoning_efforts;
        let fast = self.capabilities(&pending).supports_fast_mode;
        match column.stage {
            ModelStage::Model => self.rewrite_model_text(&format!("{MODEL_PREFIX}{value}")),
            ModelStage::Effort => {
                let effort = ReasoningEffort::parse(&value).unwrap_or(ReasoningEffort::Auto);
                self.rewrite_model_text(&format!("{MODEL_PREFIX}{pending} {}", effort.label()));
                let index = effort_index(&efforts, &effort);
                self.model_flow
                    .begin(&pending, index, fast, ModelStage::Effort);
            }
            ModelStage::Fast => {
                let index = self.model_flow.effort.index;
                let effort = effort_at(&efforts, index);
                self.rewrite_model_text(&format!(
                    "{MODEL_PREFIX}{pending} {} {value}",
                    effort.label()
                ));
                let fast_mode = value == FAST_OPTIONS[1];
                let index = effort_index(&efforts, &effort);
                self.model_flow
                    .begin(&pending, index, fast_mode, ModelStage::Fast);
            }
        }
    }

    pub(super) fn advance_model_column_on_space(&mut self) {
        if self.composer.cursor() != self.composer.text().len() {
            return;
        }
        let Some(column) = self.model_column() else {
            return;
        };
        let Some(value) = column.exact else {
            return;
        };
        match column.stage {
            ModelStage::Model => self.begin_model_options(&value),
            ModelStage::Effort => {
                let pending = self.model_flow.pending.clone();
                if self.capabilities(&pending).supports_fast_mode {
                    self.enter_fast_stage(&pending, &value);
                }
            }
            ModelStage::Fast => {}
        }
    }

    pub(super) fn submit_model_column(&mut self) -> bool {
        let Some(column) = self.model_column() else {
            return false;
        };
        let pending = self.model_flow.pending.clone();
        let selected = column.selected();
        match column.stage {
            ModelStage::Model => {
                if matches!(self.catalog, CatalogLoad::Idle | CatalogLoad::Loading) {
                    let query = &self.composer.text()[column.token_start..];
                    return query.trim_matches([' ', '\t']).is_empty();
                }
                let Some(model) = selected else {
                    return false;
                };
                self.begin_model_options(&model);
            }
            ModelStage::Effort => {
                let Some(label) = selected else {
                    return true;
                };
                if self.capabilities(&pending).supports_fast_mode {
                    self.enter_fast_stage(&pending, &label);
                } else {
                    let effort = ReasoningEffort::parse(&label).unwrap_or(ReasoningEffort::Auto);
                    self.pick_model(pending, effort, None);
                }
            }
            ModelStage::Fast => {
                let Some(label) = selected else {
                    return true;
                };
                let efforts = self.capabilities(&pending).reasoning_efforts;
                let effort = effort_at(&efforts, self.model_flow.effort.index);
                self.pick_model(pending, effort, Some(label == FAST_OPTIONS[1]));
            }
        }
        true
    }

    pub(super) fn step_back_model_column(&mut self) -> bool {
        let Some(query) = self.model_query() else {
            return false;
        };
        let stage = query.stage;
        let pending = self.model_flow.pending.clone();
        let efforts = self.capabilities(&pending).reasoning_efforts;
        match stage {
            ModelStage::Model => return false,
            ModelStage::Fast if !efforts.is_empty() => {
                let index =
                    effort_index(&efforts, &effort_at(&efforts, self.model_flow.effort.index));
                self.rewrite_model_text(&format!("{MODEL_PREFIX}{pending} "));
                self.model_flow
                    .begin(&pending, index, false, ModelStage::Effort);
            }
            ModelStage::Effort | ModelStage::Fast => {
                self.rewrite_model_text(&format!("{MODEL_PREFIX}{pending}"));
                let values = self.model_completions(&pending);
                let index = values.iter().position(|id| *id == pending).unwrap_or(0);
                self.model_flow.model = Cursor {
                    index,
                    window_start: update_edge_start(
                        0,
                        values.len(),
                        index,
                        DEFAULT_MAX_PICKER_ROWS,
                    ),
                };
            }
        }
        true
    }

    pub(super) fn submit_explicit_model(&mut self) -> bool {
        match explicit_model(self.composer.text()) {
            ExplicitModel::None => false,
            ExplicitModel::Invalid => {
                self.push_entry(Entry::Notice(Notice::new(
                    NoticeTone::Error,
                    "",
                    MODEL_USAGE,
                )));
                true
            }
            ExplicitModel::Pick {
                model,
                effort,
                fast_mode,
            } => {
                let model = model.to_owned();
                self.pick_model(model, effort, fast_mode);
                true
            }
        }
    }

    fn enter_fast_stage(&mut self, model: &str, effort_label: &str) {
        let efforts = self.capabilities(model).reasoning_efforts;
        let effort = ReasoningEffort::parse(effort_label).unwrap_or(ReasoningEffort::Auto);
        self.rewrite_model_text(&format!("{MODEL_PREFIX}{model} {} ", effort.label()));
        let index = effort_index(&efforts, &effort);
        self.model_flow.begin(model, index, true, ModelStage::Fast);
    }

    fn begin_model_options(&mut self, model: &str) {
        let capabilities = self.capabilities(model);
        let efforts = !capabilities.reasoning_efforts.is_empty();
        if !efforts && !capabilities.supports_fast_mode {
            self.pick_model(model.to_owned(), ReasoningEffort::Auto, None);
            return;
        }
        let (stage, text) = if efforts {
            (ModelStage::Effort, format!("{MODEL_PREFIX}{model} "))
        } else {
            (ModelStage::Fast, format!("{MODEL_PREFIX}{model} auto "))
        };
        self.rewrite_model_text(&text);
        self.model_flow
            .begin(model, 0, capabilities.supports_fast_mode, stage);
    }

    fn rewrite_model_text(&mut self, text: &str) {
        self.composer.replace_text(text);
        self.model_flow = ModelFlow {
            owned_revision: Some(self.composer.edit_revision()),
            ..ModelFlow::default()
        };
    }

    fn pick_model(&mut self, model: String, effort: ReasoningEffort, fast_mode: Option<bool>) {
        self.composer.clear();
        self.model_flow = ModelFlow::default();
        self.send(UiCommand::SelectModel {
            model,
            effort,
            fast_mode,
        });
    }
}

#[cfg(test)]
mod tests;
