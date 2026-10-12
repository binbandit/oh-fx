use ofx_gateway::CreditsSnapshot;

use super::*;

fn reported(snapshot: &CreditsSnapshot, format: OutputFormat) -> (ExitCode, String, String) {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = report(snapshot, format, &mut stdout, &mut stderr);
    (
        code,
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    )
}

fn snapshot(balance: Option<&str>, used: Option<&str>, plan: Option<&str>) -> CreditsSnapshot {
    CreditsSnapshot {
        balance: balance.map(str::to_owned),
        used: used.map(str::to_owned),
        plan: plan.map(str::to_owned),
        error: None,
    }
}

#[test]
fn credits_print_upstream_lines_and_json() {
    let full = snapshot(Some("12.50"), Some("7.50"), Some("pro"));
    assert_eq!(
        reported(&full, OutputFormat::Text),
        (
            ExitCode::SUCCESS,
            "[credits] balance=12.50\n[credits] used=7.50\n[credits] plan=pro\n".to_owned(),
            String::new()
        )
    );
    let partial = snapshot(None, Some("1"), None);
    assert_eq!(
        reported(&partial, OutputFormat::Json).1,
        "{\"kind\":\"credits\",\"balance\":null,\"used\":\"1\",\"plan\":null}\n"
    );
    let empty = snapshot(None, None, None);
    assert_eq!(
        reported(&empty, OutputFormat::Text).1,
        "[credits] no data returned by gateway\n"
    );
    let hostile = snapshot(Some("1\u{1b}[2J"), None, None);
    assert!(!reported(&hostile, OutputFormat::Text).1.contains('\u{1b}'));
}

#[test]
fn a_credits_error_goes_to_stderr_in_text_and_stdout_in_json() {
    let failed = CreditsSnapshot::failed("Credits are unavailable for the selected provider.");
    assert_eq!(
        reported(&failed, OutputFormat::Text),
        (
            ExitCode::FAILURE,
            String::new(),
            "[credits] error: Credits are unavailable for the selected provider.\n".to_owned()
        )
    );
    assert_eq!(
        reported(&failed, OutputFormat::Json),
        (
            ExitCode::FAILURE,
            "{\"kind\":\"credits\",\"error\":\"Credits are unavailable for the selected provider.\"}\n"
                .to_owned(),
            String::new()
        )
    );
}

#[tokio::test]
async fn only_the_gateway_has_credits() {
    let fixture = crate::provider_activation::tests::Fixture::new();
    let auth = ofx_testkit::FakeServer::start([]);
    let server = ofx_testkit::FakeServer::start([]);
    let profile = fixture.profile(&auth, &server);
    let snapshot = credits(&profile, Credits::Unavailable, false).await;
    assert_eq!(
        snapshot.error.as_deref(),
        Some("Credits are unavailable for the selected provider.")
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn host_managed_and_missing_credentials_send_no_authorization() {
    for host_managed in [true, false] {
        let fixture = crate::provider_activation::tests::Fixture::new();
        let auth = ofx_testkit::FakeServer::start([]);
        let server =
            ofx_testkit::FakeServer::start([ofx_testkit::Reply::status(200, r#"{"balance":"3"}"#)]);
        let profile = fixture.profile(&auth, &server);
        let snapshot = credits(&profile, Credits::Gateway(None), host_managed).await;
        assert_eq!(snapshot.balance.as_deref(), Some("3"));
        assert_eq!(server.requests()[0].header("authorization"), None);
        assert_eq!(server.requests()[0].path, "/v1/coding-agent/v1/credits");
    }
}
