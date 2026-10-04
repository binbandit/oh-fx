use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
#[cfg(target_os = "linux")]
use std::io::Read;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{PoisonError, RwLock};

#[cfg(target_os = "linux")]
use rustix::fs::{Mode, OFlags};
#[cfg(target_os = "linux")]
use rustix::io::Errno;
#[cfg(target_os = "linux")]
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
#[cfg(target_os = "linux")]
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
#[cfg(target_os = "linux")]
use rustix::termios::{OptionalActions, OutputModes, Winsize, tcgetattr, tcsetattr, tcsetwinsize};

use ofx_cli::{HelpStyle, TopLevelKind, render_command_help, render_top_level_help};

const SIGPIPE: i32 = 13;
const SIGKILL: i32 = 9;

static FORKS: RwLock<()> = RwLock::new(());

fn spawn(command: &mut Command) -> Child {
    let _forking = FORKS.read().unwrap_or_else(PoisonError::into_inner);
    command.spawn().expect("run oh-fx")
}

fn run<S: AsRef<OsStr>>(args: &[S], environment: &[(&str, &str)], stdout: Stdio) -> Output {
    let home = tempfile::tempdir().expect("create a temporary home");
    spawn(
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(args)
            .current_dir(home.path())
            .env_clear()
            .env("HOME", home.path())
            .env("OH_FX_AUTO_UPGRADE", "0")
            .envs(environment.iter().copied())
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::piped()),
    )
    .wait_with_output()
    .expect("wait for oh-fx")
}

fn oh_fx<S: AsRef<OsStr>>(args: &[S], environment: &[(&str, &str)]) -> Output {
    run(args, environment, Stdio::piped())
}

#[cfg(target_os = "linux")]
fn into_full_device(args: &[&str]) -> Output {
    let full = File::create("/dev/full").expect("open /dev/full");
    run(args, &[], Stdio::from(full))
}

fn into_closed_pipe(args: &[&str]) -> Output {
    let writer = {
        let _no_forks = FORKS.write().unwrap_or_else(PoisonError::into_inner);
        let (reader, writer) = io::pipe().expect("create a pipe");
        drop(reader);
        writer
    };
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
    for (args, token) in [
        (&["frobnicate"][..], "frobnicate"),
        (&["slack"], "slack"),
        (&["slack", "install"], "slack"),
        (&["slack", "status", "--json"], "slack"),
        (&["slack", "--help"], "slack"),
    ] {
        let output = oh_fx(args, &[("COLUMNS", "60")]);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stdout(&output), "", "{args:?}");
        assert_eq!(
            stderr(&output),
            format!(
                "oh-fx: unknown subcommand: {token}\n\n{}",
                render_top_level_help(80, ofx_upgrade::VERSION, HelpStyle::Plain)
            ),
            "{args:?}"
        );
    }
}

#[test]
fn commands_the_binary_cannot_run_yet_fail_with_one_message() {
    for (args, feature) in [
        (
            &["--context-limit", "mcp_description_bytes=1"][..],
            "--context-limit",
        ),
        (&["--add-dir", "/tmp/shared", "-c"], "--add-dir"),
        (&["--provider", "local"], "--provider"),
        (&["--sessions-v2"], "--sessions-v2"),
        (&["--sessions-v2", "resume", "last"], "--sessions-v2"),
        (&["login", "vercel"], "login"),
        (&["replay", "tape"], "replay"),
        (&["status"], "status"),
        (&["balance"], "credits"),
        (&["sessions"], "sessions"),
        (&["mcp", "list"], "mcp"),
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

fn with_settings(settings: Option<&str>, args: &[&str]) -> Output {
    with_settings_and_environment(settings, &[], args)
}

fn with_settings_and_environment(
    settings: Option<&str>,
    environment: &[(&str, &str)],
    args: &[&str],
) -> Output {
    let home = tempfile::tempdir().expect("create a temporary home");
    if let Some(settings) = settings {
        let config = home.path().join(".config/oh-fx");
        std::fs::create_dir_all(&config).expect("create the config directory");
        std::fs::write(config.join("settings.json"), settings).expect("write settings.json");
    }
    spawn(
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(args)
            .current_dir(home.path())
            .env_clear()
            .env("HOME", home.path())
            .env("OH_FX_AUTO_UPGRADE", "0")
            .envs(environment.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .wait_with_output()
    .expect("wait for oh-fx")
}

#[test]
fn a_skill_or_context_limit_diagnostic_keeps_the_saved_ask_mode() {
    for settings in [
        r#"{"permission_mode":"ask","skill_match_fuzzy":true}"#,
        r#"{"permission_mode":"ask","skill_symlink_authorities":"/nix/store"}"#,
        r#"{"permission_mode":"ask","context_limits":{"unknown_limit":10}}"#,
    ] {
        let output = with_settings(Some(settings), &["permissions", "--json"]);
        assert!(output.status.success(), "{settings}: {}", stderr(&output));
        assert!(
            stdout(&output).starts_with(r#"{"kind":"permissions","mode":"ask","#),
            "{settings}: {}",
            stdout(&output)
        );
    }
}

#[test]
fn permissions_reports_the_saved_mode_with_upstreams_text_and_json() {
    let text = |mode: &str| {
        format!(
            "[permissions] mode={mode}\n[permissions] configured rules: (none)\n[permissions] session grants: (none)\n"
        )
    };
    let json = |mode: &str| {
        format!(
            "{{\"kind\":\"permissions\",\"mode\":\"{mode}\",\"grant_count\":0,\"grant_scope\":\"session\",\"runtime_grants_available\":false,\"rules_scope\":\"persistent_config\",\"rules\":[],\"grants\":[]}}\n"
        )
    };
    for (settings, shown, label) in [
        (None, "auto", "auto"),
        (Some(r#"{"permission_mode":"ask"}"#), "ask", "ask"),
        (
            Some(r#"{"permission_mode":"full-access"}"#),
            "full access",
            "yolo",
        ),
    ] {
        let output = with_settings(settings, &["permissions"]);
        assert!(output.status.success(), "{settings:?}: {}", stderr(&output));
        assert_eq!(stdout(&output), text(shown), "{settings:?}");
        assert_eq!(stderr(&output), "", "{settings:?}");
        let output = with_settings(settings, &["permissions", "--json"]);
        assert!(output.status.success(), "{settings:?}: {}", stderr(&output));
        assert_eq!(stdout(&output), json(label), "{settings:?}");
    }
    for (variable, shown, label) in [
        ("yolo", "full access", "yolo"),
        ("Full Access", "full access", "yolo"),
        ("auto", "auto", "auto"),
        ("sometimes", "ask", "ask"),
    ] {
        let settings = Some(r#"{"permission_mode":"ask"}"#);
        let environment = [("OH_FX_PERMISSION_MODE", variable)];
        let output = with_settings_and_environment(settings, &environment, &["permissions"]);
        assert_eq!(stdout(&output), text(shown), "{variable}");
        let output =
            with_settings_and_environment(settings, &environment, &["permissions", "--json"]);
        assert_eq!(stdout(&output), json(label), "{variable}");
    }
    let output = with_settings(Some("{"), &["permissions", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "oh-fx: InvalidProfileConfiguration\n");
    let output = with_settings(None, &["permissions", "--all"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "usage: oh-fx permissions [--json]\n");
}

#[test]
fn interactive_launches_that_select_v2_sessions_are_not_available_yet() {
    for value in ["1", "TRUE"] {
        let output = oh_fx(&["-c"], &[("OH_FX_SESSIONS_V2", value)]);
        assert_eq!(output.status.code(), Some(1), "{value}");
        assert_eq!(
            stderr(&output),
            "oh-fx: OH_FX_SESSIONS_V2 is not available yet\n",
            "{value}"
        );
    }
    let output = oh_fx(&[] as &[&str], &[("OH_FX_SESSIONS_V2", "0")]);
    assert_eq!(
        stderr(&output),
        "oh-fx requires an interactive terminal (TTY).\n"
    );
}

#[test]
fn interactive_and_resume_launches_need_a_terminal() {
    for args in [
        &[][..],
        &["--model", "x", "--fast"],
        &["-c"],
        &["-r"],
        &["--resume-abc"],
        &["session", "resume", "last"],
    ] {
        let output = oh_fx(args, &[]);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stdout(&output), "", "{args:?}");
        assert_eq!(
            stderr(&output),
            "oh-fx requires an interactive terminal (TTY).\n",
            "{args:?}"
        );
    }
}

#[test]
fn json_requests_for_commands_the_binary_cannot_run_yet_print_the_failure_envelope() {
    for (args, kind) in [
        (&["status", "--json"][..], "status"),
        (&["models", "--json"], "models"),
        (&["doctor", "--json"], "doctor"),
        (&["balance", "--json"], "credits"),
        (&["usage", "--json"], "usage"),
        (&["sessions", "--json"], "sessions"),
        (&["session", "last", "--json"], "session"),
        (&["session", "migrate", "x", "--json"], "session"),
        (&["session", "recover", "x", "--json"], "session"),
        (&["workspace", "--json"], "workspace"),
        (&["replay", "tape", "--json"], "replay"),
    ] {
        let output = oh_fx(args, &[]);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(
            stderr(&output),
            format!("oh-fx: {kind} is not available yet\n"),
            "{args:?}"
        );
        assert_eq!(
            stdout(&output),
            format!(
                "{{\"kind\":\"{kind}\",\"error\":\"{kind} is not available yet\",\"code\":\"NotAvailableYet\"}}\n"
            ),
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
fn invalid_auth_modes_fail_every_command_except_top_level_help() {
    let invalid = [("OH_FX_AUTH_MODE", "bogus")];
    for args in [
        &["status"][..],
        &["--version"],
        &["ask", "--help"],
        &["status", "--bogus"],
        &["bogus"],
        &["--context-limit", "mcp_description_bytes=1", "help"],
        &[],
    ] {
        let output = oh_fx(args, &invalid);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stdout(&output), "", "{args:?}");
        assert_eq!(
            stderr(&output),
            "oh-fx: OH_FX_AUTH_MODE must be local or host-managed\n",
            "{args:?}"
        );
    }
    let empty = oh_fx(&["--version"], &[("OH_FX_AUTH_MODE", "")]);
    assert_eq!(empty.status.code(), Some(1));
    for args in [&["--help"][..], &["-h"], &["help", "--json"]] {
        let output = oh_fx(args, &invalid);
        assert!(output.status.success(), "{args:?}");
        assert!(stdout(&output).starts_with("oh-fx v"), "{args:?}");
    }
    for mode in ["local", "host-managed"] {
        let output = oh_fx(&["--version"], &[("OH_FX_AUTH_MODE", mode)]);
        assert!(output.status.success(), "{mode}");
    }
}

#[test]
#[cfg(target_os = "linux")]
fn full_disk_writes_follow_each_upstream_path() {
    for (args, expected) in [
        (&["--help"][..], ""),
        (&["help", "--json"], ""),
        (
            &["--context-limit", "mcp_description_bytes=1", "help"],
            "oh-fx: WriteFailed\n",
        ),
        (&["--version"], "oh-fx: WriteFailed\n"),
        (&["status", "--help"], "oh-fx: WriteFailed\n"),
        (&["sessions", "--help"], "oh-fx: WriteFailed\n"),
        (&["permissions"], "oh-fx: WriteFailed\n"),
        (&["status", "--json", "--bogus"], "oh-fx: WriteFailed\n"),
        (&["sessions", "--json", "--bogus"], "oh-fx: WriteFailed\n"),
        (
            &["session", "last", "--json"],
            "oh-fx: session is not available yet\noh-fx: WriteFailed\n",
        ),
        (&["replay", "--json"], ""),
    ] {
        let output = into_full_device(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stderr(&output), expected, "{args:?}");
    }
}

#[test]
fn read_only_stdout_is_treated_like_a_closed_one() {
    for args in [&["--help"][..], &["--version"], &["status", "--help"]] {
        let read_only = File::open("/dev/null").expect("open /dev/null");
        let output = run(args, &[], Stdio::from(read_only));
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert_eq!(stderr(&output), "", "{args:?}");
    }
}

#[test]
fn closed_pipes_follow_each_upstream_path() {
    for args in [
        &["--help"][..],
        &["--version"],
        &["sessions", "--help"],
        &["sessions", "--json", "--bogus"],
        &["replay", "--json"],
        &["permissions", "--json"],
    ] {
        let output = into_closed_pipe(args);
        assert_eq!(output.status.signal(), Some(SIGPIPE), "{args:?}");
    }
    for args in [
        &["status", "--help"][..],
        &["status", "--json", "--bogus"],
        &["models", "--json"],
    ] {
        let output = into_closed_pipe(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(
            stderr(&output).ends_with("oh-fx: WriteFailed\n"),
            "{args:?}"
        );
    }
}

#[test]
fn upgrade_rejects_unknown_arguments_like_upstream() {
    for (args, expected) in [
        (
            &["upgrade", "--channel", "dev"][..],
            "usage: oh-fx upgrade [--json]\n",
        ),
        (
            &["upgrade", "--json", "--json"],
            "oh-fx: InvalidUpgradeArgs\n",
        ),
        (
            &["upgrade", "--background", "x"],
            "usage: oh-fx upgrade [--json]\n",
        ),
    ] {
        let output = oh_fx(args, &[]);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stderr(&output), expected, "{args:?}");
    }
}

#[test]
fn launch_modifiers_before_help_select_the_plain_layout() {
    let output = oh_fx(
        &["--context-limit", "mcp_description_bytes=1", "help"],
        &[("COLUMNS", "60")],
    );
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        render_top_level_help(80, ofx_upgrade::VERSION, HelpStyle::Plain)
    );
}

#[test]
fn commands_that_keep_no_sessions_accept_and_ignore_sessions_v2() {
    let output = oh_fx(&["--sessions-v2", "--version"], &[]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), format!("{}\n", ofx_upgrade::VERSION));
    let output = oh_fx(&["--sessions-v2", "help"], &[("COLUMNS", "60")]);
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        render_top_level_help(80, ofx_upgrade::VERSION, HelpStyle::Plain)
    );
    let output = oh_fx(&["--sessions-v2", "ask", "--help"], &[]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), render_command_help(TopLevelKind::Ask));
    let output = oh_fx(&["--sessions-v2", "upgrade", "--channel", "dev"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "usage: oh-fx upgrade [--json]\n");
    let output = oh_fx(&["--sessions-v2", "status"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "oh-fx: status is not available yet\n");
}

#[test]
fn invalid_command_arguments_fail_before_the_availability_check() {
    let output = oh_fx(&["status", "--wat"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "usage: oh-fx status [--json]\n");
    let output = oh_fx(&["status", "--json", "--wat"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout(&output),
        "{\"kind\":\"status\",\"error\":\"invalid arguments\",\"code\":\"InvalidLocalSurfaceArgs\"}\n"
    );
    let output = oh_fx(&["--model", "x", "ask", "hi"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "oh-fx: --provider, --model, --effort, --fast, --ultrafast, --provider-order, and --provider-strict apply to interactive sessions; for one-shot runs pass model flags after `oh-fx ask`\n"
    );
}

#[test]
fn launch_modifiers_that_ask_cannot_honor_yet_fail_with_the_shared_message() {
    for (args, feature) in [
        (
            &["--context-limit", "mcp_description_bytes=1", "ask", "hi"][..],
            "--context-limit",
        ),
        (&["--add-dir", "/tmp", "ask", "hi"], "--add-dir"),
        (&["--sessions-v2", "ask", "hi"], "--sessions-v2"),
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
    let output = oh_fx(&["--add-dir=/tmp", "ask", "--json", "hi"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "oh-fx: --add-dir is not available yet\n");
    assert_eq!(
        stdout(&output),
        "{\"output\":\"\",\"final_output\":\"\",\"exit_code\":1,\"model\":\"\",\"resolved_provider\":null,\"session_id\":\"\",\"steps\":0,\"tool_calls\":[],\"usage\":{\"input_tokens\":null,\"output_tokens\":null},\"error\":\"NotAvailableYet\"}\n"
    );
}

#[cfg(target_os = "linux")]
fn spawn_into(home: &Path, args: &[&str], environment: &[(&str, &str)], stdout: OwnedFd) -> Child {
    spawn(
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(args)
            .current_dir(home)
            .env_clear()
            .env("HOME", home)
            .env("OH_FX_AUTO_UPGRADE", "0")
            .envs(environment.iter().copied())
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::null()),
    )
}

#[cfg(target_os = "linux")]
fn on_terminal(args: &[&str], environment: &[(&str, &str)], columns: u16) -> Vec<u8> {
    let controller = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)
        .expect("open a pseudoterminal");
    grantpt(&controller).expect("grant the pseudoterminal");
    unlockpt(&controller).expect("unlock the pseudoterminal");
    let name = ptsname(&controller, Vec::new()).expect("name the pseudoterminal");
    let terminal = rustix::fs::open(
        name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open the terminal side");
    let mut modes = tcgetattr(&terminal).expect("read the terminal modes");
    modes.output_modes.remove(OutputModes::OPOST);
    tcsetattr(&terminal, OptionalActions::Now, &modes).expect("keep newlines raw");
    let size = Winsize {
        ws_row: 24,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    tcsetwinsize(&terminal, size).expect("size the terminal");
    let home = tempfile::tempdir().expect("create a temporary home");
    let mut child = spawn_into(home.path(), args, environment, terminal);
    let mut output = Vec::new();
    if let Err(error) = File::from(controller).read_to_end(&mut output) {
        assert_eq!(
            error.raw_os_error(),
            Some(Errno::IO.raw_os_error()),
            "{error}"
        );
    }
    assert!(child.wait().expect("wait for oh-fx").success());
    output
}

#[cfg(target_os = "linux")]
fn stdout_records(args: &[&str]) -> Vec<Vec<u8>> {
    let (reader, writer) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .expect("create a packet socket pair");
    let home = tempfile::tempdir().expect("create a temporary home");
    let mut child = spawn_into(home.path(), args, &[], writer);
    let mut records = Vec::new();
    let mut buffer = vec![0; 1 << 16];
    loop {
        match rustix::io::read(&reader, &mut buffer) {
            Ok(0) => break,
            Ok(count) => records.push(buffer[..count].to_vec()),
            Err(Errno::INTR) => {}
            Err(error) => panic!("read stdout: {error}"),
        }
    }
    assert!(child.wait().expect("wait for oh-fx").success());
    records
}

#[test]
#[cfg(target_os = "linux")]
fn help_and_version_reach_stdout_in_one_write() {
    assert_eq!(
        stdout_records(&["--help"]),
        [render_top_level_help(80, ofx_upgrade::VERSION, HelpStyle::Plain).into_bytes()]
    );
    assert_eq!(
        stdout_records(&["--version"]),
        [format!("{}\n", ofx_upgrade::VERSION).into_bytes()]
    );
}

#[test]
#[cfg(target_os = "linux")]
fn top_level_help_on_a_terminal_follows_its_width_and_color_settings() {
    for (environment, columns, width, style) in [
        (&[][..], 60, 60, HelpStyle::Ansi),
        (&[("NO_COLOR", "")], 60, 60, HelpStyle::Plain),
        (&[("TERM", "dumb")], 60, 60, HelpStyle::Plain),
        (&[("COLUMNS", "120")], 60, 60, HelpStyle::Ansi),
        (&[("COLUMNS", "120")], 0, 120, HelpStyle::Ansi),
    ] {
        assert_eq!(
            on_terminal(&["--help"], environment, columns),
            render_top_level_help(width, ofx_upgrade::VERSION, style).into_bytes(),
            "{environment:?} {columns}"
        );
    }
}

fn working_directory_identity() -> String {
    let metadata = std::fs::metadata(env::temp_dir()).expect("read the temporary directory");
    format!("{}:{}", metadata.dev(), metadata.ino())
}

fn run_released_session(script: &str) -> Output {
    run_released_session_in(&working_directory_identity(), script)
}

fn run_released_session_in(cwd_identity: &str, script: &str) -> Output {
    let mut supervisor = spawn(
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args([
                "__oh_fx_foreground_session__",
                "none",
                cwd_identity,
                "/bin/sh",
                "-c",
                script,
            ])
            .current_dir(env::temp_dir())
            .env_clear()
            .env("PATH", env::var_os("PATH").unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    let mut input = supervisor.stdin.take().expect("supervisor input");
    let mut release = vec![b'a'; 32];
    release.push(0x06);
    io::Write::write_all(&mut input, &release).expect("release the command");
    let output = supervisor
        .wait_with_output()
        .expect("wait for the supervisor");
    drop(input);
    output
}

fn status_frame(status: &str) -> Vec<u8> {
    let mut frame = b"\0OH_FX_FOREGROUND_STATUS:".to_vec();
    frame.extend_from_slice(&[b'a'; 32]);
    frame.push(b':');
    frame.extend_from_slice(status.as_bytes());
    frame.push(b'\n');
    frame
}

#[test]
fn the_hidden_session_supervisor_runs_a_released_command_without_a_terminal() {
    let output = run_released_session("printf out; printf err >&2; exit 3");
    assert_eq!(output.status.signal(), Some(SIGKILL));
    assert_eq!(stdout(&output), "out");
    let mut expected = b"\x1eerr".to_vec();
    expected.extend_from_slice(&status_frame("exit:3"));
    assert_eq!(output.stderr, expected);
}

#[test]
fn the_hidden_session_supervisor_kills_leftover_jobs_when_the_command_exits() {
    let output = run_released_session("(sleep 5; printf survived) & exit 0");
    assert_eq!(output.status.signal(), Some(SIGKILL));
    assert_eq!(stdout(&output), "");
    let mut expected = b"\x1e".to_vec();
    expected.extend_from_slice(&status_frame("exit:0"));
    assert_eq!(output.stderr, expected);
}

#[test]
fn the_hidden_session_supervisor_launches_nothing_outside_the_directory_it_was_given() {
    let output = run_released_session_in("0:0", "printf ran");
    assert_eq!(output.status.code(), Some(125));
    assert_eq!(stdout(&output), "");
    let mut expected = b"\x1e\0OH_FX_FOREGROUND_EXEC_FAILED:".to_vec();
    expected.extend_from_slice(&[b'a'; 32]);
    expected.extend_from_slice(b":CommandAuthorityContextMismatch\n");
    assert_eq!(output.stderr, expected);
}

#[test]
fn the_hidden_session_supervisor_runs_nothing_without_a_release() {
    let output = spawn(
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args([
                "__oh_fx_foreground_session__",
                "none",
                &working_directory_identity(),
                "/bin/sh",
                "-c",
                "printf ran",
            ])
            .current_dir(env::temp_dir())
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .wait_with_output()
    .expect("run the supervisor");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(output.stderr, b"\x1e");
}
