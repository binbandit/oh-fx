use std::sync::Mutex;

use ofx_contract::{PathAccess, ToolCallId, ToolResultStatus};
use tokio_util::sync::CancellationToken;

use super::*;

const UPSTREAM_DESCRIPTION: &str = "Delegate work and receive one terminal child result. Use run for one temporary child and one task. Use message with a stable name to create or continue a persistent conversation in this parent session. A plain message to a working child queues feedback for its next safe boundary without cancelling its current tool. A delivery receipt is not the child's final result; that result arrives separately. Optional instructions replace only that child's system overlay between turns; fx preserves its trusted base prompt. Optional model and effort apply only when a child is created and are rejected for an existing child. fx owns timing, worker identities, cancellation, permissions, persistence, and cleanup.";
const UPSTREAM_INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"request":{"oneOf":[{"type":"object","properties":{"action":{"type":"string","enum":["run"]},"task":{"type":"string","minLength":1,"maxLength":65536,"description":"One complete task for a temporary child. The child accepts no follow-up."},"model":{"type":"string","minLength":1,"maxLength":256,"description":"Optional model for this child, as a catalog model ID such as openai/gpt-5.6-terra. Unambiguous partial names resolve to catalog IDs; unknown or ambiguous names are rejected with candidate IDs. Inherits the parent's model when omitted."},"effort":{"type":"string","minLength":1,"maxLength":64,"description":"Optional reasoning effort for this child. Inherits the parent's effort when omitted."}},"additionalProperties":false,"required":["action","task"]},{"type":"object","properties":{"action":{"type":"string","enum":["message"]},"agent":{"type":"string","minLength":1,"maxLength":64,"description":"Stable lowercase name for one persistent conversation in this parent session. A new valid name creates it; later calls continue it."},"instructions":{"type":"string","minLength":1,"maxLength":65536,"description":"Optional persistent instructions for this child. Replaces its child-specific system overlay before this message when idle; rejected while the child is working. Omit to preserve the overlay or send live feedback. Cannot replace fx's trusted base prompt or widen authority."},"message":{"type":"string","minLength":1,"maxLength":65536,"description":"Message for that named agent: creates it on first use, continues an idle conversation, or queues feedback for a working child. Do not resend merely to poll for completion."},"model":{"type":"string","minLength":1,"maxLength":256,"description":"Optional model applied when this message creates the child, as a catalog model ID such as openai/gpt-5.6-terra. Unambiguous partial names resolve to catalog IDs; unknown or ambiguous names are rejected with candidate IDs. Inherits the parent's model when omitted. Rejected when the named child already exists."},"effort":{"type":"string","minLength":1,"maxLength":64,"description":"Optional reasoning effort applied when this message creates the child. Inherits the parent's effort when omitted. Rejected when the named child already exists."}},"additionalProperties":false,"required":["action","agent","message"]}]}},"additionalProperties":false,"required":["request"]}"#;

#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(SubagentRequest, String)>>,
}

impl SubagentProvider for Recorder {
    fn execute(
        &self,
        request: SubagentRequest,
        context: ToolContext,
    ) -> BoxFuture<'static, ToolOutput> {
        self.calls
            .lock()
            .unwrap()
            .push((request, context.call_id.as_str().to_owned()));
        Box::pin(async { ToolOutput::success(r#"{"ok":true}"#) })
    }
}

fn tool() -> (SubagentTool, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    (
        SubagentTool::new(Arc::clone(&recorder) as Arc<dyn SubagentProvider>),
        recorder,
    )
}

fn context(call_id: &str) -> ToolContext {
    ToolContext::new(
        ToolCallId::new(call_id),
        CancellationToken::new(),
        PathAccess::WorkspaceOnly,
    )
}

fn expect_request(arguments: &str) -> SubagentRequest {
    decode(arguments).unwrap_or_else(|code| panic!("{arguments}: {code}"))
}

fn expect_failure(arguments: &str, code: &str) {
    assert_eq!(decode(arguments), Err(code), "{arguments}");
    let (tool, _) = tool();
    let prepared = tool.prepare(arguments).unwrap();
    let refusal = prepared.refusal().expect("a refused call");
    assert_eq!(refusal.status, ToolResultStatus::Failure);
    assert!(
        refusal
            .content
            .contains(&format!("\"error_code\":\"{code}\"")),
        "{}",
        refusal.content
    );
}

#[test]
fn call_executes_a_validated_managed_request_through_the_provider() {
    let (tool, recorder) = tool();
    let prepared = tool
        .prepare(r#"{"request":{"action":"run","task":"review this"}}"#)
        .unwrap();
    assert!(prepared.refusal().is_none());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let output = runtime.block_on(prepared.execute(context("call-1")));
    assert_eq!(output, ToolOutput::success(r#"{"ok":true}"#));
    let calls = recorder.calls.lock().unwrap();
    let [(request, call_id)] = calls.as_slice() else {
        panic!("one provider call: {calls:?}");
    };
    assert_eq!(request.action(), ofx_contract::SubagentAction::Run);
    assert_eq!(request.content(), "review this");
    assert_eq!(call_id, "call-1");
}

#[test]
fn decode_accepts_only_delegation_intents() {
    for (arguments, action) in [
        (
            r#"{"request":{"action":"run","task":"do it"}}"#,
            ofx_contract::SubagentAction::Run,
        ),
        (
            r#"{"request":{"action":"message","agent":"reviewer","message":"next"}}"#,
            ofx_contract::SubagentAction::Message,
        ),
        (
            r#"{"request":{"action":"message","agent":"reviewer","instructions":"Review strictly.","message":"next"}}"#,
            ofx_contract::SubagentAction::Message,
        ),
        (
            r#"{"request":{"action":"run","task":"do it","model":"gpt-5.6-sol-fast","effort":"medium"}}"#,
            ofx_contract::SubagentAction::Run,
        ),
        (
            r#"{"request":{"action":"message","agent":"reviewer","message":"next","model":"gpt-5.6-sol-fast"}}"#,
            ofx_contract::SubagentAction::Message,
        ),
        (
            r#"{"request":{"action":"message","agent":"reviewer","message":"next","effort":"high"}}"#,
            ofx_contract::SubagentAction::Message,
        ),
        (
            r#"{"action":"run","task":"flat requests decode too"}"#,
            ofx_contract::SubagentAction::Run,
        ),
    ] {
        assert_eq!(expect_request(arguments).action(), action, "{arguments}");
    }
    for arguments in [
        r#"{"request":{"action":"wait","child_id":"01J00000000000000000000000"}}"#,
        r#"{"request":{"action":"stop","child_id":"01J00000000000000000000000"}}"#,
        r#"{"request":{"action":"cancel","child_id":"01J00000000000000000000000"}}"#,
    ] {
        expect_failure(arguments, "invalid_enum");
    }
}

#[test]
fn decode_rejects_invalid_creation_overrides() {
    expect_failure(
        r#"{"request":{"action":"run","task":"do it","model":""}}"#,
        "invalid_model",
    );
    expect_failure(
        r#"{"request":{"action":"run","task":"do it","effort":"not an effort!"}}"#,
        "invalid_effort",
    );
    expect_failure(
        r#"{"request":{"action":"message","agent":"reviewer","message":"next","model":7}}"#,
        "invalid_field_type",
    );
    expect_failure(
        r#"{"request":{"action":"run","task":"do it","provider":"gateway"}}"#,
        "unknown_field",
    );
}

#[test]
fn decode_rejects_manager_input_cross_action_fields_and_unknown_actions() {
    expect_failure(
        r#"{"command":{"create":{"name":"worker"}}}"#,
        "missing_field",
    );
    expect_failure(
        r#"{"request":{"action":"wait","child_id":"01J00000000000000000000000","task":"wrong"}}"#,
        "invalid_enum",
    );
    expect_failure(
        r#"{"request":{"action":"inspect","child_id":"01J00000000000000000000000"}}"#,
        "invalid_enum",
    );
    expect_failure(r#"{"request":{"action":"wait"}}"#, "invalid_enum");
    expect_failure(r#"{"request":null}"#, "invalid_field_type");
}

#[test]
fn decode_reports_upstream_codes_for_every_shape_and_validation_failure() {
    for (arguments, code) in [
        ("{", "invalid_json"),
        ("[]", "invalid_field_type"),
        (
            r#"{"request":{"action":"run","task":"x"},"extra":1}"#,
            "unknown_field",
        ),
        (r#"{"request":{"action":7}}"#, "invalid_field_type"),
        (
            r#"{"request":{"action":"run","task":null}}"#,
            "invalid_field_type",
        ),
        (
            r#"{"request":{"action":"run","task":"x","agent":"reviewer"}}"#,
            "unknown_field",
        ),
        (r#"{"request":{"action":"run","task":""}}"#, "invalid_task"),
        (
            r#"{"request":{"action":"message","agent":"Reviewer","message":"x"}}"#,
            "invalid_agent",
        ),
        (
            r#"{"request":{"action":"message","agent":"reviewer","instructions":"","message":"x"}}"#,
            "invalid_instructions",
        ),
        (
            r#"{"request":{"action":"message","agent":"reviewer","message":""}}"#,
            "invalid_message",
        ),
        (
            r#"{"request":{"action":"message","agent":"reviewer"}}"#,
            "missing_field",
        ),
    ] {
        expect_failure(arguments, code);
    }
    assert_eq!(
        tool()
            .0
            .prepare(r#"{"request":{"action":"run","task":""}}"#)
            .unwrap()
            .refusal()
            .unwrap()
            .content,
        r#"{"ok":false,"result":null,"error_code":"invalid_task"}"#
    );
}

#[test]
fn built_in_subagent_owns_product_metadata_and_schema() {
    let (tool, _) = tool();
    let spec = tool.spec();
    assert_eq!(spec.name, "subagent");
    assert_eq!(spec.description, UPSTREAM_DESCRIPTION);
    assert_eq!(spec.input_schema, UPSTREAM_INPUT_SCHEMA);
    assert!(spec.description.contains("one temporary child"));
    assert!(spec.description.contains("stable name"));
    assert!(!spec.input_schema.contains("subagent_type"));
    let described = tool
        .prepare(r#"{"request":{"action":"message","agent":"reviewer","message":"check auth"}}"#)
        .unwrap()
        .describe();
    assert_eq!(
        described,
        CallDescription {
            title: "reviewer working · check auth".to_owned(),
            label: None,
            activity: ToolActivity::Subagent,
            effect: ToolEffect::Mutating,
            concurrency: Concurrency::Parallel,
        }
    );
    let refused = tool
        .prepare(r#"{"request":{"action":"inspect"}}"#)
        .unwrap()
        .describe();
    assert_eq!(refused.title, "Managing subagent");
    assert_eq!(refused.effect, ToolEffect::None);
    assert_eq!(refused.activity, ToolActivity::Subagent);
}
