use ofx_config::ProviderId;
use ofx_contract::ReasoningEffort;

use crate::json_fields::{Json, parse_json, string};
use crate::session_codec::{
    DEFAULT_CONVERSATION_LANGUAGE, SavedProvider, SessionPreferences,
    is_valid_conversation_language, is_valid_model, parse_saved_provider,
};
use crate::session_manager::full_string;
use crate::session_usage::UsageSnapshot;

const INSTRUCTIONS_KEY: &str = "instructions";
const PREFERENCE_KEYS: [&str; 5] = ["provider", "model", "effort", "fast_mode", "ultrafast_mode"];

pub(super) enum RecordError {
    UnexpectedToken,
    InvalidDurableField,
    InvalidUsageCheckpoint,
}

impl RecordError {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::UnexpectedToken => "UnexpectedToken",
            Self::InvalidDurableField => "InvalidDurableField",
            Self::InvalidUsageCheckpoint => "InvalidUsageCheckpoint",
        }
    }
}

pub(super) fn decode_title(raw: Option<&[u8]>) -> Result<Option<String>, RecordError> {
    raw.map(|raw| full_string(raw).ok_or(RecordError::UnexpectedToken))
        .transpose()
}

pub(super) fn decode_language(raw: Option<&[u8]>) -> Result<String, RecordError> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_CONVERSATION_LANGUAGE.to_owned());
    };
    let tag = full_string(raw).ok_or(RecordError::UnexpectedToken)?;
    if !is_valid_conversation_language(&tag) {
        return Err(RecordError::InvalidDurableField);
    }
    Ok(tag)
}

pub(super) fn decode_preferences(raw: &[u8]) -> Result<SessionPreferences, RecordError> {
    let Ok(Json::Object(object)) = parse_json(raw) else {
        return Err(RecordError::UnexpectedToken);
    };
    let mut entries: Vec<_> = object
        .into_entries()
        .into_iter()
        .filter(|(name, _)| name != INSTRUCTIONS_KEY)
        .collect();
    let has = |entries: &Vec<(_, Json<'_>)>, key: &str| entries.iter().any(|(name, _)| name == key);
    let legacy = has(&entries, "connection_id") || has(&entries, "model_id");
    let mut allowed: Vec<&str> = if legacy {
        vec!["connection_id", "model_id", "effort", "fast_mode"]
    } else if has(&entries, "provider") {
        PREFERENCE_KEYS[..4].to_vec()
    } else {
        PREFERENCE_KEYS[1..4].to_vec()
    };
    if has(&entries, "ultrafast_mode") {
        allowed.push("ultrafast_mode");
    }
    if entries.len() != allowed.len()
        || !entries
            .iter()
            .all(|(name, _)| allowed.contains(&name.as_ref()))
    {
        return Err(RecordError::InvalidDurableField);
    }
    let mut take = |key: &str| {
        entries
            .iter()
            .position(|(name, _)| name == key)
            .map(|at| entries.swap_remove(at).1)
    };
    let provider = if legacy {
        match take("connection_id").and_then(string).as_deref() {
            Some("vercel") => gateway()?,
            _ => return Err(RecordError::InvalidDurableField),
        }
    } else {
        match take("provider") {
            Some(value) => parse_saved_provider(&value).ok_or(RecordError::InvalidDurableField)?,
            None => gateway()?,
        }
    };
    let model = take(if legacy { "model_id" } else { "model" })
        .and_then(string)
        .filter(|model| is_valid_model(model))
        .ok_or(RecordError::InvalidDurableField)?;
    let effort = take("effort")
        .and_then(string)
        .and_then(|label| ReasoningEffort::parse(&label))
        .ok_or(RecordError::InvalidDurableField)?;
    let fast_mode = take("fast_mode")
        .and_then(|value| value.as_bool())
        .ok_or(RecordError::InvalidDurableField)?;
    let ultrafast_mode = match take("ultrafast_mode") {
        Some(value) => value.as_bool().ok_or(RecordError::InvalidDurableField)?,
        None => false,
    };
    Ok(SessionPreferences {
        provider,
        model,
        effort,
        fast_mode,
        ultrafast_mode,
    })
}

fn gateway() -> Result<SavedProvider, RecordError> {
    SavedProvider::new(ProviderId::Gateway, None).ok_or(RecordError::InvalidDurableField)
}

pub(super) fn decode_usage(raw: &[u8]) -> Result<UsageSnapshot, RecordError> {
    let Ok(Json::Object(object)) = parse_json(raw) else {
        return Err(RecordError::InvalidUsageCheckpoint);
    };
    object
        .get("at_ms")
        .and_then(Json::as_i64)
        .ok_or(RecordError::InvalidUsageCheckpoint)?;
    let snapshot = object
        .get("snapshot")
        .ok_or(RecordError::InvalidUsageCheckpoint)?;
    UsageSnapshot::parse_rich(snapshot).map_err(|_| RecordError::InvalidUsageCheckpoint)
}

#[cfg(test)]
mod tests;
