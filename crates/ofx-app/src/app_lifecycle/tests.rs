use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::PoisonError;
use std::time::Instant;

use ofx_auth::ChatGptEndpoints;
use ofx_config::{PrivateDir, ProfilePaths, Settings};
use ofx_gateway::{CodexEndpoints, CodexModelsEndpoints};
use ofx_testkit::{FakeServer, PtySession, Reply};
use ofx_tui::PromptHistory;
use serde_json::{Value, json};

use super::*;
use crate::app_panic_runtime::HOOK_TESTS;

const CHILD_HOME: &str = "OH_FX_LIFECYCLE_CHILD_HOME";
const CHILD_AUTH: &str = "OH_FX_LIFECYCLE_CHILD_AUTH";
const CHILD_CODEX: &str = "OH_FX_LIFECYCLE_CHILD_CODEX";
const CHILD_CATALOG: &str = "OH_FX_LIFECYCLE_CHILD_CATALOG";
const CHILD_PANIC: &str = "OH_FX_LIFECYCLE_CHILD_PANIC";
const CHILD_CLIPBOARD: &str = "OH_FX_LIFECYCLE_CHILD_CLIPBOARD";
const CLIPBOARD_TEST: &str =
    "app_lifecycle::tests::leaving_stops_the_agent_before_it_waits_for_a_copy_in_flight";
const PANIC_TEST: &str =
    "app_lifecycle::tests::a_worker_panic_ends_the_shell_and_a_contained_one_does_not";
const REFRESH_TEST: &str = "app_lifecycle::tests::an_exit_during_a_slow_codex_refresh_restores_the_terminal_then_saves_the_rotated_login";
const FIRST_FRAME: &str = "Run /help for commands";
const REFRESH_STARTED: &str = "refresh-started";
const WAIT: Duration = Duration::from_secs(15);
const LOCK_HELD: Duration = Duration::from_millis(1250);
const REFRESH_DELAY: Duration = Duration::from_millis(14_250);
const SAVED_WAIT: Duration = Duration::from_secs(30);
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
    let (shell, _events) = ui_channel().unwrap();
    let finished = Worker::spawn(shell.clone(), || {}).unwrap();
    assert!(finished.finish(None, &panics).is_ok());
    let crashed = Worker::spawn(shell, || panic!("worker exploded")).unwrap();
    let message = crashed.finish(None, &panics).unwrap_err().to_string();
    assert!(
        message.starts_with("oh-fx: the agent stopped unexpectedly: panicked at "),
        "{message}"
    );
    assert!(message.ends_with(": worker exploded"), "{message}");
    assert_eq!(panics.take_worker_report(), None);
}

#[test]
fn an_exit_during_a_slow_codex_refresh_restores_the_terminal_then_saves_the_rotated_login() {
    if let Some(home) = env::var_os(CHILD_HOME) {
        run_a_codex_session(Path::new(&home));
    }
    let home = tempfile::tempdir().unwrap();
    let paths = profile_paths(home.path());
    let refresh_started = home.path().join(REFRESH_STARTED);
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
    let held = PrivateDir::open_existing_private(&paths.data)
        .unwrap()
        .unwrap()
        .try_lock("chatgpt-auth.lock")
        .unwrap()
        .expect("the credential lock is free");
    session.send(b"hello\r");
    wait_until(|| !codex.requests().is_empty());
    let refreshing = Instant::now();
    let release = thread::spawn(move || {
        thread::sleep(LOCK_HELD.saturating_sub(refreshing.elapsed()));
        drop(held);
    });
    wait_until(|| refresh_started.exists());
    session.send(b"\x03");
    session
        .wait_for(WAIT, |screen| screen.contains("Cancelled"))
        .unwrap_or_else(|screen| panic!("the turn was not cancelled:\n{screen}"));
    session.send(b"\x04");
    wait_until(|| session.cooked().unwrap());
    wait_until(|| !auth.requests().is_empty());
    assert!(refreshing.elapsed() >= LOCK_HELD);
    release.join().unwrap();
    assert!(
        session.wait_exit(Duration::ZERO).is_none(),
        "the exit did not wait for the refresh"
    );
    assert_eq!(saved(&paths)["refresh_token"], REFRESH_TOKEN);
    let status = session
        .wait_exit(SAVED_WAIT)
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
    let profile = Profile::new(workspace, Some(home.into()), Some(paths), settings).unwrap();
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
                fast_mode: None,
                context_limits: &[],
                command_timeout: None,
                executions: &executions,
                endpoints,
            },
            &CancellationToken::new(),
        ))
        .unwrap();
    report_the_first_refresh(setup.refreshes(), home.join(REFRESH_STARTED));
    let session = Session {
        profile,
        setup,
        executions,
        permission_mode: PermissionMode::Auto,
    };
    process::exit(i32::from(run(session, None, runtime).is_err()));
}

fn report_the_first_refresh(refreshes: Option<Arc<DetachedRefreshes>>, marker: PathBuf) {
    let refreshes = refreshes.expect("the interactive session detaches its refreshes");
    thread::spawn(move || {
        while !refreshes.pending() {
            thread::sleep(Duration::from_millis(5));
        }
        fs::write(marker, "").unwrap();
    });
}

#[test]
fn a_worker_panic_ends_the_shell_and_a_contained_one_does_not() {
    if env::var_os(CHILD_PANIC).is_some() {
        run_a_worker_that_panics();
    }
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args([PANIC_TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_PANIC, "1")
        .env("TERM", "xterm-256color");
    let mut session = PtySession::spawn(command, 24, 80).unwrap();
    session
        .wait_for(WAIT, |screen| screen.contains(FIRST_FRAME))
        .unwrap_or_else(|screen| panic!("the shell never started:\n{screen}"));
    session.send(b"one\r");
    session
        .wait_for(WAIT, |screen| {
            screen.contains("✗ panic: thread 'oh-fx-agent' panicked at ")
                && screen.contains("still serving")
        })
        .unwrap_or_else(|screen| panic!("the contained panic ended the shell:\n{screen}"));
    assert!(session.wait_exit(Duration::ZERO).is_none());
    assert!(!session.cooked().unwrap());
    session.send(b"two\r");
    let status = session
        .wait_exit(WAIT)
        .expect("a worker panic ends the shell");
    assert_eq!(status.code(), Some(1));
    assert!(session.cooked().unwrap());
    assert!(session.drain_output(WAIT), "the terminal never closed");
    let output = String::from_utf8_lossy(&session.output()).into_owned();
    let report = output
        .split("oh-fx: the agent stopped unexpectedly: panicked at ")
        .nth(1)
        .unwrap_or_else(|| panic!("no report after the terminal was restored: {output:?}"));
    assert!(report.contains(": worker exploded"), "{output:?}");
    assert!(!report.contains("tool exploded"), "{output:?}");
}

fn run_a_worker_that_panics() -> ! {
    let (events, receiver) = ui_channel().unwrap();
    let options = ShellOptions {
        version: "0.1.0".to_owned(),
        model: "model-a".to_owned(),
        permission_mode: PermissionMode::Auto,
        full_access_warning: false,
        workspace_label: "workspace".to_owned(),
        workspace_root: PathBuf::from("/workspace"),
        commands: Vec::new(),
        command_categories: Vec::new(),
        prompt_history: PromptHistory::disabled(),
        file_mentions: None,
    };
    let outcome = host(options, events, receiver, None, |events, mut commands| {
        let _ = commands.blocking_recv();
        assert!(panic::catch_unwind(|| panic!("tool exploded")).is_err());
        events.send(UiEvent::Notice {
            notice: Notice::new(NoticeTone::Neutral, "", "still serving"),
        });
        let _ = commands.blocking_recv();
        panic!("worker exploded");
    });
    if let Err(error) = &outcome {
        eprintln!("{error}");
    }
    process::exit(i32::from(outcome.is_err()));
}

#[test]
fn leaving_stops_the_agent_before_it_waits_for_a_copy_in_flight() {
    if let Some(directory) = env::var_os(CHILD_CLIPBOARD) {
        run_a_shell_that_copies(Path::new(&directory));
    }
    let directory = tempfile::tempdir().unwrap();
    let tools = directory.path().join("tools");
    fs::create_dir(&tools).unwrap();
    let copied = directory.path().join("copied");
    let stopped = directory.path().join("stopped");
    let finished = directory.path().join("finished");
    let script = format!(
        "#!/bin/sh\ncat > '{}'\nwhile [ ! -e '{}' ]; do sleep 0.01; done\ntouch '{}'\n",
        copied.display(),
        stopped.display(),
        finished.display()
    );
    for name in ["pbcopy", "xclip"] {
        fs::write(tools.join(name), &script).unwrap();
        fs::set_permissions(tools.join(name), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args([CLIPBOARD_TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_CLIPBOARD, directory.path())
        .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
        .env("TERM", "xterm-256color");
    let mut session = PtySession::spawn(command, 24, 80).unwrap();
    session
        .wait_for(WAIT, |screen| screen.contains(FIRST_FRAME))
        .unwrap_or_else(|screen| panic!("the shell never started:\n{screen}"));
    session.send(b"keep me\x1b[97;9u\x1b[99;9u");
    wait_until(|| fs::read_to_string(&copied).is_ok_and(|text| text == "keep me"));
    session.send(b"\x03\x04");
    let status = session
        .wait_exit(WAIT)
        .expect("the shell exits once the copy finishes");
    assert!(status.success(), "{status:?}");
    assert!(stopped.exists());
    assert!(
        finished.exists(),
        "the agent kept running until the copy was given up"
    );
}

fn run_a_shell_that_copies(directory: &Path) -> ! {
    let (events, receiver) = ui_channel().unwrap();
    let options = ShellOptions {
        version: "0.1.0".to_owned(),
        model: "model-a".to_owned(),
        permission_mode: PermissionMode::Auto,
        full_access_warning: false,
        workspace_label: "workspace".to_owned(),
        workspace_root: PathBuf::from("/workspace"),
        commands: Vec::new(),
        command_categories: Vec::new(),
        prompt_history: PromptHistory::disabled(),
        file_mentions: None,
    };
    let stopped = directory.join("stopped");
    let outcome = host(options, events, receiver, None, move |_, mut commands| {
        while commands.blocking_recv().is_some() {}
        fs::write(stopped, "").unwrap();
    });
    process::exit(i32::from(outcome.is_err()));
}
