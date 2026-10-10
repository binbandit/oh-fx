use ofx_config::ProviderId;
use ofx_contract::ReasoningEffort;

use super::durable_turn::{LegacyTurn, history_turn, is_valid_work_id};
use crate::json_fields::{Fields, Json, parse_json};
use crate::session_authority::{Identifier, parse_identifier};
use crate::session_codec::{SavedProvider, SessionPreferences, parse_saved_provider};
use crate::session_error::SessionError;

const ENVELOPE_SCHEMA_VERSION: u64 = 1;
const LEGACY_CONNECTION: &str = "vercel";

pub(super) struct Envelope {
    pub(super) generation: Identifier,
    pub(super) seq: u64,
    pub(super) event_id: Identifier,
    pub(super) timestamp_ms: i64,
    pub(super) event: Event,
}

pub(super) struct Started {
    pub(super) id: String,
    pub(super) created_at_ms: i64,
    pub(super) origin_workspace_root: String,
    pub(super) workspace_root: String,
    pub(super) conversation_language: String,
    pub(super) preferences: SessionPreferences,
    pub(super) subagent_child: bool,
}

#[derive(Default)]
pub(super) struct PreferenceChange {
    pub(super) provider: Option<SavedProvider>,
    pub(super) model: Option<String>,
    pub(super) effort: Option<ReasoningEffort>,
    pub(super) fast_mode: Option<bool>,
    pub(super) ultrafast_mode: Option<bool>,
}

pub(super) enum Event {
    Started(Started),
    PreferencesChanged(PreferenceChange),
    WorkspaceRebound {
        previous: String,
        current: String,
    },
    TurnCommitted {
        conversation_language: String,
        turn: LegacyTurn,
    },
    UsageCheckpointed,
}

pub(super) fn decode_frame(line: &[u8]) -> Result<Envelope, SessionError> {
    let document = parse_json(line).map_err(|_| SessionError::InvalidSessionFormat)?;
    let mut fields = Fields::new(document).ok_or(SessionError::InvalidSessionFormat)?;
    let version = fields
        .unsigned("schema_version")
        .ok_or(SessionError::InvalidSessionFormat)?;
    if version != ENVELOPE_SCHEMA_VERSION {
        return Err(SessionError::UnsupportedSessionSchema);
    }
    envelope(fields).ok_or(SessionError::InvalidSessionFormat)
}

fn envelope(mut fields: Fields<'_>) -> Option<Envelope> {
    let generation = parse_identifier(&fields.string("log_generation")?)?;
    let seq = fields.unsigned("seq").filter(|seq| *seq > 0)?;
    let event_id = parse_identifier(&fields.string("event_id")?)?;
    let timestamp_ms = fields.signed("timestamp_ms").filter(|ms| *ms >= 0)?;
    let kind = fields.string("kind")?;
    let payload = fields.required("payload")?;
    let event = match kind.as_str() {
        "session_started" => Event::Started(started(payload)?),
        "preferences_changed" => Event::PreferencesChanged(preference_change(payload)?),
        "workspace_rebound" => workspace_rebound(payload)?,
        "history_turn_committed" => turn_committed(payload)?,
        "usage_checkpointed" => usage_checkpointed(payload)?,
        _ => return None,
    };
    fields.finish(Envelope {
        generation,
        seq,
        event_id,
        timestamp_ms,
        event,
    })
}

fn started(payload: Json<'_>) -> Option<Started> {
    let mut fields = Fields::new(payload)?;
    let started = Started {
        id: fields.string("id")?,
        created_at_ms: fields.signed("created_at_ms")?,
        origin_workspace_root: fields.string("origin_workspace_root")?,
        workspace_root: fields.string("workspace_root")?,
        conversation_language: fields.string("conversation_language")?,
        preferences: preferences(fields.required("preferences")?)?,
        subagent_child: fields.or("subagent_child", false, |value| {
            value.as_bool()?.then_some(true)
        })?,
    };
    fields.or("usage", (), |value| usage(&value))?;
    fields.finish(started)
}

fn preferences(value: Json<'_>) -> Option<SessionPreferences> {
    let mut fields = Fields::new(value)?;
    let (provider, model) = if let Some(connection) = fields.required("connection_id") {
        (connection.as_str()? == LEGACY_CONNECTION).then_some(())?;
        (gateway()?, fields.string("model_id")?)
    } else {
        let provider = match fields.required("provider") {
            Some(provider) => parse_saved_provider(&provider)?,
            None => gateway()?,
        };
        (provider, fields.string("model")?)
    };
    let preferences = SessionPreferences {
        provider,
        model,
        effort: ReasoningEffort::parse(&fields.string("effort")?)?,
        fast_mode: fields.flag("fast_mode")?,
        ultrafast_mode: fields.or("ultrafast_mode", false, |value| value.as_bool())?,
    };
    fields.finish(preferences)
}

fn gateway() -> Option<SavedProvider> {
    SavedProvider::new(ProviderId::Gateway, None)
}

fn preference_change(payload: Json<'_>) -> Option<PreferenceChange> {
    let mut fields = Fields::new(payload)?;
    let change = PreferenceChange {
        provider: fields.or("provider", None, |value| {
            parse_saved_provider(&value).map(Some)
        })?,
        model: fields.or("model", None, |value| {
            value.as_str().map(|model| Some(model.to_owned()))
        })?,
        effort: fields.or("effort", None, |value| {
            ReasoningEffort::parse(value.as_str()?).map(Some)
        })?,
        fast_mode: fields.or("fast_mode", None, |value| value.as_bool().map(Some))?,
        ultrafast_mode: fields.or("ultrafast_mode", None, |value| value.as_bool().map(Some))?,
    };
    let changes_something = change.provider.is_some()
        || change.model.is_some()
        || change.effort.is_some()
        || change.fast_mode.is_some()
        || change.ultrafast_mode.is_some();
    changes_something.then_some(())?;
    fields.finish(change)
}

fn workspace_rebound(payload: Json<'_>) -> Option<Event> {
    let mut fields = Fields::new(payload)?;
    let previous = fields
        .string("previous_workspace_root")
        .filter(|root| !root.is_empty())?;
    let current = fields
        .string("workspace_root")
        .filter(|root| !root.is_empty())?;
    fields.finish(Event::WorkspaceRebound { previous, current })
}

fn turn_committed(payload: Json<'_>) -> Option<Event> {
    let mut fields = Fields::new(payload)?;
    let conversation_language = fields.string("conversation_language")?;
    fields.unsigned("total_input_tokens")?;
    fields.unsigned("total_output_tokens")?;
    let mut turn = history_turn(fields.required("turn")?)?;
    let work_id = fields.or("work_id", None, |value| {
        value.as_str().map(|id| Some(id.to_owned()))
    })?;
    associate_work(&mut turn, work_id.as_deref())?;
    fields.finish(Event::TurnCommitted {
        conversation_language,
        turn,
    })
}

fn associate_work(turn: &mut LegacyTurn, committed: Option<&str>) -> Option<()> {
    match (committed, turn.work_id.as_deref()) {
        (None, None) => Some(()),
        (None, Some(_)) => None,
        (Some(committed), saved) => {
            is_valid_work_id(committed).then_some(())?;
            if let Some(saved) = saved {
                return (saved == committed).then_some(());
            }
            turn.work_id = Some(committed.to_owned());
            Some(())
        }
    }
}

fn usage_checkpointed(payload: Json<'_>) -> Option<Event> {
    let mut fields = Fields::new(payload)?;
    usage(&fields.required("usage")?)?;
    fields.finish(Event::UsageCheckpointed)
}

fn usage(value: &Json<'_>) -> Option<()> {
    matches!(value, Json::Object(_)).then_some(())
}
