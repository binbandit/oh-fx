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
            input_schema: r#"{"type":"object"}"#,
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
                vec![ToolCall {
                    id: ToolCallId::new("call-1"),
                    name: "mutate".to_owned(),
                    arguments: r#"{"text":"x"}"#.to_owned(),
                }],
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
        })
    );
}
