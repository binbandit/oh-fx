use std::sync::{Arc, Mutex, PoisonError, Weak};

use ofx_contract::ModelProvider;
use ofx_session::{TitleRequest, WritableSession, generate_title};
use tokio_util::sync::CancellationToken;

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
