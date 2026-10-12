use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::session_manager::test_support::framed;

const FIXTURE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/sessions_v2/listing/index.jsonl"
));
const A: &str = "nJI52ukeF2bU";
const B: &str = "U9K-4ZzltsUh";
const C: &str = "pgdu6JEc1UOO";
const D: &str = "hDoX4WdPFWDK";
const GONE: &str = "NHrCmAK4obFR";

struct Root {
    directory: tempfile::TempDir,
}

impl Root {
    fn new(index: &[u8], sessions: &[&str]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(directory.path().join(INDEX_NAME), index).unwrap();
        for id in sessions {
            fs::create_dir(directory.path().join(id)).unwrap();
        }
        Self { directory }
    }

    fn dir(&self) -> PrivateDir {
        PrivateDir::open_existing(self.directory.path())
            .unwrap()
            .unwrap()
    }

    fn ids(&self, cursor: Option<&ListCursor>, limit: usize) -> (Vec<String>, Option<ListCursor>) {
        let page = list(&self.dir(), cursor, limit).unwrap();
        (
            page.items.into_iter().map(|item| item.id).collect(),
            page.next,
        )
    }
}

fn put(id: &str, updated_ms: u64, opened_ms: [u64; HOST_COUNT]) -> Vec<u8> {
    let opened: Vec<String> = opened_ms.iter().map(u64::to_string).collect();
    framed(&format!(
        "{{\"op\":\"put\",\"id\":\"{id}\",\"role\":\"root\",\"host\":\"ask\",\"workspace\":\"/w\",\"created\":1,\"updated\":{updated_ms},\"turns\":1,\"opened\":[{}]",
        opened.join(",")
    ))
}

fn del(id: &str, ts_ms: u64) -> Vec<u8> {
    framed(&format!("{{\"op\":\"del\",\"id\":\"{id}\",\"ts\":{ts_ms}"))
}

#[test]
fn records_from_an_fx_index_decode_and_fold_to_the_newest_put_per_id() {
    let index = Index::fold(FIXTURE);
    let ids: Vec<&str> = index
        .entries
        .iter()
        .map(|entry| entry.summary.id.as_str())
        .collect();
    assert_eq!(ids, [A, B, C, D, GONE]);
    let a = &index.entries[0];
    assert!(!a.deleted);
    assert_eq!(a.summary.role, Role::Root);
    assert_eq!(a.summary.workspace, "/private/tmp/fxv2/listing/workspace");
    assert_eq!(
        a.summary.title.as_deref(),
        Some(&b"\"List question A.\""[..])
    );
    assert_eq!(a.summary.language.as_deref(), Some(&b"\"und-Latn\""[..]));
    assert_eq!(a.summary.created_ms, 1_791_772_881_566);
    assert_eq!(a.summary.updated_ms, 1_791_772_881_704);
    assert_eq!(a.summary.turns, 2);
    assert_eq!(a.summary.opened_ms, [0, 1_791_772_881_660, 0, 0, 0]);
    let gone = &index.entries[4];
    assert!(gone.deleted);
    assert_eq!(gone.summary.title.as_deref(), Some(&b"\"Gone soon.\""[..]));
}

#[test]
fn the_fold_keeps_the_newest_put_merges_open_times_and_a_tombstone_is_final() {
    let mut bytes = put("one", 5, [0, 7, 0, 0, 0]);
    bytes.extend(put("one", 6, [3, 2, 0, 0, 0]));
    bytes.extend(framed(
        "{\"op\":\"opened\",\"id\":\"one\",\"host\":\"acp\",\"ts\":9",
    ));
    bytes.extend(framed(
        "{\"op\":\"opened\",\"id\":\"nobody\",\"host\":\"acp\",\"ts\":9",
    ));
    bytes.extend(del("two", 4));
    bytes.extend(put("two", 8, [0; HOST_COUNT]));
    bytes.extend(b"{\"op\":\"put\"");
    let index = Index::fold(&bytes);
    assert_eq!(index.entries.len(), 2);
    assert_eq!(index.entries[0].summary.updated_ms, 6);
    assert_eq!(index.entries[0].summary.opened_ms, [3, 7, 9, 0, 0]);
    let two = &index.entries[1];
    assert!(two.deleted);
    assert_eq!((two.summary.created_ms, two.summary.updated_ms), (4, 4));
    assert_eq!(two.summary.workspace, "");
}

#[test]
fn a_damaged_line_is_skipped_and_the_rest_still_folds() {
    let mut damaged = put("one", 5, [0; HOST_COUNT]);
    let last = damaged.len() - 4;
    damaged[last] ^= 1;
    let mut bytes = damaged;
    bytes.extend(put("two", 6, [0; HOST_COUNT]));
    bytes.extend(framed("{\"op\":\"put\",\"id\":\"three\""));
    bytes.extend(framed("{\"op\":\"rename\",\"id\":\"four\""));
    let ids: Vec<String> = Index::fold(&bytes)
        .entries
        .into_iter()
        .map(|entry| entry.summary.id)
        .collect();
    assert_eq!(ids, ["two"]);
}

#[test]
fn a_put_with_an_out_of_range_number_is_skipped_and_an_underflow_reads_as_zero() {
    let record = |id: &str, updated: &str| {
        framed(&format!(
            "{{\"op\":\"put\",\"id\":\"{id}\",\"role\":\"root\",\"host\":\"ask\",\"workspace\":\"/w\",\"created\":0,\"updated\":{updated},\"turns\":1,\"opened\":[0,0,0,0,0]"
        ))
    };
    let mut bytes = record("huge", "1e9223372036854775807");
    bytes.extend(record("tiny", "1.0e-9223372036854775807"));
    bytes.extend(record("negative", "-1.0e-9223372036854775808"));
    bytes.extend(put("plain", 5, [0; HOST_COUNT]));
    let root = Root::new(&bytes, &["huge", "tiny", "negative", "plain"]);
    let listed: Vec<(String, u64)> = list(&root.dir(), None, 10)
        .unwrap()
        .items
        .into_iter()
        .map(|item| (item.id, item.updated_ms))
        .collect();
    assert_eq!(
        listed,
        [
            ("plain".to_owned(), 5),
            ("negative".to_owned(), 0),
            ("tiny".to_owned(), 0)
        ]
    );
}

#[test]
fn list_gives_root_sessions_newest_first_and_skips_folders_that_are_gone() {
    let root = Root::new(FIXTURE, &[A, B, C, D]);
    assert_eq!(
        root.ids(None, 10),
        (
            vec![D.to_owned(), C.to_owned(), B.to_owned(), A.to_owned()],
            None
        )
    );
    fs::remove_dir(root.directory.path().join(C)).unwrap();
    assert_eq!(root.ids(None, 10).0, [D, B, A]);
    assert_eq!(
        fs::read(root.directory.path().join(INDEX_NAME)).unwrap(),
        FIXTURE
    );
}

#[test]
fn list_pages_with_a_cursor_and_ties_go_to_the_smaller_id() {
    let mut bytes = put("b", 5, [0; HOST_COUNT]);
    bytes.extend(put("a", 5, [0; HOST_COUNT]));
    bytes.extend(put("c", 9, [0; HOST_COUNT]));
    bytes.extend(framed(
        "{\"op\":\"put\",\"id\":\"kid\",\"role\":\"child\",\"host\":\"child\",\"workspace\":\"/w\",\"parent\":\"c\",\"created\":1,\"updated\":99,\"turns\":1,\"opened\":[0,0,0,0,0]",
    ));
    let root = Root::new(&bytes, &["a", "b", "c", "kid"]);
    let (first, cursor) = root.ids(None, 2);
    assert_eq!(first, ["c", "a"]);
    let cursor = cursor.unwrap();
    assert_eq!(
        cursor,
        ListCursor {
            updated_ms: 5,
            id: "a".to_owned()
        }
    );
    assert_eq!(root.ids(Some(&cursor), 2), (vec!["b".to_owned()], None));
    assert_eq!(
        root.ids(None, 3),
        (vec!["c".to_owned(), "a".to_owned(), "b".to_owned()], None)
    );
}

#[test]
fn a_missing_index_lists_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let root = PrivateDir::open_existing(directory.path())
        .unwrap()
        .unwrap();
    assert!(list(&root, None, 5).unwrap().items.is_empty());
}
