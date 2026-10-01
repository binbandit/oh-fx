const MAX_VERSION_BYTES: usize = 32;
const VERSION_PARTS: usize = 3;
const MIN_REVISION_BYTES: usize = 7;
const MAX_REVISION_BYTES: usize = 64;

pub fn normalize_version(raw: &str) -> &str {
    raw.strip_prefix('v').unwrap_or(raw)
}

pub fn is_valid_version(raw: &str) -> bool {
    if raw.is_empty() || raw.len() > MAX_VERSION_BYTES {
        return false;
    }
    let mut parts = 0;
    for part in raw.split('.') {
        if parts == VERSION_PARTS
            || part.is_empty()
            || !part.bytes().all(|byte| byte.is_ascii_digit())
            || part.parse::<u32>().is_err()
        {
            return false;
        }
        parts += 1;
    }
    parts == VERSION_PARTS
}

pub fn is_valid_revision(raw: &str) -> bool {
    (MIN_REVISION_BYTES..=MAX_REVISION_BYTES).contains(&raw.len())
        && raw.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_three_bounded_numeric_parts() {
        assert_eq!(normalize_version("v0.153.1"), "0.153.1");
        assert_eq!(normalize_version("0.153.1"), "0.153.1");
        for valid in [
            "0.153.1",
            "4294967295.0.0",
            "1111111111.1111111111.1111111111",
        ] {
            assert!(is_valid_version(valid), "{valid}");
        }
        for invalid in [
            "",
            "latest",
            "1.2",
            "1.2.3.4",
            "1.2.3?x=y",
            "1.2.3\r\nHeader: value",
            "4294967296.1.2",
            "1..2",
            "+1.2.3",
            "1.2.3 ",
            "01111111111.1111111111.1111111111",
        ] {
            assert!(!is_valid_version(invalid), "{invalid:?}");
        }
    }

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
