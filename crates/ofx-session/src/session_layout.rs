use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

const MAX_SESSION_ID_BYTES: usize = 255;
const SESSIONS_V2_DIR: &str = "v2";
const SESSION_ID_RANDOM_BYTES: usize = 9;

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

pub(crate) fn generate_session_id() -> Option<String> {
    let mut random = [0_u8; SESSION_ID_RANDOM_BYTES];
    getrandom::fill(&mut random).ok()?;
    Some(URL_SAFE_NO_PAD.encode(random))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_session_id_is_a_compact_url_safe_token() {
        let id = generate_session_id().unwrap();
        assert_eq!(id.len(), 12);
        assert!(is_valid_session_id(&id));
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        );
        assert_ne!(generate_session_id().unwrap(), id);
        assert!(is_valid_session_id(
            "1786460757753-1786460757753277000-ef75d8fd94fdab1"
        ));
    }

    #[test]
    fn session_ids_reject_separators_nul_and_non_ascii() {
        for id in [
            "/tmp/outside",
            "nested/session",
            "nested\\session",
            "a\0b",
            "\u{ff}a",
        ] {
            assert!(!is_valid_session_id(id), "{id:?}");
        }
        for id in [".hidden-session", "session..branch", "last", "resume"] {
            assert!(is_valid_session_id(id), "{id:?}");
        }
    }

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
