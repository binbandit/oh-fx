use std::os::unix::ffi::OsStringExt;

use super::*;

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn usage(kind: TopLevelKind) -> impl Fn(&CliError) -> bool {
    move |error| matches!(error, CliError::Usage(command) if *command == kind)
}

fn invalid(kind: TopLevelKind, expected: ArgumentErrorCode) -> impl Fn(&CliError) -> bool {
    move |error| {
        matches!(
            error,
            CliError::InvalidArguments { command, code } if *command == kind && *code == expected
        )
    }
}

fn unhandled(expected: ArgumentErrorCode) -> impl Fn(&CliError) -> bool {
    move |error| matches!(error, CliError::Unhandled(code) if *code == expected)
}

fn fails<T: std::fmt::Debug>(result: Result<T, CliError>, check: impl Fn(&CliError) -> bool) {
    match result {
        Err(error) => assert!(check(&error), "unexpected error {error:?}"),
        Ok(value) => panic!("expected an error, got {value:?}"),
    }
}

#[test]
fn usage_arguments_accept_only_rolling_periods_and_one_json_flag() {
    assert_eq!(parse_usage(os(&[])).unwrap(), OutputFormat::Text);
    assert_eq!(
        parse_usage(os(&["--json", "--period", "7d"])).unwrap(),
        OutputFormat::Json
    );
    for period in ["24h", "7d", "30d"] {
        assert!(parse_usage(os(&["--period", period])).is_ok(), "{period}");
    }
    for args in [
        &["--period"][..],
        &["--period", "session"],
        &["--period", "24h", "--period", "7d"],
        &["30d"],
        &["--period=24h"],
    ] {
        fails(parse_usage(os(args)), usage(TopLevelKind::Usage));
    }
    fails(
        parse_usage(os(&["--json", "--json"])),
        invalid(TopLevelKind::Usage, ArgumentErrorCode::Usage),
    );
}

#[test]
fn parse_acp_args_extracts_known_flags_and_rejects_invalid_arguments() {
    assert!(
        validate_acp(os(&[
            "--model",
            "openai/gpt-4o",
            "--log-file",
            "/tmp/fx.log"
        ]))
        .is_ok()
    );
    assert!(validate_acp(os(&[])).is_ok());
    for args in [
        &["--unknown"][..],
        &["--model"],
        &["--log-file"],
        &["--model", "first", "--model", "second"],
        &["--log-file", "a", "--log-file", "b"],
        &["--model=inline"],
    ] {
        fails(validate_acp(os(args)), usage(TopLevelKind::Acp));
    }
}

#[test]
fn parse_local_surface_args_accepts_only_json() {
    assert_eq!(
        parse_output_format(TopLevelKind::Status, &os(&[])).unwrap(),
        OutputFormat::Text
    );
    assert_eq!(
        parse_output_format(TopLevelKind::Status, &os(&["--json", "--json"])).unwrap(),
        OutputFormat::Json
    );
    fails(
        parse_output_format(TopLevelKind::Status, &os(&["--wat"])),
        usage(TopLevelKind::Status),
    );
    fails(
        parse_output_format(TopLevelKind::Credits, &os(&["--json", "--wat"])),
        invalid(TopLevelKind::Credits, ArgumentErrorCode::LocalSurface),
    );
}

#[test]
fn parse_upgrade_args_accept_only_one_json_flag() {
    assert_eq!(parse_upgrade(&os(&[])).unwrap(), OutputFormat::Text);
    assert_eq!(parse_upgrade(&os(&["--json"])).unwrap(), OutputFormat::Json);
    for args in [
        &["--channel", "dev"][..],
        &["--channel=stable"],
        &["--bogus"],
    ] {
        fails(parse_upgrade(&os(args)), usage(TopLevelKind::Upgrade));
    }
    for args in [&["--json", "--json"][..], &["--json", "--channel", "dev"]] {
        fails(
            parse_upgrade(&os(args)),
            unhandled(ArgumentErrorCode::Upgrade),
        );
    }
}

#[test]
fn parse_session_list_args_supports_bounded_canonical_pagination() {
    assert_eq!(parse_session_list(os(&[])).unwrap(), OutputFormat::Text);
    assert_eq!(
        parse_session_list(os(&[
            "--json",
            "--all",
            "--limit",
            "2",
            "--cursor",
            "v1:20:session-a"
        ]))
        .unwrap(),
        OutputFormat::Json
    );
    for args in [
        &["--limit", "1_0"][..],
        &["--limit", "100"],
        &["--cursor", "v1:-5:x"],
        &["--cursor", "v1:20:v2x"],
    ] {
        assert!(parse_session_list(os(args)).is_ok(), "{args:?}");
    }
    for args in [
        &["--all", "--all"][..],
        &["--limit", "0"],
        &["--limit", "101"],
        &["--limit", "2", "--limit", "3"],
        &["--cursor"],
        &["--cursor", "v1:020:session-a"],
        &["--cursor", "v2:20:session-a"],
        &["--cursor", "v1:20:../unsafe"],
        &["--cursor", "v1:20:v2"],
        &["--cursor", "v1:20:V2"],
        &["--cursor", "v1:+20:session-a"],
        &["--cursor", "v1:20:a:b"],
        &["--cursor", "v1:1:a", "--cursor", "v1:2:b"],
    ] {
        fails(parse_session_list(os(args)), usage(TopLevelKind::Sessions));
    }
    fails(
        parse_session_list(vec![
            OsString::from("--limit"),
            OsString::from_vec(b"1\xff".to_vec()),
        ]),
        usage(TopLevelKind::Sessions),
    );
    fails(
        parse_session_list(os(&["--json", "--json"])),
        invalid(TopLevelKind::Sessions, ArgumentErrorCode::LocalSurface),
    );
}

#[test]
fn parse_session_detail_args_owns_string_ids_and_frees_through_deinit() {
    assert_eq!(
        parse_session(os(&["last", "--json"])).unwrap(),
        OutputFormat::Json
    );
    assert_eq!(
        parse_session(os(&[" sess-1 "])).unwrap(),
        OutputFormat::Text
    );
    for args in [&["a", "b"][..], &[""], &[]] {
        fails(parse_session(os(args)), usage(TopLevelKind::Session));
    }
    fails(
        parse_session(os(&["--json"])),
        invalid(TopLevelKind::Session, ArgumentErrorCode::SessionDetail),
    );
}

#[test]
fn parse_session_detail_args_accepts_explicit_id_flag() {
    assert_eq!(
        parse_session(os(&["--id", "release.2026.06", "--json"])).unwrap(),
        OutputFormat::Json
    );
    assert!(parse_session(os(&["--id", "last"])).is_ok());
    assert!(parse_session(vec![OsString::from_vec(b"\xff".to_vec())]).is_ok());
}

#[test]
fn parse_session_migration_args_accepts_positional_and_exact_ids() {
    assert_eq!(
        parse_session(os(&["migrate", "session.v2", "--allow-large", "--json"])).unwrap(),
        OutputFormat::Json
    );
    assert_eq!(
        parse_session(os(&["migrate", "--id", "--allow-large", "--json"])).unwrap(),
        OutputFormat::Json
    );
}

#[test]
fn parse_session_migration_args_rejects_missing_repeated_and_mixed_targets() {
    for args in [
        &["migrate", "--id"][..],
        &["migrate", "session.v2", "--id", "session.v3"],
        &["migrate", "session.v2", "session.v3"],
        &["migrate"],
        &["last", "--allow-large"],
    ] {
        fails(parse_session(os(args)), usage(TopLevelKind::Session));
    }
    fails(
        parse_session(os(&["migrate", "--json"])),
        invalid(TopLevelKind::Session, ArgumentErrorCode::SessionMigration),
    );
}

#[test]
fn parse_session_recovery_args_accepts_exact_ids_and_rejects_ambiguity() {
    assert_eq!(
        parse_session(os(&["recover", "session.v3", "--json"])).unwrap(),
        OutputFormat::Json
    );
    assert!(parse_session(os(&["recover", "--id", "last"])).is_ok());
    for args in [
        &["recover", "--id"][..],
        &["recover", "first", "second"],
        &["recover", "a", "--allow-large"],
    ] {
        fails(parse_session(os(args)), usage(TopLevelKind::Session));
    }
    fails(
        parse_session(os(&["recover", "--json"])),
        invalid(TopLevelKind::Session, ArgumentErrorCode::SessionRecovery),
    );
}

#[test]
fn workspace_arguments_accept_one_action_and_one_json_flag() {
    assert_eq!(parse_workspace(os(&[])).unwrap(), OutputFormat::Text);
    assert_eq!(
        parse_workspace(os(&["list", "--json"])).unwrap(),
        OutputFormat::Json
    );
    for args in [
        &["add", "/tmp/shared"][..],
        &["remove", "--other"],
        &["clear"],
    ] {
        assert!(parse_workspace(os(args)).is_ok(), "{args:?}");
    }
    for args in [
        &["add"][..],
        &["add", ""],
        &["list", "extra"],
        &["clear", "x"],
        &["bogus"],
        &["add", "a", "b"],
    ] {
        fails(parse_workspace(os(args)), usage(TopLevelKind::Workspace));
    }
    fails(
        parse_workspace(os(&["add", "--json"])),
        invalid(TopLevelKind::Workspace, ArgumentErrorCode::Workspace),
    );
    assert!(parse_workspace(os(&["--json", "--json"])).is_err());
}

#[test]
fn login_providers_accept_one_known_slug() {
    assert_eq!(parse_login(TopLevelKind::Login, &os(&[])).ok(), Some(None));
    assert_eq!(
        parse_login(TopLevelKind::Login, &os(&["Vercel"])).ok(),
        Some(Some(ProviderId::Gateway))
    );
    assert_eq!(
        parse_login(TopLevelKind::Logout, &os(&["CODEX"])).ok(),
        Some(Some(ProviderId::Codex))
    );
    fails(
        parse_login(TopLevelKind::Logout, &os(&["foo"])),
        usage(TopLevelKind::Logout),
    );
    fails(
        parse_login(TopLevelKind::Login, &os(&["codex", "grok"])),
        usage(TopLevelKind::Login),
    );
}

#[test]
fn provider_command_requires_exactly_one_valid_name() {
    assert!(validate_provider(&os(&["codex"])).is_ok());
    assert!(validate_provider(&os(&["vercel"])).is_ok());
    fails(validate_provider(&os(&["9"])), |error| {
        matches!(error, CliError::UnknownProvider)
    });
    fails(
        validate_provider(&[OsString::from_vec(b"a\xff".to_vec())]),
        |error| matches!(error, CliError::UnknownProvider),
    );
    fails(validate_provider(&os(&[])), usage(TopLevelKind::Provider));
}

#[test]
fn mcp_subcommands_validate_their_operand_shapes() {
    for args in [
        &["add", "fixture", "node", "server.js"][..],
        &["add", "fixture", "node"],
        &["add", "--transport", "http", "remote", "https://x.test/mcp"],
        &["auth", "linear"],
        &["list"],
        &["list", "--connect"],
        &["path"],
        &["remove", "x"],
        &["logout", "x"],
        &["trust", "approve-all"],
        &["trust", "reset"],
        &["trust", "approve", "x"],
        &["trust", "reject", "x"],
    ] {
        assert!(validate_mcp(&os(args)).is_ok(), "{args:?}");
    }
    for args in [
        &["add"][..],
        &["add", "x"],
        &["add", "--transport", "sse", "a", "b"],
        &["add", "--transport", "http", "a"],
    ] {
        fails(validate_mcp(&os(args)), |error| {
            matches!(error, CliError::McpAddUsage)
        });
    }
    for args in [&["auth"][..], &["auth", ""], &["auth", "a", "b"]] {
        fails(validate_mcp(&os(args)), |error| {
            matches!(error, CliError::McpAuthUsage)
        });
    }
    for args in [
        &["bogus"][..],
        &["list", "--x"],
        &["trust"],
        &["trust", "approve"],
        &["trust", "approve", ""],
        &["path", "x"],
        &["remove"],
        &["remove", ""],
        &["logout", "a", "b"],
    ] {
        fails(validate_mcp(&os(args)), usage(TopLevelKind::Mcp));
    }
}
