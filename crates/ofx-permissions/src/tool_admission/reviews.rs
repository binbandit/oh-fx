use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ofx_contract::{
    ChatMessage, CommandProfile, Completion, FileMutationState, FinishReason, ModelRequest,
    ReviewTransport, ReviewTransportOutcome,
};
use serde_json::Value;

use super::*;

#[derive(Default)]
struct Recording {
    bodies: Mutex<Vec<Value>>,
}

impl ReviewTransport for Recording {
    fn model<'a>(&'a self, source_model: &'a str) -> &'a str {
        source_model
    }

    fn max_output_tokens(&self, _model: &str) -> u32 {
        2048
    }

    fn request_body(&self, request: &ModelRequest<'_>) -> Option<String> {
        let user = request.messages.iter().find_map(|message| match message {
            ChatMessage::User { content, .. } => Some(content.clone()),
            _ => None,
        });
        Some(serde_json::json!({"user": user, "instructions": request.instructions}).to_string())
    }

    fn send<'a>(
        &'a self,
        _request: &'a ModelRequest<'a>,
        body: String,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ReviewTransportOutcome> {
        self.bodies
            .lock()
            .unwrap()
            .push(serde_json::from_str(&body).unwrap());
        Box::pin(async {
            ReviewTransportOutcome::Completion(Completion {
                content: None,
                tool_calls: vec![ToolCall::new(
                    "review",
                    "permission_decision",
                    r#"{"decision":"clear"}"#,
                )],
                finish_reason: FinishReason::ToolCalls,
                usage: Usage::default(),
                provider_replay: None,
            })
        })
    }
}

fn reviewed_policy() -> (PermissionPolicy, Arc<Recording>) {
    let transport = Arc::new(Recording::default());
    let policy = PermissionPolicy::new(PermissionMode::Auto, "/workspace")
        .with_reviewer(Reviewer::new(transport.clone(), Duration::from_secs(1)));
    (policy, transport)
}

fn shell_call() -> ToolCall {
    ToolCall::new(
        "call-1",
        "shell",
        r#"{"request":{"action":"run","command":"touch marker"}}"#,
    )
}

fn touch() -> CommandRequest {
    CommandRequest::Run {
        command: "touch marker".to_owned(),
        cwd: PathBuf::from("/workspace"),
        profile: CommandProfile::User,
        shell: Some(PathBuf::from("/bin/sh")),
        terminal: false,
        reload: false,
    }
}

fn request<'a>(
    batch: &'a [ToolCall],
    action: GatedAction<'a>,
    earlier: &'a [&'a str],
    compacted_turns: Option<usize>,
) -> ReviewRequest<'a> {
    ReviewRequest {
        model: "test/main",
        current_request: "now",
        earlier_requests: earlier,
        compacted_turns,
        turn: &[],
        held: &[],
        batch,
        call: &batch[0],
        action,
        file: None,
        schema: None,
        attempt_available: true,
    }
}

async fn verdict(policy: &PermissionPolicy, request: ReviewRequest<'_>) -> ReviewVerdict {
    policy
        .review(request, &CancellationToken::new())
        .await
        .expect("the review was not cancelled")
        .verdict
}

#[tokio::test]
async fn a_policy_without_a_reviewer_reports_it_unconfigured() {
    let batch = [shell_call()];
    let command = touch();
    let policy = PermissionPolicy::new(PermissionMode::Auto, "/workspace");
    assert_eq!(
        verdict(
            &policy,
            request(&batch, GatedAction::Command(&command), &[], None)
        )
        .await,
        ReviewVerdict::Unavailable(ReviewFailure::ReviewerUnconfigured)
    );
}

#[tokio::test]
async fn a_reviewer_set_on_a_shared_policy_reviews_the_next_held_call() {
    let batch = [shell_call()];
    let command = touch();
    let first = Arc::new(Recording::default());
    let policy = Arc::new(
        PermissionPolicy::new(PermissionMode::Auto, "/workspace")
            .with_reviewer(Reviewer::new(first.clone(), Duration::from_secs(1))),
    );
    let shared = Arc::clone(&policy);
    let second = Arc::new(Recording::default());
    shared.set_reviewer(Reviewer::new(second.clone(), Duration::from_secs(1)));
    assert_eq!(
        verdict(
            &policy,
            request(&batch, GatedAction::Command(&command), &[], None)
        )
        .await,
        ReviewVerdict::Clear
    );
    assert!(first.bodies.lock().unwrap().is_empty());
    assert_eq!(second.bodies.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_spent_reviewer_budget_and_an_unprepared_change_hold_without_a_request() {
    let (policy, transport) = reviewed_policy();
    let batch = [shell_call()];
    let command = touch();
    let mut spent = request(&batch, GatedAction::Command(&command), &[], None);
    spent.attempt_available = false;
    assert_eq!(
        verdict(&policy, spent).await,
        ReviewVerdict::Unavailable(ReviewFailure::TurnReviewBudgetExhausted)
    );
    let mutation = FileMutation {
        target: PathBuf::from("/outside/notes.txt"),
        state: FileMutationState::Unread,
    };
    assert_eq!(
        verdict(
            &policy,
            request(&batch, GatedAction::FileMutation(&mutation), &[], None)
        )
        .await,
        ReviewVerdict::EvidenceIncomplete
    );
    assert!(transport.bodies.lock().unwrap().is_empty());
}

#[tokio::test]
async fn contextual_reviews_carry_only_the_users_bounded_requests() {
    let (policy, transport) = reviewed_policy();
    let batch = [shell_call()];
    let command = touch();
    let earlier = ["first", "second"];
    for compacted in [None, Some(3)] {
        assert_eq!(
            verdict(
                &policy,
                request(&batch, GatedAction::Command(&command), &earlier, compacted)
            )
            .await,
            ReviewVerdict::Clear
        );
    }
    let bodies = transport.bodies.lock().unwrap();
    assert_eq!(
        bodies[0]["user"],
        "review_context_kind: contextual\ntrusted_root_context:\ncurrent_request: now\nfirst_root_user_request: first\nrecent_root_user_request: second\n"
    );
    assert_eq!(
        bodies[1]["user"],
        "review_context_kind: contextual\ntrusted_root_context:\ncurrent_request: now\nrecent_root_user_request: second\nrecent_root_user_request: first\nomitted_proven_root_user_turns: 3\n"
    );
    let instruction = bodies[0]["instructions"][0].as_str().unwrap();
    assert!(
        instruction.contains("target[target]: /workspace::@fx-terminal-env:user:7:/bin/sh::touch marker\naction: command\ncommand: touch marker\ncwd: /workspace\nbackground: false\n"),
        "{instruction}"
    );
}

#[tokio::test]
async fn an_mcp_tool_review_sends_its_exact_arguments_and_advertised_schema() {
    let (policy, transport) = reviewed_policy();
    let batch = [ToolCall::new(
        "call-1",
        "mcp_example_write",
        r#"{"path":"outside.txt","value":"exact"}"#,
    )];
    let schema = r#"{"type":"function","name":"mcp_example_write","description":"Write.","inputSchema":{"type":"object"}}"#;
    let mut reviewed = request(&batch, GatedAction::McpTool(&batch[0]), &[], None);
    reviewed.schema = Some(schema);
    assert_eq!(verdict(&policy, reviewed).await, ReviewVerdict::Clear);
    let unschematized = request(&batch, GatedAction::McpTool(&batch[0]), &[], None);
    assert_eq!(
        verdict(&policy, unschematized).await,
        ReviewVerdict::EvidenceIncomplete
    );
    let bodies = transport.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        bodies[0]["user"],
        "review_context_kind: contextual\ntrusted_root_context:\ncurrent_request: now\n"
    );
    let instruction = bodies[0]["instructions"][0].as_str().unwrap();
    assert!(
        instruction.contains(&format!(
            "target[target]: mcp_example_write\naction: tool\ntool: mcp_example_write\narguments_json: {{\"path\":\"outside.txt\",\"value\":\"exact\"}}\nschema_json: {schema}\naction_evidence_incomplete: false\n"
        )),
        "{instruction}"
    );
}

struct SwitchingMidReview {
    mode: LivePermissionMode,
}

impl ReviewTransport for SwitchingMidReview {
    fn model<'a>(&'a self, source_model: &'a str) -> &'a str {
        source_model
    }

    fn max_output_tokens(&self, _model: &str) -> u32 {
        2048
    }

    fn request_body(&self, _request: &ModelRequest<'_>) -> Option<String> {
        Some(String::new())
    }

    fn send<'a>(
        &'a self,
        _request: &'a ModelRequest<'a>,
        _body: String,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ReviewTransportOutcome> {
        self.mode.set(PermissionMode::Yolo);
        Box::pin(async {
            ReviewTransportOutcome::Completion(Completion {
                content: None,
                tool_calls: vec![ToolCall::new(
                    "review",
                    "permission_decision",
                    r#"{"decision":"caution","rationale":"Untrusted output."}"#.to_owned(),
                )],
                finish_reason: FinishReason::ToolCalls,
                usage: Usage::default(),
                provider_replay: None,
            })
        })
    }
}

#[tokio::test]
async fn a_review_in_flight_keeps_its_verdict_when_full_access_is_switched_on() {
    let mode = LivePermissionMode::from(PermissionMode::Auto);
    let transport = Arc::new(SwitchingMidReview { mode: mode.clone() });
    let policy = PermissionPolicy::new(mode.clone(), "/workspace")
        .with_reviewer(Reviewer::new(transport, Duration::from_secs(1)));
    let batch = [shell_call()];
    let command = touch();
    assert_eq!(policy.admit_command(&command), Admission::ReviewRequired);
    assert_eq!(
        verdict(
            &policy,
            request(&batch, GatedAction::Command(&command), &[], None)
        )
        .await,
        ReviewVerdict::Caution("Untrusted output.".to_owned())
    );
    assert_eq!(mode.get(), PermissionMode::Yolo);
    assert_eq!(
        policy.admit_command(&command),
        Admission::Allowed(PathAccess::WorkspaceOrExternal)
    );
}
