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
}
