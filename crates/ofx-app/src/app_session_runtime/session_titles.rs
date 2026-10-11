use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Instant;

use ofx_contract::{ModelProvider, UiEvent};
use ofx_session::{
    MAX_TITLE_BYTES, SessionError, TitleOutcome, TitleRequest, WritableSession, generate_title,
};
use ofx_trace::trace_log;
use tokio_util::sync::CancellationToken;

use crate::app_agent_runtime::Emit;

const TITLE_TRIM: [char; 4] = [' ', '\t', '\r', '\n'];
const SESSION: &str = "session";

pub struct TitleGeneration {
    pub(super) provider: Arc<dyn ModelProvider>,
    pub(super) model: &'static str,
    pub(super) session_id: String,
    pub(super) excerpt: String,
    pub(super) session: Weak<Mutex<WritableSession>>,
    pub(super) progress: Option<TitleProgress>,
}

impl TitleGeneration {
    #[must_use]
    pub(crate) fn reporting(mut self, progress: TitleProgress) -> Self {
        self.progress = Some(progress);
        self
    }

    pub async fn run(self, cancel: &CancellationToken) -> Option<String> {
        if let Some(progress) = &self.progress {
            progress.start(&self.session_id, self.model);
        }
        let started = Instant::now();
        let request = TitleRequest {
            model: self.model,
            session_id: &self.session_id,
            prompt_excerpt: &self.excerpt,
        };
        let outcome = generate_title(&*self.provider, request, cancel).await;
        let mut last = LastTitle {
            status: TitleStatus::Failed,
            reason: None,
            detail: String::new(),
            elapsed_ms: Some(started.elapsed().as_millis()),
            session: self.session_id.clone(),
            model: self.model.to_owned(),
        };
        let title = match outcome {
            TitleOutcome::Generated(title) => self.install(title, &mut last),
            TitleOutcome::Unavailable(failure) => {
                last.reason = Some(failure.reason());
                failure.detail().clone_into(&mut last.detail);
                None
            }
        };
        if let Some(progress) = &self.progress {
            progress.finish(last);
        }
        title
    }

    fn install(&self, title: String, last: &mut LastTitle) -> Option<String> {
        last.status = TitleStatus::Installed;
        let Some(session) = self.session.upgrade() else {
            trace_log!(
                SESSION,
                "event=title_generation_apply result=dropped reason=no_active_session"
            );
            last.dropped("no_active_session", String::new());
            return None;
        };
        let installed = session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .install_generated_title(&title);
        match installed {
            Ok(true) => {
                trace_log!(
                    SESSION,
                    "event=title_generation_apply result=installed session={}",
                    self.session_id
                );
                Some(title)
            }
            Ok(false) => {
                last.dropped("user_title_present", String::new());
                None
            }
            Err(error) => {
                trace_log!(
                    SESSION,
                    "event=title_generation_apply result=failed session={} err={error}",
                    self.session_id
                );
                last.dropped("install_failed", error.to_string());
                None
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum TitleStatus {
    #[default]
    None,
    Installed,
    Dropped,
    Failed,
}

impl TitleStatus {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Installed => "installed",
            Self::Dropped => "dropped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LastTitle {
    pub(crate) status: TitleStatus,
    pub(crate) reason: Option<&'static str>,
    pub(crate) detail: String,
    pub(crate) elapsed_ms: Option<u128>,
    pub(crate) session: String,
    pub(crate) model: String,
}

impl LastTitle {
    fn dropped(&mut self, reason: &'static str, detail: String) {
        self.status = TitleStatus::Dropped;
        self.reason = Some(reason);
        self.detail = detail;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunningTitle {
    pub(crate) session: String,
    pub(crate) model: String,
    pub(crate) elapsed_ms: u128,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TitleSnapshot {
    pub(crate) running: Option<RunningTitle>,
    pub(crate) last: LastTitle,
}

#[derive(Debug, Default)]
struct TitleState {
    running: Option<(String, String, Instant)>,
    last: LastTitle,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TitleProgress(Arc<Mutex<TitleState>>);

impl TitleProgress {
    fn state(&self) -> MutexGuard<'_, TitleState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn start(&self, session: &str, model: &str) {
        self.state().running = Some((session.to_owned(), model.to_owned(), Instant::now()));
    }

    pub(super) fn abandon(&self) {
        self.state().running = None;
    }

    fn finish(&self, last: LastTitle) {
        let mut state = self.state();
        state.running = None;
        state.last = last;
    }

    pub(crate) fn snapshot(&self) -> TitleSnapshot {
        let state = self.state();
        TitleSnapshot {
            running: state
                .running
                .as_ref()
                .map(|(session, model, started)| RunningTitle {
                    session: session.clone(),
                    model: model.clone(),
                    elapsed_ms: started.elapsed().as_millis(),
                }),
            last: state.last.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct SessionTitle {
    title: Arc<Mutex<Option<String>>>,
    emit: Emit,
}

impl SessionTitle {
    pub(crate) fn new(emit: Emit) -> Self {
        Self {
            title: Arc::default(),
            emit,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<String>> {
        self.title.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn is_untitled(&self) -> bool {
        self.lock().is_none()
    }

    pub(crate) fn current(&self) -> Option<String> {
        self.lock().clone()
    }

    pub(crate) fn set(&self, title: Option<&str>) {
        let title = title.map(|title| {
            title
                .chars()
                .filter(|character| !matches!(character, '\0'..='\x1f' | '\x7f'))
                .collect::<String>()
        });
        self.lock().clone_from(&title);
        (self.emit)(UiEvent::SessionTitleChanged { title });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenameError {
    EmptyTitle,
    TitleTooLong,
    InvalidTitle,
    NoActiveSession,
    NotSaved(SessionError),
}

pub(crate) fn validate_session_title(raw: &str) -> Result<&str, RenameError> {
    let trimmed = raw.trim_matches(TITLE_TRIM);
    if trimmed.is_empty() {
        return Err(RenameError::EmptyTitle);
    }
    if trimmed.len() > MAX_TITLE_BYTES {
        return Err(RenameError::TitleTooLong);
    }
    if trimmed.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(RenameError::InvalidTitle);
    }
    Ok(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_progress_reports_a_running_generation_until_it_finishes_or_is_abandoned() {
        let progress = TitleProgress::default();
        assert_eq!(progress.snapshot(), TitleSnapshot::default());
        progress.start("s1", "title-model");
        let running = progress.snapshot().running.unwrap();
        assert_eq!(
            (running.session.as_str(), running.model.as_str()),
            ("s1", "title-model")
        );
        progress.abandon();
        assert_eq!(progress.snapshot(), TitleSnapshot::default());
        progress.start("s2", "title-model");
        let mut last = LastTitle {
            status: TitleStatus::Installed,
            session: "s2".to_owned(),
            ..LastTitle::default()
        };
        last.dropped("user_title_present", String::new());
        progress.finish(last.clone());
        assert_eq!(
            progress.snapshot(),
            TitleSnapshot {
                running: None,
                last,
            }
        );
        assert_eq!(progress.snapshot().last.status.name(), "dropped");
    }

    #[test]
    fn a_session_title_is_trimmed_printable_text_of_at_most_240_bytes() {
        assert_eq!(
            validate_session_title("  hello world \n"),
            Ok("hello world")
        );
        assert_eq!(validate_session_title("héllo ✓"), Ok("héllo ✓"));
        assert_eq!(validate_session_title("   "), Err(RenameError::EmptyTitle));
        assert_eq!(
            validate_session_title("bad\ttitle"),
            Err(RenameError::InvalidTitle)
        );
        assert_eq!(
            validate_session_title("bad\x07title"),
            Err(RenameError::InvalidTitle)
        );
        assert_eq!(
            validate_session_title("bad\x7ftitle"),
            Err(RenameError::InvalidTitle)
        );
        let longest = "x".repeat(MAX_TITLE_BYTES);
        assert_eq!(
            validate_session_title(&format!(" {longest}\t")),
            Ok(longest.as_str())
        );
        assert_eq!(
            validate_session_title(&format!("{longest}x")),
            Err(RenameError::TitleTooLong)
        );
    }
}
