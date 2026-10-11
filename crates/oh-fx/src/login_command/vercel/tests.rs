use std::io;

use ofx_app::SubscriptionEndpoints;

use super::*;
use crate::provider_activation::tests::Fixture;

const CHOSEN: &str = r#"{"provider":"gateway","credential_source":"fx_login"}"#;

struct Broken;

impl Write for Broken {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("closed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn profile(fixture: &Fixture) -> Profile {
    Profile {
        paths: Some(fixture.paths.clone()),
        workspace: Ok(fixture.workspace.clone()),
        endpoints: SubscriptionEndpoints::default(),
        lookup: |_| None,
    }
}

fn reported(fixture: &Fixture, result: LogoutResult) -> (ExitCode, String, String) {
    let (mut output, mut errors) = (Vec::new(), Vec::new());
    let code = report(&profile(fixture), result, &mut output, &mut errors);
    (
        code,
        String::from_utf8(output).unwrap(),
        String::from_utf8(errors).unwrap(),
    )
}

#[test]
fn a_finished_logout_forgets_the_chosen_source_and_reports_the_session() {
    let fixture = Fixture::new();
    fixture.write_settings(CHOSEN);
    let (code, output, errors) = reported(&fixture, LogoutResult::default());
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(output, "No oh-fx login session found.\n");
    assert_eq!(errors, "");
    assert_eq!(
        fixture.settings().as_deref(),
        Some("{\"provider\":\"gateway\"}\n")
    );
}

#[test]
fn a_removal_that_was_not_durable_keeps_the_chosen_source_and_still_warns() {
    let fixture = Fixture::new();
    fixture.write_settings(CHOSEN);
    let (code, output, errors) = reported(
        &fixture,
        LogoutResult {
            session_deleted: true,
            local_durability_failed: true,
            remote_revocation_failed: true,
        },
    );
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(output, "");
    assert_eq!(errors, format!("{DELETE_FAILED}{REVOCATION_WARNING}"));
    assert_eq!(fixture.settings().as_deref(), Some(CHOSEN));
}

#[test]
fn a_selection_that_cannot_be_cleared_fails_before_any_report() {
    let fixture = Fixture::new();
    fixture.write_settings("{");
    let (code, output, errors) = reported(
        &fixture,
        LogoutResult {
            session_deleted: true,
            local_durability_failed: false,
            remote_revocation_failed: true,
        },
    );
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(output, "");
    assert_eq!(errors, SELECTION_NOT_CLEARED);
}

#[test]
fn a_report_that_cannot_be_written_fails_before_the_warning() {
    let fixture = Fixture::new();
    let mut errors = Vec::new();
    let code = report(
        &profile(&fixture),
        LogoutResult {
            session_deleted: true,
            local_durability_failed: false,
            remote_revocation_failed: true,
        },
        &mut Broken,
        &mut errors,
    );
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(String::from_utf8(errors).unwrap(), "oh-fx: WriteFailed\n");
}
