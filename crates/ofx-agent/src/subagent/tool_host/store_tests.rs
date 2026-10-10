use std::sync::Mutex;

use ofx_contract::{
    ChatMessage, ConversationLog, HistoryCut, HistoryTurn, LogFailure, RecoveryPoint,
    RestoredHistory, SubagentRequest, ToolCallId, ToolContext, ToolOutput, TurnEnd,
    format_tool_execution_error_json,
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
    resumed: Mutex<Vec<String>>,
    unreadable: Mutex<bool>,
}

#[derive(Default)]
struct Record {
    works: Mutex<Vec<String>>,
    turns: Arc<Mutex<Vec<String>>>,
    unsaved: bool,
    replies: Arc<Mutex<Vec<(String, String)>>>,
}

struct Log {
    turns: Arc<Mutex<Vec<String>>>,
    unsaved: bool,
    replies: Arc<Mutex<Vec<(String, String)>>>,
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

    fn record(&self, child_id: &str) -> Arc<Record> {
        let index = self
            .started
            .lock()
            .unwrap()
            .iter()
            .position(|(id, _)| id == child_id)
            .unwrap();
        Arc::clone(&self.records.lock().unwrap()[index])
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

    fn load_registry(&self) -> Result<Option<Vec<u8>>, LogFailure> {
        if *self.unreadable.lock().unwrap() {
            return Ok(Some(b"{".to_vec()));
        }
        Ok(self.saved.lock().unwrap().last().cloned())
    }

    fn resume_child(&self, child_id: &str) -> Result<ResumedChild, LogFailure> {
        self.resumed.lock().unwrap().push(child_id.to_owned());
        let settings = self
            .started
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| id == child_id)
            .map(|(_, settings)| settings.clone())
            .unwrap();
        let record = self.record(child_id);
        let mut messages = Vec::new();
        let mut turn_starts = Vec::new();
        for (user, reply) in record.replies.lock().unwrap().iter() {
            turn_starts.push(messages.len());
            messages.push(ChatMessage::user(user.clone()));
            messages.push(ChatMessage::Assistant {
                content: Some(reply.clone()),
                tool_calls: Vec::new(),
                provider_replay: None,
            });
        }
        Ok(ResumedChild {
            record,
            settings,
            history: RestoredHistory {
                checkpoint: None,
                compaction_count: 0,
                messages,
                turn_starts,
            },
        })
    }

    fn reply_for_work(&self, child_id: &str, work_id: &str) -> Result<Option<String>, LogFailure> {
        let record = self.record(child_id);
        let works = record.works.lock().unwrap();
        let replies = record.replies.lock().unwrap();
        Ok(works
            .iter()
            .position(|work| work == work_id)
            .map(|index| replies[index].1.clone()))
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
            replies: Arc::clone(&self.replies),
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
        if let TurnEnd::Replied { text, .. } = turn.end {
            self.replies
                .lock()
                .unwrap()
                .push((turn.user.to_owned(), text.to_owned()));
        }
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

    fn record_recovery(&self, _point: &RecoveryPoint<'_>) -> Result<(), LogFailure> {
        Ok(())
    }

    fn clear_recovery(&self) -> Result<(), LogFailure> {
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

#[tokio::test]
async fn a_resumed_parent_continues_its_named_children_and_interrupts_unfinished_work() {
    let store = Store::new("parent");
    let earlier = Harness::new(vec![Script::Reply("a looks fine"), Script::Hold]);
    earlier
        .host
        .bind(Some(Arc::clone(&store) as Arc<dyn ChildStore>));
    assert_eq!(
        earlier
            .run("call-1", message("reviewer", None, "review a"))
            .await,
        succeeded("a looks fine")
    );
    let cancel = CancellationToken::new();
    let held = tokio::spawn(earlier.call("call-2", run("never finishes"), &cancel));
    earlier.provider.holding.notified().await;
    held.abort();
    let saved = store.last_saved();
    assert_eq!(saved["children"][1]["phase"], "running");
    let resumed = Harness::new(vec![Script::Reply("b has a bug")]);
    resumed
        .host
        .bind(Some(Arc::clone(&store) as Arc<dyn ChildStore>));
    let recovered = store.last_saved();
    assert_eq!(
        recovered["generation"],
        saved["generation"].as_u64().unwrap() + 1
    );
    assert_eq!(recovered["children"][1]["phase"], "interrupted");
    assert_eq!(recovered["children"][1]["last_outcome"], "interrupted");
    assert_eq!(recovered["children"][1]["active"], Value::Null);
    assert_eq!(
        resumed
            .run("call-3", message("reviewer", None, "review b"))
            .await,
        succeeded("b has a bug")
    );
    assert_eq!(*store.resumed.lock().unwrap(), ["parent-child-1"]);
    assert_eq!(
        resumed.provider.seen()[0].messages,
        [
            ChatMessage::user("review a"),
            ChatMessage::Assistant {
                content: Some("a looks fine".to_owned()),
                tool_calls: Vec::new(),
                provider_replay: None,
            },
            ChatMessage::user("review b"),
        ]
    );
    let finished = store.last_saved();
    assert_eq!(finished["children"][0]["phase"], "idle");
    assert_eq!(finished["children"][0]["work_generation"], 2);
    assert_eq!(store.started.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_repeated_operation_after_a_resume_replays_the_childs_saved_reply() {
    let store = Store::new("parent");
    let earlier = Harness::new(vec![Script::Reply("done once")]);
    earlier
        .host
        .bind(Some(Arc::clone(&store) as Arc<dyn ChildStore>));
    assert_eq!(
        earlier.run("call-1", run("inspect")).await,
        succeeded("done once")
    );
    let resumed = Harness::new(Vec::new());
    resumed
        .host
        .bind(Some(Arc::clone(&store) as Arc<dyn ChildStore>));
    assert_eq!(
        resumed.run("call-1", run("inspect")).await,
        succeeded("done once")
    );
    assert!(resumed.provider.seen().is_empty());
}

#[tokio::test]
async fn an_unreadable_registry_leaves_delegation_unavailable() {
    let store = Store::new("parent");
    *store.unreadable.lock().unwrap() = true;
    let harness = Harness::new(vec![Script::Reply("fresh")]);
    harness
        .host
        .bind(Some(Arc::clone(&store) as Arc<dyn ChildStore>));
    assert_eq!(
        harness.run("call-1", run("inspect")).await,
        rejected("host_unavailable")
    );
    assert!(store.saved.lock().unwrap().is_empty());
    let readable = Store::new("other");
    harness
        .host
        .bind(Some(Arc::clone(&readable) as Arc<dyn ChildStore>));
    assert_eq!(
        harness.run("call-2", run("inspect")).await,
        succeeded("fresh")
    );
}

#[tokio::test]
async fn a_child_waiting_on_approval_is_saved_as_awaiting_it_until_its_work_ends() {
    let harness = Harness::new(vec![Script::Probe, Script::Reply("probed it")]);
    let store = Store::new("parent");
    harness
        .host
        .bind(Some(Arc::clone(&store) as Arc<dyn ChildStore>));
    let running = tokio::spawn(harness.call(
        "call-1",
        message("prober", None, "probe"),
        &CancellationToken::new(),
    ));
    harness.agents.asked.notified().await;
    let pending = store.last_saved();
    assert_eq!(pending["children"][0]["phase"], "awaiting_approval");
    assert_eq!(pending["generation"], 2);
    let request = harness.agents.requested.lock().unwrap()[0].id;
    harness
        .agents
        .approvals
        .resolve(request, ofx_contract::ApprovalDecision::Once);
    assert_eq!(running.await.unwrap(), succeeded("probed it"));
    let finished = store.last_saved();
    assert_eq!(finished["children"][0]["phase"], "idle");
    assert_eq!(finished["generation"], 3);
}

#[tokio::test]
async fn a_child_whose_approval_phase_cannot_be_saved_fails_without_asking() {
    let harness = Harness::new(vec![Script::Probe, Script::Reply("probed it")]);
    let store = Store::new("parent");
    harness
        .host
        .bind(Some(Arc::clone(&store) as Arc<dyn ChildStore>));
    *store.refused_attempt.lock().unwrap() = Some(2);
    assert_eq!(
        harness
            .run("call-1", message("prober", None, "probe"))
            .await,
        ToolOutput::failure(
            SubagentResult {
                result: Some(
                    "Subagent failed: agent_turn_failed: SessionCommitFailed. Earlier tool calls may have completed; their effects are not rolled back."
                ),
                ..SubagentResult::failure("child_failed")
            }
            .encode()
        )
    );
    assert!(harness.agents.requested.lock().unwrap().is_empty());
    assert_eq!(harness.provider.seen().len(), 1);
    let finished = store.last_saved();
    assert_eq!(finished["children"][0]["phase"], "idle");
    assert_eq!(finished["children"][0]["last_outcome"], "failed");
}
