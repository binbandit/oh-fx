use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process;

use ofx_config::PrivateDir;
use sha2::{Digest, Sha256};

use super::*;
use crate::session_manager::test_support::line;

const ID: &str = "replayed0001";
const USER: &str = ",\"type\":\"user\",\"data\":{\"text\":\"Hi\",\"images\":[],\"work_id\":null}";
const TURN_END: &str = ",\"type\":\"turn_end\",\"data\":{\"files\":[],\"turn_summary\":null}";

fn assistant(text: &str) -> String {
    format!(
        ",\"type\":\"assistant\",\"data\":{{\"text\":\"{text}\",\"provider_replay\":null,\"standalone_response\":false}}"
    )
}

fn item(turn: u64, fields: &str) -> String {
    format!(",\"turn\":{turn}{fields}")
}

struct Log {
    directory: tempfile::TempDir,
    lines: Vec<(String, String)>,
}

impl Log {
    fn new() -> Self {
        let log = Self {
            directory: tempfile::tempdir().unwrap(),
            lines: Vec::new(),
        };
        log.push_created()
    }

    fn push_created(mut self) -> Self {
        self.lines.push((
            "session_created".to_owned(),
            format!(",\"id\":\"{ID}\",\"workspace\":\"/w\",\"role\":\"root\",\"host\":\"ask\""),
        ));
        self
    }

    fn push(&mut self, kind: &str, fields: &str) -> &mut Self {
        self.lines.push((kind.to_owned(), fields.to_owned()));
        self
    }

    fn blob(&self, bytes: &[u8]) -> String {
        let hash = ofx_text::lowercase_hex(&Sha256::digest(bytes));
        let blobs = self.directory.path().join(ID).join("blobs");
        fs::create_dir_all(&blobs).unwrap();
        fs::write(blobs.join(&hash), bytes).unwrap();
        hash
    }

    fn turns(&self) -> Result<Vec<ArchivedTurn>, SessionError> {
        let session = self.directory.path().join(ID);
        fs::create_dir_all(&session).unwrap();
        let bytes: Vec<u8> = self
            .lines
            .iter()
            .enumerate()
            .flat_map(|(index, (kind, fields))| line(index as u64 + 1, kind, fields))
            .collect();
        fs::write(session.join("log.jsonl"), bytes).unwrap();
        let root = PrivateDir::open_existing(self.directory.path())
            .unwrap()
            .unwrap();
        detail_turns(&Manager::new(root), ID).map_err(|error| match error {
            ReplayError::Open(_) => SessionError::SessionNotFound,
            ReplayError::Session(error) => error,
        })
    }
}

fn replied(turn: &ArchivedTurn) -> Option<&str> {
    match turn {
        ArchivedTurn::Replied { assistant, .. } => Some(assistant),
        _ => None,
    }
}

#[test]
fn a_superseded_turn_is_dropped_and_its_rewrite_kept() {
    let mut log = Log::new();
    log.push("turn_started", ",\"turn\":1")
        .push("item", &item(1, USER))
        .push("item", &item(1, &assistant("stale")))
        .push("item", &item(1, ",\"type\":\"superseded\",\"data\":{}"))
        .push("turn_interrupted", ",\"turn\":1,\"reason\":\"failed\"")
        .push("turn_started", ",\"turn\":2")
        .push("item", &item(2, USER))
        .push("item", &item(2, &assistant("fresh")))
        .push("item", &item(2, TURN_END))
        .push("turn_committed", ",\"turn\":2");
    let turns = log.turns().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(replied(&turns[0]), Some("fresh"));
}

#[test]
fn a_turn_the_manager_or_a_host_ended_comes_back_interrupted() {
    for reason in ["crash", "closed", "cancel", "failed"] {
        let mut log = Log::new();
        log.push("turn_started", ",\"turn\":1")
            .push("item", &item(1, USER))
            .push(
                "turn_interrupted",
                &format!(",\"turn\":1,\"reason\":\"{reason}\""),
            );
        let turns = log.turns().unwrap();
        assert!(
            matches!(&turns[..], [ArchivedTurn::Interrupted { user, .. }] if user == "Hi"),
            "{reason}: {turns:?}"
        );
    }
}

#[test]
fn a_compaction_summary_sits_where_its_line_does() {
    let mut log = Log::new();
    log.push("turn_started", ",\"turn\":1")
        .push("item", &item(1, USER))
        .push("item", &item(1, &assistant("one")))
        .push("item", &item(1, TURN_END))
        .push("turn_committed", ",\"turn\":1")
        .push(
            "compacted",
            ",\"data\":{\"summary\":\"S\",\"removed_turn_count\":1,\"compaction_count\":1,\"keep_from_turn\":null}",
        );
    let turns = log.turns().unwrap();
    assert_eq!(
        turns[1],
        ArchivedTurn::Compacted(crate::session_log::CompactedHistory {
            summary: "S".to_owned(),
            removed_turn_count: 1,
            compaction_count: 1,
        })
    );
    let mut bad = Log::new();
    bad.push(
        "compacted",
        ",\"data\":{\"summary\":\"S\",\"removed_turn_count\":1}",
    );
    assert_eq!(bad.turns().err(), Some(SessionError::InvalidSessionFormat));
}

#[test]
fn a_piece_kept_as_a_blob_comes_back_whole_and_a_lost_one_is_damage() {
    let mut log = Log::new();
    let hash =
        log.blob(br#"{"text":"from the blob","provider_replay":null,"standalone_response":false}"#);
    log.push("turn_started", ",\"turn\":1")
        .push("item", &item(1, USER))
        .push(
            "item",
            &item(
                1,
                &format!(",\"type\":\"assistant\",\"data\":{{\"$blob\":\"{hash}\"}},\"blobs\":[\"{hash}\"]"),
            ),
        )
        .push("item", &item(1, TURN_END))
        .push("turn_committed", ",\"turn\":1");
    assert_eq!(replied(&log.turns().unwrap()[0]), Some("from the blob"));
    let blob = log.directory.path().join(ID).join("blobs").join(&hash);
    fs::set_permissions(&blob, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(blob).unwrap();
    assert_eq!(log.turns().err(), Some(SessionError::InvalidSessionFormat));
}

#[test]
fn unknown_items_and_an_unfinished_turn_are_left_out() {
    let mut log = Log::new();
    log.push("turn_started", ",\"turn\":1")
        .push("item", &item(1, USER))
        .push("item", &item(1, ",\"type\":\"future_piece\",\"data\":{}"))
        .push("item", &item(1, &assistant("kept")))
        .push("item", &item(1, TURN_END))
        .push("turn_committed", ",\"turn\":1")
        .push("turn_started", ",\"turn\":2")
        .push("item", &item(2, USER));
    let turns = log.turns().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(replied(&turns[0]), Some("kept"));
}

const TRACE_CHILD: &str = "OH_FX_SESSIONS_V2_REPLAY_TRACE_CHILD";
const TRACE_TEST: &str = "session_adapter::replay::tests::replays_write_upstream_trace_lines";

#[test]
fn replays_write_upstream_trace_lines() {
    if let Some(root) = std::env::var_os(TRACE_CHILD) {
        ofx_trace::configure_from_env(Path::new(&root), None);
        let mut log = Log::new();
        let hash = "0".repeat(64);
        log.push("turn_started", ",\"turn\":1")
            .push("item", &item(1, USER))
            .push("item", &item(1, ",\"type\":\"future_piece\",\"data\":{}"))
            .push("item", &item(1, ",\"type\":\"superseded\",\"data\":{}"))
            .push("turn_interrupted", ",\"turn\":1,\"reason\":\"failed\"")
            .push("turn_started", ",\"turn\":2")
            .push("item", &item(2, USER))
            .push(
                "item",
                &item(
                    2,
                    ",\"type\":\"tool_running\",\"data\":{\"call_id\":\"c1\",\"tool_name\":\"shell\",\"arguments_json\":\"{}\"}",
                ),
            )
            .push("turn_interrupted", ",\"turn\":2,\"reason\":\"crash\"")
            .push("turn_started", ",\"turn\":3")
            .push("item", &item(3, USER));
        log.turns().unwrap();
        let mut lost = Log::new();
        lost.push("turn_started", ",\"turn\":1")
            .push("item", &item(1, USER))
            .push(
                "item",
                &item(
                    1,
                    &format!(",\"type\":\"assistant\",\"data\":{{\"$blob\":\"{hash}\"}},\"blobs\":[\"{hash}\"]"),
                ),
            );
        assert_eq!(lost.turns().err(), Some(SessionError::InvalidSessionFormat));
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("trace.log");
    let status = process::Command::new(std::env::current_exe().unwrap())
        .args([TRACE_TEST, "--exact", "--test-threads=1"])
        .env_clear()
        .env(TRACE_CHILD, root.path())
        .env("OH_FX_TRACE_LOG", &log)
        .stdout(process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "{status:?}");
    let written = fs::read_to_string(&log).unwrap();
    for expected in [
        format!(
            " [session] event=sessions_v2_unknown_item session={ID} type=future_piece dropped=item\n"
        ),
        format!(
            " [session] event=sessions_v2_replay_superseded session={ID} turn=1 dropped=stale_turn\n"
        ),
        format!(
            " [session] event=sessions_v2_replay_unfinished_tools session={ID} turn=2 calls=1\n"
        ),
        format!(
            " [session] event=sessions_v2_replay_open_turn session={ID} dropped=unfinished_pieces\n"
        ),
        format!(" [session] event=sessions_v2_blob_unreadable session={ID} err=NotFound\n"),
    ] {
        assert!(written.contains(&expected), "{expected}{written}");
    }
}
