pub(crate) fn trim<'a>(bytes: &'a [u8], set: &[u8]) -> &'a [u8] {
    trim_end(trim_start(bytes, set), set)
}

pub(crate) fn trim_start<'a>(bytes: &'a [u8], set: &[u8]) -> &'a [u8] {
    let start = bytes
        .iter()
        .position(|byte| !set.contains(byte))
        .unwrap_or(bytes.len());
    &bytes[start..]
}

pub(crate) fn trim_end<'a>(bytes: &'a [u8], set: &[u8]) -> &'a [u8] {
    let end = bytes
        .iter()
        .rposition(|byte| !set.contains(byte))
        .map_or(0, |index| index + 1);
    &bytes[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_trims_remove_only_the_given_set_from_each_end() {
        assert_eq!(trim(b" \tname: x \t", b" \t"), b"name: x");
        assert_eq!(trim(b" \t ", b" \t"), b"");
        assert_eq!(trim_start(b"\r\n\r\nbody\n", b"\r\n"), b"body\n");
        assert_eq!(trim_end(b"use the,: ", b" ,:"), b"use the");
        assert_eq!(trim_end(b"", b" "), b"");
    }
}
