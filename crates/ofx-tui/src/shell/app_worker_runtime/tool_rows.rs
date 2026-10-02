use std::path::PathBuf;

use ofx_contract::{
    ActionLabel, ApprovalRequest, ApprovalScope, CallDescription, CommandProcessPresentation,
    Concurrency, FileChangeStats, FileMutation, FileMutationState, PathAccess, RequestId,
    ToolActivity, ToolCallId, ToolDeferral, ToolEffect, ToolRejection, ToolResultStatus,
    ToolStatusDetail, TurnId, TurnOutcome, UiCommand, UiEvent, tool_permission_denied_json,
};

use super::super::test_shell::TestShell;

const TURN: u64 = 1;

fn turn() -> TurnId {
    TurnId::new(TURN)
}

fn description(
    activity: ToolActivity,
    label: (&'static str, &'static str, &str),
) -> CallDescription {
    CallDescription {
        title: format!("{} {}", label.0, label.2),
        label: Some(ActionLabel {
            active: label.0,
            completed: label.1,
            target: label.2.to_owned(),
        }),
        activity,
        effect: ToolEffect::ReadOnly,
        concurrency: Concurrency::Parallel,
    }
}

fn started(
    call: &str,
    tool: &str,
    activity: ToolActivity,
    label: (&'static str, &'static str, &str),
) -> UiEvent {
    UiEvent::ToolStarted {
        turn_id: turn(),
        call_id: ToolCallId::new(call),
        tool_name: tool.to_owned(),
        description: description(activity, label),
    }
}

fn read(call: &str, path: &str) -> UiEvent {
    started(
        call,
        "read_file",
        ToolActivity::Read,
        ("Reading", "Read", path),
    )
}

fn command(call: &str, text: &str) -> UiEvent {
    started(
        call,
        "shell",
        ToolActivity::Command,
        ("Running", "Ran", text),
    )
}

struct Outcome {
    status: ToolResultStatus,
    content: String,
    process: Option<CommandProcessPresentation>,
    status_detail: Option<ToolStatusDetail>,
    file_change: Option<FileChangeStats>,
}

fn success() -> Outcome {
    Outcome {
        status: ToolResultStatus::Success,
        content: String::new(),
        process: None,
        status_detail: None,
        file_change: None,
    }
}

fn failure(content: &str) -> Outcome {
    Outcome {
        status: ToolResultStatus::Failure,
        content: content.to_owned(),
        ..success()
    }
}

fn finished(call: &str, tool: &str, outcome: Outcome) -> UiEvent {
    UiEvent::ToolFinished {
        turn_id: turn(),
        call_id: ToolCallId::new(call),
        tool_name: tool.to_owned(),
        arguments: "{}".to_owned(),
        status: outcome.status,
        content: outcome.content,
        command_result: None,
        process: outcome.process,
        status_detail: outcome.status_detail,
        file_change: outcome.file_change,
    }
}

fn text(text: &str) -> UiEvent {
    UiEvent::AssistantText {
        turn_id: turn(),
        text: text.to_owned(),
    }
}

fn turn_finished(outcome: TurnOutcome) -> UiEvent {
    UiEvent::TurnFinished {
        turn_id: turn(),
        outcome,
    }
}

fn running(prompt: &str) -> TestShell {
    let mut test = TestShell::start();
    test.submit(prompt);
    test.deliver(UiEvent::TurnStarted { turn_id: turn() });
    test
}

fn rows(screen: &str) -> Vec<&str> {
    screen.lines().map(str::trim_end).collect()
}

fn block<'a>(screen: &'a str, first: &str, count: usize) -> Vec<&'a str> {
    let rows = rows(screen);
    let start = rows
        .iter()
        .position(|row| *row == first)
        .unwrap_or_else(|| panic!("{first:?} in\n{screen}"));
    rows[start..(start + count).min(rows.len())].to_vec()
}

#[test]
fn each_call_adds_a_row_to_one_group_until_prose_follows() {
    let mut test = running("run the scenario");
    test.deliver(text("I will read it."));
    test.deliver(read("a", "README.md"));
    test.deliver(started(
        "b",
        "glob_files",
        ToolActivity::List,
        ("Matching", "Matched", "*.rs"),
    ));
    test.deliver(finished("a", "read_file", success()));
    test.deliver(finished("b", "glob_files", success()));
    test.deliver(command("c", "printf 'one\\ntwo'; echo err >&2"));
    test.deliver(finished("c", "shell", success()));
    test.deliver(command("d", "exit 7"));
    test.deliver(finished(
        "d",
        "shell",
        Outcome {
            process: Some(CommandProcessPresentation::ExitCode(7)),
            ..failure("")
        },
    ));
    test.deliver(started(
        "e",
        "grep_files",
        ToolActivity::Read,
        ("Searching", "Searched", "beta"),
    ));
    test.deliver(finished("e", "grep_files", success()));
    test.deliver(text("It describes a service.\n"));
    test.deliver(turn_finished(TurnOutcome::Completed));
    let screen = test.screen();
    assert_eq!(
        block(&screen, "  I will read it.", 10),
        [
            "  I will read it.",
            "",
            "● 5 tool calls · 2 read · 2 commands · 1 list · 1 failed",
            "├ Read README.md",
            "├ Matched *.rs",
            "├ Ran printf 'one\\ntwo'; echo err >&2",
            "├ Exited 7 exit 7",
            "└ Searched beta",
            "",
            "  It describes a service.",
        ],
        "{screen}"
    );
}

#[test]
fn group_rows_use_the_upstream_palette() {
    let mut test = running("go");
    test.deliver(command("a", "cat log.txt | head -80"));
    test.deliver(finished("a", "shell", success()));
    test.deliver(turn_finished(TurnOutcome::Completed));
    let written = test.written();
    assert!(
        written.contains("\u{1b}[0;38;5;255m●\u{1b}[0m \u{1b}[0;38;5;245m1 tool call · 1 command"),
        "{written:?}"
    );
    assert!(written.contains("\u{1b}[0;38;5;245m└ Ran \u{1b}[0;38;5;252mcat\u{1b}[0;38;5;245m log.txt \u{1b}[0;38;5;252m|\u{1b}[0;38;5;245m \u{1b}[0;38;5;252mhead\u{1b}[0;38;5;245m \u{1b}[0;38;5;250m-80\u{1b}[0m"), "{written:?}");
}

#[test]
fn running_rows_stay_live_and_reach_the_scrollback_once_settled() {
    let mut test = running("go");
    test.deliver(read("a", "notes.md"));
    test.deliver(command("b", "sleep 8"));
    let screen = test.screen();
    assert!(
        screen.contains("● 2 tool calls · 1 read · 1 command\n├ Reading notes.md\n└ Running sleep 8\n\n• Running (0s)"),
        "{screen}"
    );
    test.deliver(finished("a", "read_file", success()));
    assert!(test.screen().contains("├ Read notes.md\n└ Running sleep 8"));
    test.deliver(finished("b", "shell", success()));
    test.deliver(text("Done.\n"));
    test.deliver(turn_finished(TurnOutcome::Completed));
    let screen = test.screen();
    assert!(
        screen.contains(
            "● 2 tool calls · 1 read · 1 command\n├ Read notes.md\n└ Ran sleep 8\n\n  Done."
        ),
        "{screen}"
    );
    assert_eq!(screen.matches("tool calls").count(), 1, "{screen}");
}

#[test]
fn escape_cancels_running_calls_in_place_of_the_turn_notice() {
    let mut test = running("go");
    test.deliver(read("a", "README.md"));
    test.deliver(finished("a", "read_file", success()));
    test.deliver(command("b", "sleep 8; echo done"));
    test.draining(super::super::Shell::cancel_visible_turn);
    let screen = test.screen();
    assert_eq!(
        block(
            &screen,
            "● 2 tool calls · 1 read · 1 command · 1 cancelled",
            5
        ),
        [
            "● 2 tool calls · 1 read · 1 command · 1 cancelled",
            "├ Read README.md",
            "└ Cancelled sleep 8; echo done",
            "",
            "■ Cancelled sleep 8; echo done · What can oh-fx do differently?",
        ],
        "{screen}"
    );
    assert_eq!(
        screen.matches("What can oh-fx do differently?").count(),
        1,
        "{screen}"
    );
    assert!(test.sent().contains(&UiCommand::Cancel { turn_id: turn() }));
    test.deliver(finished("b", "shell", failure("cancelled")));
    test.deliver(turn_finished(TurnOutcome::Interrupted));
    assert_eq!(test.screen().matches("Cancelled sleep 8").count(), 2);
}

#[test]
fn escape_after_every_call_settled_keeps_the_turn_notice() {
    let mut test = running("go");
    test.deliver(read("a", "README.md"));
    test.deliver(finished("a", "read_file", success()));
    test.draining(super::super::Shell::cancel_visible_turn);
    let screen = test.screen();
    assert!(
        screen.contains("└ Read README.md\n\n■ Cancelled · What can oh-fx do differently?"),
        "{screen}"
    );
}

fn approval(
    call: &str,
    tool: &str,
    activity: ToolActivity,
    label: (&'static str, &'static str, &str),
    file: Option<FileMutation>,
) -> UiEvent {
    UiEvent::ApprovalRequested {
        turn_id: turn(),
        request: Box::new(ApprovalRequest {
            id: RequestId::new(7),
            call_id: ToolCallId::new(call),
            tool_name: tool.to_owned(),
            description: description(activity, label),
            tool_arguments_preview: String::new(),
            tool_arguments_truncated: false,
            scope: ApprovalScope {
                target: Some(PathBuf::from("/home/notes.txt")),
                access: PathAccess::Within(PathBuf::from("/home")),
                always: None,
            },
            command: None,
            file,
        }),
    }
}

#[test]
fn a_call_awaiting_approval_shows_as_running_and_settles_with_its_decision() {
    let mut test = running("go");
    test.deliver(approval(
        "a",
        "read_file",
        ToolActivity::Read,
        ("Reading", "Read", "../notes.txt"),
        None,
    ));
    let screen = test.screen();
    assert!(
        screen.contains("● 1 tool call · 1 read\n└ Reading ../notes.txt"),
        "{screen}"
    );
    test.deliver(read("a", "../notes.txt"));
    test.deliver(finished(
        "a",
        "read_file",
        failure(&tool_permission_denied_json("read_file")),
    ));
    test.deliver(turn_finished(TurnOutcome::Completed));
    let screen = test.screen();
    assert!(
        screen.contains("● 1 tool call · 1 read · 1 denied\n└ Denied ../notes.txt\n"),
        "{screen}"
    );
}

#[test]
fn cancelling_a_pending_file_change_names_no_target() {
    let mut test = running("go");
    let change = FileMutation {
        target: PathBuf::from("/workspace/new.txt"),
        state: FileMutationState::Creates,
    };
    test.deliver(approval(
        "a",
        "write_file",
        ToolActivity::Write,
        ("Writing", "Wrote", "new.txt"),
        Some(change),
    ));
    let screen = test.screen();
    assert!(!screen.contains("tool call"), "{screen}");
    test.draining(super::super::Shell::approval_escape);
    let screen = test.screen();
    assert_eq!(
        block(&screen, "● 1 tool call · 1 write · 1 cancelled", 4),
        [
            "● 1 tool call · 1 write · 1 cancelled",
            "└ Cancelled file",
            "",
            "■ Cancelled file · What can oh-fx do differently?",
        ],
        "{screen}"
    );
}

#[test]
fn failures_rejections_and_deferrals_name_their_outcome() {
    let mut test = running("go");
    test.deliver(read("a", "missing.txt"));
    test.deliver(finished(
        "a",
        "read_file",
        Outcome {
            status_detail: Some(ToolStatusDetail::PreflightFailed),
            ..failure("Path not found: missing.txt")
        },
    ));
    test.deliver(UiEvent::ToolRejected {
        turn_id: turn(),
        call_id: ToolCallId::new("b"),
        tool_name: "read_file".to_owned(),
        arguments: "{\"path\":".to_owned(),
        reason: ToolRejection::MalformedArguments,
        description: None,
        content: String::new(),
    });
    test.deliver(UiEvent::ToolRejected {
        turn_id: turn(),
        call_id: ToolCallId::new("c"),
        tool_name: "no_such_tool".to_owned(),
        arguments: "{}".to_owned(),
        reason: ToolRejection::Unsupported,
        description: None,
        content: "Unsupported tool: no_such_tool".to_owned(),
    });
    test.deliver(started(
        "d",
        "edit_file",
        ToolActivity::Edit,
        ("Editing", "Edited", "README.md"),
    ));
    test.deliver(finished(
        "d",
        "edit_file",
        Outcome {
            file_change: Some(FileChangeStats {
                additions: 2,
                deletions: 1,
            }),
            ..success()
        },
    ));
    test.deliver(started(
        "e",
        "write_file",
        ToolActivity::Write,
        ("Writing", "Wrote", "file"),
    ));
    test.deliver(UiEvent::ToolDeferred {
        turn_id: turn(),
        call_id: ToolCallId::new("e"),
        deferral: ToolDeferral::TargetChanged,
    });
    test.deliver(read("f", "runtime.zig"));
    test.deliver(UiEvent::ToolDeferred {
        turn_id: turn(),
        call_id: ToolCallId::new("f"),
        deferral: ToolDeferral::ProjectInstructions,
    });
    test.deliver(command("g", "sleep 5"));
    test.deliver(finished(
        "g",
        "shell",
        Outcome {
            process: Some(CommandProcessPresentation::TimedOut),
            ..failure("")
        },
    ));
    test.deliver(turn_finished(TurnOutcome::Completed));
    let screen = test.screen();
    assert_eq!(
        block(
            &screen,
            "● 7 tool calls · 2 read · 1 write · 1 edit · 1 command · 1 timed out · 3 failed…",
            8
        ),
        [
            "● 7 tool calls · 2 read · 1 write · 1 edit · 1 command · 1 timed out · 3 failed…",
            "├ Failed missing.txt: Path not found: missing.txt",
            "├ Failed tool call: invalid JSON arguments",
            "├ Failed no_such_tool",
            "├ Edited README.md +2 / -1",
            "├ Not executed file",
            "├ Reading project instructions before continuing: runtime.zig",
            "└ Timed out sleep 5",
        ],
        "{screen}"
    );
}

#[test]
fn hostile_targets_and_long_commands_never_reach_the_terminal_raw() {
    let mut test = running("go");
    let long = format!("printf {}", "alpha-beta-gamma-".repeat(40));
    test.deliver(read("a", "evil\u{1b}]0;pwned\u{7}\u{1b}[2J\u{202e}.txt"));
    test.deliver(command("b", &long));
    test.deliver(finished("a", "read_file", success()));
    test.deliver(finished("b", "shell", success()));
    test.deliver(turn_finished(TurnOutcome::Completed));
    let written = test.written();
    assert!(!written.contains('\u{7}'), "{written:?}");
    assert!(!written.contains("\u{1b}]"), "{written:?}");
    assert!(!written.contains("\u{1b}[2J"), "{written:?}");
    assert!(!written.contains('\u{202e}'), "{written:?}");
    let screen = test.screen();
    assert!(
        screen.contains(
            "├ Read evil\\x1b]0;pwned\\x07\\x1b[2J\\u{202e}.txt\n└ Ran printf alpha-beta-gamma-"
        ),
        "{screen}"
    );
    let ran = rows(&screen)
        .into_iter()
        .find(|row| row.starts_with("└ Ran"))
        .unwrap()
        .to_owned();
    assert_eq!(ran.chars().count(), 80, "{ran}");
    assert!(ran.ends_with('…'), "{ran}");
}

#[test]
fn resizing_reflows_rows_at_the_new_width() {
    let mut test = running("go");
    let long = format!("printf {}", "alpha-beta-gamma-".repeat(10));
    test.deliver(command("a", &long));
    test.deliver(finished("a", "shell", success()));
    test.deliver(turn_finished(TurnOutcome::Completed));
    let screen = test.screen();
    assert!(
        screen.contains(&format!("└ Ran {}…", &long[..73])),
        "{screen}"
    );
    test.resize(24, 200);
    let screen = test.screen();
    assert!(screen.contains(&format!("└ Ran {long}\n")), "{screen}");
    test.resize(24, 30);
    let screen = test.screen();
    assert!(
        screen.contains("● 1 tool call · 1 command\n└ Ran printf alpha-beta-gamma…"),
        "{screen}"
    );
    test.resize(24, 12);
    let screen = test.screen();
    assert!(screen.contains("● 1 tool ca…\n└ Ran print…"), "{screen}");
}

#[test]
fn wide_characters_clip_by_their_cells() {
    let mut test = running("go");
    test.deliver(read("a", &"界".repeat(60)));
    test.deliver(read("b", &"😀".repeat(60)));
    test.deliver(finished("a", "read_file", success()));
    test.deliver(finished("b", "read_file", success()));
    test.deliver(text("Done.\n"));
    test.deliver(turn_finished(TurnOutcome::Completed));
    let screen = test.screen();
    assert_eq!(
        block(&screen, "● 2 tool calls · 2 read", 5),
        [
            "● 2 tool calls · 2 read",
            &format!("├ Read {}…", "界".repeat(36)),
            &format!("└ Read {}…", "😀".repeat(36)),
            "",
            "  Done.",
        ],
        "{screen}"
    );
}

#[test]
fn a_group_taller_than_the_screen_reaches_the_scrollback_whole() {
    let mut test = running("go");
    for index in 0..60 {
        test.deliver(read(&format!("call-{index}"), &format!("file-{index}.txt")));
    }
    let live = test.screen();
    assert!(live.contains("Reading file-59.txt"), "{live}");
    assert!(!live.contains("Reading file-0.txt"), "{live}");
    for index in 0..60 {
        test.deliver(finished(&format!("call-{index}"), "read_file", success()));
    }
    test.deliver(text("Done.\n"));
    test.deliver(turn_finished(TurnOutcome::Completed));
    let written = test.written();
    for index in 0..60 {
        assert_eq!(
            written
                .matches(&format!("Read file-{index}.txt\u{1b}"))
                .count(),
            1,
            "{index}"
        );
    }
    assert_eq!(written.matches("60 tool calls · 60 read").count(), 1);
}

#[test]
fn notices_and_hidden_turns_leave_running_rows_alone() {
    let mut test = running("go");
    test.deliver(read("a", "a.txt"));
    test.deliver(UiEvent::Notice {
        notice: ofx_contract::Notice::new(
            ofx_contract::NoticeTone::Warning,
            "context",
            "rules changed",
        ),
    });
    test.deliver(UiEvent::ToolStarted {
        turn_id: TurnId::new(9),
        call_id: ToolCallId::new("stray"),
        tool_name: "read_file".to_owned(),
        description: description(ToolActivity::Read, ("Reading", "Read", "stray.txt")),
    });
    test.deliver(read("b", "b.txt"));
    test.deliver(finished("a", "read_file", success()));
    test.deliver(finished("b", "read_file", success()));
    test.deliver(turn_finished(TurnOutcome::Completed));
    let screen = test.screen();
    assert!(!screen.contains("stray"), "{screen}");
    assert!(
        screen.contains("● 1 tool call · 1 read\n└ Read a.txt\n\n! context: rules changed\n\n● 1 tool call · 1 read\n└ Read b.txt"),
        "{screen}"
    );
}

#[test]
fn a_reused_call_id_settles_the_newest_row_with_that_id() {
    let mut test = running("go");
    test.deliver(read("dup", "a.txt"));
    test.deliver(finished("dup", "read_file", success()));
    test.deliver(read("dup", "b.txt"));
    test.deliver(finished("dup", "read_file", failure("boom")));
    test.deliver(turn_finished(TurnOutcome::Completed));
    let screen = test.screen();
    assert!(
        screen.contains("● 2 tool calls · 2 read · 1 failed\n├ Read a.txt\n└ Failed b.txt\n"),
        "{screen}"
    );
}
