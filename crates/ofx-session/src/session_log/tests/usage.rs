use ofx_contract::{UsageCompleteness, UsageIncident};

use super::*;
use crate::session_usage_sidecar::SIDECAR_FILE;

const FRESH_SIDECAR: &str = "{\"schema_version\":1,\"session_id\":\"fresh\",\"snapshot\":{\"schema_version\":3,\"billing\":\"complete\",\"api_duration_complete\":true,\"wall_duration_complete\":true,\"code_complete\":true,\"next_sequence\":1,\"settled_through_sequence\":0,\"api_duration_ms\":0,\"wall_duration_ms\":0,\"total_cost\":0,\"input_tokens\":0,\"output_tokens\":0,\"cache_read_tokens\":0,\"cache_write_tokens\":0,\"reasoning_tokens\":0,\"request_count\":0,\"billable_web_search_calls\":0,\"lines_added\":0,\"lines_removed\":0,\"models\":[],\"pending\":[],\"publication_backlog\":[],\"incidents\":[]}}";

#[test]
fn a_new_session_saves_and_resumes_fresh_usage() {
    let fixture = Fixture::new();
    let session = fixture.start("fresh");
    assert_eq!(session.usage(), &UsageSnapshot::fresh());
    drop(session);
    assert_eq!(
        fs::read_to_string(fixture.dir("fresh").join(SIDECAR_FILE)).unwrap(),
        FRESH_SIDECAR
    );
    let resumed = fixture.resume("fresh").unwrap();
    assert_eq!(resumed.usage(), &UsageSnapshot::fresh());
}

#[test]
fn a_session_without_usage_resumes_with_one_gap_at_its_last_update() {
    let fixture = Fixture::new();
    drop(fixture.start("older"));
    fs::remove_file(fixture.dir("older").join(SIDECAR_FILE)).unwrap();
    let resumed = fixture.resume("older").unwrap();
    let mut expected = UsageSnapshot::unavailable();
    expected
        .append_incident(UsageIncident {
            occurred_at_ms: 1,
            completeness: UsageCompleteness::Incomplete,
        })
        .unwrap();
    assert_eq!(resumed.usage(), &expected);
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
