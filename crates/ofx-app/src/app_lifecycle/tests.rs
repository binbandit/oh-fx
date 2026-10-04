use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::PoisonError;
use std::time::Instant;

use ofx_auth::ChatGptEndpoints;
use ofx_config::{PrivateDir, ProfilePaths, Settings};
use ofx_gateway::{CodexEndpoints, CodexModelsEndpoints};
use ofx_testkit::{FakeServer, PtySession, Reply, chat_text_events};
use ofx_tui::PromptHistory;
use ofx_upgrade::UpgradeControl;
use serde_json::{Value, json};

use super::*;
use crate::app_panic_runtime::HOOK_TESTS;
use crate::app_upgrade_runtime::{CheckOutcome, ReleaseCheck, Timing};

const CHILD_HOME: &str = "OH_FX_LIFECYCLE_CHILD_HOME";
const CHILD_AUTH: &str = "OH_FX_LIFECYCLE_CHILD_AUTH";
const CHILD_CODEX: &str = "OH_FX_LIFECYCLE_CHILD_CODEX";
const CHILD_CATALOG: &str = "OH_FX_LIFECYCLE_CHILD_CATALOG";
const CHILD_PANIC: &str = "OH_FX_LIFECYCLE_CHILD_PANIC";
const CHILD_CLIPBOARD: &str = "OH_FX_LIFECYCLE_CHILD_CLIPBOARD";
const CHILD_UPGRADE: &str = "OH_FX_LIFECYCLE_CHILD_UPGRADE";
const CLIPBOARD_TEST: &str =
    "app_lifecycle::tests::leaving_stops_the_agent_before_it_waits_for_a_copy_in_flight";
const UPGRADE_TEST: &str =
    "app_lifecycle::tests::a_signal_exit_restores_the_terminal_then_waits_out_an_admitted_install";
const SECOND_SIGNAL_TEST: &str =
    "app_lifecycle::tests::a_second_signal_during_the_install_wait_ends_the_process_at_once";
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
const SIGTERM: i32 = 15;
const INSTALL_HELD: Duration = Duration::from_millis(500);

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
    assert!(finished.finish(None, None, &panics).is_ok());
    let crashed = Worker::spawn(shell, || panic!("worker exploded")).unwrap();
    let message = crashed.finish(None, None, &panics).unwrap_err().to_string();
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
        grok: ofx_auth::GrokEndpoints::default(),
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
                web_fetch_progress: None,
                mode: None,
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
        persistence: None,
        opening: Opening::Welcome,
        ultrafast_requested: false,
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
    let outcome = host(
        shell_options(),
        events,
        receiver,
        None,
        None,
        None,
        |events, mut commands| {
            let _ = commands.blocking_recv();
            assert!(panic::catch_unwind(|| panic!("tool exploded")).is_err());
            events.send(UiEvent::Notice {
                notice: Notice::new(NoticeTone::Neutral, "", "still serving"),
            });
            let _ = commands.blocking_recv();
            panic!("worker exploded");
        },
    );
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
    let stopped = directory.join("stopped");
    let outcome = host(
        shell_options(),
        events,
        receiver,
        None,
        None,
        None,
        move |_, mut commands| {
            while commands.blocking_recv().is_some() {}
            fs::write(stopped, "").unwrap();
        },
    );
    process::exit(i32::from(outcome.is_err()));
}

const INSTALL_TEST: &str =
    "app_lifecycle::tests::quit_drains_accepted_installs_after_the_provider_worker_grace";
const CHILD_INSTALL: &str = "OH_FX_LIFECYCLE_CHILD_INSTALL";

#[test]
fn quit_drains_accepted_installs_after_the_provider_worker_grace() {
    if let Some(home) = env::var_os(CHILD_INSTALL) {
        run_a_local_install_session(Path::new(&home));
    }
    let home = tempfile::tempdir().unwrap();
    let home_path = fs::canonicalize(home.path()).unwrap();
    let paths = profile_paths(&home_path);
    let workspace = home_path.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let locks = paths.config.join(".skill-install-locks");
    fs::create_dir_all(&locks).unwrap();
    fs::set_permissions(&locks, fs::Permissions::from_mode(0o700)).unwrap();
    let mut held = Vec::new();
    for name in ["first-pack", "second-pack", "final-pack"] {
        let source = home_path.join(name);
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: install fixture\n---\nbody\n"),
        )
        .unwrap();
        if name != "final-pack" {
            let file = fs::File::create(locks.join(name)).unwrap();
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .unwrap();
            rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive).unwrap();
            held.push(file);
        }
    }
    let server = FakeServer::start([Reply::held_sse(&chat_text_events(&["STREAM_READY"])[..2])]);
    fs::write(paths.config.join("settings.json"), json!({
        "provider":"local", "providers":{"local":{"protocol":"openai-chat-completions", "base_url":server.base_url(), "auth":{"type":"none"}, "models":["model-a"]}}
    }).to_string()).unwrap();
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args([INSTALL_TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_INSTALL, &home_path)
        .env("TERM", "xterm-256color")
        .env("OH_FX_AUTO_UPGRADE", "0");
    let mut session = PtySession::spawn(command, 40, 160).unwrap();
    session
        .wait_for(WAIT, |screen| screen.contains(FIRST_FRAME))
        .unwrap();
    session.send(b"start\r");
    session
        .wait_for(WAIT, |screen| screen.contains("Generating"))
        .unwrap();
    for name in ["first-pack", "second-pack", "final-pack"] {
        session.send(format!("/skills install {}\r", home_path.join(name).display()).as_bytes());
    }
    session.send(b"/stats\r");
    session
        .wait_for(WAIT, |screen| screen.contains("ansi_bytes"))
        .unwrap();
    assert!(!paths.config.join("skills/final-pack").exists());
    session.send(b"/quit\r");
    let status = session
        .wait_exit(WAIT)
        .expect("quit drains accepted installs");
    assert!(status.success(), "{status:?}");
    assert!(session.cooked().unwrap());
    let root = paths.config.join("skills");
    assert!(
        root.join("final-pack/SKILL.md").is_file(),
        "quit abandoned an accepted installation"
    );
    assert!(!root.join("first-pack").exists());
    assert!(!root.join("second-pack").exists());
    assert!(
        fs::read_dir(&root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".skill-install-")),
        "quit left a staging transaction"
    );
    drop(held);
}

fn run_a_local_install_session(home: &Path) -> ! {
    let paths = profile_paths(home);
    let workspace = fs::canonicalize(home.join("workspace")).unwrap();
    let settings = Settings::load(&paths, &workspace).unwrap();
    let profile = Profile::new(workspace, Some(home.into()), Some(paths), settings).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let executions = ManagedExecutions::new(SessionSupervisor::new("/nonexistent"));
    let setup = runtime
        .block_on(profile.connect_interactive(
            Launch {
                model: None,
                mode: None,
                permission_mode: PermissionMode::Auto,
                system_prompt: None,
                reasoning_effort: None,
                fast_mode: None,
                context_limits: &[],
                command_timeout: None,
                executions: &executions,
                endpoints: SubscriptionEndpoints::default(),
                web_fetch_progress: None,
            },
            &CancellationToken::new(),
        ))
        .unwrap();
    let session = Session {
        profile,
        setup,
        executions,
        permission_mode: PermissionMode::Auto,
        persistence: None,
        opening: Opening::Welcome,
        ultrafast_requested: false,
    };
    process::exit(i32::from(run(session, None, runtime).is_err()));
}

#[test]
fn installation_shutdown_guards_release_after_failure_and_unwinding() {
    let _serial = HOOK_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
    let panics = PanicCapture::install(WORKER_THREAD, drop);
    let installations = Arc::new(Installations::default());
    let accepted = installations.start();
    let running = installations.start();
    drop(accepted);
    assert!(installations.wait_for_running(Duration::ZERO));
    drop(running);
    assert!(!installations.wait_for_running(Duration::ZERO));
    let guard = installations.start();
    let (shell, _events) = ui_channel().unwrap();
    let crashed = Worker::spawn(shell, move || {
        let _guard = guard;
        panic!("installation worker exploded");
    })
    .unwrap();
    assert!(crashed.finish(None, Some(&installations), &panics).is_err());
    assert!(!installations.wait_for_running(Duration::ZERO));
}

#[test]
fn unrelated_provider_work_keeps_the_existing_shutdown_grace() {
    let _serial = HOOK_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
    let panics = PanicCapture::install(WORKER_THREAD, drop);
    let installations = Installations::default();
    let (release, wait) = mpsc::channel();
    let (done, completed) = mpsc::channel();
    let (shell, _events) = ui_channel().unwrap();
    let worker = Worker::spawn(shell, move || {
        let _ = wait.recv();
        let _ = done.send(());
    })
    .unwrap();
    assert!(worker.finish(None, Some(&installations), &panics).is_ok());
    assert!(matches!(
        completed.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    release.send(()).unwrap();
    completed.recv_timeout(WAIT).unwrap();
}

fn shell_options() -> ShellOptions {
    ShellOptions {
        version: "0.1.0".to_owned(),
        model: "model-a".to_owned(),
        provider: "local".to_owned(),
        providers: Vec::new(),
        permission_mode: PermissionMode::Auto,
        full_access_warning: false,
        workspace_label: "workspace".to_owned(),
        startup_scrollback: true,
        commands: Vec::new(),
        command_categories: Vec::new(),
        prompt_history: PromptHistory::disabled(),
        file_mentions: None,
        skill_catalog: None,
        lifecycle: None,
        steering: None,
        opening: Opening::Welcome,
        statusline: ofx_contract::StatuslineToggles::default(),
        workspace_identity: None,
        theme: None,
    }
}

#[test]
fn a_signal_exit_restores_the_terminal_then_waits_out_an_admitted_install() {
    if let Some(directory) = env::var_os(CHILD_UPGRADE) {
        run_a_shell_while_an_install_is_held(Path::new(&directory));
    }
    let (directory, mut session) = signalled_during_a_held_install(UPGRADE_TEST);
    let installed = directory.path().join("installed");
    assert!(
        session.wait_exit(INSTALL_HELD).is_none(),
        "the signal ended the process during the install"
    );
    assert!(!installed.exists());
    fs::write(directory.path().join("released"), "").unwrap();
    let status = session
        .wait_exit(WAIT)
        .expect("the signal ends the process once the install settles");
    assert_eq!(status.signal(), Some(SIGTERM), "{status:?}");
    assert!(installed.exists());
}

#[test]
fn a_second_signal_during_the_install_wait_ends_the_process_at_once() {
    if let Some(directory) = env::var_os(CHILD_UPGRADE) {
        run_a_shell_while_an_install_is_held(Path::new(&directory));
    }
    let (directory, mut session) = signalled_during_a_held_install(SECOND_SIGNAL_TEST);
    assert!(
        session.wait_exit(INSTALL_HELD).is_none(),
        "the first signal ended the process during the install"
    );
    session.terminate().unwrap();
    let status = session
        .wait_exit(WAIT)
        .expect("a second signal ends the process without waiting");
    assert_eq!(status.signal(), Some(SIGTERM), "{status:?}");
    assert!(!directory.path().join("installed").exists());
    fs::write(directory.path().join("released"), "").unwrap();
}

fn signalled_during_a_held_install(test: &str) -> (tempfile::TempDir, PtySession) {
    let directory = tempfile::tempdir().unwrap();
    let installing = directory.path().join("installing");
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_UPGRADE, directory.path())
        .env("TERM", "xterm-256color");
    let session = PtySession::spawn(command, 24, 80).unwrap();
    session
        .wait_for(WAIT, |screen| screen.contains(FIRST_FRAME))
        .unwrap_or_else(|screen| panic!("the shell never started:\n{screen}"));
    wait_until(|| installing.exists());
    session.terminate().unwrap();
    wait_until(|| session.cooked().unwrap());
    (directory, session)
}

struct HeldInstall {
    directory: PathBuf,
}

impl ReleaseCheck for HeldInstall {
    fn check<'a>(
        &'a mut self,
        control: &'a UpgradeControl,
        _found: &'a mut (dyn FnMut(&str) + Send),
    ) -> BoxFuture<'a, CheckOutcome> {
        Box::pin(async move {
            let installed = control.install_unless_stopped(|| {
                fs::write(self.directory.join("installing"), "").unwrap();
                while !self.directory.join("released").exists() {
                    thread::sleep(Duration::from_millis(5));
                }
                fs::write(self.directory.join("installed"), "").unwrap();
                Ok(())
            });
            match installed {
                Ok(()) => CheckOutcome::Installed,
                Err(_) => CheckOutcome::Stopped,
            }
        })
    }
}

fn run_a_shell_while_an_install_is_held(directory: &Path) -> ! {
    let (events, receiver) = ui_channel().unwrap();
    let upgrader = SessionUpgrader::start(
        HeldInstall {
            directory: directory.to_owned(),
        },
        Timing {
            initial_delay: Duration::ZERO,
            interval: Duration::from_mins(10),
        },
        drop,
    )
    .unwrap();
    let outcome = host(
        shell_options(),
        events,
        receiver,
        None,
        None,
        Some(&upgrader),
        |_, mut commands| while commands.blocking_recv().is_some() {},
    );
    process::exit(i32::from(outcome.is_err()));
}
