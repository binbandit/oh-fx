use ofx_config::PrivateDir;
use ofx_text::lowercase_hex;
use sha2::{Digest, Sha256};

use crate::session_error::SessionError;
use crate::session_log::managed_file::read_managed_file;

pub(crate) const PREVIEW_BYTES: usize = 4 * 1024;
const STORED_TEXT_MAX_BYTES: usize = 8 * 1024 * 1024;
const TOOL_RESULTS_DIR: &str = "tool-results";
const HANDLE_PREFIX: &str = "result-";
const HANDLE_SUFFIX: &str = ".txt";
const MAX_HANDLE_BYTES: usize = 160;
const MAX_TOOL_PART_BYTES: usize = 48;
const DIGEST_HEX_BYTES: usize = 8;

pub(crate) fn make_handle(tool_call_id: &str, tool_name: &str, text: &str) -> String {
    let mut handle = String::from(HANDLE_PREFIX);
    if tool_name.is_empty() {
        handle.push_str("call");
    }
    for byte in tool_name.bytes().take(MAX_TOOL_PART_BYTES) {
        let safe = byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-');
        handle.push(if safe { char::from(byte) } else { '-' });
    }
    handle.push('-');
    push_digest_hex(&mut handle, tool_call_id.as_bytes());
    handle.push('-');
    push_digest_hex(&mut handle, text.as_bytes());
    handle.push_str(HANDLE_SUFFIX);
    handle
}

pub(crate) fn store_result(
    session: &PrivateDir,
    handle: &str,
    text: &str,
) -> Result<(), SessionError> {
    if !is_valid_handle(handle) || text.len() > STORED_TEXT_MAX_BYTES {
        return Err(SessionError::InvalidConversationEvent);
    }
    let results = session.open_or_create_child(TOOL_RESULTS_DIR)?;
    results.replace(handle, text.as_bytes())?;
    Ok(())
}

pub(crate) fn read_for_replay(
    session: &PrivateDir,
    handle: &str,
    expected_bytes: u64,
) -> Option<String> {
    let expected = usize::try_from(expected_bytes)
        .ok()
        .filter(|bytes| *bytes <= STORED_TEXT_MAX_BYTES)?;
    if !is_valid_handle(handle) {
        return None;
    }
    let results = session.open_child(TOOL_RESULTS_DIR).ok()??;
    let bytes = read_managed_file(&results, handle, expected).ok()??;
    if bytes.len() != expected {
        return None;
    }
    String::from_utf8(bytes).ok()
}

pub(crate) fn preview(text: &str) -> &str {
    &text[..text.floor_char_boundary(PREVIEW_BYTES)]
}

pub(crate) fn format_stored_result_output(
    handle: &str,
    preview: &str,
    stored_bytes: u64,
) -> String {
    format!(
        "<tool_result_preview handle=\"{handle}\" stored_bytes=\"{stored_bytes}\">\n{preview}\n</tool_result_preview>\n<tool_result_handle>{handle}</tool_result_handle>\nFull result is stored outside session JSON. Use read_tool_result with this handle to inspect a byte range or literal query."
    )
}

fn is_valid_handle(handle: &str) -> bool {
    (1..=MAX_HANDLE_BYTES).contains(&handle.len())
        && !handle.contains("..")
        && handle
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn push_digest_hex(out: &mut String, bytes: &[u8]) {
    out.push_str(&lowercase_hex(&Sha256::digest(bytes)[..DIGEST_HEX_BYTES]));
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn session() -> (tempfile::TempDir, PrivateDir) {
        let root = tempfile::tempdir().unwrap();
        let dir = PrivateDir::open_or_create(&root.path().join("session")).unwrap();
        (root, dir)
    }

    #[test]
    fn large_result_storage_creates_a_stable_handle_and_bounded_preview() {
        let (root, dir) = session();
        let text = "x".repeat(PREVIEW_BYTES * 2);
        let handle = make_handle("call_1", "shell", &text);
        let digests = handle
            .strip_prefix("result-shell-")
            .and_then(|rest| rest.strip_suffix(HANDLE_SUFFIX))
            .unwrap();
        assert_eq!(digests.len(), 16 + 1 + 16);
        assert_eq!(handle, make_handle("call_1", "shell", &text));
        assert_ne!(handle, make_handle("call_2", "shell", &text));
        assert_ne!(handle, make_handle("call_1", "shell", "other"));
        store_result(&dir, &handle, &text).unwrap();
        let stored = root.path().join("session/tool-results").join(&handle);
        assert_eq!(std::fs::read_to_string(&stored).unwrap(), text);
        assert_eq!(
            std::fs::metadata(&stored).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(root.path().join("session/tool-results"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(preview(&text).len(), PREVIEW_BYTES);
        let bytes = u64::try_from(text.len()).unwrap();
        assert_eq!(read_for_replay(&dir, &handle, bytes), Some(text));
        assert_eq!(read_for_replay(&dir, &handle, bytes - 1), None);
    }

    #[test]
    fn handles_never_expose_token_shaped_call_ids_or_unsafe_tool_names() {
        let secret = "sk-live-0123456789abcdef";
        let handle = make_handle(secret, "mcp/server tool..x", "body");
        assert!(!handle.contains(secret));
        assert!(handle.starts_with("result-mcp-server-tool--x-"));
        assert!(is_valid_handle(&make_handle("c", &"t".repeat(200), "body")));
        assert!(make_handle("c", "", "body").starts_with("result-call-"));
        assert!(!is_valid_handle("../escape.txt"));
        assert!(!is_valid_handle("a/b.txt"));
        assert!(!is_valid_handle(""));
    }

    #[test]
    fn previews_keep_complete_codepoints() {
        let text = format!("x{}", "\u{e9}".repeat(PREVIEW_BYTES));
        let cut = preview(&text);
        assert!(cut.len() <= PREVIEW_BYTES);
        assert!(cut.ends_with('\u{e9}'));
    }

    #[test]
    fn replay_reads_refuse_missing_linked_and_resized_results() {
        let (root, dir) = session();
        assert_eq!(read_for_replay(&dir, "result-x-0-0.txt", 4), None);
        let handle = make_handle("c", "t", "body");
        store_result(&dir, &handle, "body").unwrap();
        let results = root.path().join("session/tool-results");
        std::fs::write(results.join(&handle), "bodies").unwrap();
        assert_eq!(read_for_replay(&dir, &handle, 4), None);
        std::fs::remove_file(results.join(&handle)).unwrap();
        std::fs::write(root.path().join("outside"), "body").unwrap();
        symlink(root.path().join("outside"), results.join(&handle)).unwrap();
        assert_eq!(read_for_replay(&dir, &handle, 4), None);
        assert_eq!(
            store_result(&dir, "../outside", "x"),
            Err(SessionError::InvalidConversationEvent)
        );
    }

    #[test]
    fn stored_previews_point_at_the_handle() {
        assert_eq!(
            format_stored_result_output("result-a-1-2.txt", "head", 99),
            "<tool_result_preview handle=\"result-a-1-2.txt\" stored_bytes=\"99\">\nhead\n</tool_result_preview>\n<tool_result_handle>result-a-1-2.txt</tool_result_handle>\nFull result is stored outside session JSON. Use read_tool_result with this handle to inspect a byte range or literal query."
        );
    }
}
