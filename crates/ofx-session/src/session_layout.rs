const MAX_SESSION_ID_BYTES: usize = 255;
const SESSIONS_V2_DIR: &str = "v2";

pub fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID_BYTES
        && id != "."
        && id != ".."
        && !id.eq_ignore_ascii_case(SESSIONS_V2_DIR)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_bounded_path_safe_names() {
        assert!(is_valid_session_id("session.v3"));
        assert!(is_valid_session_id("a_b-c"));
        assert!(is_valid_session_id("..."));
        assert!(is_valid_session_id(&"a".repeat(MAX_SESSION_ID_BYTES)));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("."));
        assert!(!is_valid_session_id(".."));
        assert!(!is_valid_session_id("../unsafe"));
        assert!(!is_valid_session_id("a b"));
        assert!(!is_valid_session_id(&"a".repeat(MAX_SESSION_ID_BYTES + 1)));
    }

    #[test]
    fn the_sessions_v2_root_is_never_a_v1_session_id() {
        assert!(!is_valid_session_id(SESSIONS_V2_DIR));
        assert!(!is_valid_session_id("V2"));
        assert!(is_valid_session_id("v2x"));
    }
}
