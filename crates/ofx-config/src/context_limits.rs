use ofx_text::parse_unsigned;

const TRIMMED: &[u8] = b" \t\r\n";

const NAMES: [&str; 11] = [
    "skill_description_bytes",
    "skill_catalog_bytes",
    "skill_chunk_bytes",
    "skill_file_bytes",
    "mcp_description_bytes",
    "mcp_search_result_bytes",
    "mcp_server_instructions_bytes",
    "mcp_selected_schema_bytes",
    "project_instruction_file_bytes",
    "project_instructions_total_bytes",
    "image_adapter_output_bytes",
];

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub enum ContextLimitError {
    #[error("InvalidContextLimitOverride")]
    InvalidContextLimitOverride,
    #[error("UnknownContextLimit")]
    UnknownContextLimit,
    #[error("InvalidContextLimitValue")]
    InvalidContextLimitValue,
}

pub fn validate_context_limit_override(raw: &[u8]) -> Result<(), ContextLimitError> {
    let separator = raw
        .iter()
        .position(|byte| *byte == b'=')
        .ok_or(ContextLimitError::InvalidContextLimitOverride)?;
    let (name, value) = (trim(&raw[..separator]), trim(&raw[separator + 1..]));
    if name.is_empty() || value.is_empty() {
        return Err(ContextLimitError::InvalidContextLimitOverride);
    }
    if !NAMES.iter().any(|known| known.as_bytes() == name) {
        return Err(ContextLimitError::UnknownContextLimit);
    }
    if is_valid_value(value) {
        Ok(())
    } else {
        Err(ContextLimitError::InvalidContextLimitValue)
    }
}

fn trim(raw: &[u8]) -> &[u8] {
    let start = raw
        .iter()
        .position(|byte| !TRIMMED.contains(byte))
        .unwrap_or(raw.len());
    let end = raw
        .iter()
        .rposition(|byte| !TRIMMED.contains(byte))
        .map_or(start, |index| index + 1);
    &raw[start..end]
}

fn is_valid_value(raw: &[u8]) -> bool {
    raw.eq_ignore_ascii_case(b"off")
        || std::str::from_utf8(raw)
            .ok()
            .and_then(parse_unsigned::<u64>)
            .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_limit_overrides_accept_bytes_and_off() {
        for raw in [
            &b"skill_chunk_bytes=4096"[..],
            b"mcp_description_bytes=off",
            b" image_adapter_output_bytes \t= OFF\r\n",
            b"project_instructions_total_bytes=1_000",
        ] {
            assert_eq!(validate_context_limit_override(raw), Ok(()), "{raw:?}");
        }
    }

    #[test]
    fn context_limit_overrides_reject_unknown_names_and_malformed_values() {
        for (raw, expected) in [
            (&b"wat=1"[..], ContextLimitError::UnknownContextLimit),
            (
                b"skill_chunk_bytes=-1",
                ContextLimitError::InvalidContextLimitValue,
            ),
            (
                b"skill_chunk_bytes=1\xff",
                ContextLimitError::InvalidContextLimitValue,
            ),
            (
                b"skill_chunk_bytes",
                ContextLimitError::InvalidContextLimitOverride,
            ),
            (b" =1", ContextLimitError::InvalidContextLimitOverride),
            (
                b"skill_chunk_bytes= ",
                ContextLimitError::InvalidContextLimitOverride,
            ),
            (b"\xff=1", ContextLimitError::UnknownContextLimit),
        ] {
            assert_eq!(
                validate_context_limit_override(raw),
                Err(expected),
                "{raw:?}"
            );
        }
    }
}
