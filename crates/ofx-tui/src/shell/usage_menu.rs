use ofx_contract::{ModelUsage, UsageReport, UsageScope};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UsageMenu {
    requested_scope: UsageScope,
    pub(crate) selected_model: usize,
    pub(crate) expanded_model: Option<usize>,
    pub(crate) model_window_start: usize,
    pub(crate) report: Option<Box<UsageReport>>,
    pub(crate) refresh_failed: bool,
}

impl UsageMenu {
    pub(crate) fn opened(report: Box<UsageReport>) -> Self {
        Self {
            requested_scope: report.scope,
            report: Some(report),
            ..Self::loading(UsageScope::Days30)
        }
    }

    pub(crate) fn loading(scope: UsageScope) -> Self {
        Self {
            requested_scope: scope,
            selected_model: 0,
            expanded_model: None,
            model_window_start: 0,
            report: None,
            refresh_failed: false,
        }
    }

    pub(crate) fn set_loading_scope(&mut self, scope: UsageScope) {
        *self = Self::loading(scope);
    }

    pub(crate) fn replace(&mut self, report: Box<UsageReport>) {
        let models = self
            .report
            .as_ref()
            .map_or(&[][..], |report| report.models.as_slice());
        let old_selected = models
            .get(self.selected_model.min(models.len().saturating_sub(1)))
            .map(|model| model.model.clone());
        let old_expanded = self
            .expanded_model
            .and_then(|index| models.get(index))
            .map(|model| model.model.clone());
        let selected = find_model(&report.models, old_selected.as_deref()).unwrap_or_else(|| {
            self.selected_model
                .min(report.models.len().saturating_sub(1))
        });
        self.expanded_model = find_model(&report.models, old_expanded.as_deref());
        self.requested_scope = report.scope;
        self.report = Some(report);
        self.selected_model = selected;
        self.refresh_failed = false;
        self.model_window_start = self.model_window_start.min(selected);
    }

    pub(crate) fn record_refresh_failure(&mut self, attempted_scope: UsageScope) {
        self.refresh_failed = true;
        self.requested_scope = attempted_scope;
    }

    pub(crate) fn request(&mut self, scope: UsageScope) {
        self.requested_scope = scope;
    }

    pub(crate) fn move_model(&mut self, delta: isize, visible_model_rows: usize) -> bool {
        let count = self.model_count();
        if delta == 0 || count == 0 {
            return false;
        }
        let next = if delta > 0 {
            self.selected_model.saturating_add(1).min(count - 1)
        } else {
            self.selected_model.saturating_sub(1)
        };
        if next == self.selected_model {
            return false;
        }
        self.selected_model = next;
        self.keep_selection_visible(visible_model_rows);
        true
    }

    pub(crate) fn toggle_expanded(&mut self, visible_model_rows: usize) -> bool {
        let count = self.model_count();
        if count == 0 {
            return false;
        }
        let selected = self.selected_model.min(count - 1);
        self.expanded_model = if self.expanded_model == Some(selected) {
            None
        } else {
            Some(selected)
        };
        self.keep_selection_visible(visible_model_rows);
        true
    }

    pub(crate) fn scope(&self) -> UsageScope {
        self.report
            .as_ref()
            .map_or(self.requested_scope, |report| report.scope)
    }

    pub(crate) fn navigation_scope(&self) -> UsageScope {
        self.requested_scope
    }

    fn model_count(&self) -> usize {
        self.report.as_ref().map_or(0, |report| report.models.len())
    }

    fn keep_selection_visible(&mut self, visible_model_rows: usize) {
        let visible = visible_model_rows.max(1);
        if self.selected_model < self.model_window_start {
            self.model_window_start = self.selected_model;
        } else if self.selected_model >= self.model_window_start.saturating_add(visible) {
            self.model_window_start = self.selected_model - (visible - 1);
        }
    }
}

fn find_model(models: &[ModelUsage], target: Option<&str>) -> Option<usize> {
    let name = target?;
    models.iter().position(|model| model.model == name)
}

#[cfg(test)]
mod tests;
