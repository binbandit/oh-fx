use std::env;
use std::os::unix::ffi::OsStrExt;

use ofx_trace::trace_log;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::auth_runtime::{PreparationError, classify_vercel_failure, preparation_error};
use crate::login_flow::{VercelError, VercelOAuth};
use crate::stored_key::{StoredKeyError, StoredKeyStore};
use crate::subscription_access::now_ms;

mod fx_login;

pub use fx_login::VERCEL_SENT_REFRESH_LIMIT;

pub(crate) const GROK_SOURCE_LABEL: &str = "Grok subscription";

pub const AUTH_MODE_VARIABLE: &str = "OH_FX_AUTH_MODE";

pub const MISSING_CHATGPT_CREDENTIAL_MESSAGE: &str =
    "oh-fx needs a Codex subscription login for this model. Run oh-fx login codex.";
pub const CHATGPT_RELOGIN_MESSAGE: &str = "Run oh-fx login codex to sign in again.";
pub const HOST_MANAGED_AUTH_MESSAGE: &str = "Authentication is managed by the host.";
pub const CHATGPT_SOURCE_LABEL: &str = "Codex subscription";
pub const MISSING_GATEWAY_CREDENTIAL_MESSAGE: &str = "oh-fx needs access to Vercel AI Gateway. Run oh-fx login to sign in, oh-fx setup to use an API key, or set AI_GATEWAY_API_KEY.";
pub const UNREADABLE_STORED_KEY_MESSAGE: &str = if cfg!(target_os = "macos") {
    "oh-fx could not read the stored API key from macOS Keychain. A key may be saved but unreadable. Set OH_FX_TRACE_LOG for the failing step, or set AI_GATEWAY_API_KEY."
} else {
    "oh-fx could not read the stored API key from profile file. A key may be saved but unreadable. Set OH_FX_TRACE_LOG for the failing step, or set AI_GATEWAY_API_KEY."
};
const SELECTED_STORED_KEY_UNREADABLE: &str = if cfg!(target_os = "macos") {
    "The selected stored API key could not be read from macOS Keychain. Start oh-fx and open /provider to choose an available credential; no other credential was selected."
} else {
    "The selected stored API key could not be read from profile file. Start oh-fx and open /provider to choose an available credential; no other credential was selected."
};
const STORED_KEY_LABEL: &str = if cfg!(target_os = "macos") {
    "stored API key (macOS Keychain)"
} else {
    "stored API key (profile file)"
};

const BLANK: [char; 4] = [' ', '\t', '\r', '\n'];
const AUTH: &str = "auth";
const MAX_TEAM_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewaySource {
    VercelOidcToken,
    AiGatewayApiKey,
    FxLogin,
    StoredKey,
}

impl GatewaySource {
    const ENVIRONMENT: [(Self, &'static str); 2] = [
        (Self::VercelOidcToken, "VERCEL_OIDC_TOKEN"),
        (Self::AiGatewayApiKey, "AI_GATEWAY_API_KEY"),
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::VercelOidcToken => "VERCEL_OIDC_TOKEN",
            Self::AiGatewayApiKey => "AI_GATEWAY_API_KEY",
            Self::FxLogin => "oh-fx login",
            Self::StoredKey => STORED_KEY_LABEL,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::VercelOidcToken => "vercel_oidc_token",
            Self::AiGatewayApiKey => "ai_gateway_api_key",
            Self::FxLogin => "fx_login",
            Self::StoredKey => "stored_key",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        [
            Self::VercelOidcToken,
            Self::AiGatewayApiKey,
            Self::FxLogin,
            Self::StoredKey,
        ]
        .into_iter()
        .find(|source| source.name() == name)
    }

    pub const fn refreshable(self) -> bool {
        matches!(self, Self::FxLogin)
    }

    const fn selected_message(self, login_unavailable: bool) -> &'static str {
        match self {
            Self::VercelOidcToken => {
                "VERCEL_OIDC_TOKEN is selected but unavailable. Set VERCEL_OIDC_TOKEN before starting oh-fx; no other credential was selected."
            }
            Self::AiGatewayApiKey => {
                "AI_GATEWAY_API_KEY is selected but unavailable. Set AI_GATEWAY_API_KEY before starting oh-fx; no other credential was selected."
            }
            Self::StoredKey => {
                "A stored API key is selected but unavailable. Start oh-fx and open /provider to choose an available credential; no other credential was selected."
            }
            Self::FxLogin if login_unavailable => {
                "The saved oh-fx login could not be loaded. Run oh-fx login to repair this source; no other credential was selected."
            }
            Self::FxLogin => {
                "oh-fx login is selected but unavailable. Run oh-fx login to reconnect; no other credential was selected."
            }
        }
    }
}

pub struct GatewayKey {
    pub source: GatewaySource,
    pub secret: Zeroizing<String>,
    pub team_id: Option<String>,
    pub team_slug: Option<String>,
    pub refresh_after_ms: Option<i64>,
}

impl GatewayKey {
    fn environment(source: GatewaySource, secret: Zeroizing<String>) -> Self {
        Self {
            source,
            secret,
            team_id: None,
            team_slug: None,
            refresh_after_ms: None,
        }
    }

    pub fn gateway_team(&self) -> Option<&str> {
        self.team_id.as_deref().or(self.team_slug.as_deref())
    }

    pub fn display_team(&self) -> Option<&str> {
        self.team_slug.as_deref().or(self.team_id.as_deref())
    }

    pub fn needs_refresh_at(&self, now_ms: i64) -> bool {
        self.refresh_after_ms
            .is_some_and(|refresh_after| refresh_after <= now_ms)
    }
}

pub fn valid_gateway_team(team: &str) -> bool {
    !team.is_empty()
        && team.len() <= MAX_TEAM_BYTES
        && team
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Read {
    NotAttempted,
    Absent,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    Login(VercelError),
    StoredKey,
}

impl Failure {
    fn preparation(self) -> Option<PreparationError> {
        match self {
            Self::Login(error) => preparation_error(classify_vercel_failure(error)),
            Self::StoredKey => Some(PreparationError::CredentialStorageUnavailable),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayRefusal {
    Missing(&'static str),
    Preparation(PreparationError),
}

pub struct GatewayResolution {
    key: Option<GatewayKey>,
    preferred: Option<GatewaySource>,
    login: Read,
    stored_key: Read,
    failure: Option<Failure>,
}

impl GatewayResolution {
    fn found(key: GatewayKey, preferred: Option<GatewaySource>) -> Self {
        Self {
            key: Some(key),
            ..Self::empty(preferred)
        }
    }

    const fn empty(preferred: Option<GatewaySource>) -> Self {
        Self {
            key: None,
            preferred,
            login: Read::NotAttempted,
            stored_key: Read::NotAttempted,
            failure: None,
        }
    }

    pub fn key(&self) -> Option<&GatewayKey> {
        self.key.as_ref()
    }

    pub fn unreadable(&self) -> bool {
        self.login == Read::Unavailable || self.stored_key == Read::Unavailable
    }

    pub fn help(&self) -> Option<&'static str> {
        if self.key.is_some() {
            return None;
        }
        if self.stored_key == Read::Unavailable {
            return Some(if self.preferred == Some(GatewaySource::StoredKey) {
                SELECTED_STORED_KEY_UNREADABLE
            } else {
                UNREADABLE_STORED_KEY_MESSAGE
            });
        }
        if let Some(preparation) = self.failure.and_then(Failure::preparation) {
            return Some(preparation.notice());
        }
        Some(
            self.preferred
                .map_or(MISSING_GATEWAY_CREDENTIAL_MESSAGE, |source| {
                    source.selected_message(self.login == Read::Unavailable)
                }),
        )
    }

    pub fn prepared(self) -> Result<GatewayKey, GatewayRefusal> {
        let missing = Err(GatewayRefusal::Missing(
            self.preferred
                .map_or(MISSING_GATEWAY_CREDENTIAL_MESSAGE, |source| {
                    source.selected_message(false)
                }),
        ));
        let Some(key) = self.key else {
            if self.stored_key == Read::Unavailable && self.preferred.is_none() {
                return Err(GatewayRefusal::Missing(UNREADABLE_STORED_KEY_MESSAGE));
            }
            if let Some(failure) = self.failure {
                return match failure.preparation() {
                    Some(preparation) => Err(GatewayRefusal::Preparation(preparation)),
                    None => missing,
                };
            }
            return missing;
        };
        let blocked = key.secret.is_empty()
            || key.needs_refresh_at(now_ms())
            || (key.source == GatewaySource::FxLogin
                && !key.gateway_team().is_some_and(valid_gateway_team));
        if blocked { missing } else { Ok(key) }
    }
}

pub struct GatewaySources<'a> {
    pub lookup: &'a dyn Fn(&str) -> Option<String>,
    pub store: &'a StoredKeyStore,
    pub login: Option<&'a VercelOAuth>,
    pub preferred: Option<GatewaySource>,
}

impl GatewaySources<'_> {
    pub fn stored(&self) -> GatewayResolution {
        let login = match self.login {
            Some(login) if self.needs_login() => login.load_stored_credential(),
            _ => Ok(None),
        };
        self.resolve(login, "stored")
    }

    pub async fn refreshed(&self) -> GatewayResolution {
        let login = match self.login {
            Some(login) if self.needs_login() => {
                login.load_credential(&CancellationToken::new()).await
            }
            _ => Ok(None),
        };
        self.resolve(login, "refresh_if_needed")
    }

    fn needs_login(&self) -> bool {
        match self.preferred {
            Some(source) => source == GatewaySource::FxLogin,
            None => self.environment().is_none(),
        }
    }

    fn environment(&self) -> Option<GatewayKey> {
        GatewaySource::ENVIRONMENT
            .into_iter()
            .find_map(|(source, variable)| self.variable(source, variable))
    }

    fn variable(&self, source: GatewaySource, variable: &str) -> Option<GatewayKey> {
        let secret = Zeroizing::new((self.lookup)(variable)?);
        (!secret.trim_matches(BLANK).is_empty()).then(|| GatewayKey::environment(source, secret))
    }

    fn resolve(
        &self,
        login: Result<Option<GatewayKey>, VercelError>,
        mode: &str,
    ) -> GatewayResolution {
        match self.preferred {
            Some(source) => self.resolve_selected(source, login),
            None => self.resolve_automatic(login, mode),
        }
    }

    fn resolve_automatic(
        &self,
        login: Result<Option<GatewayKey>, VercelError>,
        mode: &str,
    ) -> GatewayResolution {
        if let Some(key) = self.environment() {
            return GatewayResolution::found(key, None);
        }
        let mut resolution = GatewayResolution::empty(None);
        match login {
            Ok(Some(key)) => return GatewayResolution::found(key, None),
            Ok(None) => resolution.login = Read::Absent,
            Err(error) => {
                trace_log!(
                    AUTH,
                    "fx login load failed mode={mode} err={error}; using precedence"
                );
                resolution.login = Read::Unavailable;
                resolution.failure = Some(Failure::Login(error));
            }
        }
        if self.store.disabled() {
            return resolution;
        }
        match self.store.load() {
            Ok(Some(secret)) => {
                resolution.key = Some(GatewayKey::environment(GatewaySource::StoredKey, secret));
                resolution.failure = None;
            }
            Ok(None) => resolution.stored_key = Read::Absent,
            Err(error) => {
                trace_log!(
                    AUTH,
                    "stored key load failed err={error} status=unavailable"
                );
                resolution.stored_key = Read::Unavailable;
                resolution.failure = Some(Failure::StoredKey);
            }
        }
        resolution
    }

    fn resolve_selected(
        &self,
        source: GatewaySource,
        login: Result<Option<GatewayKey>, VercelError>,
    ) -> GatewayResolution {
        let name = source.name();
        let mut resolution = GatewayResolution::empty(Some(source));
        if source == GatewaySource::StoredKey && self.store.disabled() {
            trace_log!(
                AUTH,
                "explicit source unavailable source={name} reason=disabled"
            );
            return resolution;
        }
        let loaded = match source {
            GatewaySource::VercelOidcToken => Ok(self.variable(source, "VERCEL_OIDC_TOKEN")),
            GatewaySource::AiGatewayApiKey => Ok(self.variable(source, "AI_GATEWAY_API_KEY")),
            GatewaySource::FxLogin => login.map_err(Failure::Login),
            GatewaySource::StoredKey => self
                .store
                .load()
                .map(|key| key.map(|secret| GatewayKey::environment(source, secret)))
                .map_err(|_: StoredKeyError| Failure::StoredKey),
        };
        let read = match loaded {
            Ok(Some(key)) => return GatewayResolution::found(key, Some(source)),
            Ok(None) => {
                trace_log!(AUTH, "explicit source unavailable source={name}");
                Read::Absent
            }
            Err(failure) => {
                match failure {
                    Failure::Login(error) => trace_log!(
                        AUTH,
                        "explicit source load failed source={name} err={error}"
                    ),
                    Failure::StoredKey => trace_log!(
                        AUTH,
                        "explicit source load failed source={name} err=StoredKeyUnreadable"
                    ),
                }
                resolution.failure = Some(failure);
                Read::Unavailable
            }
        };
        match source {
            GatewaySource::FxLogin => resolution.login = read,
            GatewaySource::StoredKey => resolution.stored_key = read,
            GatewaySource::VercelOidcToken | GatewaySource::AiGatewayApiKey => {}
        }
        resolution
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Local,
    HostManaged,
}

pub fn parse_auth_mode(value: Option<&[u8]>) -> Option<AuthMode> {
    match value {
        None | Some(b"local") => Some(AuthMode::Local),
        Some(b"host-managed") => Some(AuthMode::HostManaged),
        Some(_) => None,
    }
}

pub fn host_managed_auth() -> bool {
    let mode = env::var_os(AUTH_MODE_VARIABLE);
    parse_auth_mode(mode.as_deref().map(OsStrExt::as_bytes)) == Some(AuthMode::HostManaged)
}

pub fn is_valid_auth_mode(value: Option<&[u8]>) -> bool {
    parse_auth_mode(value).is_some()
}

#[cfg(test)]
mod tests;
