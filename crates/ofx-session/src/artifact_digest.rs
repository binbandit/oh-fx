const DIGEST_HEX_BYTES: usize = 16;
const BLOB_HEX_BYTES: usize = 64;

pub(crate) fn has_content_digest(handle: &str, suffix: &str) -> bool {
    encoded_digest(handle, suffix).is_some()
}

fn encoded_digest<'a>(handle: &'a str, suffix: &str) -> Option<&'a str> {
    let stem = handle.strip_suffix(suffix).filter(|_| !suffix.is_empty())?;
    let (_, encoded) = stem.rsplit_once('-')?;
    ((encoded.len() == DIGEST_HEX_BYTES || encoded.len() == BLOB_HEX_BYTES)
        && is_lower_hex(encoded))
    .then_some(encoded)
}

pub(crate) fn is_lower_hex(text: &str) -> bool {
    text.bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_carries_a_digest_of_sixteen_or_sixty_four_lowercase_hex_digits() {
        let short = "fx-command-replay-0011223344556677.bin";
        let long = format!("fx-command-replay-{}.bin", "ab".repeat(32));
        assert!(has_content_digest(short, ".bin"));
        assert!(has_content_digest(&long, ".bin"));
        for handle in [
            "fx-command-replay-0011223344556677.log",
            "fx-command-replay-00112233445566.bin",
            "fx-command-replay-00112233445566AA.bin",
            "nodigest.bin",
            "fx-command-0011223344556677",
        ] {
            assert!(!has_content_digest(handle, ".bin"), "{handle}");
        }
        assert!(!has_content_digest(short, ""));
    }
}
