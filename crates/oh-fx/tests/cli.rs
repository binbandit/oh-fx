use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Output, Stdio};

use ofx_cli::{HelpStyle, TopLevelKind, render_command_help, render_top_level_help};

const SIGPIPE: i32 = 13;

fn run<S: AsRef<OsStr>>(args: &[S], environment: &[(&str, &str)], stdout: Stdio) -> Output {
    let home = tempfile::tempdir().expect("create a temporary home");
    Command::new(env!("CARGO_BIN_EXE_oh-fx"))
        .args(args)
        .current_dir(home.path())
        .env_clear()
        .env("HOME", home.path())
        .env("OH_FX_AUTO_UPGRADE", "0")
        .envs(environment.iter().copied())
        .stdin(Stdio::null())
        .stdout(stdout)
        .output()
        .expect("run oh-fx")
}

fn oh_fx<S: AsRef<OsStr>>(args: &[S], environment: &[(&str, &str)]) -> Output {
    run(args, environment, Stdio::piped())
}

fn into_full_device(args: &[&str]) -> Output {
    let full = File::create("/dev/full").expect("open /dev/full");
    run(args, &[], Stdio::from(full))
}

fn into_closed_pipe(args: &[&str]) -> Output {
    let (reader, writer) = io::pipe().expect("create a pipe");
    drop(reader);
    run(args, &[], Stdio::from(writer))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn top_level_help_follows_the_columns_variable_when_stdout_is_not_a_terminal() {
    for (args, columns, width) in [
        (&["--help"][..], "60", 60),
        (&["-h"], " 120\t", 120),
        (&["help", "ignored"], "0", 80),
        (&["help \t"], "wide", 80),
    ] {
        let output = oh_fx(args, &[("COLUMNS", columns)]);
        assert!(output.status.success(), "{args:?}");
        assert_eq!(
            stdout(&output),
            render_top_level_help(width, ofx_upgrade::VERSION, HelpStyle::Plain),
            "{args:?} {columns:?}"
        );
    }
}

#[test]
fn command_help_comes_from_the_spec_table() {
    for (args, kind) in [
        (&["ask", "--help"][..], TopLevelKind::Ask),
        (&["upgrade", "-h"], TopLevelKind::Upgrade),
        (&["balance", "--help"], TopLevelKind::Credits),
        (&["mcp"], TopLevelKind::Mcp),
    ] {
        let output = oh_fx(args, &[]);
        assert!(output.status.success(), "{args:?}");
        assert_eq!(stdout(&output), render_command_help(kind), "{args:?}");
    }
}

#[test]
fn version_prints_the_build_version_and_rejects_extra_arguments() {
    let output = oh_fx(&["-v"], &[]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), format!("{}\n", ofx_upgrade::VERSION));
    let output = oh_fx(&["--version", "extra"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "usage: oh-fx --version\n");
}

#[test]
fn unknown_commands_print_the_plain_help_on_stderr() {
    let output = oh_fx(&["frobnicate"], &[("COLUMNS", "60")]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        format!(
            "oh-fx: unknown subcommand: frobnicate\n\n{}",
            render_top_level_help(80, ofx_upgrade::VERSION, HelpStyle::Plain)
        )
    );
}

#[test]
fn commands_the_binary_cannot_run_yet_fail_with_one_message() {
    for (args, feature) in [
        (&[][..], "interactive mode"),
        (&["-c"], "resume"),
        (&["status", "--json"], "status"),
        (&["balance"], "credits"),
        (&["sessions"], "sessions"),
    ] {
        let output = oh_fx(args, &[]);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stdout(&output), "", "{args:?}");
        assert_eq!(
            stderr(&output),
            format!("oh-fx: {feature} is not available yet\n"),
            "{args:?}"
        );
    }
}

#[test]
fn upgrade_rejects_unknown_arguments_with_its_usage() {
    for args in [
        &["upgrade", "--json", "--json"][..],
        &["upgrade", "--channel", "dev"],
    ] {
        let output = oh_fx(args, &[]);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(
            stderr(&output),
            "usage: oh-fx upgrade [--json]\n",
            "{args:?}"
        );
    }
}

#[test]
fn unknown_commands_echo_a_terminal_safe_token() {
    for (raw, echoed) in [
        (&b"\x1b]0;pwned\x07"[..], "\\x1b]0;pwned\\x07"),
        (b"b\xffd", "b\\xffd"),
    ] {
        let output = oh_fx(&[OsString::from_vec(raw.to_vec())], &[]);
        assert_eq!(output.status.code(), Some(1));
        let text = stderr(&output);
        assert!(
            text.starts_with(&format!("oh-fx: unknown subcommand: {echoed}\n\n")),
            "{text:?}"
        );
        assert!(!output.stderr.contains(&0x1b), "{text:?}");
        assert!(!output.stderr.contains(&0xff), "{text:?}");
    }
}

#[test]
fn stdout_write_failures_follow_each_upstream_path() {
    for (args, expected) in [
        (&["--help"][..], ""),
        (&["help", "--json"], ""),
        (&["--version"], "oh-fx: WriteFailed\n"),
        (&["status", "--help"], "oh-fx: WriteFailed\n"),
        (&["sessions", "--help"], "oh-fx: WriteFailed\n"),
    ] {
        let output = into_full_device(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stderr(&output), expected, "{args:?}");
    }
    for args in [&["--help"][..], &["--version"], &["sessions", "--help"]] {
        let output = into_closed_pipe(args);
        assert_eq!(output.status.signal(), Some(SIGPIPE), "{args:?}");
    }
    let output = into_closed_pipe(&["status", "--help"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "oh-fx: WriteFailed\n");
}
