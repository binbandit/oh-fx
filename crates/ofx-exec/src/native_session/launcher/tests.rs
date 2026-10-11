use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Sender};

use super::*;

const NONCE: &str = "0123456789abcdef0123456789abcdef";
const LONG: Duration = Duration::from_secs(20);
const SHELL_PID: u32 = 4242;

fn status(raw: i32) -> ExitStatus {
    ExitStatus::from_raw(raw)
}

#[test]
fn only_the_bare_launcher_flag_is_a_launcher_invocation() {
    assert!(is_terminal_launcher_invocation(&[LAUNCHER_MODE.into()]));
    assert!(!is_terminal_launcher_invocation(&[]));
    assert!(!is_terminal_launcher_invocation(&[
        LAUNCHER_MODE.into(),
        "extra".into()
    ]));
    assert!(!is_terminal_launcher_invocation(&[
        "--fx-internal-terminal-launcher".into()
    ]));
}

#[test]
fn launcher_wait_status_classifies_terminal_results_before_stops() {
    assert_eq!(
        termination_frame(status(23 << 8)),
        (ControlKind::CommandExited, 23)
    );
    assert_eq!(
        termination_frame(status(Signal::TERM.as_raw())),
        (ControlKind::CommandSignal, 15)
    );
    assert_eq!(
        termination_frame(status(Signal::SEGV.as_raw() | 0x80)),
        (ControlKind::CommandSignal, 11)
    );
    let stopped = status((Signal::TTIN.as_raw() << 8) | 0x7f);
    assert_eq!(stopped.stopped_signal(), Some(Signal::TTIN.as_raw()));
    assert!(is_terminal_stop(Signal::TTIN.as_raw()));
    assert!(is_terminal_stop(Signal::TTOU.as_raw()));
    assert!(!is_terminal_stop(Signal::TSTP.as_raw()));
    assert_eq!(termination_frame(stopped), (ControlKind::InvalidTerm, 0));
}

struct Fixture<W> {
    _directory: tempfile::TempDir,
    server: ControlServer<W>,
}

impl<W> Fixture<W> {
    fn new(frames: W) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let paths = LauncherPaths {
            bootstrap: directory.path().join("bootstrap"),
            command: directory.path().join("command"),
            control: directory.path().join("control.sock"),
        };
        let files = Arc::new(PrivateFiles::default());
        files.write(&paths.bootstrap, b"bootstrap").unwrap();
        files.write(&paths.command, b"command").unwrap();
        let listener = UnixListener::bind(&paths.control).unwrap();
        listener.set_nonblocking(true).unwrap();
        files.track(&paths.control);
        Self {
            _directory: directory,
            server: ControlServer {
                listener,
                paths,
                files,
                nonce: NONCE.as_bytes().to_vec(),
                shell: Pid::from_raw(4242).unwrap(),
                phase: Phase::AwaitingShell,
                frames,
                terminal: None,
            },
        }
    }
}

fn frame(nonce: &str, kind: u8) -> [u8; MARKER_FRAME_LEN] {
    let mut frame = [0; MARKER_FRAME_LEN];
    frame[..NONCE_LEN].copy_from_slice(nonce.as_bytes());
    frame[NONCE_LEN] = kind;
    frame
}

fn control_frame(kind: ControlKind, value: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_control_frame(&mut bytes, kind, value).unwrap();
    bytes
}

#[test]
fn markers_need_the_nonce_and_arrive_in_order() {
    let mut fixture = Fixture::new(Vec::new());
    let server = &mut fixture.server;
    let watch = Watch::default();
    let wrong = frame(&"f".repeat(NONCE_LEN), MarkerKind::ShellReady as u8);
    assert_eq!(server.marker(&wrong), None);
    assert_eq!(server.marker(&frame(NONCE, 9)), None);
    let ready = frame(NONCE, MarkerKind::ShellReady as u8);
    assert_eq!(server.marker(&ready), Some(MarkerKind::ShellReady));
    assert!(!server.accept(MarkerKind::CommandStarted, &watch).unwrap());
    assert!(server.paths.command.exists());
    assert!(server.accept(MarkerKind::ShellReady, &watch).unwrap());
    assert_eq!(server.phase, Phase::ShellReady);
    assert!(!server.paths.bootstrap.exists());
    assert!(!server.accept(MarkerKind::ShellReady, &watch).unwrap());
    watch.update(|state| state.command_released = true);
    assert!(server.accept(MarkerKind::CommandStarted, &watch).unwrap());
    assert_eq!(server.phase, Phase::CommandStarted);
    assert!(!server.paths.command.exists());
    let mut expected = control_frame(ControlKind::ShellReady, SHELL_PID);
    expected.extend(control_frame(ControlKind::CommandStarted, SHELL_PID));
    assert_eq!(server.frames, expected);
}

#[test]
fn a_command_start_fails_when_the_owner_left_or_the_shell_ended() {
    for gone in [
        (|state: &mut WatchState| state.owner_gone = true) as fn(&mut WatchState),
        |state: &mut WatchState| state.done = true,
    ] {
        let mut fixture = Fixture::new(Vec::new());
        fixture.server.phase = Phase::ShellReady;
        let watch = Watch::default();
        watch.update(gone);
        assert!(
            fixture
                .server
                .accept(MarkerKind::CommandStarted, &watch)
                .is_err()
        );
        assert_eq!(fixture.server.phase, Phase::ShellReady);
    }
}

struct FrameSender(Sender<Vec<u8>>);

impl Write for FrameSender {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _ = self.0.send(bytes.to_vec());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_command_start_waiting_for_its_release_ends_when_the_shell_ends() {
    let (frames, written) = mpsc::channel();
    let mut fixture = Fixture::new(FrameSender(frames));
    fixture.server.phase = Phase::ShellReady;
    let watch = Arc::new(Watch::default());
    let (finished, outcome) = mpsc::channel();
    let waiting = {
        let watch = Arc::clone(&watch);
        thread::spawn(move || {
            let accepted = fixture.server.accept(MarkerKind::CommandStarted, &watch);
            let _ = finished.send(accepted.is_err());
        })
    };
    assert_eq!(
        written.recv_timeout(LONG).unwrap(),
        control_frame(ControlKind::CommandStarted, SHELL_PID)
    );
    watch.finish();
    assert!(outcome.recv_timeout(LONG).unwrap());
    waiting.join().unwrap();
}

#[test]
fn the_control_socket_acknowledges_only_accepted_markers() {
    let fixture = Fixture::new(Vec::new());
    let control = fixture.server.paths.control.clone();
    let Fixture {
        _directory: directory,
        server,
    } = fixture;
    let watch = Arc::new(Watch::default());
    watch.update(|state| state.command_released = true);
    let serving = {
        let watch = Arc::clone(&watch);
        thread::spawn(move || server.serve(&watch))
    };
    let send = |frame: [u8; MARKER_FRAME_LEN]| {
        let mut stream = UnixStream::connect(&control).unwrap();
        stream.write_all(&frame).unwrap();
        stream.set_read_timeout(Some(LONG)).unwrap();
        let mut ack = Vec::new();
        stream.read_to_end(&mut ack).unwrap();
        ack
    };
    let wrong = frame(&"f".repeat(NONCE_LEN), MarkerKind::ShellReady as u8);
    assert!(send(wrong).is_empty());
    let started = frame(NONCE, MarkerKind::CommandStarted as u8);
    assert!(send(started).is_empty());
    assert_eq!(
        send(frame(NONCE, MarkerKind::ShellReady as u8)),
        [MARKER_ACK]
    );
    assert_eq!(send(started), [MARKER_ACK]);
    assert_eq!(serving.join().unwrap().unwrap(), Phase::CommandStarted);
    assert!(!control.exists());
    drop(directory);
}

#[test]
fn private_files_are_owner_only_and_only_created_ones_are_removed() {
    let directory = tempfile::tempdir().unwrap();
    let existing = directory.path().join("existing");
    fs::write(&existing, "someone else's").unwrap();
    let created = directory.path().join("created");
    let files = PrivateFiles::default();
    assert!(files.write(&existing, b"bootstrap").is_err());
    files.write(&created, b"command").unwrap();
    let mode = fs::symlink_metadata(&created).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    files.remove(&existing);
    drop(files);
    assert_eq!(fs::read_to_string(&existing).unwrap(), "someone else's");
    assert!(!created.exists());
}
