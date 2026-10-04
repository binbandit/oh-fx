use std::os::unix::fs::FileExt;

use super::*;
use crate::history_snapshot::{HISTORY_CACHE_FILE, HistoryCache};

impl Fixture {
    fn cache(&self, id: &str) -> PathBuf {
        self.dir(id).join(HISTORY_CACHE_FILE)
    }

    fn cache_size(&self, id: &str) -> u64 {
        fs::metadata(self.cache(id)).unwrap().len()
    }

    fn cache_coverage(&self, id: &str) -> Option<u64> {
        let dir = self.sessions.open_child(id).unwrap().unwrap();
        let log = File::open(self.events(id)).unwrap();
        let length = log.metadata().unwrap().len();
        HistoryCache::open(&dir, id, &log, length, Access::ReadOnly).map(|cache| cache.covered())
    }

    fn log_len(&self, id: &str) -> u64 {
        fs::metadata(self.events(id)).unwrap().len()
    }

    fn rewrite_log(&self, id: &str, from: &str, to: &str) {
        let bytes = fs::read(self.events(id)).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(from.len(), to.len());
        fs::write(self.events(id), text.replacen(from, to, 1)).unwrap();
    }

    fn resumed_history(&self, id: &str) -> SavedHistory {
        self.resume(id).unwrap().take_history()
    }
}

fn two_turn_session(fixture: &Fixture, id: &str) {
    let mut session = fixture.start(id);
    session.append(20, &turn("first question")).unwrap();
    session
        .append(30, &tool_turn("second question", "call-1"))
        .unwrap();
}

#[test]
fn history_snapshot_cache_builds_on_writable_resume_and_replays_identically() {
    let fixture = Fixture::new();
    let id = "snapshot-identical";
    two_turn_session(&fixture, id);
    assert!(!fixture.cache(id).exists());

    let cached_history = fixture.resumed_history(id);
    assert!(fixture.cache(id).exists());
    assert_eq!(mode(&fixture.cache(id)), 0o600);
    assert_eq!(fixture.cache_coverage(id), Some(fixture.log_len(id)));

    let mut second = fixture.resume(id).unwrap();
    let snapshot_history = second.take_history();
    assert_eq!(snapshot_history, cached_history);
    let cache_size_before = fixture.cache_size(id);
    second.append(40, &turn("mirror question")).unwrap();
    assert_eq!(fixture.cache_size(id), cache_size_before);
    drop(second);

    let third = fixture.resumed_history(id);
    assert_eq!(third.turns[..2], snapshot_history.turns[..]);
    assert_eq!(
        prompts(&third),
        ["first question", "second question", "mirror question"]
    );
    assert!(fixture.cache_size(id) > cache_size_before);
    assert_eq!(fixture.cache_coverage(id), Some(fixture.log_len(id)));

    fs::remove_file(fixture.cache(id)).unwrap();
    assert_eq!(fixture.resumed_history(id), third);
    assert!(fixture.cache(id).exists());
}

#[test]
fn history_snapshot_resume_reads_cached_content_over_a_middle_rewritten_log() {
    let fixture = Fixture::new();
    let id = "snapshot-consumed";
    two_turn_session(&fixture, id);
    drop(fixture.resume(id).unwrap());
    assert!(fixture.cache(id).exists());

    fixture.rewrite_log(id, "first question", "first qu3stion");
    let history = fixture.resumed_history(id);
    assert_eq!(prompts(&history)[0], "first question");
}

#[test]
fn history_snapshot_tolerates_torn_cache_tails_and_spliced_log_growth() {
    let fixture = Fixture::new();
    let id = "snapshot-torn-and-grown";
    two_turn_session(&fixture, id);
    let baseline = fixture.resumed_history(id);
    assert!(fixture.cache(id).exists());

    let cache = OpenOptions::new()
        .write(true)
        .open(fixture.cache(id))
        .unwrap();
    cache.set_len(fixture.cache_size(id) - 3).unwrap();
    drop(cache);
    assert_eq!(fixture.resumed_history(id), baseline);
    assert_eq!(fixture.cache_coverage(id), Some(fixture.log_len(id)));

    let mut session = fixture.resume(id).unwrap();
    session.append(50, &turn("third question")).unwrap();
    drop(session);
    let rebuilt = fixture.resumed_history(id);
    assert_eq!(rebuilt.turns[..2], baseline.turns[..]);
    assert_eq!(
        prompts(&rebuilt),
        ["first question", "second question", "third question"]
    );

    let cache = OpenOptions::new()
        .write(true)
        .open(fixture.cache(id))
        .unwrap();
    cache
        .write_all_at(b"Z", fixture.cache_size(id) - 2)
        .unwrap();
    drop(cache);
    let grown = fixture.resumed_history(id);
    assert_eq!(grown, rebuilt);
    assert_eq!(fixture.cache_coverage(id), Some(fixture.log_len(id)));
}

#[test]
fn history_snapshot_replays_compacted_sessions_identically() {
    let fixture = Fixture::new();
    let id = "snapshot-compacted";
    let mut session = fixture.start(id);
    session.append(20, &turn("old question")).unwrap();
    session
        .append(
            30,
            &[checkpoint(
                3,
                "<context_handoff>earlier work summarized</context_handoff>",
            )],
        )
        .unwrap();
    session.append(40, &turn("new question")).unwrap();
    drop(session);

    let uncached = fixture.resumed_history(id);
    assert!(fixture.cache(id).exists());
    let cached = fixture.resumed_history(id);
    assert_eq!(cached, uncached);
    assert_eq!(
        cached.compacted.unwrap().summary,
        "<context_handoff>earlier work summarized</context_handoff>"
    );
    assert_eq!(prompts(&uncached), ["new question"]);
}

#[test]
fn an_unfinished_turn_cut_from_the_log_is_cut_from_the_cache() {
    let fixture = Fixture::new();
    let id = "snapshot-unfinished";
    two_turn_session(&fixture, id);
    drop(fixture.resume(id).unwrap());
    let committed = fixture.log_len(id);
    fixture.append_raw(id, &frame(10, &user("lost question")));
    fixture.append_raw(id, &frame(11, &assistant("lost answer")));

    let history = fixture.resumed_history(id);
    assert_eq!(prompts(&history), ["first question", "second question"]);
    assert_eq!(fixture.log_len(id), committed);
    assert_eq!(fixture.cache_coverage(id), Some(committed));

    fixture.append_raw(id, &frame(10, &user("found question")));
    fixture.append_raw(id, &frame(11, &assistant("found answer")));
    fixture.append_raw(id, &frame(12, &completed()));
    let history = fixture.resumed_history(id);
    assert_eq!(
        prompts(&history),
        ["first question", "second question", "found question"]
    );
    assert_eq!(fixture.cache_coverage(id), Some(fixture.log_len(id)));
}

#[test]
fn read_only_loads_use_the_cache_but_never_create_it() {
    let fixture = Fixture::new();
    let id = "snapshot-read-only";
    two_turn_session(&fixture, id);
    let loaded = load_session(&fixture.sessions, id).unwrap();
    assert!(!fixture.cache(id).exists());

    drop(fixture.resume(id).unwrap());
    let size = fixture.cache_size(id);
    fixture.rewrite_log(id, "first question", "first qu3stion");
    let cached = load_session(&fixture.sessions, id).unwrap();
    assert_eq!(cached.history, loaded.history);
    assert_eq!(fixture.cache_size(id), size);

    fs::write(fixture.cache(id), b"not a cache").unwrap();
    let fallback = load_session(&fixture.sessions, id).unwrap();
    assert_eq!(prompts(&fallback.history)[0], "first qu3stion");
    assert_eq!(fs::read(fixture.cache(id)).unwrap(), b"not a cache");
}

#[test]
fn a_cached_frame_that_fails_to_decode_is_read_from_the_log_and_the_cache_rebuilt() {
    let fixture = Fixture::new();
    let id = "snapshot-undecodable";
    two_turn_session(&fixture, id);
    let baseline = fixture.resumed_history(id);

    let mut bytes = fs::read(fixture.cache(id)).unwrap();
    let frame_start = 18 + 24 + id.len();
    let payload_start = frame_start + 8;
    let frame_len = u32::from_le_bytes(bytes[frame_start..frame_start + 4].try_into().unwrap());
    let payload_end = payload_start + usize::try_from(frame_len).unwrap() - 4;
    bytes[payload_start + 24 + 24] = 9;
    let crc = crc32fast::hash(&bytes[payload_start..payload_end]);
    bytes[frame_start + 4..frame_start + 8].copy_from_slice(&crc.to_le_bytes());
    fs::write(fixture.cache(id), &bytes).unwrap();
    assert_eq!(fixture.cache_coverage(id), Some(fixture.log_len(id)));

    assert_eq!(
        load_session(&fixture.sessions, id).unwrap().history,
        baseline
    );
    let mut resumed = fixture.resume(id).unwrap();
    assert_eq!(resumed.take_history(), baseline);
    let mut walked = Vec::new();
    resumed.visit_transcript(|turn| walked.push(turn)).unwrap();
    assert_eq!(walked, baseline.turns);
    drop(resumed);
    assert!(!fixture.cache(id).exists());
    assert_eq!(fixture.resumed_history(id), baseline);
    assert_eq!(fixture.cache_coverage(id), Some(fixture.log_len(id)));
}

#[test]
fn a_cache_whose_frames_fail_validation_is_rebuilt_from_the_log() {
    let fixture = Fixture::new();
    let id = "snapshot-invalid";
    two_turn_session(&fixture, id);
    let baseline = load_session(&fixture.sessions, id).unwrap().history;
    let log = fs::read(fixture.events(id)).unwrap();
    let lines: Vec<&[u8]> = log.split_inclusive(|byte| *byte == b'\n').collect();
    let dir = fixture.sessions.open_child(id).unwrap().unwrap();
    let mut cache = HistoryCache::create(&dir, id).unwrap();
    let mut tee = cache.tee();
    let mut offset = 0;
    for (index, line) in lines.iter().enumerate() {
        let forged = frame(2, &result("never-called"));
        let source = if index == 1 { &forged[..] } else { line };
        let envelope = crate::session_event::decode_conversation_frame(source).unwrap();
        tee.append(offset, line, &envelope);
        offset += u64::try_from(line.len()).unwrap();
    }
    assert!(cache.finish(&dir, offset).is_some());
    assert_eq!(fixture.cache_coverage(id), Some(fixture.log_len(id)));
    let forged = fs::read(fixture.cache(id)).unwrap();

    assert_eq!(
        load_session(&fixture.sessions, id).unwrap().history,
        baseline
    );
    assert_eq!(fs::read(fixture.cache(id)).unwrap(), forged);
    assert_eq!(fixture.resumed_history(id), baseline);
    assert_ne!(fs::read(fixture.cache(id)).unwrap(), forged);
    assert_eq!(fixture.cache_coverage(id), Some(fixture.log_len(id)));
    fixture.rewrite_log(id, "first question", "first qu3stion");
    assert_eq!(fixture.resumed_history(id), baseline);
}

#[test]
fn the_resumed_transcript_reads_the_cache_and_the_turns_committed_after_it() {
    let fixture = Fixture::new();
    let id = "snapshot-transcript";
    two_turn_session(&fixture, id);
    drop(fixture.resume(id).unwrap());
    fixture.rewrite_log(id, "first question", "first qu3stion");
    let mut resumed = fixture.resume(id).unwrap();
    resumed.append(40, &turn("third question")).unwrap();
    let mut walked = Vec::new();
    resumed.visit_transcript(|turn| walked.push(turn)).unwrap();
    let walked = SavedHistory {
        compacted: None,
        turns: walked,
    };
    assert_eq!(
        prompts(&walked),
        ["first question", "second question", "third question"]
    );
}

#[test]
fn a_new_session_keeps_no_cache_until_a_resume_finds_content() {
    let fixture = Fixture::new();
    let id = "snapshot-empty";
    drop(fixture.start(id));
    drop(fixture.resume(id).unwrap());
    assert!(!fixture.cache(id).exists());
}
