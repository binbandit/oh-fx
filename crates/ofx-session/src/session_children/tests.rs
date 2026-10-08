use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use ofx_config::ProviderId;
use ofx_contract::{
    HistoryCut, HistoryStep, HistoryTurn, ModelRecoveryCause, ReasoningEffort, RecoveryPoint,
    RecoveryProgress, RecoveryToolState, StepResult, ToolCall, ToolResultStatus, TurnEnd, TurnId,
    TurnStop,
};

use crate::session_codec::recovery_checkpoint::RouteCredential;
use crate::session_codec::{SavedProvider, SessionPreferences, decode_session_metadata};
use crate::session_error::SessionError;
use crate::session_store::{ListScope, SessionStore};

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn store(&self) -> SessionStore {
        SessionStore::open(&self.root.path().join("data"), "/workspace").unwrap()
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.root.path().join("data/sessions").join(id)
    }
}

fn preferences() -> SessionPreferences {
    SessionPreferences {
        provider: SavedProvider::new(ProviderId::Gateway, None).unwrap(),
        model: "openai/gpt-5".to_owned(),
        effort: ReasoningEffort::Auto,
        fast_mode: false,
    }
}

fn mode(path: &PathBuf) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

fn replied(user: &str) -> HistoryTurn<'_> {
    HistoryTurn {
        user,
        steps: Vec::new(),
        steering: Vec::new(),
        files: &[],
        end: TurnEnd::Replied {
            text: "done",
            provider_replay: None,
        },
    }
}

#[test]
fn a_child_session_is_marked_as_upstream_marks_it() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let parent = store.start(preferences()).unwrap();
    let children = store.children(parent.id()).unwrap();
    let child = children.start("child-1", preferences(), "en").unwrap();
    assert_eq!(child.id(), "child-1");
    assert!(child.metadata().subagent_child);
    let manifest = fs::read(fixture.dir("child-1").join("session.json")).unwrap();
    assert!(decode_session_metadata(&manifest).unwrap().subagent_child);
    let control = fixture.dir("child-1").join("subagent");
    assert_eq!(mode(&control), 0o700);
    assert_eq!(mode(&control.join("owner.json")), 0o600);
    assert_eq!(
        fs::read_to_string(control.join("owner.json")).unwrap(),
        format!("{{\"schema_version\":1,\"parent_id\":\"{}\"}}", parent.id())
    );
}

#[test]
fn a_child_turn_records_the_work_it_belongs_to() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let parent = store.start(preferences()).unwrap();
    let children = store.children(parent.id()).unwrap();
    let mut child = children.start("child-1", preferences(), "en").unwrap();
    let provider = SavedProvider::new(ProviderId::Gateway, None).unwrap();
    child.begin_work("call_1");
    child
        .record_turn(&replied("read the notes"), &provider)
        .unwrap();
    child.begin_work("call_2");
    child.record_turn(&replied("again"), &provider).unwrap();
    let events = fs::read_to_string(fixture.dir("child-1").join("events.jsonl")).unwrap();
    let users: Vec<&str> = events
        .lines()
        .filter(|line| line.contains("\"user\""))
        .collect();
    assert_eq!(users.len(), 2);
    assert!(
        users[0].contains(
            "{\"user\":{\"text\":\"read the notes\",\"images\":[],\"work_id\":\"call_1\"}}"
        )
    );
    assert!(
        users[1].contains("{\"user\":{\"text\":\"again\",\"images\":[],\"work_id\":\"call_2\"}}")
    );
}

#[test]
fn the_registry_lives_beside_the_parent_session() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let parent = store.start(preferences()).unwrap();
    let children = store.children(parent.id()).unwrap();
    assert_eq!(children.load_registry(), Ok(None));
    children.save_registry(b"{\"first\":1}").unwrap();
    children.save_registry(b"{\"second\":2}").unwrap();
    let control = fixture.dir(parent.id()).join("subagent");
    assert_eq!(mode(&control), 0o700);
    assert_eq!(mode(&control.join("children.json")), 0o600);
    assert_eq!(mode(&control.join("children.lock")), 0o600);
    assert_eq!(
        fs::read(control.join("children.json")).unwrap(),
        b"{\"second\":2}"
    );
    assert_eq!(
        children.load_registry(),
        Ok(Some(b"{\"second\":2}".to_vec()))
    );
    assert_eq!(
        children.save_registry(&vec![b' '; 512 * 1024 + 1]),
        Err(SessionError::SessionCommitFailed)
    );
}

#[test]
fn child_sessions_are_hidden_from_listings_latest_and_direct_resume() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let provider = SavedProvider::new(ProviderId::Gateway, None).unwrap();
    let mut parent = store.start(preferences()).unwrap();
    parent.record_turn(&replied("delegate"), &provider).unwrap();
    let parent_id = parent.id().to_owned();
    let children = store.children(&parent_id).unwrap();
    let mut child = children.start("child-1", preferences(), "en").unwrap();
    child.begin_work("call_1");
    child.record_turn(&replied("read"), &provider).unwrap();
    drop(child);
    drop(parent);
    let page = store
        .catalog()
        .unwrap()
        .page(ListScope::AllWorkspaces, None, None, 10);
    let listed: Vec<&str> = page
        .summaries
        .iter()
        .map(|summary| summary.id.as_str())
        .collect();
    assert_eq!(listed, [parent_id.as_str()]);
    assert_eq!(store.resume_latest().unwrap().id(), parent_id);
    assert_eq!(
        store.resume("child-1").err(),
        Some(SessionError::OneOffSessionNotResumable)
    );
}

#[test]
fn a_session_marked_only_by_its_owner_file_is_still_a_child() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let provider = SavedProvider::new(ProviderId::Gateway, None).unwrap();
    let mut parent = store.start(preferences()).unwrap();
    parent.record_turn(&replied("delegate"), &provider).unwrap();
    let parent_id = parent.id().to_owned();
    drop(parent);
    let mut other = store.start(preferences()).unwrap();
    other.record_turn(&replied("read"), &provider).unwrap();
    let other_id = other.id().to_owned();
    drop(other);
    let control = fixture.dir(&other_id).join("subagent");
    fs::create_dir(&control).unwrap();
    fs::set_permissions(&control, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        control.join("owner.json"),
        format!("{{\"schema_version\":1,\"parent_id\":\"{parent_id}\"}}"),
    )
    .unwrap();
    fs::set_permissions(
        control.join("owner.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let page = store
        .catalog()
        .unwrap()
        .page(ListScope::AllWorkspaces, None, None, 10);
    let listed: Vec<&str> = page
        .summaries
        .iter()
        .map(|summary| summary.id.as_str())
        .collect();
    assert_eq!(listed, [parent_id.as_str()]);
    assert_eq!(
        store.resume(&other_id).err(),
        Some(SessionError::OneOffSessionNotResumable)
    );
}

#[test]
fn a_child_session_reopens_with_its_conversation_and_settings() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let parent = store.start(preferences()).unwrap();
    let children = store.children(parent.id()).unwrap();
    let mut chosen = preferences();
    chosen.model = "openai/gpt-5-mini".to_owned();
    chosen.effort = ReasoningEffort::Named("high".to_owned());
    let mut child = children.start("child-1", chosen.clone(), "en").unwrap();
    let provider = SavedProvider::new(ProviderId::Gateway, None).unwrap();
    child.begin_work("call_1");
    child
        .record_turn(&replied("read the notes"), &provider)
        .unwrap();
    drop(child);
    let mut reopened = children.resume("child-1").unwrap();
    assert_eq!(reopened.metadata().preferences, chosen);
    let history = reopened.restored_history().unwrap();
    assert_eq!(history.turn_starts, [0]);
    assert_eq!(history.messages.len(), 2);
    let parent_id = parent.id().to_owned();
    drop(parent);
    assert_eq!(
        children.resume(&parent_id).err(),
        Some(SessionError::SessionNotFound)
    );
}

#[test]
fn a_childs_reply_is_found_by_the_work_it_answered() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let parent = store.start(preferences()).unwrap();
    let children = store.children(parent.id()).unwrap();
    let mut child = children.start("child-1", preferences(), "en").unwrap();
    let provider = SavedProvider::new(ProviderId::Gateway, None).unwrap();
    child.begin_work("call_1");
    child.record_turn(&replied("first"), &provider).unwrap();
    child.begin_work("call_2");
    child
        .record_turn(
            &HistoryTurn {
                user: "second",
                steps: Vec::new(),
                steering: Vec::new(),
                files: &[],
                end: TurnEnd::Stopped {
                    reason: TurnStop::Failed,
                    partial: "half an answer",
                },
            },
            &provider,
        )
        .unwrap();
    assert_eq!(
        children.reply_for_work("child-1", "call_1"),
        Ok(Some("done".to_owned()))
    );
    assert_eq!(
        children.reply_for_work("child-1", "call_2"),
        Ok(Some("half an answer".to_owned()))
    );
    assert_eq!(children.reply_for_work("child-1", "call_3"), Ok(None));
}

#[test]
fn a_child_reopened_over_a_compacted_turn_and_its_checkpoint_closes_that_work_first() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let parent = store.start(preferences()).unwrap();
    let children = store.children(parent.id()).unwrap();
    let mut child = children.start("child-1", preferences(), "en").unwrap();
    let provider = SavedProvider::new(ProviderId::Gateway, None).unwrap();
    let calls = [ToolCall::new("call-1", "shell", r#"{"command":"ls"}"#)];
    let active = HistoryTurn {
        user: "old task",
        steps: vec![HistoryStep {
            assistant: "",
            provider_replay: None,
            tool_calls: &calls,
            tool_results: vec![StepResult {
                call_id: "call-1",
                tool_name: "shell",
                output: "listing",
                output_bytes: 7,
                status: ToolResultStatus::Success,
                process: None,
                permission_feedback: Vec::new(),
                model_view_covers_full_file: false,
            }],
        }],
        steering: Vec::new(),
        files: &[],
        end: TurnEnd::Replied {
            text: "",
            provider_replay: None,
        },
    };
    child.begin_work("old-work");
    child
        .record_compaction(
            "<summary>listed</summary>",
            HistoryCut {
                turns: 0,
                tool_steps: 1,
                steering: 0,
            },
            Some(&active),
            &provider,
        )
        .unwrap();
    child
        .record_recovery(
            &RecoveryPoint {
                turn_id: TurnId::new(1),
                turn: active.clone(),
                source: "",
                cause: ModelRecoveryCause::RateLimited,
                progress: RecoveryProgress::Paused,
                tool_state: RecoveryToolState::None,
                model: "openai/gpt-5",
                requested_fast_mode: false,
                fast_mode: false,
                attempt_limit: 10,
                consumed_attempts: 10,
            },
            &provider,
            RouteCredential::configured(),
        )
        .unwrap();
    drop(child);
    let mut reopened = children.resume("child-1").unwrap();
    reopened.restored_history().unwrap();
    reopened.begin_work("new-work");
    reopened
        .record_turn(&replied("new task"), &provider)
        .unwrap();
    drop(reopened);
    assert_eq!(
        children.reply_for_work("child-1", "new-work"),
        Ok(Some("done".to_owned()))
    );
    assert_eq!(
        children.reply_for_work("child-1", "old-work"),
        Ok(Some(String::new()))
    );
    let mut again = children.resume("child-1").unwrap();
    let history = again.restored_history().unwrap();
    assert_eq!(history.turn_starts.len(), 2);
}
