pub(crate) mod recovery_checkpoint;

use ofx_config::ProviderId;
use ofx_contract::{ReasoningEffort, StrictJsonError};
use ofx_text::lowercase_hex;
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

use crate::fixed_field::False;
use crate::json_fields::{Fields, Json, parse_json, string};
use crate::session_error::SessionError;
use crate::session_layout::is_valid_session_id;
use crate::session_store_paths::is_valid_workspace_root;

const SESSION_METADATA_SCHEMA_VERSION: u8 = 4;
pub(crate) const MAX_SESSION_METADATA_BYTES: usize = 64 * 1024;
const MAX_SESSION_TITLE_BYTES: usize = 240;
const MAX_MODEL_BYTES: usize = 1024;
const MAX_CONVERSATION_LANGUAGE_BYTES: usize = 24;
pub(crate) const DEFAULT_CONVERSATION_LANGUAGE: &str = "und";
const BINDING_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedProvider {
    id: ProviderId,
    binding: Option<[u8; BINDING_BYTES]>,
}

impl SavedProvider {
    pub fn new(id: ProviderId, binding: Option<[u8; BINDING_BYTES]>) -> Option<Self> {
        let bound = matches!(id, ProviderId::Configured(_));
        let reads_back = ProviderId::parse(id.label()).as_ref() == Some(&id);
        (bound == binding.is_some() && reads_back).then_some(Self { id, binding })
    }

    pub fn id(&self) -> &ProviderId {
        &self.id
    }

    pub fn binding(&self) -> Option<[u8; BINDING_BYTES]> {
        self.binding
    }
}

impl Serialize for SavedProvider {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let Some(binding) = self.binding else {
            return serializer.serialize_str(self.id.label());
        };
        let encoded = lowercase_hex(&binding);
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("name", self.id.label())?;
        map.serialize_entry("binding", &encoded)?;
        map.end()
    }
}

#[cfg(test)]
impl<'de> serde::Deserialize<'de> for SavedProvider {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        parse_saved_provider(&<Json<'de> as serde::Deserialize>::deserialize(
            deserializer,
        )?)
        .ok_or_else(|| serde::de::Error::custom("InvalidProviderBinding"))
    }
}

pub(crate) fn parse_saved_provider(value: &Json<'_>) -> Option<SavedProvider> {
    if let Some(name) = value.as_str() {
        let id = ProviderId::parse(name)?;
        return SavedProvider::new(id, None);
    }
    if !matches!(value, Json::Object(fields) if fields.len() == 2) {
        return None;
    }
    let id = ProviderId::parse(value.get("name")?.as_str()?)?;
    let encoded = value.get("binding")?.as_str()?;
    if encoded.len() != BINDING_BYTES * 2 || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut binding = [0_u8; BINDING_BYTES];
    for (index, byte) in binding.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&encoded[index * 2..index * 2 + 2], 16).ok()?;
    }
    SavedProvider::new(id, Some(binding))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPreferences {
    pub provider: SavedProvider,
    pub model: String,
    pub effort: ReasoningEffort,
    pub fast_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMetadata {
    pub id: String,
    pub origin_workspace_root: String,
    pub workspace_root: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub conversation_language: String,
    pub preferences: SessionPreferences,
    pub title: Option<String>,
}

#[derive(Serialize)]
struct MetadataWire<'a> {
    schema_version: u8,
    id: &'a str,
    origin_workspace_root: &'a str,
    workspace_root: &'a str,
    created_at_ms: i64,
    updated_at_ms: i64,
    conversation_language: &'a str,
    provider: &'a SavedProvider,
    model: &'a str,
    effort: &'a str,
    fast_mode: bool,
    title: Option<&'a str>,
    subagent_child: False,
}

pub(crate) fn encode_session_metadata(metadata: &SessionMetadata) -> Result<Vec<u8>, SessionError> {
    validate_session_metadata(metadata)?;
    let effort = effort_label(&metadata.preferences.effort);
    let bytes = serde_json::to_vec(&MetadataWire {
        schema_version: SESSION_METADATA_SCHEMA_VERSION,
        id: &metadata.id,
        origin_workspace_root: &metadata.origin_workspace_root,
        workspace_root: &metadata.workspace_root,
        created_at_ms: metadata.created_at_ms,
        updated_at_ms: metadata.updated_at_ms,
        conversation_language: &metadata.conversation_language,
        provider: &metadata.preferences.provider,
        model: &metadata.preferences.model,
        effort,
        fast_mode: metadata.preferences.fast_mode,
        title: metadata.title.as_deref(),
        subagent_child: False,
    })
    .map_err(|_| SessionError::InvalidSessionMetadata)?;
    if bytes.is_empty() || bytes.len() > MAX_SESSION_METADATA_BYTES {
        return Err(SessionError::SessionMetadataTooLarge);
    }
    Ok(bytes)
}

pub(crate) fn decode_session_metadata(bytes: &[u8]) -> Result<SessionMetadata, SessionError> {
    if bytes.is_empty() || bytes.len() > MAX_SESSION_METADATA_BYTES {
        return Err(SessionError::SessionMetadataTooLarge);
    }
    let document = parse_json(bytes).map_err(|error| match error {
        StrictJsonError::Syntax => SessionError::InvalidSessionFormat,
        StrictJsonError::DuplicateField => SessionError::InvalidSessionMetadata,
    })?;
    let undecodable = undecodable_metadata(&document);
    let metadata = metadata_from(document).ok_or(undecodable)?;
    validate_session_metadata(&metadata).map_err(|_| SessionError::InvalidSessionMetadata)?;
    Ok(metadata)
}

fn metadata_from(document: Json<'_>) -> Option<SessionMetadata> {
    let mut fields = Fields::new(document)?;
    fields
        .unsigned("schema_version")
        .filter(|version| *version == u64::from(SESSION_METADATA_SCHEMA_VERSION))?;
    let metadata = SessionMetadata {
        id: fields.string("id")?,
        origin_workspace_root: fields.string("origin_workspace_root")?,
        workspace_root: fields.string("workspace_root")?,
        created_at_ms: fields.signed("created_at_ms")?,
        updated_at_ms: fields.signed("updated_at_ms")?,
        conversation_language: fields.string("conversation_language")?,
        preferences: SessionPreferences {
            provider: parse_saved_provider(&fields.required("provider")?)?,
            model: fields.string("model")?,
            effort: ReasoningEffort::parse(&fields.string("effort")?)?,
            fast_mode: fields.flag("fast_mode")?,
        },
        title: fields.nullable("title", |value| string(value).map(Some))?,
    };
    fields.fixed::<False>("subagent_child")?;
    fields.finish(metadata)
}

fn undecodable_metadata(document: &Json<'_>) -> SessionError {
    match document.get("schema_version") {
        None | Some(Json::Null) => SessionError::InvalidSessionFormat,
        Some(version) if version.as_u64() == Some(u64::from(SESSION_METADATA_SCHEMA_VERSION)) => {
            SessionError::InvalidSessionMetadata
        }
        Some(_) => SessionError::UnsupportedSessionSchema,
    }
}

fn validate_session_metadata(metadata: &SessionMetadata) -> Result<(), SessionError> {
    if !is_valid_session_id(&metadata.id)
        || !is_valid_workspace_root(&metadata.origin_workspace_root)
        || !is_valid_workspace_root(&metadata.workspace_root)
        || !is_valid_conversation_language(&metadata.conversation_language)
        || !is_valid_model(&metadata.preferences.model)
    {
        return Err(SessionError::InvalidDurableField);
    }
    if metadata.created_at_ms < 0
        || metadata.updated_at_ms < metadata.created_at_ms
        || ReasoningEffort::parse(effort_label(&metadata.preferences.effort)).is_none()
    {
        return Err(SessionError::InvalidSessionMetadata);
    }
    if metadata
        .title
        .as_ref()
        .is_some_and(|title| title.is_empty() || title.len() > MAX_SESSION_TITLE_BYTES)
    {
        return Err(SessionError::InvalidSessionMetadata);
    }
    Ok(())
}

fn effort_label(effort: &ReasoningEffort) -> &str {
    match effort {
        ReasoningEffort::Auto => "auto",
        ReasoningEffort::Named(name) => name,
    }
}

fn is_valid_model(model: &str) -> bool {
    (1..=MAX_MODEL_BYTES).contains(&model.len())
        && !model.starts_with(is_ascii_space)
        && !model.ends_with(is_ascii_space)
        && !model.bytes().any(|byte| byte.is_ascii_control())
}

fn is_valid_conversation_language(language: &str) -> bool {
    (1..=MAX_CONVERSATION_LANGUAGE_BYTES).contains(&language.len())
        && language.trim_matches(['\t', '\n', '\r', ' ']) == language
        && !language.bytes().any(|byte| byte.is_ascii_control())
}

fn is_ascii_space(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

#[cfg(test)]
mod tests;
