use ofx_contract::{StatuslineItem, StatuslineToggles, WorkspaceIdentity, WorkspaceIdentitySource};
use ofx_text::visible_width;

use super::command_text::{prefix_terminal_safe_by_width, suffix_terminal_safe_by_width};

pub(crate) const MAX_STATUS_LINE_BYTES: usize = 512;
const MAX_IDENTITY_BYTES: usize = 512;
const MIN_SPLIT_IDENTITY_WIDTH: usize = 7;
const MIN_BRANCH_WIDTH: usize = 4;
const MARKER: &str = "…";

pub(crate) struct Statusline {
    toggles: StatuslineToggles,
    context_used: u64,
    context_total: Option<u32>,
    window_stale: bool,
    identity_source: Option<Box<dyn WorkspaceIdentitySource>>,
    identity: WorkspaceIdentity,
    session_title: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StatuslineView<'a> {
    pub(crate) context_used: u64,
    pub(crate) context_total: Option<u32>,
    pub(crate) session_title: Option<&'a str>,
    pub(crate) identity: Option<&'a WorkspaceIdentity>,
}

impl Statusline {
    pub(crate) fn new(
        toggles: StatuslineToggles,
        identity_source: Option<Box<dyn WorkspaceIdentitySource>>,
    ) -> Self {
        Self {
            toggles,
            context_used: 0,
            context_total: None,
            window_stale: false,
            identity_source,
            identity: WorkspaceIdentity::default(),
            session_title: None,
        }
    }

    pub(crate) fn session_title_changed(&mut self, title: Option<&str>) {
        self.session_title = title.map(str::to_owned);
    }

    pub(crate) fn set(&mut self, item: StatuslineItem, enabled: bool) {
        self.toggles.set(item, enabled);
    }

    pub(crate) fn usage_reported(
        &mut self,
        input_tokens: Option<u64>,
        context_window: Option<u32>,
    ) {
        if let Some(input) = input_tokens {
            self.context_used = input;
        }
        if !self.window_stale {
            self.context_total = context_window;
        }
    }

    pub(crate) fn model_changed(&mut self) {
        self.context_total = None;
        self.window_stale = true;
    }

    pub(crate) fn turn_started(&mut self) {
        self.window_stale = false;
    }

    pub(crate) fn conversation_cleared(&mut self) {
        self.context_used = 0;
    }

    pub(crate) fn refresh(&mut self) {
        if !self.toggles.enabled(StatuslineItem::Workspace) {
            return;
        }
        if let Some(source) = &mut self.identity_source {
            self.identity = source.refresh();
        }
    }

    pub(crate) fn view(&self) -> StatuslineView<'_> {
        let context = self.toggles.enabled(StatuslineItem::Context);
        StatuslineView {
            context_used: if context { self.context_used } else { 0 },
            context_total: self.context_total.filter(|_| context),
            session_title: self
                .session_title
                .as_deref()
                .filter(|_| self.toggles.enabled(StatuslineItem::Session)),
            identity: (self.toggles.enabled(StatuslineItem::Workspace)
                && self.identity_source.is_some())
            .then_some(&self.identity),
        }
    }
}

pub(crate) fn context_segment(used: u64, total: Option<u32>) -> Option<String> {
    if used == 0 {
        return None;
    }
    let used_k = used / 1000;
    Some(match total {
        Some(total) => {
            let total = u64::from(total);
            let percent = used.saturating_mul(100).checked_div(total).unwrap_or(0);
            format!("{used_k}k/{}k {percent}%", total / 1000)
        }
        None => format!("{used_k}k"),
    })
}

pub(crate) fn workspace_identity_segment(
    identity: &WorkspaceIdentity,
    max_width: usize,
    max_bytes: usize,
) -> Option<String> {
    if identity.label.is_empty() || max_width == 0 {
        return None;
    }
    let max_bytes = max_bytes.min(MAX_IDENTITY_BYTES);
    let branch = identity
        .branch
        .as_deref()
        .filter(|branch| !branch.is_empty());
    (1..=max_width.min(max_bytes))
        .rev()
        .map(|width_budget| compose(&identity.label, branch, width_budget))
        .find(|composed| composed.len() <= max_bytes)
}

fn compose(label: &str, branch: Option<&str>, width_budget: usize) -> String {
    let Some(branch) = branch.filter(|_| width_budget >= MIN_SPLIT_IDENTITY_WIDTH) else {
        return clipped_suffix(label, width_budget);
    };
    let branch_budget = visible_width(branch)
        .min(width_budget - 4)
        .min(MIN_BRANCH_WIDTH.max(width_budget / 2));
    let path = clipped_suffix(label, width_budget - 3 - branch_budget);
    format!("{path} ({})", clipped_prefix(branch, branch_budget))
}

fn clipped_suffix(encoded: &str, max_width: usize) -> String {
    if visible_width(encoded) <= max_width {
        return encoded.to_owned();
    }
    if max_width <= 1 {
        return if max_width == 1 {
            MARKER.to_owned()
        } else {
            String::new()
        };
    }
    format!(
        "{MARKER}{}",
        suffix_terminal_safe_by_width(encoded, max_width - 1)
    )
}

fn clipped_prefix(encoded: &str, max_width: usize) -> String {
    if visible_width(encoded) <= max_width {
        return encoded.to_owned();
    }
    if max_width <= 1 {
        return if max_width == 1 {
            MARKER.to_owned()
        } else {
            String::new()
        };
    }
    format!(
        "{}{MARKER}",
        prefix_terminal_safe_by_width(encoded, max_width - 1)
    )
}

#[cfg(test)]
mod tests;
