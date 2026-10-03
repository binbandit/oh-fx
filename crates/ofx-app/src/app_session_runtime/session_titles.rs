use std::sync::{Arc, Mutex, PoisonError, Weak};

use ofx_contract::ModelProvider;
use ofx_session::{MAX_TITLE_BYTES, SessionError, TitleRequest, WritableSession, generate_title};
use tokio_util::sync::CancellationToken;

const TITLE_TRIM: [char; 4] = [' ', '\t', '\r', '\n'];

pub(super) type CachedTitle = Arc<Mutex<Option<String>>>;

pub struct TitleGeneration {
    pub(super) provider: Arc<dyn ModelProvider>,
    pub(super) model: &'static str,
    pub(super) session_id: String,
    pub(super) excerpt: String,
    pub(super) session: Weak<Mutex<WritableSession>>,
    pub(super) cached: CachedTitle,
}

impl TitleGeneration {
    pub async fn run(self, cancel: &CancellationToken) {
        let request = TitleRequest {
            model: self.model,
            session_id: &self.session_id,
            prompt_excerpt: &self.excerpt,
        };
        let Some(title) = generate_title(&*self.provider, request, cancel).await else {
            return;
        };
        let Some(session) = self.session.upgrade() else {
            return;
        };
        let installed = session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .install_generated_title(&title);
        if installed == Ok(true) {
            *self.cached.lock().unwrap_or_else(PoisonError::into_inner) = Some(title);
        }
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
