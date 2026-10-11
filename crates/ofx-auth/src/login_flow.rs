use std::io::{IsTerminal, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::OnceLock;

use ofx_contract::parse_strict_json_value;
use ofx_trace::trace_log;
use serde_json::Value;

use crate::oauth::device::{self, DEFAULT_SCOPE, DeviceAuthorization, Metadata, TokenSet};
use crate::oauth::{self, OAuthError};
use crate::oauth_session::{self, Session, SessionStore};
use crate::oauth_transport::{Transport, TransportError};
use crate::subscription_access::now_ms;

mod poll;
mod team_picker;

use poll::{BrowserOpenPrompt, TerminalPoll};

const AUTH: &str = "auth";
const TEAMS_ENDPOINT: &str = "https://api.vercel.com/v2/teams";
const MAX_TEAMS_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VercelError {
    #[error("LoginTimedOut")]
    LoginTimedOut,
    #[error("NoRefreshToken")]
    NoRefreshToken,
    #[error("InvalidTeamSelection")]
    InvalidTeamSelection,
    #[error("TeamRequestFailed")]
    TeamRequestFailed,
    #[error("InvalidTeamsResponse")]
    InvalidTeamsResponse,
    #[error("InvalidIssuer")]
    InvalidIssuer,
    #[error("InvalidOAuthResponse")]
    InvalidOAuthResponse,
    #[error("AuthorizationPending")]
    AuthorizationPending,
    #[error("SlowDown")]
    SlowDown,
    #[error("AccessDenied")]
    AccessDenied,
    #[error("ExpiredToken")]
    ExpiredToken,
    #[error("InvalidClient")]
    InvalidClient,
    #[error("InvalidGrant")]
    InvalidGrant,
    #[error("OAuthRequestFailed")]
    OAuthRequestFailed,
    #[error("InvalidE2EOAuthEndpoint")]
    InvalidE2EOAuthEndpoint,
    #[error("CredentialStorageUnavailable")]
    CredentialStorageUnavailable,
    #[error("InsecureAuthFile")]
    InsecureAuthFile,
    #[error("InvalidAuthSession")]
    InvalidAuthSession,
    #[error("LockBusy")]
    LockBusy,
    #[error("DurablePathUnsafe")]
    DurablePathUnsafe,
    #[error("PrivateStatePermissionsUnsupported")]
    PrivateStatePermissionsUnsupported,
    #[error("DurableReplacePreRenameFailed")]
    DurableReplacePreRenameFailed,
    #[error("DurableReplacePostRenameFailed")]
    DurableReplacePostRenameFailed,
    #[error("CredentialPersistenceFailed")]
    CredentialPersistenceFailed,
    #[error("OAuthSessionCleanupUncertain")]
    OAuthSessionCleanupUncertain,
    #[error("Cancelled")]
    Cancelled,
    #[error("OAuthTransportUnavailable")]
    OAuthTransportUnavailable,
    #[error("ConnectionFailed")]
    ConnectionFailed,
    #[error("Timeout")]
    Timeout,
    #[error("OAuthResponseTooLarge")]
    OAuthResponseTooLarge,
    #[error("ReadFailed")]
    ReadFailed,
    #[error("WriteFailed")]
    WriteFailed,
}

impl From<TransportError> for VercelError {
    fn from(error: TransportError) -> Self {
        match error {
            TransportError::Unavailable => Self::OAuthTransportUnavailable,
            TransportError::ConnectionFailed => Self::ConnectionFailed,
            TransportError::Timeout => Self::Timeout,
            TransportError::ResponseTooLarge => Self::OAuthResponseTooLarge,
        }
    }
}

impl From<OAuthError> for VercelError {
    fn from(_: OAuthError) -> Self {
        Self::InvalidOAuthResponse
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VercelEndpoints {
    pub issuer: String,
    pub client_id: String,
}

impl Default for VercelEndpoints {
    fn default() -> Self {
        Self {
            issuer: oauth_session::ISSUER.to_owned(),
            client_id: oauth_session::DEFAULT_CLIENT_ID.to_owned(),
        }
    }
}

impl VercelEndpoints {
    pub fn from_environment() -> Self {
        let lookup = |name: &str| std::env::var(name).ok();
        Self {
            issuer: oauth_session::configured_issuer_url(&lookup),
            client_id: oauth_session::configured_client_id(&lookup),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginTerminal {
    pub browser_prompt: bool,
    pub team_picker: bool,
}

impl LoginTerminal {
    pub fn from_process() -> Self {
        let stdin_is_tty = std::io::stdin().is_terminal();
        Self {
            browser_prompt: browser_open_enabled(
                !crate::browser_allowed(),
                stdin_is_tty,
                cfg!(target_os = "macos"),
            ),
            team_picker: stdin_is_tty && std::io::stdout().is_terminal(),
        }
    }
}

fn browser_open_enabled(no_open_browser: bool, stdin_is_tty: bool, native_url_open: bool) -> bool {
    !no_open_browser && stdin_is_tty && native_url_open
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Team {
    pub(crate) id: String,
    pub(crate) slug: String,
    pub(crate) name: String,
}

struct PreparedLogin {
    metadata: Metadata,
    device: DeviceAuthorization,
    client_id: String,
}

#[derive(Debug, Clone)]
pub struct VercelOAuth {
    store: SessionStore,
    user_agent: String,
    transport: OnceLock<Transport>,
    endpoints: VercelEndpoints,
}

impl VercelOAuth {
    pub fn new(directory: PathBuf, user_agent: &str, endpoints: VercelEndpoints) -> Self {
        Self {
            store: SessionStore::new(directory),
            user_agent: user_agent.to_owned(),
            transport: OnceLock::new(),
            endpoints,
        }
    }

    pub(crate) fn store(&self) -> &SessionStore {
        &self.store
    }

    pub(crate) fn transport(&self) -> Result<&Transport, VercelError> {
        if let Some(transport) = self.transport.get() {
            return Ok(transport);
        }
        let transport = Transport::new(&self.user_agent)?;
        Ok(self.transport.get_or_init(|| transport))
    }

    pub async fn run_login<F: AsFd>(
        &self,
        output: &mut dyn Write,
        input: &F,
        terminal: LoginTerminal,
    ) -> Result<(), VercelError> {
        let prepared = self.prepare_login().await?;
        let url = prepared.device.display_url();
        write_all(
            output,
            &format!("Open {url}\nCode: {}\n\n", prepared.device.user_code),
        )?;
        let mut prompt = BrowserOpenPrompt::new(url, terminal.browser_prompt);
        write_all(output, prompt.waiting_message())?;
        let mut deps = TerminalPoll {
            transport: self.transport()?,
            metadata: &prepared.metadata,
            client_id: &prepared.client_id,
            input,
        };
        let token = poll::poll_for_token(&mut deps, &prepared.device, &mut prompt).await?;
        let teams = self
            .fetch_teams(token.access_token.expose(), &prepared.metadata.issuer)
            .await
            .unwrap_or_default();
        let selected = select_team(&teams, None, terminal.team_picker, input, output)?;
        let session = take_login_session(
            &prepared.metadata.issuer,
            &prepared.client_id,
            token,
            selected.map(|index| &teams[index]),
            now_ms(),
        )?;
        self.store
            .save_new_session(&session)
            .await
            .map_err(|error| {
                trace_log!(AUTH, "sign-in session save failed err={error}");
                VercelError::CredentialPersistenceFailed
            })
    }

    async fn prepare_login(&self) -> Result<PreparedLogin, VercelError> {
        self.store.require_sign_in_storage()?;
        let transport = self.transport()?;
        let issuer = &self.endpoints.issuer;
        let metadata = device::discover(transport, issuer).await?;
        oauth_session::validate_e2e_endpoint(issuer, &metadata.device_authorization_endpoint)?;
        oauth_session::validate_e2e_endpoint(issuer, &metadata.token_endpoint)?;
        let mut client_id = self.endpoints.client_id.clone();
        let device = match device::request_device_authorization(transport, &metadata, &client_id)
            .await
        {
            Err(VercelError::InvalidClient) if client_id != oauth_session::DEFAULT_CLIENT_ID => {
                oauth_session::DEFAULT_CLIENT_ID.clone_into(&mut client_id);
                device::request_device_authorization(transport, &metadata, &client_id).await?
            }
            device => device?,
        };
        Ok(PreparedLogin {
            metadata,
            device,
            client_id,
        })
    }

    async fn fetch_teams(
        &self,
        access_token: &str,
        issuer_url: &str,
    ) -> Result<Vec<Team>, VercelError> {
        let endpoint = if oauth_session::is_loopback_e2e_issuer(issuer_url) {
            format!("{issuer_url}/v2/teams")
        } else {
            TEAMS_ENDPOINT.to_owned()
        };
        let response = self
            .transport()?
            .execute_authorized_get_within(&endpoint, access_token, MAX_TEAMS_RESPONSE_BYTES)
            .await?;
        if !response.accepted {
            return Err(VercelError::TeamRequestFailed);
        }
        parse_teams(&response.body)
    }
}

fn write_all(output: &mut dyn Write, text: &str) -> Result<(), VercelError> {
    output
        .write_all(text.as_bytes())
        .and_then(|()| output.flush())
        .map_err(|_| VercelError::WriteFailed)
}

pub(crate) fn parse_teams(bytes: &[u8]) -> Result<Vec<Team>, VercelError> {
    let Ok(Value::Object(root)) = parse_strict_json_value(bytes) else {
        return Err(VercelError::InvalidTeamsResponse);
    };
    let Some(Value::Array(entries)) = root.get("teams") else {
        return Err(VercelError::InvalidTeamsResponse);
    };
    Ok(entries
        .iter()
        .filter_map(|entry| {
            let entry = entry.as_object()?;
            let id = entry.get("id")?.as_str()?;
            let slug = entry.get("slug")?.as_str()?;
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or(slug);
            Some(Team {
                id: id.to_owned(),
                slug: slug.to_owned(),
                name: name.to_owned(),
            })
        })
        .collect())
}

fn select_team<F: AsFd>(
    teams: &[Team],
    current: Option<&str>,
    interactive: bool,
    input: &F,
    output: &mut dyn Write,
) -> Result<Option<usize>, VercelError> {
    if teams.len() < 2 {
        return Ok((!teams.is_empty()).then_some(0));
    }
    let default_index = default_team_index(teams, current);
    let index = if interactive {
        team_picker::select_interactive(teams, default_index, input, output)?
    } else {
        team_picker::select_by_line(teams, default_index, input, output)?
    };
    Ok(Some(index))
}

fn default_team_index(teams: &[Team], current: Option<&str>) -> usize {
    current
        .and_then(|needle| {
            teams
                .iter()
                .position(|team| team.id == needle || team.slug == needle)
        })
        .unwrap_or(0)
}

fn take_login_session(
    issuer: &str,
    client_id: &str,
    token: TokenSet,
    team: Option<&Team>,
    now_ms: i64,
) -> Result<Session, VercelError> {
    let refresh_token = token.refresh_token.ok_or(VercelError::NoRefreshToken)?;
    let expires_at_ms = oauth::expiry_timestamp_ms(now_ms, token.expires_in)?;
    if let Some(missing) = device::missing_granted_scope(DEFAULT_SCOPE, &token.scope) {
        trace_log!(
            AUTH,
            "issuer reduced the grant missing_scope={missing} granted={}",
            token.scope
        );
    }
    Ok(Session {
        issuer: issuer.to_owned(),
        client_id: client_id.to_owned(),
        access_token: token.access_token,
        refresh_token,
        expires_at_ms,
        scope: token.scope,
        token_type: token.token_type,
        team_slug: team.map(|team| team.slug.clone()),
        team_id: team.map(|team| team.id.clone()),
    })
}

#[cfg(test)]
mod tests;
