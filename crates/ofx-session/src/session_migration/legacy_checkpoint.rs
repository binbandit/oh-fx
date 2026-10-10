use ofx_config::ProviderId;

use super::durable_turn::{ConversationTurn, Execution, TurnClose, execution, user};
use crate::json_fields::{Fields, Json};
use crate::session_codec::recovery_checkpoint::{
    ACTIONS, CAUSES, CREDENTIAL_SOURCES, durable_text, lowercase_digest, one_of, tool_state,
};
use crate::session_codec::{SavedProvider, is_valid_model, parse_saved_provider};
use crate::session_event::InterruptReason;

const DELIVERIES: [&str; 2] = ["possibly_sent", "definitely_unsent"];
const LEGACY_CONNECTION: &str = "vercel";
const LEGACY_ADAPTER: &str = "vercel_ai_gateway";
const MAX_ROUTE_FIELD_BYTES: usize = 4096;
const GATEWAY_FOREIGN_SOURCES: [&str; 3] =
    ["chatgpt_subscription", "grok_subscription", "configured"];

pub(super) enum LegacyCheckpoint {
    Archived(Box<ConversationTurn>),
    Continuable(Box<Continuable>),
}

pub(super) struct Continuable {
    pub(super) turn_id: u64,
    pub(super) user: String,
    pub(super) work_id: Option<String>,
    pub(super) assistant_source: String,
    pub(super) execution: Execution,
    pub(super) cause: &'static str,
    pub(super) action: &'static str,
    pub(super) tool_state: &'static str,
    pub(super) provider: SavedProvider,
    pub(super) model: String,
    pub(super) credential_source: Option<&'static str>,
    pub(super) credential_identity: Option<String>,
    pub(super) requested_fast_mode: bool,
    pub(super) fast_mode: bool,
    pub(super) ultrafast: Option<(bool, bool)>,
    pub(super) max_provider_attempts: u64,
    pub(super) consumed_provider_attempts: u64,
    pub(super) outstanding_reservation: bool,
}

struct Authority {
    provider: SavedProvider,
    model: String,
    credential_source: Option<&'static str>,
    credential_identity: Option<String>,
}

pub(super) fn legacy_checkpoint(value: Json<'_>) -> Option<LegacyCheckpoint> {
    let mut fields = Fields::new(value)?;
    let version = fields.unsigned("version")?;
    let legacy_route = match fields.required("route_identity") {
        Some(route) => {
            legacy_route_is_valid(&route, version).then_some(())?;
            one_of(&fields.required("delivery")?, &DELIVERIES)?;
            true
        }
        None => false,
    };
    let turn_id = fields.unsigned("turn_id")?;
    let (user, work_id) = user(fields.required("user")?)?;
    let assistant_source = durable_text(fields.required("assistant_source")?)?;
    let execution = execution(fields.required("execution")?)?;
    let cause = one_of(&fields.required("cause")?, &CAUSES)?;
    let action = one_of(&fields.required("action")?, &ACTIONS)?;
    let tool_state = tool_state(&fields.required("tool_state")?)?.as_str();
    let authority = match (legacy_route, version) {
        (true, _) => Authority::bare(ProviderId::Gateway, &mut fields)?,
        (false, 1) => {
            let provider = fields.or("route_provider", ProviderId::Gateway, |value| {
                ProviderId::parse(value.as_str()?)
            })?;
            Authority::bare(provider, &mut fields)?
        }
        (false, 2) => authority(fields.required("authority")?)?,
        _ => return None,
    };
    let requested_fast_mode = fields.flag("requested_fast_mode")?;
    let fast_mode = fields.flag("fast_mode")?;
    let ultrafast = match (
        fields.required("requested_ultrafast_mode"),
        fields.required("ultrafast_mode"),
    ) {
        (None, None) => None,
        (Some(requested), Some(effective)) if !legacy_route && version == 2 => {
            Some((requested.as_bool()?, effective.as_bool()?))
        }
        _ => return None,
    };
    let max_provider_attempts = fields.unsigned("max_provider_attempts")?;
    let consumed_provider_attempts = fields.unsigned("consumed_provider_attempts")?;
    let outstanding_reservation = fields.flag("outstanding_reservation")?;
    let attempts_fit = turn_id != 0
        && max_provider_attempts != 0
        && consumed_provider_attempts <= max_provider_attempts
        && !(outstanding_reservation && consumed_provider_attempts >= max_provider_attempts);
    attempts_fit.then_some(())?;
    if legacy_route {
        let partial = (!assistant_source.is_empty()).then_some(assistant_source);
        let archived = ConversationTurn {
            user,
            work_id,
            execution,
            close: TurnClose::Interrupted {
                reason: InterruptReason::Failed,
                partial,
                pending: None,
                completed: Vec::new(),
                cancelled: None,
            },
        };
        return fields.finish(LegacyCheckpoint::Archived(Box::new(archived)));
    }
    let continuable = Continuable {
        turn_id,
        user,
        work_id,
        assistant_source,
        execution,
        cause,
        action,
        tool_state,
        provider: authority.provider,
        model: authority.model,
        credential_source: authority.credential_source,
        credential_identity: authority.credential_identity,
        requested_fast_mode,
        fast_mode,
        ultrafast,
        max_provider_attempts,
        consumed_provider_attempts,
        outstanding_reservation,
    };
    fields.finish(LegacyCheckpoint::Continuable(Box::new(continuable)))
}

impl Authority {
    fn bare(provider: ProviderId, fields: &mut Fields<'_>) -> Option<Self> {
        Some(Self {
            provider: SavedProvider::new(provider, None)?,
            model: model(fields.required("route_model")?)?,
            credential_source: None,
            credential_identity: None,
        })
    }
}

fn authority(value: Json<'_>) -> Option<Authority> {
    let mut fields = Fields::new(value)?;
    let provider = parse_saved_provider(&fields.required("provider")?)?;
    let model = model(fields.required("model")?)?;
    let credential_source = match fields.required("credential_source")? {
        Json::Null => None,
        source => Some(
            one_of(&source, &CREDENTIAL_SOURCES)
                .filter(|source| authorizes(provider.id(), source))?,
        ),
    };
    let credential_identity = match fields.required("credential_identity")? {
        Json::Null => None,
        Json::String(hex) => {
            lowercase_digest(&hex)?;
            Some(hex.into_owned())
        }
        _ => return None,
    };
    if credential_identity.is_some() && credential_source.is_none() {
        return None;
    }
    fields.finish(Authority {
        provider,
        model,
        credential_source,
        credential_identity,
    })
}

fn model(value: Json<'_>) -> Option<String> {
    durable_text(value).filter(|model| is_valid_model(model))
}

fn authorizes(provider: &ProviderId, source: &str) -> bool {
    source == "host_managed"
        || match provider {
            ProviderId::Gateway => !GATEWAY_FOREIGN_SOURCES.contains(&source),
            ProviderId::Codex => source == "chatgpt_subscription",
            ProviderId::Grok => source == "grok_subscription",
            ProviderId::Configured(_) => source == "configured",
        }
}

fn legacy_route_is_valid(route: &Json<'_>, version: u64) -> bool {
    let keys: &[&str] = match version {
        2 => &[
            "connection_id",
            "adapter_kind",
            "permission_review_model_id",
        ],
        3 => &[
            "connection_id",
            "adapter_kind",
            "permission_review_model_id",
            "vision_model_id",
            "subagent_model_id",
        ],
        4 => &[
            "version",
            "connection_id",
            "adapter_kind",
            "endpoint",
            "protocol",
            "credential_ref",
            "permission_review_model_id",
            "vision_model_id",
            "subagent_model_id",
        ],
        _ => return false,
    };
    let Some(object) = route.as_object() else {
        return false;
    };
    let text = |key| route.get(key).and_then(Json::as_str);
    let exact = object.len() == keys.len() && keys.iter().all(|key| object.contains_key(key));
    let fields_fit = object.iter().all(|(key, value)| {
        key == "version"
            || value
                .as_str()
                .is_some_and(|text| text.len() <= MAX_ROUTE_FIELD_BYTES)
    });
    let versioned = version != 4
        || (route.get("version").and_then(Json::as_u64) == Some(1)
            && text("protocol") == Some(LEGACY_ADAPTER));
    exact
        && fields_fit
        && versioned
        && text("connection_id") == Some(LEGACY_CONNECTION)
        && text("adapter_kind") == Some(LEGACY_ADAPTER)
}
