use std::env;
use std::os::unix::ffi::OsStrExt;

pub(crate) const GROK_SOURCE_LABEL: &str = "Grok subscription";

pub const AUTH_MODE_VARIABLE: &str = "OH_FX_AUTH_MODE";

pub const MISSING_CHATGPT_CREDENTIAL_MESSAGE: &str =
    "oh-fx needs a Codex subscription login for this model. Run oh-fx login codex.";
pub const CHATGPT_RELOGIN_MESSAGE: &str = "Run oh-fx login codex to sign in again.";
pub const HOST_MANAGED_AUTH_MESSAGE: &str = "Authentication is managed by the host.";
pub const CHATGPT_SOURCE_LABEL: &str = "Codex subscription";

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
    }
}
