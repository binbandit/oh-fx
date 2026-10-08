use std::fs::{self, File};
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const SIGPIPE: i32 = 13;

struct Workspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create a temporary workspace");
        let root = dir
            .path()
            .canonicalize()
            .expect("canonicalize the workspace");
        Self { _dir: dir, root }
    }

    fn tape(&self, cols: u16, rows: u16, screen: &[u8]) -> PathBuf {
        let mut bytes = b"FXTP\x01".to_vec();
        bytes.extend_from_slice(&cols.to_le_bytes());
        bytes.extend_from_slice(&rows.to_le_bytes());
        bytes.extend_from_slice(&1_i64.to_le_bytes());
        bytes.extend_from_slice(b"\x01v");
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.push(1);
        bytes.extend_from_slice(
            &u32::try_from(screen.len())
                .expect("a short screen")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(screen);
        let path = self.root.join("session.fxtape");
        fs::write(&path, bytes).expect("write the tape");
        path
    }

    fn replay(&self, args: &[&Path], stdout: Stdio) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .arg("replay")
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", &self.root)
            .env("OH_FX_AUTO_UPGRADE", "0")
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::piped())
            .output()
            .expect("run oh-fx replay")
    }
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("UTF-8 output")
}

#[test]
fn replay_prints_the_final_screen_of_a_tape() {
    let workspace = Workspace::new();
    let tape = workspace.tape(5, 2, b"hi\r\nthere");
    let output = workspace.replay(&[&tape], Stdio::piped());
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(text(&output.stdout), "|hi   |\n|there|\n");
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn replay_reports_a_missing_tape_on_stderr_or_as_json() {
    let workspace = Workspace::new();
    let missing = workspace.root.join("missing.fxtape");
    let output = workspace.replay(&[&missing], Stdio::piped());
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        format!(
            "oh-fx replay: cannot open {}: FileNotFound\n",
            missing.display()
        )
    );
    let output = workspace.replay(&[&missing, Path::new("--json")], Stdio::piped());
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stderr), "");
    assert_eq!(
        text(&output.stdout),
        format!(
            "{{\"kind\":\"replay\",\"error\":\"oh-fx replay: cannot open {}: FileNotFound\",\"code\":\"FileNotFound\"}}\n",
            missing.display()
        )
    );
}

#[test]
fn replay_names_a_terminal_grid_error_it_cannot_recover_from() {
    let workspace = Workspace::new();
    let tape = workspace.tape(0, 2, b"");
    let output = workspace.replay(&[&tape], Stdio::piped());
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(text(&output.stderr), "oh-fx: InvalidGridSize\n");
}

#[test]
fn replay_into_a_closed_pipe_dies_by_sigpipe() {
    let workspace = Workspace::new();
    let tape = workspace.tape(5, 2, b"hi");
    let (reader, writer) = io::pipe().expect("create a pipe");
    drop(reader);
    let output = workspace.replay(&[&tape], Stdio::from(writer));
    assert_eq!(output.status.signal(), Some(SIGPIPE));
}

#[test]
fn replay_discards_output_to_a_read_only_stdout_and_names_a_full_one() {
    let workspace = Workspace::new();
    let tape = workspace.tape(5, 2, b"hi");
    let read_only = File::open("/dev/null").expect("open /dev/null");
    let output = workspace.replay(&[&tape], Stdio::from(read_only));
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(text(&output.stderr), "");
    #[cfg(target_os = "linux")]
    {
        let full = File::create("/dev/full").expect("open /dev/full");
        let output = workspace.replay(&[&tape], Stdio::from(full));
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(text(&output.stderr), "oh-fx: NoSpaceLeft\n");
    }
}
