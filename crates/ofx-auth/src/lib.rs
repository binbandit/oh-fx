mod auth_runtime;
mod browser_callback;
mod chatgpt_oauth;
mod chatgpt_session;
mod credentials;
mod oauth;
mod oauth_transport;
mod provider_catalog;
mod secret;
mod session_presence;
mod url_opener;

pub use auth_runtime::{
    PreparationError, login_failure_detail, prepare_chatgpt_credential, refresh_chatgpt_credential,
};
pub use chatgpt_oauth::{
    CHATGPT_REFRESH_LIMIT, ChatGptAccess, ChatGptEndpoints, ChatGptError, ChatGptOAuth, RefreshMode,
};
pub use chatgpt_session::DeleteOutcome;
pub use credentials::{
    AuthMode, CHATGPT_RELOGIN_MESSAGE, CHATGPT_SOURCE_LABEL, HOST_MANAGED_AUTH_MESSAGE,
    MISSING_CHATGPT_CREDENTIAL_MESSAGE, is_valid_auth_mode, parse_auth_mode,
};
pub use provider_catalog::{label as provider_route_name, parse as parse_login_provider};
