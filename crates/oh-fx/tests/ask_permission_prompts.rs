use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ofx_testkit::{
    FakeServer, PtySession, RecordedRequest, Reply, chat_text_events, chat_tool_call_events,
};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(30);
const KEY: (&str, &str) = ("PORTKEY_API_KEY", "pk-test-0123456789");
const DENIED: &str = r#"{"error":{"type":"tool_permission_denied","tool_name":"read_file","message":"Permission denied by user","reason":"user_denied","denied":true,"suggestion":"The tool did not run. Do not retry unchanged; explain the denial or use a safer allowed alternative."}}"#;

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    secret: String,
}

impl Home {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = fs::canonicalize(directory.path()).expect("canonicalize the home");
        let workspace = root.join("workspace");
        fs::create_dir_all(root.join("config/oh-fx")).expect("create the config directory");
        fs::create_dir_all(&workspace).expect("create the workspace");
        fs::create_dir_all(root.join("outside")).expect("create the outside directory");
        let secret = root.join("outside/secret.txt");
        fs::write(&secret, "outside secret\n").expect("write the outside file");
        Self {
            _directory: directory,
            secret: secret.to_string_lossy().into_owned(),
            root,
            workspace,
        }
    }

    fn configure(&self, server: &FakeServer, permission_mode: &str) {
        let settings = json!({
            "provider": "portkey",
            "model": "@openai/gpt-4o",
            "permission_mode": permission_mode,
            "providers": {
                "portkey": {
                    "protocol": "openai-chat-completions",
                    "base_url": server.base_url(),
                    "auth": {"type": "none"},
                    "headers": {"x-portkey-api-key": "${PORTKEY_API_KEY}"},
                    "models": ["@openai/gpt-4o"]
                }
            }
        });
        fs::write(
            self.root.join("config/oh-fx/settings.json"),
            settings.to_string(),
        )
        .expect("write settings.json");
    }

    fn spawn(&self, args: &[&str]) -> PtySession {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .args(args)
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
            .env("OH_FX_TRACE_LOG", self.root.join("trace.log"))
            .env(KEY.0, KEY.1);
        PtySession::spawn(command, 40, 200).expect("spawn oh-fx in a pty")
    }

    fn read_call(&self) -> Vec<String> {
        chat_tool_call_events(
            "call_1",
            "read_file",
            &json!({"path": self.secret}).to_string(),
        )
    }
}

fn wait(session: &PtySession, needle: &str) -> String {
    session
        .wait_for(WAIT, |screen| screen.contains(needle))
        .unwrap_or_else(|screen| panic!("expected {needle:?} on screen:\n{screen}"))
}

fn finishes(session: &mut PtySession) -> i32 {
    session
        .wait_exit(WAIT)
        .and_then(|status| status.code())
        .expect("oh-fx exits")
}

fn tool_messages(request: &RecordedRequest) -> Vec<Value> {
    request.json()["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] == "tool")
        .cloned()
        .collect()
}

fn prompt(label: &str) -> String {
    format!("oh-fx wants to run:\n  {label}\n\nApprove? [y/N]")
}

#[test]
fn ask_prompts_on_the_terminal_and_runs_an_approved_read() {
    let home = Home::new();
    let server = FakeServer::start([
        Reply::sse(&home.read_call()),
        Reply::sse(&chat_text_events(&["It is a secret."])),
    ]);
    home.configure(&server, "ask");
    let mut session = home.spawn(&["ask", "read it"]);
    let screen = wait(&session, "Approve? [y/N]");
    assert!(
        screen.contains(&format!(
            "Reading {}\n{}",
            home.secret,
            prompt(&format!("read_file {}", home.secret))
        )),
        "{screen}"
    );
    let output = session.output();
    assert!(
        output.ends_with(b"Approve? [y/N] \x07"),
        "{:?}",
        String::from_utf8_lossy(&output)
    );
    session.send(b"y\r");
    wait(&session, "It is a secret.");
    assert_eq!(finishes(&mut session), 0);
    let requests = server.requests();
    assert_eq!(
        tool_messages(&requests[1]),
        [json!({
            "role": "tool",
            "content": format!("<path>{}</path>\n<content>\n1\toutside secret\n</content>", home.secret),
            "tool_call_id": "call_1",
        })]
    );
}

#[test]
fn a_denied_prompt_reaches_the_model_and_the_turn_goes_on() {
    for answer in [&b"n\r"[..], b"\r", b"yes\r"] {
        let home = Home::new();
        let server = FakeServer::start([
            Reply::sse(&home.read_call()),
            Reply::sse(&chat_text_events(&["Understood."])),
        ]);
        home.configure(&server, "ask");
        let mut session = home.spawn(&["ask", "read it"]);
        wait(&session, "Approve? [y/N]");
        session.send(answer);
        wait(&session, "Understood.");
        assert_eq!(finishes(&mut session), 0, "{answer:?}");
        assert_eq!(
            tool_messages(&server.requests()[1]),
            [json!({"role": "tool", "content": DENIED, "tool_call_id": "call_1"})],
            "{answer:?}"
        );
        assert!(
            !String::from_utf8_lossy(&session.output()).contains("Denied"),
            "{answer:?}"
        );
    }
}

#[test]
fn a_denied_prompt_writes_the_users_denial_to_the_trace() {
    let home = Home::new();
    let server = FakeServer::start([
        Reply::sse(&home.read_call()),
        Reply::sse(&chat_text_events(&["Understood."])),
    ]);
    home.configure(&server, "ask");
    let mut session = home.spawn(&["ask", "read it"]);
    wait(&session, "Approve? [y/N]");
    session.send(b"n\r");
    wait(&session, "Understood.");
    assert_eq!(finishes(&mut session), 0);
    let log = fs::read_to_string(home.root.join("trace.log")).expect("read the trace log");
    let denied = format!(
        " [tool] event=execution_result turn_id=1 step_id=1 call_id=call_1 name=read_file result_kind=permission_denied reason=user_denied model_output_bytes={}\n",
        DENIED.len()
    );
    assert!(log.contains(&denied), "{denied}\n{log}");
    assert!(!log.contains("event=execution_start"), "{log}");
}

#[test]
fn an_overlong_answer_never_answers_the_next_prompt() {
    let home = Home::new();
    let other = home.root.join("outside/other.txt");
    fs::write(&other, "another secret\n").expect("write the second outside file");
    let second_read =
        chat_tool_call_events("call_2", "read_file", &json!({"path": other}).to_string());
    let server = FakeServer::start([
        Reply::sse(&home.read_call()),
        Reply::sse(&second_read),
        Reply::sse(&chat_text_events(&["Left alone."])),
    ]);
    home.configure(&server, "ask");
    let mut session = home.spawn(&["ask", "read it twice"]);
    wait(&session, "Approve? [y/N]");
    let mut overlong = vec![b' '; 257];
    overlong.extend_from_slice(b"y\r");
    session.send(&overlong);
    session
        .wait_for(WAIT, |screen| screen.matches("Approve? [y/N]").count() == 2)
        .unwrap_or_else(|screen| panic!("expected a second prompt on screen:\n{screen}"));
    assert_eq!(server.requests().len(), 2);
    session.send(b"n\r");
    wait(&session, "Left alone.");
    assert_eq!(finishes(&mut session), 0);
    let requests = server.requests();
    assert_eq!(
        tool_messages(&requests[1]),
        [json!({"role": "tool", "content": DENIED, "tool_call_id": "call_1"})]
    );
    assert_eq!(
        tool_messages(&requests[2])[1],
        json!({"role": "tool", "content": DENIED, "tool_call_id": "call_2"})
    );
}

#[test]
fn a_label_holding_terminal_controls_is_shown_escaped() {
    let home = Home::new();
    let outside = home.root.join("outside").to_string_lossy().into_owned();
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "grep_files",
            &json!({"pattern": "\u{1b}[2J\u{1b}[Hspoofed", "path": outside}).to_string(),
        )),
        Reply::sse(&chat_text_events(&["Left alone."])),
    ]);
    home.configure(&server, "ask");
    let mut session = home.spawn(&["ask", "search it"]);
    let screen = wait(&session, "Approve? [y/N]");
    assert!(
        screen.contains(&prompt(r"grep_files \x1b[2J\x1b[Hspoofed")),
        "{screen}"
    );
    let output = session.output();
    assert!(
        !output.windows(2).any(|pair| pair == b"\x1b["),
        "{:?}",
        String::from_utf8_lossy(&output)
    );
    session.send(b"n\r");
    wait(&session, "Left alone.");
    assert_eq!(finishes(&mut session), 0);
}

#[test]
fn json_output_prompts_only_with_prompt_permissions() {
    let home = Home::new();
    let server = FakeServer::start([
        Reply::sse(&home.read_call()),
        Reply::sse(&chat_text_events(&["It is a secret."])),
        Reply::sse(&home.read_call()),
    ]);
    home.configure(&server, "ask");
    let mut session = home.spawn(&["ask", "--json", "--prompt-permissions", "read it"]);
    wait(&session, &prompt(&format!("read_file {}", home.secret)));
    session.send(b"Y\r");
    let screen = wait(&session, "\"final_output\":\"It is a secret.\"");
    assert!(
        screen.contains("\"tool_calls\":[{\"name\":\"read_file\",\"status\":\"success\"}]"),
        "{screen}"
    );
    assert_eq!(finishes(&mut session), 0);

    let mut blocked = home.spawn(&["ask", "--json", "read it"]);
    let screen = wait(&blocked, "NonInteractivePermissionRequired");
    assert!(!screen.contains("Approve?"), "{screen}");
    assert!(
        screen.contains("oh-fx ask: permission required for tool execution in noninteractive mode"),
        "{screen}"
    );
    assert_eq!(finishes(&mut blocked), 1);
}

#[test]
fn commands_show_their_risk_and_file_changes_show_the_mutation_label() {
    let home = Home::new();
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "shell",
            r#"{"request":{"action":"run","command":"rm -rf build"}}"#,
        )),
        Reply::sse(&chat_tool_call_events(
            "call_2",
            "write_file",
            r#"{"path":"notes.txt","content":"new\n"}"#,
        )),
        Reply::sse(&chat_text_events(&["Left alone."])),
    ]);
    home.configure(&server, "ask");
    let mut session = home.spawn(&["ask", "tidy up"]);
    wait(
        &session,
        &prompt(
            "shell.run rm -rf build (risk: command may remove files forcefully; safer: inspect targets first)",
        ),
    );
    session.send(b"n\r");
    let screen = wait(&session, &prompt("file_mutation"));
    assert!(!screen.contains("Writing"), "{screen}");
    session.send(b"n\r");
    let screen = wait(&session, "Left alone.");
    assert!(
        screen.contains(&format!("{} n\nWriting file", prompt("file_mutation"))),
        "{screen}"
    );
    assert_eq!(finishes(&mut session), 0);
    assert!(!Path::new(&home.workspace.join("notes.txt")).exists());
}
