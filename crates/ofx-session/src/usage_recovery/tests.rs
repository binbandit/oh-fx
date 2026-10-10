use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use ofx_config::ProviderId;
use ofx_contract::{DeliveryOutcome, ProviderBilling, ReasoningEffort};

use super::*;
use crate::session_codec::{SavedProvider, SessionPreferences};
use crate::session_log::WritableSession;
use crate::session_store::SessionStore;
use crate::session_usage::PendingGeneration;
use crate::session_usage_sidecar;
use crate::usage_recovery_registry::{RecoveryRegistry, USAGE_RECOVERY_DIR};

const WORKSPACE: &str = "/workspace";

struct Profile {
    root: tempfile::TempDir,
}

impl Profile {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn data(&self) -> PathBuf {
        self.root.path().join("oh-fx")
    }

    fn start(&self) -> WritableSession {
        SessionStore::open(&self.data(), WORKSPACE)
            .unwrap()
            .start(SessionPreferences {
                provider: SavedProvider::new(ProviderId::Codex, None).unwrap(),
                model: "gpt-test".to_owned(),
                effort: ReasoningEffort::Auto,
                fast_mode: false,
                ultrafast_mode: false,
            })
            .unwrap()
    }

    fn marker(&self, id: &str) -> PathBuf {
        self.data().join(USAGE_RECOVERY_DIR).join(id)
    }

    fn collect(&self) -> UsageRecovery {
        UsageRecovery::collect(&self.data())
    }
}

fn exact(session: &mut WritableSession, generation_id: &str) {
    let ticket = session.begin_request().unwrap();
    let billing = ProviderBilling {
        generation_id: generation_id.to_owned(),
        created_at_ms: 1_000,
        model: "codex/gpt-test".to_owned(),
        total_cost: 0.0,
        input_tokens: 17,
        output_tokens: 7,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
        billable_web_search_calls: 0,
    };
    session
        .finish_exact_request(
            ticket,
            &billing,
            &SavedProvider::new(ProviderId::Codex, None).unwrap(),
        )
        .unwrap();
}

#[test]
fn a_profile_without_markers_has_nothing_to_recover() {
    let profile = Profile::new();
    assert_eq!(profile.collect(), UsageRecovery::default());
    drop(profile.start());
    assert_eq!(profile.collect(), UsageRecovery::default());
}

#[test]
fn only_marked_sessions_are_read_and_their_unpublished_usage_is_folded() {
    let profile = Profile::new();
    let mut owed = profile.start();
    exact(&mut owed, "resp_owed");
    let mut settled = profile.start();
    let ticket = settled.begin_request().unwrap();
    assert!(profile.marker(settled.id()).exists());
    settled
        .finish_request(ticket, DeliveryOutcome::Unbilled)
        .unwrap();
    assert!(!profile.marker(settled.id()).exists());
    let marker = fs::read_to_string(profile.marker(owed.id())).unwrap();

    let recovery = profile.collect();
    assert!(!recovery.unknown_pending);
    assert_eq!(recovery.facts.len(), 1);
    assert_eq!(recovery.facts[0].model, "codex/gpt-test");
    assert_eq!(recovery.pending.len(), 1);
    assert_eq!(recovery.pending[0].id, recovery.facts[0].id);
    assert!(recovery.incidents.is_empty());
    assert_eq!(
        fs::read_to_string(profile.marker(owed.id())).unwrap(),
        marker
    );
}

#[test]
fn a_marker_newer_than_its_checkpoint_leaves_the_usage_unknown() {
    let profile = Profile::new();
    let session = profile.start();
    let id = session.id().to_owned();
    drop(session);
    thread::sleep(Duration::from_millis(20));
    let data = PrivateDir::open_existing(&profile.data()).unwrap().unwrap();
    RecoveryRegistry::new(data).mark(&id, 1, true).unwrap();
    assert!(profile.collect().unknown_pending);

    thread::sleep(Duration::from_millis(20));
    let sessions = PrivateDir::open_existing(&profile.data().join(SESSIONS_DIR))
        .unwrap()
        .unwrap();
    let dir = sessions.open_child(&id).unwrap().unwrap();
    session_usage_sidecar::write(&dir, &id, &UsageSnapshot::fresh()).unwrap();
    assert_eq!(profile.collect(), UsageRecovery::default());
    assert!(profile.marker(&id).exists());
}

#[test]
fn a_marked_session_that_cannot_be_read_leaves_the_usage_unknown() {
    let profile = Profile::new();
    drop(profile.start());
    let data = PrivateDir::open_existing(&profile.data()).unwrap().unwrap();
    RecoveryRegistry::new(data)
        .mark("vanished", 1, true)
        .unwrap();
    let recovery = profile.collect();
    assert!(recovery.unknown_pending);
    assert!(recovery.facts.is_empty());

    fs::write(profile.marker("vanished"), "v9 1\n").unwrap();
    assert_eq!(
        profile.collect(),
        UsageRecovery {
            unknown_pending: true,
            ..UsageRecovery::default()
        }
    );
}

#[test]
fn unexplained_incomplete_billing_is_recovered_as_a_gap_at_the_last_update() {
    let mut usage = UsageSnapshot::fresh();
    usage.billing = Availability::Incomplete;
    usage.next_sequence = 2;
    usage.settled_through_sequence = 1;
    usage.pending.push(PendingGeneration {
        id: "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
        sequence: 1,
        provider: ProviderId::Codex,
        origin: "exact/codex".to_owned(),
        team: None,
        credential_source: None,
        credential_identity: None,
        account_id: None,
        observed_at_ms: None,
    });
    let mut recovery = UsageRecovery::default();
    recovery.add(&usage, 42, true);
    assert!(!recovery.unknown_pending);
    assert_eq!(
        recovery.pending,
        [PendingMarker {
            id: "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
            observed_at_ms: 42,
        }]
    );
    assert_eq!(
        recovery.incidents,
        [UsageIncident {
            occurred_at_ms: 42,
            completeness: UsageCompleteness::Incomplete,
        }]
    );

    usage.settled_through_sequence = 0;
    let mut unsettled = UsageRecovery::default();
    unsettled.add(&usage, 42, true);
    assert!(unsettled.unknown_pending);
    assert!(unsettled.incidents.is_empty());

    let mut stale = UsageRecovery::default();
    stale.add(&UsageSnapshot::fresh(), 42, false);
    assert!(stale.unknown_pending);
    assert!(stale.pending.is_empty());
}
