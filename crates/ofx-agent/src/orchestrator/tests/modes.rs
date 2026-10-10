use ofx_contract::{ActiveMode, ModeRegistry, ModeSpec, PermissionMode, ToolPolicy};

use super::*;

static MODES: [ModeSpec; 2] = [
    ModeSpec {
        id: "full",
        name: "Full",
        description: "",
        permission_mode: PermissionMode::Ask,
        tool_policy: ToolPolicy::Full,
        tool_policy_denial_message: None,
    },
    ModeSpec {
        id: "inspect",
        name: "Inspect",
        description: "",
        permission_mode: PermissionMode::Ask,
        tool_policy: ToolPolicy::ReadOnly,
        tool_policy_denial_message: Some("Inspection mode blocks mutations."),
    },
];

static REGISTRY: ModeRegistry = ModeRegistry {
    default_mode_id: "inspect",
    modes: &MODES,
};

fn mode(id: &'static str) -> ActiveMode {
    ActiveMode {
        registry: &REGISTRY,
        id,
        read_only_tool_names: &["echo"],
    }
}

fn mutate_tool() -> Arc<dyn Tool> {
    Arc::new(EchoTool {
        spec: ToolSpec {
            name: "mutate".to_owned(),
            description: "Mutate the workspace.".to_owned(),
            input_schema: r#"{"type":"object"}"#.into(),
        },
        cleaned_up: Arc::new(AtomicBool::new(false)),
        meeting: Arc::new(tokio::sync::Barrier::new(2)),
    })
}

fn advertised(request: &SeenRequest) -> Vec<&str> {
    request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect()
}

#[tokio::test]
async fn a_read_only_mode_advertises_only_its_read_only_tools() {
    for (id, expected) in [("inspect", &["echo"][..]), ("full", &["echo", "mutate"])] {
        let provider = FakeProvider::new(vec![text_reply("Done.")]);
        let mut agent =
            new_agent(Arc::clone(&provider), vec![echo_tool(), mutate_tool()]).with_mode(mode(id));
        run(&mut agent, "go").await;
        assert_eq!(advertised(&provider.requests()[0]), expected, "{id}");
    }
}

#[tokio::test]
async fn a_call_the_mode_blocks_is_rejected_with_its_policy_message_and_never_runs() {
    let provider = FakeProvider::new(vec![
        Script::Reply(
            Vec::new(),
            completion(
                None,
                vec![ToolCall::new("call-1", "mutate", r#"{"text":"x"}"#)],
                FinishReason::ToolCalls,
            ),
        ),
        text_reply("Blocked."),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool(), mutate_tool()])
        .with_mode(mode("inspect"));
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.final_text, "Blocked.");
    assert_eq!(
        rejections(&events),
        [(
            "call-1",
            "mutate",
            r#"{"text":"x"}"#,
            ToolRejection::Invalid,
            None
        )]
    );
    assert!(finished(&events).is_empty());
    let messages = &provider.requests()[1].messages;
    assert_eq!(
        messages.last(),
        Some(&ChatMessage::Tool {
            call_id: ToolCallId::new("call-1"),
            tool_name: "mutate".to_owned(),
            content: r#"{"error":{"type":"tool_execution_failed","tool_name":"mutate","message":"Inspection mode blocks mutations.","suggestion":"Do not retry the same tool call unchanged. Adjust the request or use an allowed alternative."}}"#.to_owned(),
            status: ToolResultStatus::Failure,
            images: Vec::new(),
        })
    );
}

#[tokio::test]
async fn a_mode_works_from_the_specs_read_when_the_agent_was_built() {
    for id in ["full", "inspect"] {
        let reads = Arc::new(AtomicUsize::new(0));
        let echo: Arc<dyn Tool> = Arc::new(SpecReadOnce {
            inner: echo_tool(),
            reads: Arc::clone(&reads),
        });
        let provider = FakeProvider::new(vec![
            tool_reply(&[("call-1", r#"{"text":"found"}"#)]),
            text_reply("ok"),
        ]);
        let mut agent =
            new_agent(Arc::clone(&provider), vec![echo, mutate_tool()]).with_mode(mode(id));
        let (report, _) = run(&mut agent, "go").await;
        assert_eq!(report.outcome, TurnOutcome::Completed, "{id}");
        assert_eq!(reads.load(Ordering::SeqCst), 1, "{id}");
        assert_eq!(
            provider.requests()[1].messages[2..],
            [tool_message(
                "call-1",
                r#"echo {"text":"found"}"#,
                ToolResultStatus::Success
            )],
            "{id}"
        );
    }
}

#[tokio::test]
async fn provider_executed_tools_offer_guidance_only_when_the_mode_allows_them() {
    for (id, guidance) in [("full", Some("Search the web.")), ("inspect", None)] {
        let provider = FakeProvider::new(vec![text_reply("Done.")]);
        let tools = vec![provider_tool("search", "Search the web."), echo_tool()];
        let mut agent = new_agent(Arc::clone(&provider), tools).with_mode(mode(id));
        run(&mut agent, "go").await;
        let request = &provider.requests()[0];
        assert_eq!(advertised(request), ["echo"], "{id}");
        let expected: Vec<&str> = [SYSTEM_PROMPT]
            .into_iter()
            .chain(guidance)
            .chain([TURN_CONTEXT, RESPONSE_LANGUAGE_CONTROL])
            .collect();
        assert_eq!(request.instructions, expected, "{id}");
    }
}

#[tokio::test]
async fn work_tools_given_to_a_child_keep_its_modes_projection_and_denials() {
    let provider = FakeProvider::new(vec![
        Script::Reply(
            Vec::new(),
            completion(
                None,
                vec![ToolCall::new("call-1", "mutate", r#"{"text":"x"}"#)],
                FinishReason::ToolCalls,
            ),
        ),
        text_reply("Blocked."),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new()).with_mode(mode("inspect"));
    agent.replace_tools(vec![echo_tool(), mutate_tool()]);
    run(&mut agent, "go").await;
    let requests = provider.requests();
    assert_eq!(advertised(&requests[0]), ["echo"]);
    assert_eq!(
        requests[1].messages.last(),
        Some(&ChatMessage::Tool {
            call_id: ToolCallId::new("call-1"),
            tool_name: "mutate".to_owned(),
            content: r#"{"error":{"type":"tool_execution_failed","tool_name":"mutate","message":"Inspection mode blocks mutations.","suggestion":"Do not retry the same tool call unchanged. Adjust the request or use an allowed alternative."}}"#.to_owned(),
            status: ToolResultStatus::Failure,
            images: Vec::new(),
        })
    );
}

#[tokio::test]
async fn a_read_only_mode_leaves_selected_dynamic_tools_offered() {
    let provider = FakeProvider::new(vec![
        select_reply("select-1", r#"{"select":["echo","mutate"]}"#),
        text_reply("Done."),
    ]);
    let source = SwitchedTools::publishing(vec![echo_tool(), mutate_tool()]);
    let mut agent = new_agent(Arc::clone(&provider), vec![selector_tool()])
        .with_dynamic_tools(Arc::clone(&source) as _)
        .with_mode(ActiveMode {
            read_only_tool_names: &["select"],
            ..mode("inspect")
        });
    run(&mut agent, "go").await;
    assert_eq!(
        advertised(&provider.requests()[1]),
        ["select", "echo", "mutate"]
    );
}
