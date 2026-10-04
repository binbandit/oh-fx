use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::session_event::{
    ConversationEvent, SteeringEvent, decode_conversation_frame, encode_conversation_frame,
};

struct Fixture {
    root: tempfile::TempDir,
    dir: PrivateDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let dir = PrivateDir::open_or_create(&root.path().join("session")).unwrap();
        Self { root, dir }
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.root.path().join("session").join(name)
    }

    fn log(&self, lines: &[Vec<u8>]) -> (File, u64) {
        fs::write(self.path("events.jsonl"), lines.concat()).unwrap();
        let file = File::open(self.path("events.jsonl")).unwrap();
        let length = file.metadata().unwrap().len();
        (file, length)
    }

    fn cache_bytes(&self) -> Vec<u8> {
        fs::read(self.path(HISTORY_CACHE_FILE)).unwrap()
    }

    fn write_cache_at(&self, offset: u64, bytes: &[u8]) {
        let file = OpenOptions::new()
            .write(true)
            .open(self.path(HISTORY_CACHE_FILE))
            .unwrap();
        file.write_all_at(bytes, offset).unwrap();
    }

    fn open(&self, id: &str, log: &File, length: u64) -> Option<HistoryCache> {
        HistoryCache::open(&self.dir, id, log, length)
    }
}

fn line(seq: u64) -> Vec<u8> {
    let event = ConversationEvent::Steering(SteeringEvent {
        text: "keep going".to_owned(),
    });
    encode_conversation_frame(seq, 1_726_000_000_000, &event).unwrap()
}

fn length(line: &[u8]) -> u64 {
    u64::try_from(line.len()).unwrap()
}

fn mirror(cache: &mut CacheWriter, lines: &[Vec<u8>]) {
    let (_, mut tee) = cache.split();
    let mut offset = 0;
    for line in lines {
        tee.append(offset, line, &decode_conversation_frame(line).unwrap());
        offset += length(line);
    }
}

fn written(fixture: &Fixture, id: &str, lines: &[Vec<u8>], committed: u64) {
    let mut cache = CacheWriter::create(&fixture.dir, id).unwrap();
    mirror(&mut cache, lines);
    assert!(cache.finish(&fixture.dir, committed).is_some());
}

fn seqs(cache: &HistoryCache, start: u64, end: u64) -> Vec<u64> {
    let frames = cache.view().frames(start, end).unwrap();
    frames.map(|frame| frame.envelope.seq).collect()
}

#[test]
fn history_snapshot_verify_cursor_and_watermark_authority() {
    let fixture = Fixture::new();
    let lines = [line(1), line(2)];
    let (log, log_len) = fixture.log(&lines);
    written(&fixture, "sess-1", &lines, log_len);

    let cache = fixture.open("sess-1", &log, log_len).unwrap();
    assert_eq!(cache.covered(), log_len);
    let mut frames = cache.view().frames(0, log_len).unwrap();
    let first = frames.next().unwrap();
    assert_eq!((first.offset, first.bytes), (0, length(&lines[0])));
    assert_eq!(first.envelope.seq, 1);
    assert_eq!(
        first.envelope.event,
        ConversationEvent::Steering(SteeringEvent {
            text: "keep going".to_owned()
        })
    );
    assert_eq!(seqs(&cache, length(&lines[0]), log_len), [2]);
    assert_eq!(seqs(&cache, 0, log_len - 1), [1]);
    assert!(cache.view().frames(1, log_len).is_none());
    assert!(cache.view().frames(log_len, log_len).is_none());
    drop(cache);

    assert!(fixture.open("sess-2", &log, log_len).is_none());
    assert!(fixture.open("sess-1", &log, log_len - 1).is_none());
    let rewritten = OpenOptions::new()
        .write(true)
        .open(fixture.path("events.jsonl"))
        .unwrap();
    rewritten.write_all_at(b"X", log_len - 2).unwrap();
    assert!(fixture.open("sess-1", &log, log_len).is_none());
}

#[test]
fn history_snapshot_rejects_other_format_and_schema_versions_before_decoding() {
    let fixture = Fixture::new();
    let lines = [line(1)];
    let (log, log_len) = fixture.log(&lines);
    written(&fixture, "legacy", &lines, log_len);
    assert!(fixture.open("legacy", &log, log_len).is_some());

    fixture.write_cache_at(18, &2_u64.to_le_bytes());
    assert!(fixture.open("legacy", &log, log_len).is_none());
    fixture.write_cache_at(18, &3_u64.to_le_bytes());
    fixture.write_cache_at(26, &2_u64.to_le_bytes());
    assert!(fixture.open("legacy", &log, log_len).is_none());
    fixture.write_cache_at(26, &3_u64.to_le_bytes());
    fixture.write_cache_at(0, b"fx-history-cachf");
    assert!(fixture.open("legacy", &log, log_len).is_none());
}

#[test]
fn history_snapshot_verification_tolerates_a_torn_tail_as_a_shorter_prefix() {
    let fixture = Fixture::new();
    let lines = [line(1), line(2)];
    let (log, log_len) = fixture.log(&lines);
    written(&fixture, "sess-1", &lines[..1], log_len);
    let end = length(&fixture.cache_bytes());
    fixture.write_cache_at(end, &[0, 64, 0, 0, 1, 2, 3, 4]);

    let cache = fixture.open("sess-1", &log, log_len).unwrap();
    assert_eq!(cache.covered(), length(&lines[0]));
    assert_eq!(seqs(&cache, 0, log_len), [1]);
}

#[test]
fn a_corrupt_frame_ends_the_valid_prefix() {
    let fixture = Fixture::new();
    let lines = [line(1), line(2), line(3)];
    let (log, log_len) = fixture.log(&lines);
    written(&fixture, "sess-1", &lines, log_len);
    let end = length(&fixture.cache_bytes());
    fixture.write_cache_at(end - 2, b"Z");

    let cache = fixture.open("sess-1", &log, log_len).unwrap();
    assert_eq!(cache.covered(), length(&lines[0]) + length(&lines[1]));
}

#[test]
fn a_writable_open_drops_the_invalid_tail_and_extends_the_prefix() {
    let fixture = Fixture::new();
    let lines = [line(1), line(2)];
    let (log, log_len) = fixture.log(&lines);
    written(&fixture, "sess-1", &lines[..1], log_len);
    let prefix = length(&fixture.cache_bytes());
    fixture.write_cache_at(prefix, &[0, 64, 0, 0, 1, 2, 3, 4]);

    let mut cache = CacheWriter::open(&fixture.dir, "sess-1", &log, log_len).unwrap();
    assert_eq!(length(&fixture.cache_bytes()), prefix);
    let (_, mut tee) = cache.split();
    tee.append(
        length(&lines[0]),
        &lines[1],
        &decode_conversation_frame(&lines[1]).unwrap(),
    );
    let cache = cache.finish(&fixture.dir, log_len).unwrap();
    assert_eq!(seqs(&cache, 0, log_len), [1, 2]);
    drop(cache);
    assert_eq!(
        fixture.open("sess-1", &log, log_len).unwrap().covered(),
        log_len
    );
}

#[test]
fn finishing_trims_frames_past_the_committed_log() {
    let fixture = Fixture::new();
    let lines = [line(1), line(2), line(3)];
    let (log, log_len) = fixture.log(&lines);
    let committed = length(&lines[0]) + length(&lines[1]);
    let mut cache = CacheWriter::create(&fixture.dir, "sess-1").unwrap();
    mirror(&mut cache, &lines);
    let cache = cache.finish(&fixture.dir, committed).unwrap();
    assert_eq!(cache.covered(), committed);
    assert_eq!(seqs(&cache, 0, log_len), [1, 2]);
    drop(cache);
    let reopened = fixture.open("sess-1", &log, log_len).unwrap();
    assert_eq!(reopened.covered(), committed);
}

#[test]
fn a_finished_writer_keeps_only_the_frame_index_for_reads() {
    let fixture = Fixture::new();
    let large = ConversationEvent::Steering(SteeringEvent {
        text: "x".repeat(4 * 1024 * 1024),
    });
    let lines = [
        line(1),
        encode_conversation_frame(2, 1, &large).unwrap(),
        line(3),
    ];
    let (_, log_len) = fixture.log(&lines);
    let mut writer = CacheWriter::create(&fixture.dir, "sess-1").unwrap();
    mirror(&mut writer, &lines);
    assert!(writer.tee.pending.capacity() > 4 * 1024 * 1024);
    let cache = writer.finish(&fixture.dir, log_len).unwrap();
    assert_eq!(cache.frames.len(), 3);
    assert_eq!(cache.frames.capacity(), cache.frames.len());
    assert_eq!(
        size_of::<HistoryCache>(),
        size_of::<(File, Vec<FrameMeta>)>()
    );
    assert_eq!(seqs(&cache, 0, log_len), [1, 2, 3]);
}

#[test]
fn a_frame_that_does_not_continue_the_log_breaks_the_writer_and_removes_the_cache() {
    let fixture = Fixture::new();
    let lines = [line(1), line(2)];
    let (_, log_len) = fixture.log(&lines);
    let mut cache = CacheWriter::create(&fixture.dir, "sess-1").unwrap();
    let (_, mut tee) = cache.split();
    tee.append(0, &lines[0], &decode_conversation_frame(&lines[0]).unwrap());
    tee.append(
        length(&lines[0]) + 1,
        &lines[1],
        &decode_conversation_frame(&lines[1]).unwrap(),
    );
    assert!(cache.finish(&fixture.dir, log_len).is_none());
    assert!(!fixture.path(HISTORY_CACHE_FILE).exists());
}

#[test]
fn the_cache_is_private_and_never_follows_links() {
    let fixture = Fixture::new();
    let lines = [line(1)];
    let (log, log_len) = fixture.log(&lines);
    written(&fixture, "sess-1", &lines, log_len);
    let mode = fs::metadata(fixture.path(HISTORY_CACHE_FILE))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    fs::hard_link(fixture.path(HISTORY_CACHE_FILE), fixture.path("other")).unwrap();
    assert!(fixture.open("sess-1", &log, log_len).is_none());
    fs::remove_file(fixture.path("other")).unwrap();
    assert!(fixture.open("sess-1", &log, log_len).is_some());

    fs::set_permissions(
        fixture.path(HISTORY_CACHE_FILE),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(CacheWriter::open(&fixture.dir, "sess-1", &log, log_len).is_none());
    let mut rebuilt = CacheWriter::create(&fixture.dir, "sess-1").unwrap();
    mirror(&mut rebuilt, &lines);
    assert!(rebuilt.finish(&fixture.dir, log_len).is_some());
    let mode = fs::metadata(fixture.path(HISTORY_CACHE_FILE))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn reading_stops_at_a_cached_frame_that_no_longer_decodes() {
    let fixture = Fixture::new();
    let lines = [line(1), line(2)];
    let (log, log_len) = fixture.log(&lines);
    written(&fixture, "sess-1", &lines, log_len);
    let cache = fixture.open("sess-1", &log, log_len).unwrap();
    let end = length(&fixture.cache_bytes());
    let first_end = end - (end - u64::try_from(HEADER_BYTES + 6).unwrap()) / 2;
    fixture.write_cache_at(first_end - 1, b"\xff");
    let mut frames = cache.view().frames(0, log_len).unwrap();
    assert!(frames.next().is_none());
    assert!(frames.next().is_none());
    assert_eq!(seqs(&cache, length(&lines[0]), log_len), [2]);
}
