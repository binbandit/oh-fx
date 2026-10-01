use ofx_text::parse_unsigned;
use serde_json::Value;

pub const EMERGENCY_CEILING_BYTES: usize = 64 * 1024 * 1024;

const TRIMMED: &[u8] = b" \t\r\n";
const NAME_COUNT: usize = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextLimitName {
    SkillDescriptionBytes,
    SkillCatalogBytes,
    SkillChunkBytes,
    SkillFileBytes,
    McpDescriptionBytes,
    McpSearchResultBytes,
    McpServerInstructionsBytes,
    McpSelectedSchemaBytes,
    ProjectInstructionFileBytes,
    ProjectInstructionsTotalBytes,
    ImageAdapterOutputBytes,
}

const NAMES: [(ContextLimitName, &str, usize); NAME_COUNT] = [
    (
        ContextLimitName::SkillDescriptionBytes,
        "skill_description_bytes",
        1024,
    ),
    (
        ContextLimitName::SkillCatalogBytes,
        "skill_catalog_bytes",
        16 * 1024,
    ),
    (
        ContextLimitName::SkillChunkBytes,
        "skill_chunk_bytes",
        20 * 1024,
    ),
    (
        ContextLimitName::SkillFileBytes,
        "skill_file_bytes",
        1024 * 1024,
    ),
    (
        ContextLimitName::McpDescriptionBytes,
        "mcp_description_bytes",
        1024,
    ),
    (
        ContextLimitName::McpSearchResultBytes,
        "mcp_search_result_bytes",
        16 * 1024,
    ),
    (
        ContextLimitName::McpServerInstructionsBytes,
        "mcp_server_instructions_bytes",
        2 * 1024,
    ),
    (
        ContextLimitName::McpSelectedSchemaBytes,
        "mcp_selected_schema_bytes",
        64 * 1024,
    ),
    (
        ContextLimitName::ProjectInstructionFileBytes,
        "project_instruction_file_bytes",
        64 * 1024,
    ),
    (
        ContextLimitName::ProjectInstructionsTotalBytes,
        "project_instructions_total_bytes",
        128 * 1024,
    ),
    (
        ContextLimitName::ImageAdapterOutputBytes,
        "image_adapter_output_bytes",
        20 * 1024,
    ),
];

impl ContextLimitName {
    fn parse(raw: &[u8]) -> Option<Self> {
        NAMES
            .iter()
            .find(|(_, name, _)| name.as_bytes() == raw)
            .map(|(limit, _, _)| *limit)
    }

    fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextLimitSource {
    CompiledDefault,
    GlobalSettings,
    WorkspaceSettings,
    CommandLine,
}

impl ContextLimitSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::CompiledDefault => "compiled default",
            Self::GlobalSettings => "global settings",
            Self::WorkspaceSettings => "workspace settings",
            Self::CommandLine => "command line",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextLimitValue {
    Bytes(usize),
    Off,
}

impl ContextLimitValue {
    fn parse_text(raw: &[u8]) -> Result<Self, ContextLimitError> {
        if raw.eq_ignore_ascii_case(b"off") {
            return Ok(Self::Off);
        }
        std::str::from_utf8(raw)
            .ok()
            .and_then(parse_unsigned::<usize>)
            .map(Self::Bytes)
            .ok_or(ContextLimitError::InvalidContextLimitValue)
    }

    fn parse_json(value: &Value) -> Result<Self, ContextLimitError> {
        match value {
            Value::Number(number) => number
                .as_i64()
                .and_then(|integer| usize::try_from(integer).ok())
                .map(Self::Bytes)
                .ok_or(ContextLimitError::InvalidContextLimitValue),
            Value::String(text) => Self::parse_text(text.as_bytes()),
            _ => Err(ContextLimitError::InvalidContextLimitValue),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextLimit {
    pub value: ContextLimitValue,
    pub source: ContextLimitSource,
}

impl ContextLimit {
    pub fn effective_bytes(self) -> usize {
        match self.value {
            ContextLimitValue::Bytes(bytes) => bytes.min(EMERGENCY_CEILING_BYTES),
            ContextLimitValue::Off => EMERGENCY_CEILING_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextLimits {
    values: [ContextLimit; NAME_COUNT],
}

impl Default for ContextLimits {
    fn default() -> Self {
        Self {
            values: NAMES.map(|(_, _, bytes)| ContextLimit {
                value: ContextLimitValue::Bytes(bytes),
                source: ContextLimitSource::CompiledDefault,
            }),
        }
    }
}

impl ContextLimits {
    pub fn get(&self, name: ContextLimitName) -> ContextLimit {
        self.values[name.index()]
    }

    pub fn apply_command_line(&mut self, overrides: &[ContextLimitOverride]) {
        for limit in overrides {
            self.values[limit.name.index()] = ContextLimit {
                value: limit.value,
                source: ContextLimitSource::CommandLine,
            };
        }
    }

    pub(crate) fn apply(&mut self, overrides: &ContextLimitOverrides, source: ContextLimitSource) {
        for (slot, value) in self.values.iter_mut().zip(overrides.values) {
            if let Some(value) = value {
                *slot = ContextLimit { value, source };
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ContextLimitOverrides {
    values: [Option<ContextLimitValue>; NAME_COUNT],
}

impl ContextLimitOverrides {
    pub(crate) fn parse_json(value: &Value) -> Result<Self, ContextLimitError> {
        let Value::Object(entries) = value else {
            return Err(ContextLimitError::InvalidContextLimitsType);
        };
        let mut overrides = Self::default();
        for (name, value) in entries {
            let name = ContextLimitName::parse(name.as_bytes())
                .ok_or(ContextLimitError::UnknownContextLimit)?;
            overrides.values[name.index()] = Some(ContextLimitValue::parse_json(value)?);
        }
        Ok(overrides)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextLimitOverride {
    pub name: ContextLimitName,
    pub value: ContextLimitValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ContextLimitError {
    #[error("InvalidContextLimitOverride")]
    InvalidContextLimitOverride,
    #[error("InvalidContextLimitsType")]
    InvalidContextLimitsType,
    #[error("UnknownContextLimit")]
    UnknownContextLimit,
    #[error("InvalidContextLimitValue")]
    InvalidContextLimitValue,
}

pub fn parse_context_limit_override(raw: &[u8]) -> Result<ContextLimitOverride, ContextLimitError> {
    let separator = raw
        .iter()
        .position(|byte| *byte == b'=')
        .ok_or(ContextLimitError::InvalidContextLimitOverride)?;
    let (name, value) = (trim(&raw[..separator]), trim(&raw[separator + 1..]));
    if name.is_empty() || value.is_empty() {
        return Err(ContextLimitError::InvalidContextLimitOverride);
    }
    Ok(ContextLimitOverride {
        name: ContextLimitName::parse(name).ok_or(ContextLimitError::UnknownContextLimit)?,
        value: ContextLimitValue::parse_text(value)?,
    })
}

pub fn line_safe_prefix_length(bytes: &[u8], max_bytes: usize) -> usize {
    let end = utf8_prefix_length(bytes, max_bytes);
    if end == bytes.len() {
        return end;
    }
    bytes[..end]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(end, |newline| newline + 1)
}

pub fn utf8_prefix_length(bytes: &[u8], max_bytes: usize) -> usize {
    let candidate = &bytes[..max_bytes.min(bytes.len())];
    std::str::from_utf8(candidate).map_or_else(|error| error.valid_up_to(), str::len)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_public_context_limit_contract() {
        let limits = ContextLimits::default();
        for (name, _, bytes) in NAMES {
            assert_eq!(limits.get(name).effective_bytes(), bytes);
            assert_eq!(limits.get(name).source, ContextLimitSource::CompiledDefault);
        }
        assert_eq!(
            limits
                .get(ContextLimitName::ProjectInstructionFileBytes)
                .effective_bytes(),
            64 * 1024
        );
        assert_eq!(
            limits
                .get(ContextLimitName::ProjectInstructionsTotalBytes)
                .effective_bytes(),
            128 * 1024
        );
    }

    #[test]
    fn context_limit_overrides_accept_bytes_and_off() {
        for (raw, name, value) in [
            (
                &b"skill_chunk_bytes=4096"[..],
                ContextLimitName::SkillChunkBytes,
                ContextLimitValue::Bytes(4096),
            ),
            (
                b"mcp_description_bytes=off",
                ContextLimitName::McpDescriptionBytes,
                ContextLimitValue::Off,
            ),
            (
                b" image_adapter_output_bytes \t= OFF\r\n",
                ContextLimitName::ImageAdapterOutputBytes,
                ContextLimitValue::Off,
            ),
            (
                b"project_instructions_total_bytes=1_000",
                ContextLimitName::ProjectInstructionsTotalBytes,
                ContextLimitValue::Bytes(1000),
            ),
        ] {
            assert_eq!(
                parse_context_limit_override(raw),
                Ok(ContextLimitOverride { name, value }),
                "{raw:?}"
            );
        }
        let off = ContextLimit {
            value: ContextLimitValue::Off,
            source: ContextLimitSource::CommandLine,
        };
        assert_eq!(off.effective_bytes(), EMERGENCY_CEILING_BYTES);
        let huge = ContextLimit {
            value: ContextLimitValue::Bytes(usize::MAX),
            source: ContextLimitSource::CommandLine,
        };
        assert_eq!(huge.effective_bytes(), EMERGENCY_CEILING_BYTES);
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
            assert_eq!(parse_context_limit_override(raw), Err(expected), "{raw:?}");
        }
    }

    #[test]
    fn settings_objects_parse_integers_strings_and_off_by_name() {
        let parsed = ContextLimitOverrides::parse_json(&serde_json::json!({
            "skill_chunk_bytes": 111,
            "mcp_description_bytes": "off",
            "project_instruction_file_bytes": "1_024",
        }))
        .unwrap();
        let mut limits = ContextLimits::default();
        limits.apply(&parsed, ContextLimitSource::GlobalSettings);
        assert_eq!(
            limits.get(ContextLimitName::SkillChunkBytes),
            ContextLimit {
                value: ContextLimitValue::Bytes(111),
                source: ContextLimitSource::GlobalSettings,
            }
        );
        assert_eq!(
            limits
                .get(ContextLimitName::McpDescriptionBytes)
                .effective_bytes(),
            EMERGENCY_CEILING_BYTES
        );
        assert_eq!(
            limits
                .get(ContextLimitName::ProjectInstructionFileBytes)
                .effective_bytes(),
            1024
        );
        assert_eq!(
            limits.get(ContextLimitName::SkillCatalogBytes).source,
            ContextLimitSource::CompiledDefault
        );
    }

    #[test]
    fn settings_objects_reject_unknown_names_wrong_types_and_bad_values() {
        for (value, expected) in [
            (
                serde_json::json!([]),
                ContextLimitError::InvalidContextLimitsType,
            ),
            (
                serde_json::json!(null),
                ContextLimitError::InvalidContextLimitsType,
            ),
            (
                serde_json::json!({"unknown_limit": 10}),
                ContextLimitError::UnknownContextLimit,
            ),
            (
                serde_json::json!({"skill_chunk_bytes": -1}),
                ContextLimitError::InvalidContextLimitValue,
            ),
            (
                serde_json::json!({"skill_chunk_bytes": 1.5}),
                ContextLimitError::InvalidContextLimitValue,
            ),
            (
                serde_json::json!({"skill_chunk_bytes": u64::MAX}),
                ContextLimitError::InvalidContextLimitValue,
            ),
            (
                serde_json::json!({"skill_chunk_bytes": " 5"}),
                ContextLimitError::InvalidContextLimitValue,
            ),
            (
                serde_json::json!({"skill_chunk_bytes": true}),
                ContextLimitError::InvalidContextLimitValue,
            ),
        ] {
            assert_eq!(
                ContextLimitOverrides::parse_json(&value),
                Err(expected),
                "{value}"
            );
        }
    }

    #[test]
    fn command_line_overrides_win_in_order_over_settings() {
        let mut limits = ContextLimits::default();
        limits.apply(
            &ContextLimitOverrides::parse_json(&serde_json::json!({"skill_chunk_bytes": 111}))
                .unwrap(),
            ContextLimitSource::GlobalSettings,
        );
        let overrides = [
            parse_context_limit_override(b"skill_chunk_bytes=333").unwrap(),
            parse_context_limit_override(b"project_instruction_file_bytes=off").unwrap(),
            parse_context_limit_override(b"project_instruction_file_bytes=12").unwrap(),
        ];
        limits.apply_command_line(&overrides);
        assert_eq!(
            limits.get(ContextLimitName::SkillChunkBytes),
            ContextLimit {
                value: ContextLimitValue::Bytes(333),
                source: ContextLimitSource::CommandLine,
            }
        );
        assert_eq!(
            limits
                .get(ContextLimitName::ProjectInstructionFileBytes)
                .effective_bytes(),
            12
        );
        assert_eq!(
            limits.get(ContextLimitName::McpDescriptionBytes).source,
            ContextLimitSource::CompiledDefault
        );
    }

    #[test]
    fn line_safe_prefix_preserves_utf8_and_complete_lines_when_possible() {
        assert_eq!(line_safe_prefix_length(b"one\ntwo\n", 7), 4);
        assert_eq!(line_safe_prefix_length("éclair".as_bytes(), 1), 0);
        assert_eq!(line_safe_prefix_length("éclair".as_bytes(), 2), 2);
        assert_eq!(line_safe_prefix_length(b"abc\xe4", 4), 3);
        assert_eq!(line_safe_prefix_length(b"short", 64), 5);
    }
}
