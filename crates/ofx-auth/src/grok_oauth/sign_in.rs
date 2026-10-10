use super::{
    GrokError, GrokOAuth, LOGIN_TIMEOUT, MAX_MANUAL_CODE_BYTES, build_browser_authorization_url,
    classify_browser_callback,
};
use crate::browser_callback::{
    Accepted, AwaitError, BindError, CallbackListener, Classifier, Response,
};
use crate::oauth::{pkce_challenge, random_url_safe_secret};
use crate::secret::Secret;
use ofx_trace::trace_log;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const CALLBACK_ORIGIN: &str = "https://accounts.x.ai";
pub(crate) struct GrokSignIn {
    oauth: GrokOAuth,
    listener: CallbackListener,
    redirect_uri: String,
    verifier: Secret,
    state: Secret,
    url: String,
    manual: Mutex<Option<Secret>>,
    notify: Notify,
    deadline: tokio::time::Instant,
    phase: AtomicU8,
}
impl std::fmt::Debug for GrokSignIn {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("GrokSignIn").finish_non_exhaustive()
    }
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

    pub(crate) fn authorization_url(&self) -> &str {
        &self.url
    }
    pub(crate) fn submit_manual_code(&self, code: &str) -> Result<(), GrokError> {
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
            .map_err(|_| GrokError::GrokLoginStateUnavailable)?;
        if self.phase.load(Ordering::Acquire) == 2 {
            return Err(GrokError::GrokLoginBusy);
        }
        *manual = Some(Secret::new(code.to_owned()));
        self.notify.notify_one();
        Ok(())
    }

    pub(crate) async fn finish(&self, cancel: &CancellationToken) -> Result<(), GrokError> {
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
                .map_err(|_| GrokError::GrokLoginStateUnavailable)?
                .take()
            {
                break code;
            }
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(GrokError::Cancelled),
                () = notification => {},
                result = self.listener.accept_with_origin(&classify, cancel, Some(CALLBACK_ORIGIN)) => {
                    let callback = result.map_err(|error| match error {
                        AwaitError::Cancelled => GrokError::Cancelled,
                        AwaitError::ListenerFailed => GrokError::GrokOAuthCallbackListenerFailed,
                        AwaitError::Rejected(error) => error,
                    })?;
                    let code = callback.callback.clone();
                    accepted = Some(callback);
                    break code;
                }
            }
        };
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(GrokError::Cancelled),
            result = self.oauth.exchange_authorization_code(
                code.expose(), self.verifier.expose(), &self.redirect_uri
            ) => result,
        };
        let saved = match result {
            Ok(session) if !cancel.is_cancelled() => self
                .oauth
                .store
                .save_new_session_cancellable(&session, cancel)
                .await
                .map_err(|error| {
                    if error == GrokError::Cancelled {
                        error
                    } else {
                        GrokError::CredentialPersistenceFailed
                    }
                }),
            Ok(_) => Err(GrokError::Cancelled),
            Err(error) => Err(error),
        };
        if let Some(callback) = accepted {
            let outcome = if saved.is_ok() {
                Response::Ok
            } else {
                Response::Failed
            };
            if let Err(error) = callback.respond(outcome).await {
                trace_log!(
                    super::AUTH,
                    "Grok completion response failed err={:?}",
                    error.kind()
                );
            }
        }
        saved
    }
}

struct SignInCompletionGuard<'a>(&'a GrokSignIn);
impl Drop for SignInCompletionGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut manual) = self.0.manual.lock() {
            *manual = None;
        }
        self.0.phase.store(2, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn poisoned_manual_state_reports_a_login_state_failure() {
        let directory = tempfile::tempdir().unwrap();
        let oauth = GrokOAuth::new(
            directory.path().join("profile"),
            "oh-fx/test",
            super::super::GrokEndpoints::default(),
        )
        .unwrap();
        let sign_in = GrokSignIn::new(oauth).await.unwrap();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _manual = sign_in.manual.lock().unwrap();
            panic!("poison manual state");
        }));
        assert!(poisoned.is_err());
        assert_eq!(
            sign_in.submit_manual_code("code"),
            Err(GrokError::GrokLoginStateUnavailable)
        );
        assert_eq!(
            sign_in.finish(&CancellationToken::new()).await,
            Err(GrokError::GrokLoginStateUnavailable)
        );
    }
}
