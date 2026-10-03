use super::{
    GrokError, GrokOAuth, LOGIN_TIMEOUT, MAX_MANUAL_CODE_BYTES, build_browser_authorization_url,
    classify_browser_callback,
};
use crate::browser_callback::{
    Accepted, AwaitError, BindError, CallbackListener, Classifier, Response,
};
use crate::oauth::{pkce_challenge, random_url_safe_secret};
use crate::secret::Secret;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
#[derive(Debug)]
pub struct GrokSignIn {
    oauth: GrokOAuth,
    pub(super) listener: CallbackListener,
    pub(super) redirect_uri: String,
    pub(super) verifier: Secret,
    pub(super) state: Secret,
    url: String,
    manual: Mutex<Option<Secret>>,
    notify: Notify,
    deadline: tokio::time::Instant,
    phase: AtomicU8,
}
impl GrokSignIn {
    pub(super) async fn new(oauth: GrokOAuth) -> Result<Self, GrokError> {
        oauth.store.require_sign_in_storage()?;
        let listener = CallbackListener::bind(&[0])
            .await
            .map_err(|error| match error {
                BindError::PortUnavailable => GrokError::GrokOAuthCallbackPortUnavailable,
                BindError::Failed => GrokError::GrokOAuthCallbackListenerFailed,
            })?;
        let redirect_uri = format!("http://127.0.0.1:{}/callback", listener.port());
        let verifier = random_url_safe_secret()?;
        let state = random_url_safe_secret()?;
        let url = build_browser_authorization_url(
            &oauth.endpoints.issuer,
            &redirect_uri,
            &pkce_challenge(verifier.expose()),
            state.expose(),
        );
        Ok(GrokSignIn {
            oauth,
            listener,
            redirect_uri,
            verifier,
            state,
            url,
            manual: Mutex::new(None),
            notify: Notify::new(),
            deadline: tokio::time::Instant::now() + LOGIN_TIMEOUT,
            phase: AtomicU8::new(0),
        })
    }

    pub fn authorization_url(&self) -> &str {
        &self.url
    }
    pub fn submit_manual_code(&self, code: &str) -> Result<(), GrokError> {
        let code = code.trim_matches([' ', '\t', '\r', '\n']);
        if code.is_empty() {
            return Err(GrokError::InvalidGrokAuthorizationCode);
        }
        if code.len() > MAX_MANUAL_CODE_BYTES {
            return Err(GrokError::InvalidGrokAuthorizationCode);
        }
        if !code.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
            return Err(GrokError::InvalidGrokAuthorizationCode);
        }
        let mut manual = self
            .manual
            .lock()
            .map_err(|_| GrokError::InvalidGrokAuthorizationCode)?;
        if self.phase.load(Ordering::Acquire) == 2 {
            return Err(GrokError::GrokLoginBusy);
        }
        *manual = Some(Secret::new(code.to_owned()));
        self.notify.notify_one();
        Ok(())
    }

    pub async fn finish(&self, cancel: &CancellationToken) -> Result<(), GrokError> {
        self.phase
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| GrokError::GrokLoginBusy)?;
        let _completion = SignInCompletionGuard(self);
        if tokio::time::Instant::now() >= self.deadline {
            return Err(GrokError::LoginTimedOut);
        }
        tokio::time::timeout_at(self.deadline, self.finish_inner(cancel))
            .await
            .map_err(|_| GrokError::LoginTimedOut)?
    }

    async fn finish_inner(&self, cancel: &CancellationToken) -> Result<(), GrokError> {
        let state = self.state.clone();
        let classify: Classifier<Secret, GrokError> =
            Arc::new(move |target| classify_browser_callback(target, state.expose()));
        let mut accepted: Option<Accepted<Secret>> = None;
        let code = loop {
            let notification = self.notify.notified();
            if cancel.is_cancelled() {
                return Err(GrokError::Cancelled);
            }
            if let Some(code) = self
                .manual
                .lock()
                .map_err(|_| GrokError::InvalidGrokAuthorizationCode)?
                .take()
            {
                break code;
            }
            tokio::select! { biased;
                () = cancel.cancelled() => return Err(GrokError::Cancelled),
                () = notification => {},
                result = self.listener.accept_with_origin(&classify,cancel,Some("https://accounts.x.ai")) => {
                    let callback = result.map_err(|error| match error { AwaitError::Cancelled => GrokError::Cancelled, AwaitError::ListenerFailed => GrokError::GrokOAuthCallbackListenerFailed, AwaitError::Rejected(error) => error })?;
                    let code = callback.callback.clone(); accepted = Some(callback); break code;
                }
            }
        };
        let result = tokio::select! { biased; () = cancel.cancelled() => Err(GrokError::Cancelled), result = self.oauth.exchange_authorization_code(code.expose(),self.verifier.expose(),&self.redirect_uri) => result };
        let saved = match result {
            Ok(session) if !cancel.is_cancelled() => self
                .oauth
                .store
                .save_new_session(&session)
                .await
                .map_err(|_| GrokError::CredentialPersistenceFailed),
            Ok(_) => Err(GrokError::Cancelled),
            Err(error) => Err(error),
        };
        if let Some(callback) = accepted {
            let _ = callback
                .respond(if saved.is_ok() {
                    Response::Ok
                } else {
                    Response::Failed
                })
                .await;
        }
        saved
    }
}

struct SignInCompletionGuard<'a>(&'a GrokSignIn);
impl Drop for SignInCompletionGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut manual) = self.0.manual.lock() {
            *manual = None;
            self.0.phase.store(2, Ordering::Release);
        } else {
            self.0.phase.store(2, Ordering::Release);
        }
    }
}
