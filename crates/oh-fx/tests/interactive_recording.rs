use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ofx_testkit::{FakeServer, PtySession, Reply, chat_text_events};
use ofx_workspace::{TapeKind, TapeParser};
use serde_json::json;

const WAIT: Duration = Duration::from_secs(15);
const NOTICE_TAIL: &str = "visible terminal content, including typed prompt text, is recorded";

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new(base_url: &str) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = directory
            .path()
            .canonicalize()
            .expect("canonicalize the home");
        let config = root.join("config/oh-fx");
        fs::create_dir_all(&config).expect("create the config directory");
        fs::create_dir_all(root.join("workspace")).expect("create the workspace");
        let settings = json!({
            "provider": "local",
            "providers": {
                "local": {
                    "protocol": "openai-chat-completions",
                    "base_url": base_url,
                    "auth": {"type": "none"},
                    "models": ["model-a"]
                }
            }
        });
        fs::write(config.join("settings.json"), settings.to_string()).expect("write settings.json");
        Self {
            _directory: directory,
            root,
        }
    }

    fn spawn(&self, environment: &[(&str, &Path)]) -> PtySession {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .current_dir(self.root.join("workspace"))
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .envs(environment.iter().copied())
            .process_group(0);
        PtySession::spawn(command, 24, 80).expect("spawn oh-fx in a pty")
    }

    fn shell(&self, environment: &[(&str, &Path)]) -> PtySession {
        let session = self.spawn(environment);
        wait(&session, "auto · model-a");
        session
    }
}

fn wait(session: &PtySession, needle: &str) -> String {
    session
        .wait_for(WAIT, |screen| screen.contains(needle))
        .unwrap_or_else(|screen| panic!("expected {needle:?} on screen:\n{screen}"))
}

fn quit(mut session: PtySession) {
    session.send(b"\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
}

struct Tape {
    cols: u16,
    rows: u16,
    version: Vec<u8>,
    frames: Vec<(TapeKind, Vec<u8>)>,
}

fn read_tape(path: &Path) -> Tape {
    let bytes = fs::read(path).expect("read the tape");
    let mut parser = TapeParser::new(&bytes).expect("a tape header");
    let mut frames = Vec::new();
    while let Ok(Some(frame)) = parser.next_frame() {
        frames.push((frame.kind, frame.payload.to_vec()));
    }
    Tape {
        cols: parser.header.cols,
        rows: parser.header.rows,
        version: parser.header.version.to_vec(),
        frames,
    }
}

fn payloads(tape: &Tape, kind: TapeKind) -> Vec<Vec<u8>> {
    tape.frames
        .iter()
        .filter(|(found, _)| *found == kind)
        .map(|(_, payload)| payload.clone())
        .collect()
}

fn replayed_rows(tape: &Path) -> Vec<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_oh-fx"))
        .arg("replay")
        .arg(tape)
        .env_clear()
        .env("OH_FX_AUTO_UPGRADE", "0")
        .output()
        .expect("run oh-fx replay");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout)
        .expect("a UTF-8 screen")
        .lines()
        .map(|row| {
            row.strip_prefix('|')
                .and_then(|row| row.strip_suffix('|'))
                .expect("a framed row")
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn replays_the_screen(session: &PtySession, tape: &Path) {
    session
        .wait_for(WAIT, |_| replayed_rows(tape) == session.screen_rows())
        .unwrap_or_else(|screen| {
            panic!(
                "the tape replays\n{}\ninstead of\n{screen}",
                replayed_rows(tape).join("\n")
            )
        });
}

#[test]
fn a_recorded_session_replays_to_the_screen_the_terminal_showed() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Recorded reply."]))]);
    let home = Home::new(&server.base_url());
    let tape = home.root.join("tapes/session.fxtape");
    let session = home.shell(&[
        ("OH_FX_RECORD", &tape),
        ("OH_FX_RECORD_INPUT", Path::new(" On ")),
    ]);
    wait(&session, NOTICE_TAIL);
    let screen = wait(&session, "recording: visual terminal capture:");
    assert!(screen.contains("session.fxtape"), "{screen}");
    session.send(b"remember the tape\r");
    wait(&session, "Recorded reply.");
    replays_the_screen(&session, &tape);
    session.resize(30, 100).expect("resize the pty");
    replays_the_screen(&session, &tape);
    quit(session);

    let recorded = read_tape(&tape);
    assert_eq!((recorded.cols, recorded.rows), (80, 24));
    assert_eq!(recorded.version, ofx_upgrade::VERSION.as_bytes());
    assert_eq!(payloads(&recorded, TapeKind::Resize), [[100, 0, 30, 0]]);
    let stdin = payloads(&recorded, TapeKind::Stdin).concat();
    assert!(
        stdin
            .windows(b"remember the tape".len())
            .any(|window| window == b"remember the tape"),
        "{stdin:?}"
    );
}

#[test]
fn input_typed_while_the_startup_probes_wait_is_recorded_once() {
    let server = FakeServer::start([]);
    let home = Home::new(&server.base_url());
    let tape = home.root.join("startup.fxtape");
    let mut session = home.spawn(&[
        ("OH_FX_RECORD", &tape),
        ("OH_FX_RECORD_INPUT", Path::new("1")),
    ]);
    session.send(b"early");
    wait(&session, "auto · model-a");
    session.send(b"late");
    wait(&session, "earlylate");
    session.send(b"\x15\x04");
    assert!(session.wait_exit(WAIT).expect("ctrl+d exits").success());
    let stdin = payloads(&read_tape(&tape), TapeKind::Stdin).concat();
    assert!(stdin.starts_with(b"early"), "{stdin:?}");
    let early = stdin
        .windows(b"early".len())
        .filter(|window| *window == b"early");
    assert_eq!(early.count(), 1, "{stdin:?}");
    assert!(
        stdin.windows(b"late".len()).any(|window| window == b"late"),
        "{stdin:?}"
    );
}

#[test]
fn input_is_left_out_of_a_recording_unless_it_is_asked_for() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Noted."]))]);
    let home = Home::new(&server.base_url());
    let tape = home.root.join("session.fxtape");
    let session = home.shell(&[("OH_FX_RECORD", &tape)]);
    session.send(b"private words\r");
    wait(&session, "Noted.");
    quit(session);
    let recorded = read_tape(&tape);
    assert!(payloads(&recorded, TapeKind::Stdin).is_empty());
    assert!(!payloads(&recorded, TapeKind::Stdout).is_empty());
}

#[test]
fn a_silent_debug_recording_keeps_a_private_tape_without_a_notice() {
    let server = FakeServer::start([]);
    let home = Home::new(&server.base_url());
    let session = home.shell(&[
        ("OH_FX_DEBUG_RECORD", Path::new("yes")),
        ("OH_FX_DEBUG_RECORD_SILENT_BANNER", Path::new("1")),
    ]);
    let screen = session.screen();
    assert!(!screen.contains("visual terminal capture"), "{screen}");
    quit(session);
    let recordings = home.root.join("state/oh-fx/recordings");
    let tapes: Vec<PathBuf> = fs::read_dir(&recordings)
        .expect("list the recordings")
        .map(|entry| entry.expect("a recording").path())
        .collect();
    assert_eq!(tapes.len(), 1, "{tapes:?}");
    let name = tapes[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("oh-fx-record-") && name.ends_with(".fxtape"),
        "{name}"
    );
    let mode = fs::metadata(&tapes[0]).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let recorded = read_tape(&tapes[0]);
    assert_eq!((recorded.cols, recorded.rows), (80, 24));
}

#[test]
fn a_debug_recording_that_cannot_start_ends_the_launch() {
    let server = FakeServer::start([]);
    let home = Home::new(&server.base_url());
    let blocked = home.root.join("blocked");
    fs::write(&blocked, "").expect("write a file where a directory belongs");
    let mut session = home.spawn(&[
        ("OH_FX_DEBUG_RECORD", Path::new("1")),
        ("XDG_STATE_HOME", &blocked),
    ]);
    wait(&session, "oh-fx: unable to start terminal recording.");
    let status = session.wait_exit(WAIT).expect("the launch ends");
    assert_eq!(status.code(), Some(1));
    assert!(session.cooked().expect("read the terminal modes"));
}

#[test]
fn an_explicit_recording_that_cannot_start_is_skipped() {
    let server = FakeServer::start([]);
    let home = Home::new(&server.base_url());
    let blocked = home.root.join("blocked");
    fs::write(&blocked, "").expect("write a file where a directory belongs");
    let session = home.shell(&[("OH_FX_RECORD", &blocked.join("session.fxtape"))]);
    let screen = session.screen();
    assert!(!screen.contains("visual terminal capture"), "{screen}");
    quit(session);
}
