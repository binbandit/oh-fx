use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ofx_testkit::{FakeServer, PtySession, Reply, chat_text_events, chat_tool_call_events};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(15);

struct Socket {
    path: PathBuf,
    lines: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Socket {
    fn start(path: PathBuf) -> Self {
        let listener = UnixListener::bind(&path).expect("Herdr fixture operation succeeds");
        listener
            .set_nonblocking(true)
            .expect("Herdr fixture operation succeeds");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&lines);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket
                            .set_nonblocking(false)
                            .expect("Herdr fixture operation succeeds");
                        socket
                            .set_read_timeout(Some(WAIT))
                            .expect("Herdr fixture operation succeeds");
                        let mut line = Vec::new();
                        BufReader::new(
                            socket
                                .try_clone()
                                .expect("Herdr fixture operation succeeds"),
                        )
                        .read_until(b'\n', &mut line)
                        .expect("Herdr fixture operation succeeds");
                        captured
                            .lock()
                            .expect("Herdr fixture operation succeeds")
                            .push(
                                serde_json::from_slice(&line)
                                    .expect("Herdr fixture operation succeeds"),
                            );
                        socket
                            .write_all(b"{}\n")
                            .expect("Herdr fixture operation succeeds");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            }
        });
        Self {
            path,
            lines,
            stop,
            worker: Some(worker),
        }
    }

    fn wait(&self, count: usize) -> Vec<Value> {
        let started = Instant::now();
        loop {
            let lines = self
                .lines
                .lock()
                .expect("Herdr fixture operation succeeds")
                .clone();
            if lines.len() >= count {
                return lines;
            }
            assert!(started.elapsed() < WAIT, "received reports: {lines:?}");
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker
            .take()
            .expect("Herdr fixture operation succeeds")
            .join()
            .expect("Herdr fixture operation succeeds");
    }
}

#[test]
fn foreground_session_reports_startup_visible_attention_turns_and_release() {
    let root = tempfile::tempdir().expect("Herdr fixture operation succeeds");
    let home = fs::canonicalize(root.path()).expect("Herdr fixture operation succeeds");
    let workspace = home.join("workspace");
    let config = home.join("config/oh-fx");
    fs::create_dir_all(&workspace).expect("Herdr fixture operation succeeds");
    fs::create_dir_all(&config).expect("Herdr fixture operation succeeds");
    fs::write(home.join("notes.txt"), "outside\n").expect("Herdr fixture operation succeeds");
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "read-1",
            "read_file",
            r#"{"path":"../notes.txt"}"#,
        )),
        Reply::sse(&chat_text_events(&["Read complete."])),
        Reply::sse(&chat_tool_call_events(
            "question-1",
            "ask_user_question",
            r#"{"questions":[{"question":"Proceed?","options":[{"label":"Yes"},{"label":"No"}]}]}"#,
        )),
        Reply::sse(&chat_text_events(&["Question complete."])),
    ]);
    fs::write(config.join("settings.json"), json!({"provider":"local","providers":{"local":{"protocol":"openai-chat-completions","base_url":server.base_url(),"auth":{"type":"none"},"models":["model-a"]}}}).to_string()).expect("Herdr fixture operation succeeds");
    let socket = Socket::start(home.join("herdr.sock"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
    command
        .current_dir(&workspace)
        .env_clear()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm-256color")
        .env("OH_FX_AUTO_UPGRADE", "0")
        .env("HERDR_SOCKET_PATH", &socket.path)
        .env("HERDR_PANE_ID", "test-pane")
        .process_group(0);
    let mut session =
        PtySession::spawn(command, 30, 100).expect("Herdr fixture operation succeeds");
    session
        .wait_for(WAIT, |screen| screen.contains("Run /help for commands"))
        .expect("Herdr fixture operation succeeds");
    let first = socket.wait(1);
    assert_eq!(first[0]["method"], "pane.report_agent_session");
    let initial = socket.wait(4);
    let id = initial[0]["params"]["agent_session_id"]
        .as_str()
        .expect("Herdr fixture operation succeeds");
    assert!(
        home.join("data/oh-fx/sessions")
            .join(id)
            .join("session.json")
            .is_file()
    );
    assert_eq!(initial[1]["params"]["state"], "idle");
    assert_eq!(initial[2]["params"]["label"], "fx");
    assert_eq!(initial[3]["params"]["name"], "fx");
    session.send(b"read the notes\r");
    session
        .wait_for(WAIT, |screen| screen.contains("Permission needed"))
        .expect("Herdr fixture operation succeeds");
    assert_eq!(socket.wait(6)[5]["params"]["custom_status"], "permission");
    thread::sleep(Duration::from_millis(700));
    session.send(b"1");
    session
        .wait_for(WAIT, |screen| screen.contains("Read complete."))
        .expect("Herdr fixture operation succeeds");
    assert_eq!(socket.wait(7)[6]["params"]["state"], "idle");
    session.send(b"choose\r");
    session
        .wait_for(WAIT, |screen| screen.contains("Proceed?"))
        .expect("Herdr fixture operation succeeds");
    assert_eq!(socket.wait(9)[8]["params"]["custom_status"], "question");
    session.send(b"1");
    session
        .wait_for(WAIT, |screen| screen.contains("Question complete."))
        .expect("Herdr fixture operation succeeds");
    assert_eq!(socket.wait(10)[9]["params"]["state"], "idle");
    session.send(b"/exit\r");
    assert!(
        session
            .wait_exit(WAIT)
            .expect("Herdr fixture operation succeeds")
            .success()
    );
    let reports = socket.wait(13);
    assert_eq!(reports.len(), 13);
    assert_eq!(reports[4]["params"]["state"], "working");
    assert_eq!(reports[7]["params"]["state"], "working");
    assert_eq!(reports[10]["method"], "agent.rename");
    assert!(reports[10]["params"]["name"].is_null());
    assert_eq!(reports[11]["method"], "pane.clear_agent_authority");
    assert_eq!(reports[12]["method"], "pane.rename");
    assert!(reports[12]["params"]["label"].is_null());
}

#[test]
fn disabled_foreground_and_noninteractive_commands_do_not_contact_herdr() {
    let root = tempfile::tempdir().expect("Herdr fixture operation succeeds");
    let home = fs::canonicalize(root.path()).expect("Herdr fixture operation succeeds");
    let config = home.join("config/oh-fx");
    fs::create_dir_all(&config).expect("Herdr fixture operation succeeds");
    let server = FakeServer::start([]);
    fs::write(config.join("settings.json"), json!({"provider":"local","providers":{"local":{"protocol":"openai-chat-completions","base_url":server.base_url(),"auth":{"type":"none"},"models":["model-a"]}}}).to_string()).expect("Herdr fixture operation succeeds");
    let socket = Socket::start(home.join("herdr.sock"));
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .current_dir(&home)
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("XDG_CACHE_HOME", home.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .env("HERDR_SOCKET_PATH", &socket.path)
            .env("HERDR_PANE_ID", "test-pane");
        command
    };
    assert!(
        command()
            .arg("--version")
            .output()
            .expect("Herdr fixture operation succeeds")
            .status
            .success()
    );
    let mut disabled = command();
    disabled.env("OH_FX_HERDR", "FaLsE").process_group(0);
    let mut session =
        PtySession::spawn(disabled, 30, 100).expect("Herdr fixture operation succeeds");
    session
        .wait_for(WAIT, |screen| screen.contains("Run /help for commands"))
        .expect("Herdr fixture operation succeeds");
    session.send(b"/exit\r");
    assert!(
        session
            .wait_exit(WAIT)
            .expect("Herdr fixture operation succeeds")
            .success()
    );
    assert!(
        socket
            .lines
            .lock()
            .expect("Herdr fixture operation succeeds")
            .is_empty()
    );
}
