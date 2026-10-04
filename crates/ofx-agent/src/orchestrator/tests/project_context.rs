use std::cell::Cell;
use std::collections::HashSet;

use ofx_contract::{TargetKind, ToolArgs, parse_tool_args_object};

use super::*;

const RULED: [&str; 3] = ["/w/a", "/w/b", "/w/b/file"];

#[derive(Default)]
struct World {
    log: Vec<String>,
    created: HashSet<String>,
    moved: HashSet<String>,
    removed: HashSet<String>,
    prepared: usize,
}

type SharedWorld = Arc<Mutex<World>>;

fn resolve(world: &SharedWorld, path: &str) -> Option<PathBuf> {
    let world = world.lock().unwrap();
    if world.removed.contains(path) {
        return None;
    }
    if world.moved.contains(path) {
        return Some(PathBuf::from(format!("{path}.moved")));
    }
    (!path.contains("missing") || world.created.contains(path)).then(|| PathBuf::from(path))
}

struct ScopedTool {
    spec: ToolSpec,
    world: SharedWorld,
}

struct ScopedCall {
    arguments: String,
    parsed: ToolArgs,
    world: SharedWorld,
    completed: bool,
    resolved: Option<PathBuf>,
    mutation: Option<FileMutation>,
    refusal: Option<ToolOutput>,
    projected: Cell<bool>,
}

impl ScopedCall {
    fn text(&self, key: &str) -> Option<&str> {
        self.parsed.optional_string(key)
    }

    fn has(&self, key: &str) -> bool {
        self.parsed.get(key).is_some()
    }

    fn writes(&self) -> bool {
        self.text("write").is_some()
    }

    fn mutates(&self) -> bool {
        self.has("mutation")
    }

    fn resolve_mutation(&mut self) {
        self.resolved = self
            .text("write")
            .and_then(|path| resolve(&self.world, path));
        self.mutation = self
            .resolved
            .clone()
            .filter(|_| !self.completed || !self.has("unreadable"))
            .map(|target| FileMutation {
                target,
                state: FileMutationState::Changes,
            });
    }
}

impl Tool for ScopedTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        self.world.lock().unwrap().prepared += 1;
        let parsed = parse_tool_args_object(arguments).unwrap();
        if parsed.get("invalid").is_some() {
            return Err(ToolOutput::failure("invalid arguments"));
        }
        let mut call = ScopedCall {
            arguments: arguments.to_owned(),
            parsed,
            world: Arc::clone(&self.world),
            completed: false,
            resolved: None,
            mutation: None,
            refusal: None,
            projected: Cell::new(false),
        };
        if call.has("refused") {
            call.refusal = Some(ToolOutput::failure("refused arguments"));
        }
        if call.mutates() {
            call.resolve_mutation();
        }
        Ok(Box::new(call))
    }
}

impl PreparedCall for ScopedCall {
    fn describe(&self) -> CallDescription {
        let writes = self.writes();
        CallDescription {
            title: format!("Scoping {}", self.arguments),
            label: None,
            activity: if writes {
                ToolActivity::Write
            } else if self.has("delegate") {
                ToolActivity::Subagent
            } else {
                ToolActivity::Read
            },
            effect: if self.has("inert") {
                ToolEffect::None
            } else if writes {
                ToolEffect::Irreversible
            } else {
                ToolEffect::ReadOnly
            },
            concurrency: if writes {
                Concurrency::Serial
            } else {
                Concurrency::Parallel
            },
        }
    }

    fn untargeted_label(&self) -> Option<ActionLabel> {
        self.writes().then(|| ActionLabel {
            active: "Writing",
            completed: "Wrote",
            target: "file".to_owned(),
        })
    }

    fn complete(&mut self) {
        self.completed = true;
        self.world
            .lock()
            .unwrap()
            .log
            .push(format!("complete {}", self.arguments));
        if self.mutates() {
            self.resolve_mutation();
        }
    }

    fn applicable_target(&self) -> Option<ApplicableTarget> {
        assert!(!self.has("target_panic"), "target panicked");
        let checked_again = self.projected.replace(true);
        assert!(
            !checked_again || !self.has("recheck_panic"),
            "target panicked when checked again"
        );
        let path = if self.mutates() {
            self.resolved.clone().filter(|_| !self.has("untargeted"))?
        } else {
            resolve(&self.world, self.text("write")?)?
        };
        Some(ApplicableTarget {
            path,
            kind: TargetKind::File,
        })
    }

    fn file_mutation(&self) -> Option<&FileMutation> {
        self.mutation.as_ref()
    }

    fn refusal(&self) -> Option<&ToolOutput> {
        self.refusal.as_ref()
    }

    fn execute(self: Box<Self>, _context: ToolContext) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let mut world = self.world.lock().unwrap();
            world.log.push(format!("execute {}", self.arguments));
            if let Some(created) = self.text("creates") {
                world.created.insert(created.to_owned());
            }
            if let Some(moved) = self.text("moves") {
                world.moved.insert(moved.to_owned());
            }
            if let Some(removed) = self.text("removes") {
                world.removed.insert(removed.to_owned());
            }
            if !self.completed {
                ToolOutput::failure("not completed")
            } else if self.mutates() && self.mutation.is_none() {
                ToolOutput::failure("unreadable target")
            } else if self.has("fail") {
                ToolOutput::failure("scoped failure")
            } else {
                ToolOutput::success("scoped")
            }
        })
    }
}

struct TargetGate {
    world: SharedWorld,
}

impl PermissionGate for TargetGate {
    fn admit(&self, _call: &ToolCall) -> Admission {
        Admission::Allowed(PathAccess::WorkspaceOnly)
    }

    fn admit_file_mutation(&self, mutation: &FileMutation) -> Admission {
        self.world
            .lock()
            .unwrap()
            .log
            .push(format!("admit {}", mutation.target.display()));
        Admission::Allowed(PathAccess::WorkspaceOnly)
    }

    fn applicable_target(&self, call: &ToolCall) -> Option<ApplicableTarget> {
        let arguments = parse_tool_args_object(&call.arguments).unwrap();
        let path = resolve(&self.world, arguments.optional_string("read")?)?;
        Some(ApplicableTarget {
            path,
            kind: TargetKind::Directory,
        })
    }

    fn forget_approvals(&self) {}
}

#[derive(Default)]
struct FakeProject {
    selections: Mutex<Vec<(Vec<PathBuf>, DeliveryState)>>,
}

impl ProjectContextProvider for FakeProject {
    fn select(&self, targets: &[ApplicableTarget], delivery: &DeliveryState) -> ProjectContext {
        let paths: Vec<PathBuf> = targets.iter().map(|target| target.path.clone()).collect();
        assert!(
            !paths.contains(&PathBuf::from("/w/panic")),
            "selection panicked"
        );
        self.selections
            .lock()
            .unwrap()
            .push((paths.clone(), delivery.clone()));
        let fresh: Vec<PathBuf> = paths
            .into_iter()
            .filter(|path| !delivery.evaluated_endpoints.contains(path))
            .collect();
        let ruled: Vec<PathBuf> = fresh
            .iter()
            .filter(|path| RULED.iter().any(|ruled| path.as_os_str() == *ruled))
            .cloned()
            .collect();
        let content = (!ruled.is_empty()).then(|| {
            ruled
                .iter()
                .map(|path| format!("RULE {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n")
        });
        let notices = content
            .iter()
            .map(|content| format!("notice for {content}"))
            .collect();
        ProjectContext {
            content,
            delivered_sources: ruled,
            evaluated_endpoints: fresh,
            notices,
        }
    }
}

struct Harness {
    provider: Arc<FakeProvider>,
    project: Arc<FakeProject>,
    world: SharedWorld,
    agent: Agent,
}

fn harness(scripts: Vec<Script>, snapshot: ProjectContext) -> Harness {
    built(scripts, Some(snapshot))
}

fn unscoped_harness(scripts: Vec<Script>) -> Harness {
    built(scripts, None)
}

fn built(scripts: Vec<Script>, snapshot: Option<ProjectContext>) -> Harness {
    let provider = FakeProvider::new(scripts);
    let project = Arc::new(FakeProject::default());
    let world = SharedWorld::default();
    let tool: Arc<dyn Tool> = Arc::new(ScopedTool {
        spec: ToolSpec {
            name: "scoped".to_owned(),
            description: "Scoped tool.".to_owned(),
            input_schema: r#"{"type":"object"}"#.into(),
        },
        world: Arc::clone(&world),
    });
    let shared: Arc<FakeProvider> = Arc::clone(&provider);
    let agent = Agent::new(
        shared,
        vec![tool],
        Arc::new(FixedContext),
        Arc::new(TargetGate {
            world: Arc::clone(&world),
        }),
        config(),
    );
    let agent = match snapshot {
        Some(snapshot) => agent.with_project_context(
            Arc::clone(&project) as Arc<dyn ProjectContextProvider>,
            snapshot,
        ),
        None => agent,
    };
    Harness {
        provider,
        project,
        world,
        agent,
    }
}

fn scoped_reply(calls: &[(&str, &str)]) -> Script {
    let calls = calls
        .iter()
        .map(|(id, arguments)| ToolCall::new(*id, "scoped", *arguments))
        .collect();
    Script::Reply(Vec::new(), completion(None, calls, FinishReason::ToolCalls))
}

fn snapshot() -> ProjectContext {
    ProjectContext {
        content: Some("SNAPSHOT".to_owned()),
        delivered_sources: vec![PathBuf::from("/w/AGENTS.md")],
        evaluated_endpoints: vec![PathBuf::from("/w")],
        notices: Vec::new(),
    }
}

fn snapshot_delivery() -> DeliveryState {
    DeliveryState::from_snapshot(&snapshot())
}

fn scoped_messages(messages: &[ChatMessage]) -> Vec<(&str, &str, ToolResultStatus)> {
    messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::Tool {
                call_id,
                content,
                status,
                ..
            } => Some((call_id.as_str(), content.as_str(), *status)),
            _ => None,
        })
        .collect()
}

fn lifecycle(events: &[UiEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ToolStarted {
                call_id,
                description,
                ..
            } => Some(format!("start {} {}", call_id.as_str(), description.title)),
            UiEvent::ToolFinished { call_id, .. } => Some(format!("finish {}", call_id.as_str())),
            UiEvent::ToolRejected { call_id, .. } => Some(format!("reject {}", call_id.as_str())),
            UiEvent::ToolDeferred {
                call_id, deferral, ..
            } => Some(format!("defer {} {deferral:?}", call_id.as_str())),
            UiEvent::ContextNotice { text, .. } => Some(format!("notice {text}")),
            _ => None,
        })
        .collect()
}

fn log(harness: &Harness) -> Vec<String> {
    harness.world.lock().unwrap().log.clone()
}

#[tokio::test]
async fn the_snapshot_and_later_rules_follow_the_system_prompt_across_turns() {
    let mut harness = harness(
        vec![
            scoped_reply(&[("call-1", r#"{"read":"/w/a"}"#)]),
            text_reply("ok"),
            scoped_reply(&[("call-2", r#"{"read":"/w/a"}"#)]),
            text_reply("again"),
        ],
        snapshot(),
    );
    run(&mut harness.agent, "hi").await;
    run(&mut harness.agent, "more").await;
    let requests = harness.provider.requests();
    assert_eq!(
        requests[0].instructions,
        [
            SYSTEM_PROMPT,
            "SNAPSHOT",
            TURN_CONTEXT,
            RESPONSE_LANGUAGE_CONTROL
        ]
    );
    for request in &requests[1..] {
        assert_eq!(
            request.instructions,
            [
                SYSTEM_PROMPT,
                "SNAPSHOT",
                "RULE /w/a",
                TURN_CONTEXT,
                RESPONSE_LANGUAGE_CONTROL
            ]
        );
    }
    let selections = harness.project.selections.lock().unwrap();
    assert_eq!(selections.len(), 2);
    assert_eq!(selections[0].1, snapshot_delivery());
    assert_eq!(
        selections[1].1.evaluated_endpoints,
        [PathBuf::from("/w"), PathBuf::from("/w/a")]
    );
    assert_eq!(
        selections[1].1.delivered_sources,
        [PathBuf::from("/w/AGENTS.md"), PathBuf::from("/w/a")]
    );
}

#[tokio::test]
async fn clearing_history_forgets_scoped_rules_and_keeps_the_snapshot() {
    let mut harness = harness(
        vec![
            scoped_reply(&[("call-1", r#"{"read":"/w/a"}"#)]),
            text_reply("ok"),
            scoped_reply(&[("call-2", r#"{"read":"/w/a"}"#)]),
            text_reply("again"),
        ],
        snapshot(),
    );
    run(&mut harness.agent, "hi").await;
    harness.agent.clear_history();
    run(&mut harness.agent, "fresh").await;
    let requests = harness.provider.requests();
    assert_eq!(
        requests[2].instructions,
        [
            SYSTEM_PROMPT,
            "SNAPSHOT",
            TURN_CONTEXT,
            RESPONSE_LANGUAGE_CONTROL
        ]
    );
    assert_eq!(requests[2].messages, [ChatMessage::user("fresh")]);
    let selections = harness.project.selections.lock().unwrap();
    assert_eq!(selections.len(), 2);
    assert_eq!(selections[1].1, snapshot_delivery());
}

#[tokio::test]
async fn read_targets_add_scoped_rules_before_the_reads_run_and_report_notices() {
    let mut harness = harness(
        vec![
            scoped_reply(&[("call-1", r#"{"read":"/w/a"}"#)]),
            text_reply("done"),
        ],
        snapshot(),
    );
    let (report, events) = run(&mut harness.agent, "read").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        lifecycle(&events),
        [
            "notice notice for RULE /w/a",
            r#"start call-1 Scoping {"read":"/w/a"}"#,
            "finish call-1",
        ]
    );
    assert_eq!(
        scoped_messages(&harness.provider.requests()[1].messages),
        [("call-1", "scoped", ToolResultStatus::Success)]
    );
}

#[tokio::test]
async fn a_lone_write_with_new_rules_is_deferred_until_the_model_reissues_it() {
    let write = r#"{"write":"/w/b/file"}"#;
    let mut harness = harness(
        vec![
            scoped_reply(&[("call-1", write)]),
            scoped_reply(&[("call-2", write)]),
            text_reply("done"),
        ],
        snapshot(),
    );
    let (report, events) = run(&mut harness.agent, "write").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        log(&harness),
        [format!("complete {write}"), format!("execute {write}")]
    );
    let requests = harness.provider.requests();
    assert_eq!(
        scoped_messages(&requests[2].messages),
        [
            ("call-1", CONTEXT_DEFERRED_OUTPUT, ToolResultStatus::Failure),
            ("call-2", "scoped", ToolResultStatus::Success),
        ]
    );
    assert_eq!(requests[1].instructions[2], "RULE /w/b/file");
    assert_eq!(
        lifecycle(&events),
        [
            "notice notice for RULE /w/b/file",
            "start call-1 Writing file",
            "defer call-1 ProjectInstructions",
            &format!("start call-2 Scoping {write}"),
            "finish call-2",
        ]
    );
}

#[tokio::test]
async fn batches_defer_only_writes_whose_own_targets_bring_new_rules() {
    let mut harness = harness(
        vec![
            scoped_reply(&[
                ("call-1", r#"{"read":"/w/a"}"#),
                ("call-2", r#"{"write":"/w/b/file"}"#),
                ("call-3", r#"{"write":"/w/c/file"}"#),
                ("call-4", r#"{"invalid":true}"#),
            ]),
            text_reply("done"),
        ],
        snapshot(),
    );
    let (report, _) = run(&mut harness.agent, "batch").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = harness.provider.requests();
    assert_eq!(requests[1].instructions[2], "RULE /w/a\nRULE /w/b/file");
    assert_eq!(
        scoped_messages(&requests[1].messages),
        [
            ("call-1", "scoped", ToolResultStatus::Success),
            ("call-2", CONTEXT_DEFERRED_OUTPUT, ToolResultStatus::Failure),
            ("call-3", "scoped", ToolResultStatus::Success),
            ("call-4", "invalid arguments", ToolResultStatus::Failure),
        ]
    );
    let selections = harness.project.selections.lock().unwrap();
    let probed: Vec<&Vec<PathBuf>> = selections.iter().map(|(paths, _)| paths).collect();
    assert_eq!(
        probed,
        [
            &vec![
                PathBuf::from("/w/a"),
                PathBuf::from("/w/b/file"),
                PathBuf::from("/w/c/file")
            ],
            &vec![PathBuf::from("/w/b/file")],
            &vec![PathBuf::from("/w/c/file")],
        ]
    );
    assert!(
        selections
            .iter()
            .all(|(_, delivery)| *delivery == snapshot_delivery())
    );
}

#[tokio::test]
async fn deferred_results_do_not_count_as_repeated_failures() {
    let failing = r#"{"write":"/w/b","fail":true}"#;
    let mut harness = harness(
        vec![
            scoped_reply(&[("call-1", failing)]),
            scoped_reply(&[("call-2", failing)]),
            text_reply("done"),
        ],
        ProjectContext::default(),
    );
    let (report, _) = run(&mut harness.agent, "fail").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        scoped_messages(&harness.provider.requests()[2].messages),
        [
            ("call-1", CONTEXT_DEFERRED_OUTPUT, ToolResultStatus::Failure),
            ("call-2", "scoped failure", ToolResultStatus::Failure),
        ]
    );
}

#[tokio::test]
async fn every_call_is_prepared_once_and_completed_only_when_it_runs() {
    let mut harness = harness(
        vec![
            scoped_reply(&[
                ("call-1", r#"{"write":"/w/x","creates":"/w/c"}"#),
                ("call-2", r#"{"invalid":true}"#),
                ("call-3", r#"{"write":"/w/y"}"#),
                ("call-4", r#"{"read":"/w/z","inert":true}"#),
            ]),
            text_reply("done"),
        ],
        ProjectContext::default(),
    );
    let (report, _) = run(&mut harness.agent, "batch").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(harness.world.lock().unwrap().prepared, 4);
    assert_eq!(
        log(&harness),
        [
            r#"complete {"write":"/w/x","creates":"/w/c"}"#,
            r#"execute {"write":"/w/x","creates":"/w/c"}"#,
            r#"complete {"write":"/w/y"}"#,
            r#"execute {"write":"/w/y"}"#,
            r#"execute {"read":"/w/z","inert":true}"#,
        ]
    );
}

#[tokio::test]
async fn without_project_context_a_call_after_a_parallel_group_is_completed_only_when_it_runs() {
    let read = r#"{"read":"/w/z"}"#;
    let write = r#"{"write":"/w/x","mutation":true}"#;
    let mut harness = unscoped_harness(vec![
        scoped_reply(&[("call-1", read), ("call-2", write)]),
        text_reply("done"),
    ]);
    let (report, _) = run(&mut harness.agent, "batch").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        log(&harness),
        [
            format!("complete {read}"),
            format!("execute {read}"),
            format!("complete {write}"),
            "admit /w/x".to_owned(),
            format!("execute {write}"),
        ]
    );
}

#[tokio::test]
async fn calls_whose_targets_change_before_they_run_are_not_executed() {
    let mut harness = harness(
        vec![
            scoped_reply(&[
                ("call-1", r#"{"write":"/w/x","creates":"/w/missing"}"#),
                ("call-2", r#"{"read":"/w/missing"}"#),
                ("call-3", r#"{"read":"/w/c"}"#),
                ("call-4", r#"{"write":"/w/missing","fail":true}"#),
            ]),
            scoped_reply(&[("call-5", r#"{"read":"/w/missing"}"#)]),
            text_reply("done"),
        ],
        ProjectContext::default(),
    );
    let (report, events) = run(&mut harness.agent, "batch").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = harness.provider.requests();
    assert_eq!(
        scoped_messages(&requests[2].messages),
        [
            ("call-1", "scoped", ToolResultStatus::Success),
            ("call-2", NOT_EXECUTED_OUTPUT, ToolResultStatus::Failure),
            ("call-3", "scoped", ToolResultStatus::Success),
            ("call-4", NOT_EXECUTED_OUTPUT, ToolResultStatus::Failure),
            ("call-5", "scoped", ToolResultStatus::Success),
        ]
    );
    assert_eq!(
        lifecycle(&events),
        [
            r#"start call-1 Scoping {"write":"/w/x","creates":"/w/missing"}"#,
            "finish call-1",
            r#"start call-2 Scoping {"read":"/w/missing"}"#,
            "defer call-2 TargetChanged",
            r#"start call-3 Scoping {"read":"/w/c"}"#,
            "finish call-3",
            "start call-4 Writing file",
            "defer call-4 TargetChanged",
            r#"start call-5 Scoping {"read":"/w/missing"}"#,
            "finish call-5",
        ]
    );
    assert!(
        !log(&harness)
            .iter()
            .any(|entry| entry.contains(r#""fail":true"#))
    );
}

#[tokio::test]
async fn a_parallel_group_runs_together_only_while_every_target_is_fresh() {
    let mut harness = harness(
        vec![
            scoped_reply(&[
                ("call-1", r#"{"write":"/w/x","creates":"/w/missing"}"#),
                ("call-2", r#"{"read":"/w/missing"}"#),
                ("call-3", r#"{"read":"/w/c"}"#),
                ("call-4", r#"{"read":"/w/d"}"#),
            ]),
            text_reply("done"),
        ],
        ProjectContext::default(),
    );
    let (report, events) = run(&mut harness.agent, "batch").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        lifecycle(&events)[2..],
        [
            r#"start call-2 Scoping {"read":"/w/missing"}"#,
            "defer call-2 TargetChanged",
            r#"start call-3 Scoping {"read":"/w/c"}"#,
            r#"start call-4 Scoping {"read":"/w/d"}"#,
            "finish call-3",
            "finish call-4",
        ]
    );
}

#[tokio::test]
async fn delegations_and_reads_run_as_separate_parallel_groups() {
    let mut harness = harness(
        vec![
            scoped_reply(&[
                ("call-1", r#"{"read":"/w/c"}"#),
                ("call-2", r#"{"read":"/w/d","delegate":true}"#),
                ("call-3", r#"{"read":"/w/e","delegate":true}"#),
                ("call-4", r#"{"read":"/w/f"}"#),
            ]),
            text_reply("done"),
        ],
        ProjectContext::default(),
    );
    let (report, events) = run(&mut harness.agent, "batch").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        lifecycle(&events),
        [
            r#"start call-1 Scoping {"read":"/w/c"}"#,
            "finish call-1",
            r#"start call-2 Scoping {"read":"/w/d","delegate":true}"#,
            r#"start call-3 Scoping {"read":"/w/e","delegate":true}"#,
            "finish call-2",
            "finish call-3",
            r#"start call-4 Scoping {"read":"/w/f"}"#,
            "finish call-4",
        ]
    );
}

#[tokio::test]
async fn a_failed_selection_fails_every_call_and_the_turn() {
    let mut harness = harness(
        vec![scoped_reply(&[
            ("call-1", r#"{"read":"/w/panic"}"#),
            ("call-2", r#"{"write":"/w/b/file"}"#),
        ])],
        snapshot(),
    );
    let (report, events) = run(&mut harness.agent, "panic").await;
    assert_eq!(report.failure, Some(TurnFailure::ProjectContext));
    assert_eq!(lifecycle(&events), ["reject call-1", "reject call-2"]);
    let reasons: Vec<ToolRejection> = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ToolRejected { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect();
    assert_eq!(reasons, [ToolRejection::Panicked, ToolRejection::Panicked]);
    assert!(log(&harness).is_empty());
    assert!(harness.agent.history.is_empty());
}

#[tokio::test]
async fn a_target_projection_that_panics_rejects_only_its_call() {
    let mut harness = harness(
        vec![
            scoped_reply(&[
                ("call-1", r#"{"write":"/w/b/file","target_panic":true}"#),
                ("call-2", r#"{"read":"/w/c"}"#),
            ]),
            text_reply("done"),
        ],
        ProjectContext::default(),
    );
    let (report, _) = run(&mut harness.agent, "panic").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let messages = harness.provider.requests()[1].messages.clone();
    let results = scoped_messages(&messages);
    assert_eq!(results[0].0, "call-1");
    assert!(results[0].1.contains("Tool execution panicked"));
    assert_eq!(results[1], ("call-2", "scoped", ToolResultStatus::Success));
}

#[tokio::test]
async fn file_changes_run_only_when_their_completed_target_is_the_gate_target() {
    let relocating = r#"{"write":"/w/x","moves":"/w/m","removes":"/w/v"}"#;
    let moved = r#"{"write":"/w/m","mutation":true}"#;
    let vanished = r#"{"write":"/w/v","mutation":true}"#;
    let unreadable = r#"{"write":"/w/u","mutation":true,"unreadable":true}"#;
    let fresh = r#"{"write":"/w/n","mutation":true}"#;
    let untargeted = r#"{"write":"/w/q","mutation":true,"untargeted":true}"#;
    let mut harness = harness(
        vec![
            scoped_reply(&[
                ("call-1", relocating),
                ("call-2", moved),
                ("call-3", vanished),
                ("call-4", unreadable),
                ("call-5", fresh),
                ("call-6", untargeted),
            ]),
            text_reply("done"),
        ],
        ProjectContext::default(),
    );
    let (report, events) = run(&mut harness.agent, "batch").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        scoped_messages(&harness.provider.requests()[1].messages),
        [
            ("call-1", "scoped", ToolResultStatus::Success),
            ("call-2", NOT_EXECUTED_OUTPUT, ToolResultStatus::Failure),
            ("call-3", NOT_EXECUTED_OUTPUT, ToolResultStatus::Failure),
            ("call-4", "unreadable target", ToolResultStatus::Failure),
            ("call-5", "scoped", ToolResultStatus::Success),
            ("call-6", NOT_EXECUTED_OUTPUT, ToolResultStatus::Failure),
        ]
    );
    assert_eq!(
        log(&harness),
        [
            format!("complete {relocating}"),
            format!("execute {relocating}"),
            format!("complete {moved}"),
            format!("complete {vanished}"),
            format!("complete {unreadable}"),
            format!("execute {unreadable}"),
            format!("complete {fresh}"),
            "admit /w/n".to_owned(),
            format!("execute {fresh}"),
        ]
    );
    assert_eq!(
        lifecycle(&events)[2..],
        [
            "start call-2 Writing file",
            "defer call-2 TargetChanged",
            "start call-3 Writing file",
            "defer call-3 TargetChanged",
            &format!("start call-4 Scoping {unreadable}"),
            "finish call-4",
            &format!("start call-5 Scoping {fresh}"),
            "finish call-5",
            "start call-6 Writing file",
            "defer call-6 TargetChanged",
        ]
    );
}

#[tokio::test]
async fn a_target_that_panics_when_checked_again_rejects_only_its_call() {
    let mut harness = harness(
        vec![
            scoped_reply(&[
                ("call-1", r#"{"read":"/w/c","recheck_panic":true}"#),
                ("call-2", r#"{"read":"/w/d"}"#),
            ]),
            text_reply("done"),
        ],
        ProjectContext::default(),
    );
    let (report, events) = run(&mut harness.agent, "panic").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let messages = harness.provider.requests()[1].messages.clone();
    let results = scoped_messages(&messages);
    assert_eq!(results[0].0, "call-1");
    assert!(
        results[0].1.contains("Tool execution panicked"),
        "{results:?}"
    );
    assert_eq!(results[0].2, ToolResultStatus::Failure);
    assert_eq!(results[1], ("call-2", "scoped", ToolResultStatus::Success));
    assert_eq!(
        lifecycle(&events),
        [
            "reject call-1",
            r#"start call-2 Scoping {"read":"/w/d"}"#,
            "finish call-2",
        ]
    );
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::ToolRejected { call_id, reason: ToolRejection::Panicked, .. }
            if call_id.as_str() == "call-1"
    )));
}

#[tokio::test]
async fn a_refused_call_is_rejected_before_its_target_reaches_the_gate() {
    let refused = r#"{"write":"/w/b/file","refused":true}"#;
    let mut harness = harness(
        vec![
            scoped_reply(&[("call-1", refused), ("call-2", r#"{"read":"/w/c"}"#)]),
            text_reply("done"),
        ],
        snapshot(),
    );
    let (report, events) = run(&mut harness.agent, "refused").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        scoped_messages(&harness.provider.requests()[1].messages),
        [
            ("call-1", "refused arguments", ToolResultStatus::Failure),
            ("call-2", "scoped", ToolResultStatus::Success),
        ]
    );
    let selections = harness.project.selections.lock().unwrap();
    let probed: Vec<&Vec<PathBuf>> = selections.iter().map(|(paths, _)| paths).collect();
    assert_eq!(probed, [&vec![PathBuf::from("/w/c")]]);
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::ToolRejected { call_id, reason: ToolRejection::Invalid, description: Some(description), .. }
            if call_id.as_str() == "call-1" && description.title == format!("Scoping {refused}")
    )));
    assert!(!log(&harness).iter().any(|entry| entry.contains("refused")));
}

#[tokio::test]
async fn malformed_calls_never_reach_preparation_or_target_selection() {
    let truncated = r#"{"write":"/w/b/file""#;
    let mut harness = harness(
        vec![
            scoped_reply(&[("call-1", truncated), ("call-2", r#"{"read":"/w/c"}"#)]),
            scoped_reply(&[("call-3", "[]")]),
            text_reply("done"),
        ],
        snapshot(),
    );
    let (report, events) = run(&mut harness.agent, "malformed").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let malformed =
        malformed_tool_arguments_json("scoped", &ToolArgumentDiagnostic::diagnose(truncated));
    let non_object = non_object_tool_arguments_json("scoped");
    let messages = &harness.provider.requests()[2].messages;
    assert_eq!(
        scoped_messages(messages),
        [
            ("call-1", malformed.as_str(), ToolResultStatus::Failure),
            ("call-2", "scoped", ToolResultStatus::Success),
            ("call-3", non_object.as_str(), ToolResultStatus::Failure),
        ]
    );
    let replayed: Vec<(&str, &str)> = messages
        .iter()
        .flat_map(|message| match message {
            ChatMessage::Assistant { tool_calls, .. } => tool_calls.as_slice(),
            _ => &[],
        })
        .map(|call| (call.id.as_str(), call.arguments.as_str()))
        .collect();
    assert_eq!(
        replayed,
        [
            ("call-1", "{}"),
            ("call-2", r#"{"read":"/w/c"}"#),
            ("call-3", "{}")
        ]
    );
    assert_eq!(
        lifecycle(&events),
        [
            "reject call-1",
            r#"start call-2 Scoping {"read":"/w/c"}"#,
            "finish call-2",
            "reject call-3",
        ]
    );
    assert_eq!(harness.world.lock().unwrap().prepared, 1);
    let selections = harness.project.selections.lock().unwrap();
    let probed: Vec<&Vec<PathBuf>> = selections.iter().map(|(paths, _)| paths).collect();
    assert_eq!(probed, [&vec![PathBuf::from("/w/c")]]);
    assert!(
        events
            .iter()
            .filter(|event| matches!(event, UiEvent::ToolRejected { .. }))
            .all(|event| matches!(
                event,
                UiEvent::ToolRejected {
                    reason: ToolRejection::MalformedArguments,
                    arguments,
                    description: None,
                    ..
                } if arguments == "{}"
            ))
    );
}
