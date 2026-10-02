use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{self, Command};
use std::sync::PoisonError;

use ofx_auth::ChatGptEndpoints;
use ofx_config::{ProfilePaths, Settings};
use ofx_gateway::{CodexEndpoints, CodexModelsEndpoints};
use ofx_testkit::{FakeServer, PtySession, Reply};
use serde_json::{Value, json};

use super::*;
use crate::app_panic_runtime::HOOK_TESTS;

const CHILD_HOME: &str = "OH_FX_LIFECYCLE_CHILD_HOME";
const CHILD_AUTH: &str = "OH_FX_LIFECYCLE_CHILD_AUTH";
const CHILD_CODEX: &str = "OH_FX_LIFECYCLE_CHILD_CODEX";
const CHILD_CATALOG: &str = "OH_FX_LIFECYCLE_CHILD_CATALOG";
const REFRESH_TEST: &str = "app_lifecycle::tests::an_exit_during_a_codex_refresh_restores_the_terminal_then_saves_the_rotated_login";
const FIRST_FRAME: &str = "Run /help for commands";
const WAIT: Duration = Duration::from_secs(15);
const REFRESH_DELAY: Duration = Duration::from_secs(4);
const SAVED_TOKEN: &str = "eyJhbGciOiJub25lIn0.c2F2ZWQtYWNjZXNz.c2lnbmF0dXJl";
const FRESH_TOKEN: &str = "eyJhbGciOiJub25lIn0.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF90ZXN0In0sImV4cCI6NDEwMjQ0NDgwMCwibWFya2VyIjoiZnJlc2gifQ.c2lnbmF0dXJl";
const REFRESH_TOKEN: &str = "rt-refresh-secret-0123456789";
const ROTATED_REFRESH_TOKEN: &str = "rt-rotated-secret-9876543210";
const FAR_FUTURE_MS: i64 = 4_102_444_800_000;

#[test]
fn terminal_refusals_read_as_upstream_prints_them() {
    let message = |error| SessionError::Terminal(error).to_string();
    assert_eq!(
        message(TerminalError::TerminalTooSmall),
        "oh-fx needs at least 5 terminal rows."
    );
    assert_eq!(
        message(TerminalError::NotATerminal),
        "oh-fx requires an interactive terminal (TTY)."
    );
    assert_eq!(
        message(TerminalError::UnableToReadTerminalSize),
        "oh-fx: unable to read the terminal size"
    );
}

#[test]
fn worker_panics_are_reported_instead_of_printed_and_clean_exits_are_quiet() {
    let _serial = HOOK_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
    let panics = PanicCapture::install(WORKER_THREAD, drop);
    let finished = Worker::spawn(|| {}).unwrap();
    assert!(finished.finish(None, &panics).is_ok());
    let crashed = Worker::spawn(|| panic!("worker exploded")).unwrap();
    let message = crashed.finish(None, &panics).unwrap_err().to_string();
    assert!(
        message.starts_with("oh-fx: the agent stopped unexpectedly: panicked at "),
        "{message}"
    );
    assert!(message.ends_with(": worker exploded"), "{message}");
    assert_eq!(panics.take_worker_report(), None);
}

#[test]
fn an_exit_during_a_codex_refresh_restores_the_terminal_then_saves_the_rotated_login() {
    if let Some(home) = env::var_os(CHILD_HOME) {
        run_a_codex_session(Path::new(&home));
    }
    let home = tempfile::tempdir().unwrap();
    let paths = profile_paths(home.path());
    write_codex_profile(&paths, &home.path().join("workspace"));
    let rotated = json!({
        "access_token": FRESH_TOKEN,
        "refresh_token": ROTATED_REFRESH_TOKEN,
        "expires_in": 3600,
    });
    let auth = FakeServer::start([Reply::delayed_status(
        200,
        rotated.to_string(),
        REFRESH_DELAY,
    )]);
    let codex = FakeServer::start([Reply::status(
        401,
        r#"{"error":{"message":"token expired"}}"#,
    )]);
    let catalog = FakeServer::start([]);
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args([REFRESH_TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_HOME, home.path())
        .env(CHILD_AUTH, auth.base_url())
        .env(CHILD_CODEX, codex.base_url())
        .env(CHILD_CATALOG, catalog.base_url())
        .env("HOME", home.path())
        .env("TERM", "xterm-256color");
    let mut session = PtySession::spawn(command, 24, 80).unwrap();
    session
        .wait_for(WAIT, |screen| screen.contains(FIRST_FRAME))
        .unwrap_or_else(|screen| panic!("the shell never started:\n{screen}"));
    session.send(b"hello\r");
    wait_until(|| !auth.requests().is_empty());
    session.send(b"\x03");
    session
        .wait_for(WAIT, |screen| screen.contains("Cancelled"))
        .unwrap_or_else(|screen| panic!("the turn was not cancelled:\n{screen}"));
    session.send(b"\x04");
    wait_until(|| session.cooked().unwrap());
    assert!(
        session.wait_exit(Duration::ZERO).is_none(),
        "the exit did not wait for the refresh"
    );
    assert_eq!(saved(&paths)["refresh_token"], REFRESH_TOKEN);
    let status = session
        .wait_exit(WAIT)
        .expect("the exit ends once the refresh is saved");
    assert!(status.success(), "{status:?}");
    assert_eq!(auth.requests().len(), 1);
    assert_eq!(saved(&paths)["access_token"], FRESH_TOKEN);
    assert_eq!(saved(&paths)["refresh_token"], ROTATED_REFRESH_TOKEN);
}

fn wait_until(ready: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

fn profile_paths(home: &Path) -> ProfilePaths {
    ProfilePaths {
        config: home.join("config"),
        data: home.join("data"),
        state: home.join("state"),
        cache: home.join("cache"),
    }
}

fn write_codex_profile(paths: &ProfilePaths, workspace: &Path) {
    fs::create_dir_all(&paths.config).unwrap();
    fs::create_dir_all(&paths.data).unwrap();
    fs::create_dir_all(workspace).unwrap();
    fs::write(
        paths.config.join("settings.json"),
        json!({"provider": "codex", "models": {"codex": "gpt-5.4"}}).to_string(),
    )
    .unwrap();
    fs::set_permissions(&paths.data, fs::Permissions::from_mode(0o700)).unwrap();
    let login = paths.data.join("chatgpt-auth.json");
    let session = json!({
        "version": 1,
        "access_token": SAVED_TOKEN,
        "refresh_token": REFRESH_TOKEN,
        "expires_at_ms": FAR_FUTURE_MS,
        "account_id": "acct_test",
    });
    fs::write(&login, format!("{session}\n")).unwrap();
    fs::set_permissions(&login, fs::Permissions::from_mode(0o600)).unwrap();
}

fn saved(paths: &ProfilePaths) -> Value {
    serde_json::from_slice(&fs::read(paths.data.join("chatgpt-auth.json")).unwrap()).unwrap()
}

fn run_a_codex_session(home: &Path) -> ! {
    let url = |name: &str| env::var(name).unwrap();
    let paths = profile_paths(home);
    let workspace = fs::canonicalize(home.join("workspace")).unwrap();
    let settings = Settings::load(&paths, &workspace).unwrap();
    let profile = Profile::new(workspace, Some(paths), settings).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let executions = ManagedExecutions::new(SessionSupervisor::new("/nonexistent"));
    let auth = url(CHILD_AUTH);
    let catalog = url(CHILD_CATALOG);
    let endpoints = SubscriptionEndpoints {
        chatgpt: ChatGptEndpoints {
            issuer: auth.clone(),
            token_url: format!("{auth}/oauth/token"),
            callback_ports: vec![0],
        },
        codex: CodexEndpoints {
            responses: format!("{}/backend-api/codex/responses", url(CHILD_CODEX)),
        },
        models: CodexModelsEndpoints {
            models: format!("{catalog}/backend-api/codex/models"),
            client_version: format!("{catalog}/@openai/codex/latest"),
        },
    };
    let setup = runtime
        .block_on(profile.connect_interactive(
            Launch {
                model: None,
                permission_mode: PermissionMode::Auto,
                system_prompt: None,
                reasoning_effort: None,
                fast_mode: false,
                context_limits: &[],
                command_timeout: None,
                executions: &executions,
                endpoints,
            },
            &CancellationToken::new(),
        ))
        .unwrap();
    let session = Session {
        profile,
        setup,
        executions,
        permission_mode: PermissionMode::Auto,
    };
    process::exit(i32::from(run(session, None, runtime).is_err()));
}
