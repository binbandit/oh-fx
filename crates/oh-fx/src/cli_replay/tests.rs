use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ofx_cli::ReplayArgs;
use ofx_vt::GridError;

use super::{Fault, Output, Status, replay};

const STDOUT: u8 = 1;
const RESIZE: u8 = 3;
const MARKER: u8 = 5;

#[derive(Default)]
struct Capture {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Output for Capture {
    fn stdout(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stdout.extend_from_slice(bytes);
        Ok(())
    }

    fn stderr(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stderr.extend_from_slice(bytes);
        Ok(())
    }
}

struct Workspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        Self { _dir: dir, root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn missing_path_of(&self, len: usize) -> PathBuf {
        let mut path = self.root.clone().into_os_string().into_string().unwrap();
        while len - path.len() > 2 {
            path.push_str("/a");
        }
        path.push_str(&"/b"[..len - path.len()]);
        PathBuf::from(path)
    }

    fn tape(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}

fn tape(cols: u16, rows: u16, version: &[u8], frames: &[(i32, u8, &[u8])]) -> Vec<u8> {
    let mut bytes = b"FXTP\x01".to_vec();
    bytes.extend_from_slice(&cols.to_le_bytes());
    bytes.extend_from_slice(&rows.to_le_bytes());
    bytes.extend_from_slice(&123_456_789_i64.to_le_bytes());
    let version = &version[..version.len().min(255)];
    bytes.push(u8::try_from(version.len()).unwrap());
    bytes.extend_from_slice(version);
    for (delta, kind, payload) in frames {
        bytes.extend_from_slice(&delta.to_le_bytes());
        bytes.push(*kind);
        bytes.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(payload);
    }
    bytes
}

fn resize(cols: u16, rows: u16) -> Vec<u8> {
    [cols.to_le_bytes(), rows.to_le_bytes()].concat()
}

fn args(path: &Path) -> ReplayArgs {
    ReplayArgs {
        path: path.into(),
        ..ReplayArgs::default()
    }
}

fn json(path: &Path) -> ReplayArgs {
    ReplayArgs {
        json: true,
        ..args(path)
    }
}

fn run(args: &ReplayArgs) -> (Result<Status, Fault>, Capture) {
    let mut capture = Capture::default();
    let result = replay(args, &mut capture);
    (result, capture)
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

#[test]
fn a_minimal_stdout_tape_replays_to_the_final_grid_snapshot() {
    let workspace = Workspace::new();
    let path = workspace.tape(
        "stdout.fxtape",
        &tape(5, 2, b"vtest", &[(0, STDOUT, b"hi")]),
    );
    let (result, capture) = run(&args(&path));
    assert!(matches!(result, Ok(Status::Replayed)));
    assert_eq!(text(&capture.stdout), "|hi   |\n|     |\n");
    assert_eq!(text(&capture.stderr), "");
}

#[test]
fn frames_mode_prints_non_marker_frame_snapshots_only() {
    let workspace = Workspace::new();
    let path = workspace.tape(
        "frames.fxtape",
        &tape(
            4,
            1,
            b"vtest",
            &[
                (3, MARKER, b"ignored"),
                (7, STDOUT, b"ok"),
                (-2, 2, b"typed"),
            ],
        ),
    );
    let (result, capture) = run(&ReplayArgs {
        frames: true,
        ..args(&path)
    });
    assert!(matches!(result, Ok(Status::Replayed)));
    assert_eq!(
        text(&capture.stdout),
        "\n--- frame 2 (stdout, +7ms) ---\n|ok  |\n\n--- frame 3 (stdin, +-2ms) ---\n|ok  |\n"
    );
}

#[test]
fn the_json_summary_reports_frame_resize_and_stdout_metadata() {
    let workspace = Workspace::new();
    let path = workspace.tape(
        "summary.fxtape",
        &tape(
            4,
            1,
            b"vtest",
            &[
                (1, STDOUT, b"ab"),
                (2, RESIZE, &resize(2, 2)),
                (4, RESIZE, b"\x02\x00"),
                (8, 4, b""),
            ],
        ),
    );
    let (result, capture) = run(&json(&path));
    assert!(matches!(result, Ok(Status::Replayed)));
    assert_eq!(
        text(&capture.stdout),
        "{\"cols\":4,\"rows\":1,\"epoch_ms\":123456789,\"version\":\"vtest\",\"frames\":[{\"delta_ms\":1,\"kind\":\"stdout\",\"len\":2},{\"delta_ms\":2,\"kind\":\"resize\",\"len\":4},{\"delta_ms\":4,\"kind\":\"resize\",\"len\":2},{\"delta_ms\":8,\"kind\":\"sigint\",\"len\":0}],\"frame_count\":4,\"resize_count\":1,\"stdout_bytes\":2}\n"
    );
    assert_eq!(text(&capture.stderr), "");
}

#[test]
fn json_output_escapes_metadata_strings_as_upstream_does() {
    let workspace = Workspace::new();
    let path = workspace.tape(
        "escape.fxtape",
        &tape(3, 1, b"v\"\\\n\t\x01\x08\x0c\r\x7f\xc3\xa9", &[]),
    );
    let (result, capture) = run(&json(&path));
    assert!(matches!(result, Ok(Status::Replayed)));
    assert_eq!(
        text(&capture.stdout),
        "{\"cols\":3,\"rows\":1,\"epoch_ms\":123456789,\"version\":\"v\\\"\\\\\\n\\t\\u0001\\b\\f\\r\u{7f}é\",\"frames\":[],\"frame_count\":0,\"resize_count\":0,\"stdout_bytes\":0}\n"
    );
    serde_json::from_slice::<serde_json::Value>(&capture.stdout).unwrap();
}

#[test]
fn json_output_writes_a_version_that_is_not_utf8_as_its_bytes() {
    let workspace = Workspace::new();
    let path = workspace.tape("bytes.fxtape", &tape(3, 1, b"v\xff", &[]));
    let (_, capture) = run(&json(&path));
    assert!(text(&capture.stdout).starts_with(
        "{\"cols\":3,\"rows\":1,\"epoch_ms\":123456789,\"version\":[118,255],\"frames\":[]"
    ));
}

#[test]
fn json_output_includes_unknown_frame_metadata_without_altering_the_grid() {
    let workspace = Workspace::new();
    let path = workspace.tape(
        "unknown-json.fxtape",
        &tape(3, 1, b"vtest", &[(5, 200, b"ignored")]),
    );
    let (result, capture) = run(&json(&path));
    assert!(matches!(result, Ok(Status::Replayed)));
    assert_eq!(
        text(&capture.stdout),
        "{\"cols\":3,\"rows\":1,\"epoch_ms\":123456789,\"version\":\"vtest\",\"frames\":[{\"delta_ms\":5,\"kind\":\"unknown\",\"len\":7}],\"frame_count\":1,\"resize_count\":0,\"stdout_bytes\":0}\n"
    );
}

#[test]
fn a_json_replay_recovers_an_incomplete_final_frame_through_stderr() {
    let workspace = Workspace::new();
    let mut bytes = tape(4, 1, b"vtest", &[(0, STDOUT, b"ok")]);
    bytes.extend_from_slice(&[1, 2, 3]);
    let path = workspace.tape("truncated-tail.fxtape", &bytes);
    let (result, capture) = run(&json(&path));
    assert!(matches!(result, Ok(Status::Replayed)));
    assert!(text(&capture.stdout).contains("\"frame_count\":1,"));
    assert_eq!(
        text(&capture.stderr),
        "oh-fx replay: ignored incomplete final tape frame\n"
    );
}

#[test]
fn an_incomplete_final_payload_still_prints_the_screen_so_far() {
    let workspace = Workspace::new();
    let mut bytes = tape(4, 1, b"vtest", &[(0, STDOUT, b"ok")]);
    bytes.extend_from_slice(&tape(0, 0, b"", &[(0, STDOUT, b"lost")])[18..24]);
    let path = workspace.tape("truncated-payload.fxtape", &bytes);
    let (result, capture) = run(&args(&path));
    assert!(matches!(result, Ok(Status::Replayed)));
    assert_eq!(text(&capture.stdout), "|ok  |\n");
    assert_eq!(
        text(&capture.stderr),
        "oh-fx replay: ignored incomplete final tape frame\n"
    );
}

#[test]
fn frames_mode_prints_unknown_frame_snapshots_without_trapping() {
    let workspace = Workspace::new();
    let path = workspace.tape(
        "unknown-frames.fxtape",
        &tape(3, 1, b"vtest", &[(8, 201, b"ignored")]),
    );
    let (result, capture) = run(&ReplayArgs {
        frames: true,
        ..args(&path)
    });
    assert!(matches!(result, Ok(Status::Replayed)));
    assert_eq!(
        text(&capture.stdout),
        "\n--- frame 1 (unknown, +8ms) ---\n|   |\n"
    );
}

#[test]
fn frames_and_json_print_the_frames_before_the_summary() {
    let workspace = Workspace::new();
    let path = workspace.tape("both.fxtape", &tape(2, 1, b"v", &[(1, STDOUT, b"a")]));
    let (_, capture) = run(&ReplayArgs {
        frames: true,
        ..json(&path)
    });
    assert_eq!(
        text(&capture.stdout),
        "\n--- frame 1 (stdout, +1ms) ---\n|a |\n{\"cols\":2,\"rows\":1,\"epoch_ms\":123456789,\"version\":\"v\",\"frames\":[{\"delta_ms\":1,\"kind\":\"stdout\",\"len\":1}],\"frame_count\":1,\"resize_count\":0,\"stdout_bytes\":1}\n"
    );
}

#[test]
fn a_golden_path_gets_the_final_snapshot_and_stdout_stays_empty() {
    let workspace = Workspace::new();
    let path = workspace.tape(
        "golden.fxtape",
        &tape(4, 1, b"vtest", &[(0, STDOUT, b"ok")]),
    );
    let golden = workspace.path("golden.txt");
    fs::write(&golden, "an older golden file that is longer").unwrap();
    let (result, capture) = run(&ReplayArgs {
        golden: Some(golden.clone().into()),
        ..args(&path)
    });
    assert!(matches!(result, Ok(Status::Replayed)));
    assert_eq!(text(&capture.stdout), "");
    assert_eq!(read(&golden), "|ok  |\n");
}

#[test]
fn a_frames_dir_gets_a_manifest_and_per_frame_artifacts() {
    let workspace = Workspace::new();
    let path = workspace.tape(
        "frames-dir.fxtape",
        &tape(
            5,
            2,
            b"vtest",
            &[
                (1, STDOUT, b"hello"),
                (2, MARKER, b"hello"),
                (3, RESIZE, &resize(6, 2)),
                (4, MARKER, b"absent"),
                (5, MARKER, b""),
            ],
        ),
    );
    let out = workspace.path("frames-dir-out/nested");
    let (result, capture) = run(&ReplayArgs {
        frames_dir: Some(out.clone().into()),
        ..args(&path)
    });
    assert!(matches!(result, Ok(Status::Replayed)));
    assert_eq!(text(&capture.stdout), "|hello |\n|      |\n");
    assert_eq!(
        read(&out.join("manifest.json")),
        "{\"cols\":5,\"rows\":2,\"epoch_ms\":123456789,\"version\":\"vtest\",\"frame_count\":5,\"resize_count\":1,\"stdout_bytes\":5,\"frames_dir\":\"frames\"}\n"
    );
    let frames = out.join("frames");
    assert_eq!(
        read(&frames.join("0001.json")),
        "{\"index\":1,\"delta_ms\":1,\"elapsed_ms\":1,\"kind\":\"stdout\",\"payload_len\":5,\"size\":{\"cols\":5,\"rows\":2},\"cursor\":{\"row\":1,\"col\":5,\"visible\":true},\"footer_candidates\":[],\"visible_markers\":[]}\n"
    );
    assert_eq!(read(&frames.join("0001.grid.txt")), "|hello|\n|     |\n");
    assert_eq!(
        read(&frames.join("0002.json")),
        "{\"index\":2,\"delta_ms\":2,\"elapsed_ms\":3,\"kind\":\"marker\",\"payload_len\":5,\"size\":{\"cols\":5,\"rows\":2},\"cursor\":{\"row\":1,\"col\":5,\"visible\":true},\"footer_candidates\":[],\"visible_markers\":[\"hello\"]}\n"
    );
    assert_eq!(
        read(&frames.join("0003.json")),
        "{\"index\":3,\"delta_ms\":3,\"elapsed_ms\":6,\"kind\":\"resize\",\"payload_len\":4,\"size\":{\"cols\":6,\"rows\":2},\"cursor\":{\"row\":1,\"col\":5,\"visible\":true},\"footer_candidates\":[],\"visible_markers\":[\"hello\"]}\n"
    );
    assert_eq!(read(&frames.join("0003.grid.txt")), "|hello |\n|      |\n");
    assert_eq!(
        read(&frames.join("0005.json")),
        "{\"index\":5,\"delta_ms\":5,\"elapsed_ms\":15,\"kind\":\"marker\",\"payload_len\":0,\"size\":{\"cols\":6,\"rows\":2},\"cursor\":{\"row\":1,\"col\":5,\"visible\":true},\"footer_candidates\":[],\"visible_markers\":[\"hello\"]}\n"
    );
}

#[test]
fn footer_candidates_name_input_rows_between_two_dividers() {
    let workspace = Workspace::new();
    let screen = "x\r\n────\r\n> a\r\n════\r\n[1] ❯\r\n━━━━\r\n❯ b".as_bytes();
    let path = workspace.tape("footer.fxtape", &tape(5, 7, b"v", &[(0, STDOUT, screen)]));
    let out = workspace.path("footer-out");
    let (result, _) = run(&ReplayArgs {
        frames_dir: Some(out.clone().into()),
        ..args(&path)
    });
    assert!(matches!(result, Ok(Status::Replayed)));
    let frame = read(&out.join("frames/0001.json"));
    assert!(
        frame.contains("\"footer_candidates\":[{\"top_divider\":2,\"input\":3,\"bottom_divider\":4},{\"top_divider\":4,\"input\":5,\"bottom_divider\":6}],"),
        "{frame}"
    );
}

#[test]
fn a_missing_tape_fails_with_the_open_error() {
    let workspace = Workspace::new();
    let path = workspace.path("missing.fxtape");
    let (result, capture) = run(&args(&path));
    assert!(matches!(result, Ok(Status::Failed)));
    assert_eq!(text(&capture.stdout), "");
    assert_eq!(
        text(&capture.stderr),
        format!(
            "oh-fx replay: cannot open {}: FileNotFound\n",
            path.display()
        )
    );
}

#[test]
fn json_failures_use_stdout_for_missing_files_and_malformed_tapes() {
    let workspace = Workspace::new();
    let missing = workspace.path("missing.fxtape");
    let (result, capture) = run(&json(&missing));
    assert!(matches!(result, Ok(Status::Failed)));
    assert_eq!(text(&capture.stderr), "");
    assert_eq!(
        text(&capture.stdout),
        format!(
            "{{\"kind\":\"replay\",\"error\":\"oh-fx replay: cannot open {}: FileNotFound\",\"code\":\"FileNotFound\"}}\n",
            missing.display()
        )
    );
    let malformed = workspace.tape("bad-json.fxtape", b"not a tape");
    let (result, capture) = run(&json(&malformed));
    assert!(matches!(result, Ok(Status::Failed)));
    assert_eq!(text(&capture.stderr), "");
    assert_eq!(
        text(&capture.stdout),
        "{\"kind\":\"replay\",\"error\":\"oh-fx replay: bad tape: TapeTooShort\",\"code\":\"TapeTooShort\"}\n"
    );
}

#[test]
fn a_malformed_tape_fails_with_the_parser_error() {
    let workspace = Workspace::new();
    for (bytes, error) in [
        (&b"not a tape"[..], "TapeTooShort"),
        (b"not a tape, though long enough", "BadTapeMagic"),
        (&tape(1, 1, b"version", &[])[..20], "TruncatedVersion"),
    ] {
        let path = workspace.tape("malformed.fxtape", bytes);
        let (result, capture) = run(&args(&path));
        assert!(matches!(result, Ok(Status::Failed)), "{error}");
        assert_eq!(
            text(&capture.stderr),
            format!("oh-fx replay: bad tape: {error}\n")
        );
    }
}

#[test]
fn a_directory_is_not_a_tape() {
    let workspace = Workspace::new();
    let (result, capture) = run(&args(&workspace.root));
    assert!(matches!(result, Ok(Status::Failed)));
    assert_eq!(
        text(&capture.stderr),
        format!(
            "oh-fx replay: cannot open {}: IsDir\n",
            workspace.root.display()
        )
    );
}

#[test]
fn a_message_longer_than_upstreams_buffer_falls_back_to_its_short_form() {
    let workspace = Workspace::new();
    let open_message_bytes = "oh-fx replay: cannot open : FileNotFound\n".len();
    let fits = workspace.missing_path_of(512 - open_message_bytes);
    let long = workspace.missing_path_of(513 - open_message_bytes);
    let (_, capture) = run(&args(&fits));
    assert!(text(&capture.stderr).ends_with(": FileNotFound\n"));
    let (result, capture) = run(&args(&long));
    assert!(matches!(result, Ok(Status::Failed)));
    assert_eq!(text(&capture.stderr), "oh-fx replay: open failed\n");
    let (_, capture) = run(&json(&long));
    assert_eq!(
        text(&capture.stdout),
        "{\"kind\":\"replay\",\"error\":\"oh-fx replay: open failed\",\"code\":\"FileNotFound\"}\n"
    );
}

#[test]
fn a_path_that_is_not_utf8_is_written_as_bytes() {
    use std::os::unix::ffi::OsStringExt;

    let workspace = Workspace::new();
    let mut raw = workspace.root.clone().into_os_string().into_vec();
    raw.extend_from_slice(b"/m\xff");
    let path = PathBuf::from(OsString::from_vec(raw.clone()));
    let (_, capture) = run(&args(&path));
    let mut expected = b"oh-fx replay: cannot open ".to_vec();
    expected.extend_from_slice(&raw);
    expected.extend_from_slice(b": FileNotFound\n");
    assert_eq!(capture.stderr, expected);
    let (_, capture) = run(&json(&path));
    assert!(text(&capture.stdout).starts_with("{\"kind\":\"replay\",\"error\":[111,104,45,"));
    assert!(text(&capture.stdout).ends_with(
        ",255,58,32,70,105,108,101,78,111,116,70,111,117,110,100],\"code\":\"FileNotFound\"}\n"
    ));
}

#[test]
fn frames_dir_failures_name_the_directory_and_the_error() {
    let workspace = Workspace::new();
    let path = workspace.tape("frames.fxtape", &tape(2, 1, b"v", &[(0, STDOUT, b"a")]));
    let file = workspace.tape("plain-file", b"");
    let (result, capture) = run(&ReplayArgs {
        frames_dir: Some(file.clone().into()),
        ..args(&path)
    });
    assert!(matches!(result, Ok(Status::Failed)));
    assert_eq!(
        text(&capture.stderr),
        format!(
            "oh-fx replay: cannot prepare frames dir {}: NotDir\n",
            file.display()
        )
    );

    let blocked = workspace.path("blocked");
    fs::create_dir_all(blocked.join("frames/0001.json")).unwrap();
    let (result, capture) = run(&ReplayArgs {
        frames_dir: Some(blocked.clone().into()),
        ..json(&path)
    });
    assert!(matches!(result, Ok(Status::Failed)));
    assert_eq!(
        text(&capture.stdout),
        format!(
            "{{\"kind\":\"replay\",\"error\":\"oh-fx replay: cannot write frame artifacts to {}: IsDir\",\"code\":\"IsDir\"}}\n",
            blocked.display()
        )
    );

    let no_manifest = workspace.path("no-manifest");
    fs::create_dir_all(no_manifest.join("manifest.json")).unwrap();
    let (result, capture) = run(&ReplayArgs {
        frames_dir: Some(no_manifest.clone().into()),
        ..args(&path)
    });
    assert!(matches!(result, Ok(Status::Failed)));
    assert_eq!(text(&capture.stdout), "");
    assert_eq!(
        text(&capture.stderr),
        format!(
            "oh-fx replay: cannot write frames manifest to {}: IsDir\n",
            no_manifest.display()
        )
    );
}

#[test]
fn a_golden_path_that_cannot_be_written_fails_after_the_summary() {
    let workspace = Workspace::new();
    let path = workspace.tape("golden.fxtape", &tape(2, 1, b"v", &[]));
    let (result, capture) = run(&ReplayArgs {
        golden: Some(workspace.root.clone().into()),
        ..json(&path)
    });
    assert!(matches!(result, Ok(Status::Failed)));
    assert_eq!(
        text(&capture.stdout),
        format!(
            "{{\"cols\":2,\"rows\":1,\"epoch_ms\":123456789,\"version\":\"v\",\"frames\":[],\"frame_count\":0,\"resize_count\":0,\"stdout_bytes\":0}}\n{{\"kind\":\"replay\",\"error\":\"oh-fx replay: cannot write {}: IsDir\",\"code\":\"IsDir\"}}\n",
            workspace.root.display()
        )
    );
}

#[test]
fn grid_errors_end_the_replay_unhandled() {
    let workspace = Workspace::new();
    for (bytes, error) in [
        (tape(0, 1, b"v", &[]), GridError::InvalidGridSize),
        (
            tape(2, 1, b"v", &[(0, RESIZE, &resize(0, 1))]),
            GridError::InvalidGridSize,
        ),
        (
            tape(
                2,
                1,
                b"v",
                &[
                    (0, STDOUT, b"\x1b[?2026h"),
                    (0, STDOUT, &vec![b'x'; (1 << 20) + 1]),
                ],
            ),
            GridError::SynchronizedUpdateTooLarge,
        ),
    ] {
        let path = workspace.tape("grid.fxtape", &bytes);
        let (result, _) = run(&args(&path));
        assert!(
            matches!(result, Err(Fault::Grid(found)) if found == error),
            "{error}"
        );
    }
}
