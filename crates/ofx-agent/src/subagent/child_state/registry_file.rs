use std::sync::Arc;

use ofx_contract::{
    ChildPhase, ModelFailureDiagnostic, PermissionMode, valid_agent_name, valid_instructions,
    valid_session_id,
};
use ofx_text::lowercase_hex;
use serde::Serialize;
use serde_json::Value;

use super::{ActiveWork, Child, Kind, MAX_CHILDREN, Outcome, Registry, RegistryError};

const SCHEMA_VERSION: u8 = 2;

#[derive(Serialize)]
struct RegistryWire<'a> {
    schema_version: u8,
    parent_id: &'a str,
    generation: u64,
    children: Vec<ChildWire<'a>>,
}

#[derive(Serialize)]
struct ChildWire<'a> {
    id: &'a str,
    kind: &'static str,
    persistent: Option<PersistentWire<'a>>,
    phase: &'static str,
    work_generation: u64,
    active: Option<ActiveWire<'a>>,
    last_work_id: Option<&'a str>,
    last_request_fingerprint: Option<String>,
    last_outcome: Option<&'static str>,
    last_failure: Option<&'a str>,
}

#[derive(Serialize)]
struct PersistentWire<'a> {
    agent: &'a str,
    instructions: &'a str,
}

#[derive(Serialize)]
struct ActiveWire<'a> {
    id: &'a str,
    request_fingerprint: String,
    message: &'a str,
    root_user_intent_context: &'a str,
    root_user_messages: [&'a str; 0],
    root_user_evidence_complete: bool,
    permission_mode: &'static str,
    created_at_ms: i64,
}

pub(super) fn render(registry: &Registry, parent_id: &str) -> Vec<u8> {
    let wire = RegistryWire {
        schema_version: SCHEMA_VERSION,
        parent_id,
        generation: registry.generation,
        children: registry.children.iter().map(child).collect(),
    };
    serde_json::to_vec(&wire).unwrap_or_default()
}

fn child(child: &Child) -> ChildWire<'_> {
    let (kind, persistent) = match &child.kind {
        Kind::OneOff => ("one_off", None),
        Kind::Persistent {
            agent,
            instructions,
        } => (
            "persistent",
            Some(PersistentWire {
                agent,
                instructions,
            }),
        ),
    };
    ChildWire {
        id: &child.id,
        kind,
        persistent,
        phase: phase(child.phase),
        work_generation: child.work_generation,
        active: child.active.as_ref().map(active),
        last_work_id: child.last_work_id.as_deref(),
        last_request_fingerprint: child
            .last_request_fingerprint
            .map(|fingerprint| lowercase_hex(&fingerprint)),
        last_outcome: child.last_outcome.map(outcome),
        last_failure: child
            .last_failure
            .as_ref()
            .map(ModelFailureDiagnostic::as_str),
    }
}

fn active(work: &ActiveWork) -> ActiveWire<'_> {
    ActiveWire {
        id: &work.id,
        request_fingerprint: lowercase_hex(&work.request_fingerprint),
        message: &work.message,
        root_user_intent_context: &work.root_user_context,
        root_user_messages: [],
        root_user_evidence_complete: false,
        permission_mode: work.permission_mode.label(),
        created_at_ms: work.created_at_ms,
    }
}

fn phase(phase: ChildPhase) -> &'static str {
    match phase {
        ChildPhase::Idle => "idle",
        ChildPhase::Running => "running",
        ChildPhase::AwaitingApproval => "awaiting_approval",
        ChildPhase::Interrupted => "interrupted",
        ChildPhase::Finished => "finished",
    }
}

fn outcome(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Completed => "completed",
        Outcome::Failed => "failed",
        Outcome::Cancelled => "cancelled",
        Outcome::Interrupted => "interrupted",
    }
}

type Object = serde_json::Map<String, Value>;

const REGISTRY_FIELDS: [&str; 4] = ["schema_version", "parent_id", "generation", "children"];
const CHILD_FIELDS: [&str; 10] = [
    "id",
    "kind",
    "persistent",
    "phase",
    "work_generation",
    "active",
    "last_work_id",
    "last_request_fingerprint",
    "last_outcome",
    "last_failure",
];
const ACTIVE_FIELDS: [&str; 8] = [
    "id",
    "request_fingerprint",
    "message",
    "root_user_intent_context",
    "root_user_messages",
    "root_user_evidence_complete",
    "permission_mode",
    "created_at_ms",
];
const MAX_ADMISSION_ITEMS: usize = 64;

pub(super) fn parse(bytes: &[u8], parent_id: &str) -> Result<Registry, RegistryError> {
    let root: Value = serde_json::from_slice(bytes).map_err(|_| RegistryError::InvalidState)?;
    let root = object(&root)?;
    exact_fields(root, &REGISTRY_FIELDS)?;
    let version = unsigned(root, "schema_version")?;
    if version != 1 && version != u64::from(SCHEMA_VERSION) {
        return Err(RegistryError::UnsupportedSchema);
    }
    if string(root, "parent_id")? != parent_id {
        return Err(RegistryError::InvalidParentId);
    }
    let values = root
        .get("children")
        .and_then(Value::as_array)
        .filter(|values| values.len() <= MAX_CHILDREN)
        .ok_or(RegistryError::InvalidState)?;
    let registry = Registry {
        generation: unsigned(root, "generation")?,
        children: values
            .iter()
            .map(|value| parse_child(value, version))
            .collect::<Result<_, _>>()?,
    };
    validate(&registry)?;
    Ok(registry)
}

fn parse_child(value: &Value, version: u64) -> Result<Child, RegistryError> {
    let source = object(value)?;
    let fields = if version == 1 {
        &CHILD_FIELDS[..CHILD_FIELDS.len() - 1]
    } else {
        &CHILD_FIELDS[..]
    };
    exact_fields(source, fields)?;
    let last_failure = if version == 1 {
        None
    } else {
        optional_string(source, "last_failure")?
            .map(|raw| ModelFailureDiagnostic::restored(raw).ok_or(RegistryError::InvalidState))
            .transpose()?
    };
    let id = string(source, "id")?;
    if !valid_session_id(id) {
        return Err(RegistryError::InvalidState);
    }
    let persistent = source
        .get("persistent")
        .ok_or(RegistryError::InvalidState)?;
    let kind = match string(source, "kind")? {
        "one_off" if persistent.is_null() => Kind::OneOff,
        "persistent" => parse_persistent(persistent)?,
        _ => return Err(RegistryError::InvalidState),
    };
    let active = match source.get("active") {
        Some(Value::Null) => None,
        Some(value) => Some(parse_active(value)?),
        None => return Err(RegistryError::InvalidState),
    };
    Ok(Child {
        id: id.to_owned(),
        kind,
        phase: parse_phase(string(source, "phase")?)?,
        work_generation: unsigned(source, "work_generation")?,
        active,
        last_work_id: optional_string(source, "last_work_id")?.map(str::to_owned),
        last_request_fingerprint: optional_string(source, "last_request_fingerprint")?
            .map(fingerprint)
            .transpose()?,
        last_outcome: optional_string(source, "last_outcome")?
            .map(parse_outcome)
            .transpose()?,
        last_failure,
    })
}

fn parse_persistent(value: &Value) -> Result<Kind, RegistryError> {
    let source = object(value)?;
    exact_fields(source, &["agent", "instructions"])?;
    let agent = string(source, "agent")?;
    let instructions = string(source, "instructions")?;
    if !valid_agent_name(agent) || !valid_instructions(instructions) {
        return Err(RegistryError::InvalidState);
    }
    Ok(Kind::Persistent {
        agent: agent.to_owned(),
        instructions: instructions.to_owned(),
    })
}

fn parse_active(value: &Value) -> Result<ActiveWork, RegistryError> {
    let source = object(value)?;
    exact_fields(source, &ACTIVE_FIELDS)?;
    let messages = source
        .get("root_user_messages")
        .and_then(Value::as_array)
        .filter(|messages| messages.len() <= MAX_ADMISSION_ITEMS)
        .ok_or(RegistryError::InvalidState)?;
    if !messages.iter().all(Value::is_string)
        || !source
            .get("root_user_evidence_complete")
            .is_some_and(Value::is_boolean)
    {
        return Err(RegistryError::InvalidState);
    }
    Ok(ActiveWork {
        id: string(source, "id")?.to_owned(),
        request_fingerprint: fingerprint(string(source, "request_fingerprint")?)?,
        message: string(source, "message")?.to_owned(),
        root_user_requests: Arc::default(),
        root_user_context: string(source, "root_user_intent_context")?.to_owned(),
        permission_mode: permission_mode(string(source, "permission_mode")?)?,
        created_at_ms: source
            .get("created_at_ms")
            .and_then(Value::as_i64)
            .ok_or(RegistryError::InvalidState)?,
    })
}

fn validate(registry: &Registry) -> Result<(), RegistryError> {
    for (index, child) in registry.children.iter().enumerate() {
        let working = matches!(
            child.phase,
            ChildPhase::Running | ChildPhase::AwaitingApproval
        );
        let failure_fits = child.last_failure.is_none()
            || (child.last_outcome == Some(Outcome::Failed) && child.last_work_id.is_some());
        let unique = registry.children[..index].iter().all(|prior| {
            prior.id != child.id
                && (child.agent_name().is_none() || prior.agent_name() != child.agent_name())
        });
        if working != child.active.is_some() || !failure_fits || !unique {
            return Err(RegistryError::InvalidState);
        }
    }
    Ok(())
}

fn parse_phase(raw: &str) -> Result<ChildPhase, RegistryError> {
    [
        ChildPhase::Idle,
        ChildPhase::Running,
        ChildPhase::AwaitingApproval,
        ChildPhase::Interrupted,
        ChildPhase::Finished,
    ]
    .into_iter()
    .find(|candidate| phase(*candidate) == raw)
    .ok_or(RegistryError::InvalidState)
}

fn permission_mode(raw: &str) -> Result<PermissionMode, RegistryError> {
    [
        PermissionMode::Ask,
        PermissionMode::Auto,
        PermissionMode::Yolo,
    ]
    .into_iter()
    .find(|mode| mode.label() == raw)
    .ok_or(RegistryError::InvalidState)
}

fn parse_outcome(raw: &str) -> Result<Outcome, RegistryError> {
    [
        Outcome::Completed,
        Outcome::Failed,
        Outcome::Cancelled,
        Outcome::Interrupted,
    ]
    .into_iter()
    .find(|candidate| outcome(*candidate) == raw)
    .ok_or(RegistryError::InvalidState)
}

fn fingerprint(raw: &str) -> Result<[u8; 32], RegistryError> {
    let bytes = raw.as_bytes();
    if bytes.len() != 64 {
        return Err(RegistryError::InvalidState);
    }
    let mut result = [0_u8; 32];
    for (index, byte) in result.iter_mut().enumerate() {
        let pair = std::str::from_utf8(&bytes[index * 2..index * 2 + 2])
            .map_err(|_| RegistryError::InvalidState)?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| RegistryError::InvalidState)?;
    }
    Ok(result)
}

fn object(value: &Value) -> Result<&Object, RegistryError> {
    value.as_object().ok_or(RegistryError::InvalidState)
}

fn exact_fields(source: &Object, allowed: &[&str]) -> Result<(), RegistryError> {
    if source.len() == allowed.len() && source.keys().all(|key| allowed.contains(&key.as_str())) {
        Ok(())
    } else {
        Err(RegistryError::InvalidState)
    }
}

fn string<'a>(source: &'a Object, name: &str) -> Result<&'a str, RegistryError> {
    source
        .get(name)
        .and_then(Value::as_str)
        .ok_or(RegistryError::InvalidState)
}

fn optional_string<'a>(source: &'a Object, name: &str) -> Result<Option<&'a str>, RegistryError> {
    match source.get(name) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        _ => Err(RegistryError::InvalidState),
    }
}

fn unsigned(source: &Object, name: &str) -> Result<u64, RegistryError> {
    source
        .get(name)
        .and_then(Value::as_u64)
        .ok_or(RegistryError::InvalidState)
}
