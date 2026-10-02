use ofx_contract::ModelProvider;
use ofx_gateway::CodexReviewTransport;
use ofx_permissions::{DEFAULT_REVIEW_TIMEOUT, Reviewer};

use super::*;

fn function_call_events(call_id: &str, name: &str, arguments: &str) -> Vec<String> {
    events(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":call_id,"name":name,"arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":arguments}),
        json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":arguments}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":5}}}),
    ])
}

fn reviewed_agent(fixture: &Fixture, provider: CodexProvider) -> Agent {
    let workspace = fixture.canonical_workspace();
    let provider: Arc<dyn ModelProvider> = Arc::new(provider);
    let reviewer = Reviewer::new(
        Arc::new(CodexReviewTransport::new(Arc::clone(&provider))),
        DEFAULT_REVIEW_TIMEOUT,
    );
    Agent::new(
        provider,
        ask_tools(&workspace),
        Arc::new(HostRuntimeContext::new(
            workspace.clone(),
            PermissionMode::Auto,
            false,
        )),
        Arc::new(PermissionPolicy::new(PermissionMode::Auto, workspace).with_reviewer(reviewer)),
        agent_config(MODEL, None, false),
    )
}

#[tokio::test]
async fn codex_logins_review_held_changes_with_the_catalog_reviewer_model() {
    let fixture = Fixture::new();
    fixture.write_session(FAR_FUTURE_MS, 0o600);
    let auth = FakeServer::start([]);
    let codex = FakeServer::start([
        Reply::sse(&function_call_events(
            "call_1",
            "write_file",
            r#"{"path":".git/config","content":"[core]\n"}"#,
        )),
        Reply::sse(&function_call_events(
            "review_1",
            "permission_decision",
            r#"{"decision":"clear"}"#,
        )),
        Reply::sse(&text_events("Configured.")),
    ]);
    let provider = fixture
        .provider(&auth, &codex)
        .await
        .expect("the saved login builds a provider");
    let mut agent = reviewed_agent(&fixture, provider);
    let (report, _) = run(&mut agent, "Set up the repository config").await;

    assert_eq!(report.outcome, TurnOutcome::Completed, "{report:?}");
    assert_eq!(
        fs::read_to_string(fixture.workspace.join(".git/config")).expect("read the config"),
        "[core]\n"
    );
    let requests = codex.requests();
    assert_eq!(requests.len(), 3);
    assert_codex_request(&requests[1], SAVED_TOKEN);
    let review = requests[1].json();
    assert_eq!(review["model"], "gpt-5.6-luna");
    assert_eq!(review["tool_choice"], "required");
    assert_eq!(review["tools"].as_array().expect("tools").len(), 1);
    assert_eq!(review["tools"][0]["name"], "permission_decision");
    assert!(review.get("reasoning").is_none());
    let instructions = review["instructions"].as_str().expect("instructions");
    assert!(instructions.starts_with("<permission_review>\n"));
    assert!(instructions.contains("action: prepared_file_mutation\ntool: write_file\npath: .git/config\npreimage: absent\nadditions: 1\ndeletions: 0\nreview[addition]: [core]\n"));
    let input = review["input"].as_array().expect("input");
    assert_eq!(
        input[0],
        json!({"role":"user","content":[{"type":"input_text","text":"review_context_kind: normal\n"}]})
    );
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["call_id"], "call_1");
    assert_eq!(
        input[2],
        json!({"type":"function_call_output","call_id":"call_1","output":"Tool call has not executed; it is pending permission review."})
    );
    assert_eq!(report.usage.input_tokens, Some(40));
}
