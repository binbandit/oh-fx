use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofx_contract::{McpServersCatalog, McpServersSection};
use ofx_mcp::{
    Availability, BaselineEntry, McpRuntime, ServerSummary, render_change_notice,
    render_model_catalog,
};

pub(crate) struct McpServers {
    runtime: Option<Arc<McpRuntime>>,
    baseline: Option<Mutex<Option<Vec<BaselineEntry>>>>,
}

impl McpServers {
    pub(crate) fn new(runtime: Option<Arc<McpRuntime>>, reports_changes: bool) -> Self {
        Self {
            runtime,
            baseline: reports_changes.then(Mutex::default),
        }
    }

    fn change_notice(&self, current: &[ServerSummary]) -> Option<String> {
        let baseline = self.baseline.as_ref()?;
        if current
            .iter()
            .any(|server| server.availability == Availability::Discovering)
        {
            return None;
        }
        let mut baseline = lock(baseline);
        let notice = baseline
            .as_deref()
            .and_then(|previous| render_change_notice(previous, current));
        *baseline = Some(current.iter().map(BaselineEntry::from).collect());
        notice
    }
}

impl McpServersCatalog for McpServers {
    fn section(&self) -> McpServersSection {
        let servers = self
            .runtime
            .as_ref()
            .map(|runtime| runtime.model_catalog())
            .unwrap_or_default();
        let rendered = render_model_catalog(&servers);
        McpServersSection {
            text: rendered.text,
            change_notice: self.change_notice(&servers),
            notice: rendered.notice,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
