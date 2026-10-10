use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};

use super::*;
use crate::session_codec::SessionMetadata;
use crate::session_event::{ContextCheckpointEvent, decode_conversation_frame};
use crate::session_log::read_metadata;

const REWRITTEN_GENERATION: &str = "05050505050505050505050505050505";
const REPLACEMENT_ID: &str = "07070707070707070707070707070707";
const RAW_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const PREFERENCES_007: &str =
    "{\"model\":\"openai/gpt-5\",\"effort\":\"auto\",\"fast_mode\":false,\"provider\":\"gateway\"}";

fn identity_007(fields: &[&[u8]]) -> Vec<u8> {
    let mut canonical = Vec::new();
    for field in fields {
        canonical.extend(u64::try_from(field.len()).unwrap().to_be_bytes());
        canonical.extend_from_slice(field);
    }
    canonical
}

fn durable_007(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => serde_json::to_string(text).unwrap(),
        Err(_) => format!(
            "{{\"encoding\":\"base64\",\"data\":\"{}\"}}",
            STANDARD.encode(bytes)
        ),
    }
}

fn permissions_007() -> String {
    let command = identity_007(&[
        b"fx-permission-state-v2",
        b"git status",
        b"/work",
        b"false",
        b"macos",
    ]);
    let digest = Sha256::digest(b"fn main() {}");
    let file = identity_007(&[
        b"fx-file-mutation-v1",
        b"/work/src/main.rs",
        digest.as_slice(),
        &42_u64.to_be_bytes(),
    ]);
    format!(
        "{{\"schema_version\":2,\"next_generation\":3,\"rules\":[{{\"id\":1,\"kind\":\"command\",\"canonical\":{},\"display_identity\":\"git status\",\"decision\":\"allow\",\"generation\":1}},{{\"id\":2,\"kind\":\"file_mutation\",\"canonical\":{},\"display_identity\":\"src/main.rs\",\"decision\":\"allow\",\"generation\":2}}]}}",
        durable_007(&command),
        durable_007(&file)
    )
}

fn reply_007(prompt: &str, answer: &str) -> String {
    format!(
        "{{\"kind\":\"assistant\",\"user\":{{\"text\":\"{prompt}\",\"images\":[]}},\"assistant\":\"{answer}\",\"execution\":{{\"schema_version\":4,\"tool_steps\":[],\"files\":[]}}}}"
    )
}

fn compacted_007(summary: &str, removed: usize, compactions: usize) -> String {
    format!(
        "{{\"kind\":\"compacted_summary\",\"summary\":\"{summary}\",\"removed_turn_count\":{removed},\"compaction_count\":{compactions}}}"
    )
}

fn state_007(
    id: &str,
    updated_at_ms: usize,
    history: &[String],
    context_history_start: usize,
    tail: &str,
) -> String {
    format!(
        "{{\"id\":\"{id}\",\"origin_workspace_root\":\"/work\",\"workspace_root\":\"/work\",\"created_at_ms\":10,\"updated_at_ms\":{updated_at_ms},\"conversation_language\":\"en\",\"preferences\":{PREFERENCES_007},\"history\":[{}],\"total_input_tokens\":0,\"total_output_tokens\":0,\"context_history_start\":{context_history_start},\"permission_state\":{}{tail}}}",
        history.join(","),
        permissions_007()
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    lowercase_hex(&Sha256::digest(bytes))
}

impl LegacyLog {
    fn started_007(id: &str) -> Self {
        Self::with_preferences(id, "/work", PREFERENCES_007)
    }

    fn started_at(mut self, timestamp_ms: usize) -> Self {
        self.frames[0].2 = Some(timestamp_ms);
        self
    }

    fn replaced(self, reason: &str, state: &str, timestamp_ms: usize) -> Self {
        let bytes = state.as_bytes();
        let chunks: Vec<&[u8]> = bytes.chunks(RAW_CHUNK_BYTES).collect();
        let digest = sha256_hex(bytes);
        let mut log = self.frame_at(
            "state_replacement_started",
            &format!(
                "{{\"replacement_id\":\"{REPLACEMENT_ID}\",\"reason\":\"{reason}\",\"encoded_bytes\":{},\"sha256\":\"{digest}\",\"chunk_count\":{}}}",
                bytes.len(),
                chunks.len()
            ),
            timestamp_ms,
        );
        for (index, chunk) in chunks.iter().enumerate() {
            log = log.frame_at(
                "state_replacement_chunk",
                &format!(
                    "{{\"replacement_id\":\"{REPLACEMENT_ID}\",\"chunk_index\":{index},\"raw_bytes\":{},\"chunk_sha256\":\"{}\",\"base64\":\"{}\"}}",
                    chunk.len(),
                    sha256_hex(chunk),
                    STANDARD.encode(chunk)
                ),
                timestamp_ms,
            );
        }
        log.frame_at(
            "state_replacement_committed",
            &format!(
                "{{\"replacement_id\":\"{REPLACEMENT_ID}\",\"encoded_bytes\":{},\"sha256\":\"{digest}\",\"chunk_count\":{}}}",
                bytes.len(),
                chunks.len()
            ),
            timestamp_ms,
        )
    }

    fn edited(mut self, frame: usize, edit: impl Fn(&str) -> String) -> Self {
        self.frames[frame].1 = edit(&self.frames[frame].1);
        self
    }
}

fn written(fixture: &Fixture, log: &LegacyLog) -> (Vec<ConversationEvent>, SessionMetadata) {
    let converted = read_schema_v3(&fixture.dir(log), &log.id).unwrap().unwrap();
    let copy = fixture.root.path().join("copies").join(&log.id);
    fs::create_dir_all(&copy).unwrap();
    for dir in [copy.parent().unwrap(), copy.as_path()] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let dir = PrivateDir::open_existing(&copy).unwrap().unwrap();
    converted.write(&dir).unwrap();
    let events = fs::read_to_string(copy.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| {
            decode_conversation_frame(format!("{line}\n").as_bytes())
                .unwrap()
                .event
        })
        .collect();
    (events, read_metadata(&dir, &log.id).unwrap())
}

fn checkpoints(events: &[ConversationEvent]) -> Vec<(usize, &ContextCheckpointEvent)> {
    events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            ConversationEvent::ContextCheckpoint(checkpoint) => Some((index + 1, checkpoint)),
            _ => None,
        })
        .collect()
}

fn prompts(events: &[ConversationEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            ConversationEvent::User(user) => Some(user.text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_compaction_committed_as_a_turn_covers_the_turns_it_removed() {
    let fixture = Fixture::new();
    let log = LegacyLog::started_007("legacy-compacted")
        .turn(&reply_007("one", "first"))
        .turn(&reply_007("two", "second"))
        .turn(&reply_007("three", "third"))
        .turn(&compacted_007("Earlier: one and two.", 2, 1))
        .turn(&reply_007("four", "fourth"));
    let summary = fixture.summary(&log).unwrap().unwrap();
    assert_eq!(summary.history_len, 5);

    let (events, metadata) = written(&fixture, &log);
    assert_eq!(prompts(&events), ["one", "two", "three", "four"]);
    let checkpoints = checkpoints(&events);
    assert_eq!(checkpoints.len(), 1);
    let (seq, checkpoint) = checkpoints[0];
    assert_eq!(seq, 10);
    assert_eq!(checkpoint.covers_through_seq, 6);
    assert_eq!(checkpoint.summary, "Earlier: one and two.");
    assert_eq!(metadata.title.as_deref(), Some("one"));
}

#[test]
fn a_summary_that_is_not_the_active_one_covers_nothing_new() {
    let fixture = Fixture::new();
    let log = LegacyLog::started_007("legacy-two-summaries")
        .turn(&reply_007("one", "first"))
        .turn(&compacted_007("Earlier: one.", 1, 1))
        .turn(&reply_007("two", "second"))
        .turn(&compacted_007("Earlier: one and two.", 2, 2))
        .turn(&reply_007("three", "third"));
    let (events, _) = written(&fixture, &log);
    let covered: Vec<u64> = checkpoints(&events)
        .iter()
        .map(|(_, checkpoint)| checkpoint.covers_through_seq)
        .collect();
    assert_eq!(covered, [0, 7]);

    let overreaching = LegacyLog::started_007("legacy-overreach")
        .turn(&reply_007("one", "first"))
        .turn(&compacted_007("Earlier: more than happened.", 3, 1));
    assert!(fixture.summary(&overreaching).is_err());
}

#[test]
fn a_compact_command_replacement_keeps_every_turn_as_upstream_imports_it() {
    let fixture = Fixture::new();
    let history = [reply_007("one", "first"), reply_007("two", "second")];
    let state = state_007(
        "legacy-replaced",
        77,
        &history,
        1,
        &format!(",\"usage\":{USAGE}"),
    )
    .replace(
        "\"effort\":\"auto\",\"fast_mode\":false",
        "\"effort\":\"high\",\"fast_mode\":true",
    );
    let log = LegacyLog::started_007("legacy-replaced")
        .turn(&reply_007("one", "first"))
        .turn(&reply_007("two", "second"))
        .replaced("compaction", &state, 77)
        .turn(&reply_007("three", "third"));
    let summary = fixture.summary(&log).unwrap().unwrap();
    assert_eq!(summary.history_len, 3);

    let (events, metadata) = written(&fixture, &log);
    assert_eq!(prompts(&events), ["one", "two", "three"]);
    assert!(checkpoints(&events).is_empty());
    assert_eq!(metadata.preferences.effort.label(), "high");
    assert!(metadata.preferences.fast_mode);
    assert_eq!(
        metadata.updated_at_ms,
        i64::try_from(log.frame_count() * 10).unwrap()
    );
}

#[test]
fn a_log_fx_rewrote_past_4096_frames_reads_from_its_new_generation() {
    let fixture = Fixture::new();
    let output = "o".repeat(3_000);
    let mut history: Vec<String> = (0..1_500)
        .map(|turn| command_turn_007(turn, &output))
        .collect();
    history.insert(
        1_000,
        compacted_007("Earlier: the first thousand turns.", 1_000, 1),
    );
    let state = state_007(
        "legacy-rewritten",
        900_000,
        &history,
        1_000,
        &format!(",\"usage\":{USAGE}"),
    );
    assert!(state.len() > RAW_CHUNK_BYTES);
    let log = LegacyLog::started_007("legacy-rewritten")
        .in_generation(REWRITTEN_GENERATION)
        .started_at(900_000)
        .replaced("log_compaction", &state, 900_000)
        .turn(&reply_007("after the rewrite", "still here"));
    assert_eq!(log.frame_count(), 8);
    let dir = log.write(fixture.root.path());
    fs::write(
        dir.join(format!("commit.{GENERATION}.json")),
        "{\"schema_version\":1,\"session_id\":\"legacy-rewritten\",\"log_generation\":\"01010101010101010101010101010101\",\"through_seq\":4096,\"through_event_id\":\"00000000000000000000000000001100\",\"through_event_log_bytes\":99999999}\n",
    )
    .unwrap();

    let summary = fixture.summary(&log).unwrap().unwrap();
    assert_eq!(summary.history_len, 1_502);
    let (events, _) = written(&fixture, &log);
    let prompts = prompts(&events);
    assert_eq!(prompts.len(), 1_501);
    assert_eq!(prompts[1_500], "after the rewrite");
    let checkpoints = checkpoints(&events);
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].1.covers_through_seq, 6_000);
    assert_eq!(checkpoints[0].0, 6_001);
}

fn broken_state(updated: usize, tail: &str) -> String {
    state_007(
        "legacy-broken",
        updated,
        &[reply_007("one", "first")],
        0,
        tail,
    )
}

fn broken(reason: &str, state: &str) -> LegacyLog {
    LegacyLog::started_007("legacy-broken")
        .turn(&reply_007("one", "first"))
        .replaced(reason, state, 50)
}

fn recovery_tail() -> String {
    format!(
        ",\"recovery_checkpoint\":{}",
        &REQUEST_CHECKPOINT[14..REQUEST_CHECKPOINT.len() - 1]
    )
}

fn assert_unreadable(cases: Vec<(&str, LegacyLog)>) {
    let fixture = Fixture::new();
    for (case, log) in cases {
        assert!(fixture.summary(&log).is_err(), "{case}");
    }
}

#[test]
fn a_replacement_whose_frames_do_not_add_up_makes_the_session_unreadable() {
    let fixture = Fixture::new();
    let good = || broken("compaction", &broken_state(50, ""));
    assert!(fixture.summary(&good()).unwrap().is_some());
    let rules = permissions_007();
    assert!(rules.contains("\\u0000"), "{rules}");
    assert!(rules.contains("\"encoding\":\"base64\""), "{rules}");
    assert_unreadable(vec![
        (
            "chunk digest",
            good().edited(5, |payload| zero_digest(payload, "chunk_sha256")),
        ),
        (
            "overall digest",
            good()
                .edited(4, |payload| zero_digest(payload, "sha256"))
                .edited(6, |payload| zero_digest(payload, "sha256")),
        ),
        (
            "commit size",
            good().edited(6, |payload| {
                payload.replace("\"encoded_bytes\":", "\"encoded_bytes\":1")
            }),
        ),
        (
            "chunk index",
            good().edited(5, |payload| {
                payload.replace("\"chunk_index\":0", "\"chunk_index\":1")
            }),
        ),
        (
            "reason",
            good().edited(4, |payload| payload.replace("compaction", "tidying")),
        ),
        ("cut by the watermark", good().committed_through(6)),
        (
            "workspace",
            broken(
                "compaction",
                &broken_state(50, "").replace(
                    "\"workspace_root\":\"/work\"",
                    "\"workspace_root\":\"/moved\"",
                ),
            ),
        ),
        ("updated", broken("compaction", &broken_state(40, ""))),
        (
            "log rewrite time",
            broken("log_compaction", &broken_state(50, "")),
        ),
    ]);
}

#[test]
fn a_replacement_whose_state_does_not_check_out_makes_the_session_unreadable() {
    assert_unreadable(vec![
        (
            "key order",
            broken("compaction", &context_last(&broken_state(50, ""))),
        ),
        (
            "start past history",
            broken(
                "compaction",
                &broken_state(50, "")
                    .replace("\"context_history_start\":0", "\"context_history_start\":2"),
            ),
        ),
        (
            "unknown key",
            broken("compaction", &broken_state(50, ",\"unknown\":true")),
        ),
        (
            "rule id",
            broken(
                "compaction",
                &broken_state(50, "").replace("\"id\":1,", "\"id\":0,"),
            ),
        ),
        (
            "rule decision",
            broken(
                "compaction",
                &broken_state(50, "").replace("\"allow\"", "\"maybe\""),
            ),
        ),
        (
            "rule generation",
            broken(
                "compaction",
                &broken_state(50, "").replace("\"next_generation\":3", "\"next_generation\":2"),
            ),
        ),
        (
            "child flag",
            broken("compaction", &broken_state(50, ",\"subagent_child\":false")),
        ),
        (
            "recovery left set",
            broken("compaction", &broken_state(50, &recovery_tail())),
        ),
    ]);
}

#[test]
fn a_replaced_state_keeps_its_child_flag_settles_its_checkpoint_and_reads_older_rules() {
    let fixture = Fixture::new();
    let child = broken("compaction", &broken_state(50, ",\"subagent_child\":true"));
    assert_eq!(fixture.summary(&child).unwrap(), None);
    let settled =
        broken("compaction", &broken_state(50, &recovery_tail())).turn(&reply_007("two", "second"));
    assert_eq!(fixture.summary(&settled).unwrap().unwrap().history_len, 2);
    let older_rules = broken(
        "compaction",
        &broken_state(50, "").replace(
            "\"schema_version\":2,\"next_generation\"",
            "\"schema_version\":1,\"next_generation\"",
        ),
    );
    assert!(fixture.summary(&older_rules).unwrap().is_some());
}

fn zero_digest(payload: &str, key: &str) -> String {
    let marker = format!("\"{key}\":\"");
    let start = payload.find(&marker).unwrap() + marker.len();
    format!(
        "{}{}{}",
        &payload[..start],
        "0".repeat(64),
        &payload[start + 64..]
    )
}

fn context_last(state: &str) -> String {
    let moved = state.replace(
        "\"context_history_start\":0,\"permission_state\":",
        "\"permission_state\":",
    );
    format!(
        "{},\"context_history_start\":0}}",
        &moved[..moved.len() - 1]
    )
}

fn command_turn_007(turn: usize, output: &str) -> String {
    let result = command_result(&format!("call_{turn}"), output, "null");
    let result = format!(
        "{},\"terminal_action_presentation\":null}}",
        &result[..result.len() - 1]
    );
    command_turn(
        &format!("prompt {turn}"),
        &format!("call_{turn}"),
        &result,
        "done",
    )
    .replace("\"schema_version\":3", "\"schema_version\":4")
}

#[test]
fn listing_and_importing_a_rewritten_log_leaves_fx_untouched() {
    let fixture = Fixture::new();
    let history = [
        reply_007("one", "first"),
        compacted_007("Earlier: one.", 1, 1),
    ];
    let log = LegacyLog::started_007("legacy-untouched")
        .in_generation(REWRITTEN_GENERATION)
        .started_at(70)
        .replaced(
            "log_compaction",
            &state_007("legacy-untouched", 70, &history, 1, ""),
            70,
        )
        .turn(&reply_007("two", "second"));
    let dir = fixture.dir(&log);
    let before = fs::read_dir(fixture.root.path().join("legacy-untouched"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let (events, _) = written(&fixture, &log);
    assert_eq!(prompts(&events), ["one", "two"]);
    assert_eq!(checkpoints(&events)[0].1.covers_through_seq, 3);
    summarize_schema_v3(&dir, "legacy-untouched").unwrap();
    let after = fs::read_dir(fixture.root.path().join("legacy-untouched"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(before, after);
}
