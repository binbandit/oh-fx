use std::fmt::Write;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use ofx_testkit::{FakeServer, PtySession, Reply, chat_text_events, chat_tool_call_events};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(15);
const APPROVAL_ARMING: Duration = Duration::from_millis(700);
const CANCELLATION: &str = "■ Cancelled · What can oh-fx do differently?";
const SIGTERM: i32 = 15;
const SIGNAL_RESTORE: &[u8] = b"\x1b[<u\x1b[>4;0m\x1b[?2004l\x1b[?2031l\x1b[?25h";
const SHIFT_TAB: &[u8] = b"\x1b[Z";
const FULL_ACCESS_WARNING: &str = "Full access enabled: oh-fx permission checks disabled";
const PERMISSIONS_USAGE: &str = "usage: /permissions [ask|auto|full-access|reset]";

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
}

impl Home {
    fn with_settings(settings: &Value) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = directory.path().to_owned();
        let config = root.join("config/oh-fx");
        let workspace = root.join("workspace");
        fs::create_dir_all(&config).expect("create the config directory");
        fs::create_dir_all(&workspace).expect("create the workspace");
        fs::write(config.join("settings.json"), settings.to_string()).expect("write settings.json");
        Self {
            _directory: directory,
            root,
            workspace,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .current_dir(&self.workspace)
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .process_group(0);
        command
    }

    fn shell(&self, rows: u16, cols: u16) -> PtySession {
        self.shell_with(&[], rows, cols, "auto · model-a")
    }

    fn shell_with(&self, args: &[&str], rows: u16, cols: u16, hint: &str) -> PtySession {
        let mut command = self.command();
        command.args(args);
        let session = PtySession::spawn(command, rows, cols).expect("spawn oh-fx in a pty");
        session
            .wait_for(WAIT, |screen| {
                screen.contains("Run /help for commands") && screen.contains(hint)
            })
            .expect("the shell starts");
        session
    }
}

fn settings(base_url: &str) -> Value {
    json!({
        "provider": "local",
        "providers": {
            "local": {
                "protocol": "openai-chat-completions",
                "base_url": base_url,
                "auth": {"type": "none"},
                "models": ["model-a", "vendor/model-b"]
            }
        }
    })
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn wait(session: &PtySession, needle: &str) -> String {
    session
        .wait_for(WAIT, |screen| screen.contains(needle))
        .unwrap_or_else(|screen| panic!("expected {needle:?} on screen:\n{screen}"))
}

#[test]
fn the_picker_alias_and_launch_models_open_the_shell() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Chosen."]))]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let mut session = home.shell_with(&["-r"], 24, 80, "auto · model-a");
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
    let mut session = home.shell_with(
        &["--model", "vendor/model-b", "--fast"],
        24,
        80,
        "auto · model-b",
    );
    session.send(b"go\r");
    wait(&session, "Chosen.");
    assert_eq!(server.requests()[0].json()["model"], "vendor/model-b");
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

#[test]
fn reads_outside_the_workspace_wait_for_approval_in_the_footer() {
    let read = chat_tool_call_events("call-1", "read_file", r#"{"path":"../notes.txt"}"#);
    let server = FakeServer::start([
        Reply::sse(&read),
        Reply::sse(&chat_text_events(&["Read it."])),
        Reply::sse(&read),
        Reply::sse(&chat_text_events(&["Read it again."])),
    ]);
    let home = Home::with_settings(&settings(&server.base_url()));
    fs::write(home.root.join("notes.txt"), "outside notes\n").expect("write the outside file");
    let mut session = home.shell(30, 100);
    session.send(b"read the notes\r");
    let screen = wait(&session, "Permission needed · Choose one");
    for line in [
        "read_file ",
        "notes.txt",
        "❯ 1. Yes",
        "2. Yes, and allow reads under ",
        " for this session",
        "3. No",
    ] {
        assert!(screen.contains(line), "{line}\n{screen}");
    }
    thread::sleep(APPROVAL_ARMING);
    session.send(b"2");
    wait(&session, "Read it.");
    session.send(b"again\r");
    wait(&session, "Read it again.");
    let screen = session.screen();
    assert!(!screen.contains("Permission needed"), "{screen}");
    let requests = server.requests();
    for request in [&requests[1], &requests[3]] {
        let body = request.json();
        let tool = body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .rev()
            .find(|message| message["role"] == "tool")
            .expect("a tool result")
            .clone();
        assert!(
            tool["content"]
                .as_str()
                .expect("tool content")
                .contains("outside notes"),
            "{tool}"
        );
    }
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

fn last_tool_result(request: &ofx_testkit::RecordedRequest) -> String {
    let body = request.json();
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("a tool result")
        .to_owned()
}

fn settings_file(home: &Home) -> PathBuf {
    home.root.join("config/oh-fx/settings.json")
}

fn saved_settings(home: &Home) -> Value {
    let bytes = fs::read(settings_file(home)).expect("read settings.json");
    serde_json::from_slice(&bytes).expect("settings.json holds JSON")
}

fn wait_saved(home: &Home, key: &str, value: &Value) {
    let deadline = Instant::now() + WAIT;
    while saved_settings(home)[key] != *value {
        assert!(
            Instant::now() < deadline,
            "{key} never became {value}: {}",
            saved_settings(home)
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn shift_tab_cycles_the_permission_mode_and_the_next_tool_call_follows_it() {
    let read = chat_tool_call_events("call-1", "read_file", r#"{"path":"../notes.txt"}"#);
    let server = FakeServer::start([
        Reply::sse(&read),
        Reply::sse(&chat_text_events(&["Read it."])),
    ]);
    let home = Home::with_settings(&settings(&server.base_url()));
    fs::write(home.root.join("notes.txt"), "outside notes\n").expect("write the outside file");
    let mut session = home.shell(30, 100);
    session.send(SHIFT_TAB);
    let screen = wait(&session, FULL_ACCESS_WARNING);
    assert!(screen.contains("full access · model-a"), "{screen}");
    wait_saved(&home, "permission_mode", &json!("yolo"));
    wait_saved(&home, "yolo_acknowledged", &json!(true));
    session.send(b"read the notes\r");
    wait(&session, "Read it.");
    assert!(!session.screen().contains("Permission needed"));
    assert!(last_tool_result(&server.requests()[1]).contains("outside notes"));
    session.send(SHIFT_TAB);
    wait(&session, "ask · model-a");
    wait_saved(&home, "permission_mode", &json!("ask"));
    session.send(b"/permissions\r");
    let screen = wait(&session, "saved-session permission rules: none");
    for line in [
        "permissions: mode=ask",
        "configured rules: (none)",
        "session grants: (none)",
        PERMISSIONS_USAGE,
        "/permissions remember <allow|deny> <tool-name> <arguments-json>",
        "/permissions revoke <rule-id>",
    ] {
        assert!(screen.contains(line), "{line}\n{screen}");
    }
    session.send(b"/permissions sometimes\r");
    session
        .wait_for(WAIT, |screen| {
            screen.matches(PERMISSIONS_USAGE).count() == 2
        })
        .unwrap_or_else(|screen| panic!("expected a second usage notice:\n{screen}"));
    session.send(b"/permissions revoke 1\r");
    wait(
        &session,
        "permissions: saved-session permission rules require an active saved session",
    );
    assert_eq!(saved_settings(&home)["permission_mode"], "ask");
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
    let mut session = home.shell_with(&[], 30, 100, "ask · model-a");
    session.send(b"/permissions full-access\r");
    wait(&session, "permissions: mode set to full access");
    let screen = wait(&session, "full access · model-a");
    assert!(!screen.contains(FULL_ACCESS_WARNING), "{screen}");
    wait_saved(&home, "permission_mode", &json!("yolo"));
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

#[test]
fn the_permission_mode_variable_picks_the_starting_mode_and_shift_tab_cycles_on_from_it() {
    let mut settings = settings("http://127.0.0.1:9");
    settings["permission_mode"] = json!("auto");
    let home = Home::with_settings(&settings);
    let mut command = home.command();
    command.env("OH_FX_PERMISSION_MODE", "full-access");
    let mut session = PtySession::spawn(command, 30, 100).expect("spawn oh-fx in a pty");
    let screen = wait(&session, FULL_ACCESS_WARNING);
    assert!(screen.contains("full access · model-a"), "{screen}");
    wait_saved(&home, "yolo_acknowledged", &json!(true));
    assert_eq!(saved_settings(&home)["permission_mode"], "auto");
    session.send(SHIFT_TAB);
    wait(&session, "ask · model-a");
    wait_saved(&home, "permission_mode", &json!("ask"));
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

#[test]
fn a_mode_that_cannot_be_saved_still_applies_and_says_so() {
    let home = Home::with_settings(&settings("http://127.0.0.1:9"));
    let unsaveable = fs::read_to_string(settings_file(&home))
        .expect("read settings.json")
        .replacen('{', "{\"note\":123456789012345678901234567890,", 1);
    fs::write(settings_file(&home), &unsaveable).expect("write settings.json");
    let mut session = home.shell(30, 120);
    session.send(SHIFT_TAB);
    let screen = wait(
        &session,
        "full-access-acknowledgment: active for this process but not saved to user settings (SettingsNumberNotPreserved)",
    );
    for line in [
        "permission-mode: active for this process but not saved to user settings (SettingsNumberNotPreserved)",
        "full access · model-a",
        FULL_ACCESS_WARNING,
    ] {
        assert!(screen.contains(line), "{line}\n{screen}");
    }
    assert_eq!(
        fs::read_to_string(settings_file(&home)).expect("read settings.json"),
        unsaveable
    );
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

#[test]
fn ask_mode_file_changes_run_once_approved_and_project_instructions_reach_the_model() {
    let write = |id: &str, content: &str| {
        chat_tool_call_events(
            id,
            "write_file",
            &json!({"path": "notes.md", "content": content}).to_string(),
        )
    };
    let server = FakeServer::start([
        Reply::sse(&write("call-1", "approved\n")),
        Reply::sse(&chat_text_events(&["Wrote it."])),
        Reply::sse(&write("call-2", "denied\n")),
        Reply::sse(&chat_text_events(&["Left it alone."])),
    ]);
    let mut settings = settings(&server.base_url());
    settings["permission_mode"] = json!("ask");
    let home = Home::with_settings(&settings);
    fs::write(home.workspace.join("AGENTS.md"), "WORKSPACE RULE\n").expect("write AGENTS.md");
    let mut session = home.shell_with(&[], 30, 100, "ask · model-a");
    session.send(b"write the notes\r");
    let screen = wait(&session, "Permission needed · Choose one");
    assert!(screen.contains("notes.md"), "{screen}");
    thread::sleep(APPROVAL_ARMING);
    session.send(b"1");
    wait(&session, "Wrote it.");
    assert_eq!(
        fs::read_to_string(home.workspace.join("notes.md")).expect("read notes.md"),
        "approved\n"
    );
    session.send(b"rewrite them\r");
    wait(&session, "Permission needed · Choose one");
    session.send(b"3");
    wait(&session, "Left it alone.");
    assert_eq!(
        fs::read_to_string(home.workspace.join("notes.md")).expect("read notes.md"),
        "approved\n"
    );
    let requests = server.requests();
    assert!(
        requests[0].body_text().contains("WORKSPACE RULE"),
        "{}",
        requests[0].body_text()
    );
    assert!(last_tool_result(&requests[3]).contains("tool_permission_denied"));
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

fn output_after(session: &PtySession, start: usize, needle: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + WAIT;
    loop {
        let output = session.output()[start..].to_vec();
        if output.windows(needle.len()).any(|window| window == needle) {
            return output;
        }
        assert!(
            Instant::now() < deadline,
            "expected {:?} in {:?}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(&output)
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn sigterm_restores_the_terminal_and_ends_the_shell_with_the_signal() {
    let server = FakeServer::start([]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let mut session = home.shell(24, 80);
    assert!(!session.cooked().unwrap());
    let started = session.output().len();
    session.terminate().unwrap();
    let status = session.wait_exit(WAIT).expect("SIGTERM ends the shell");
    assert_eq!(status.signal(), Some(SIGTERM));
    assert!(session.cooked().unwrap());
    output_after(&session, started, SIGNAL_RESTORE);
}

#[test]
fn sigterm_ends_a_frame_write_blocked_on_a_stalled_terminal() {
    let mut reply = String::new();
    for line in 0..5_000 {
        writeln!(
            reply,
            "line {line} of a reply larger than the terminal can buffer"
        )
        .unwrap();
    }
    let server = FakeServer::start([Reply::sse(&chat_text_events(&[&reply]))]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let mut session = home.shell(24, 80);
    session.stall_output_after(b"line 0 of a reply").unwrap();
    session.send(b"go\r");
    assert!(
        session.wait_for_pending_output(WAIT),
        "the reply never reached the stalled terminal"
    );
    session.terminate().unwrap();
    let status = session
        .wait_exit(WAIT)
        .expect("SIGTERM ends a shell blocked writing a frame");
    assert_eq!(status.signal(), Some(SIGTERM));
    assert!(session.cooked().unwrap());
    assert!(session.drain_output(WAIT), "the terminal never closed");
    assert_eq!(count(&session.output(), b"line 4999 of a reply"), 0);
}

#[test]
fn a_terminal_shorter_than_five_rows_is_refused_as_upstream_refuses_it() {
    let server = FakeServer::start([]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let mut session = PtySession::spawn(home.command(), 4, 80).expect("spawn oh-fx in a pty");
    let status = session
        .wait_exit(WAIT)
        .expect("a short terminal ends the shell");
    assert!(status.success(), "{status:?}");
    assert!(session.drain_output(WAIT), "the terminal never closed");
    let output = String::from_utf8_lossy(&session.output()).into_owned();
    assert_eq!(
        count(
            output.as_bytes(),
            b"oh-fx needs at least 5 terminal rows.\r\n"
        ),
        1,
        "{output:?}"
    );
    assert!(!output.contains("oh-fx: oh-fx"), "{output:?}");
    assert!(session.cooked().unwrap());
}

#[test]
fn an_exit_waits_for_a_stalled_terminal_and_sigterm_still_restores_it() {
    let server = FakeServer::start([]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let mut session = PtySession::spawn(home.command(), 24, 80).expect("spawn oh-fx in a pty");
    session
        .stall_output_after(b"Run /help for commands")
        .unwrap();
    let deadline = Instant::now() + WAIT;
    while count(&session.output(), b"Run /help for commands") == 0 {
        assert!(Instant::now() < deadline, "the shell never started");
        thread::sleep(Duration::from_millis(20));
    }
    session.fill_stalled_output().unwrap();
    session.send(b"\x04");
    assert!(
        session.wait_exit(Duration::from_millis(500)).is_none(),
        "the exit gave up on a terminal that only stopped reading"
    );
    session.terminate().unwrap();
    let status = session
        .wait_exit(WAIT)
        .expect("SIGTERM ends an exit blocked on the terminal");
    assert_eq!(status.signal(), Some(SIGTERM));
    assert!(session.cooked().unwrap());
}

#[test]
fn a_prompt_streams_a_reply_and_a_second_ctrl_c_exits() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&[
        "Hello from the fake ",
        "gateway.\n\nSecond **paragraph**.",
    ]))]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let mut session = home.shell(24, 80);
    session.send(b"hello there");
    wait(&session, "┃ hello there");
    session.send(b"\r");
    let screen = wait(&session, "s (↑4 ↓3)");
    let rows = session.screen_rows();
    assert!(rows[0].starts_with("oh-fx v"));
    assert!(rows[0].ends_with(" · Run /help for commands"));
    assert_eq!(rows[2], "┃ hello there");
    assert_eq!(rows[4], "  Hello from the fake gateway.");
    assert_eq!(rows[6], "  Second paragraph.");
    assert!(rows[8].starts_with("  0s (↑4 ↓3)"), "{screen}");
    assert_eq!(rows[10], "┃");
    assert_eq!(rows[12], "auto · model-a");
    let request = &server.requests()[0];
    assert_eq!(request.json()["model"], "model-a");
    session.send(b"\x03");
    wait(&session, "press ctrl+c again to exit");
    session.send(b"\x03");
    let status = session.wait_exit(WAIT).expect("oh-fx exits");
    assert!(status.success());
    assert!(session.drain_output(WAIT), "the terminal never closed");
    let rows = session.screen_rows();
    assert_eq!(rows[6], "  Second paragraph.");
    assert!(!session.screen().contains("auto · model-a"));
    let output = String::from_utf8_lossy(&session.output()).into_owned();
    assert!(output.contains("\x1b[?2004h"));
    assert!(output.ends_with("\x1b[?25h\r\n"), "{output:?}");
}

#[test]
fn slash_commands_switch_models_show_help_and_exit() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Switched reply."]))]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let mut session = home.shell(30, 100);
    session.send(b"/model\r");
    wait(&session, "* model: model-a");
    wait(&session, "available: model-a, vendor/model-b");
    session.send(b"/model model-b\r");
    wait(&session, "* Switched to vendor/model-b");
    wait(&session, "auto · model-b");
    session.send(b"/bogus\r");
    wait(&session, "✗ command: Unknown command. Try /help.");
    session.send(b"/help\r");
    let screen = wait(&session, "Commands 14");
    assert!(screen.contains("  /permissions    choose what oh-fx is allowed to do"));
    assert!(screen.contains("  /skills         browse and manage skills"));
    assert!(screen.contains("  /quit           exit the interactive shell"));
    assert!(screen.contains("  /reset          reset the current session context"));
    assert!(screen.contains("  /new            start a fresh session"));
    assert!(screen.contains("Commands 14  [All]  General  Session  Account  Model"));
    session.send(b"/version\r");
    wait(&session, &format!("* version: {}", ofx_upgrade::VERSION));
    session.send(b"/stats\r");
    wait(&session, "* stats: ansi_bytes=");
    session.send(b"/copy\r");
    wait(&session, "* clipboard: No assistant reply to copy.");
    session.send(b"/fast\r");
    wait(
        &session,
        "* fast: This model does not come with a fast mode.",
    );
    session.send(b"/status\r");
    let screen = wait(&session, "* status: model=vendor/model-b");
    assert!(screen.contains("permission_mode=auto"), "{screen}");
    assert!(screen.contains("history_turns=0"), "{screen}");
    let saved = saved_settings(&home);
    assert_eq!(saved["models"]["local"], "vendor/model-b");
    assert_eq!(saved["fast_mode"], false);
    session.send(b"/compact\r");
    wait(&session, "No context to compact.");
    session.send(b"go\r");
    wait(&session, "Switched reply.");
    assert_eq!(server.requests()[0].json()["model"], "vendor/model-b");
    session.send(b"/exit\r");
    assert!(session.wait_exit(WAIT).expect("oh-fx exits").success());
}

#[test]
fn a_saved_fast_choice_follows_only_the_model_it_was_saved_with() {
    let server = FakeServer::start([]);
    let mut saved = settings(&server.base_url());
    saved["models"] = json!({"local": "model-a"});
    saved["fast_mode"] = json!(true);
    saved["fast_mode_model_bound"] = json!(true);
    let home = Home::with_settings(&saved);
    for (args, hint, fast) in [
        (&[][..], "auto · model-a", "* fast: off"),
        (&["--model", "model-a"], "auto · model-a", "* fast: off"),
        (
            &["--model", "vendor/model-b"],
            "auto · model-b",
            "* fast: This model does not come with a fast mode.",
        ),
        (
            &["--model", "vendor/model-b", "--fast"],
            "auto · model-b",
            "* fast: off",
        ),
    ] {
        let mut session = home.shell_with(args, 24, 100, hint);
        session.send(b"/fast\r");
        wait(&session, fast);
        session.send(b"\x04");
        assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
        fs::write(settings_file(&home), saved.to_string()).expect("restore settings.json");
    }
}

#[test]
fn a_skill_chosen_in_the_skills_menu_is_loaded_for_the_prompt() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Reviewed."]))]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let skill = home.workspace.join(".oh-fx/skills/review");
    fs::create_dir_all(&skill).expect("create the skill");
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: review\ndescription: Review a diff\n---\nCheck every hunk.\n",
    )
    .expect("write the skill");
    let mut session = home.shell(30, 100);
    session.send(b"/skills\r");
    let screen = wait(&session, "Skills 1  [All]  oh-fx");
    assert!(screen.contains("  review    oh-fx · Workspace"), "{screen}");
    assert!(screen.contains("↑↓ navigate     tab source     enter use     esc close"));
    session.send(b"\r");
    wait(&session, "auto · model-a");
    session.send(b"the change\r");
    wait(&session, "Reviewed.");
    let request = server.requests()[0].json();
    let system: String = request["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] == "system")
        .filter_map(|message| message["content"].as_str())
        .collect();
    assert!(
        system.contains("<skill_content name=\"review\"") && system.contains("Check every hunk."),
        "{system}"
    );
    let user = request["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .expect("the prompt");
    assert_eq!(user["content"], "$review the change");
    session.send(b"/exit\r");
    assert!(session.wait_exit(WAIT).expect("oh-fx exits").success());
}

#[test]
fn ctrl_c_cancels_a_streaming_turn_and_clear_starts_over() {
    let held =
        Reply::held_sse(&chat_text_events(&["First line.\nSecond line.\n", "still going"])[..3]);
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["One."])),
        held,
        Reply::sse(&chat_text_events(&["Fresh."])),
    ]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let mut session = home.shell(24, 80);
    session.send(b"first\r");
    wait(&session, "One.");
    session.send(b"slow\r");
    wait(&session, "First line.");
    wait(&session, "Generating");
    session.send(b"\x03");
    let screen = wait(&session, CANCELLATION);
    assert!(screen.contains("press ctrl+c again to exit"));
    assert!(!screen.contains("still going"));
    session.send(b"\x1b");
    session
        .wait_for(WAIT, |screen| screen.contains("auto · model-a"))
        .expect("escape disarms the exit hint");
    session.send(b"/clear\r");
    session
        .wait_for(WAIT, |screen| !screen.contains(CANCELLATION))
        .expect("the old transcript leaves the screen");
    session.send(b"again\r");
    wait(&session, "Fresh.");
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let messages = requests[2].json()["messages"].clone();
    let users: Vec<&Value> = messages
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .collect();
    assert_eq!(users.len(), 1);
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

#[test]
fn resizing_replays_the_transcript_at_the_new_width() {
    let words = (0..12)
        .map(|index| format!("word{index:02}"))
        .collect::<Vec<_>>()
        .join(" ");
    let server = FakeServer::start([Reply::sse(&chat_text_events(&[words.as_str()]))]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let mut session = home.shell(20, 100);
    session.send(b"wrap\r");
    wait(&session, "word11");
    session.resize(20, 30).expect("resize the pty");
    let screen = session
        .wait_for(WAIT, |screen| {
            screen.contains("  word04 word05 word06 word07")
        })
        .unwrap_or_else(|screen| panic!("expected re-wrapped rows:\n{screen}"));
    assert!(screen.contains("  word00 word01 word02 word03\n"));
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).is_some());
}

#[test]
fn job_control_stops_reenter_the_terminal_once() {
    let server = FakeServer::start([]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let mut session = home.shell(24, 80);
    session.send(b"\x1a");
    assert!(session.wait_until_stopped(WAIT), "ctrl+z stops the shell");
    assert!(!session.screen().contains("auto · model-a"));
    session.resume().expect("continue the shell");
    wait(&session, "auto · model-a");
    session.resume().expect("send a stray continue");
    session.send(b"after");
    wait(&session, "┃ after");
    session.send(b"\x15\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
    assert!(session.drain_output(WAIT), "the terminal never closed");
    let output = session.output();
    assert_eq!(count(&output, b"\x1b[>1u"), 2);
    assert_eq!(count(&output, b"\x1b[<u"), 2);
    assert_eq!(count(&output, b"\x1b[3J"), 1);
}

fn user_messages(request: &ofx_testkit::RecordedRequest) -> usize {
    request.json()["messages"].as_array().map_or(0, |messages| {
        messages
            .iter()
            .filter(|message| message["role"] == "user")
            .count()
    })
}

#[test]
fn a_prompt_sent_right_after_clear_runs_visibly() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["One."])),
        Reply::sse(&chat_text_events(&["Two."])),
    ]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let mut session = home.shell(24, 80);
    session.send(b"first\r");
    wait(&session, "One.");
    session.send(b"/clear\rsecond\r");
    let screen = wait(&session, "Two.");
    assert!(screen.contains("┃ second"), "{screen}");
    assert!(!screen.contains("One."), "{screen}");
    assert_eq!(user_messages(&server.requests()[1]), 1);
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).is_some());
}

#[test]
fn a_prompt_sent_right_after_a_mid_turn_clear_is_not_dropped() {
    let held =
        Reply::held_sse(&chat_text_events(&["First line.\nSecond line.\n", "still going"])[..3]);
    let server = FakeServer::start([held, Reply::sse(&chat_text_events(&["Fresh."]))]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let mut session = home.shell(24, 80);
    session.send(b"slow\r");
    wait(&session, "First line.");
    session.send(b"/clear\rnext\r");
    let screen = wait(&session, "Fresh.");
    assert!(screen.contains("┃ next"), "{screen}");
    assert!(!screen.contains("First line."), "{screen}");
    assert_eq!(user_messages(&server.requests()[1]), 1);
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).is_some());
}

#[test]
fn a_failed_turn_keeps_its_error_under_its_prompt_while_prompts_queue() {
    let server = FakeServer::start([
        Reply::status(
            400,
            r#"{"error":{"message":"first prompt rejected","type":"invalid_request_error"}}"#,
        ),
        Reply::sse(&chat_text_events(&["Second answer."])),
    ]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let mut session = home.shell(24, 80);
    session.send(b"one\rtwo\r");
    let screen = wait(&session, "Second answer.");
    let error = "⚠ API request failed · HTTP 400 · invalid_request_error: first prompt rejected";
    let order: Vec<usize> = ["┃ one", error, "┃ two", "Second answer."]
        .iter()
        .map(|needle| {
            screen
                .find(needle)
                .unwrap_or_else(|| panic!("{needle:?} missing:\n{screen}"))
        })
        .collect();
    assert!(order.is_sorted(), "{screen}");
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
    assert!(session.screen().contains(error));
}

#[test]
fn typing_during_startup_reaches_the_composer() {
    let server = FakeServer::start([]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let mut session = PtySession::spawn(home.command(), 24, 80).expect("spawn oh-fx in a pty");
    session.send(b"typed early");
    wait(&session, "┃ typed early");
    wait(&session, "auto · model-a");
    session.send(b"\x15\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

#[test]
fn project_instruction_notices_reach_the_transcript_with_their_repair_hints() {
    let scoped = "sub\u{9b}2J";
    let read = json!({"path": format!("{scoped}/notes.txt")}).to_string();
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events("call-1", "read_file", &read)),
        Reply::sse(&chat_text_events(&["Read it."])),
    ]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let rules = "Run the whole test suite before every single commit.\n";
    fs::write(home.workspace.join("AGENTS.md"), rules).expect("write AGENTS.md");
    fs::create_dir_all(home.workspace.join(scoped).join("AGENTS.md"))
        .expect("create a non-regular rule file");
    fs::write(home.workspace.join(scoped).join("notes.txt"), "notes\n").expect("write the notes");
    let workspace = fs::canonicalize(&home.workspace).expect("canonicalize the workspace");
    let workspace = workspace.display();
    let truncated = format!(
        "! context: project instruction file \"{workspace}/AGENTS.md\" truncated: observed={} bytes effective=4 bytes source=command line; override with --context-limit project_instruction_file_bytes=BYTES|off",
        rules.len()
    );
    let shown_scope = ofx_text::encode_terminal_safe(scoped.as_bytes(), usize::MAX).text;
    let omitted = format!(
        "! context: project instructions action=omitted reason=non-regular rule file source=\"{workspace}/{shown_scope}/AGENTS.md\"; repair=replace the source with a regular file"
    );
    let mut session = home.shell_with(
        &["--context-limit", "project_instruction_file_bytes=4"],
        30,
        300,
        "auto · model-a",
    );
    wait(&session, &truncated);
    session.send(b"read the notes\r");
    let screen = wait(&session, "Read it.");
    assert_eq!(screen.matches(&truncated).count(), 1, "{screen}");
    assert!(screen.contains(&omitted), "{screen}");
    let output = session.output();
    assert_eq!(count(&output, "\u{9b}2J".as_bytes()), 0);
    session.send(b"/clear\r");
    let screen = session
        .wait_for(WAIT, |screen| {
            !screen.contains("Read it.") && screen.contains(&truncated)
        })
        .unwrap_or_else(|screen| {
            panic!("the startup notice did not return after /clear:\n{screen}")
        });
    assert!(!screen.contains(&omitted), "{screen}");
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

#[test]
fn accepted_prompts_are_recalled_in_the_next_session_of_the_workspace() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Noted."]))]);
    let home = Home::with_settings(&settings(&server.base_url()));
    let mut session = home.shell(24, 80);
    session.send(b"remember this prompt\r");
    wait(&session, "Noted.");
    session.send(b"/he\r");
    wait(&session, "Commands 14");
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());

    let history = home.root.join("data/oh-fx/history.jsonl");
    let workspace = fs::canonicalize(&home.workspace).expect("canonicalize the workspace");
    let lines: Vec<Value> = fs::read_to_string(&history)
        .expect("read the prompt history")
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON record"))
        .collect();
    let texts: Vec<&str> = lines
        .iter()
        .map(|line| line["text"].as_str().expect("text"))
        .collect();
    assert_eq!(texts, ["remember this prompt", "/help"]);
    assert_eq!(lines[0]["schema_version"], 1);
    assert_eq!(lines[0]["workspace_root"], workspace.to_str().unwrap());
    assert_eq!(
        fs::metadata(&history).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let mut session = home.shell(24, 80);
    session.send(b"\x1b[A");
    wait(&session, "┃ /help");
    session.send(b"\x1b[A\x1b[A");
    wait(&session, "┃ remember this prompt");
    session.send(b"\x15\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

#[test]
fn disabled_prompt_history_saves_nothing() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Unsaved."]))]);
    let mut settings = settings(&server.base_url());
    settings["prompt_history"] = json!({"enabled": false});
    let home = Home::with_settings(&settings);
    let mut session = home.shell(24, 80);
    session.send(b"forget this prompt\r");
    wait(&session, "Unsaved.");
    session.send(b"\x1b[A");
    session.send(b"x");
    wait(&session, "┃ x");
    session.send(b"\x15\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
    assert!(!home.root.join("data/oh-fx/history.jsonl").exists());
}

#[test]
fn at_mentions_pick_workspace_files_and_reach_the_model_as_typed() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Read it."]))]);
    let home = Home::with_settings(&settings(&server.base_url()));
    fs::create_dir_all(home.workspace.join("src")).expect("create src");
    fs::create_dir_all(home.workspace.join("docs")).expect("create docs");
    for file in ["src/main.rs", "src/mailbox.rs", "docs/my notes.md"] {
        fs::write(home.workspace.join(file), "").expect("write a workspace file");
    }
    let mut session = home.shell(24, 80);
    session.send(b"explain @mai");
    let screen = wait(&session, "src/mailbox.rs");
    assert!(screen.contains("src/main.rs"), "{screen}");
    session.send(b"\t");
    wait(&session, "┃ explain @src/main.rs");
    session.send(b"and @notes");
    wait(&session, "docs/my notes.md");
    session.send(b"\r");
    wait(&session, "┃ explain @src/main.rs and @\"docs/my notes.md\"");
    session.send(b"\r");
    wait(&session, "Read it.");
    let requests = server.requests();
    assert!(
        requests[0]
            .body_text()
            .contains("explain @src/main.rs and @\\\"docs/my notes.md\\\""),
        "{}",
        requests[0].body_text()
    );
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
    let cached = fs::read_dir(home.root.join("cache/oh-fx/file-index"))
        .expect("read the file index cache")
        .count();
    assert_eq!(cached, 1);
}
