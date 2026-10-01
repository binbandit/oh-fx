pub fn is_valid_auth_mode(value: Option<&[u8]>) -> bool {
    value.is_none_or(|mode| mode == b"local" || mode == b"host-managed")
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
    }
}
