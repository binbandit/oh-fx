use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::*;

fn tape_path(directory: &tempfile::TempDir, name: &str) -> PathBuf {
    fs::canonicalize(directory.path()).unwrap().join(name)
}

fn create(path: &Path, cols: u16, rows: u16, version: &str) -> io::Result<TapeRecorder> {
    TapeRecorder::open(path, cols, rows, version, Placement::Replace, true)
}

fn header(cols: u16, rows: u16, epoch_ms: i64, version: &[u8]) -> Vec<u8> {
    let mut bytes = TAPE_MAGIC.to_vec();
    bytes.extend_from_slice(&cols.to_le_bytes());
    bytes.extend_from_slice(&rows.to_le_bytes());
    bytes.extend_from_slice(&epoch_ms.to_le_bytes());
    bytes.push(u8::try_from(version.len().min(255)).unwrap());
    bytes.extend_from_slice(version);
    bytes
}

fn frame(bytes: &mut Vec<u8>, delta_ms: i32, kind: u8, payload: &[u8]) {
    bytes.extend_from_slice(&delta_ms.to_le_bytes());
    bytes.push(kind);
    bytes.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(payload);
}

fn frames(bytes: &[u8]) -> Vec<(TapeKind, Vec<u8>)> {
    let mut parser = TapeParser::new(bytes).unwrap();
    let mut frames = Vec::new();
    while let Some(frame) = parser.next_frame().unwrap() {
        frames.push((frame.kind, frame.payload.to_vec()));
    }
    frames
}

fn environment(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    move |name| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }
}

#[test]
fn a_tape_header_round_trips() {
    let directory = tempfile::tempdir().unwrap();
    let path = tape_path(&directory, "header.fxtape");
    let recorder = create(&path, 120, 40, "1.2.3").unwrap();
    recorder.shutdown();
    let bytes = fs::read(&path).unwrap();
    let mut parser = TapeParser::new(&bytes).unwrap();
    assert_eq!(parser.header.cols, 120);
    assert_eq!(parser.header.rows, 40);
    assert!(parser.header.epoch_ms > 0);
    assert_eq!(parser.header.version, b"1.2.3");
    assert_eq!(parser.next_frame(), Ok(None));
}

#[test]
fn recorded_frames_parse_back_in_order() {
    let directory = tempfile::tempdir().unwrap();
    let path = tape_path(&directory, "frames.fxtape");
    let recorder = create(&path, 80, 24, "vtest").unwrap();
    recorder.record_stdout(b"hello\n");
    recorder.record_resize(60, 20);
    recorder.record_stdout(b"");
    recorder.record_stdout(b"world");
    recorder.shutdown();
    let mut resize = 60_u16.to_le_bytes().to_vec();
    resize.extend_from_slice(&20_u16.to_le_bytes());
    assert_eq!(
        frames(&fs::read(&path).unwrap()),
        [
            (TapeKind::Stdout, b"hello\n".to_vec()),
            (TapeKind::Resize, resize),
            (TapeKind::Stdout, b"world".to_vec()),
        ]
    );
}

#[test]
fn frame_deltas_count_milliseconds_since_the_previous_frame() {
    let directory = tempfile::tempdir().unwrap();
    let path = tape_path(&directory, "deltas.fxtape");
    let started = std::time::Instant::now();
    let recorder = create(&path, 80, 24, "v").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    recorder.record_stdout(b"later");
    recorder.record_stdout(b"soon");
    let elapsed_ms = i32::try_from(started.elapsed().as_millis()).unwrap();
    recorder.shutdown();
    let bytes = fs::read(&path).unwrap();
    let mut parser = TapeParser::new(&bytes).unwrap();
    let first = parser.next_frame().unwrap().unwrap();
    let second = parser.next_frame().unwrap().unwrap();
    assert!(first.delta_ms >= 30, "{}", first.delta_ms);
    assert!(second.delta_ms >= 0, "{}", second.delta_ms);
    assert!(
        first.delta_ms + second.delta_ms <= elapsed_ms + 1,
        "{} + {} after {elapsed_ms} ms",
        first.delta_ms,
        second.delta_ms
    );
}

#[test]
fn the_parser_rejects_bad_magic_and_short_input() {
    assert_eq!(
        TapeParser::new(b"NOTATAPEATALL0000000").err(),
        Some(TapeError::BadTapeMagic)
    );
    assert_eq!(
        TapeParser::new(&TAPE_MAGIC[..TAPE_MAGIC.len() - 1]).err(),
        Some(TapeError::TapeTooShort)
    );
    let mut truncated = header(80, 24, 1, b"");
    let last = truncated.len() - 1;
    truncated[last] = 4;
    truncated.extend_from_slice(b"ab");
    assert_eq!(
        TapeParser::new(&truncated).err(),
        Some(TapeError::TruncatedVersion)
    );
}

#[test]
fn the_parser_reports_a_truncated_frame_header_or_payload() {
    let mut short_header = header(80, 24, 1, b"v");
    short_header.extend_from_slice(&[1, 2, 3]);
    let mut parser = TapeParser::new(&short_header).unwrap();
    assert_eq!(parser.next_frame(), Err(TapeError::TruncatedFrameHeader));

    let mut short_payload = header(80, 24, 1, b"v");
    short_payload.extend_from_slice(&5_i32.to_le_bytes());
    short_payload.push(1);
    short_payload.extend_from_slice(&10_u32.to_le_bytes());
    short_payload.extend_from_slice(b"abc");
    let mut parser = TapeParser::new(&short_payload).unwrap();
    assert_eq!(parser.next_frame(), Err(TapeError::TruncatedFramePayload));
}

#[test]
fn the_parser_keeps_unknown_kinds_and_any_payload_length() {
    let mut bytes = header(80, 24, 1, b"v");
    frame(&mut bytes, 7, 200, b"x");
    frame(&mut bytes, 1, 3, b"x");
    let mut parser = TapeParser::new(&bytes).unwrap();
    let unknown = parser.next_frame().unwrap().unwrap();
    assert_eq!(unknown.kind, TapeKind::Unknown(200));
    assert_eq!(unknown.delta_ms, 7);
    assert_eq!(unknown.payload, b"x");
    let resize = parser.next_frame().unwrap().unwrap();
    assert_eq!(resize.kind, TapeKind::Resize);
    assert_eq!(resize.payload, b"x");
    assert_eq!(parser.next_frame(), Ok(None));
}

#[test]
fn kinds_are_named_as_upstream_names_them() {
    let names: Vec<&str> = [1, 2, 3, 4, 5, 0, 6, 255]
        .map(TapeKind::from_byte)
        .iter()
        .map(|kind| kind.name())
        .collect();
    assert_eq!(
        names,
        [
            "stdout", "stdin", "resize", "sigint", "marker", "unknown", "unknown", "unknown"
        ]
    );
}

#[test]
fn a_failed_frame_write_stops_the_capture() {
    let directory = tempfile::tempdir().unwrap();
    let path = tape_path(&directory, "write-failure.fxtape");
    fs::write(&path, header(80, 24, 1, b"v")).unwrap();
    let recorder = TapeRecorder {
        capture: Mutex::new(Capture::Active(ActiveTape {
            file: File::open(&path).unwrap(),
            path: path.clone(),
            last_ms: now_ms(),
            record_stdin: true,
            show_inline_notice: true,
        })),
    };
    recorder.record_stdout(b"after-close");
    assert_eq!(recorder.capture_status(), CaptureStatus::Failed);
    recorder.record_stdout(b"ignored");
    recorder.record_stdin(b"ignored");
    recorder.record_resize(10, 10);
    assert_eq!(recorder.capture_status(), CaptureStatus::Failed);
    assert_eq!(fs::read(&path).unwrap(), header(80, 24, 1, b"v"));
}

#[test]
fn a_closed_recorder_ignores_later_frames() {
    let directory = tempfile::tempdir().unwrap();
    let path = tape_path(&directory, "closed.fxtape");
    let recorder = create(&path, 80, 24, "v").unwrap();
    assert_eq!(
        recorder.capture_status(),
        CaptureStatus::Active {
            path: path.clone(),
            show_inline_notice: true,
        }
    );
    recorder.shutdown();
    recorder.shutdown();
    recorder.record_stdout(b"ignored");
    recorder.record_resize(10, 10);
    assert_eq!(recorder.capture_status(), CaptureStatus::Inactive);
    assert!(frames(&fs::read(&path).unwrap()).is_empty());
}

#[test]
fn input_is_left_out_unless_it_was_asked_for() {
    let directory = tempfile::tempdir().unwrap();
    let path = tape_path(&directory, "stdin-default.fxtape");
    let recorder = create(&path, 80, 24, "v").unwrap();
    recorder.record_stdin(b"typed");
    recorder.shutdown();
    assert!(frames(&fs::read(&path).unwrap()).is_empty());
}

#[test]
fn the_recording_policy_reads_the_environment_without_effects() {
    let cases: [(&[(&str, &str)], RecordingPolicy); 6] = [
        (
            &[],
            RecordingPolicy {
                destination: RecordingDestination::Inactive,
                record_stdin: false,
                strict_start: false,
                show_inline_notice: false,
            },
        ),
        (
            &[("OH_FX_DEBUG_RECORD", "1")],
            RecordingPolicy {
                destination: RecordingDestination::Automatic,
                record_stdin: false,
                strict_start: true,
                show_inline_notice: true,
            },
        ),
        (
            &[("OH_FX_DEBUG_RECORD", "0"), ("OH_FX_RECORD_INPUT", "1")],
            RecordingPolicy {
                destination: RecordingDestination::Inactive,
                record_stdin: false,
                strict_start: false,
                show_inline_notice: false,
            },
        ),
        (
            &[
                ("OH_FX_RECORD", " /tmp/recording.fxtape "),
                ("OH_FX_RECORD_INPUT", "true"),
                ("OH_FX_DEBUG_RECORD_SILENT_BANNER", "yes"),
            ],
            RecordingPolicy {
                destination: RecordingDestination::Explicit("/tmp/recording.fxtape".to_owned()),
                record_stdin: true,
                strict_start: false,
                show_inline_notice: false,
            },
        ),
        (
            &[
                ("OH_FX_DEBUG_RECORD", "ON"),
                ("OH_FX_RECORD", "/tmp/explicit.fxtape"),
                ("OH_FX_DEBUG_RECORD_SILENT_BANNER", "false"),
            ],
            RecordingPolicy {
                destination: RecordingDestination::Explicit("/tmp/explicit.fxtape".to_owned()),
                record_stdin: false,
                strict_start: true,
                show_inline_notice: true,
            },
        ),
        (
            &[
                ("OH_FX_RECORD", "/tmp/input-yes.fxtape"),
                ("OH_FX_RECORD_INPUT", "yes"),
            ],
            RecordingPolicy {
                destination: RecordingDestination::Explicit("/tmp/input-yes.fxtape".to_owned()),
                record_stdin: false,
                strict_start: false,
                show_inline_notice: true,
            },
        ),
    ];
    for (pairs, expected) in cases {
        assert_eq!(recording_policy(&environment(pairs)), expected, "{pairs:?}");
    }
    let blank = recording_policy(&environment(&[("OH_FX_RECORD", " \t")]));
    assert_eq!(blank.destination, RecordingDestination::Inactive);
}

fn started(pairs: &[(&str, &str)], state: Option<&Path>) -> Option<TapeRecorder> {
    TapeRecorder::start(&environment(pairs), state, 80, 24, "vtest").unwrap()
}

#[test]
fn a_debug_recording_creates_a_private_tape_in_the_state_directory() {
    let directory = tempfile::tempdir().unwrap();
    let state = fs::canonicalize(directory.path()).unwrap().join("state");
    let recorder = started(&[("OH_FX_DEBUG_RECORD", "1")], Some(&state)).unwrap();
    let CaptureStatus::Active {
        path,
        show_inline_notice,
    } = recorder.capture_status()
    else {
        panic!("the recording is active");
    };
    assert!(show_inline_notice);
    assert_eq!(path.parent(), Some(state.join("recordings").as_path()));
    let name = path.file_name().unwrap().to_str().unwrap();
    let stem = name
        .strip_prefix("oh-fx-record-")
        .and_then(|rest| rest.strip_suffix(".fxtape"))
        .unwrap();
    let (millis, random) = stem.split_once('-').unwrap();
    assert!(millis.parse::<i64>().unwrap() > 0);
    assert_eq!(random.len(), 12);
    assert!(random.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o077, 0);
    recorder.shutdown();
}

#[test]
fn a_debug_recording_without_a_state_directory_uses_the_temporary_directory() {
    let directory = tempfile::tempdir().unwrap();
    let temporary = fs::canonicalize(directory.path()).unwrap();
    let recorder = started(
        &[
            ("OH_FX_DEBUG_RECORD", "yes"),
            ("TMPDIR", temporary.to_str().unwrap()),
        ],
        None,
    )
    .unwrap();
    let CaptureStatus::Active { path, .. } = recorder.capture_status() else {
        panic!("the recording is active");
    };
    assert_eq!(
        path.parent(),
        Some(temporary.join("oh-fx-recordings").as_path())
    );
    recorder.shutdown();
}

#[test]
fn an_explicit_recording_that_cannot_start_is_skipped_unless_debugging() {
    let directory = tempfile::tempdir().unwrap();
    let blocked = tape_path(&directory, "file");
    fs::write(&blocked, "").unwrap();
    let inside = blocked.join("tape.fxtape");
    let inside = inside.to_str().unwrap();
    assert!(started(&[("OH_FX_RECORD", inside)], None).is_none());
    let strict = TapeRecorder::start(
        &environment(&[("OH_FX_RECORD", inside), ("OH_FX_DEBUG_RECORD", "1")]),
        None,
        80,
        24,
        "v",
    );
    assert!(strict.is_err());
    assert!(started(&[], None).is_none());
}

#[test]
fn input_is_recorded_for_the_accepted_values_only() {
    let directory = tempfile::tempdir().unwrap();
    for (index, value) in ["1", "TrUe", " ON ", "yes"].iter().enumerate() {
        let path = tape_path(&directory, &format!("stdin-{index}.fxtape"));
        let recorder = started(
            &[
                ("OH_FX_RECORD", path.to_str().unwrap()),
                ("OH_FX_RECORD_INPUT", value),
            ],
            None,
        )
        .unwrap();
        recorder.record_stdin(b"typed");
        recorder.shutdown();
        let expected = if *value == "yes" {
            Vec::new()
        } else {
            vec![(TapeKind::Stdin, b"typed".to_vec())]
        };
        assert_eq!(frames(&fs::read(&path).unwrap()), expected, "{value:?}");
    }
}

#[test]
fn an_explicit_tape_is_replaced_and_its_parent_created() {
    let directory = tempfile::tempdir().unwrap();
    let path = tape_path(&directory, "nested/deeper/tape.fxtape");
    let recorder = started(&[("OH_FX_RECORD", path.to_str().unwrap())], None).unwrap();
    recorder.record_stdout(b"first");
    recorder.shutdown();
    let recorder = started(&[("OH_FX_RECORD", path.to_str().unwrap())], None).unwrap();
    recorder.shutdown();
    assert!(frames(&fs::read(&path).unwrap()).is_empty());
}

#[test]
fn long_versions_keep_the_whole_tail_while_declaring_255_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = tape_path(&directory, "long-version.fxtape");
    let version = "v".repeat(300);
    let recorder = create(&path, 80, 24, &version).unwrap();
    recorder.shutdown();
    let bytes = fs::read(&path).unwrap();
    assert_eq!(bytes[HEADER_LEN - 1], 255);
    assert_eq!(bytes.len(), HEADER_LEN + version.len());
    assert_eq!(&bytes[HEADER_LEN..], version.as_bytes());
    let parser = TapeParser::new(&bytes).unwrap();
    assert_eq!(parser.header.version.len(), 255);
}
