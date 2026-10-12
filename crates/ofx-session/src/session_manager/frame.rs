use super::crc32c::crc32c;

pub(crate) const MAX_LINE_BYTES: usize = 4 << 20;
const CRC_FIELD: &[u8] = b",\"crc\":\"";
const CRC_CLOSE: &[u8] = b"\"}\n";
const CRC_SUFFIX_LEN: usize = CRC_FIELD.len() + 8 + CRC_CLOSE.len();

pub(crate) fn check_frame(line: &[u8]) -> Option<&[u8]> {
    if line.len() < CRC_SUFFIX_LEN || line.len() > MAX_LINE_BYTES {
        return None;
    }
    let (payload, suffix) = line.split_at(line.len() - CRC_SUFFIX_LEN);
    let digits = suffix.strip_prefix(CRC_FIELD)?.strip_suffix(CRC_CLOSE)?;
    (crc32c(payload) == parse_hex32(digits)?).then_some(payload)
}

fn parse_hex32(digits: &[u8]) -> Option<u32> {
    digits.iter().try_fold(0_u32, |value, &digit| {
        let nibble = match digit {
            b'0'..=b'9' => digit - b'0',
            b'a'..=b'f' => digit - b'a' + 10,
            _ => return None,
        };
        Some((value << 4) | u32::from(nibble))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_manager::test_support::framed;

    #[test]
    fn a_framed_line_checks_and_its_payload_comes_back() {
        let line = framed("{\"op\":\"del\",\"id\":\"abc\",\"ts\":11");
        assert_eq!(
            line,
            b"{\"op\":\"del\",\"id\":\"abc\",\"ts\":11,\"crc\":\"3f06ef02\"}\n"
        );
        assert_eq!(
            check_frame(&line),
            Some(&b"{\"op\":\"del\",\"id\":\"abc\",\"ts\":11"[..])
        );
    }

    #[test]
    fn flipping_any_single_bit_of_a_line_is_detected() {
        let line = framed("{\"v\":1,\"seq\":2,\"ts\":3,\"kind\":\"closed\"");
        for index in 0..line.len() {
            for bit in 0..8 {
                let mut flipped = line.clone();
                flipped[index] ^= 1 << bit;
                assert_eq!(check_frame(&flipped), None, "byte {index} bit {bit}");
            }
        }
    }

    #[test]
    fn a_frame_refuses_uppercase_hex_short_lines_and_lines_over_the_cap() {
        let line = framed("{\"op\":\"del\",\"id\":\"abc\",\"ts\":11");
        let upper = String::from_utf8(line.clone())
            .unwrap()
            .replace("3f06ef02", "3F06EF02");
        assert_eq!(check_frame(upper.as_bytes()), None);
        assert_eq!(check_frame(&line[line.len() - 19..]), None);
        assert_eq!(check_frame(&line[..line.len() - 1]), None);
        let long = framed(&format!("{{\"pad\":\"{}\"", "x".repeat(MAX_LINE_BYTES)));
        assert_eq!(check_frame(&long), None);
    }
}
