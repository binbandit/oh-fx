use ofx_contract::{TurnSummary, TurnTokenProgress};
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

use crate::json_fields::{Fields, Json};

struct Wire(TurnSummary);

struct WireProgress(TurnTokenProgress);

impl Serialize for Wire {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let summary = self.0;
        let mut wire = serializer.serialize_struct("TurnSummary", 5)?;
        wire.serialize_field("started_at_ms", &summary.started_at_ms)?;
        wire.serialize_field("completed_at_ms", &summary.completed_at_ms)?;
        wire.serialize_field("thinking_duration_ms", &summary.thinking_duration_ms)?;
        wire.serialize_field("turn_duration_ms", &summary.turn_duration_ms)?;
        wire.serialize_field("token_progress", &WireProgress(summary.token_progress))?;
        wire.end()
    }
}

impl Serialize for WireProgress {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let progress = self.0;
        let mut wire = serializer.serialize_struct("TurnTokenProgress", 4)?;
        wire.serialize_field("input_tokens", &progress.input_tokens)?;
        wire.serialize_field("output_tokens", &progress.output_tokens)?;
        wire.serialize_field("input_exact", &progress.input_exact)?;
        wire.serialize_field("output_exact", &progress.output_exact)?;
        wire.end()
    }
}

pub(crate) fn serialize<S: Serializer, P: Copy + Into<Option<TurnSummary>>>(
    summary: &P,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match (*summary).into() {
        Some(summary) => Wire(summary).serialize(serializer),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
pub(crate) fn deserialize<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<TurnSummary>, D::Error> {
    let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(None);
    }
    let text = value.to_string();
    crate::json_fields::parse_json(text.as_bytes())
        .ok()
        .and_then(read_frame)
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom("InvalidTurnSummary"))
}

pub(crate) fn read_frame(value: Json<'_>) -> Option<TurnSummary> {
    let mut fields = Fields::new(value)?;
    let summary = TurnSummary {
        started_at_ms: fields.or("started_at_ms", 0, |value| value.as_i64())?,
        completed_at_ms: fields.or("completed_at_ms", 0, |value| value.as_i64())?,
        thinking_duration_ms: fields.or("thinking_duration_ms", 0, |value| value.as_u64())?,
        turn_duration_ms: fields.or("turn_duration_ms", 0, |value| value.as_u64())?,
        token_progress: fields.or(
            "token_progress",
            TurnTokenProgress::default(),
            frame_progress,
        )?,
    };
    fields.finish(summary)
}

fn frame_progress(value: Json<'_>) -> Option<TurnTokenProgress> {
    let mut fields = Fields::new(value)?;
    let defaults = TurnTokenProgress::default();
    let progress = TurnTokenProgress {
        input_tokens: fields.or("input_tokens", 0, |value| value.as_u64())?,
        output_tokens: fields.or("output_tokens", 0, |value| value.as_u64())?,
        input_exact: fields.or("input_exact", defaults.input_exact, |value| value.as_bool())?,
        output_exact: fields.or("output_exact", defaults.output_exact, |value| {
            value.as_bool()
        })?,
    };
    fields.finish(progress)
}

pub(crate) fn read_checkpoint(value: Json<'_>) -> Option<TurnSummary> {
    let mut fields = Fields::new(value)?;
    let summary = TurnSummary {
        started_at_ms: fields.signed("started_at_ms")?,
        completed_at_ms: fields.signed("completed_at_ms")?,
        thinking_duration_ms: fields.unsigned("thinking_duration_ms")?,
        turn_duration_ms: fields.unsigned("turn_duration_ms")?,
        token_progress: checkpoint_progress(fields.required("token_progress")?)?,
    };
    let ordered = summary.started_at_ms >= 0 && summary.completed_at_ms >= summary.started_at_ms;
    fields.finish(summary).filter(|_| ordered)
}

fn checkpoint_progress(value: Json<'_>) -> Option<TurnTokenProgress> {
    let mut fields = Fields::new(value)?;
    let progress = TurnTokenProgress {
        input_tokens: fields.unsigned("input_tokens")?,
        output_tokens: fields.unsigned("output_tokens")?,
        input_exact: fields.flag("input_exact")?,
        output_exact: fields.flag("output_exact")?,
    };
    fields.finish(progress)
}
