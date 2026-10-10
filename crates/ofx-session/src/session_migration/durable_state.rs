use std::borrow::Cow;
use std::collections::HashSet;

use crate::json_fields::{Fields, Json, parse_json, string};
use crate::session_codec::SessionPreferences;
use crate::session_codec::recovery_checkpoint::{durable_bytes, list};

use super::durable_turn::{LegacyTurn, history_turn, is_valid_work_id};
use super::legacy_frame::preferences;

const STATE_TAIL: [&str; 6] = [
    "context_history_start",
    "permission_state",
    "usage",
    "last_subagent_work_id",
    "subagent_child",
    "recovery_checkpoint",
];
const PERMISSION_SCHEMAS: [u64; 2] = [1, 2];
const RULE_KINDS: [&str; 3] = ["command", "file_mutation", "structured_tool"];
const RULE_DECISIONS: [&str; 2] = ["allow", "deny"];
const MAX_RULES: usize = 1024;
const MAX_RULE_IDENTITY_BYTES: usize = 4096;

pub(super) struct DurableState {
    pub(super) id: String,
    pub(super) origin_workspace_root: String,
    pub(super) workspace_root: String,
    pub(super) created_at_ms: i64,
    pub(super) updated_at_ms: i64,
    pub(super) conversation_language: String,
    pub(super) preferences: SessionPreferences,
    pub(super) turns: Vec<LegacyTurn>,
    pub(super) context_history_start: usize,
    pub(super) subagent_child: bool,
    pub(super) recovery_set: bool,
}

struct Rule {
    id: u64,
    kind: String,
    canonical: Vec<u8>,
    generation: u64,
}

pub(super) fn durable_state(bytes: &[u8]) -> Option<DurableState> {
    let Json::Object(object) = parse_json(bytes).ok()? else {
        return None;
    };
    let mut entries = object.into_entries().into_iter();
    let mut state = DurableState {
        id: string(next(&mut entries, "id")?)?,
        origin_workspace_root: string(next(&mut entries, "origin_workspace_root")?)?,
        workspace_root: string(next(&mut entries, "workspace_root")?)?,
        created_at_ms: next(&mut entries, "created_at_ms")?.as_i64()?,
        updated_at_ms: next(&mut entries, "updated_at_ms")?.as_i64()?,
        conversation_language: string(next(&mut entries, "conversation_language")?)?,
        preferences: preferences(next(&mut entries, "preferences")?)?,
        turns: list(next(&mut entries, "history")?, history_turn)?,
        context_history_start: 0,
        subagent_child: false,
        recovery_set: false,
    };
    next(&mut entries, "total_input_tokens")?.as_u64()?;
    next(&mut entries, "total_output_tokens")?.as_u64()?;
    let mut position = 0;
    for (key, value) in entries {
        let index = STATE_TAIL.iter().position(|tail| *tail == key)?;
        (index >= position).then_some(())?;
        position = index + 1;
        match index {
            0 => {
                state.context_history_start = usize::try_from(value.as_u64()?).ok()?;
            }
            1 => permission_state(value)?,
            2 => matches!(value, Json::Object(_)).then_some(())?,
            3 => value
                .as_str()
                .filter(|id| is_valid_work_id(id))
                .map(|_| ())?,
            4 => state.subagent_child = value.as_bool()?.then_some(true)?,
            _ => state.recovery_set = matches!(value, Json::Object(_)).then_some(true)?,
        }
    }
    (state.context_history_start <= state.turns.len()).then_some(state)
}

fn next<'a>(
    entries: &mut impl Iterator<Item = (Cow<'a, str>, Json<'a>)>,
    key: &str,
) -> Option<Json<'a>> {
    let (name, value) = entries.next()?;
    (name == key).then_some(value)
}

fn permission_state(value: Json<'_>) -> Option<()> {
    let mut fields = Fields::new(value)?;
    fields
        .unsigned("schema_version")
        .filter(|version| PERMISSION_SCHEMAS.contains(version))?;
    let next_generation = fields
        .unsigned("next_generation")
        .filter(|next| *next > 0)?;
    let rules = list(fields.required("rules")?, rule)?;
    fields.finish(())?;
    (rules.len() <= MAX_RULES).then_some(())?;
    let mut ids = HashSet::new();
    let mut keys = HashSet::new();
    rules
        .iter()
        .all(|rule| {
            rule.id > 0
                && rule.id <= rule.generation
                && rule.generation < next_generation
                && ids.insert(rule.id)
                && keys.insert((rule.kind.as_str(), rule.canonical.as_slice()))
        })
        .then_some(())
}

fn rule(value: Json<'_>) -> Option<Rule> {
    let mut fields = Fields::new(value)?;
    let identity = |bytes: Vec<u8>| {
        (1..=MAX_RULE_IDENTITY_BYTES)
            .contains(&bytes.len())
            .then_some(bytes)
    };
    let rule = Rule {
        id: fields.unsigned("id")?,
        kind: fields
            .string("kind")
            .filter(|kind| RULE_KINDS.contains(&kind.as_str()))?,
        canonical: identity(durable_bytes(fields.required("canonical")?)?)?,
        generation: fields.unsigned("generation")?,
    };
    identity(durable_bytes(fields.required("display_identity")?)?)?;
    fields
        .string("decision")
        .filter(|decision| RULE_DECISIONS.contains(&decision.as_str()))?;
    fields.finish(rule)
}
