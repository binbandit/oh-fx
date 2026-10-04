use ofx_contract::ChildPhase;
use ofx_text::lowercase_hex;
use serde::Serialize;

use super::{ActiveWork, Child, Kind, Outcome, Registry};

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
            .map(ofx_contract::ModelFailureDiagnostic::as_str),
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
