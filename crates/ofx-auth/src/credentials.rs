use std::env;
use std::os::unix::ffi::OsStrExt;

use zeroize::Zeroizing;

pub(crate) const GROK_SOURCE_LABEL: &str = "Grok subscription";

pub const AUTH_MODE_VARIABLE: &str = "OH_FX_AUTH_MODE";

pub const MISSING_CHATGPT_CREDENTIAL_MESSAGE: &str =
    "oh-fx needs a Codex subscription login for this model. Run oh-fx login codex.";
pub const CHATGPT_RELOGIN_MESSAGE: &str = "Run oh-fx login codex to sign in again.";
pub const HOST_MANAGED_AUTH_MESSAGE: &str = "Authentication is managed by the host.";
pub const CHATGPT_SOURCE_LABEL: &str = "Codex subscription";
pub const MISSING_GATEWAY_CREDENTIAL_MESSAGE: &str =
    "oh-fx needs access to Vercel AI Gateway. Set AI_GATEWAY_API_KEY.";

const BLANK: [char; 4] = [' ', '\t', '\r', '\n'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewaySource {
    VercelOidcToken,
    AiGatewayApiKey,
}

impl GatewaySource {
    const PRECEDENCE: [Self; 2] = [Self::VercelOidcToken, Self::AiGatewayApiKey];

    pub const fn variable(self) -> &'static str {
        match self {
            Self::VercelOidcToken => "VERCEL_OIDC_TOKEN",
            Self::AiGatewayApiKey => "AI_GATEWAY_API_KEY",
        }
    }
}

pub struct GatewayEnvCredential {
    pub source: GatewaySource,
    pub secret: Zeroizing<String>,
}

pub fn gateway_env_credential(
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<GatewayEnvCredential> {
    GatewaySource::PRECEDENCE.into_iter().find_map(|source| {
        let secret = Zeroizing::new(lookup(source.variable())?);
        (!secret.trim_matches(BLANK).is_empty()).then_some(GatewayEnvCredential { source, secret })
    })
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
mod tests {
    use super::*;

    #[test]
    fn auth_mode_accepts_only_local_and_host_managed_process_values() {
        for valid in [None, Some(&b"local"[..]), Some(b"host-managed")] {
            assert!(is_valid_auth_mode(valid), "{valid:?}");
        }
        for invalid in [&b""[..], b"Local", b" local", b"host_managed", b"remote"] {
            assert!(!is_valid_auth_mode(Some(invalid)), "{invalid:?}");
        }
        assert_eq!(parse_auth_mode(None), Some(AuthMode::Local));
        assert_eq!(
            parse_auth_mode(Some(b"host-managed")),
            Some(AuthMode::HostManaged)
        );
    }

    #[test]
    fn public_credential_guidance_spells_oh_fx_lowercase() {
        for message in [MISSING_CHATGPT_CREDENTIAL_MESSAGE, CHATGPT_RELOGIN_MESSAGE] {
            assert!(message.contains("Run oh-fx login codex"), "{message}");
        }
        assert!(MISSING_GATEWAY_CREDENTIAL_MESSAGE.starts_with("oh-fx needs"));
    }

    fn resolved(variables: &[(&str, &str)]) -> Option<(GatewaySource, String)> {
        let lookup = |name: &str| {
            variables
                .iter()
                .find(|(variable, _)| *variable == name)
                .map(|(_, value)| (*value).to_owned())
        };
        gateway_env_credential(&lookup)
            .map(|credential| (credential.source, credential.secret.to_string()))
    }

    #[test]
    fn the_oidc_token_outranks_the_api_key_and_blank_values_are_absent() {
        let oidc = GatewaySource::VercelOidcToken;
        let key = GatewaySource::AiGatewayApiKey;
        assert_eq!(resolved(&[]), None);
        assert_eq!(
            resolved(&[
                ("AI_GATEWAY_API_KEY", "key"),
                ("VERCEL_OIDC_TOKEN", "token")
            ]),
            Some((oidc, "token".to_owned()))
        );
        assert_eq!(
            resolved(&[
                ("AI_GATEWAY_API_KEY", " key\n"),
                ("VERCEL_OIDC_TOKEN", " \t\r\n")
            ]),
            Some((key, " key\n".to_owned()))
        );
        assert_eq!(resolved(&[("AI_GATEWAY_API_KEY", "")]), None);
        assert_eq!(oidc.variable(), "VERCEL_OIDC_TOKEN");
        assert_eq!(key.variable(), "AI_GATEWAY_API_KEY");
    }
}
