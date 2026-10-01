use std::os::unix::ffi::OsStringExt;

use super::resume::UPGRADE_RELAUNCH_ARG;
use super::*;
use crate::command_specs::render_top_level_help;

const RESUME_USAGE: &str = "usage: oh-fx session resume [last|<id>] | session resume --id <id> | --resume [last|<id>] | resume [last|<id>] | resume --id <id> | --resume-last | --continue | -c | -r | --resume-<id>\n";
const WORKSPACE_MODIFIERS: &str = "oh-fx: --add-dir and --no-additional-dirs are only supported for interactive, resume, ask, ACP, PR, and issue launches\n";

const COMMAND_SHAPES: &[(&[&str], TopLevelKind, OutputFormat)] = &[
    (
        &["acp", "--model", "m"],
        TopLevelKind::Acp,
        OutputFormat::Text,
    ),
    (&["pr", "ready"], TopLevelKind::Pr, OutputFormat::Text),
    (&["issue", "flaky"], TopLevelKind::Issue, OutputFormat::Text),
    (&["setup"], TopLevelKind::Setup, OutputFormat::Text),
    (
        &["status", "--json"],
        TopLevelKind::Status,
        OutputFormat::Json,
    ),
    (
        &["permissions"],
        TopLevelKind::Permissions,
        OutputFormat::Text,
    ),
    (
        &["models", "--json"],
        TopLevelKind::Models,
        OutputFormat::Json,
    ),
    (
        &["mcp", "add", "fixture", "node"],
        TopLevelKind::Mcp,
        OutputFormat::Text,
    ),
    (&["doctor"], TopLevelKind::Doctor, OutputFormat::Text),
    (
        &["session", "last"],
        TopLevelKind::Session,
        OutputFormat::Text,
    ),
    (
        &["session", "last", "--json"],
        TopLevelKind::Session,
        OutputFormat::Json,
    ),
    (
        &["sessions", "--json"],
        TopLevelKind::Sessions,
        OutputFormat::Json,
    ),
    (
        &["credits", "--json"],
        TopLevelKind::Credits,
        OutputFormat::Json,
    ),
    (&["balance"], TopLevelKind::Credits, OutputFormat::Text),
    (
        &["usage", "--period", "24h"],
        TopLevelKind::Usage,
        OutputFormat::Text,
    ),
    (&["upgrade"], TopLevelKind::Upgrade, OutputFormat::Text),
    (
        &["upgrade", "--json"],
        TopLevelKind::Upgrade,
        OutputFormat::Json,
    ),
    (
        &["replay", "tape"],
        TopLevelKind::Replay,
        OutputFormat::Text,
    ),
    (
        &["replay", "tape", "--json"],
        TopLevelKind::Replay,
        OutputFormat::Json,
    ),
    (&["workspace"], TopLevelKind::Workspace, OutputFormat::Text),
    (
        &["workspace", "--json"],
        TopLevelKind::Workspace,
        OutputFormat::Json,
    ),
    (&["teams"], TopLevelKind::Teams, OutputFormat::Text),
    (&["login"], TopLevelKind::Login, OutputFormat::Text),
    (
        &["logout", "codex"],
        TopLevelKind::Logout,
        OutputFormat::Text,
    ),
    (
        &["provider", "grok"],
        TopLevelKind::Provider,
        OutputFormat::Text,
    ),
    (
        &["ask", "--json", "hi"],
        TopLevelKind::Ask,
        OutputFormat::Json,
    ),
    (&["ask", "hi"], TopLevelKind::Ask, OutputFormat::Text),
];

fn parse(args: &[&str]) -> Result<Invocation, CliError> {
    parse_args(args.iter().copied())
}

fn command(args: &[&str]) -> Command {
    match parse(args) {
        Ok(Invocation::Command(launch)) => launch.command,
        other => panic!("expected a command for {args:?}, got {other:?}"),
    }
}

fn launch(args: &[&str]) -> CommandLaunch {
    match parse(args) {
        Ok(Invocation::Command(launch)) => launch,
        other => panic!("expected a command for {args:?}, got {other:?}"),
    }
}

fn is_interactive(args: &[&str]) -> bool {
    matches!(parse(args), Ok(Invocation::Interactive))
}

fn resumes(args: &[&str]) -> bool {
    matches!(parse(args), Ok(Invocation::Resume))
}

fn help(args: &[&str]) -> Option<TopLevelKind> {
    match parse(args) {
        Ok(Invocation::CommandHelp(kind)) => Some(kind),
        _ => None,
    }
}

fn top_level_help(args: &[&str]) -> Option<HelpLayout> {
    match parse(args) {
        Ok(Invocation::TopLevelHelp(layout)) => Some(layout),
        _ => None,
    }
}

fn stderr(args: &[&str]) -> String {
    let report = parse(args).unwrap_err().report("0.0.0");
    assert_eq!(report.stdout, "", "{args:?}");
    report.stderr
}

fn stdout(args: &[&str]) -> String {
    let report = parse(args).unwrap_err().report("0.0.0");
    assert_eq!(report.stderr, "", "{args:?}");
    report.stdout
}

fn command_shape(args: &[&str]) -> (TopLevelKind, OutputFormat) {
    let command = command(args);
    (command.kind(), command.output_format())
}

#[test]
fn parse_recognizes_every_top_level_command_and_preserves_unknown_commands() {
    assert!(is_interactive(&[]));
    assert_eq!(top_level_help(&["help"]), Some(HelpLayout::Terminal));
    let Command::Ask(ask) = command(&["ask", "hello"]) else {
        panic!("expected ask");
    };
    assert_eq!(
        ask.resolve_prompt(|| unreachable!("stdin must stay unread"))
            .unwrap(),
        "hello"
    );
    for (args, kind, format) in COMMAND_SHAPES {
        assert_eq!(command_shape(args), (*kind, *format), "{args:?}");
    }
    assert!(resumes(&["session", "resume", "last"]));
    assert!(resumes(&["resume", "last"]));
    for unknown in ["background", "wat", "task", "tasks"] {
        match parse(&[unknown]) {
            Err(CliError::UnknownSubcommand(token)) => assert_eq!(token, unknown),
            other => panic!("expected an unknown subcommand, got {other:?}"),
        }
    }
}

#[test]
fn help_aliases_route_to_help() {
    for alias in ["--help", "-h", "help \t"] {
        assert_eq!(
            top_level_help(&[alias, "ignored"]),
            Some(HelpLayout::Terminal)
        );
    }
    assert_eq!(
        top_level_help(&["--context-limit", "skill_chunk_bytes=1", "help"]),
        Some(HelpLayout::Plain)
    );
}

#[test]
fn per_command_help_wins_over_command_arguments() {
    for (args, kind) in [
        (&["ask", "--help"][..], TopLevelKind::Ask),
        (&["ask", "--", "-h"], TopLevelKind::Ask),
        (&["balance", "--help"], TopLevelKind::Credits),
        (&["credits", "-h", "extra"], TopLevelKind::Credits),
        (&["session", "resume", "--help"], TopLevelKind::Session),
        (&["-c", "--help"], TopLevelKind::Resume),
    ] {
        assert_eq!(help(args), Some(kind), "{args:?}");
    }
    assert_eq!(stderr(&["--resume-abc", "--help"]), RESUME_USAGE);
}

#[test]
fn mcp_without_arguments_prints_its_help() {
    assert_eq!(help(&["mcp"]), Some(TopLevelKind::Mcp));
}

#[test]
fn slack_is_an_unknown_command_in_every_form() {
    for args in [
        &["slack"][..],
        &["slack", "install"],
        &["slack", "status", "--json"],
        &["slack", "--help"],
        &["--sessions-v2", "slack", "refresh"],
    ] {
        match parse(args) {
            Err(CliError::UnknownSubcommand(token)) => assert_eq!(token, "slack", "{args:?}"),
            other => panic!("expected an unknown subcommand for {args:?}, got {other:?}"),
        }
    }
    assert_eq!(stderr(&["--add-dir", "/tmp", "slack"]), WORKSPACE_MODIFIERS);
}

#[test]
fn parse_interactive_launch_accepts_legacy_and_revision_bearing_upgrade_relaunches() {
    let revision = "abcdef0123456789abcdef0123456789abcdef01";
    assert!(resumes(&["resume", "session-123", UPGRADE_RELAUNCH_ARG]));
    assert!(resumes(&[
        "resume",
        "session-123",
        UPGRADE_RELAUNCH_ARG,
        revision
    ]));
    assert_eq!(
        stderr(&[
            "resume",
            "session-123",
            UPGRADE_RELAUNCH_ARG,
            "not-a-revision"
        ]),
        RESUME_USAGE
    );
    assert_eq!(
        stderr(&["resume", "a", UPGRADE_RELAUNCH_ARG, revision, "extra"]),
        RESUME_USAGE
    );
}

#[test]
fn parse_interactive_launch_shares_native_resume_grammar() {
    for args in [
        &["--resume"][..],
        &["--resume", "last"],
        &["--resume", "session-123"],
        &["session", "resume", "last"],
        &["session", "resume", "--id", "session.v3"],
    ] {
        assert!(resumes(args), "{args:?}");
    }
    assert_eq!(stderr(&["--resume", "one", "two"]), RESUME_USAGE);
    assert!(stderr(&["--add-dir"]).starts_with("oh-fx: --add-dir requires a directory path\n"));
}

#[test]
fn global_modifiers_carry_into_interactive_and_resume_launches() {
    assert!(resumes(&["--model", "x", "--add-dir", "/tmp/shared", "-c"]));
    assert!(is_interactive(&["--fast"]));
    assert!(is_interactive(&["--provider", "my-llm", "--effort=high"]));
    assert!(is_interactive(&["--context-limit", "skill_chunk_bytes=1"]));
}

#[test]
fn workspace_launch_modifiers_preserve_supported_command_help() {
    assert_eq!(
        help(&["--add-dir", "/tmp/shared", "ask", "--help"]),
        Some(TopLevelKind::Ask)
    );
    assert_eq!(
        help(&["--add-dir", "/tmp/shared", "session", "resume", "--help"]),
        Some(TopLevelKind::Session)
    );
    let pr = launch(&["--add-dir", "/tmp/shared", "pr", "context"]);
    assert_eq!(pr.command.kind(), TopLevelKind::Pr);
    assert!(pr.modifiers.adds_directories());
    assert!(!pr.modifiers.sets_context_limits());
}

#[test]
fn workspace_launch_modifiers_still_reject_unsupported_local_command_help() {
    for args in [
        &["--add-dir", "/tmp/shared", "status", "--help"][..],
        &["--add-dir", "x", "status"],
        &["--no-additional-dirs", "help"],
        &["--add-dir", "x", "--version"],
        &["--add-dir", "x", "frob"],
    ] {
        assert_eq!(stderr(args), WORKSPACE_MODIFIERS, "{args:?}");
    }
}

#[test]
fn sessions_v2_reaches_every_command_and_the_ask_options() {
    assert!(
        launch(&["--sessions-v2", "ask", "hi"])
            .modifiers
            .selects_sessions_v2()
    );
    for args in [
        &["--sessions-v2", "status"][..],
        &["--sessions-v2", "upgrade"],
        &["--sessions-v2", "login", "codex"],
        &["--sessions-v2", "sessions", "--json"],
    ] {
        assert!(launch(args).modifiers.selects_sessions_v2(), "{args:?}");
    }
    assert!(matches!(
        parse(&["--sessions-v2", "--version"]),
        Ok(Invocation::Version)
    ));
    assert_eq!(
        top_level_help(&["--sessions-v2", "help"]),
        Some(HelpLayout::Plain)
    );
    assert_eq!(
        help(&["--sessions-v2", "ask", "--help"]),
        Some(TopLevelKind::Ask)
    );
    assert!(is_interactive(&["--sessions-v2"]));
    assert!(resumes(&["--sessions-v2", "--resume", "last"]));
    let ask = launch(&["ask", "--sessions-v2", "hi"]);
    assert!(!ask.modifiers.selects_sessions_v2());
    let Command::Ask(args) = ask.command else {
        panic!("expected ask");
    };
    assert!(args.session.sessions_v2);
}

#[test]
fn model_launch_modifiers_apply_only_to_interactive_sessions() {
    for args in [
        &["--model", "x", "ask", "hi"][..],
        &["--model", "x", "help"],
        &["--model", "x", "--version"],
        &["--fast", "-c", "--help"],
        &["--provider", "grok", "status"],
    ] {
        assert_eq!(
            stderr(args),
            "oh-fx: --provider, --model, --effort, --fast, --provider-order, and --provider-strict apply to interactive sessions; for one-shot runs pass model flags after `oh-fx ask`\n",
            "{args:?}"
        );
    }
}

fn raw_args(args: &[&[u8]]) -> Vec<OsString> {
    args.iter()
        .map(|arg| OsString::from_vec(arg.to_vec()))
        .collect()
}

fn raw_stderr(args: &[&[u8]]) -> String {
    let report = parse_args(raw_args(args)).unwrap_err().report("0.0.0");
    assert_eq!(report.stdout, "", "{args:?}");
    report.stderr
}

#[test]
fn joined_global_modifiers_accept_non_utf8_values() {
    match parse_args(raw_args(&[b"--add-dir=/tmp/\xff", b"ask", b"hi"])) {
        Ok(Invocation::Command(launch)) => assert!(launch.modifiers.adds_directories()),
        other => panic!("expected an ask launch, got {other:?}"),
    }
    assert!(matches!(
        parse_args(raw_args(&[b"--model=m\xff"])),
        Ok(Invocation::Interactive)
    ));
    assert_eq!(
        raw_stderr(&[b"--add-dir=/tmp/\xff", b"status"]),
        WORKSPACE_MODIFIERS
    );
    assert!(raw_stderr(&[b"--context-limit=\xff"]).starts_with(
        "oh-fx: invalid global launch option: InvalidContextLimitOverride\nusage: oh-fx "
    ));
    assert!(raw_stderr(&[b"--provider=\xff"]).starts_with(
        "oh-fx: --provider accepts gateway, codex, grok, or a configured provider name\nusage: oh-fx "
    ));
}

#[test]
fn non_utf8_resume_targets_parse_and_fail_session_id_validation_later() {
    for args in [
        &[&b"--resume"[..], b"\xff"][..],
        &[b"--resume-\xff"],
        &[b"resume", b"\xff"],
        &[b"resume", b"--id", b"\xff"],
        &[b"session", b"resume", b"\xff"],
    ] {
        assert!(
            matches!(parse_args(raw_args(args)), Ok(Invocation::Resume)),
            "{args:?}"
        );
    }
}

#[test]
fn global_workspace_launch_option_errors_use_user_facing_copy() {
    for (args, expected) in [
        (
            &["--add-dir"][..],
            "oh-fx: --add-dir requires a directory path\n",
        ),
        (
            &["--no-additional-dirs", "--no-additional-dirs"],
            "oh-fx: --no-additional-dirs may only be specified once\n",
        ),
        (
            &["--context-limit", "foo=1"],
            "oh-fx: invalid global launch option: UnknownContextLimit\n",
        ),
    ] {
        let text = stderr(args);
        assert!(text.starts_with(expected), "{text}");
        assert!(text.ends_with("<command>\n"), "{text}");
    }
}

#[test]
fn run_if_requested_version_flags_write_configured_version() {
    for args in [
        &["--version"][..],
        &["-v"],
        &["--context-limit=skill_file_bytes=1", "-v"],
    ] {
        assert!(matches!(parse(args), Ok(Invocation::Version)), "{args:?}");
    }
}

#[test]
fn run_if_requested_version_flags_reject_extra_args() {
    for args in [&["--version", "extra"][..], &["-v", "extra"]] {
        assert_eq!(stderr(args), "usage: oh-fx --version\n");
    }
}

#[test]
fn run_if_requested_rejects_removed_record_flag_as_unknown_input() {
    assert!(stderr(&["--record"]).starts_with("oh-fx: unknown subcommand: --record\n\n"));
}

#[test]
fn run_if_requested_invalid_local_flags_write_usage() {
    assert_eq!(
        stderr(&["status", "--wat"]),
        "usage: oh-fx status [--json]\n"
    );
}

#[test]
fn run_if_requested_invalid_json_local_flags_write_json_error() {
    assert_eq!(
        stdout(&["status", "--json", "--wat"]),
        "{\"kind\":\"status\",\"error\":\"invalid arguments\",\"code\":\"InvalidLocalSurfaceArgs\"}\n"
    );
}

#[test]
fn run_if_requested_resume_no_args_returns_last_target() {
    assert!(resumes(&["resume"]));
    assert!(resumes(&["session", "resume"]));
}

#[test]
fn run_if_requested_r_asks_which_session_to_resume() {
    assert!(resumes(&["-r"]));
    assert_eq!(stderr(&["-r", "session.123"]), RESUME_USAGE);
}

#[test]
fn run_if_requested_top_level_resume_aliases_return_the_existing_target() {
    for args in [
        &["--resume"][..],
        &["--resume-last"],
        &["--continue"],
        &["-c"],
        &["--resume", "session.123"],
        &["--resume-session.123"],
        &["resume", "--resume", "--last"],
    ] {
        assert!(resumes(args), "{args:?}");
    }
}

#[test]
fn run_if_requested_rejects_malformed_resume_aliases_with_canonical_usage() {
    for args in [
        &["--resume-"][..],
        &["--resume-last", "unexpected"],
        &["--continue", "unexpected"],
        &["--resume", "   "],
        &["resume", "--resume"],
    ] {
        assert_eq!(stderr(args), RESUME_USAGE, "{args:?}");
    }
}

#[test]
fn run_if_requested_resume_id_returns_owned_id() {
    assert!(resumes(&["resume", "abc123"]));
}

#[test]
fn run_if_requested_invalid_resume_writes_usage() {
    assert_eq!(stderr(&["resume", "a", "b"]), RESUME_USAGE);
}

#[test]
fn run_if_requested_unknown_command_writes_header_and_help() {
    assert!(stderr(&["wat"]).starts_with(
        "oh-fx: unknown subcommand: wat\n\noh-fx v0.0.0\nFast, native coding agent for the terminal.\n"
    ));
}

#[test]
fn run_if_requested_bare_version_subcommand_remains_unknown() {
    assert!(stderr(&["version"]).starts_with(
        "oh-fx: unknown subcommand: version\n\noh-fx v0.0.0\nFast, native coding agent for the terminal.\n"
    ));
}

#[test]
fn unknown_subcommand_help_is_plain_at_the_default_width() {
    let text = stderr(&["-cr"]);
    let help = render_top_level_help(
        crate::command_specs::TOP_LEVEL_HELP_DEFAULT_WIDTH,
        "0.0.0",
        crate::command_specs::HelpStyle::Plain,
    );
    assert_eq!(text, format!("oh-fx: unknown subcommand: -cr\n\n{help}"));
}

#[test]
fn unknown_subcommands_never_echo_terminal_controls_or_raw_bytes() {
    for (raw, echoed) in [
        (&b"\x1b]0;pwned\x07"[..], "\\x1b]0;pwned\\x07"),
        (b"\xff\x1b[31mred", "\\xff\\x1b[31mred"),
        (b"st\xffatus", "st\\xffatus"),
    ] {
        let text = raw_stderr(&[raw]);
        assert!(
            text.starts_with(&format!("oh-fx: unknown subcommand: {echoed}\n\n")),
            "{text:?}"
        );
    }
}

#[test]
fn command_usage_errors_use_the_spec_usage() {
    for (args, expected) in [
        (
            &["acp", "--bogus"][..],
            "usage: oh-fx acp [--model <id>] [--log-file <path>]\n",
        ),
        (
            &["login", "foo"],
            "usage: oh-fx login [vercel|codex|grok]\n",
        ),
        (
            &["logout", "a", "b"],
            "usage: oh-fx logout [vercel|codex|grok]\n",
        ),
        (&["provider"], "usage: oh-fx provider <name>\n"),
        (&["teams", "x"], "usage: oh-fx teams\n"),
        (&["setup", "x"], "usage: oh-fx setup\n"),
        (&["mcp", "bogus"], "usage: oh-fx mcp <command> ...\n"),
        (&["mcp", "auth"], "usage: oh-fx mcp auth NAME\n"),
        (
            &["mcp", "add", "x"],
            "usage: oh-fx mcp add NAME COMMAND [ARGS...] | oh-fx mcp add --transport http NAME URL\n",
        ),
        (
            &["usage", "--period", "1d"],
            "usage: oh-fx usage [--period <24h|7d|30d>] [--json]\n",
        ),
        (
            &["workspace", "add"],
            "usage: oh-fx workspace [list|add PATH|remove PATH|clear] [--json]\n",
        ),
        (
            &["sessions", "--limit", "1_0", "--bogus"],
            "usage: oh-fx sessions [--all] [--limit <1-100>] [--cursor <cursor>] [--json]\n",
        ),
        (
            &["provider", "9"],
            "oh-fx provider: expected gateway, codex, grok, or a configured name\n",
        ),
        (
            &["upgrade", "--json", "--bogus"],
            "oh-fx: InvalidUpgradeArgs\n",
        ),
        (
            &["upgrade", "--channel", "dev"],
            "usage: oh-fx upgrade [--json]\n",
        ),
    ] {
        assert_eq!(stderr(args), expected, "{args:?}");
    }
    assert!(
        stderr(&["session"])
            .starts_with("usage: oh-fx session <last|id>|--id <id> [--json] | session resume")
    );
}

#[test]
fn ask_and_replay_parse_errors_keep_their_own_reports() {
    assert_eq!(
        stderr(&["ask", "--bogus"]),
        format!("usage: oh-fx {}\n", TopLevelKind::Ask.spec().usage)
    );
    match parse(&["ask", "--bogus", "--json"]) {
        Err(CliError::Ask(error)) => {
            assert_eq!(error.kind, crate::cli_ask::AskErrorKind::InvalidAskArgs);
            assert!(error.json);
        }
        other => panic!("expected an ask error, got {other:?}"),
    }
    assert_eq!(
        stderr(&["replay", "a", "b"]),
        "oh-fx replay: too many positional arguments\n"
    );
}

#[test]
fn command_json_errors_use_the_failure_envelope() {
    for (args, expected) in [
        (
            &["session", "--json"][..],
            "{\"kind\":\"session\",\"error\":\"invalid arguments\",\"code\":\"InvalidSessionDetailArgs\"}\n",
        ),
        (
            &["workspace", "add", "--json"],
            "{\"kind\":\"workspace\",\"error\":\"invalid arguments\",\"code\":\"InvalidWorkspaceArgs\"}\n",
        ),
        (
            &["usage", "--period", "1d", "--json"],
            "{\"kind\":\"usage\",\"error\":\"invalid arguments\",\"code\":\"InvalidUsageArgs\"}\n",
        ),
        (
            &["balance", "--json", "--x"],
            "{\"kind\":\"credits\",\"error\":\"invalid arguments\",\"code\":\"InvalidLocalSurfaceArgs\"}\n",
        ),
    ] {
        assert_eq!(stdout(args), expected, "{args:?}");
    }
}
