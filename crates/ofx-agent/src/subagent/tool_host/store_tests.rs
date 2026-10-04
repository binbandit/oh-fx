use std::sync::Mutex;

use ofx_contract::{
    ChatMessage, ConversationLog, HistoryCut, HistoryTurn, LogFailure, SubagentRequest, ToolCallId,
    ToolContext, ToolOutput, format_tool_execution_error_json,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::tests::{Harness, Script, message, rejected, run, succeeded};
use super::*;

#[derive(Default)]
struct Store {
    parent: String,
    issued: Mutex<Vec<String>>,
    saved: Mutex<Vec<Vec<u8>>>,
    started: Mutex<Vec<(String, ChildSettings)>>,
    records: Mutex<Vec<Arc<Record>>>,
    refuse_saves: Mutex<bool>,
    attempts: Mutex<usize>,
    refused_attempt: Mutex<Option<usize>>,
    unsaved_turns: Mutex<bool>,
}

#[derive(Default)]
struct Record {
    works: Mutex<Vec<String>>,
    turns: Arc<Mutex<Vec<String>>>,
    unsaved: bool,
}

struct Log {
    turns: Arc<Mutex<Vec<String>>>,
    unsaved: bool,
}

fn commit_failed() -> LogFailure {
    LogFailure {
        code: "SessionCommitFailed".to_owned(),
    }
}

impl Store {
    fn new(parent: &str) -> Arc<Self> {
        Arc::new(Self {
            parent: parent.to_owned(),
            ..Self::default()
        })
    }

    fn last_saved(&self) -> Value {
        serde_json::from_slice(self.saved.lock().unwrap().last().unwrap()).unwrap()
    }
}

impl ChildStore for Store {
    fn parent_id(&self) -> &str {
        &self.parent
    }

    fn new_child_id(&self) -> Result<String, LogFailure> {
        let mut issued = self.issued.lock().unwrap();
        let id = format!("{}-child-{}", self.parent, issued.len() + 1);
        issued.push(id.clone());
        Ok(id)
    }

    fn save_registry(&self, registry: &[u8]) -> Result<(), LogFailure> {
        let mut attempts = self.attempts.lock().unwrap();
        *attempts += 1;
        if *self.refuse_saves.lock().unwrap()
            || *self.refused_attempt.lock().unwrap() == Some(*attempts)
        {
            return Err(commit_failed());
        }
        self.saved.lock().unwrap().push(registry.to_vec());
        Ok(())
    }

    fn start_child(
        &self,
        child_id: &str,
        settings: &ChildSettings,
    ) -> Result<Arc<dyn ChildRecord>, LogFailure> {
        self.started
            .lock()
            .unwrap()
            .push((child_id.to_owned(), settings.clone()));
        let record = Arc::new(Record {
            unsaved: *self.unsaved_turns.lock().unwrap(),
            ..Record::default()
        });
        self.records.lock().unwrap().push(Arc::clone(&record));
        Ok(record)
    }
}

impl ChildRecord for Record {
    fn begin_work(&self, work_id: &str) {
        self.works.lock().unwrap().push(work_id.to_owned());
    }

    fn log(&self) -> Box<dyn ConversationLog> {
        Box::new(Log {
            turns: Arc::clone(&self.turns),
            unsaved: self.unsaved,
        })
    }
}

impl ConversationLog for Log {
    fn require_writable(&self) -> Result<(), LogFailure> {
        Ok(())
    }

    fn record_turn(&mut self, turn: &HistoryTurn<'_>) -> Result<(), LogFailure> {
        if self.unsaved {
            return Err(commit_failed());
        }
        self.turns.lock().unwrap().push(turn.user.to_owned());
        Ok(())
    }

    fn record_compaction(
        &mut self,
        _checkpoint: &str,
        _cut: HistoryCut,
        _active: Option<&HistoryTurn<'_>>,
    ) -> Result<(), LogFailure> {
        Ok(())
    }
}

fn ids(saved: &Value) -> Vec<(String, String)> {
    saved["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|child| {
            (
                child["id"].as_str().unwrap().to_owned(),
                child["phase"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_bound_parent_keeps_its_registry_and_each_child_logs_into_its_own_session() {
    let harness = Harness::new(vec![
        Script::Reply("one-off done"),
        Script::Reply("reviewed"),
        Script::Reply("reviewed again"),
    ]);
    let store = Store::new("parent");
    harness
        .host
        .bind(Some(Arc::clone(&store) as Arc<dyn ChildStore>));
    assert_eq!(
        harness.run("call-1", run("inspect")).await,
        succeeded("one-off done")
    );
    assert_eq!(
        harness
            .run("call-2", message("reviewer", Some("Be terse."), "review a"))
            .await,
        succeeded("reviewed")
    );
    assert_eq!(
        harness
            .run("call-3", message("reviewer", None, "review b"))
            .await,
        succeeded("reviewed again")
    );
    let started: Vec<String> = store
        .started
        .lock()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(started, ["parent-child-1", "parent-child-2"]);
    let saved = store.last_saved();
    assert_eq!(saved["schema_version"], 2);
    assert_eq!(saved["parent_id"], "parent");
    assert_eq!(saved["generation"], 6);
    assert_eq!(
        ids(&saved),
        [
            ("parent-child-1".to_owned(), "finished".to_owned()),
            ("parent-child-2".to_owned(), "idle".to_owned()),
        ]
    );
    assert_eq!(saved["children"][1]["last_work_id"], operation_id("call-3"));
    assert_eq!(store.saved.lock().unwrap().len(), 6);
    let records = store.records.lock().unwrap();
    assert_eq!(*records[0].works.lock().unwrap(), [operation_id("call-1")]);
    assert_eq!(
        *records[1].works.lock().unwrap(),
        [operation_id("call-2"), operation_id("call-3")]
    );
    assert_eq!(*records[0].turns.lock().unwrap(), ["inspect"]);
    assert_eq!(*records[1].turns.lock().unwrap(), ["review a", "review b"]);
}

#[tokio::test]
async fn a_registry_that_cannot_be_saved_fails_the_call_and_keeps_no_child() {
    let harness = Harness::new(vec![Script::Reply("fresh")]);
    let store = Store::new("parent");
    harness
        .host
        .bind(Some(Arc::clone(&store) as Arc<dyn ChildStore>));
    *store.refuse_saves.lock().unwrap() = true;
    assert_eq!(
        harness
            .run("call-1", message("reviewer", None, "review"))
            .await,
        ToolOutput::failure(format_tool_execution_error_json(
            "subagent",
            "SessionCommitFailed"
        ))
    );
    *store.refuse_saves.lock().unwrap() = false;
    assert_eq!(
        harness
            .run("call-2", message("reviewer", None, "review"))
            .await,
        succeeded("fresh")
    );
    assert_eq!(ids(&store.last_saved()).len(), 1);
    assert_eq!(harness.provider.seen().len(), 1);
}

#[tokio::test]
async fn binding_another_parent_forgets_the_children_and_binding_the_same_one_keeps_them() {
    let harness = Harness::new(vec![
        Script::Reply("first"),
        Script::Reply("second"),
        Script::Reply("third"),
    ]);
    let first = Store::new("first");
    harness
        .host
        .bind(Some(Arc::clone(&first) as Arc<dyn ChildStore>));
    harness
        .run("call-1", message("reviewer", None, "one"))
        .await;
    harness
        .host
        .bind(Some(Arc::clone(&first) as Arc<dyn ChildStore>));
    harness
        .run("call-2", message("reviewer", None, "two"))
        .await;
    let second = Store::new("second");
    harness
        .host
        .bind(Some(Arc::clone(&second) as Arc<dyn ChildStore>));
    harness
        .run("call-3", message("reviewer", None, "three"))
        .await;
    assert_eq!(first.started.lock().unwrap().len(), 1);
    assert_eq!(second.started.lock().unwrap().len(), 1);
    let seen = harness.provider.seen();
    assert_eq!(seen[1].messages.len(), 3);
    assert_eq!(seen[2].messages, vec![ChatMessage::user("three")]);
}

#[tokio::test]
async fn an_unbound_host_keeps_its_children_in_memory() {
    let harness = Harness::new(vec![Script::Reply("done")]);
    let output = harness
        .host
        .execute(
            SubagentRequest::validate(run("inspect")).unwrap(),
            ToolContext::new(
                ToolCallId::new("call-1"),
                CancellationToken::new(),
                ofx_contract::PathAccess::WorkspaceOnly,
            ),
        )
        .await;
    assert_eq!(output, succeeded("done"));
}

fn bound(scripts: Vec<Script>) -> (Harness, Arc<Store>) {
    let harness = Harness::new(scripts);
    let store = Store::new("parent");
    harness
        .host
        .bind(Some(Arc::clone(&store) as Arc<dyn ChildStore>));
    (harness, store)
}

#[tokio::test]
async fn a_continuation_that_cannot_be_saved_keeps_the_named_child_and_its_conversation() {
    let (harness, store) = bound(vec![Script::Reply("first"), Script::Reply("second")]);
    assert_eq!(
        harness
            .run("call-1", message("reviewer", None, "one"))
            .await,
        succeeded("first")
    );
    *store.refuse_saves.lock().unwrap() = true;
    assert_eq!(
        harness
            .run("call-2", message("reviewer", None, "two"))
            .await,
        ToolOutput::failure(format_tool_execution_error_json(
            "subagent",
            "SessionCommitFailed"
        ))
    );
    *store.refuse_saves.lock().unwrap() = false;
    assert_eq!(
        harness
            .run("call-3", message("reviewer", None, "three"))
            .await,
        succeeded("second")
    );
    assert_eq!(
        harness.provider.seen()[1].messages,
        vec![
            ChatMessage::user("one"),
            ChatMessage::Assistant {
                content: Some("first".to_owned()),
                tool_calls: Vec::new(),
                provider_replay: None,
            },
            ChatMessage::user("three"),
        ]
    );
    assert_eq!(store.started.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn work_whose_finish_cannot_be_saved_reports_its_state_unavailable() {
    let (harness, store) = bound(vec![Script::Reply("done")]);
    *store.refused_attempt.lock().unwrap() = Some(2);
    assert_eq!(
        harness.run("call-1", run("inspect")).await,
        rejected("state_unavailable")
    );
    assert_eq!(
        ids(&store.last_saved()),
        [("parent-child-1".to_owned(), "running".to_owned())]
    );
    assert_eq!(
        harness.run("call-1", run("inspect")).await,
        rejected("state_unavailable")
    );
}

#[tokio::test]
async fn a_reply_the_childs_log_cannot_save_fails_the_work_and_keeps_the_reply() {
    let (harness, store) = bound(vec![Script::Reply("fixed it")]);
    *store.unsaved_turns.lock().unwrap() = true;
    assert_eq!(
        harness.run("call-1", run("fix it")).await,
        ToolOutput::failure(
            SubagentResult {
                result: Some(
                    "Subagent failed: agent_turn_failed: SessionCommitFailed. Earlier tool calls may have completed; their effects are not rolled back.\n\nPartial result:\nfixed it"
                ),
                ..SubagentResult::failure("child_failed")
            }
            .encode()
        )
    );
}

#[tokio::test]
async fn a_new_child_whose_admission_cannot_be_saved_starts_no_session() {
    let (harness, store) = bound(vec![Script::Reply("fresh")]);
    *store.refuse_saves.lock().unwrap() = true;
    harness
        .run("call-1", message("reviewer", None, "review"))
        .await;
    harness.run("call-2", run("inspect")).await;
    assert!(store.started.lock().unwrap().is_empty());
}
