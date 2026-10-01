use std::os::unix::ffi::OsStringExt;

use super::*;

fn parse(args: &[&str]) -> Result<AskArgs, AskError> {
    parse_ask(args.iter().map(OsString::from).collect())
}

fn resolve(args: &[&str], stdin: StdinPrompt) -> Result<String, AskError> {
    parse(args)?.resolve_prompt(|| stdin)
}

fn parsed(args: &[&str]) -> AskArgs {
    let options = parse(args).unwrap();
    options.resolve_prompt(|| StdinPrompt::Terminal).unwrap();
    options
}

fn error(args: &[&str]) -> AskErrorKind {
    resolve(args, StdinPrompt::Terminal).unwrap_err().kind
}

fn prompt(options: &AskArgs) -> String {
    options
        .resolve_prompt(|| unreachable!("stdin must stay unread"))
        .unwrap()
}

fn raw(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

#[test]
fn parse_options_preserves_active_ask_flags_and_operands() {
    let options = parsed(&[
        "--auto",
        "--image",
        "a.png",
        "--system",
        "first",
        "--system",
        "second",
        "--json",
        "--prompt-permissions",
        "--quiet",
        "--verbose",
        "--no-save",
        "--no-color",
        "--timeout",
        "123",
        "hello",
        "world",
    ]);
    assert_eq!(options.permissions.mode, Some(PermissionMode::Auto));
    assert!(options.output.json);
    assert!(options.output.quiet);
    assert!(options.output.no_color);
    assert!(options.permissions.prompt);
    assert!(options.session.no_save);
    assert!(options.timeout);
    assert_eq!(options.system_prompt.as_deref(), Some("second"));
    assert!(options.images);
    assert_eq!(prompt(&options), "hello world");
}

#[test]
fn parse_options_accepts_full_access_aliases_and_rejects_permission_flag_conflicts() {
    for alias in ["--full-access", "--yolo"] {
        let options = parsed(&[alias, "hello"]);
        assert_eq!(options.permissions.mode, Some(PermissionMode::Yolo));
        assert_eq!(prompt(&options), "hello");
    }
    let flags = ["--auto", "--full-access", "--yolo"];
    for first in flags {
        for second in flags {
            assert_eq!(
                error(&[first, second, "hello"]),
                AskErrorKind::InvalidAskArgs
            );
        }
    }
}

#[test]
fn parse_options_preserves_full_access_aliases_after_the_delimiter_as_prompt_text() {
    let literal = parsed(&["--", "--full-access", "--yolo", "--auto"]);
    assert_eq!(literal.permissions.mode, None);
    assert_eq!(prompt(&literal), "--full-access --yolo --auto");

    for (flag, mode) in [
        ("--auto", PermissionMode::Auto),
        ("--full-access", PermissionMode::Yolo),
        ("--yolo", PermissionMode::Yolo),
    ] {
        let options = parsed(&[flag, "--", "--full-access", "--yolo", "--auto"]);
        assert_eq!(options.permissions.mode, Some(mode));
        assert_eq!(prompt(&options), "--full-access --yolo --auto");
    }
}

#[test]
fn parse_options_preserves_model_effort_and_fast_overrides() {
    let options = parsed(&[
        "--model",
        "provider/override-model",
        "--effort",
        "high",
        "--fast",
        "hello",
    ]);
    assert_eq!(
        options.model.as_deref(),
        Some(OsStr::new("provider/override-model"))
    );
    assert_eq!(prompt(&options), "hello");
    assert_eq!(prompt(&parsed(&["--no-fast", "hello"])), "hello");
    assert_eq!(prompt(&parsed(&["--effort", "auto", "hello"])), "hello");
    assert_eq!(parsed(&["hello"]).model, None);

    let last_model = parsed(&["--model", "first/model", "--model", "second/model", "hello"]);
    assert_eq!(
        last_model.model.as_deref(),
        Some(OsStr::new("second/model"))
    );
    let trimmed = parsed(&["--model", " \tspaced\r\n", "hello"]);
    assert_eq!(trimmed.model.as_deref(), Some(OsStr::new("spaced")));
}

#[test]
fn parse_options_keep_non_utf8_models_and_reject_non_utf8_system_prompts() {
    let options = parse_ask(vec![
        OsString::from("--model"),
        raw(b" m\xff "),
        OsString::from("hi"),
    ])
    .unwrap();
    assert_eq!(options.model, Some(raw(b"m\xff")));
    assert_eq!(prompt(&options), "hi");
    for json in [false, true] {
        let mut args = vec![
            OsString::from("--system"),
            raw(b"s\xff"),
            OsString::from("hi"),
        ];
        if json {
            args.insert(0, OsString::from("--json"));
        }
        let error = parse_ask(args).unwrap_err();
        assert_eq!(error.kind, AskErrorKind::InvalidAskArgs);
        assert_eq!(error.json, json);
    }
}

#[test]
fn parse_options_accepts_provider_routing_flags_and_rejects_malformed_values() {
    for args in [
        &[
            "--provider-order",
            "azure,anthropic",
            "--provider-strict",
            "hello",
        ][..],
        &["--provider-order=bedrock", "--no-provider-strict", "hello"],
        &["--provider-order", "a", "--provider-order", "b", "hello"],
    ] {
        assert_eq!(prompt(&parsed(args)), "hello", "{args:?}");
    }
    assert_eq!(error(&["--provider-order"]), AskErrorKind::InvalidAskArgs);
    assert_eq!(
        error(&["--provider-order=Bad Slug", "hello"]),
        AskErrorKind::InvalidAskArgs
    );
    assert_eq!(
        error(&["--provider-strict", "--no-provider-strict", "hello"]),
        AskErrorKind::InvalidAskArgs
    );
}

#[test]
fn parse_options_rejects_invalid_model_effort_and_fast_flag_forms() {
    for args in [
        &["--model"][..],
        &["--model", "  ", "hello"],
        &["--effort"],
        &["--effort", "not an effort", "hello"],
        &["--fast", "--no-fast", "hello"],
        &["--no-fast", "--fast", "hello"],
        &["--model=inline", "hello"],
    ] {
        assert_eq!(error(args), AskErrorKind::InvalidAskArgs, "{args:?}");
    }
    let literal = parsed(&["--", "--model", "--fast"]);
    assert_eq!(literal.model, None);
    assert_eq!(prompt(&literal), "--model --fast");
}

#[test]
fn parse_options_rejects_unknown_flags_and_accepts_dash_prompts_after_sentinel() {
    assert_eq!(
        error(&["--timeout", "nope", "--not-a-flag", "prompt"]),
        AskErrorKind::InvalidAskArgs
    );
    let options = parsed(&["--timeout", "nope", "--", "--not-a-flag", "prompt"]);
    assert!(!options.timeout);
    assert_eq!(prompt(&options), "--not-a-flag prompt");
    assert_eq!(prompt(&parsed(&["-", "x"])), "- x");
}

#[test]
fn parse_options_accept_sessions_v2_anywhere_before_the_delimiter() {
    for args in [
        &["--sessions-v2", "hello"][..],
        &["hello", "--sessions-v2"],
        &["--sessions-v2", "--no-save", "--sessions-v2", "hello"],
    ] {
        let options = parsed(args);
        assert!(options.session.sessions_v2, "{args:?}");
        assert_eq!(prompt(&options), "hello", "{args:?}");
    }
    assert!(!parsed(&["hello"]).session.sessions_v2);
    let options = parsed(&["--", "--sessions-v2"]);
    assert!(!options.session.sessions_v2);
    assert_eq!(prompt(&options), "--sessions-v2");
    assert_eq!(
        error(&["--sessions-v2=1", "hello"]),
        AskErrorKind::InvalidAskArgs
    );
}

#[test]
fn parse_options_reports_missing_operands_and_empty_tty_input_as_missing_prompt() {
    for args in [&["--image"][..], &["--system"], &["--timeout"], &[]] {
        assert_eq!(error(args), AskErrorKind::MissingPrompt, "{args:?}");
    }
}

#[test]
fn stdin_prompt_resource_limit_accepts_exact_bytes_and_rejects_one_over_without_a_partial_prompt() {
    let exact = vec![b'a'; ASK_STDIN_PROMPT_LIMIT_BYTES];
    assert!(resolve(&[], StdinPrompt::Bytes(exact)).is_ok());
    let over = vec![b'a'; ASK_STDIN_PROMPT_LIMIT_BYTES + 1];
    assert_eq!(
        resolve(&[], StdinPrompt::Bytes(over)).unwrap_err().kind,
        AskErrorKind::PromptResourceLimitExceeded
    );
    assert_eq!(
        resolve(&[], StdinPrompt::ReadFailed).unwrap_err().kind,
        AskErrorKind::PromptInputReadFailed
    );
}

#[test]
fn stdin_is_read_only_without_prompt_arguments() {
    let options = parse(&["--verbose", "hello"]).unwrap();
    assert_eq!(prompt(&options), "hello");
}

#[test]
fn parse_options_trims_explicit_stdin_fallback() {
    assert_eq!(
        resolve(
            &[],
            StdinPrompt::Bytes(b" \n prompt from stdin \t".to_vec())
        )
        .unwrap(),
        "prompt from stdin"
    );
    assert_eq!(
        resolve(&[], StdinPrompt::Bytes(b" \r\n\t ".to_vec()))
            .unwrap_err()
            .kind,
        AskErrorKind::MissingPrompt
    );
}

#[test]
fn parse_options_rejects_model_unsafe_prompt_text_from_arguments_and_stdin() {
    assert_eq!(error(&["bad\0prompt"]), AskErrorKind::InvalidPromptText);
    assert_eq!(
        resolve(&[], StdinPrompt::Bytes(b"bad\xffprompt".to_vec()))
            .unwrap_err()
            .kind,
        AskErrorKind::InvalidPromptText
    );
}

#[test]
fn parse_options_preserves_exact_resume_id_operands() {
    let options = parsed(&["--resume-id", "last", "continue"]);
    assert_eq!(options.session.resume_flag, Some("--resume-id"));
    assert_eq!(prompt(&options), "continue");
    assert_eq!(
        parsed(&["--resume", " last ", "continue"])
            .session
            .resume_flag,
        Some("--resume")
    );
    for args in [
        &["--resume"][..],
        &["--resume", " \t", "x"],
        &["--resume-id", ""],
    ] {
        assert_eq!(error(args), AskErrorKind::InvalidAskArgs, "{args:?}");
    }
}

#[test]
fn parse_options_requires_an_explicit_saved_session_for_recovery_continuation() {
    let options = parsed(&["--resume-id", "session.v3", "--continue-recovery"]);
    assert!(options.session.continue_recovery);
    assert_eq!(prompt(&options), "");
    assert_eq!(
        error(&["--continue-recovery"]),
        AskErrorKind::InvalidAskArgs
    );
    assert_eq!(
        error(&["--resume", "last", "--continue-recovery", "new prompt"]),
        AskErrorKind::InvalidAskArgs
    );
    assert_eq!(
        error(&[
            "--resume",
            "last",
            "--continue-recovery",
            "--image",
            "a.png"
        ]),
        AskErrorKind::InvalidAskArgs
    );
    assert_eq!(
        error(&[
            "--resume",
            "last",
            "--continue-recovery",
            "--continue-recovery"
        ]),
        AskErrorKind::InvalidAskArgs
    );
}

#[test]
fn parse_options_rejects_repeated_resume_targets_and_no_save_resume() {
    assert_eq!(
        error(&["--resume", "last", "--resume-id", "session.v3", "continue"]),
        AskErrorKind::InvalidAskArgs
    );
    assert_eq!(
        error(&["--no-save", "--resume", "last", "continue"]),
        AskErrorKind::NoSaveResumeConflict
    );
    assert_eq!(
        error(&["--resume-id", "session.v3", "--no-save", "continue"]),
        AskErrorKind::NoSaveResumeConflict
    );
    assert_eq!(
        error(&["--no-save", "--resume", "last"]),
        AskErrorKind::MissingPrompt
    );
    assert_eq!(
        resolve(
            &["--no-save", "--resume", "last"],
            StdinPrompt::Bytes(b"piped".to_vec())
        )
        .unwrap_err()
        .kind,
        AskErrorKind::NoSaveResumeConflict
    );
}

#[test]
fn timeouts_follow_integer_parsing_and_ignore_malformed_values() {
    for valid in ["+2", "-0", "1_0", "0", "18446744073709551"] {
        assert!(parsed(&["--timeout", valid, "x"]).timeout, "{valid}");
    }
    for invalid in ["-1", "18446744073709552", "18446744073709551615", "", " 1"] {
        assert!(!parsed(&["--timeout", invalid, "x"]).timeout, "{invalid}");
    }
    assert!(!parsed(&["--timeout", "5", "--timeout", "never", "x"]).timeout);
    let non_utf8 = parse_ask(vec![
        OsString::from("--timeout"),
        raw(b"5\xff"),
        OsString::from("x"),
    ])
    .unwrap();
    assert!(!non_utf8.timeout);
}

#[test]
fn json_errors_follow_flags_before_the_delimiter() {
    assert!(parse(&["--bogus", "--json"]).unwrap_err().json);
    assert!(!parse(&["--bogus", "--", "--json"]).unwrap_err().json);
    assert!(parse(&["--model", "--json", "--bogus"]).unwrap_err().json);
}

#[test]
fn ask_errors_render_upstream_text_reports_and_error_names() {
    let usage = format!("usage: oh-fx {}\n", TopLevelKind::Ask.spec().usage);
    let report = |kind| AskError { kind, json: false }.report();

    assert_eq!(
        report(AskErrorKind::MissingPrompt).stderr,
        format!("oh-fx ask: missing prompt\n{usage}")
    );
    assert_eq!(
        report(AskErrorKind::NoSaveResumeConflict).stderr,
        format!("oh-fx ask: --no-save cannot be used with --resume or --resume-id\n{usage}")
    );
    assert_eq!(report(AskErrorKind::InvalidAskArgs).stderr, usage);
    assert_eq!(
        report(AskErrorKind::PromptResourceLimitExceeded).stderr,
        "oh-fx ask: prompt exceeds the local input safety limit\n"
    );
    assert_eq!(
        report(AskErrorKind::PromptInputReadFailed).stderr,
        "oh-fx ask: failed to read prompt from stdin\n"
    );
    assert_eq!(
        report(AskErrorKind::InvalidPromptText).stderr,
        "oh-fx ask: prompt must be valid UTF-8 and contain no NUL bytes\n"
    );
    assert_eq!(
        AskErrorKind::PromptResourceLimitExceeded.name(),
        "PromptResourceLimitExceeded"
    );
    assert_eq!(AskErrorKind::NoSaveResumeConflict.name(), "InvalidAskArgs");
}
