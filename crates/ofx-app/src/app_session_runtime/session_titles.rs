use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use ofx_contract::{ModelProvider, UiEvent};
use ofx_session::{MAX_TITLE_BYTES, SessionError, TitleRequest, WritableSession, generate_title};
use tokio_util::sync::CancellationToken;

use crate::app_agent_runtime::Emit;

const TITLE_TRIM: [char; 4] = [' ', '\t', '\r', '\n'];

pub struct TitleGeneration {
    pub(super) provider: Arc<dyn ModelProvider>,
    pub(super) model: &'static str,
    pub(super) session_id: String,
    pub(super) excerpt: String,
    pub(super) session: Weak<Mutex<WritableSession>>,
}

impl TitleGeneration {
    pub async fn run(self, cancel: &CancellationToken) -> Option<String> {
        let request = TitleRequest {
            model: self.model,
            session_id: &self.session_id,
            prompt_excerpt: &self.excerpt,
        };
        let title = generate_title(&*self.provider, request, cancel).await?;
        let session = self.session.upgrade()?;
        let installed = session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .install_generated_title(&title);
        (installed == Ok(true)).then_some(title)
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
