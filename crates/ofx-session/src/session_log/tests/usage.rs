use std::sync::{Arc, Mutex};

use ofx_contract::{
    ConversationLog, DeliveryOutcome, FileChangeStats, ProviderBilling, UsageCompleteness,
    UsageIncident,
};

use super::*;
use crate::profile_usage_runtime::ProfilePublisher;
use crate::profile_usage_store::ProfileUsageStore;
use crate::session_codec::recovery_checkpoint::RouteCredential;
use crate::session_conversation_log::SessionLog;
use crate::session_usage::Availability;
use crate::session_usage_sidecar::{SIDECAR_FILE, load_conversation};
use crate::usage_publisher::UsagePublisher;

const FRESH_SIDECAR: &str = "{\"schema_version\":1,\"session_id\":\"fresh\",\"snapshot\":{\"schema_version\":3,\"billing\":\"complete\",\"api_duration_complete\":true,\"wall_duration_complete\":true,\"code_complete\":true,\"next_sequence\":1,\"settled_through_sequence\":0,\"api_duration_ms\":0,\"wall_duration_ms\":0,\"total_cost\":0,\"input_tokens\":0,\"output_tokens\":0,\"cache_read_tokens\":0,\"cache_write_tokens\":0,\"reasoning_tokens\":0,\"request_count\":0,\"billable_web_search_calls\":0,\"lines_added\":0,\"lines_removed\":0,\"models\":[],\"pending\":[],\"publication_backlog\":[],\"incidents\":[]}}";

fn saved(fixture: &Fixture, id: &str) -> UsageSnapshot {
    let dir = fixture.sessions.open_child(id).unwrap().unwrap();
    load_conversation(&dir, id, 0).unwrap()
}

fn current(session: &mut WritableSession) -> UsageSnapshot {
    session.usage.snapshot(now_ms())
}

fn codex() -> SavedProvider {
    SavedProvider::new(ProviderId::Codex, None).unwrap()
}

fn exact_billing(generation_id: &str) -> ProviderBilling {
    ProviderBilling {
        generation_id: generation_id.to_owned(),
        created_at_ms: 1_000,
        model: "codex/gpt-test".to_owned(),
        total_cost: 0.0,
        input_tokens: 17,
        output_tokens: 7,
        cache_read_tokens: 5,
        cache_write_tokens: 0,
        reasoning_tokens: Some(3),
        billable_web_search_calls: 0,
    }
}

fn publishing_log(session: &Arc<Mutex<WritableSession>>, publisher: &UsagePublisher) -> SessionLog {
    SessionLog::new(Arc::clone(session), codex(), RouteCredential::configured())
        .publishing_with(Some(publisher.scheduler()))
}

fn settle_exact(log: &SessionLog, generation_id: &str) {
    let ticket = log.begin_request().unwrap();
    log.finish_exact_request(ticket, &exact_billing(generation_id))
        .unwrap();
}

#[test]
fn a_new_session_saves_and_resumes_fresh_usage() {
    let fixture = Fixture::new();
    let session = fixture.start("fresh");
    assert_eq!(session.usage, Usage::fresh());
    drop(session);
    assert_eq!(
        fs::read_to_string(fixture.dir("fresh").join(SIDECAR_FILE)).unwrap(),
        FRESH_SIDECAR
    );
    let mut resumed = fixture.resume("fresh").unwrap();
    let usage = current(&mut resumed);
    assert_eq!(usage.billing, Availability::Complete);
    assert_eq!(usage.next_sequence, 1);
    assert!(usage.incidents.is_empty());
    assert!(usage.wall_duration_complete);
}

#[test]
fn a_session_without_usage_resumes_with_one_gap_at_its_last_update() {
    let fixture = Fixture::new();
    drop(fixture.start("older"));
    fs::remove_file(fixture.dir("older").join(SIDECAR_FILE)).unwrap();
    let mut resumed = fixture.resume("older").unwrap();
    let usage = current(&mut resumed);
    assert_eq!(usage.billing, Availability::Incomplete);
    assert_eq!(
        usage.incidents,
        [UsageIncident {
            occurred_at_ms: 1,
            completeness: UsageCompleteness::Incomplete,
        }]
    );
    drop(resumed);
    assert!(!fixture.dir("older").join(SIDECAR_FILE).exists());
}

#[test]
fn an_unsafe_usage_sidecar_refuses_the_resume() {
    let fixture = Fixture::new();
    drop(fixture.start("loose"));
    let sidecar = fixture.dir("loose").join(SIDECAR_FILE);
    fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        fixture.resume("loose").err(),
        Some(SessionError::InvalidUsageSidecar)
    );
    assert_eq!(mode(&sidecar), 0o644);
}

#[test]
fn each_request_is_saved_when_it_starts_and_when_it_settles() {
    let fixture = Fixture::new();
    let mut session = fixture.start("counted");
    let first = session.begin_request().unwrap();
    assert_eq!(first.sequence, 1);
    let started = saved(&fixture, "counted");
    assert_eq!(started.next_sequence, 2);
    assert_eq!(started.settled_through_sequence, 0);
    assert_eq!(started.billing, Availability::Incomplete);
    assert!(!started.api_duration_complete);

    session
        .finish_request(first, DeliveryOutcome::Unbilled)
        .unwrap();
    let settled = saved(&fixture, "counted");
    assert_eq!(settled.settled_through_sequence, 1);
    assert_eq!(settled.billing, Availability::Complete);
    assert!(settled.api_duration_complete);
    assert!(settled.incidents.is_empty());

    let second = session.begin_request().unwrap();
    session
        .finish_request(second, DeliveryOutcome::PossiblyBilledWithoutIdentity)
        .unwrap();
    session.record_committed_lines(FileChangeStats {
        additions: 7,
        deletions: 2,
    });
    let billed = saved(&fixture, "counted");
    assert_eq!(billed.next_sequence, 3);
    assert_eq!(billed.settled_through_sequence, 2);
    assert_eq!(billed.billing, Availability::Incomplete);
    assert_eq!(billed.incidents.len(), 1);
    assert_eq!((billed.lines_added, billed.lines_removed), (7, 2));

    drop(session);
    let mut resumed = fixture.resume("counted").unwrap();
    let ticket = resumed.begin_request().unwrap();
    assert_eq!(ticket.sequence, 3);
    let restored = current(&mut resumed);
    assert_eq!((restored.lines_added, restored.incidents.len()), (7, 1));
}

#[test]
fn a_request_whose_start_cannot_be_saved_is_refused_and_settled() {
    let fixture = Fixture::new();
    let mut session = fixture.start("refused");
    let sidecar = fixture.dir("refused").join(SIDECAR_FILE);
    fs::remove_file(&sidecar).unwrap();
    fs::create_dir(&sidecar).unwrap();
    assert_eq!(
        session.begin_request().err(),
        Some(SessionError::SessionPathUnsafe)
    );
    let usage = current(&mut session);
    assert_eq!(usage.next_sequence, 2);
    assert_eq!(usage.settled_through_sequence, 1);
    assert_eq!(usage.billing, Availability::Complete);

    fs::remove_dir(&sidecar).unwrap();
    let ticket = session.begin_request().unwrap();
    assert_eq!(ticket.sequence, 2);
}

#[test]
fn a_settlement_that_cannot_be_saved_marks_billing_incomplete_and_continues() {
    let fixture = Fixture::new();
    let mut session = fixture.start("unsaved");
    let ticket = session.begin_request().unwrap();
    let sidecar = fixture.dir("unsaved").join(SIDECAR_FILE);
    fs::remove_file(&sidecar).unwrap();
    fs::create_dir(&sidecar).unwrap();
    session
        .finish_request(ticket, DeliveryOutcome::Unbilled)
        .unwrap();
    let usage = current(&mut session);
    assert_eq!(usage.billing, Availability::Incomplete);
    assert_eq!(usage.incidents.len(), 1);
    assert_eq!(usage.settled_through_sequence, 1);

    session.record_committed_lines(FileChangeStats {
        additions: 1,
        deletions: 0,
    });
    assert!(!current(&mut session).code_complete);
    fs::remove_dir(&sidecar).unwrap();
    session.record_committed_lines(FileChangeStats {
        additions: 1,
        deletions: 0,
    });
    let recovered = saved(&fixture, "unsaved");
    assert_eq!(recovered.lines_added, 2);
    assert!(!recovered.code_complete);
    assert_eq!(recovered.billing, Availability::Incomplete);
}

#[test]
fn a_subagent_log_accounts_its_requests_in_the_parent_session() {
    let fixture = Fixture::new();
    let parent = Arc::new(Mutex::new(fixture.start("parent")));
    let child = Arc::new(Mutex::new(fixture.start("child")));
    let log = SessionLog::new(
        Arc::clone(&child),
        SavedProvider::new(ProviderId::Codex, None).unwrap(),
        RouteCredential::configured(),
    )
    .accounting_in(Arc::downgrade(&parent));
    let ticket = log.begin_request().unwrap();
    log.finish_request(ticket, DeliveryOutcome::Unbilled)
        .unwrap();
    log.record_committed_lines(FileChangeStats {
        additions: 2,
        deletions: 1,
    });
    let accounted = saved(&fixture, "parent");
    assert_eq!(accounted.next_sequence, 2);
    assert_eq!(accounted.settled_through_sequence, 1);
    assert_eq!((accounted.lines_added, accounted.lines_removed), (2, 1));
    assert_eq!(
        fs::read_to_string(fixture.dir("child").join(SIDECAR_FILE)).unwrap(),
        FRESH_SIDECAR.replace("fresh", "child")
    );

    drop(parent);
    assert_eq!(log.begin_request().unwrap().sequence, 1);
    assert_eq!(saved(&fixture, "child").next_sequence, 2);
}

#[test]
fn an_exact_request_is_saved_with_its_generation_waiting_for_publication() {
    let fixture = Fixture::new();
    let mut session = fixture.start("exact");
    let ticket = session.begin_request().unwrap();
    session
        .finish_exact_request(ticket, &exact_billing("resp_saved"), &codex())
        .unwrap();
    let saved = saved(&fixture, "exact");
    assert_eq!(saved.billing, Availability::Pending);
    assert_eq!(saved.settled_through_sequence, 1);
    assert_eq!(saved.pending.len(), 1);
    assert_eq!(saved.pending[0].origin, "exact/codex");
    assert_eq!(saved.publication_backlog.len(), 1);
    assert_eq!(saved.publication_backlog[0].id, saved.pending[0].id);
    assert_eq!(
        (
            saved.publication_backlog[0].model.as_str(),
            saved.publication_backlog[0].cache_read_tokens,
        ),
        ("codex/gpt-test", 5)
    );
    assert_eq!(saved.input_tokens, 0);

    drop(session);
    let mut resumed = fixture.resume("exact").unwrap();
    let usage = current(&mut resumed);
    assert_eq!(usage.billing, Availability::Pending);
    assert_eq!(usage.pending, saved.pending);
    assert_eq!(usage.publication_backlog, saved.publication_backlog);
}

#[test]
fn a_session_publishes_its_exact_usage_to_the_profile_ledger() {
    let fixture = Fixture::new();
    let data = fixture.root.path().join("data/oh-fx");
    let session = Arc::new(Mutex::new(fixture.start("published")));
    let publisher = UsagePublisher::new(&session, ProfilePublisher::open(&data).unwrap());
    let log = publishing_log(&session, &publisher);
    settle_exact(&log, "resp_one");
    settle_exact(&log, "resp_two");
    publisher.finish_before_shutdown();

    let usage = saved(&fixture, "published");
    assert_eq!(usage.billing, Availability::Complete);
    assert!(usage.pending.is_empty());
    assert!(usage.publication_backlog.is_empty());
    assert_eq!((usage.input_tokens, usage.request_count), (34, Some(2)));
    assert_eq!(usage.models.len(), 1);
    let ledger = ProfileUsageStore::open(&data).unwrap().load().unwrap();
    assert!(ledger.coverage_started_at_ms.is_some());
    assert_eq!(ledger.facts.len(), 2);
    assert_eq!(ledger.pending.len(), 2);
    assert!(ledger.incidents.is_empty());
    let lines = fs::read_to_string(data.join("usage.jsonl")).unwrap();
    assert!(lines.starts_with("{\"schema_version\":1,\"kind\":\"coverage\","));
    assert_eq!(lines.lines().count(), 5);
}

#[test]
fn an_abandoned_ledger_leaves_the_backlog_for_the_next_resume() {
    let fixture = Fixture::new();
    let data = fixture.root.path().join("data/oh-fx");
    let session = Arc::new(Mutex::new(fixture.start("held")));
    let profile = ProfilePublisher::open(&data).unwrap();
    profile.abandon_for_process_exit();
    let publisher = UsagePublisher::new(&session, profile);
    let log = publishing_log(&session, &publisher);
    settle_exact(&log, "resp_held");
    publisher.finish_before_shutdown();
    let held = saved(&fixture, "held");
    assert_eq!(held.billing, Availability::Pending);
    assert_eq!(held.publication_backlog.len(), 1);
    assert!(!data.join("usage.jsonl").exists());
    drop((log, publisher, session));

    let resumed = Arc::new(Mutex::new(fixture.resume("held").unwrap()));
    let publisher = UsagePublisher::new(&resumed, ProfilePublisher::open(&data).unwrap());
    publisher.schedule();
    drop(publisher);
    let settled = saved(&fixture, "held");
    assert_eq!(settled.billing, Availability::Complete);
    assert!(settled.publication_backlog.is_empty());
    assert_eq!(settled.input_tokens, 17);
    let ledger = ProfileUsageStore::open(&data).unwrap().load().unwrap();
    assert_eq!(ledger.facts, held.publication_backlog);
}
