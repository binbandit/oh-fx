use std::io::Write as _;

use ofx_config::PrivateDir;
use ofx_text::lowercase_hex;
use sha2::{Digest, Sha256};

use crate::artifact_digest::is_lower_hex;
use crate::session_error::SessionError;
use crate::session_log::managed_file::{create_managed_file, sync_dir};

pub(crate) const PREVIEW_BYTES: usize = 4 * 1024;
pub(crate) const RESULT_UNAVAILABLE: &str =
    "Saved tool-result content is unavailable. The complete output could not be restored.";
pub(crate) const STORED_TEXT_MAX_BYTES: usize = 8 * 1024 * 1024;
const TOOL_RESULTS_DIR: &str = "tool-results";
const HANDLE_PREFIX: &str = "result-";
const HANDLE_SUFFIX: &str = ".txt";
const MAX_HANDLE_BYTES: usize = 160;
const MAX_TOOL_PART_BYTES: usize = 48;
const DIGEST_HEX_BYTES: usize = 8;
const DIFF_HANDLE_PREFIX: &str = "diff-";
const DIFF_HANDLE_SUFFIX: &str = ".json";
pub(crate) const DIFF_CONTENT_MAX_BYTES: usize = 2 * STORED_TEXT_MAX_BYTES;
const DIFF_DIGEST_HEX_BYTES: usize = 2 * DIGEST_HEX_BYTES;
const PACK_PREVIOUS: &[u8] = b"{\"previous_content\":";
const PACK_AFTER: &[u8] = b",\"after_content\":";
const PACK_NULL: &[u8] = b"null";
const MAX_ESCAPED_BYTE_LEN: usize = 6;

pub(crate) fn make_handle(tool_call_id: &str, tool_name: &str, text: &str) -> String {
    bytes_handle(tool_call_id, tool_name, text.as_bytes())
}

pub(crate) fn bytes_handle(tool_call_id: &str, tool_name: &str, bytes: &[u8]) -> String {
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
    push_digest_hex(&mut handle, bytes);
    handle.push_str(HANDLE_SUFFIX);
    handle
}

pub(crate) fn fits_diff_pack(
    previous_content: Option<&[u8]>,
    after_content: Option<&[u8]>,
) -> bool {
    let raw = [previous_content, after_content]
        .into_iter()
        .flatten()
        .map(<[u8]>::len)
        .fold(0_usize, usize::saturating_add);
    let framing = PACK_PREVIOUS.len() + PACK_AFTER.len() + 1 + 2 * PACK_NULL.len();
    if raw
        .saturating_mul(MAX_ESCAPED_BYTE_LEN)
        .saturating_add(framing)
        <= DIFF_CONTENT_MAX_BYTES
    {
        return true;
    }
    let length = PACK_PREVIOUS.len()
        + pack_content_len(previous_content)
        + PACK_AFTER.len()
        + pack_content_len(after_content)
        + 1;
    length <= DIFF_CONTENT_MAX_BYTES
}

fn pack_content_len(content: Option<&[u8]>) -> usize {
    let Some(bytes) = content else {
        return PACK_NULL.len();
    };
    if std::str::from_utf8(bytes).is_ok() {
        return 2 + bytes.iter().map(|byte| escaped_len(*byte)).sum::<usize>();
    }
    let commas = bytes.len().saturating_sub(1);
    2 + commas + bytes.iter().map(|byte| decimal_len(*byte)).sum::<usize>()
}

fn escaped_len(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | 0x08 | 0x0c | b'\n' | b'\r' | b'\t' => 2,
        0x00..=0x1f => 6,
        _ => 1,
    }
}

fn decimal_len(byte: u8) -> usize {
    match byte {
        0..=9 => 1,
        10..=99 => 2,
        _ => 3,
    }
}

pub(crate) fn diff_content_pack(
    tool_call_id: &str,
    previous_content: Option<&[u8]>,
    after_content: Option<&[u8]>,
) -> Option<(String, Vec<u8>)> {
    let mut pack = PACK_PREVIOUS.to_vec();
    push_pack_content(&mut pack, previous_content)?;
    pack.extend_from_slice(PACK_AFTER);
    push_pack_content(&mut pack, after_content)?;
    pack.push(b'}');
    let mut handle = String::from(DIFF_HANDLE_PREFIX);
    push_digest_hex(&mut handle, tool_call_id.as_bytes());
    handle.push('-');
    push_digest_hex(&mut handle, &pack);
    handle.push_str(DIFF_HANDLE_SUFFIX);
    Some((handle, pack))
}

pub(crate) fn store_result(
    session: &PrivateDir,
    handle: &str,
    text: &str,
) -> Result<(), SessionError> {
    if !is_valid_handle(handle) {
        return Err(SessionError::InvalidConversationEvent);
    }
    let results = session.open_or_create_child(TOOL_RESULTS_DIR)?;
    results.replace(handle, text.as_bytes())?;
    Ok(())
}

fn push_pack_content(pack: &mut Vec<u8>, content: Option<&[u8]>) -> Option<()> {
    let Some(bytes) = content else {
        pack.extend_from_slice(PACK_NULL);
        return Some(());
    };
    if let Ok(text) = std::str::from_utf8(bytes) {
        return serde_json::to_writer(pack, text).ok();
    }
    pack.push(b'[');
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            pack.push(b',');
        }
        pack.extend_from_slice(byte.to_string().as_bytes());
    }
    pack.push(b']');
    Some(())
}

pub(crate) fn store_new_results<'a>(
    session: &PrivateDir,
    results: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> Result<(), SessionError> {
    let mut results = results.into_iter().peekable();
    if results.peek().is_none() {
        return Ok(());
    }
    let dir = session.open_or_create_child(TOOL_RESULTS_DIR)?;
    for (handle, bytes) in results {
        if !is_valid_handle(handle) {
            return Err(SessionError::InvalidConversationEvent);
        }
        match create_managed_file(&dir, handle) {
            Ok(mut file) => {
                file.write_all(bytes)?;
                rustix::fs::fsync(&file)?;
            }
            Err(SessionError::SessionAlreadyExists) => dir.replace(handle, bytes)?,
            Err(error) => return Err(error),
        }
    }
    sync_dir(&dir)
}

struct ResultReader {
    file: std::fs::File,
    size: usize,
}

impl ResultReader {
    fn open(session: &PrivateDir, handle: &str) -> Result<Option<Self>, SessionError> {
        use crate::session_log::managed_file::{Access, open_managed_file};
        if !is_valid_handle(handle) {
            return Err(SessionError::InvalidConversationEvent);
        }
        let Some(results) = session.open_child(TOOL_RESULTS_DIR)? else {
            return Ok(None);
        };
        let Some(file) = open_managed_file(&results, handle, Access::ReadOnly)? else {
            return Ok(None);
        };
        let size = usize::try_from(file.metadata()?.len())
            .map_err(|_| SessionError::InvalidSessionFormat)?;
        Ok(Some(Self { file, size }))
    }

    fn read_page(&self, offset: usize, max_bytes: usize) -> Result<Vec<u8>, SessionError> {
        use std::os::unix::fs::FileExt;
        if offset >= self.size || max_bytes == 0 {
            return Ok(Vec::new());
        }
        let mut bytes = vec![0; max_bytes.min(self.size - offset)];
        let mut read = 0;
        while read < bytes.len() {
            let position =
                u64::try_from(offset + read).map_err(|_| SessionError::InvalidSessionFormat)?;
            match self.file.read_at(&mut bytes[read..], position) {
                Ok(0) => break,
                Ok(count) => read += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        bytes.truncate(read);
        Ok(bytes)
    }
}

pub(crate) fn read_for_replay(
    session: &PrivateDir,
    handle: &str,
    expected_bytes: u64,
) -> Option<String> {
    let expected = usize::try_from(expected_bytes)
        .ok()
        .filter(|bytes| *bytes <= STORED_TEXT_MAX_BYTES)?;
    let reader = ResultReader::open(session, handle).ok()??;
    read_opened_for_replay(&reader, expected, expected_bytes)
}

fn read_opened_for_replay(
    reader: &ResultReader,
    expected: usize,
    expected_bytes: u64,
) -> Option<String> {
    if reader.size != expected {
        return None;
    }
    let bytes = reader.read_page(0, expected).ok()?;
    if bytes.len() != expected || reader.file.metadata().ok()?.len() != expected_bytes {
        return None;
    }
    String::from_utf8(bytes).ok()
}

pub(crate) fn preview(text: &str) -> &str {
    &text[..text.floor_char_boundary(PREVIEW_BYTES)]
}

pub(crate) fn bytes_preview(bytes: &[u8]) -> Option<&str> {
    let mut end = PREVIEW_BYTES.min(bytes.len());
    while end > 0 && end < bytes.len() && bytes[end] & 0b1100_0000 == 0b1000_0000 {
        end -= 1;
    }
    std::str::from_utf8(&bytes[..end]).ok()
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

pub(crate) fn diff_handle_matches_call(handle: &str, tool_call_id: &str) -> bool {
    let Some(digests) = handle
        .strip_prefix(DIFF_HANDLE_PREFIX)
        .and_then(|rest| rest.strip_suffix(DIFF_HANDLE_SUFFIX))
    else {
        return false;
    };
    let Some((call, content)) = digests.split_once('-') else {
        return false;
    };
    let mut expected = String::new();
    push_digest_hex(&mut expected, tool_call_id.as_bytes());
    [call, content]
        .iter()
        .all(|digest| digest.len() == DIFF_DIGEST_HEX_BYTES && is_lower_hex(digest))
        && call == expected
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
    fn new_results_are_stored_privately_and_an_existing_handle_is_replaced() {
        let (root, dir) = session();
        store_new_results(&dir, []).unwrap();
        assert!(!root.path().join("session/tool-results").exists());
        let first = make_handle("call_1", "shell", "one");
        let second = make_handle("call_2", "shell", "two");
        store_result(&dir, &second, "stale").unwrap();
        store_new_results(
            &dir,
            [
                (first.as_str(), &b"one"[..]),
                (second.as_str(), &b"two"[..]),
            ],
        )
        .unwrap();
        let results = root.path().join("session/tool-results");
        for (handle, text) in [(&first, "one"), (&second, "two")] {
            let path = results.join(handle);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(
            store_new_results(&dir, [("../escape", &b"x"[..])]),
            Err(SessionError::InvalidConversationEvent)
        );
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
    fn storing_results_above_replay_limit_preserves_all_bytes() {
        let (root, dir) = session();
        let text = format!(
            "FULL_READER_HEAD\n{}\nFULL_READER_TAIL\n",
            "x".repeat(STORED_TEXT_MAX_BYTES)
        );
        let handle = make_handle("full-reader", "read_file", &text);
        store_result(&dir, &handle, &text).unwrap();
        assert_eq!(
            std::fs::read(root.path().join("session/tool-results").join(&handle)).unwrap(),
            text.as_bytes()
        );
        assert_eq!(
            read_for_replay(&dir, &handle, u64::try_from(text.len()).unwrap()),
            None
        );
        let reader = ResultReader::open(&dir, &handle).unwrap().unwrap();
        assert_eq!(reader.size, text.len());
        assert!(
            reader
                .read_page(0, 64 * 1024)
                .unwrap()
                .starts_with(b"FULL_READER_HEAD\n")
        );
        assert_eq!(
            reader.read_page(reader.size / 2, 64 * 1024).unwrap(),
            vec![b'x'; 64 * 1024]
        );
        assert_eq!(
            reader
                .read_page(reader.size - "\nFULL_READER_TAIL\n".len(), 64 * 1024)
                .unwrap(),
            b"\nFULL_READER_TAIL\n"
        );
    }

    #[test]
    fn held_reader_returns_bounded_raw_pages_and_does_not_reopen_swapped_routes() {
        let (root, dir) = session();
        let text = "headé-middle-tail";
        let handle = make_handle("c", "t", text);
        store_result(&dir, &handle, text).unwrap();
        let reader = ResultReader::open(&dir, &handle).unwrap().unwrap();
        assert_eq!(reader.size, text.len());
        let results = root.path().join("session/tool-results");
        std::fs::rename(&results, root.path().join("retained")).unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join(&handle), "wrong").unwrap();
        symlink(&outside, &results).unwrap();
        assert_eq!(reader.read_page(0, 4).unwrap(), b"head");
        assert_eq!(reader.read_page(4, 1).unwrap(), [0xc3]);
        assert_eq!(
            reader.read_page(text.len() - 4, 64 * 1024).unwrap(),
            b"tail"
        );
        assert!(reader.read_page(text.len(), 20).unwrap().is_empty());
        assert!(reader.read_page(usize::MAX, usize::MAX).unwrap().is_empty());
        assert!(reader.read_page(0, 0).unwrap().is_empty());
    }

    #[test]
    fn reader_absence_is_read_only_and_replay_does_not_authenticate_content() {
        let (root, dir) = session();
        assert!(ResultReader::open(&dir, "missing.txt").unwrap().is_none());
        assert!(!root.path().join("session/tool-results").exists());
        let handle = make_handle("c", "t", "body");
        store_result(&dir, &handle, "body").unwrap();
        std::fs::write(
            root.path().join("session/tool-results").join(&handle),
            "same",
        )
        .unwrap();
        assert_eq!(read_for_replay(&dir, &handle, 4), Some("same".to_owned()));
    }

    #[test]
    fn replay_rejects_growth_after_the_reader_captures_size() {
        use std::io::Write;
        let (root, dir) = session();
        let handle = make_handle("c", "t", "body");
        store_result(&dir, &handle, "body").unwrap();
        let reader = ResultReader::open(&dir, &handle).unwrap().unwrap();
        let path = root.path().join("session/tool-results").join(&handle);
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"extra")
            .unwrap();
        assert_eq!(reader.read_page(0, 20).unwrap(), b"body");
        assert_eq!(read_opened_for_replay(&reader, 4, 4), None);
    }

    #[test]
    fn stored_previews_point_at_the_handle() {
        assert_eq!(
            format_stored_result_output("result-a-1-2.txt", "head", 99),
            "<tool_result_preview handle=\"result-a-1-2.txt\" stored_bytes=\"99\">\nhead\n</tool_result_preview>\n<tool_result_handle>result-a-1-2.txt</tool_result_handle>\nFull result is stored outside session JSON. Use read_tool_result with this handle to inspect a byte range or literal query."
        );
    }

    #[test]
    fn a_diff_pack_fits_exactly_when_its_encoding_does() {
        let text = "plain \"quoted\" back\\slash \u{1}\u{8}\u{c}\n\r\t\u{1f} caf\u{e9} \u{7f}";
        let bytes: Vec<u8> = (0..=255).collect();
        for (previous, unit) in [(text.as_bytes(), 1), (&bytes[..], 3)] {
            let (_, base) =
                diff_content_pack("call", Some(previous), Some(text.as_bytes())).unwrap();
            let room = (DIFF_CONTENT_MAX_BYTES - base.len()) / unit;
            let fits: Vec<bool> = [room, room + 1]
                .into_iter()
                .map(|extra| {
                    let mut longer = previous.to_vec();
                    longer.extend(std::iter::repeat_n(b'a', extra));
                    let (_, pack) =
                        diff_content_pack("call", Some(&longer), Some(text.as_bytes())).unwrap();
                    let fits = fits_diff_pack(Some(&longer), Some(text.as_bytes()));
                    assert_eq!(fits, pack.len() <= DIFF_CONTENT_MAX_BYTES);
                    fits
                })
                .collect();
            assert_eq!(fits, [true, false]);
        }
        let (_, pack) = diff_content_pack("call", None, Some(text.as_bytes())).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&pack).unwrap()["after_content"],
            text
        );
        assert!(fits_diff_pack(None, None));
    }

    #[test]
    fn a_diff_pack_escapes_text_as_json_does() {
        let every_control: String = (0_u8..0x20).map(char::from).collect();
        for text in [
            String::new(),
            "plain".to_owned(),
            every_control,
            "\"q\" \\ \u{7f} caf\u{e9} \u{2028} \u{1f600}".to_owned(),
            "\n".repeat(3),
        ] {
            let (_, pack) = diff_content_pack("call", Some(text.as_bytes()), None).unwrap();
            let expected = format!(
                "{{\"previous_content\":{},\"after_content\":null}}",
                serde_json::to_string(&text).unwrap()
            );
            assert_eq!(String::from_utf8(pack).unwrap(), expected);
        }
    }
}
