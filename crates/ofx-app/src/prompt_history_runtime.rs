use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ofx_contract::{Notice, NoticeTone};
use ofx_session::{PromptHistoryError, PromptHistoryStore};
use ofx_tui::PromptHistory;

const LOADED_ENTRIES: usize = 100;
const TOPIC: &str = "history";
const UNAVAILABLE: &str = "durable prompt history unavailable";

pub(crate) struct PromptHistoryRuntime {
    store: Option<PromptHistoryStore>,
    workspace_root: String,
    writes_available: bool,
}

impl PromptHistoryRuntime {
    pub(crate) fn initialize(data_dir: Option<&Path>, workspace_root: &Path) -> Self {
        let store = data_dir.and_then(|data_dir| PromptHistoryStore::open(data_dir).ok());
        Self {
            writes_available: store.is_some(),
            store,
            workspace_root: workspace_root.to_str().unwrap_or_default().to_owned(),
        }
    }

    pub(crate) fn into_shell_history(mut self, enabled: bool) -> (PromptHistory, Option<Notice>) {
        let (entries, notice) = if self.store.is_none() {
            (Vec::new(), Some(warning(UNAVAILABLE.to_owned())))
        } else if !enabled {
            (Vec::new(), None)
        } else {
            match self.load_recent() {
                Ok(entries) => (entries, None),
                Err(notice) => (Vec::new(), Some(notice)),
            }
        };
        let available = self.store.is_some();
        let saver = move |text: &str| {
            self.record_accepted(now_ms(), text)
                .map_err(|error| error.to_string())
        };
        let history = match (available, enabled) {
            (false, _) => PromptHistory::disabled(),
            (true, true) => PromptHistory::enabled(entries, saver),
            (true, false) => PromptHistory::paused(saver),
        };
        (history, notice)
    }

    fn load_recent(&mut self) -> Result<Vec<String>, Notice> {
        let Some(store) = &mut self.store else {
            return Ok(Vec::new());
        };
        store
            .load_recent(&self.workspace_root, LOADED_ENTRIES)
            .map_err(|error| {
                self.writes_available = false;
                warning(format!("failed to load durable prompt history ({error})"))
            })
    }

    fn record_accepted(&mut self, timestamp_ms: i64, text: &str) -> Result<(), PromptHistoryError> {
        let Some(store) = self.store.as_mut().filter(|_| self.writes_available) else {
            return Ok(());
        };
        store
            .append(timestamp_ms, &self.workspace_root, text)
            .map(drop)
            .inspect_err(|error| {
                if *error == PromptHistoryError::LockUnsupported {
                    self.writes_available = false;
                }
            })
    }
}

fn warning(body: String) -> Notice {
    Notice::new(NoticeTone::Warning, TOPIC, body)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests;
