use std::fs;
use std::path::{Path, PathBuf};
use std::process;

use ofx_config::ProviderId;
use ofx_contract::{HistoryStep, ReasoningEffort, StepResult, ToolCall, ToolResultStatus};
use serde_json::Value;

use super::*;
use crate::session_adapter::SessionsV2;
use crate::session_adapter::codec::decode_preferences;
use crate::session_log::ArchivedTurn;
use crate::session_summary_codec::SessionSource;

const WORKSPACE: &str = "/private/tmp/oh-fx-v2/workspace";

struct Fixture {
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().unwrap(),
        }
    }

    fn data(&self) -> PathBuf {
        self.directory.path().join("data")
    }

    fn root(&self) -> PathBuf {
        self.data().join("sessions").join("v2")
    }

    fn store(&self) -> SessionsV2 {
        SessionsV2::open(&self.data(), &self.directory.path().join("home"), WORKSPACE).unwrap()
    }

    fn start(&self, prompt: &str) -> SessionV2 {
        self.store().start_ask(&preferences(), prompt).unwrap()
    }

    fn lines(&self, id: &str) -> Vec<Value> {
        fs::read_to_string(self.root().join(id).join("log.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn preferences() -> SessionPreferences {
    SessionPreferences {
        provider: SavedProvider::new(ProviderId::Gateway, None).unwrap(),
        model: "spacexai/grok-4.7".to_owned(),
        effort: ReasoningEffort::Auto,
        fast_mode: false,
        ultrafast_mode: false,
    }
}

fn replied<'a>(user: &'a str, text: &'a str) -> HistoryTurn<'a> {
    HistoryTurn {
        user,
        steps: Vec::new(),
        steering: Vec::new(),
        files: &[],
        end: TurnEnd::Replied {
            text,
            provider_replay: None,
        },
    }
}

fn stopped<'a>(user: &'a str, reason: TurnStop, partial: &'a str) -> HistoryTurn<'a> {
    HistoryTurn {
        end: TurnEnd::Stopped { reason, partial },
        ..replied(user, "")
    }
}

fn kinds(lines: &[Value]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            match (
                line["kind"].as_str().unwrap(),
                line["type"].as_str(),
                line["key"].as_str(),
            ) {
                (kind, Some(item), _) => format!("{kind}:{item}"),
                (kind, _, Some(key)) => format!("{kind}:{key}"),
                (kind, None, None) => kind.to_owned(),
            }
        })
        .collect()
}

#[test]
fn a_replied_turn_commits_in_one_batch_as_fx_commits_it() {
    let fixture = Fixture::new();
    let session = fixture.start("List question A.");
    let id = session.id().to_owned();
    session
        .log()
        .record_turn(&replied("List question A.", "LIST_A1"))
        .unwrap();
    assert_eq!(session.close(), id);
    let lines = fixture.lines(&id);
    assert_eq!(
        kinds(&lines),
        [
            "session_created",
            "set:prefs",
            "set:permissions",
            "set:language",
            "turn_started",
            "item:user",
            "item:assistant",
            "item:turn_end",
            "turn_committed",
            "set:title",
            "closed"
        ]
    );
    assert_eq!(
        lines[1]["value"],
        serde_json::json!({"provider":"gateway","model":"spacexai/grok-4.7","effort":"auto","fast_mode":false})
    );
    assert_eq!(
        lines[2]["value"],
        serde_json::json!({"schema_version":2,"next_generation":1,"rules":[]})
    );
    assert_eq!(lines[3]["value"], "und-Latn");
    assert_eq!(
        lines[5]["data"],
        serde_json::json!({"text":"List question A.","images":[],"work_id":null})
    );
    assert_eq!(
        lines[6]["data"],
        serde_json::json!({"text":"LIST_A1","provider_replay":null,"standalone_response":false})
    );
    assert_eq!(lines[9]["value"], "List question A.");
    assert!(lines[1]["ts"].as_u64() <= lines[0]["ts"].as_u64());
    let batch: Vec<_> = lines[4..10].iter().map(|line| &line["ts"]).collect();
    assert!(batch.iter().all(|ts| *ts == batch[0]));
}

#[test]
fn the_seed_language_follows_the_prompt_and_preferences_keep_upstreams_shape() {
    let fixture = Fixture::new();
    let mut saved = preferences();
    saved.provider =
        SavedProvider::new(ProviderId::Configured("portkey".to_owned()), Some([7; 32])).unwrap();
    saved.effort = ReasoningEffort::parse("high").unwrap();
    saved.fast_mode = true;
    saved.ultrafast_mode = true;
    let session = fixture
        .store()
        .start_ask(&saved, "Проверь файл, пожалуйста.")
        .unwrap();
    let id = session.id().to_owned();
    session
        .log()
        .record_turn(&replied("Проверь файл, пожалуйста.", "ok"))
        .unwrap();
    session.close();
    let lines = fixture.lines(&id);
    assert_eq!(lines[3]["value"], "und-Cyrl");
    let prefs = serde_json::to_vec(&lines[1]["value"]).unwrap();
    assert_eq!(
        String::from_utf8(prefs.clone()).unwrap(),
        format!(
            "{{\"provider\":{{\"name\":\"portkey\",\"binding\":\"{}\"}},\"model\":\"spacexai/grok-4.7\",\"effort\":\"high\",\"fast_mode\":true,\"ultrafast_mode\":true}}",
            "07".repeat(32)
        )
    );
    assert!(decode_preferences(&prefs).is_ok_and(|decoded| decoded == saved));
}

#[test]
fn a_tool_turn_opens_first_stores_each_result_as_a_blob_and_lists_it_on_the_first_item() {
    let fixture = Fixture::new();
    let session = fixture.start("Read the notes.");
    let id = session.id().to_owned();
    let calls = [ToolCall::new(
        "call_1",
        "read_file",
        "{\"path\":\"notes.txt\"}",
    )];
    let output = "<path>notes.txt</path>\nNOTE_TEXT";
    let turn = HistoryTurn {
        steps: vec![HistoryStep {
            assistant: "",
            provider_replay: None,
            tool_calls: &calls,
            tool_results: vec![StepResult {
                call_id: "call_1",
                tool_name: "read_file",
                output,
                output_bytes: output.len(),
                status: ToolResultStatus::Success,
                model_view_covers_full_file: true,
                process: None,
                permission_feedback: Vec::new(),
            }],
        }],
        ..replied("Read the notes.", "Read it.")
    };
    session.log().record_turn(&turn).unwrap();
    session.close();
    let hash = blob_hash(output.as_bytes());
    let lines = fixture.lines(&id);
    assert_eq!(
        kinds(&lines)[4..],
        [
            "turn_started",
            "item:user",
            "item:tool_call",
            "item:tool_result",
            "item:assistant",
            "item:turn_end",
            "turn_committed",
            "set:title",
            "closed"
        ]
    );
    assert_eq!(lines[5]["blobs"], serde_json::json!([hash]));
    assert!(lines[6].get("blobs").is_none());
    assert_eq!(
        lines[7]["data"]["artifact_ref"],
        format!("result-{hash}.txt")
    );
    assert_eq!(lines[7]["data"]["stored_bytes"], output.len());
    assert_eq!(
        fs::read(fixture.root().join(&id).join("blobs").join(&hash)).unwrap(),
        output.as_bytes()
    );
    let archive = fixture.store().archive(&id).unwrap();
    let [ArchivedTurn::Replied { execution, .. }] = archive.turns.as_slice() else {
        panic!("one replied turn");
    };
    assert_eq!(
        execution.steps[0].results[0].output_handle.as_deref(),
        Some(format!("result-{hash}.txt").as_str())
    );
}

#[test]
fn a_cancelled_turn_ends_interrupted_and_a_failed_turn_that_did_nothing_writes_nothing() {
    let fixture = Fixture::new();
    let idle = fixture.start("Never answered.");
    idle.log()
        .record_turn(&stopped("Never answered.", TurnStop::Failed, ""))
        .unwrap();
    assert_eq!(idle.close(), "");
    assert!(!fixture.data().exists());
    let session = fixture.start("Stop halfway.");
    let id = session.id().to_owned();
    session
        .log()
        .record_turn(&stopped("Stop halfway.", TurnStop::Cancelled, "half"))
        .unwrap();
    assert_eq!(session.close(), id);
    let lines = fixture.lines(&id);
    assert_eq!(
        kinds(&lines)[4..],
        [
            "turn_started",
            "item:user",
            "item:interruption",
            "turn_interrupted",
            "set:title",
            "closed"
        ]
    );
    assert_eq!(lines[7]["reason"], "cancel");
    assert_eq!(lines[6]["data"]["reason"], "cancelled");
}

#[test]
fn a_written_session_lists_and_reads_back_through_the_v2_store() {
    let fixture = Fixture::new();
    let session = fixture.start("First question.");
    let id = session.id().to_owned();
    let mut log = session.log();
    log.record_turn(&replied("First question.", "First answer."))
        .unwrap();
    log.record_turn(&replied("Second question.", "Second answer."))
        .unwrap();
    drop(log);
    session.close();
    let catalog = fixture.store().catalog().unwrap();
    let listed = catalog
        .listed_page(crate::session_store::ListScope::AllWorkspaces, None, 10)
        .summaries;
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].id.as_str(), listed[0].source),
        (id.as_str(), SessionSource::OhFx)
    );
    assert_eq!(listed[0].title.as_deref(), Some("First question."));
    let archive = fixture.store().archive(&id).unwrap();
    let users: Vec<_> = archive
        .turns
        .iter()
        .map(|turn| match turn {
            ArchivedTurn::Replied {
                user, assistant, ..
            } => (user.as_str(), assistant.as_str()),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        users,
        [
            ("First question.", "First answer."),
            ("Second question.", "Second answer.")
        ]
    );
    let titles = kinds(&fixture.lines(&id))
        .iter()
        .filter(|kind| *kind == "set:title")
        .count();
    assert_eq!(titles, 1);
}

#[test]
fn the_log_keeps_no_recovery_and_counts_requests_in_memory() {
    let fixture = Fixture::new();
    let session = fixture.start("Hi.");
    let log = session.log();
    assert!(!log.keeps_recovery());
    assert_eq!(log.clear_recovery(), Ok(()));
    assert_eq!(log.require_writable(), Ok(()));
    let first = log.begin_request().unwrap();
    let second = log.begin_request().unwrap();
    assert_eq!((first.sequence, second.sequence), (1, 2));
    assert_eq!(log.finish_request(first, DeliveryOutcome::Unbilled), Ok(()));
    drop(log);
    assert_eq!(session.close(), "");
    assert!(!fixture.data().exists());
}

const TRACE_CHILD: &str = "OH_FX_SESSIONS_V2_WRITER_TRACE_CHILD";
const TRACE_TEST: &str = "session_adapter::session::tests::a_written_session_writes_upstreams_wiring_and_diagnostic_lines";

fn write_traced(root: &Path) {
    let store = SessionsV2::open(&root.join("data"), &root.join("home"), WORKSPACE).unwrap();
    let held = store.start_ask(&preferences(), "Never answered.").unwrap();
    held.close();
    let session = store.start_ask(&preferences(), "Question.").unwrap();
    let mut log = session.log();
    log.record_turn(&replied("Question.", "Answer.")).unwrap();
    log.record_turn(&stopped("Again.", TurnStop::Cancelled, ""))
        .unwrap();
    log.record_compaction("summary", HistoryCut::default(), None)
        .unwrap();
    drop(log);
    session.close();
}

#[test]
fn a_written_session_writes_upstreams_wiring_and_diagnostic_lines() {
    if let Some(root) = std::env::var_os(TRACE_CHILD) {
        ofx_trace::configure_from_env(Path::new(&root), None);
        write_traced(Path::new(&root));
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let trace = root.path().join("trace.log");
    let status = process::Command::new(std::env::current_exe().unwrap())
        .args([TRACE_TEST, "--exact", "--test-threads=1"])
        .env_clear()
        .env(TRACE_CHILD, root.path())
        .env("OH_FX_TRACE_LOG", &trace)
        .stdout(process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "{status:?}");
    let written = fs::read_to_string(&trace).unwrap();
    let lines: Vec<&str> = written
        .lines()
        .filter_map(|line| line.split_once(' ').map(|(_, rest)| rest))
        .filter(|line| line.contains("[wiring]") || line.contains("sessions_v2"))
        .collect();
    let ids: Vec<&str> = lines
        .iter()
        .filter_map(|line| line.strip_prefix("[wiring] action=Open session="))
        .filter_map(|rest| rest.split_once(' ').map(|(id, _)| id))
        .collect();
    let [held, saved] = ids[..] else {
        panic!("two opens: {written}");
    };
    let child = |line: &str| line.rsplit_once(" pid=").map(|(rest, _)| rest.to_owned());
    let expected = [
        "[wiring] action=StoreOpen session=-".to_owned(),
        format!("[wiring] action=Open session={held}"),
        format!(
            "[session] event=sessions_v2_diagnostic kind=held_lines_dropped session={held} count=3 offset=0"
        ),
        format!("[wiring] action=Close session={held}"),
        format!("[wiring] action=Open session={saved}"),
        format!("[wiring] action=BeginTurn session={saved}"),
        format!("[wiring] action=EndTurn session={saved} end=commit"),
        format!("[wiring] action=BeginTurn session={saved}"),
        format!("[wiring] action=EndTurn session={saved} end=interrupt"),
        format!("[wiring] action=Close session={saved}"),
        "[wiring] action=StoreClose session=-".to_owned(),
    ];
    let seen: Vec<String> = lines
        .iter()
        .map(|line| child(line).unwrap_or_else(|| (*line).to_owned()))
        .collect();
    assert_eq!(seen, expected, "{written}");
}
