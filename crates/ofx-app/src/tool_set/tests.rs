use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ofx_agent::{Agent, AgentConfig, Approvals, RuntimeContext};
use ofx_contract::{
    Admission, ApplicableTarget, ApprovalDecision, ApprovalRequest, ApprovalScope, BoxFuture,
    ChatMessage, CommandProfile, CommandRequest, Completion, FileMutation, FinishReason,
    GatedAction, ModelProvider, ModelRequest, PathAccess, PermissionGate, PermissionMode,
    ProviderError, SessionGrant, StreamSink, ToolCall, ToolCallId, ToolResultStatus, UiEvent,
    Usage, tool_permission_denied_json,
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
        Self::with_gate(
            workspace,
            Arc::new(PermissionPolicy::new(PermissionMode::Ask, workspace)),
        )
    }

    fn with_gate(workspace: &Path, gate: Arc<dyn PermissionGate>) -> Self {
        let executions = ManagedExecutions::new(SessionSupervisor::new("/nonexistent"));
        let provider = Arc::new(ScriptedProvider::default());
        let approvals = Approvals::default();
        let agent = Agent::new(
            Arc::clone(&provider) as Arc<dyn ModelProvider>,
            ask_tools(workspace, &executions, None, PermissionMode::Ask),
            Arc::new(NoContext),
            gate,
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
            command: command.clone(),
            cwd: fixture.root.join("approved"),
            profile: CommandProfile::User,
            shell: None,
            terminal: false,
        })
    );
    assert_eq!(
        run.scope.always,
        Some(SessionGrant::Command {
            command,
            profile: CommandProfile::User,
            shell: None,
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
    assert_eq!(input.scope.always, None);
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
            let offered = if name == "glob_files" {
                SessionGrant::GlobsUnder(root.clone())
            } else {
                SessionGrant::GrepsUnder(root.clone())
            };
            assert_eq!(
                request.scope,
                ApprovalScope {
                    target: Some(root.clone()),
                    access: PathAccess::Within(root),
                    always: Some(offered),
                },
                "{name}"
            );
            assert_eq!(request.tool_arguments_preview, arguments, "{name}");
            titles.push(request.title);
        }
        assert_eq!(titles[0], titles[1], "{name}");
    }
}

fn always(_: &ApprovalRequest) -> ApprovalDecision {
    ApprovalDecision::Always
}

fn write(path: &str) -> String {
    serde_json::json!({"path": path, "content": "written\n"}).to_string()
}

fn run(mut request: serde_json::Value) -> String {
    request["action"] = "run".into();
    request["command"] = "echo granted".into();
    serde_json::json!({ "request": request }).to_string()
}

#[tokio::test]
async fn always_on_a_workspace_file_change_offers_and_grants_workspace_file_access_only() {
    let fixture = Fixture::new();
    let mut session = Session::new(&fixture.workspace);
    let first = session
        .call("write_file", &write("notes.txt"), always)
        .await;
    let [request] = <[ApprovalRequest; 1]>::try_from(first.requests).unwrap();
    assert_eq!(
        request.scope,
        ApprovalScope {
            target: None,
            access: PathAccess::WorkspaceOrExternal,
            always: Some(SessionGrant::WorkspaceFiles),
        }
    );
    assert_eq!(first.status, ToolResultStatus::Success);

    let nested = session
        .call("write_file", &write("src/deeper/lib.rs"), unasked)
        .await;
    assert_eq!(nested.status, ToolResultStatus::Success, "{nested:?}");
    session
        .call("write_file", &write("../approved/new.txt"), deny)
        .await
        .denied_request("write_file");
    session
        .call("read_file", r#"{"path":"../approved/data.txt"}"#, deny)
        .await
        .denied_request("read_file");
}

#[tokio::test]
async fn always_on_an_external_file_change_grants_the_tree_it_showed_before_the_prompt() {
    let fixture = Fixture::new();
    let mut session = Session::new(&fixture.workspace);
    let first = session
        .call("write_file", &write("../link/a/b/new.txt"), |_| {
            fs::create_dir_all(fixture.root.join("approved/a/b")).unwrap();
            fixture.point_link_at_unapproved();
            ApprovalDecision::Always
        })
        .await;
    let [request] = <[ApprovalRequest; 1]>::try_from(first.requests).unwrap();
    assert_eq!(
        request.scope.always,
        Some(SessionGrant::FileChangesUnder(
            fixture.root.join("approved")
        ))
    );

    let sibling = session
        .call("write_file", &write("../approved/x.txt"), unasked)
        .await;
    assert_eq!(sibling.status, ToolResultStatus::Success, "{sibling:?}");
    assert_eq!(
        fs::read_to_string(fixture.root.join("approved/x.txt")).unwrap(),
        "written\n"
    );
    for outside in ["../link/new.txt", "../unapproved/new.txt", "notes.txt"] {
        session
            .call("write_file", &write(outside), deny)
            .await
            .denied_request("write_file");
    }
    session
        .call("read_file", r#"{"path":"../approved/data.txt"}"#, deny)
        .await
        .denied_request("read_file");
}

#[tokio::test]
async fn a_remembered_command_asks_again_under_another_profile_or_terminal_mode() {
    let fixture = Fixture::new();
    let mut session = Session::new(&fixture.workspace);
    let clean = run(serde_json::json!({"profile": "clean"}));
    let first = session.call("shell", &clean, always).await;
    let [request] = <[ApprovalRequest; 1]>::try_from(first.requests).unwrap();
    let identity = |profile, terminal| {
        (
            Some(CommandRequest::Run {
                command: "echo granted".to_owned(),
                cwd: fixture.workspace.clone(),
                profile,
                shell: None,
                terminal,
            }),
            Some(SessionGrant::Command {
                command: "echo granted".to_owned(),
                profile,
                shell: None,
                terminal,
            }),
        )
    };
    assert_eq!(
        (request.command, request.scope.always),
        identity(CommandProfile::Clean, false)
    );
    let again = session.call("shell", &clean, unasked).await;
    assert!(again.requests.is_empty(), "{again:?}");
    for (changed, profile, terminal) in [
        (
            serde_json::json!({"profile": "user"}),
            CommandProfile::User,
            false,
        ),
        (serde_json::json!({}), CommandProfile::User, false),
        (
            serde_json::json!({"profile": "clean", "tty": true}),
            CommandProfile::Clean,
            true,
        ),
    ] {
        let asked = session
            .call("shell", &run(changed), deny)
            .await
            .denied_request("shell");
        assert_eq!(
            (asked.command, asked.scope.always),
            identity(profile, terminal)
        );
    }
}

#[tokio::test]
async fn a_terminal_run_that_names_its_shell_asks_with_that_shell_and_binds_it() {
    let fixture = Fixture::new();
    let mut session = Session::new(&fixture.workspace);
    let identity = |profile, shell: Option<&str>| {
        let shell = shell.map(PathBuf::from);
        (
            Some(CommandRequest::Run {
                command: "echo granted".to_owned(),
                cwd: fixture.workspace.clone(),
                profile,
                shell: shell.clone(),
                terminal: true,
            }),
            Some(SessionGrant::Command {
                command: "echo granted".to_owned(),
                profile,
                shell,
                terminal: true,
            }),
        )
    };
    let login = session
        .call("shell", &run(serde_json::json!({"tty": true})), always)
        .await;
    let [request] = <[ApprovalRequest; 1]>::try_from(login.requests).unwrap();
    assert_eq!(
        (request.command, request.scope.always),
        identity(CommandProfile::User, None)
    );
    for (clean_start, profile) in [(false, CommandProfile::User), (true, CommandProfile::Clean)] {
        let named = run(serde_json::json!({
            "tty": true,
            "shell": {"kind": "executable", "path": "/tmp/other-shell", "clean_start": clean_start}
        }));
        let asked = session
            .call("shell", &named, deny)
            .await
            .denied_request("shell");
        assert_eq!(
            (asked.command, asked.scope.always),
            identity(profile, Some("/tmp/other-shell"))
        );
    }
}

#[tokio::test]
async fn a_run_whose_working_directory_does_not_exist_offers_no_grant() {
    let fixture = Fixture::new();
    let mut session = Session::new(&fixture.workspace);
    let missing = session
        .call(
            "shell",
            &run(serde_json::json!({"cwd": "../missing"})),
            always,
        )
        .await;
    let [request] = <[ApprovalRequest; 1]>::try_from(missing.requests).unwrap();
    assert_eq!(request.scope.always, None);
    assert!(
        missing.content.starts_with("shell run cwd is invalid: "),
        "{}",
        missing.content
    );
    session
        .call("shell", &run(serde_json::json!({})), deny)
        .await
        .denied_request("shell");
}

struct WrappedPolicy {
    policy: PermissionPolicy,
    vanishing: Option<PathBuf>,
}

impl WrappedPolicy {
    fn new(workspace: &Path, vanishing: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            policy: PermissionPolicy::new(PermissionMode::Ask, workspace),
            vanishing,
        })
    }
}

impl PermissionGate for WrappedPolicy {
    fn admit(&self, call: &ToolCall) -> Admission {
        self.policy.admit(call)
    }

    fn applicable_target(&self, call: &ToolCall) -> Option<ApplicableTarget> {
        self.policy.applicable_target(call)
    }

    fn admit_file_mutation(&self, mutation: &FileMutation) -> Admission {
        self.policy.admit_file_mutation(mutation)
    }

    fn admit_command(&self, request: &CommandRequest) -> Admission {
        self.policy.admit_command(request)
    }

    fn approval_scope(&self, action: GatedAction<'_>) -> ApprovalScope {
        if let Some(target) = &self.vanishing {
            fs::remove_file(target).unwrap();
        }
        self.policy.approval_scope(action)
    }

    fn remember_approval(&self, grant: &SessionGrant) {
        self.policy.remember_approval(grant);
    }

    fn forget_approvals(&self) {
        self.policy.forget_approvals();
    }
}

#[tokio::test]
async fn a_target_that_vanishes_before_the_prompt_confines_the_approved_call_to_the_workspace() {
    let fixture = Fixture::new();
    let target = fixture.root.join("approved/data.txt");
    let mut session = Session::with_gate(
        &fixture.workspace,
        WrappedPolicy::new(&fixture.workspace, Some(target.clone())),
    );
    let read = session
        .call("read_file", r#"{"path":"../approved/data.txt"}"#, |_| {
            fs::write(&target, "approved data\n").unwrap();
            ApprovalDecision::Once
        })
        .await;
    assert_eq!(
        read.requests[0].scope,
        ApprovalScope {
            target: None,
            access: PathAccess::WorkspaceOnly,
            always: None,
        }
    );
    assert_eq!(read.status, ToolResultStatus::Failure, "{read:?}");
    assert!(read.content.contains("PathOutsideWorkspace"), "{read:?}");
}

#[tokio::test]
async fn clearing_the_conversation_forgets_the_approvals_remembered_in_it() {
    let fixture = Fixture::new();
    for mut session in [
        Session::new(&fixture.workspace),
        Session::with_gate(
            &fixture.workspace,
            WrappedPolicy::new(&fixture.workspace, None),
        ),
    ] {
        let read = r#"{"path":"../approved/data.txt"}"#;
        session.call("read_file", read, always).await;
        session.call("read_file", read, unasked).await;
        session.agent.clear_history();
        session
            .call("read_file", read, deny)
            .await
            .denied_request("read_file");
    }
}
