use std::fmt::Write as _;

use ofx_config::ProviderId;
use ofx_contract::{
    GenerationFact, Json, Object, UsageCompleteness, UsageIncident, valid_gateway_generation_id,
};
use ofx_text::lowercase_hex;

mod accounting;
mod exact;

pub(crate) use accounting::{ReserveFailure, Usage};

use crate::generation_fact_codec::{self, cost, non_negative};
use crate::json_fields::push_string;
use crate::session_codec::recovery_checkpoint::{
    CREDENTIAL_IDENTITY_BYTES, CREDENTIAL_SOURCES, lowercase_digest, one_of,
};

const MAX_MODELS: usize = 32;
const MAX_PENDING_GENERATIONS: usize = 16;
const MAX_PUBLICATION_BACKLOG: usize = 16;
const MAX_USAGE_INCIDENTS: usize = 16;
const MAX_MODEL_BYTES: usize = 1024;
const MAX_ORIGIN_BYTES: usize = 2048;
const MAX_TEAM_BYTES: usize = 255;
const MAX_IDENTIFIER_BYTES: usize = 8 * 1024;
pub(crate) const MAX_SNAPSHOT_BYTES: usize = 256 * 1024;
const RICH_SCHEMA_VERSION: u64 = 3;
const SUPPORTED_SCHEMAS: [u64; 2] = [2, 3];
const SNAPSHOT_FIELDS: usize = 23;
const MODEL_FIELDS: usize = 10;
const LEGACY_SNAPSHOT_FIELDS: usize = 18;
const LEGACY_MODEL_FIELDS: usize = 8;
const PROVIDER_PENDING_FIELDS: usize = 8;
const PLAIN_PENDING_FIELDS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum UsageSnapshotError {
    #[error("InvalidUsageSnapshot")]
    Invalid,
    #[error("UsageCapacityExceeded")]
    CapacityExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Availability {
    Complete,
    Pending,
    Incomplete,
    Legacy,
}

impl Availability {
    const fn name(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Pending => "pending",
            Self::Incomplete => "incomplete",
            Self::Legacy => "legacy",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [
            Self::Complete,
            Self::Pending,
            Self::Incomplete,
            Self::Legacy,
        ]
        .into_iter()
        .find(|availability| availability.name() == name)
    }
}

fn authorizes(source: &str, provider: &ProviderId) -> bool {
    match (source, provider) {
        ("host_managed", _)
        | ("chatgpt_subscription", ProviderId::Codex)
        | ("grok_subscription", ProviderId::Grok)
        | ("configured", ProviderId::Configured(_)) => true,
        ("chatgpt_subscription" | "grok_subscription" | "configured", _)
        | (_, ProviderId::Codex | ProviderId::Grok | ProviderId::Configured(_)) => false,
        (_, ProviderId::Gateway) => true,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelAggregate {
    pub(crate) model: String,
    pub(crate) first_sequence: u64,
    pub(crate) total_cost: f64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_write_tokens: u64,
    pub(crate) reasoning_tokens: Option<u64>,
    pub(crate) request_count: Option<u64>,
    pub(crate) billable_web_search_calls: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingGeneration {
    pub(crate) id: String,
    pub(crate) sequence: u64,
    pub(crate) provider: ProviderId,
    pub(crate) origin: String,
    pub(crate) team: Option<String>,
    pub(crate) credential_source: Option<&'static str>,
    pub(crate) credential_identity: Option<[u8; CREDENTIAL_IDENTITY_BYTES]>,
    pub(crate) account_id: Option<String>,
    pub(crate) observed_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageSnapshot {
    pub(crate) billing: Availability,
    pub(crate) api_duration_complete: bool,
    pub(crate) wall_duration_complete: bool,
    pub(crate) code_complete: bool,
    pub(crate) next_sequence: u64,
    pub(crate) settled_through_sequence: u64,
    pub(crate) api_duration_ms: u64,
    pub(crate) wall_duration_ms: u64,
    pub(crate) total_cost: f64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_write_tokens: u64,
    pub(crate) reasoning_tokens: Option<u64>,
    pub(crate) request_count: Option<u64>,
    pub(crate) billable_web_search_calls: u64,
    pub(crate) lines_added: u64,
    pub(crate) lines_removed: u64,
    pub(crate) models: Vec<ModelAggregate>,
    pub(crate) pending: Vec<PendingGeneration>,
    pub(crate) publication_backlog: Vec<GenerationFact>,
    pub(crate) incidents: Vec<UsageIncident>,
}

impl UsageSnapshot {
    pub(crate) fn fresh() -> Self {
        Self {
            billing: Availability::Complete,
            api_duration_complete: true,
            wall_duration_complete: true,
            code_complete: true,
            reasoning_tokens: Some(0),
            request_count: Some(0),
            ..Self::empty(Availability::Complete)
        }
    }

    pub(crate) fn unavailable() -> Self {
        Self::empty(Availability::Incomplete)
    }

    const fn empty(billing: Availability) -> Self {
        Self {
            billing,
            api_duration_complete: false,
            wall_duration_complete: false,
            code_complete: false,
            next_sequence: 1,
            settled_through_sequence: 0,
            api_duration_ms: 0,
            wall_duration_ms: 0,
            total_cost: 0.0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
            request_count: None,
            billable_web_search_calls: 0,
            lines_added: 0,
            lines_removed: 0,
            models: Vec::new(),
            pending: Vec::new(),
            publication_backlog: Vec::new(),
            incidents: Vec::new(),
        }
    }

    pub(crate) fn append_incident(
        &mut self,
        incident: UsageIncident,
    ) -> Result<(), UsageSnapshotError> {
        if !valid_incident(&incident) {
            return Err(UsageSnapshotError::Invalid);
        }
        self.push_incident(incident);
        Ok(())
    }

    fn push_incident(&mut self, incident: UsageIncident) {
        if self.incidents.contains(&incident) {
            return;
        }
        if self.incidents.len() == MAX_USAGE_INCIDENTS {
            let newest_at_ms = self
                .incidents
                .iter()
                .map(|existing| existing.occurred_at_ms)
                .fold(incident.occurred_at_ms, i64::max);
            self.incidents = vec![UsageIncident {
                occurred_at_ms: newest_at_ms,
                completeness: UsageCompleteness::Incomplete,
            }];
            return;
        }
        self.incidents.push(incident);
    }

    pub(crate) fn validate(&self) -> Result<(), UsageSnapshotError> {
        self.validate_contract(false).map(|_| ())
    }

    fn validate_contract(&self, allow_legacy_cache: bool) -> Result<bool, UsageSnapshotError> {
        let invalid = UsageSnapshotError::Invalid;
        let mut cache_totals_valid = true;
        if self.next_sequence == 0 || self.settled_through_sequence >= self.next_sequence {
            return Err(invalid);
        }
        if !valid_cost(self.total_cost) {
            return Err(invalid);
        }
        if self.models.len() > MAX_MODELS
            || self.pending.len() > MAX_PENDING_GENERATIONS
            || self.publication_backlog.len() > MAX_PUBLICATION_BACKLOG
            || self.incidents.len() > MAX_USAGE_INCIDENTS
        {
            return Err(UsageSnapshotError::CapacityExceeded);
        }
        match self.billing {
            Availability::Complete if !self.pending.is_empty() => return Err(invalid),
            Availability::Pending if self.pending.is_empty() => return Err(invalid),
            _ => {}
        }
        let mut totals = Sums::starting(self);
        for (index, model) in self.models.iter().enumerate() {
            if !valid_model(&model.model)
                || model.first_sequence == 0
                || model.first_sequence >= self.next_sequence
                || !valid_cost(model.total_cost)
                || model
                    .reasoning_tokens
                    .is_some_and(|reasoning| reasoning > model.output_tokens)
            {
                return Err(invalid);
            }
            totals.add(model).ok_or(invalid)?;
            if model.cache_read_tokens > model.input_tokens
                || model.cache_write_tokens > model.input_tokens
            {
                if !allow_legacy_cache {
                    return Err(invalid);
                }
                cache_totals_valid = false;
            }
            let earlier = &self.models[..index];
            if earlier.iter().any(|prior| prior.model == model.model)
                || earlier
                    .last()
                    .is_some_and(|prior| prior.first_sequence >= model.first_sequence)
            {
                return Err(invalid);
            }
        }
        if !totals.matches(self) {
            return Err(invalid);
        }
        for (index, pending) in self.pending.iter().enumerate() {
            pending.validate(self.next_sequence)?;
            if self.pending[..index]
                .iter()
                .any(|prior| prior.id == pending.id || prior.sequence == pending.sequence)
            {
                return Err(invalid);
            }
        }
        for (index, fact) in self.publication_backlog.iter().enumerate() {
            if !fact.is_valid()
                || self.publication_backlog[..index]
                    .iter()
                    .any(|prior| prior.id == fact.id)
            {
                return Err(invalid);
            }
        }
        if !self.incidents.iter().all(valid_incident) {
            return Err(invalid);
        }
        if self.identifier_bytes() > MAX_IDENTIFIER_BYTES {
            return Err(UsageSnapshotError::CapacityExceeded);
        }
        Ok(cache_totals_valid)
    }

    fn identifier_bytes(&self) -> usize {
        let models = self.models.iter().map(|model| model.model.len());
        let pending = self.pending.iter().flat_map(|pending| {
            [
                pending.id.len(),
                pending.origin.len(),
                pending.team.as_ref().map_or(0, String::len),
                pending.account_id.as_ref().map_or(0, String::len),
            ]
        });
        let backlog = self
            .publication_backlog
            .iter()
            .flat_map(|fact| [fact.id.len(), fact.model.len()]);
        models
            .chain(pending)
            .chain(backlog)
            .fold(0, usize::saturating_add)
    }

    pub(crate) fn write_rich(&self, out: &mut String) -> Result<(), UsageSnapshotError> {
        self.validate()?;
        let _ = write!(
            out,
            "{{\"schema_version\":{RICH_SCHEMA_VERSION},\"billing\":\"{}\",\"api_duration_complete\":{},\"wall_duration_complete\":{},\"code_complete\":{},\"next_sequence\":{},\"settled_through_sequence\":{},\"api_duration_ms\":{},\"wall_duration_ms\":{},\"total_cost\":{},\"input_tokens\":{},\"output_tokens\":{},\"cache_read_tokens\":{},\"cache_write_tokens\":{},\"reasoning_tokens\":",
            self.billing.name(),
            self.api_duration_complete,
            self.wall_duration_complete,
            self.code_complete,
            self.next_sequence,
            self.settled_through_sequence,
            self.api_duration_ms,
            self.wall_duration_ms,
            self.total_cost,
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
        );
        push_optional(out, self.reasoning_tokens);
        out.push_str(",\"request_count\":");
        push_optional(out, self.request_count);
        let _ = write!(
            out,
            ",\"billable_web_search_calls\":{},\"lines_added\":{},\"lines_removed\":{},\"models\":[",
            self.billable_web_search_calls, self.lines_added, self.lines_removed,
        );
        for (index, model) in self.models.iter().enumerate() {
            push_separator(out, index);
            model.write(out);
        }
        out.push_str("],\"pending\":[");
        for (index, pending) in self.pending.iter().enumerate() {
            push_separator(out, index);
            pending.write(out);
        }
        out.push_str("],\"publication_backlog\":[");
        for (index, fact) in self.publication_backlog.iter().enumerate() {
            push_separator(out, index);
            generation_fact_codec::write(out, fact);
        }
        out.push_str("],\"incidents\":[");
        for (index, incident) in self.incidents.iter().enumerate() {
            push_separator(out, index);
            let _ = write!(
                out,
                "{{\"occurred_at_ms\":{},\"completeness\":\"{}\"}}",
                incident.occurred_at_ms,
                incident.completeness.name()
            );
        }
        out.push_str("]}");
        Ok(())
    }

    pub(crate) fn parse_rich(value: &Json<'_>) -> Result<Self, UsageSnapshotError> {
        let snapshot = parse_fields(value).ok_or(UsageSnapshotError::Invalid)??;
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub(crate) fn parse_legacy(value: &Json<'_>) -> Result<Self, UsageSnapshotError> {
        let snapshot = parse_fields(value).ok_or(UsageSnapshotError::Invalid)??;
        let unversioned = value
            .as_object()
            .is_some_and(|object| object.len() == LEGACY_SNAPSHOT_FIELDS);
        if snapshot.validate_contract(unversioned)? {
            Ok(snapshot)
        } else {
            Ok(Self::empty(Availability::Legacy))
        }
    }
}

impl ModelAggregate {
    fn write(&self, out: &mut String) {
        out.push_str("{\"model\":");
        push_string(out, &self.model);
        let _ = write!(
            out,
            ",\"first_sequence\":{},\"total_cost\":{},\"input_tokens\":{},\"output_tokens\":{},\"cache_read_tokens\":{},\"cache_write_tokens\":{},\"reasoning_tokens\":",
            self.first_sequence,
            self.total_cost,
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
        );
        push_optional(out, self.reasoning_tokens);
        out.push_str(",\"request_count\":");
        push_optional(out, self.request_count);
        let _ = write!(
            out,
            ",\"billable_web_search_calls\":{}}}",
            self.billable_web_search_calls
        );
    }

    fn parse(value: &Json<'_>, legacy: bool) -> Option<Self> {
        let object = value.as_object()?;
        if object.len()
            != if legacy {
                LEGACY_MODEL_FIELDS
            } else {
                MODEL_FIELDS
            }
        {
            return None;
        }
        let optional = |key: &str| {
            if legacy {
                Some(None)
            } else {
                or_null(object.get(key)?, |value| value.as_u64().map(Some))
            }
        };
        Some(Self {
            model: object.get("model")?.as_str()?.to_owned(),
            first_sequence: object.get("first_sequence")?.as_u64()?,
            total_cost: cost(object.get("total_cost")?)?,
            input_tokens: object.get("input_tokens")?.as_u64()?,
            output_tokens: object.get("output_tokens")?.as_u64()?,
            cache_read_tokens: object.get("cache_read_tokens")?.as_u64()?,
            cache_write_tokens: object.get("cache_write_tokens")?.as_u64()?,
            reasoning_tokens: optional("reasoning_tokens")?,
            request_count: optional("request_count")?,
            billable_web_search_calls: object.get("billable_web_search_calls")?.as_u64()?,
        })
    }
}

impl PendingGeneration {
    fn validate(&self, next_sequence: u64) -> Result<(), UsageSnapshotError> {
        let valid = valid_gateway_generation_id(&self.id)
            && printable_within(&self.origin, MAX_ORIGIN_BYTES)
            && self
                .team
                .as_deref()
                .is_none_or(|team| printable_within(team, MAX_TEAM_BYTES))
            && self.account_id.as_deref().is_none_or(valid_identifier)
            && (self.credential_identity.is_none() || self.credential_source.is_some())
            && self
                .credential_source
                .is_none_or(|source| authorizes(source, &self.provider))
            && self.sequence != 0
            && self.sequence < next_sequence
            && self.observed_at_ms.is_none_or(|observed| observed >= 0);
        valid.then_some(()).ok_or(UsageSnapshotError::Invalid)
    }

    fn write(&self, out: &mut String) {
        out.push_str("{\"id\":");
        push_string(out, &self.id);
        let _ = write!(out, ",\"sequence\":{},\"provider\":", self.sequence);
        push_string(out, provider_tag(&self.provider));
        out.push_str(",\"origin\":");
        push_string(out, &self.origin);
        out.push_str(",\"team\":");
        push_optional_string(out, self.team.as_deref());
        out.push_str(",\"credential_source\":");
        push_optional_string(out, self.credential_source);
        out.push_str(",\"credential_identity\":");
        push_optional_string(
            out,
            self.credential_identity
                .map(|identity| lowercase_hex(&identity))
                .as_deref(),
        );
        out.push_str(",\"account_id\":");
        push_optional_string(out, self.account_id.as_deref());
        out.push_str(",\"observed_at_ms\":");
        push_optional(out, self.observed_at_ms);
        out.push('}');
    }

    fn parse(value: &Json<'_>) -> Option<Self> {
        let object = value.as_object()?;
        let provider_scoped = object.contains_key("provider");
        let observed = object.contains_key("observed_at_ms");
        let fields = if provider_scoped {
            PROVIDER_PENDING_FIELDS
        } else {
            PLAIN_PENDING_FIELDS
        } + usize::from(observed);
        if object.len() != fields {
            return None;
        }
        let mut pending = Self {
            id: object.get("id")?.as_str()?.to_owned(),
            sequence: object.get("sequence")?.as_u64()?,
            provider: ProviderId::Gateway,
            origin: object.get("origin")?.as_str()?.to_owned(),
            team: or_null(object.get("team")?, |value| {
                value.as_str().map(|text| Some(text.to_owned()))
            })?,
            credential_source: None,
            credential_identity: None,
            account_id: None,
            observed_at_ms: if observed {
                or_null(object.get("observed_at_ms")?, |value| {
                    non_negative(value).map(Some)
                })?
            } else {
                None
            },
        };
        if provider_scoped {
            pending.provider = ProviderId::parse(object.get("provider")?.as_str()?)?;
            pending.credential_source = match object.get("credential_source")? {
                Json::Null => None,
                value => Some(one_of(value, &CREDENTIAL_SOURCES)?),
            };
            pending.credential_identity = match object.get("credential_identity")? {
                Json::Null => None,
                value => Some(lowercase_digest(value.as_str()?)?),
            };
            pending.account_id = or_null(object.get("account_id")?, |value| {
                value.as_str().map(|text| Some(text.to_owned()))
            })?;
        }
        Some(pending)
    }
}

struct Sums {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: Option<u64>,
    request_count: Option<u64>,
    billable_web_search_calls: u64,
    total_cost: f64,
}

impl Sums {
    fn starting(snapshot: &UsageSnapshot) -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: snapshot.reasoning_tokens.map(|_| 0),
            request_count: snapshot.request_count.map(|_| 0),
            billable_web_search_calls: 0,
            total_cost: 0.0,
        }
    }

    fn add(&mut self, model: &ModelAggregate) -> Option<()> {
        self.total_cost += model.total_cost;
        if !self.total_cost.is_finite() {
            return None;
        }
        self.input_tokens = self.input_tokens.checked_add(model.input_tokens)?;
        self.output_tokens = self.output_tokens.checked_add(model.output_tokens)?;
        self.cache_read_tokens = self
            .cache_read_tokens
            .checked_add(model.cache_read_tokens)?;
        self.cache_write_tokens = self
            .cache_write_tokens
            .checked_add(model.cache_write_tokens)?;
        accumulate(&mut self.reasoning_tokens, model.reasoning_tokens)?;
        accumulate(&mut self.request_count, model.request_count)?;
        self.billable_web_search_calls = self
            .billable_web_search_calls
            .checked_add(model.billable_web_search_calls)?;
        Some(())
    }

    fn matches(&self, snapshot: &UsageSnapshot) -> bool {
        let tolerance = f64::max(1e-12, snapshot.total_cost * 1e-12);
        self.input_tokens == snapshot.input_tokens
            && self.output_tokens == snapshot.output_tokens
            && self.cache_read_tokens == snapshot.cache_read_tokens
            && self.cache_write_tokens == snapshot.cache_write_tokens
            && self.reasoning_tokens == snapshot.reasoning_tokens
            && self.request_count == snapshot.request_count
            && self.billable_web_search_calls == snapshot.billable_web_search_calls
            && (self.total_cost - snapshot.total_cost).abs() <= tolerance
    }
}

fn accumulate(total: &mut Option<u64>, count: Option<u64>) -> Option<()> {
    *total = match (*total, count) {
        (Some(total), Some(count)) => Some(total.checked_add(count)?),
        _ => None,
    };
    Some(())
}

fn parse_fields(value: &Json<'_>) -> Option<Result<UsageSnapshot, UsageSnapshotError>> {
    let object = value.as_object()?;
    let legacy = object.len() == LEGACY_SNAPSHOT_FIELDS;
    if !legacy {
        let schema_version = object.get("schema_version")?.as_u64()?;
        if object.len() != SNAPSHOT_FIELDS || !SUPPORTED_SCHEMAS.contains(&schema_version) {
            return None;
        }
    }
    let models = object.get("models")?.as_array()?;
    let pending = object.get("pending")?.as_array()?;
    if models.len() > MAX_MODELS || pending.len() > MAX_PENDING_GENERATIONS {
        return Some(Err(UsageSnapshotError::CapacityExceeded));
    }
    let list = |key: &str, limit: usize| {
        if legacy {
            return Some(&[][..]);
        }
        object
            .get(key)?
            .as_array()
            .filter(|items| items.len() <= limit)
    };
    let backlog = list("publication_backlog", MAX_PUBLICATION_BACKLOG)?;
    let incidents = list("incidents", MAX_USAGE_INCIDENTS)?;
    let optional = |key: &str| {
        if legacy {
            Some(None)
        } else {
            or_null(object.get(key)?, |value| value.as_u64().map(Some))
        }
    };
    Some(Ok(UsageSnapshot {
        billing: Availability::parse(object.get("billing")?.as_str()?)?,
        api_duration_complete: object.get("api_duration_complete")?.as_bool()?,
        wall_duration_complete: object.get("wall_duration_complete")?.as_bool()?,
        code_complete: object.get("code_complete")?.as_bool()?,
        next_sequence: object.get("next_sequence")?.as_u64()?,
        settled_through_sequence: object.get("settled_through_sequence")?.as_u64()?,
        api_duration_ms: object.get("api_duration_ms")?.as_u64()?,
        wall_duration_ms: object.get("wall_duration_ms")?.as_u64()?,
        total_cost: cost(object.get("total_cost")?)?,
        input_tokens: object.get("input_tokens")?.as_u64()?,
        output_tokens: object.get("output_tokens")?.as_u64()?,
        cache_read_tokens: object.get("cache_read_tokens")?.as_u64()?,
        cache_write_tokens: object.get("cache_write_tokens")?.as_u64()?,
        reasoning_tokens: optional("reasoning_tokens")?,
        request_count: optional("request_count")?,
        billable_web_search_calls: object.get("billable_web_search_calls")?.as_u64()?,
        lines_added: object.get("lines_added")?.as_u64()?,
        lines_removed: object.get("lines_removed")?.as_u64()?,
        models: models
            .iter()
            .map(|model| ModelAggregate::parse(model, legacy))
            .collect::<Option<_>>()?,
        pending: pending
            .iter()
            .map(PendingGeneration::parse)
            .collect::<Option<_>>()?,
        publication_backlog: backlog
            .iter()
            .map(generation_fact_codec::parse)
            .collect::<Option<_>>()?,
        incidents: incidents.iter().map(incident).collect::<Option<_>>()?,
    }))
}

fn incident(value: &Json<'_>) -> Option<UsageIncident> {
    let object: &Object<'_> = value.as_object()?;
    if object.len() != 2 {
        return None;
    }
    Some(UsageIncident {
        completeness: UsageCompleteness::parse(object.get("completeness")?.as_str()?)?,
        occurred_at_ms: non_negative(object.get("occurred_at_ms")?)?,
    })
}

fn or_null<T: Default>(value: &Json<'_>, read: impl FnOnce(&Json<'_>) -> Option<T>) -> Option<T> {
    match value {
        Json::Null => Some(T::default()),
        value => read(value),
    }
}

fn provider_tag(provider: &ProviderId) -> &'static str {
    match provider {
        ProviderId::Gateway => "gateway",
        ProviderId::Codex => "codex",
        ProviderId::Grok => "grok",
        ProviderId::Configured(_) => "configured",
    }
}

fn valid_cost(cost: f64) -> bool {
    cost.is_finite() && cost >= 0.0
}

fn valid_model(model: &str) -> bool {
    printable_within(model, MAX_MODEL_BYTES)
}

fn printable_within(text: &str, max_bytes: usize) -> bool {
    !text.is_empty()
        && text.len() <= max_bytes
        && text.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

fn valid_identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_MODEL_BYTES
        && !text.bytes().any(|byte| byte.is_ascii_control())
}

fn valid_incident(incident: &UsageIncident) -> bool {
    incident.occurred_at_ms >= 0
        && matches!(
            incident.completeness,
            UsageCompleteness::Pending | UsageCompleteness::Incomplete
        )
}

fn push_separator(out: &mut String, index: usize) {
    if index > 0 {
        out.push(',');
    }
}

fn push_optional<T: std::fmt::Display>(out: &mut String, value: Option<T>) {
    match value {
        Some(value) => {
            let _ = write!(out, "{value}");
        }
        None => out.push_str("null"),
    }
}

fn push_optional_string(out: &mut String, value: Option<&str>) {
    match value {
        Some(text) => push_string(out, text),
        None => out.push_str("null"),
    }
}

#[cfg(test)]
mod tests;
