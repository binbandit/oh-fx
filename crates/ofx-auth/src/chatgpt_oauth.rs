use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ofx_contract::CODEX_ORIGINATOR;
use serde_json::Value;
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::browser_callback::{
    AwaitError, BindError, CallbackListener, Classifier, ParseResult, Response,
};
use crate::chatgpt_session::{DeleteOutcome, Mutation, Session, SessionStore, refresh_deadline_ms};
use crate::oauth::{
    self, BrowserTokenSet, FormBody, OAuthError, parse_object, pkce_challenge,
    query_value_non_empty, random_url_safe_secret,
};
use crate::oauth_transport::{Method, Transport, TransportError};
use crate::secret::Secret;
use crate::session_presence::Presence;
use crate::url_opener;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const ISSUER_URL: &str = "https://auth.openai.com";
const JWT_AUTH_CLAIM: &str = "https://api.openai.com/auth";
const BROWSER_SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
const BROWSER_CALLBACK_PORTS: [u16; 2] = [1455, 1457];
const CALLBACK_HOST: &str = "127.0.0.1";
const BROWSER_LOGIN_TIMEOUT: Duration = Duration::from_mins(5);
const CALLBACK_PREFIX: &str = "/auth/callback?";
const MILLISECONDS_PER_SECOND: i64 = 1000;
const TERMINAL_REFRESH_CODES: [&str; 4] = [
    "\"refresh_token_expired\"",
    "\"refresh_token_reused\"",
    "\"refresh_token_invalidated\"",
    "\"invalid_grant\"",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChatGptError {
    #[error("CredentialStorageUnavailable")]
    CredentialStorageUnavailable,
    #[error("LockBusy")]
    LockBusy,
    #[error("InsecureAuthFile")]
    InsecureAuthFile,
    #[error("InvalidChatGptAuthSession")]
    InvalidChatGptAuthSession,
    #[error("DurablePathUnsafe")]
    DurablePathUnsafe,
    #[error("PrivateStatePermissionsUnsupported")]
    PrivateStatePermissionsUnsupported,
    #[error("DurableReplacePreRenameFailed")]
    DurableReplacePreRenameFailed,
    #[error("DurableReplacePostRenameFailed")]
    DurableReplacePostRenameFailed,
    #[error("AccessDenied")]
    AccessDenied,
    #[error("CredentialRefreshRejected")]
    CredentialRefreshRejected,
    #[error("CredentialRefreshPersistenceUncertain")]
    CredentialRefreshPersistenceUncertain,
    #[error("CredentialPersistenceFailed")]
    CredentialPersistenceFailed,
    #[error("ChatGptAccountChanged")]
    ChatGptAccountChanged,
    #[error("ChatGptOAuthRequestFailed")]
    ChatGptOAuthRequestFailed,
    #[error("InvalidChatGptOAuthResponse")]
    InvalidChatGptOAuthResponse,
    #[error("InvalidChatGptAccessToken")]
    InvalidChatGptAccessToken,
    #[error("ChatGptOAuthCallbackPortUnavailable")]
    ChatGptOAuthCallbackPortUnavailable,
    #[error("ChatGptOAuthCallbackListenerFailed")]
    ChatGptOAuthCallbackListenerFailed,
    #[error("ChatGptAuthorizationFailed")]
    ChatGptAuthorizationFailed,
    #[error("LoginTimedOut")]
    LoginTimedOut,
    #[error("Cancelled")]
    Cancelled,
    #[error("WriteFailed")]
    WriteFailed,
    #[error("OAuthTransportUnavailable")]
    OAuthTransportUnavailable,
    #[error("ConnectionFailed")]
    ConnectionFailed,
    #[error("Timeout")]
    Timeout,
    #[error("OAuthResponseTooLarge")]
    OAuthResponseTooLarge,
    #[error("RandomSourceUnavailable")]
    RandomSourceUnavailable,
}

impl From<TransportError> for ChatGptError {
    fn from(error: TransportError) -> Self {
        match error {
            TransportError::Unavailable => Self::OAuthTransportUnavailable,
            TransportError::ConnectionFailed => Self::ConnectionFailed,
            TransportError::Timeout => Self::Timeout,
            TransportError::ResponseTooLarge => Self::OAuthResponseTooLarge,
        }
    }
}

impl From<OAuthError> for ChatGptError {
    fn from(error: OAuthError) -> Self {
        match error {
            OAuthError::InvalidOAuthResponse => Self::InvalidChatGptOAuthResponse,
            OAuthError::RandomSourceUnavailable => Self::RandomSourceUnavailable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    IfNeeded,
    Force,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatGptAccess {
    access_token: Secret,
    account_id: String,
    refresh_after_ms: i64,
}

impl ChatGptAccess {
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn refresh_after_ms(&self) -> i64 {
        self.refresh_after_ms
    }

    pub fn into_token(self) -> String {
        self.access_token.into_inner()
    }

    pub(crate) fn access_token(&self) -> &str {
        self.access_token.expose()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatGptEndpoints {
    pub issuer: String,
    pub token_url: String,
    pub callback_ports: Vec<u16>,
}

impl Default for ChatGptEndpoints {
    fn default() -> Self {
        Self {
            issuer: ISSUER_URL.to_owned(),
            token_url: TOKEN_URL.to_owned(),
            callback_ports: BROWSER_CALLBACK_PORTS.to_vec(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatGptOAuth {
    store: SessionStore,
    transport: Transport,
    endpoints: ChatGptEndpoints,
}

impl ChatGptOAuth {
    pub fn new(
        data_directory: PathBuf,
        user_agent: &str,
        endpoints: ChatGptEndpoints,
    ) -> Result<Self, ChatGptError> {
        Ok(Self {
            store: SessionStore::new(data_directory),
            transport: Transport::new(user_agent)?,
            endpoints,
        })
    }

    pub async fn run_login(
        &self,
        output: &mut (dyn Write + Send),
        open_browser: bool,
        cancel: &CancellationToken,
    ) -> Result<(), ChatGptError> {
        self.store.require_sign_in_storage()?;
        let listener = CallbackListener::bind(&self.endpoints.callback_ports)
            .await
            .map_err(|error| match error {
                BindError::PortUnavailable => ChatGptError::ChatGptOAuthCallbackPortUnavailable,
                BindError::Failed => ChatGptError::ChatGptOAuthCallbackListenerFailed,
            })?;
        let redirect_uri = callback_redirect_uri(listener.port());
        let code_verifier = random_url_safe_secret()?;
        let state = random_url_safe_secret()?;
        let authorization_url = build_browser_authorization_url(
            &self.endpoints.issuer,
            &redirect_uri,
            &pkce_challenge(code_verifier.expose()),
            state.expose(),
        );
        write!(
            output,
            "Open this URL to sign in with Codex:\n{authorization_url}\n\nWaiting for browser authorization...\n"
        )
        .and_then(|()| output.flush())
        .map_err(|_| ChatGptError::WriteFailed)?;
        if open_browser {
            url_opener::open_url(&authorization_url);
        }
        let login = BrowserLogin {
            listener: &listener,
            redirect_uri: &redirect_uri,
            code_verifier: &code_verifier,
            state,
        };
        tokio::time::timeout(
            BROWSER_LOGIN_TIMEOUT,
            self.finish_browser_login(login, cancel),
        )
        .await
        .map_err(|_| ChatGptError::LoginTimedOut)?
    }

    pub async fn logout(&self) -> Result<DeleteOutcome, ChatGptError> {
        match self.store.begin_existing_mutation().await? {
            Some(mutation) => mutation.delete(),
            None => Ok(DeleteOutcome::Missing),
        }
    }

    pub(crate) fn storage_presence(&self) -> Presence {
        self.store.presence()
    }

    pub(crate) async fn load_access(
        &self,
        mode: RefreshMode,
    ) -> Result<Option<ChatGptAccess>, ChatGptError> {
        let Some(mutation) = self.store.begin_existing_mutation().await? else {
            return Ok(None);
        };
        let Some(mut session) = mutation.load()? else {
            return Ok(None);
        };
        if mode == RefreshMode::Force || session.expired(now_ms()) {
            session = self.refresh_session(&mutation, &session).await?;
        }
        Ok(Some(take_access(session)))
    }

    async fn finish_browser_login(
        &self,
        login: BrowserLogin<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), ChatGptError> {
        let expected = login.state.clone();
        let classify: Classifier<Secret, ChatGptError> =
            Arc::new(move |target: &str| classify_browser_callback(target, expected.expose()));
        let accepted =
            login
                .listener
                .accept(&classify, cancel)
                .await
                .map_err(|error| match error {
                    AwaitError::Cancelled => ChatGptError::Cancelled,
                    AwaitError::ListenerFailed => ChatGptError::ChatGptOAuthCallbackListenerFailed,
                    AwaitError::Rejected(error) => error,
                })?;
        let exchanged = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(ChatGptError::Cancelled),
            exchanged = self.exchange_authorization_code(
                accepted.callback.expose(),
                login.code_verifier.expose(),
                login.redirect_uri,
            ) => exchanged,
        };
        let completed = exchanged.and_then(|token| complete_sign_in(token, now_ms()));
        let saved = match completed {
            Ok(session) => self
                .store
                .save_new_session(&session)
                .await
                .map_err(|_| ChatGptError::CredentialPersistenceFailed),
            Err(error) => Err(error),
        };
        let outcome = if saved.is_ok() {
            Response::Ok
        } else {
            Response::Failed
        };
        let _ = accepted.respond(outcome).await;
        saved
    }

    async fn exchange_authorization_code(
        &self,
        authorization_code: &str,
        code_verifier: &str,
        redirect_uri: &str,
    ) -> Result<BrowserTokenSet, ChatGptError> {
        let mut form = FormBody::default();
        form.append("grant_type", "authorization_code");
        form.append("client_id", CLIENT_ID);
        form.append("code", authorization_code);
        form.append("code_verifier", code_verifier);
        form.append("redirect_uri", redirect_uri);
        let response = self
            .transport
            .execute(Method::PostForm, &self.endpoints.token_url, form.as_str())
            .await?;
        if !response.accepted {
            return Err(ChatGptError::ChatGptOAuthRequestFailed);
        }
        Ok(oauth::parse_browser_token_set(&response.body)?)
    }

    async fn refresh_session(
        &self,
        mutation: &Mutation,
        session: &Session,
    ) -> Result<Session, ChatGptError> {
        mutation.require_writable()?;
        let payload = Zeroizing::new(format!(
            "{{\"client_id\":{},\"grant_type\":\"refresh_token\",\"refresh_token\":{}}}",
            Value::String(CLIENT_ID.to_owned()),
            *Zeroizing::new(Value::String(session.refresh_token.expose().to_owned()).to_string()),
        ));
        let token = match self.request_refresh_token(&payload).await {
            Ok(token) => token,
            Err(
                ChatGptError::CredentialRefreshRejected | ChatGptError::InvalidChatGptOAuthResponse,
            ) => {
                retire_refresh_session(mutation)?;
                return Err(ChatGptError::CredentialRefreshRejected);
            }
            Err(error) => return Err(error),
        };
        let replacement = match refresh_replacement(token, session, now_ms()) {
            Ok(replacement) => replacement,
            Err(error) => {
                retire_refresh_session(mutation)?;
                return Err(if error == ChatGptError::ChatGptAccountChanged {
                    error
                } else {
                    ChatGptError::CredentialRefreshRejected
                });
            }
        };
        if mutation.save(&replacement).is_err() {
            let _ = retire_refresh_session(mutation);
            return Err(ChatGptError::CredentialRefreshPersistenceUncertain);
        }
        Ok(replacement)
    }

    async fn request_refresh_token(
        &self,
        payload: &str,
    ) -> Result<RefreshTokenResponse, ChatGptError> {
        let response = self
            .transport
            .execute(Method::PostJson, &self.endpoints.token_url, payload)
            .await?;
        if !response.accepted {
            return Err(if chatgpt_refresh_requires_sign_in(&response.body) {
                ChatGptError::CredentialRefreshRejected
            } else {
                ChatGptError::ChatGptOAuthRequestFailed
            });
        }
        parse_refresh_token_response(&response.body)
    }
}

struct BrowserLogin<'a> {
    listener: &'a CallbackListener,
    redirect_uri: &'a str,
    code_verifier: &'a Secret,
    state: Secret,
}

#[derive(Debug)]
struct RefreshTokenResponse {
    access_token: Secret,
    refresh_token: Option<Secret>,
    expires_in: Option<i64>,
}

fn parse_refresh_token_response(bytes: &[u8]) -> Result<RefreshTokenResponse, ChatGptError> {
    let invalid = ChatGptError::InvalidChatGptOAuthResponse;
    let object = parse_object(bytes).map_err(|_| invalid)?;
    let access_token = oauth::required_string(&object, "access_token").map_err(|_| invalid)?;
    let refresh_token = match object.get("refresh_token") {
        None => None,
        Some(Value::String(token)) if !token.is_empty() => Some(Secret::new(token.clone())),
        Some(_) => return Err(invalid),
    };
    let expires_in =
        oauth::optional_positive_integer(&object, "expires_in").map_err(|_| invalid)?;
    Ok(RefreshTokenResponse {
        access_token,
        refresh_token,
        expires_in,
    })
}

fn chatgpt_refresh_requires_sign_in(body: &[u8]) -> bool {
    TERMINAL_REFRESH_CODES.iter().any(|code| {
        body.windows(code.len())
            .any(|window| window == code.as_bytes())
    })
}

fn retire_refresh_session(mutation: &Mutation) -> Result<(), ChatGptError> {
    match mutation.delete() {
        Ok(DeleteOutcome::Deleted | DeleteOutcome::Missing) => Ok(()),
        Ok(DeleteOutcome::DeletedNotDurable) | Err(_) => {
            Err(ChatGptError::CredentialRefreshPersistenceUncertain)
        }
    }
}

fn refresh_replacement(
    token: RefreshTokenResponse,
    current: &Session,
    now_ms: i64,
) -> Result<Session, ChatGptError> {
    let account_id = extract_account_id(token.access_token.expose())?;
    if account_id != current.account_id {
        return Err(ChatGptError::ChatGptAccountChanged);
    }
    let expires_at_ms = session_expiry_ms(token.expires_in, token.access_token.expose(), now_ms)?;
    Ok(Session {
        access_token: token.access_token,
        refresh_token: token
            .refresh_token
            .unwrap_or_else(|| current.refresh_token.clone()),
        expires_at_ms,
        account_id,
    })
}

fn complete_sign_in(token: BrowserTokenSet, now_ms: i64) -> Result<Session, ChatGptError> {
    let account_id = extract_account_id(token.access_token.expose())?;
    let expires_at_ms = session_expiry_ms(token.expires_in, token.access_token.expose(), now_ms)?;
    Ok(Session {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at_ms,
        account_id,
    })
}

fn take_access(session: Session) -> ChatGptAccess {
    ChatGptAccess {
        refresh_after_ms: refresh_deadline_ms(session.expires_at_ms),
        access_token: session.access_token,
        account_id: session.account_id,
    }
}

fn jwt_payload(token: &str) -> Result<serde_json::Map<String, Value>, ChatGptError> {
    let invalid = ChatGptError::InvalidChatGptAccessToken;
    let mut parts = token.split('.');
    let (Some(_), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid);
    };
    let decoded = Zeroizing::new(URL_SAFE_NO_PAD.decode(payload).map_err(|_| invalid)?);
    parse_object(&decoded).map_err(|_| invalid)
}

pub(crate) fn extract_account_id(token: &str) -> Result<String, ChatGptError> {
    let invalid = ChatGptError::InvalidChatGptAccessToken;
    let payload = jwt_payload(token)?;
    let Some(Value::Object(claim)) = payload.get(JWT_AUTH_CLAIM) else {
        return Err(invalid);
    };
    match claim.get("chatgpt_account_id") {
        Some(Value::String(account)) if !account.is_empty() => Ok(account.clone()),
        _ => Err(invalid),
    }
}

fn session_expiry_ms(
    expires_in: Option<i64>,
    access_token: &str,
    now_ms: i64,
) -> Result<i64, ChatGptError> {
    match expires_in {
        Some(expires_in) => Ok(oauth::expiry_timestamp_ms(now_ms, expires_in)?),
        None => access_token_expires_at_ms(access_token),
    }
}

fn access_token_expires_at_ms(token: &str) -> Result<i64, ChatGptError> {
    let invalid = ChatGptError::InvalidChatGptOAuthResponse;
    let payload = jwt_payload(token)?;
    let exp = payload
        .get("exp")
        .filter(|value| value.is_i64())
        .and_then(Value::as_i64)
        .filter(|exp| *exp > 0)
        .ok_or(invalid)?;
    exp.checked_mul(MILLISECONDS_PER_SECOND).ok_or(invalid)
}

fn callback_redirect_uri(port: u16) -> String {
    format!("http://{CALLBACK_HOST}:{port}/auth/callback")
}

fn build_browser_authorization_url(
    issuer: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> String {
    let mut form = FormBody::default();
    form.append("response_type", "code");
    form.append("client_id", CLIENT_ID);
    form.append("redirect_uri", redirect_uri);
    form.append("scope", BROWSER_SCOPE);
    form.append("code_challenge", code_challenge);
    form.append("code_challenge_method", "S256");
    form.append("id_token_add_organizations", "true");
    form.append("codex_cli_simplified_flow", "true");
    form.append("state", state);
    form.append("originator", CODEX_ORIGINATOR);
    format!(
        "{}/oauth/authorize?{}",
        issuer.trim_end_matches('/'),
        form.as_str()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallbackError {
    Invalid,
    StateMismatch,
    Denied,
}

fn classify_browser_callback(
    target: &str,
    expected_state: &str,
) -> ParseResult<Secret, ChatGptError> {
    match parse_browser_callback_target(target, expected_state) {
        Ok(code) => ParseResult::Accepted(code),
        Err(CallbackError::Invalid | CallbackError::StateMismatch) => ParseResult::Unrelated,
        Err(CallbackError::Denied) => ParseResult::Failed(ChatGptError::ChatGptAuthorizationFailed),
    }
}

fn parse_browser_callback_target(
    target: &str,
    expected_state: &str,
) -> Result<Secret, CallbackError> {
    let query = target
        .strip_prefix(CALLBACK_PREFIX)
        .filter(|_| !target.contains('#'))
        .ok_or(CallbackError::Invalid)?;
    if query_value_non_empty(query, "error").is_ok() {
        let denial_state =
            query_value_non_empty(query, "state").map_err(|_| CallbackError::Invalid)?;
        if !states_match(&denial_state, expected_state) {
            return Err(CallbackError::Invalid);
        }
        return Err(CallbackError::Denied);
    }
    let code = query_value_non_empty(query, "code").map_err(|_| CallbackError::Invalid)?;
    let state = query_value_non_empty(query, "state").map_err(|_| CallbackError::Invalid)?;
    if !states_match(&state, expected_state) {
        return Err(CallbackError::StateMismatch);
    }
    Ok(Secret::new(code.to_string()))
}

fn states_match(received: &str, expected: &str) -> bool {
    bool::from(received.as_bytes().ct_eq(expected.as_bytes()))
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
