use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use ofx_config::{PrivateDir, ProviderId};
use ofx_contract::{ConversationLog, ProviderBilling, ReasoningEffort};

use super::*;

fn private_dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn codex_route(profile: &Path) -> SessionRoute {
    SessionRoute {
        provider: SavedProvider::new(ProviderId::Codex, None).unwrap(),
        credential: RouteCredential::configured(),
        profile: ProfilePublisher::open(profile),
    }
}

#[test]
fn exit_abandons_the_ledger_the_drain_uses_after_a_provider_switch() {
    let home = tempfile::tempdir().unwrap();
    let profile = home.path().join("profile");
    let workspace = home.path().join("workspace");
    private_dir(&profile);
    fs::create_dir_all(&workspace).unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    let store = SessionStore::open(&home.path().join("data"), workspace.to_str().unwrap()).unwrap();
    let route = codex_route(&profile);
    let preferences = SessionPreferences {
        provider: route.provider.clone(),
        model: "gpt-test".to_owned(),
        effort: ReasoningEffort::Auto,
        fast_mode: false,
        ultrafast_mode: false,
    };
    let mut live = LiveSession::start(&store, preferences, route).unwrap();
    live.route = codex_route(&profile);

    let held = PrivateDir::open_existing(&profile)
        .unwrap()
        .unwrap()
        .try_lock("usage.lock")
        .unwrap()
        .unwrap();
    let log = SessionLog::new(
        Arc::clone(&live.session),
        live.route.provider.clone(),
        RouteCredential::configured(),
    )
    .publishing_with(live.publisher.as_ref().map(UsagePublisher::scheduler));
    let ticket = log.begin_request().unwrap();
    let billing = ProviderBilling {
        generation_id: "resp_held".to_owned(),
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
    log.finish_exact_request(ticket, &billing).unwrap();

    let exiting = Instant::now();
    live.abandon_profile_ledger();
    live.finish_publications();
    assert!(
        exiting.elapsed() < Duration::from_secs(1),
        "{:?}",
        exiting.elapsed()
    );
    drop(held);
}
