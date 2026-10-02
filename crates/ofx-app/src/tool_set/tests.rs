use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ofx_agent::{Agent, AgentConfig, Approvals, RuntimeContext};
use ofx_contract::{
    ApprovalDecision, ApprovalRequest, ApprovalScope, BoxFuture, ChatMessage, CommandRequest,
    Completion, FinishReason, ModelProvider, ModelRequest, PathAccess, PermissionMode,
    ProviderError, StreamSink, ToolCall, ToolCallId, ToolResultStatus, UiEvent, Usage,
    tool_permission_denied_json,
};
use ofx_exec::{ManagedExecutions, SessionSupervisor};
use ofx_permissions::PermissionPolicy;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::ask_tools;

const OUTSIDE_THE_APPROVED_TREE: &str = r#"{"error":{"type":"tool_execution_failed","tool_name":"read_file","message":"read_file failed","details":{"field":"path","path":"../link/data.txt","error":"PathOutsideWorkspace"},"suggestion":"Run glob_files to discover matching paths, or check the path relative to the workspace."}}"#;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir(&workspace).unwrap();
        for (name, content) in [
            ("approved/data.txt", "approved data\n"),
            ("unapproved/data.txt", "unapproved data\n"),
            ("unapproved/sibling.txt", "unapproved sibling\n"),
        ] {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        symlink(root.join("approved"), root.join("link")).unwrap();
        Self {
            _temp: temp,
            root,
            workspace,
        }
    }

    fn point_link_at_unapproved(&self) {
        fs::remove_file(self.root.join("link")).unwrap();
        symlink(self.root.join("unapproved"), self.root.join("link")).unwrap();
    }
}

#[derive(Default)]
struct ScriptedProvider {
    calls: Mutex<VecDeque<ToolCall>>,
}

impl ModelProvider for ScriptedProvider {
    fn stream<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        _sink: &'a mut dyn StreamSink,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        let answered = matches!(request.messages.last(), Some(ChatMessage::Tool { .. }));
        let call = if answered {
            None
        } else {
            self.calls.lock().unwrap().pop_front()
        };
        let (content, tool_calls, finish_reason) = match call {
            Some(call) => (None, vec![call], FinishReason::ToolCalls),
            None => (Some("done".to_owned()), Vec::new(), FinishReason::Stop),
        };
        Box::pin(async move {
            Ok(Completion {
                content,
                tool_calls,
                finish_reason,
                usage: Usage::default(),
                provider_replay: None,
            })
        })
    }
}

struct NoContext;

impl RuntimeContext for NoContext {
    fn runtime_context(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
}

struct Session {
    provider: Arc<ScriptedProvider>,
    approvals: Approvals,
    agent: Agent,
}

#[derive(Debug)]
struct Outcome {
    requests: Vec<ApprovalRequest>,
    status: ToolResultStatus,
    content: String,
}

impl Session {
    fn new(workspace: &Path) -> Self {
        let executions = ManagedExecutions::new(SessionSupervisor::new("/nonexistent"));
        let provider = Arc::new(ScriptedProvider::default());
        let approvals = Approvals::default();
        let agent = Agent::new(
            Arc::clone(&provider) as Arc<dyn ModelProvider>,
            ask_tools(workspace, &executions, None, PermissionMode::Ask),
            Arc::new(NoContext),
            Arc::new(PermissionPolicy::new(PermissionMode::Ask, workspace)),
            AgentConfig {
                model: "test-model".to_owned(),
                system_prompt: String::new(),
                max_output_tokens: None,
                step_limit: 0,
                reasoning_effort: None,
                fast_mode: false,
            },
        )
        .with_approvals(approvals.clone());
        Self {
            provider,
            approvals,
            agent,
        }
    }

    async fn call(
        &mut self,
        name: &str,
        arguments: &str,
        mut decide: impl FnMut(&ApprovalRequest) -> ApprovalDecision + Send,
    ) -> Outcome {
        self.provider.calls.lock().unwrap().push_back(ToolCall {
            id: ToolCallId::new("call-1"),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        });
        let approvals = self.approvals.clone();
        let mut requests = Vec::new();
        let mut finished = None;
        self.agent
            .run_turn(
                "go",
                &mut |event| match event {
                    UiEvent::ApprovalRequested { request, .. } => {
                        assert!(approvals.resolve(request.id, decide(&request)));
                        requests.push(request);
                    }
                    UiEvent::ToolFinished {
                        status, content, ..
                    } => finished = Some((status, content)),
                    _ => {}
                },
                &CancellationToken::new(),
            )
            .await;
        let (status, content) = finished.expect("the call settles");
        Outcome {
            requests,
            status,
            content,
        }
    }
}

fn unasked(request: &ApprovalRequest) -> ApprovalDecision {
    panic!("no approval is needed: {request:?}")
}

fn deny(_: &ApprovalRequest) -> ApprovalDecision {
    ApprovalDecision::Deny
}

impl Outcome {
    fn denied_request(self, tool_name: &str) -> ApprovalRequest {
        assert_eq!(
            (self.status, self.content.as_str()),
            (
                ToolResultStatus::Failure,
                tool_permission_denied_json(tool_name).as_str()
            )
        );
        let [request] = <[ApprovalRequest; 1]>::try_from(self.requests).unwrap();
        request
    }
}

#[tokio::test]
async fn an_approved_read_stays_in_the_tree_it_resolved_to_when_the_prompt_opened() {
    for decision in [ApprovalDecision::Once, ApprovalDecision::Always] {
        let fixture = Fixture::new();
        let mut session = Session::new(&fixture.workspace);
        let swapped = session
            .call("read_file", r#"{"path":"../link/data.txt"}"#, |_| {
                fixture.point_link_at_unapproved();
                decision
            })
            .await;
        assert_eq!(swapped.requests.len(), 1, "{decision:?}");
        assert!(
            !swapped.content.contains("unapproved"),
            "{decision:?} {swapped:?}"
        );
        assert_eq!(
            (swapped.status, swapped.content.as_str()),
            (ToolResultStatus::Failure, OUTSIDE_THE_APPROVED_TREE),
            "{decision:?}"
        );
    }
}

#[tokio::test]
async fn always_remembers_the_tree_shown_and_not_the_one_a_moved_link_points_at() {
    let fixture = Fixture::new();
    let mut session = Session::new(&fixture.workspace);
    session
        .call("read_file", r#"{"path":"../link/data.txt"}"#, |_| {
            fixture.point_link_at_unapproved();
            ApprovalDecision::Always
        })
        .await;

    let sibling = session
        .call(
            "read_file",
            r#"{"path":"../unapproved/sibling.txt"}"#,
            |_| ApprovalDecision::Deny,
        )
        .await;
    assert_eq!(sibling.requests.len(), 1, "{sibling:?}");
    assert_eq!(
        (sibling.status, sibling.content),
        (
            ToolResultStatus::Failure,
            tool_permission_denied_json("read_file")
        )
    );

    let approved = session
        .call("read_file", r#"{"path":"../approved/data.txt"}"#, unasked)
        .await;
    assert_eq!(approved.status, ToolResultStatus::Success, "{approved:?}");
    assert!(
        approved
            .content
            .ends_with("<content>\n1\tapproved data\n</content>"),
        "{approved:?}"
    );
}

#[tokio::test]
async fn shell_requests_carry_the_whole_command_its_directory_and_any_input() {
    let fixture = Fixture::new();
    let mut session = Session::new(&fixture.workspace);
    let command = format!("echo {} && rm -rf /private", "a".repeat(160));
    let arguments = serde_json::json!({
        "request": {"action": "run", "command": command, "cwd": "../approved"}
    });
    let run = session
        .call("shell", &arguments.to_string(), deny)
        .await
        .denied_request("shell");
    assert!(!run.title.contains("rm -rf /private"), "{}", run.title);
    assert_eq!(
        run.command,
        Some(CommandRequest::Run {
            command,
            cwd: fixture.root.join("approved"),
            terminal: false,
        })
    );

    let input = session
        .call(
            "shell",
            r#"{"request":{"action":"interact","session_id":"shell-1","chars":"yes\n"}}"#,
            deny,
        )
        .await
        .denied_request("shell");
    assert_eq!(input.title, "Sending input to session shell-1");
    assert_eq!(
        input.command,
        Some(CommandRequest::SendInput {
            input: "yes\n".to_owned()
        })
    );
}

#[tokio::test]
async fn searches_of_different_external_roots_ask_with_roots_a_host_can_tell_apart() {
    let fixture = Fixture::new();
    let mut session = Session::new(&fixture.workspace);
    for name in ["glob_files", "grep_files"] {
        let mut titles = Vec::new();
        for tree in ["approved", "unapproved"] {
            let arguments = format!(r#"{{"pattern":"data","path":"../{tree}"}}"#);
            let request = session
                .call(name, &arguments, deny)
                .await
                .denied_request(name);
            let root = fixture.root.join(tree);
            assert_eq!(
                request.scope,
                ApprovalScope {
                    target: Some(root.clone()),
                    access: PathAccess::Within(root),
                },
                "{name}"
            );
            assert_eq!(request.tool_arguments_preview, arguments, "{name}");
            titles.push(request.title);
        }
        assert_eq!(titles[0], titles[1], "{name}");
    }
}
