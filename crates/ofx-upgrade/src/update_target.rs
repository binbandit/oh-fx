const MIN_REVISION_BYTES: usize = 7;
const MAX_REVISION_BYTES: usize = 64;

pub fn is_valid_revision(raw: &str) -> bool {
    (MIN_REVISION_BYTES..=MAX_REVISION_BYTES).contains(&raw.len())
        && raw.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_are_bounded_hex_strings() {
        assert!(is_valid_revision("abcdef0"));
        assert!(is_valid_revision("ABCDEF0"));
        assert!(is_valid_revision(&"a".repeat(MAX_REVISION_BYTES)));
        assert!(!is_valid_revision("abcdef"));
        assert!(!is_valid_revision(&"a".repeat(MAX_REVISION_BYTES + 1)));
        assert!(!is_valid_revision("ggggggg"));
        assert!(!is_valid_revision("not-a-revision"));
    }
}
