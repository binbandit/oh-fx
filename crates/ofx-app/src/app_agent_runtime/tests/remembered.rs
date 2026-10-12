use super::*;

type Remembered = Arc<Mutex<Vec<Option<String>>>>;

fn remembered_id(store: &SessionStore) -> Option<String> {
    store
        .remembered_session()
        .ok()
        .flatten()
        .map(|session| session.id)
}

async fn remembering(server: &FakeServer) -> (Harness, SessionStore, Remembered) {
    let home = tempfile::tempdir().unwrap();
    let setup = agent_setup(&home, server).await;
    let persistence = saved_persistence(&home, &setup, None);
    let workspace = fs::canonicalize(home.path().join("workspace")).unwrap();
    let open =
        || SessionStore::open(&home.path().join("data"), workspace.to_str().unwrap()).unwrap();
    let observed = open();
    let at_turn_end = Remembered::default();
    let seen = Arc::clone(&at_turn_end);
    let observe = move |event: &UiEvent| {
        if matches!(event, UiEvent::TurnFinished { .. }) {
            seen.lock().unwrap().push(remembered_id(&observed));
        }
    };
    let store = open();
    let harness = Harness::spawn_observer(
        home,
        setup,
        Some(persistence),
        false,
        UpgradeShortcut::default(),
        observe,
    );
    (harness, store, at_turn_end)
}

fn assert_remembered_before_turn_end(store: &SessionStore, at_turn_end: &Remembered) {
    let remembered = remembered_id(store);
    assert!(remembered.is_some());
    assert_eq!(*at_turn_end.lock().unwrap(), [remembered]);
}

#[tokio::test]
async fn a_new_session_is_remembered_for_continue_before_its_first_turn_ends() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["ok"]))]);
    let (mut harness, store, at_turn_end) = remembering(&server).await;
    assert_eq!(remembered_id(&store), None);
    harness.submit("go");
    harness.until(finished(TurnOutcome::Completed)).await;
    assert_remembered_before_turn_end(&store, &at_turn_end);
}

#[tokio::test]
async fn a_cancelled_first_turn_is_remembered_for_continue_before_it_ends() {
    let held = Reply::held_sse(&chat_text_events(&["partial\n"])[..2]);
    let server = FakeServer::start([held]);
    let (mut harness, store, at_turn_end) = remembering(&server).await;
    harness.submit("slow");
    harness
        .until(|event| matches!(event, UiEvent::AssistantText { .. }))
        .await;
    let turn_id = harness.running_turn();
    harness.send(UiCommand::Cancel { turn_id });
    harness.until(finished(TurnOutcome::Interrupted)).await;
    assert_remembered_before_turn_end(&store, &at_turn_end);
}

#[tokio::test]
async fn a_session_that_cannot_be_remembered_warns_once_after_its_first_turn() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["one"])),
        Reply::sse(&chat_text_events(&["two"])),
    ]);
    let (mut harness, store, at_turn_end) = remembering(&server).await;
    fs::write(harness.home.path().join("data").join("continue"), "").unwrap();
    harness.submit("first");
    harness.until(finished(TurnOutcome::Completed)).await;
    let warned = harness
        .until(|event| matches!(event, UiEvent::Notice { .. }))
        .await;
    let catalog = store.catalog().unwrap();
    let [saved] = catalog.summaries() else {
        panic!("the first turn saves one session");
    };
    assert_eq!(
        notice_body(warned),
        [format!(
            "session|Session saved, but could not remember it for -c (SessionPathUnsafe). Resume with oh-fx --resume {}.",
            saved.id
        )]
    );
    harness.submit("second");
    let second = harness.until(finished(TurnOutcome::Completed)).await;
    assert!(notice_body(second).is_empty());
    assert_eq!(*at_turn_end.lock().unwrap(), [None, None]);
    assert_eq!(remembered_id(&store), None);
}
