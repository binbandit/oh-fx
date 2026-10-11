use std::env;
use std::os::unix::ffi::OsStrExt;

use zeroize::Zeroizing;

use crate::stored_key::{StoredKeyError, StoredKeyStore};

pub(crate) const GROK_SOURCE_LABEL: &str = "Grok subscription";

pub const AUTH_MODE_VARIABLE: &str = "OH_FX_AUTH_MODE";

pub const MISSING_CHATGPT_CREDENTIAL_MESSAGE: &str =
    "oh-fx needs a Codex subscription login for this model. Run oh-fx login codex.";
pub const CHATGPT_RELOGIN_MESSAGE: &str = "Run oh-fx login codex to sign in again.";
pub const HOST_MANAGED_AUTH_MESSAGE: &str = "Authentication is managed by the host.";
pub const CHATGPT_SOURCE_LABEL: &str = "Codex subscription";
pub const MISSING_GATEWAY_CREDENTIAL_MESSAGE: &str = "oh-fx needs access to Vercel AI Gateway. Run oh-fx setup to use an API key, or set AI_GATEWAY_API_KEY.";
pub const UNREADABLE_STORED_KEY_MESSAGE: &str = if cfg!(target_os = "macos") {
    "oh-fx could not read the stored API key from macOS Keychain. A key may be saved but unreadable. Set OH_FX_TRACE_LOG for the failing step, or set AI_GATEWAY_API_KEY."
} else {
    "oh-fx could not read the stored API key from profile file. A key may be saved but unreadable. Set OH_FX_TRACE_LOG for the failing step, or set AI_GATEWAY_API_KEY."
};
const STORED_KEY_LABEL: &str = if cfg!(target_os = "macos") {
    "stored API key (macOS Keychain)"
} else {
    "stored API key (profile file)"
};

const BLANK: [char; 4] = [' ', '\t', '\r', '\n'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewaySource {
    VercelOidcToken,
    AiGatewayApiKey,
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
            Self::StoredKey => STORED_KEY_LABEL,
        }
    }
}

pub struct GatewayKey {
    pub source: GatewaySource,
    pub secret: Zeroizing<String>,
}

pub fn gateway_credential(
    lookup: &dyn Fn(&str) -> Option<String>,
    store: &StoredKeyStore,
) -> Result<Option<GatewayKey>, StoredKeyError> {
    let environment = GatewaySource::ENVIRONMENT
        .into_iter()
        .find_map(|(source, variable)| {
            let secret = Zeroizing::new(lookup(variable)?);
            (!secret.trim_matches(BLANK).is_empty()).then_some(GatewayKey { source, secret })
        });
    if environment.is_some() {
        return Ok(environment);
    }
    Ok(store.load()?.map(|secret| GatewayKey {
        source: GatewaySource::StoredKey,
        secret,
    }))
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
        assert!(UNREADABLE_STORED_KEY_MESSAGE.starts_with("oh-fx could"));
        let setup = MISSING_GATEWAY_CREDENTIAL_MESSAGE
            .find("oh-fx setup")
            .unwrap();
        let variable = MISSING_GATEWAY_CREDENTIAL_MESSAGE
            .find("AI_GATEWAY_API_KEY")
            .unwrap();
        assert!(setup < variable);
        assert!(UNREADABLE_STORED_KEY_MESSAGE.contains(crate::stored_key::STORED_KEY_BACKEND));
        assert!(
            GatewaySource::StoredKey
                .label()
                .contains(crate::stored_key::STORED_KEY_BACKEND)
        );
    }

    fn resolved_with(
        variables: &[(&str, &str)],
        store: &StoredKeyStore,
    ) -> Result<Option<(GatewaySource, String)>, StoredKeyError> {
        let lookup = |name: &str| {
            variables
                .iter()
                .find(|(variable, _)| *variable == name)
                .map(|(_, value)| (*value).to_owned())
        };
        gateway_credential(&lookup, store)
            .map(|key| key.map(|key| (key.source, key.secret.to_string())))
    }

    fn resolved(variables: &[(&str, &str)]) -> Option<(GatewaySource, String)> {
        let empty = tempfile::tempdir().unwrap();
        let store = StoredKeyStore::in_profile(Some(&empty.path().join("oh-fx")));
        resolved_with(variables, &store).unwrap()
    }

    #[test]
    fn the_stored_key_answers_only_after_the_environment_sources() {
        let root = tempfile::tempdir().unwrap();
        let store = StoredKeyStore::in_profile(Some(&root.path().join("oh-fx")));
        store
            .save(|| Some(Zeroizing::new("vck-stored".to_owned())))
            .unwrap();
        assert_eq!(
            resolved_with(&[], &store),
            Ok(Some((GatewaySource::StoredKey, "vck-stored".to_owned())))
        );
        assert_eq!(
            resolved_with(&[("AI_GATEWAY_API_KEY", "vck-env")], &store),
            Ok(Some((GatewaySource::AiGatewayApiKey, "vck-env".to_owned())))
        );
        let unreadable = root.path().join("oh-fx/api-key");
        std::fs::write(&unreadable, "k".repeat(9 * 1024)).unwrap();
        assert_eq!(resolved_with(&[], &store), Err(StoredKeyError::Unreadable));
        assert_eq!(
            resolved_with(&[("VERCEL_OIDC_TOKEN", "oidc")], &store),
            Ok(Some((GatewaySource::VercelOidcToken, "oidc".to_owned())))
        );
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
        assert_eq!(oidc.label(), "VERCEL_OIDC_TOKEN");
        assert_eq!(key.label(), "AI_GATEWAY_API_KEY");
    }
}
