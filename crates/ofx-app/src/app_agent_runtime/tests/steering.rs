use super::*;

const WRAPPER: &str = "<user_steering>\nApply this live user update to the current task.";

fn held_reply() -> Reply {
    Reply::held_sse(&chat_text_events(&["partial\n"])[..2])
}

fn user_texts(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn steered(text: &str) -> String {
    format!(
        "{WRAPPER} Continue working unless the user asks you to stop, the task is complete, or a genuine blocker prevents progress.\n\n{text}\n</user_steering>"
    )
}

fn started(events: &[UiEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, UiEvent::TurnStarted { .. }))
        .count()
}

async fn settle_commands(harness: &mut Harness) {
    harness.command("/stats");
    harness
        .until(|event| matches!(event, UiEvent::StatsRequested))
        .await;
}

async fn waiting_for_approval(harness: &mut Harness) {
    fs::write(harness.home.path().join("outside.txt"), "notes\n").unwrap();
    harness.submit("read it");
    harness.until(approval_requested).await;
}

fn approve(harness: &Harness) {
    let request_id = harness
        .seen
        .iter()
        .rev()
        .find_map(|event| match event {
            UiEvent::ApprovalRequested { request, .. } => Some(request.id),
            _ => None,
        })
        .unwrap();
    harness.send(UiCommand::Approval {
        request_id,
        decision: ApprovalDecision::Once,
    });
}

#[tokio::test]
async fn a_prompt_submitted_while_a_reply_streams_steers_that_turn() {
    let server = FakeServer::start([held_reply(), Reply::sse(&chat_text_events(&["Steered."]))]);
    let mut harness = Harness::start(&server).await;
    harness.submit("slow");
    harness
        .until(|event| matches!(event, UiEvent::AssistantText { .. }))
        .await;
    let turn_id = harness.running_turn();
    harness.submit("check the tests too");
    let events = harness
        .until(finished(TurnOutcome::Completed))
        .await
        .to_vec();
    assert_eq!(started(&events), 0);
    assert!(events.contains(&UiEvent::SteeringApplied {
        turn_id,
        prompt: 1,
        text: "check the tests too".to_owned(),
    }));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = requests[1].json();
    assert_eq!(
        user_texts(&body),
        ["slow".to_owned(), steered("check the tests too")]
    );
    let assistant = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["content"], "partial\n");
}

#[tokio::test]
async fn steering_waits_for_a_running_tool_and_rides_the_next_request() {
    let server = FakeServer::start([outside_read(), Reply::sse(&chat_text_events(&["Read."]))]);
    let mut harness = Harness::start(&server).await;
    waiting_for_approval(&mut harness).await;
    harness.submit("only the first section");
    harness.submit("and summarize it");
    settle_commands(&mut harness).await;
    assert_eq!(server.requests().len(), 1);
    approve(&harness);
    let events = harness
        .until(finished(TurnOutcome::Completed))
        .await
        .to_vec();
    assert_eq!(started(&events), 0);
    let body = server.requests()[1].json();
    assert_eq!(
        user_texts(&body)[1..],
        [
            steered("only the first section"),
            steered("and summarize it")
        ]
    );
    let roles: Vec<&str> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["role"].as_str())
        .collect();
    assert_eq!(roles[roles.len() - 3..], ["tool", "user", "user"]);
}

#[tokio::test]
async fn a_steer_waiting_for_a_tool_can_be_pulled_back_before_it_is_sent() {
    let server = FakeServer::start([outside_read(), Reply::sse(&chat_text_events(&["Read."]))]);
    let mut harness = Harness::start(&server).await;
    waiting_for_approval(&mut harness).await;
    harness.submit("first");
    harness.submit("second");
    settle_commands(&mut harness).await;
    let popped = harness.worker.pop_queued_steer_for_edit().unwrap();
    assert_eq!((popped.id, popped.text.as_str()), (2, "second"));
    approve(&harness);
    harness.until(finished(TurnOutcome::Completed)).await;
    let body = server.requests()[1].json();
    assert_eq!(user_texts(&body)[1..], [steered("first")]);
}

#[tokio::test]
async fn a_reset_drops_the_steering_still_waiting_with_the_turn() {
    let server = FakeServer::start([outside_read(), Reply::sse(&chat_text_events(&["after"]))]);
    let mut harness = Harness::start(&server).await;
    waiting_for_approval(&mut harness).await;
    harness.submit("dropped");
    harness.command("/reset");
    harness.submit("kept");
    harness.until(finished(TurnOutcome::Interrupted)).await;
    let cleared = harness
        .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
        .await;
    assert_eq!(
        cleared.last(),
        Some(&UiEvent::ConversationCleared {
            first_kept_prompt: 2
        })
    );
    timeout(
        Duration::from_secs(10),
        harness.until(finished(TurnOutcome::Completed)),
    )
    .await
    .expect("the prompt sent after reset runs");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(user_texts(&requests[1].json()), ["kept"]);
}

#[tokio::test]
async fn a_prompt_submitted_after_a_cancel_runs_next_as_its_own_turn() {
    let server = FakeServer::start([held_reply(), Reply::sse(&chat_text_events(&["next"]))]);
    let mut harness = Harness::start(&server).await;
    harness.submit("slow");
    harness
        .until(|event| matches!(event, UiEvent::AssistantText { .. }))
        .await;
    let turn_id = harness.running_turn();
    harness.send(UiCommand::Cancel { turn_id });
    harness.submit("next");
    harness.until(finished(TurnOutcome::Interrupted)).await;
    let events = harness
        .until(finished(TurnOutcome::Completed))
        .await
        .to_vec();
    assert_eq!(started(&events), 1);
    let body = server.requests()[1].json();
    assert_eq!(user_texts(&body), ["slow", "next"]);
}

#[tokio::test]
async fn steering_left_by_a_cancelled_turn_runs_next_as_a_continuation() {
    let server = FakeServer::start([
        outside_read(),
        Reply::sse(&chat_text_events(&["Using the backup."])),
    ]);
    let mut harness = Harness::start(&server).await;
    waiting_for_approval(&mut harness).await;
    harness.submit("use the backup instead");
    settle_commands(&mut harness).await;
    let turn_id = harness.running_turn();
    harness.send(UiCommand::Cancel { turn_id });
    harness.until(finished(TurnOutcome::Interrupted)).await;
    let events = harness
        .until(finished(TurnOutcome::Completed))
        .await
        .to_vec();
    assert_eq!(started(&events), 1);
    let body = server.requests()[1].json();
    assert_eq!(
        user_texts(&body),
        ["read it".to_owned(), steered("use the backup instead")]
    );
}

#[tokio::test]
async fn a_prompt_released_by_an_install_steers_the_turn_as_plain_text() {
    let server = FakeServer::start([outside_read(), Reply::sse(&chat_text_events(&["Done."]))]);
    let mut harness = Harness::start(&server).await;
    write_skill(&harness.home, "install-pack", "new-skill");
    let source = fs::canonicalize(harness.home.path().join("workspace/install-pack")).unwrap();
    let (release, installer) = held_install_lock(&harness.home);
    waiting_for_approval(&mut harness).await;
    harness.command(&format!("/skills install {}", source.display()));
    harness.submit("$new-skill check it");
    settle_commands(&mut harness).await;
    release.send(()).unwrap();
    installer.join().unwrap();
    harness
        .until(|event| matches!(event, UiEvent::Notice { notice } if notice.body == "Installed: new-skill"))
        .await;
    approve(&harness);
    let events = harness
        .until(finished(TurnOutcome::Completed))
        .await
        .to_vec();
    assert_eq!(started(&events), 0);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = requests[1].json();
    assert_eq!(user_texts(&body)[1..], [steered("$new-skill check it")]);
    assert!(!system_text(&body).contains("<skill_content name=\"new-skill\""));
}
