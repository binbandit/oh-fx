use super::*;
use crate::session_manager::log::check_line;
use crate::session_manager::schema::parse_line_body;
use crate::session_manager::test_support::{fixture_log, line};

fn fold(fork_seq: u64, lines: &[Vec<u8>]) -> State {
    let mut state = State::default();
    let mut offset = 0;
    for bytes in lines {
        let header = check_line(bytes).unwrap();
        let body = parse_line_body(header.kind.unwrap(), bytes).unwrap();
        state.apply(fork_seq, header.seq, offset, &body);
        offset += bytes.len() as u64;
    }
    state
}

fn spawned(seq: u64, child: &str, work: &str) -> Vec<u8> {
    line(
        seq,
        "child_spawned",
        &format!(",\"child\":\"{child}\",\"work_id\":\"{work}\",\"data\":{{\"agent\":null}}"),
    )
}

fn finished(seq: u64, child: &str, work: &str, outcome: &str) -> Vec<u8> {
    line(
        seq,
        "child_finished",
        &format!(",\"child\":\"{child}\",\"work_id\":\"{work}\",\"outcome\":\"{outcome}\""),
    )
}

#[test]
fn child_work_is_finished_once_and_only_after_its_spawn() {
    let state = fold(
        0,
        &[
            finished(1, "kid", "w1", "ok"),
            spawned(2, "kid", "w1"),
            finished(3, "kid", "w0", "failed"),
            finished(4, "kid", "w1", "cancelled"),
            finished(5, "kid", "w1", "ok"),
        ],
    );
    assert_eq!(state.children.len(), 1);
    let kid = &state.children[0];
    assert!(!kid.open);
    assert_eq!(kid.outcome, Some(Outcome::Cancelled));
    assert_eq!(kid.seq, 4);
    assert_eq!(kid.spawn_data.as_deref(), Some(&b"{\"agent\":null}"[..]));
    assert_eq!(kid.finish_data, None);
    assert_eq!(state.last_seq, 5);
}

#[test]
fn a_child_keeps_its_newest_work_outcome_and_data() {
    let state = fold(
        0,
        &[
            spawned(1, "kid", "w1"),
            line(
                2,
                "child_finished",
                ",\"child\":\"kid\",\"work_id\":\"w1\",\"outcome\":\"failed\",\"data\":{\"failure\":\"x\"}",
            ),
            spawned(3, "kid", "w2"),
            spawned(4, "other", "w9"),
        ],
    );
    assert_eq!(state.children.len(), 2);
    let kid = &state.children[0];
    assert_eq!(kid.work_id, "w2");
    assert!(kid.open);
    assert_eq!(kid.outcome, Some(Outcome::Failed));
    assert_eq!(
        kid.finish_data.as_deref(),
        Some(&b"{\"failure\":\"x\"}"[..])
    );
    assert_eq!(kid.seq, 3);
    assert_eq!(state.children[1].id, "other");
}

#[test]
fn a_fork_does_not_own_child_lines_from_before_its_fork_point() {
    let lines = [
        spawned(2, "old", "w1"),
        finished(3, "old", "w1", "ok"),
        spawned(4, "new", "w1"),
    ];
    let state = fold(3, &lines);
    assert_eq!(state.children.len(), 1);
    assert_eq!(state.children[0].id, "new");
}

#[test]
fn turns_settings_and_compactions_fold_as_upstream_does() {
    let state = fold(
        0,
        &[
            line(1, "turn_started", ",\"turn\":1"),
            line(2, "set", ",\"key\":\"title\",\"value\":null"),
            line(3, "compacted", ",\"turn\":1,\"data\":{}"),
            line(4, "turn_interrupted", ",\"turn\":1,\"reason\":\"crash\""),
            line(5, "turn_started", ",\"turn\":2"),
            line(6, "turn_committed", ",\"turn\":2"),
            line(7, "closed", ""),
        ],
    );
    assert_eq!(state.open_turn, None);
    assert_eq!(
        (state.last_turn, state.committed, state.interrupted),
        (2, 1, 1)
    );
    assert_eq!(state.last_interrupted, Some((1, Reason::Crash)));
    assert_eq!(
        state.settings[SetKey::Title as usize].as_deref(),
        Some(&b"null"[..])
    );
    assert_eq!(state.last_compaction_seq, Some(3));
    assert!(state.clean_exit);
}

#[test]
fn the_snapshot_fx_wrote_decodes_to_the_fold_of_the_lines_before_it() {
    let log = fixture_log("snapshot_small", "PsjMeU3Vl5Ld");
    let mut state = State::default();
    let mut offset = 0;
    let mut snapshots = 0;
    for bytes in log.split_inclusive(|&byte| byte == b'\n') {
        let header = check_line(bytes).unwrap();
        let body = parse_line_body(header.kind.unwrap(), bytes).unwrap();
        if let Body::Snapshot { state: raw } = &body {
            let decoded = State::decode(raw).unwrap().with_position_of(&state);
            assert_eq!(decoded, state);
            assert!(decoded.settings[SetKey::Usage as usize].is_some());
            snapshots += 1;
        }
        state.apply(0, header.seq, offset, &body);
        offset += bytes.len() as u64;
    }
    assert_eq!(snapshots, 1);
}

#[test]
fn a_snapshot_state_with_children_and_absent_settings_decodes() {
    let state = State::decode(
        br#"{"open_turn":3,"last_turn":3,"committed":1,"interrupted":1,"last_interrupted":{"turn":2,"reason":"cancel"},"prefs":null,"children":[{"id":"kid","work_id":"w","open":false,"seq":9,"outcome":"lost","finish_data":{"failure":"x"}}],"last_compaction_seq":null,"compaction_offset":12}"#,
    )
    .unwrap();
    assert_eq!(state.open_turn, Some(3));
    assert_eq!(state.last_interrupted, Some((2, Reason::Cancel)));
    assert_eq!(
        state.settings[SetKey::Prefs as usize].as_deref(),
        Some(&b"null"[..])
    );
    assert_eq!(state.settings[SetKey::Title as usize], None);
    assert_eq!(state.children[0].outcome, Some(Outcome::Lost));
    assert_eq!(state.compaction_offset, Some(12));
    for bad in [
        &br#"{"last_turn":1,"committed":1,"interrupted":0}"#[..],
        br#"{"last_turn":1,"committed":1,"interrupted":0,"children":[{"id":"k","work_id":"w","seq":1}]}"#,
        br#"{"last_turn":1,"committed":1,"interrupted":0,"last_interrupted":{"turn":1},"children":[]}"#,
    ] {
        assert_eq!(State::decode(bad), Err(BadBody));
    }
}
