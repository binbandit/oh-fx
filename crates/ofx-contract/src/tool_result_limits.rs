use ofx_text::sanitize_model_text_owned;

pub const DEFAULT_MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;

pub fn prepare_model_output(tool_name: &str, raw: String, max_bytes: usize) -> String {
    bound_model_output(tool_name, raw, max_bytes).0
}

pub fn bound_model_output(tool_name: &str, raw: String, max_bytes: usize) -> (String, bool) {
    let sanitized = sanitize_model_text_owned(raw.into_bytes());
    if sanitized.len() <= max_bytes {
        return (sanitized, false);
    }
    let marker = format!(
        "\n... [tool result truncated for {tool_name}: original {} bytes; cap is {max_bytes} bytes]\n",
        sanitized.len()
    );
    let prefix_len = sanitized.floor_char_boundary(max_bytes.saturating_sub(marker.len()));
    if prefix_len == 0 {
        return (marker, true);
    }
    (format!("{}{marker}", &sanitized[..prefix_len]), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_model_output_preserves_secret_shaped_assignments_verbatim() {
        let raw = "token=abcdefghijklmnopqrstuvwxyz";
        assert_eq!(
            prepare_model_output(
                "mcp__server__tool",
                raw.to_owned(),
                DEFAULT_MAX_TOOL_RESULT_BYTES
            ),
            raw
        );
    }

    #[test]
    fn prepare_model_output_caps_chatty_output_with_explicit_marker() {
        let output = prepare_model_output("grep_files", "x".repeat(256), 128);
        assert!(output.len() <= 128);
        assert!(output.contains(
            "... [tool result truncated for grep_files: original 256 bytes; cap is 128 bytes]"
        ));
    }

    #[test]
    fn prepare_model_output_keeps_complete_codepoints_at_the_cap() {
        let text = format!("x{}", "\u{e9}".repeat(300));
        for cap in [128, 129] {
            let output = prepare_model_output("grep_files", text.clone(), cap);
            assert!(output.len() <= cap);
            let marker_start = output.find("\n... [tool result truncated").unwrap();
            assert!(output[..marker_start].ends_with('\u{e9}'));
        }
    }

    #[test]
    fn prepare_model_output_keeps_only_the_marker_when_it_fills_the_cap() {
        let output = prepare_model_output("read_file", "x".repeat(200), 16);
        assert_eq!(
            output,
            "\n... [tool result truncated for read_file: original 200 bytes; cap is 16 bytes]\n"
        );
    }

    #[test]
    fn prepare_model_output_omits_text_with_nul_bytes() {
        assert_eq!(
            prepare_model_output(
                "read_file",
                "a\0b".to_owned(),
                DEFAULT_MAX_TOOL_RESULT_BYTES
            ),
            "binary or non-utf8 tool output omitted (3 bytes)"
        );
    }
}
