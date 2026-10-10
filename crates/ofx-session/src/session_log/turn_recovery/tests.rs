use std::fs;
use std::path::PathBuf;

use ofx_config::ProviderId;
use ofx_contract::{
    ChatMessage, HistoryCut, HistoryEntry, HistoryStep, HistoryTurn, ModelRecoveryCause,
    ProviderReplay, ReasoningEffort, RecoveredTurn, RecoveryPoint, RecoveryProgress,
    RecoveryStrategy, RecoveryToolState, ReplaySource, StepResult, ToolArgumentIntegrity, ToolCall,
    ToolCallId, ToolResultStatus, TurnEnd, TurnId,
};

use super::*;
use crate::session_codec::{SessionMetadata, SessionPreferences};
use crate::session_event::{
    ArtifactCompleteness, AssistantEvent, ContextCheckpointEvent, FileEvidence, FileEvidenceAction,
    SteeringEvent, ToolCallEvent, ToolResultEvent, TurnCompletedEvent, UserEvent,
};
use crate::session_log::{
    EVENTS_FILE, LOCK_DEADLINE, WritableSession, resume_session, start_session,
};

struct Fixture {
    root: tempfile::TempDir,
    sessions: PrivateDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let sessions = PrivateDir::open_or_create(&root.path().join("sessions")).unwrap();
        Self { root, sessions }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join("sessions/saved").join(name)
    }

    fn start(&self, events: &[ConversationEvent]) {
        self.start_under(
            SavedProvider::new(ProviderId::Gateway, None).unwrap(),
            events,
        );
    }

    fn start_under(&self, provider: SavedProvider, events: &[ConversationEvent]) {
        let mut metadata = metadata();
        metadata.preferences.provider = provider;
        let mut session = start_session(&self.sessions, metadata).unwrap();
        session.append(2, events).unwrap();
    }

    fn save_checkpoint(&self, conversation_seq: u64, checkpoint: &str) {
        let text =
            format!("{{\"conversation_seq\":{conversation_seq},\"checkpoint\":{checkpoint}}}\n");
        fs::write(self.path(RECOVERY_FILE), text).unwrap();
        fs::write(self.path(RECOVERY_ASKED_FILE), "{\"asked_at_ms\":3}\n").unwrap();
    }

    fn resume(&self) -> Result<WritableSession, SessionError> {
        resume_session(&self.sessions, "saved", LOCK_DEADLINE)
    }

    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.path(EVENTS_FILE))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn metadata() -> SessionMetadata {
    SessionMetadata {
        id: "saved".to_owned(),
        origin_workspace_root: "/workspace".to_owned(),
        workspace_root: "/workspace".to_owned(),
        created_at_ms: 1,
        updated_at_ms: 1,
        conversation_language: "und".to_owned(),
        preferences: SessionPreferences {
            provider: SavedProvider::new(ProviderId::Gateway, None).unwrap(),
            model: "openai/gpt-5".to_owned(),
            effort: ReasoningEffort::Auto,
            fast_mode: false,
        },
        title: None,
        subagent_child: false,
    }
}

fn user(text: &str) -> ConversationEvent {
    ConversationEvent::User(UserEvent::new(text))
}

fn assistant(text: &str) -> ConversationEvent {
    ConversationEvent::Assistant(AssistantEvent {
        text: text.to_owned(),
        provider_replay: None,
        standalone_response: false,
    })
}

fn call(id: &str) -> ConversationEvent {
    ConversationEvent::ToolCall(ToolCallEvent::new(
        id,
        "shell",
        "{\"command\":\"ls\"}",
        ToolArgumentIntegrity::Valid,
    ))
}

fn result(id: &str) -> ConversationEvent {
    ConversationEvent::ToolResult(ToolResultEvent::new(
        id,
        "shell",
        ToolResultStatus::Success,
        "result.txt",
        3,
        ArtifactCompleteness::Complete,
    ))
}

fn finished_turn() -> Vec<ConversationEvent> {
    vec![
        user("before"),
        assistant("done before"),
        ConversationEvent::TurnCompleted(TurnCompletedEvent::default()),
    ]
}

fn step(call_id: &str, output: &str) -> String {
    tool_step(call_id, "shell", r#"{\"command\":\"ls\"}"#, output)
}

fn tool_step(call_id: &str, name: &str, arguments: &str, output: &str) -> String {
    let bytes = output.len();
    format!(
        "{{\"assistant\":null,\"provider_replay\":null,\"tool_calls\":[{{\"id\":\"{call_id}\",\"name\":\"{name}\",\"arguments_json\":\"{arguments}\",\"provider_result\":null}}],\"tool_results\":[{{\"tool_call_id\":\"{call_id}\",\"tool_name\":\"{name}\",\"status\":\"success\",\"output\":\"{output}\",\"output_handle\":null,\"preview\":null,\"output_bytes\":{bytes},\"stored_output_bytes\":{bytes},\"truncated\":false,\"provider_native\":false,\"review_feedback\":false,\"created_at_ms\":5,\"permission_feedback\":[],\"committed_file_presentation\":null,\"command_output_replay\":null,\"command_process_presentation\":null,\"terminal_action_presentation\":null}}]}}"
    )
}

const EVIDENCE: &str = "{\"path\":\"a.rs\",\"new_path\":null,\"tool_call_id\":\"c2\",\"tool_name\":\"shell\",\"action\":\"read\",\"status\":\"success\",\"model_view_covers_full_file\":true,\"stale\":false}";

fn checkpoint(user: &str, steps: &[String], steering: &str, partial: &str) -> String {
    checkpoint_with_files(user, steps, steering, partial, EVIDENCE)
}

fn checkpoint_with_files(
    user: &str,
    steps: &[String],
    steering: &str,
    partial: &str,
    files: &str,
) -> String {
    let steps = steps.join(",");
    format!(
        "{{\"version\":2,\"turn_id\":7,\"user\":{{\"text\":\"{user}\",\"images\":[]}},\"assistant_source\":\"{partial}\",\"execution\":{{\"schema_version\":10,\"tool_steps\":[{steps}],\"files\":[{files}],\"steering\":[{steering}],\"turn_summary\":null}},\"cause\":\"network_interrupted\",\"action\":\"retrying_request\",\"tool_state\":\"none\",\"authority\":{{\"provider\":\"gateway\",\"model\":\"openai/gpt-5\",\"credential_source\":null,\"credential_identity\":null}},\"requested_fast_mode\":false,\"fast_mode\":false,\"max_provider_attempts\":3,\"consumed_provider_attempts\":1,\"outstanding_reservation\":false}}"
    )
}

fn evidence() -> FileEvidence {
    FileEvidence {
        path: "a.rs".to_owned(),
        new_path: None,
        tool_call_id: "c2".to_owned(),
        tool_name: "shell".to_owned(),
        action: FileEvidenceAction::Read,
        status: ToolResultStatus::Success,
        model_view_covers_full_file: true,
        stale: false,
    }
}

fn interrupted_checkpoint() -> String {
    checkpoint(
        "fix the build",
        &[step("c2", "out")],
        "{\"text\":\"also tests\",\"assistant_prefix\":null,\"after_tool_step_count\":1}",
        "Looking at",
    )
}

#[test]
fn a_settled_checkpoint_keeps_its_turn_summary_in_the_interrupted_frame() {
    let summary = "{\"started_at_ms\":1000,\"completed_at_ms\":4500,\"thinking_duration_ms\":1200,\"turn_duration_ms\":3500,\"token_progress\":{\"input_tokens\":1234,\"output_tokens\":340,\"input_exact\":true,\"output_exact\":false}}";
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    fixture.save_checkpoint(
        3,
        &interrupted_checkpoint().replace(
            "\"turn_summary\":null",
            &format!("\"turn_summary\":{summary}"),
        ),
    );
    fixture.resume().unwrap().settle_recovery().unwrap();
    let log = fixture.log();
    assert!(
        log[7].ends_with(&format!("\"turn_summary\":{summary}}}}}}}")),
        "{}",
        log[7]
    );
}

#[test]
fn a_matching_checkpoint_waits_until_it_is_settled_as_its_interrupted_turn() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    fixture.save_checkpoint(3, &interrupted_checkpoint());
    let mut resumed = fixture.resume().unwrap();
    assert_eq!(fixture.log().len(), 3);
    assert!(fixture.path(RECOVERY_FILE).exists());
    resumed.settle_recovery().unwrap();
    assert!(!resumed.turn_open());
    assert!(!fixture.path(RECOVERY_FILE).exists());
    assert!(!fixture.path(RECOVERY_ASKED_FILE).exists());
    let history = resumed.take_history();
    assert_eq!(history.turns.len(), 2);
    let events = &history.turns[1].events;
    assert_eq!(events[0], user("fix the build"));
    assert_eq!(events[1], call("c2"));
    let ConversationEvent::ToolResult(saved) = &events[2] else {
        panic!("expected a tool result, got {:?}", events[2]);
    };
    assert_eq!(resumed.tool_result_output(saved).as_deref(), Some("out"));
    assert_eq!(
        events[3],
        ConversationEvent::Steering(SteeringEvent {
            text: "also tests".to_owned(),
        })
    );
    let mut ended = InterruptedEvent::new(InterruptReason::Failed, Some("Looking at".to_owned()));
    ended.files = vec![evidence()];
    assert_eq!(events[4], ConversationEvent::Interrupted(ended));
    assert_eq!(events.len(), 5);
    let log = fixture.log();
    assert_eq!(log.len(), 8);
    assert!(
        log[7].contains(&format!("\"files\":[{EVIDENCE}]")),
        "{}",
        log[7]
    );
    drop(resumed);
    assert_eq!(fixture.resume().unwrap().take_history().turns.len(), 2);
    assert_eq!(fixture.log().len(), 8);
}

#[test]
fn a_turn_left_open_by_compaction_keeps_its_saved_prefix() {
    let fixture = Fixture::new();
    fixture.start(&[
        user("long task"),
        call("c1"),
        result("c1"),
        ConversationEvent::ContextCheckpoint(ContextCheckpointEvent {
            covers_through_seq: 1,
            summary: "<summary>started</summary>".to_owned(),
        }),
    ]);
    fixture.save_checkpoint(
        4,
        &checkpoint(
            "long task",
            &[step("c1", "old"), step("c2", "new")],
            "",
            "half way",
        ),
    );
    let mut resumed = fixture.resume().unwrap();
    assert!(resumed.turn_open());
    assert_eq!(fixture.log().len(), 4);
    resumed.settle_recovery().unwrap();
    drop(resumed);
    let log = fixture.log();
    assert_eq!(log.len(), 7, "{log:#?}");
    assert!(
        log[4].contains("\"tool_call\":{\"call_id\":\"c2\""),
        "{}",
        log[4]
    );
    assert!(
        log[5].contains("\"tool_result\":{\"call_id\":\"c2\""),
        "{}",
        log[5]
    );
    assert!(
        log[6].contains("\"interrupted\":{\"reason\":\"failed\",\"partial_text\":\"half way\""),
        "{}",
        log[6]
    );
    assert!(!fixture.path(RECOVERY_FILE).exists());
}

#[test]
fn a_stale_checkpoint_is_left_for_upstream_and_the_turn_closes_as_before() {
    let fixture = Fixture::new();
    fixture.start(&[
        user("long task"),
        call("c1"),
        result("c1"),
        ConversationEvent::ContextCheckpoint(ContextCheckpointEvent {
            covers_through_seq: 3,
            summary: "<summary>started</summary>".to_owned(),
        }),
    ]);
    fixture.save_checkpoint(3, &checkpoint("long task", &[], "", "stale"));
    drop(fixture.resume().unwrap());
    let log = fixture.log();
    assert_eq!(log.len(), 5);
    assert!(
        log[4].contains("\"interrupted\":{\"reason\":\"failed\",\"partial_text\":null"),
        "{}",
        log[4]
    );
    assert!(fixture.path(RECOVERY_FILE).exists());
}

#[test]
fn a_checkpoint_ahead_of_the_log_or_unreadable_fails_the_resume_untouched() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    let before = fixture.log();
    fixture.save_checkpoint(4, &checkpoint("next", &[], "", ""));
    assert_eq!(
        fixture.resume().err(),
        Some(SessionError::InvalidRecoveryCheckpoint)
    );
    fixture.save_checkpoint(3, "{\"version\":2}");
    assert_eq!(
        fixture.resume().err(),
        Some(SessionError::InvalidRecoveryCheckpoint)
    );
    assert_eq!(fixture.log(), before);
    assert!(fixture.path(RECOVERY_FILE).exists());
}

#[test]
fn a_saved_replay_keeps_its_own_provider_binding_under_any_preferences() {
    let saved = format!(
        "{{\"source\":{{\"provider\":{{\"name\":\"portkey\",\"binding\":\"{}\"}},\"model\":\"claude\"}},\"parts_json\":\"[1]\"}}",
        "22".repeat(32)
    );
    let replayed = step("c2", "out").replace(
        "\"provider_replay\":null",
        &format!("\"provider_replay\":{saved}"),
    );
    let preferences = [
        SavedProvider::new(ProviderId::Gateway, None).unwrap(),
        SavedProvider::new(
            ProviderId::Configured("portkey".to_owned()),
            Some([0x11; 32]),
        )
        .unwrap(),
    ];
    for provider in preferences {
        let fixture = Fixture::new();
        fixture.start_under(provider.clone(), &finished_turn());
        fixture.save_checkpoint(
            3,
            &checkpoint(
                "fix the build",
                std::slice::from_ref(&replayed),
                "",
                "Looking at",
            ),
        );
        fixture.resume().unwrap().settle_recovery().unwrap();
        let log = fixture.log();
        assert!(
            log[4].contains(&format!("\"provider_replay\":{saved}")),
            "{provider:?}: {}",
            log[4]
        );
    }
}

fn bound_replay(binding: &str) -> String {
    format!(
        "{{\"source\":{{\"provider\":{{\"name\":\"portkey\",\"binding\":\"{}\"}},\"model\":\"claude\"}},\"parts_json\":\"[1]\"}}",
        binding.repeat(32)
    )
}

fn replayed_step(call_id: &str, binding: &str) -> String {
    step(call_id, "out").replace(
        "\"provider_replay\":null",
        &format!("\"provider_replay\":{}", bound_replay(binding)),
    )
}

fn replays_in(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|line| {
            let frame: serde_json::Value = serde_json::from_str(line).unwrap();
            let replay = &frame["event"]["assistant"]["provider_replay"];
            (!replay.is_null()).then(|| replay.to_string())
        })
        .collect()
}

#[test]
fn each_recovered_step_keeps_the_replay_binding_it_was_saved_with() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    fixture.save_checkpoint(
        3,
        &checkpoint(
            "fix the build",
            &[replayed_step("c1", "11"), replayed_step("c2", "22")],
            "",
            "Looking at",
        ),
    );
    fixture.resume().unwrap().settle_recovery().unwrap();
    assert_eq!(
        replays_in(&fixture.log()),
        [bound_replay("11"), bound_replay("22")]
    );
}

#[test]
fn a_step_after_a_compacted_prefix_keeps_its_own_replay_binding() {
    let fixture = Fixture::new();
    fixture.start(&[
        user("long task"),
        call("c1"),
        result("c1"),
        ConversationEvent::ContextCheckpoint(ContextCheckpointEvent {
            covers_through_seq: 1,
            summary: "<summary>started</summary>".to_owned(),
        }),
    ]);
    fixture.save_checkpoint(
        4,
        &checkpoint(
            "long task",
            &[replayed_step("c1", "11"), replayed_step("c2", "22")],
            "",
            "half way",
        ),
    );
    fixture.resume().unwrap().settle_recovery().unwrap();
    let log = fixture.log();
    assert_eq!(replays_in(&log[4..]), [bound_replay("22")], "{log:#?}");
}

#[test]
fn a_continued_checkpoint_is_cleared_once_its_turn_is_saved() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    fixture.save_checkpoint(3, &interrupted_checkpoint());
    let mut resumed = fixture.resume().unwrap();
    assert!(resumed.take_recovery().is_some());
    drop(resumed);
    assert!(fixture.path(RECOVERY_FILE).exists());
    let mut resumed = fixture.resume().unwrap();
    let pending = resumed.take_recovery().unwrap();
    assert!(resumed.take_recovery().is_none());
    assert_eq!(pending.prompt(), "fix the build");
    let provider = metadata().preferences.provider;
    let recovered = pending.into_turn(&provider, "openai/gpt-5", false);
    assert_eq!(recovered.prompt, "fix the build");
    assert_eq!(recovered.strategy, RecoveryStrategy::ContinueResponse);
    let calls = vec![ToolCall::new("c2", "shell", "{\"command\":\"ls\"}")];
    assert_eq!(
        recovered.messages,
        [
            ChatMessage::Assistant {
                content: None,
                tool_calls: calls.clone(),
                provider_replay: None,
            },
            ChatMessage::Tool {
                call_id: ToolCallId::new("c2"),
                tool_name: "shell".to_owned(),
                content: "out".to_owned(),
                status: ToolResultStatus::Success,
            },
            ChatMessage::restored_steering("also tests"),
        ]
    );
    let finished = HistoryTurn {
        user: "fix the build",
        steps: vec![HistoryStep {
            assistant: "",
            provider_replay: None,
            tool_calls: &calls,
            tool_results: vec![StepResult {
                call_id: "c2",
                tool_name: "shell",
                output: "out",
                output_bytes: 3,
                status: ToolResultStatus::Success,
                model_view_covers_full_file: false,
                process: None,
                review_feedback: false,
                permission_feedback: Vec::new(),
            }],
        }],
        steering: Vec::new(),
        files: &[],
        end: TurnEnd::Replied {
            text: "fixed",
            provider_replay: None,
        },
    };
    resumed.record_turn(&finished, &provider).unwrap();
    assert!(!fixture.path(RECOVERY_FILE).exists());
    assert!(!fixture.path(RECOVERY_ASKED_FILE).exists());
    drop(resumed);
    let log = fixture.log();
    assert_eq!(log.len(), 8, "{log:#?}");
    assert!(log[6].contains("\"text\":\"fixed\""), "{}", log[6]);
}

fn portkey(binding: u8) -> SavedProvider {
    SavedProvider::new(
        ProviderId::Configured("portkey".to_owned()),
        Some([binding; 32]),
    )
    .unwrap()
}

fn continued_turn<'a>(
    calls: &'a [Vec<ToolCall>],
    replay: &'a ProviderReplay,
    end: TurnEnd<'a>,
) -> HistoryTurn<'a> {
    HistoryTurn {
        user: "fix the build",
        steps: calls
            .iter()
            .map(|calls| HistoryStep {
                assistant: "",
                provider_replay: Some(replay),
                tool_calls: calls,
                tool_results: vec![StepResult {
                    call_id: calls[0].id.as_str(),
                    tool_name: "shell",
                    output: "out",
                    output_bytes: 3,
                    status: ToolResultStatus::Success,
                    model_view_covers_full_file: false,
                    process: None,
                    review_feedback: false,
                    permission_feedback: Vec::new(),
                }],
            })
            .collect(),
        steering: Vec::new(),
        files: &[],
        end,
    }
}

fn shell_calls(ids: &[&str]) -> Vec<Vec<ToolCall>> {
    ids.iter()
        .map(|id| vec![ToolCall::new(*id, "shell", "{\"command\":\"ls\"}")])
        .collect()
}

fn projected_replay() -> ProviderReplay {
    ProviderReplay {
        source: ReplaySource {
            provider: "portkey".to_owned(),
            model: "claude".to_owned(),
            binding: None,
        },
        parts_json: "[1]".to_owned(),
    }
}

#[test]
fn a_continued_turn_saves_each_recovered_step_with_its_own_replay_binding() {
    for running in [
        SavedProvider::new(ProviderId::Gateway, None).unwrap(),
        portkey(0x33),
    ] {
        let fixture = Fixture::new();
        fixture.start_under(running.clone(), &finished_turn());
        fixture.save_checkpoint(
            3,
            &checkpoint(
                "fix the build",
                &[replayed_step("c1", "11"), replayed_step("c2", "22")],
                "",
                "",
            ),
        );
        let mut resumed = fixture.resume().unwrap();
        let continued = resumed
            .take_recovery()
            .unwrap()
            .into_turn(&running, "claude", false);
        let replay = projected_replay();
        let new_calls = shell_calls(&["c3"]);
        let mut finished = continued_history(&continued, replied("fixed"));
        finished
            .steps
            .extend(continued_turn(&new_calls, &replay, replied("")).steps);
        resumed.record_turn(&finished, &running).unwrap();
        let running_replay = format!(
            "{{\"source\":{{\"provider\":{},\"model\":\"claude\"}},\"parts_json\":\"[1]\"}}",
            serde_json::to_string(&running).unwrap()
        );
        let expected: Vec<String> = [bound_replay("11"), bound_replay("22")]
            .into_iter()
            .chain((running == portkey(0x33)).then_some(running_replay))
            .collect();
        assert_eq!(replays_in(&fixture.log()), expected, "{running:?}");
    }
}

#[test]
fn a_compaction_during_a_continued_turn_keeps_the_recovered_replay_bindings() {
    let fixture = Fixture::new();
    let running = portkey(0x33);
    fixture.start_under(running.clone(), &finished_turn());
    fixture.save_checkpoint(
        3,
        &checkpoint(
            "fix the build",
            &[replayed_step("c1", "11"), replayed_step("c2", "22")],
            "",
            "",
        ),
    );
    compact_then_finish(&fixture, &running);
    assert_eq!(
        replays_in(&fixture.log()),
        [bound_replay("11"), bound_replay("22")]
    );
}

fn replied(text: &str) -> TurnEnd<'_> {
    TurnEnd::Replied {
        text,
        provider_replay: None,
    }
}

fn compact_then_finish(fixture: &Fixture, running: &SavedProvider) {
    let mut resumed = fixture.resume().unwrap();
    let continued = resumed
        .take_recovery()
        .unwrap()
        .into_turn(running, "claude", false);
    let mut prefix = continued_history(&continued, replied(""));
    let rest_steps = prefix.steps.split_off(1);
    resumed
        .record_compaction(
            "<summary>first step</summary>",
            HistoryCut {
                turns: 1,
                tool_steps: 1,
                steering: 0,
            },
            Some(&prefix),
            running,
        )
        .unwrap();
    let mut rest = continued_history(&continued, replied("fixed"));
    rest.steps = rest_steps;
    resumed.record_turn(&rest, running).unwrap();
}

fn standalone_step(binding: &str) -> String {
    format!(
        "{{\"assistant\":\"Checking.\",\"provider_replay\":{},\"tool_calls\":[],\"tool_results\":[]}}",
        bound_replay(binding)
    )
}

fn standalone_turn<'a>(
    steps: usize,
    replay: &'a ProviderReplay,
    end: TurnEnd<'a>,
) -> HistoryTurn<'a> {
    HistoryTurn {
        user: "fix the build",
        steps: (0..steps)
            .map(|_| HistoryStep {
                assistant: "Checking.",
                provider_replay: Some(replay),
                tool_calls: &[],
                tool_results: Vec::new(),
            })
            .collect(),
        steering: Vec::new(),
        files: &[],
        end,
    }
}

fn standalone_checkpoint(fixture: &Fixture) {
    fixture.save_checkpoint(
        3,
        &checkpoint(
            "fix the build",
            &[standalone_step("11"), standalone_step("22")],
            "",
            "",
        ),
    );
}

#[test]
fn recovered_steps_with_the_same_text_keep_their_own_replays_when_settled() {
    let fixture = Fixture::new();
    fixture.start_under(portkey(0x33), &finished_turn());
    standalone_checkpoint(&fixture);
    fixture.resume().unwrap().settle_recovery().unwrap();
    assert_eq!(
        replays_in(&fixture.log()),
        [bound_replay("11"), bound_replay("22")]
    );
}

#[test]
fn recovered_steps_with_the_same_text_keep_their_own_replays_when_continued() {
    let fixture = Fixture::new();
    let running = portkey(0x33);
    fixture.start_under(running.clone(), &finished_turn());
    standalone_checkpoint(&fixture);
    let mut resumed = fixture.resume().unwrap();
    let continued = resumed
        .take_recovery()
        .unwrap()
        .into_turn(&running, "claude", false);
    let replay = projected_replay();
    let mut finished = continued_history(&continued, replied("fixed"));
    finished
        .steps
        .extend(standalone_turn(1, &replay, replied("")).steps);
    resumed.record_turn(&finished, &running).unwrap();
    let running_replay = format!(
        "{{\"source\":{{\"provider\":{},\"model\":\"claude\"}},\"parts_json\":\"[1]\"}}",
        serde_json::to_string(&running).unwrap()
    );
    assert_eq!(
        replays_in(&fixture.log()),
        [bound_replay("11"), bound_replay("22"), running_replay]
    );
}

#[test]
fn a_compaction_that_saves_an_earlier_recovered_step_leaves_the_later_one_its_replay() {
    let fixture = Fixture::new();
    let running = portkey(0x33);
    fixture.start_under(running.clone(), &finished_turn());
    standalone_checkpoint(&fixture);
    compact_then_finish(&fixture, &running);
    assert_eq!(
        replays_in(&fixture.log()),
        [bound_replay("11"), bound_replay("22")]
    );
}

#[test]
fn a_continued_turn_reopened_after_its_compaction_keeps_the_later_steps_replay() {
    let fixture = Fixture::new();
    let running = portkey(0x33);
    fixture.start_under(
        running.clone(),
        &[
            user("fix the build"),
            ConversationEvent::Assistant(AssistantEvent {
                text: "Checking.".to_owned(),
                provider_replay: None,
                standalone_response: true,
            }),
            ConversationEvent::ContextCheckpoint(ContextCheckpointEvent {
                covers_through_seq: 1,
                summary: "<summary>started</summary>".to_owned(),
            }),
        ],
    );
    fixture.save_checkpoint(
        3,
        &checkpoint(
            "fix the build",
            &[standalone_step("11"), standalone_step("22")],
            "",
            "",
        ),
    );
    let mut resumed = fixture.resume().unwrap();
    assert!(resumed.turn_open());
    let pending = resumed.take_recovery().unwrap();
    let continued = pending.into_turn(&running, "claude", false);
    let finished = continued_history(
        &continued,
        TurnEnd::Replied {
            text: "fixed",
            provider_replay: None,
        },
    );
    resumed.record_turn(&finished, &running).unwrap();
    assert_eq!(replays_in(&fixture.log()[3..]), [bound_replay("22")]);
}

#[test]
fn a_partly_answered_step_keeps_its_replay_when_continued() {
    let fixture = Fixture::new();
    let running = portkey(0x33);
    fixture.start_under(running.clone(), &finished_turn());
    let partly = replayed_step("c1", "22").replace(
        "\"tool_calls\":[{\"id\":\"c1\",\"name\":\"shell\",\"arguments_json\":\"{\\\"command\\\":\\\"ls\\\"}\",\"provider_result\":null}]",
        "\"tool_calls\":[{\"id\":\"c1\",\"name\":\"shell\",\"arguments_json\":\"{\\\"command\\\":\\\"ls\\\"}\",\"provider_result\":null},{\"id\":\"c2\",\"name\":\"shell\",\"arguments_json\":\"{\\\"command\\\":\\\"ls\\\"}\",\"provider_result\":null}]",
    );
    assert!(partly.contains("\"id\":\"c2\""), "{partly}");
    fixture.save_checkpoint(3, &checkpoint("fix the build", &[partly], "", ""));
    let mut resumed = fixture.resume().unwrap();
    let continued = resumed
        .take_recovery()
        .unwrap()
        .into_turn(&running, "claude", false);
    let finished = continued_history(
        &continued,
        TurnEnd::Replied {
            text: "fixed",
            provider_replay: None,
        },
    );
    assert_eq!(finished.steps[0].tool_calls.len(), 1);
    resumed.record_turn(&finished, &running).unwrap();
    assert_eq!(replays_in(&fixture.log()), [bound_replay("22")]);
}

fn continued_history<'a>(continued: &'a RecoveredTurn, end: TurnEnd<'a>) -> HistoryTurn<'a> {
    let mut steps = Vec::new();
    let mut messages = continued.messages.iter().peekable();
    while let Some(message) = messages.next() {
        let ChatMessage::Assistant {
            content,
            tool_calls,
            provider_replay,
        } = message
        else {
            continue;
        };
        let mut tool_results = Vec::new();
        while let Some(ChatMessage::Tool {
            call_id,
            tool_name,
            content,
            status,
        }) = messages.peek()
        {
            tool_results.push(StepResult {
                call_id: call_id.as_str(),
                tool_name,
                output: content,
                output_bytes: content.len(),
                status: *status,
                model_view_covers_full_file: false,
                process: None,
                review_feedback: false,
                permission_feedback: Vec::new(),
            });
            messages.next();
        }
        steps.push(HistoryStep {
            assistant: content.as_deref().unwrap_or_default(),
            provider_replay: provider_replay.as_ref(),
            tool_calls,
            tool_results,
        });
    }
    HistoryTurn {
        user: &continued.prompt,
        steps,
        steering: Vec::new(),
        files: &[],
        end,
    }
}

#[test]
fn a_recorded_checkpoint_waits_for_its_continuation_and_clears_with_the_next_saved_turn() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    let provider = metadata().preferences.provider;
    let large = "x".repeat(5_000);
    let calls = vec![ToolCall::new("c2", "shell", "{\"command\":\"ls\"}")];
    let point = RecoveryPoint {
        turn_id: TurnId::new(2),
        turn: HistoryTurn {
            user: "fix the build",
            steps: vec![HistoryStep {
                assistant: "",
                provider_replay: None,
                tool_calls: &calls,
                tool_results: vec![StepResult {
                    call_id: "c2",
                    tool_name: "shell",
                    output: &large,
                    output_bytes: large.len(),
                    status: ToolResultStatus::Success,
                    model_view_covers_full_file: false,
                    process: None,
                    review_feedback: false,
                    permission_feedback: Vec::new(),
                }],
            }],
            steering: Vec::new(),
            files: &[],
            end: TurnEnd::Replied {
                text: "",
                provider_replay: None,
            },
        },
        source: "",
        cause: ModelRecoveryCause::ProviderUnavailable,
        progress: RecoveryProgress::Paused,
        tool_state: RecoveryToolState::None,
        model: "openai/gpt-5",
        requested_fast_mode: false,
        fast_mode: false,
        attempt_limit: 10,
        consumed_attempts: 10,
    };
    let mut session = fixture.resume().unwrap();
    fs::write(fixture.path(RECOVERY_ASKED_FILE), "{\"asked_at_ms\":3}\n").unwrap();
    session
        .record_recovery(&point, &provider, RouteCredential::configured())
        .unwrap();
    assert!(fixture.path(RECOVERY_FILE).exists());
    assert!(!fixture.path(RECOVERY_ASKED_FILE).exists());
    let saved = fs::read_to_string(fixture.path(RECOVERY_FILE)).unwrap();
    assert!(saved.starts_with("{\"conversation_seq\":3,"), "{saved}");
    assert!(
        saved.contains("\"output\":\"\",\"output_handle\":\"result-shell-"),
        "{saved}"
    );
    drop(session);
    let mut resumed = fixture.resume().unwrap();
    assert_eq!(fixture.log().len(), 3);
    let pending = resumed.take_recovery().unwrap();
    assert!(pending.authorizes(RouteCredential::configured()));
    assert!(!pending.authorizes(RouteCredential::chatgpt_subscription("acct_1")));
    let continued = pending.into_turn(&provider, "openai/gpt-5", false);
    assert_eq!(continued.strategy, RecoveryStrategy::RetryRequest);
    assert!(matches!(
        &continued.messages[1],
        ChatMessage::Tool { content, .. } if *content == large
    ));
    let finished = HistoryTurn {
        user: "fix the build",
        steps: Vec::new(),
        steering: Vec::new(),
        files: &[],
        end: TurnEnd::Replied {
            text: "fixed",
            provider_replay: None,
        },
    };
    resumed.record_turn(&finished, &provider).unwrap();
    assert!(!fixture.path(RECOVERY_FILE).exists());
}

#[test]
fn a_checkpoint_written_during_a_continued_turn_keeps_the_recovered_replay_bindings() {
    let fixture = Fixture::new();
    let running = portkey(0x33);
    fixture.start_under(running.clone(), &finished_turn());
    fixture.save_checkpoint(
        3,
        &checkpoint("fix the build", &[replayed_step("c1", "11")], "", ""),
    );
    let mut resumed = fixture.resume().unwrap();
    let continued = resumed
        .take_recovery()
        .unwrap()
        .into_turn(&running, "claude", false);
    let point = RecoveryPoint {
        turn_id: TurnId::new(2),
        turn: continued_history(&continued, replied("")),
        source: "",
        cause: ModelRecoveryCause::RateLimited,
        progress: RecoveryProgress::Paused,
        tool_state: RecoveryToolState::None,
        model: "claude",
        requested_fast_mode: false,
        fast_mode: false,
        attempt_limit: 10,
        consumed_attempts: 10,
    };
    resumed
        .record_recovery(&point, &running, RouteCredential::configured())
        .unwrap();
    let saved = fs::read_to_string(fixture.path(RECOVERY_FILE)).unwrap();
    assert!(
        saved.contains(&format!("\"provider_replay\":{}", bound_replay("11"))),
        "{saved}"
    );
}

fn evidence_json(path: &str, call_id: &str, tool: &str, action: &str, flags: [bool; 2]) -> String {
    let [whole, stale] = flags;
    format!(
        "{{\"path\":\"{path}\",\"new_path\":null,\"tool_call_id\":\"{call_id}\",\"tool_name\":\"{tool}\",\"action\":\"{action}\",\"status\":\"success\",\"model_view_covers_full_file\":{whole},\"stale\":{stale}}}"
    )
}

fn read_evidence(path: &str, call_id: &str) -> ofx_contract::FileEvidence {
    ofx_contract::FileEvidence {
        path: path.to_owned(),
        new_path: None,
        tool_call_id: call_id.to_owned(),
        tool_name: "read_file".to_owned(),
        action: FileEvidenceAction::Read,
        status: ToolResultStatus::Success,
        model_view_covers_full_file: true,
        stale: false,
    }
}

fn recovered_files(fixture: &Fixture) -> Vec<ofx_contract::FileEvidence> {
    let mut resumed = fixture.resume().unwrap();
    let provider = metadata().preferences.provider;
    resumed
        .take_recovery()
        .unwrap()
        .into_turn(&provider, "openai/gpt-5", false)
        .files
}

#[test]
fn a_continued_checkpoint_hands_its_saved_file_evidence_to_the_turn() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    let saved = [
        evidence_json("gone.rs", "c0", "read_file", "read", [true, false]),
        evidence_json("a.rs", "r1", "read_file", "read", [true, false]),
    ]
    .join(",");
    fixture.save_checkpoint(
        3,
        &checkpoint_with_files(
            "fix the build",
            &[tool_step(
                "r1",
                "read_file",
                r#"{\"path\":\"a.rs\"}"#,
                "text",
            )],
            "",
            "",
            &saved,
        ),
    );
    assert_eq!(
        recovered_files(&fixture),
        [read_evidence("gone.rs", "c0"), read_evidence("a.rs", "r1")]
    );
}

#[test]
fn a_recorded_checkpoint_saves_the_file_evidence_its_turn_carries() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    let provider = metadata().preferences.provider;
    let read = [ToolCall::new("r1", "read_file", r#"{"path":"a.rs"}"#)];
    let files = [read_evidence("a.rs", "r1")];
    let point = RecoveryPoint {
        turn_id: TurnId::new(2),
        turn: HistoryTurn {
            user: "fix the build",
            steps: vec![HistoryStep {
                assistant: "",
                provider_replay: None,
                tool_calls: &read,
                tool_results: vec![StepResult {
                    call_id: "r1",
                    tool_name: "read_file",
                    output: "text",
                    output_bytes: 4,
                    status: ToolResultStatus::Success,
                    process: None,
                    review_feedback: false,
                    permission_feedback: Vec::new(),
                    model_view_covers_full_file: true,
                }],
            }],
            steering: Vec::new(),
            files: &files,
            end: replied(""),
        },
        source: "",
        cause: ModelRecoveryCause::ProviderUnavailable,
        progress: RecoveryProgress::Paused,
        tool_state: RecoveryToolState::None,
        model: "openai/gpt-5",
        requested_fast_mode: false,
        fast_mode: false,
        attempt_limit: 10,
        consumed_attempts: 10,
    };
    let mut session = fixture.resume().unwrap();
    session
        .record_recovery(&point, &provider, RouteCredential::configured())
        .unwrap();
    drop(session);
    let read_whole = evidence_json("a.rs", "r1", "read_file", "read", [true, false]);
    let saved = fs::read_to_string(fixture.path(RECOVERY_FILE)).unwrap();
    assert!(
        saved.contains(&format!("\"files\":[{read_whole}]")),
        "{saved}"
    );
    assert_eq!(recovered_files(&fixture), files);
}

#[test]
fn a_compaction_prepared_checkpoint_is_committed_when_the_session_opens() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    let prepared = checkpoint("fix the build", &[step("c2", "out")], "", "")
        .replace("\"network_interrupted\"", "\"compaction_prepared\"");
    fixture.save_checkpoint(3, &prepared);
    let mut resumed = fixture.resume().unwrap();
    assert!(resumed.take_recovery().is_none());
    assert!(resumed.recovery_transcript().is_none());
    assert!(!fixture.path(RECOVERY_FILE).exists());
    let log = fixture.log();
    assert_eq!(log.len(), 7, "{log:#?}");
    assert!(
        log[6].contains("\"interrupted\":{\"reason\":\"failed\""),
        "{}",
        log[6]
    );
}

#[test]
fn a_live_paused_turn_a_compaction_left_open_is_committed_when_settled() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    let provider = metadata().preferences.provider;
    let calls = vec![ToolCall::new("c2", "shell", "{\"command\":\"ls\"}")];
    let turn = HistoryTurn {
        user: "fix the build",
        steps: vec![HistoryStep {
            assistant: "",
            provider_replay: None,
            tool_calls: &calls,
            tool_results: vec![StepResult {
                call_id: "c2",
                tool_name: "shell",
                output: "out",
                output_bytes: 3,
                status: ToolResultStatus::Success,
                process: None,
                review_feedback: false,
                model_view_covers_full_file: false,
                permission_feedback: Vec::new(),
            }],
        }],
        steering: Vec::new(),
        files: &[],
        end: TurnEnd::Replied {
            text: "",
            provider_replay: None,
        },
    };
    let mut session = fixture.resume().unwrap();
    session.settle_open_recovery().unwrap();
    session
        .record_compaction(
            "<summary>before</summary>",
            HistoryCut {
                turns: 1,
                tool_steps: 0,
                steering: 0,
            },
            Some(&HistoryTurn {
                steps: Vec::new(),
                ..turn.clone()
            }),
            &provider,
        )
        .unwrap();
    let point = RecoveryPoint {
        turn_id: TurnId::new(2),
        turn,
        source: "",
        cause: ModelRecoveryCause::ConnectivityLost,
        progress: RecoveryProgress::Paused,
        tool_state: RecoveryToolState::None,
        model: "openai/gpt-5",
        requested_fast_mode: false,
        fast_mode: false,
        attempt_limit: 10,
        consumed_attempts: 1,
    };
    session
        .record_recovery(&point, &provider, RouteCredential::configured())
        .unwrap();
    assert!(session.turn_open());
    assert!(session.holds_recovery());
    session.settle_open_recovery().unwrap();
    assert!(!session.turn_open());
    assert!(!session.holds_recovery());
    assert!(!fixture.path(RECOVERY_FILE).exists());
    let log = fixture.log();
    let kinds: Vec<&str> = log
        .iter()
        .map(|line| {
            [
                "user",
                "context_checkpoint",
                "tool_call",
                "tool_result",
                "interrupted",
                "assistant",
                "turn_completed",
            ]
            .into_iter()
            .find(|kind| line.contains(&format!("\"event\":{{\"{kind}\"")))
            .unwrap_or("other")
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "user",
            "assistant",
            "turn_completed",
            "user",
            "context_checkpoint",
            "tool_call",
            "tool_result",
            "interrupted"
        ],
        "{log:#?}"
    );
}

const FEEDBACK: &str = "run it from the workspace root";

fn feedback_follows_its_result(messages: &[ChatMessage]) -> bool {
    messages.windows(2).any(|pair| {
        matches!(&pair[0], ChatMessage::Tool { call_id, .. } if call_id.as_str() == "c2")
            && pair[1] == ChatMessage::permission_feedback(ToolCallId::new("c2"), FEEDBACK)
    })
}

#[test]
fn a_paused_turn_keeps_its_approval_feedback_when_committed() {
    for reopened in [false, true] {
        let fixture = Fixture::new();
        fixture.start(&finished_turn());
        let provider = metadata().preferences.provider;
        let calls = vec![ToolCall::new("c2", "shell", "{\"command\":\"ls\"}")];
        let turn = HistoryTurn {
            user: "fix the build",
            steps: vec![HistoryStep {
                assistant: "",
                provider_replay: None,
                tool_calls: &calls,
                tool_results: vec![StepResult {
                    call_id: "c2",
                    tool_name: "shell",
                    output: "out",
                    output_bytes: 3,
                    status: ToolResultStatus::Success,
                    process: None,
                    review_feedback: false,
                    model_view_covers_full_file: false,
                    permission_feedback: vec![FEEDBACK],
                }],
            }],
            steering: Vec::new(),
            files: &[],
            end: replied(""),
        };
        let mut session = fixture.resume().unwrap();
        session
            .record_compaction(
                "<summary>before</summary>",
                HistoryCut {
                    turns: 1,
                    tool_steps: 0,
                    steering: 0,
                },
                Some(&HistoryTurn {
                    steps: Vec::new(),
                    ..turn.clone()
                }),
                &provider,
            )
            .unwrap();
        let point = RecoveryPoint {
            turn_id: TurnId::new(2),
            turn,
            source: "",
            cause: ModelRecoveryCause::ConnectivityLost,
            progress: RecoveryProgress::Paused,
            tool_state: RecoveryToolState::None,
            model: "openai/gpt-5",
            requested_fast_mode: false,
            fast_mode: false,
            attempt_limit: 10,
            consumed_attempts: 1,
        };
        session
            .record_recovery(&point, &provider, RouteCredential::configured())
            .unwrap();
        if reopened {
            drop(session);
            session = fixture.resume().unwrap();
            assert_eq!(
                session.recovery_transcript().unwrap().entries,
                [
                    HistoryEntry::User("fix the build".to_owned()),
                    HistoryEntry::User(FEEDBACK.to_owned()),
                ]
            );
        }
        session.settle_open_recovery().unwrap();
        assert!(!session.holds_recovery());
        let log = fixture.log();
        assert!(
            log.iter().any(|line| line.contains("\"tool_result\":{")
                && line.contains(&format!("\"permission_feedback\":[\"{FEEDBACK}\"]"))),
            "{log:#?}"
        );
        assert!(feedback_follows_its_result(
            &session.restored_history().unwrap().messages
        ));
        drop(session);
        let mut resumed = fixture.resume().unwrap();
        assert!(feedback_follows_its_result(
            &resumed.restored_history().unwrap().messages
        ));
    }
}

#[test]
fn a_paused_turn_keeps_its_review_feedback_when_committed() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    let provider = metadata().preferences.provider;
    let calls = vec![ToolCall::new(
        "c2",
        "shell",
        "{\"command\":\"rm -rf build\"}",
    )];
    let turn = HistoryTurn {
        user: "clean up",
        steps: vec![HistoryStep {
            assistant: "",
            provider_replay: None,
            tool_calls: &calls,
            tool_results: vec![StepResult {
                call_id: "c2",
                tool_name: "shell",
                output: "held",
                output_bytes: 4,
                status: ToolResultStatus::Failure,
                process: None,
                review_feedback: true,
                model_view_covers_full_file: false,
                permission_feedback: Vec::new(),
            }],
        }],
        steering: Vec::new(),
        files: &[],
        end: replied(""),
    };
    let point = RecoveryPoint {
        turn_id: TurnId::new(2),
        turn,
        source: "",
        cause: ModelRecoveryCause::ConnectivityLost,
        progress: RecoveryProgress::Paused,
        tool_state: RecoveryToolState::None,
        model: "openai/gpt-5",
        requested_fast_mode: false,
        fast_mode: false,
        attempt_limit: 10,
        consumed_attempts: 1,
    };
    let mut session = fixture.resume().unwrap();
    session
        .record_recovery(&point, &provider, RouteCredential::configured())
        .unwrap();
    drop(session);
    let mut session = fixture.resume().unwrap();
    session.settle_recovery().unwrap();
    let log = fixture.log();
    assert!(
        log.iter().any(|line| line
            .contains("\"call_id\":\"c2\",\"tool_name\":\"shell\",\"status\":\"failure\"")
            && line
                .contains("\"provider_native\":false,\"review_feedback\":true,\"created_at_ms\"")),
        "{log:#?}"
    );
}

#[test]
fn a_paused_child_turn_keeps_its_work_id_when_committed() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    let provider = metadata().preferences.provider;
    let point = RecoveryPoint {
        turn_id: TurnId::new(2),
        turn: HistoryTurn {
            user: "summarize the logs",
            steps: Vec::new(),
            steering: Vec::new(),
            files: &[],
            end: replied(""),
        },
        source: "Half",
        cause: ModelRecoveryCause::ConnectivityLost,
        progress: RecoveryProgress::Paused,
        tool_state: RecoveryToolState::None,
        model: "openai/gpt-5",
        requested_fast_mode: false,
        fast_mode: false,
        attempt_limit: 10,
        consumed_attempts: 1,
    };
    let mut session = fixture.resume().unwrap();
    session.begin_work("work-1");
    session
        .record_recovery(&point, &provider, RouteCredential::configured())
        .unwrap();
    drop(session);
    let mut session = fixture.resume().unwrap();
    session.settle_recovery().unwrap();
    let log = fixture.log();
    assert!(
        log.iter().any(|line| line.contains(
            "\"user\":{\"text\":\"summarize the logs\",\"images\":[],\"work_id\":\"work-1\"}"
        )),
        "{log:#?}"
    );
}

#[test]
fn a_continued_turn_paused_again_is_committed_before_the_next_prompt_is_saved() {
    let fixture = Fixture::new();
    let mut started = start_session(&fixture.sessions, metadata()).unwrap();
    started.append(2, &finished_turn()).unwrap();
    started
        .append(
            3,
            &[
                user("fix the build"),
                ConversationEvent::ContextCheckpoint(ContextCheckpointEvent {
                    covers_through_seq: 3,
                    summary: "<summary>before</summary>".to_owned(),
                }),
            ],
        )
        .unwrap();
    drop(started);
    fixture.save_checkpoint(5, &checkpoint("fix the build", &[], "", ""));
    let provider = metadata().preferences.provider;
    let mut session = fixture.resume().unwrap();
    let continued = session
        .take_recovery()
        .unwrap()
        .into_turn(&provider, "openai/gpt-5", false);
    let point = RecoveryPoint {
        turn_id: TurnId::new(2),
        turn: continued_history(&continued, replied("")),
        source: "",
        cause: ModelRecoveryCause::ConnectivityLost,
        progress: RecoveryProgress::Paused,
        tool_state: RecoveryToolState::None,
        model: "openai/gpt-5",
        requested_fast_mode: false,
        fast_mode: false,
        attempt_limit: 10,
        consumed_attempts: 1,
    };
    session
        .record_recovery(&point, &provider, RouteCredential::configured())
        .unwrap();
    assert!(session.holds_recovery());
    session.settle_open_recovery().unwrap();
    assert!(!session.turn_open());
    session
        .record_turn(
            &HistoryTurn {
                user: "next question",
                steps: Vec::new(),
                steering: Vec::new(),
                files: &[],
                end: replied("Moved on."),
            },
            &provider,
        )
        .unwrap();
    let log = fixture.log();
    assert_eq!(log.len(), 9, "{log:#?}");
    assert!(log[5].contains("\"interrupted\":{"), "{}", log[5]);
    assert!(
        log[6].contains("\"user\":{\"text\":\"next question\""),
        "{}",
        log[6]
    );
    assert!(log[7].contains("Moved on."), "{}", log[7]);
    assert!(!fixture.path(RECOVERY_FILE).exists());
}

#[test]
fn a_continued_turn_compacted_then_paused_is_committed_before_the_next_prompt_is_saved() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    fixture.save_checkpoint(3, &checkpoint("fix the build", &[], "", ""));
    let provider = metadata().preferences.provider;
    let mut session = fixture.resume().unwrap();
    let continued = session
        .take_recovery()
        .unwrap()
        .into_turn(&provider, "openai/gpt-5", false);
    let turn = continued_history(&continued, replied(""));
    session
        .record_compaction(
            "<summary>before</summary>",
            HistoryCut {
                turns: 1,
                tool_steps: 0,
                steering: 0,
            },
            Some(&turn),
            &provider,
        )
        .unwrap();
    assert!(session.turn_open());
    let point = RecoveryPoint {
        turn_id: TurnId::new(2),
        turn,
        source: "",
        cause: ModelRecoveryCause::ConnectivityLost,
        progress: RecoveryProgress::Paused,
        tool_state: RecoveryToolState::None,
        model: "openai/gpt-5",
        requested_fast_mode: false,
        fast_mode: false,
        attempt_limit: 10,
        consumed_attempts: 1,
    };
    session
        .record_recovery(&point, &provider, RouteCredential::configured())
        .unwrap();
    session.settle_open_recovery().unwrap();
    assert!(!session.turn_open());
    session
        .record_turn(
            &HistoryTurn {
                user: "next question",
                steps: Vec::new(),
                steering: Vec::new(),
                files: &[],
                end: replied("Moved on."),
            },
            &provider,
        )
        .unwrap();
    let log = fixture.log();
    assert_eq!(log.len(), 9, "{log:#?}");
    assert!(
        log[3].contains("\"user\":{\"text\":\"fix the build\""),
        "{}",
        log[3]
    );
    assert!(log[4].contains("\"context_checkpoint\":{"), "{}", log[4]);
    assert!(log[5].contains("\"interrupted\":{"), "{}", log[5]);
    assert!(
        log[6].contains("\"user\":{\"text\":\"next question\""),
        "{}",
        log[6]
    );
    assert!(!fixture.path(RECOVERY_FILE).exists());
}

#[test]
fn a_refused_credential_leaves_the_checkpoint_pending_until_it_is_settled() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    fixture.save_checkpoint(3, &interrupted_checkpoint());
    let mut resumed = fixture.resume().unwrap();
    assert!(matches!(
        resumed.take_authorized_recovery(RouteCredential::configured()),
        Err(SessionError::RecoveryCredentialAuthorityChanged)
    ));
    assert!(resumed.recovery_transcript().is_some());
    resumed.settle_recovery().unwrap();
    assert!(!fixture.path(RECOVERY_FILE).exists());
    assert!(matches!(
        resumed.take_authorized_recovery(RouteCredential::configured()),
        Err(SessionError::NoPendingRecovery)
    ));

    let unsent = interrupted_checkpoint().replace(
        "\"consumed_provider_attempts\":1",
        "\"consumed_provider_attempts\":0",
    );
    fixture.save_checkpoint(u64::try_from(fixture.log().len()).unwrap(), &unsent);
    drop(resumed);
    let mut resumed = fixture.resume().unwrap();
    let pending = resumed
        .take_authorized_recovery(RouteCredential::configured())
        .unwrap();
    assert_eq!(pending.prompt(), "fix the build");
    assert!(resumed.recovery_transcript().is_none());
}

#[test]
fn a_pending_checkpoint_shows_its_prompt_replies_and_steering() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    let spoken = step("c2", "out").replace("\"assistant\":null", "\"assistant\":\"Checking.\"");
    let uncertain = checkpoint(
        "fix the build",
        &[spoken],
        "{\"text\":\"also tests\",\"assistant_prefix\":null,\"after_tool_step_count\":1}",
        "Looking at",
    )
    .replace("\"tool_state\":\"none\"", "\"tool_state\":\"uncertain\"");
    fixture.save_checkpoint(3, &uncertain);
    let resumed = fixture.resume().unwrap();
    let shown = resumed.recovery_transcript().unwrap();
    assert!(shown.uncertain_tool);
    assert_eq!(
        shown.entries,
        [
            HistoryEntry::User("fix the build".to_owned()),
            HistoryEntry::Assistant("Checking.".to_owned()),
            HistoryEntry::User("also tests".to_owned()),
            HistoryEntry::Assistant("Looking at".to_owned()),
        ]
    );
    fixture.save_checkpoint(3, &checkpoint("fix the build", &[], "", ""));
    drop(resumed);
    let resumed = fixture.resume().unwrap();
    assert!(!resumed.recovery_transcript().unwrap().uncertain_tool);
}

#[test]
fn the_recovery_ask_marker_lasts_until_the_checkpoint_changes() {
    let fixture = Fixture::new();
    fixture.start(&finished_turn());
    fixture.save_checkpoint(3, &checkpoint("fix the build", &[], "", ""));
    fs::remove_file(fixture.path(RECOVERY_ASKED_FILE)).unwrap();
    let resumed = fixture.resume().unwrap();
    assert!(!resumed.recovery_was_asked());
    resumed.mark_recovery_asked();
    let marker = fs::read_to_string(fixture.path(RECOVERY_ASKED_FILE)).unwrap();
    assert!(marker.starts_with("{\"asked_at_ms\":"), "{marker}");
    assert!(marker.ends_with("}\n"), "{marker}");
    drop(resumed);
    let mut resumed = fixture.resume().unwrap();
    assert!(resumed.recovery_was_asked());
    resumed.discard_recovery();
    assert!(!resumed.recovery_was_asked());
}
