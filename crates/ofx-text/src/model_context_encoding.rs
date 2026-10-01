use std::fmt::Write;

pub fn write_scalar(output: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\u{85}' => output.push_str("&#x85;"),
            '\u{2028}' => output.push_str("&#x2028;"),
            '\u{2029}' => output.push_str("&#x2029;"),
            control if u32::from(control) < 0x20 || control == '\u{7f}' => {
                let _ = write!(output, "&#x{:02x};", u32::from(control));
            }
            other => output.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar(value: &str) -> String {
        let mut output = String::new();
        write_scalar(&mut output, value);
        output
    }

    #[test]
    fn write_scalar_preserves_safe_ascii_unicode_and_empty_input() {
        let safe = "safe value /._-' cafe\u{301} 日本語";
        assert_eq!(scalar(safe), safe);
        assert_eq!(scalar(""), "");
    }

    #[test]
    fn write_scalar_encodes_delimiters_and_ascii_controls() {
        assert_eq!(
            scalar("&<>\"\u{0}\u{1}\t\n\r\u{1f}\u{7f}"),
            "&amp;&lt;&gt;&quot;&#x00;&#x01;&#x09;&#x0a;&#x0d;&#x1f;&#x7f;"
        );
    }

    #[test]
    fn write_scalar_encodes_every_c0_control_and_del() {
        let controls: String = (0_u8..0x20).chain([0x7f]).map(char::from).collect();
        let mut expected = String::new();
        for byte in (0_u8..0x20).chain([0x7f]) {
            let _ = write!(expected, "&#x{byte:02x};");
        }
        assert_eq!(scalar(&controls), expected);
    }

    #[test]
    fn write_scalar_encodes_unicode_line_separators() {
        assert_eq!(
            scalar("before\u{0085}middle\u{2028}next\u{2029}after"),
            "before&#x85;middle&#x2028;next&#x2029;after"
        );
    }
}
