use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use ofx_testkit::{FakeServer, PtySession, Reply};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(15);
const MODEL: &str = "gpt-6.1-sol";
const SIGNED_OUT: &str =
    "! auth: Codex needs a subscription login. Run /login, open Connections, then choose Codex
  subscription.";
const ACCESS_TOKEN: &str = "header.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF9zaGVsbCJ9LCJleHAiOjQxMDI0NDQ4MDB9.signature";
const LINK_START: &str = "\x1b]8;id=fx-codex-auth;";

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

struct Servers {
    auth: FakeServer,
    catalog: FakeServer,
    codex: FakeServer,
}

impl Home {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = directory.path().to_owned();
        let config = root.join("config/oh-fx");
        fs::create_dir_all(&config).expect("create the config directory");
        fs::create_dir_all(root.join("workspace")).expect("create the workspace");
        let settings = json!({
            "provider": "codex",
            "models": {"codex": MODEL},
            "session_titles": false
        });
        fs::write(config.join("settings.json"), settings.to_string()).expect("write settings.json");
        Self {
            _directory: directory,
            root,
        }
    }

    fn shell(&self, servers: &Servers) -> PtySession {
        let session = self.spawn(servers, &[]);
        wait(&session, &format!("run /login · auto · {MODEL}"));
        session
    }

    fn spawn(&self, servers: &Servers, args: &[&str]) -> PtySession {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .args(args)
            .current_dir(self.root.join("workspace"))
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .env("OH_FX_NO_OPEN_BROWSER", "1")
            .env("OH_FX_TRACE_LOG", self.trace_log())
            .env("OH_FX_E2E_CHATGPT_ISSUER_URL", servers.auth.base_url())
            .env(
                "OH_FX_E2E_CHATGPT_TOKEN_URL",
                format!("{}/oauth/token", servers.auth.base_url()),
            )
            .env(
                "OH_FX_E2E_OPENAI_CODEX_MODELS_URL",
                format!("{}/backend-api/codex/models", servers.catalog.base_url()),
            )
            .env(
                "OH_FX_E2E_CODEX_VERSION_URL",
                format!("{}/@openai/codex/latest", servers.catalog.base_url()),
            )
            .env(
                "OH_FX_E2E_OPENAI_CODEX_RESPONSES_URL",
                format!("{}/backend-api/codex/responses", servers.codex.base_url()),
            )
            .process_group(0);
        PtySession::spawn(command, 30, 100).expect("spawn oh-fx in a pty")
    }

    fn trace_log(&self) -> PathBuf {
        self.root.join("trace.log")
    }

    fn login(&self) -> PathBuf {
        self.root.join("data/oh-fx/chatgpt-auth.json")
    }

    fn settings(&self) -> Value {
        let text = fs::read_to_string(self.root.join("config/oh-fx/settings.json"))
            .expect("read settings.json");
        serde_json::from_str(&text).expect("parse settings.json")
    }
}

fn servers(replies: usize) -> Servers {
    let tokens = json!({
        "access_token": ACCESS_TOKEN,
        "refresh_token": "rt-shell",
        "expires_in": 3600,
    });
    servers_exchanging(Reply::status(200, tokens.to_string()), replies)
}

fn servers_exchanging(exchange: Reply, replies: usize) -> Servers {
    let model = json!({
        "slug": MODEL,
        "visibility": "list",
        "supported_in_api": true,
        "supported_reasoning_levels": [{"effort": "low"}],
    });
    let events = [
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","phase":"final_answer"}}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":"Signed in and answered."}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":20,"output_tokens":3}}}),
    ]
    .map(|event| event.to_string());
    Servers {
        auth: FakeServer::start([exchange]),
        catalog: FakeServer::start([
            Reply::status(200, json!({"version": "0.153.1"}).to_string()),
            Reply::status(200, json!({"models": [model]}).to_string()),
        ]),
        codex: FakeServer::start((0..replies).map(|_| Reply::sse(&events))),
    }
}

fn wait(session: &PtySession, needle: &str) -> String {
    session
        .wait_for(WAIT, |screen| screen.contains(needle))
        .unwrap_or_else(|screen| panic!("expected {needle:?} on screen:\n{screen}"))
}

fn hold_and_sign_in(session: &PtySession) -> String {
    session.send(b"hello there\r");
    wait(session, SIGNED_OUT);
    session.send(b"/login\r");
    wait(session, "┃ /login");
    session.send(b"codex\r");
    let screen = wait(session, "Waiting for authorization…");
    assert!(
        !screen.contains("Preparing Codex subscription."),
        "{screen}"
    );
    let rows: Vec<&str> = screen.lines().map(str::trim_end).collect();
    assert!(
        rows.iter().any(|row| row.starts_with("Sign in with Codex")
            && row.ends_with("Waiting for authorization…")),
        "{screen}"
    );
    assert!(rows.contains(&"  Open   Authorize with Codex"), "{screen}");
    assert!(
        rows.contains(&"enter reopens browser · esc cancels"),
        "{screen}"
    );
    let output = String::from_utf8_lossy(&session.output()).into_owned();
    let start = output.rfind(LINK_START).expect("the authorization link") + LINK_START.len();
    let end = start + output[start..].find("\x1b\\").expect("the link end");
    output[start..end].to_owned()
}

fn authorize(url: &str) -> String {
    let query = url.split_once('?').expect("a query").1;
    let value = |key: &str| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
            .expect("the parameter")
            .replace("%3A", ":")
            .replace("%2F", "/")
    };
    let redirect = value("redirect_uri");
    let state = value("state");
    let (authority, path) = redirect
        .strip_prefix("http://")
        .and_then(|rest| rest.split_once('/'))
        .expect("a loopback redirect");
    let port: u16 = authority
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .expect("a callback port");
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("reach the callback");
    let request =
        format!("GET /{path}?code=granted&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("send the callback");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

#[test]
fn a_codex_shell_without_a_login_signs_in_from_login_and_runs_the_held_prompt() {
    let home = Home::new();
    let servers = servers(1);
    let mut session = home.shell(&servers);
    let url = hold_and_sign_in(&session);
    assert!(
        url.starts_with(&format!("{}/oauth/authorize?", servers.auth.base_url())),
        "{url}"
    );
    assert!(authorize(&url).starts_with("HTTP/1.1 200"));
    wait(
        &session,
        &format!("* provider: Switched to Codex subscription with {MODEL}."),
    );
    let screen = wait(&session, "Signed in and answered.");
    assert_eq!(screen.matches("hello there").count(), 1, "{screen}");
    assert!(!screen.contains("Sign in with Codex"), "{screen}");
    assert!(!screen.contains("run /login"), "{screen}");
    assert!(home.login().exists());
    assert_eq!(home.settings()["provider"], "codex");
    let request = servers.codex.requests()[0].json().to_string();
    assert!(request.contains("hello there"), "{request}");
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

#[test]
fn escape_cancels_the_sign_in_and_drops_the_held_prompt() {
    let home = Home::new();
    let servers = servers(0);
    let mut session = home.shell(&servers);
    hold_and_sign_in(&session);
    session.send(b"\x1b[27u");
    session
        .wait_for(WAIT, |screen| !screen.contains("Sign in with Codex"))
        .unwrap_or_else(|screen| panic!("the sign-in screen stays open:\n{screen}"));
    session.send(b"again\r");
    let screen = session
        .wait_for(WAIT, |screen| screen.matches(SIGNED_OUT).count() == 2)
        .unwrap_or_else(|screen| panic!("the next prompt is not held:\n{screen}"));
    assert!(screen.contains("run /login"), "{screen}");
    assert!(!home.login().exists());
    assert!(servers.auth.requests().is_empty());
    assert!(servers.codex.requests().is_empty());
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

#[test]
fn a_session_continued_without_its_codex_login_asks_for_one() {
    let home = Home::new();
    let servers = servers(1);
    let mut session = home.shell(&servers);
    let url = hold_and_sign_in(&session);
    authorize(&url);
    wait(&session, "Signed in and answered.");
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
    fs::remove_file(home.login()).expect("sign out outside the shell");
    let mut session = home.spawn(&servers, &["-c"]);
    let screen = wait(&session, "! auth: Codex needs a subscription login.");
    assert!(screen.contains("hello there"), "{screen}");
    assert!(screen.contains("run /login"), "{screen}");
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

#[test]
fn a_rejected_code_exchange_ends_the_sign_in_and_traces_it_without_the_code() {
    let home = Home::new();
    let rejected = json!({"error": "invalid_grant", "error_description": "code rejected"});
    let servers = servers_exchanging(Reply::status(400, rejected.to_string()), 0);
    let mut session = home.shell(&servers);
    let url = hold_and_sign_in(&session);
    authorize(&url);
    wait(
        &session,
        "Codex sign-in failed. The current credential is unchanged.",
    );
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
    assert!(!home.login().exists());
    let log = fs::read_to_string(home.trace_log()).expect("read the trace log");
    let token_url = format!("{}/oauth/token", servers.auth.base_url());
    for line in [
        format!("[auth] ChatGPT OAuth request rejected url={token_url}\n"),
        "[auth] login failed source=chatgpt_subscription err=ChatGptOAuthRequestFailed\n"
            .to_owned(),
    ] {
        assert!(log.contains(&line), "{line}{log}");
    }
    let exchange = servers.auth.requests()[0].body_text();
    let verifier = exchange
        .split('&')
        .find_map(|pair| pair.strip_prefix("code_verifier="))
        .expect("the code verifier");
    for secret in [verifier, "code=granted"] {
        assert!(
            !log.contains(secret),
            "{secret} reached the trace log:\n{log}"
        );
    }
    assert!(servers.codex.requests().is_empty());
}
