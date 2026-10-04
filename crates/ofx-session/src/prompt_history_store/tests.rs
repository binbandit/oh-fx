use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

use super::*;

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn data(&self) -> PathBuf {
        self.root.path().join("data/oh-fx")
    }

    fn history(&self) -> PathBuf {
        self.data().join(HISTORY_FILE)
    }

    fn store(&self) -> PromptHistoryStore {
        PromptHistoryStore::open(&self.data()).unwrap()
    }

    fn write(&self, bytes: &[u8]) {
        fs::create_dir_all(self.data()).unwrap();
        fs::set_permissions(self.data(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(self.history(), bytes).unwrap();
        fs::set_permissions(self.history(), fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn mode(&self, name: &str) -> u32 {
        fs::metadata(self.data().join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }
}

fn line(timestamp_ms: i64, workspace_root: &str, text: &str) -> String {
    format!(
        "{{\"schema_version\":1,\"timestamp_ms\":{timestamp_ms},\"workspace_root\":\"{workspace_root}\",\"text\":\"{text}\"}}\n"
    )
}

fn valid_record_count(path: &Path) -> usize {
    fs::read(path)
        .unwrap()
        .split(|byte| *byte == b'\n')
        .filter(|line| parse_record(line).is_some())
        .count()
}

#[test]
fn reverse_load_filters_the_workspace_bounds_results_and_keeps_chronology() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    for index in 0..105 {
        assert_eq!(
            store
                .append(index, "/tmp/workspace-a", &format!("a-{index}"))
                .unwrap(),
            AppendOutcome::Appended
        );
        store.append(index, "/tmp/workspace-b", "other").unwrap();
    }
    let all = store.load_recent("/tmp/workspace-a", 200).unwrap();
    assert_eq!(all.len(), 105);
    assert_eq!(all[0], "a-0");
    assert_eq!(all[104], "a-104");
    let bounded = store.load_recent("/tmp/workspace-a", 100).unwrap();
    assert_eq!(bounded.len(), 100);
    assert_eq!(bounded[0], "a-5");
    assert_eq!(bounded[99], "a-104");
    assert_eq!(
        store.load_recent("/tmp/workspace-b", 100).unwrap(),
        ["other"]
    );
}

#[test]
fn reverse_load_scans_past_a_mebibyte_of_newer_records_from_other_workspaces() {
    let fixture = Fixture::new();
    let mut bytes = String::new();
    for index in 0..100 {
        bytes.push_str(&line(index, "/tmp/workspace-a", &format!("kept-{index}")));
    }
    let filler = "x".repeat(2048);
    let mut index = 0;
    while bytes.len() < 1200 * 1024 {
        bytes.push_str(&line(1000 + index, "/tmp/workspace-b", &filler));
        index += 1;
    }
    fixture.write(bytes.as_bytes());
    let entries = fixture
        .store()
        .load_recent("/tmp/workspace-a", 100)
        .unwrap();
    assert_eq!(entries.len(), 100);
    assert_eq!(entries[0], "kept-0");
    assert_eq!(entries[99], "kept-99");
}

#[test]
fn reverse_load_joins_records_across_blocks_and_skips_corrupt_lines_and_the_open_tail() {
    let fixture = Fixture::new();
    let long_text = format!("block-spanning-{}", "x".repeat(160));
    let bytes = [
        line(1, "/tmp/workspace", &long_text),
        "{malformed}\n".to_owned(),
        "\n".to_owned(),
        line(9, "relative", "bad root"),
        "{\"schema_version\":2,\"timestamp_ms\":3,\"workspace_root\":\"/tmp/workspace\",\"text\":\"future\"}\n".to_owned(),
        "{\"schema_version\":1,\"timestamp_ms\":3,\"workspace_root\":\"/tmp/workspace\",\"text\":\"extra\",\"x\":1}\n".to_owned(),
        "{\"schema_version\":1,\"timestamp_ms\":3,\"workspace_root\":\"/tmp/workspace\",\"text\":{\"encoding\":\"base64\",\"data\":\"/w==\"}}\n".to_owned(),
        "{\"schema_version\":1,\"schema_version\":1,\"timestamp_ms\":3,\"workspace_root\":\"/tmp/workspace\",\"text\":\"twice\"}\n".to_owned(),
        line(2, "/tmp/workspace", "newer"),
        "{\"schema_version\":1,\"timestamp_ms\":3".to_owned(),
    ]
    .concat();
    fixture.write(bytes.as_bytes());
    let mut store = fixture.store();
    store.scan_block_bytes = 32;
    assert_eq!(
        store.load_recent("/tmp/workspace", 100).unwrap(),
        [long_text.as_str(), "newer"]
    );
}

#[test]
fn reverse_load_reads_records_however_their_json_spells_the_workspace() {
    let fixture = Fixture::new();
    let bytes = [
        line(1, "/tmp/workspace", "plain"),
        "{\"workspace_root\" : \"/tmp/workspace\", \"text\":\"reordered\",\"timestamp_ms\":2,\"schema_version\":1}\n".to_owned(),
        "{\"schema_version\":1,\"timestamp_ms\":3,\"workspace_root\":\"\\/tmp\\/work\\u0073pace\",\"text\":\"escaped\"}\n".to_owned(),
        line(4, "/tmp/workspace-b", "other"),
        line(5, "/tmp/workspace/nested", "nested"),
        "{\"schema_version\":1,\"timestamp_ms\":6,\"workspace_root\":\"/tmp/elsewhere\",\"text\":\"\\\"/tmp/workspace\\\"\"}\n".to_owned(),
    ]
    .concat();
    fixture.write(bytes.as_bytes());
    assert_eq!(
        fixture.store().load_recent("/tmp/workspace", 100).unwrap(),
        ["plain", "reordered", "escaped"]
    );
}

const WRITERS_QUEUE_BEHIND_WHOLE_RUNS: Duration = Duration::from_mins(1);

#[test]
fn concurrent_appends_from_separate_stores_keep_every_record_whole() {
    let fixture = Fixture::new();
    fixture.store().append(0, "/tmp/workspace", "seed").unwrap();
    let writers: Vec<_> = (0..4)
        .map(|writer| {
            let data = fixture.data();
            std::thread::spawn(move || {
                let mut store = PromptHistoryStore::open(&data).unwrap();
                store.lock_deadline = WRITERS_QUEUE_BEHIND_WHOLE_RUNS;
                for index in 0..25 {
                    let text = format!("writer-{writer}-prompt-{index}-{}", "x".repeat(300));
                    assert_eq!(
                        store.append(index, "/tmp/workspace", &text).unwrap(),
                        AppendOutcome::Appended
                    );
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let bytes = fs::read(fixture.history()).unwrap();
    assert!(bytes.ends_with(b"\n"));
    assert_eq!(
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .count(),
        101
    );
    assert_eq!(valid_record_count(&fixture.history()), 101);
    let entries = fixture.store().load_recent("/tmp/workspace", 200).unwrap();
    assert_eq!(entries.len(), 101);
    for writer in 0..4 {
        let prefix = format!("writer-{writer}-prompt-");
        let order: Vec<usize> = entries
            .iter()
            .filter_map(|entry| entry.strip_prefix(&prefix))
            .map(|rest| rest.split('-').next().unwrap().parse().unwrap())
            .collect();
        assert_eq!(order, (0..25).collect::<Vec<usize>>());
    }
}

#[test]
fn a_compaction_interrupted_before_its_rename_leaves_the_history_whole() {
    let fixture = Fixture::new();
    fixture.write(line(1, "/tmp/workspace", "kept").as_bytes());
    let leftover = fixture
        .data()
        .join(".history.jsonl.tmp.00112233445566778899aabbccddeeff");
    fs::write(&leftover, "{\"partial").unwrap();
    let mut store = fixture.store();
    assert_eq!(store.load_recent("/tmp/workspace", 100).unwrap(), ["kept"]);
    store.append(2, "/tmp/workspace", "next").unwrap();
    assert_eq!(
        store.load_recent("/tmp/workspace", 100).unwrap(),
        ["kept", "next"]
    );
    assert_eq!(fs::read_to_string(&leftover).unwrap(), "{\"partial");
}

#[test]
fn reverse_load_skips_records_longer_than_the_record_cap() {
    let fixture = Fixture::new();
    let oversized = line(1, "/tmp/workspace", &"x".repeat(MAX_RECORD_BYTES));
    let bytes = [
        line(0, "/tmp/workspace", "before"),
        oversized,
        line(2, "/tmp/workspace", "after"),
    ]
    .concat();
    fixture.write(bytes.as_bytes());
    for block in [DEFAULT_SCAN_BLOCK_BYTES, 4096] {
        let mut store = fixture.store();
        store.scan_block_bytes = block;
        assert_eq!(
            store.load_recent("/tmp/workspace", 100).unwrap(),
            ["before", "after"]
        );
    }
}

#[test]
fn reverse_load_stops_at_the_captured_boundary() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    store.append(1, "/tmp/workspace", "before").unwrap();
    let boundary = fs::metadata(fixture.history()).unwrap().len();
    store.append(2, "/tmp/workspace", "after").unwrap();
    let file = store.open_history(false, false).unwrap().unwrap();
    assert_eq!(
        store
            .load_recent_from_file(&file, "/tmp/workspace", 100, boundary)
            .unwrap(),
        ["before"]
    );
}

#[test]
fn appends_skip_the_latest_prompt_of_the_same_workspace_only() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    let mut other = fixture.store();
    assert_eq!(
        store.append(1, "/tmp/a", "same").unwrap(),
        AppendOutcome::Appended
    );
    assert_eq!(
        other.append(2, "/tmp/b", "between").unwrap(),
        AppendOutcome::Appended
    );
    assert_eq!(
        other.append(3, "/tmp/a", "same").unwrap(),
        AppendOutcome::Duplicate
    );
    assert_eq!(
        store.append(4, "/tmp/b", "same").unwrap(),
        AppendOutcome::Appended
    );
    assert_eq!(
        store.append(5, "/tmp/a", "same\n").unwrap(),
        AppendOutcome::Appended
    );
    assert_eq!(
        store.load_recent("/tmp/a", 100).unwrap(),
        ["same", "same\n"]
    );
    assert_eq!(
        other.load_recent("/tmp/b", 100).unwrap(),
        ["between", "same"]
    );
}

#[test]
fn records_keep_upstream_field_order_and_json_escapes() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    store
        .append(42, "/tmp/w s", "quote \" slash \\ tab\t bell\u{7} é")
        .unwrap();
    assert_eq!(
        fs::read_to_string(fixture.history()).unwrap(),
        "{\"schema_version\":1,\"timestamp_ms\":42,\"workspace_root\":\"/tmp/w s\",\"text\":\"quote \\\" slash \\\\ tab\\t bell\\u0007 é\"}\n"
    );
}

#[test]
fn an_append_repairs_an_interrupted_tail_before_writing() {
    let fixture = Fixture::new();
    fixture.write(
        [line(1, "/tmp/workspace", "kept"), "{\"schema".to_owned()]
            .concat()
            .as_bytes(),
    );
    let mut store = fixture.store();
    store.append(2, "/tmp/workspace", "next").unwrap();
    assert_eq!(
        fs::read_to_string(fixture.history()).unwrap(),
        [
            line(1, "/tmp/workspace", "kept"),
            line(2, "/tmp/workspace", "next")
        ]
        .concat()
    );
    fixture.write(b"no newline at all");
    store.append(3, "/tmp/workspace", "only").unwrap();
    assert_eq!(
        fs::read_to_string(fixture.history()).unwrap(),
        line(3, "/tmp/workspace", "only")
    );
}

#[test]
fn an_oversized_prompt_is_skipped_without_creating_durable_state() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    assert_eq!(
        store
            .append(1, "/tmp/workspace", &"x".repeat(MAX_RECORD_BYTES))
            .unwrap(),
        AppendOutcome::RecordTooLarge
    );
    assert!(!fixture.data().exists());
}

#[test]
fn compaction_keeps_the_newest_thousand_valid_records_within_a_mebibyte() {
    let fixture = Fixture::new();
    let mut bytes = String::from("{corrupt}\n");
    for index in 0..1005 {
        let text = format!("{index:04}-{}", "x".repeat(1024));
        bytes.push_str(&line(index, "/tmp/workspace", &text));
    }
    fixture.write(bytes.as_bytes());
    let mut store = fixture.store();
    assert_eq!(
        store.append(2000, "/tmp/workspace", "newest").unwrap(),
        AppendOutcome::Appended
    );
    let retained = valid_record_count(&fixture.history());
    assert!(
        retained > 0 && retained <= COMPACTION_RECORD_LIMIT,
        "{retained}"
    );
    assert!(fs::metadata(fixture.history()).unwrap().len() <= COMPACTION_THRESHOLD_BYTES);
    assert!(
        !fs::read_to_string(fixture.history())
            .unwrap()
            .contains("corrupt")
    );
    assert_eq!(fixture.mode(HISTORY_FILE), 0o600);
    let entries = store.load_recent("/tmp/workspace", 100).unwrap();
    assert_eq!(entries.last().map(String::as_str), Some("newest"));
    assert!(entries[0].starts_with(&format!("{:04}-", 1005 - 99)));
}

#[test]
fn compaction_caps_the_record_count_when_records_are_small() {
    let fixture = Fixture::new();
    let mut bytes = String::new();
    let mut index = 0;
    while bytes.len() <= usize::try_from(COMPACTION_THRESHOLD_BYTES).unwrap() {
        bytes.push_str(&line(index, "/tmp/workspace", &format!("prompt-{index}")));
        index += 1;
    }
    fixture.write(bytes.as_bytes());
    fixture
        .store()
        .append(index, "/tmp/workspace", "newest")
        .unwrap();
    assert_eq!(
        valid_record_count(&fixture.history()),
        COMPACTION_RECORD_LIMIT
    );
}

#[test]
fn a_held_lock_fails_the_append_as_busy() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    store.append(1, "/tmp/workspace", "first").unwrap();
    let holder = PrivateDir::open_existing(&fixture.data()).unwrap().unwrap();
    let held = holder.try_lock(HISTORY_LOCK_FILE).unwrap().unwrap();
    store.lock_deadline = Duration::from_millis(30);
    assert_eq!(
        store.append(2, "/tmp/workspace", "busy"),
        Err(PromptHistoryError::LockBusy)
    );
    assert_eq!(
        PromptHistoryError::LockBusy.to_string(),
        "PromptHistoryLockBusy"
    );
    drop(held);
    assert_eq!(
        store.append(3, "/tmp/workspace", "free").unwrap(),
        AppendOutcome::Appended
    );
}

#[test]
fn loading_an_absent_history_creates_no_state() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    assert!(store.load_recent("/tmp/workspace", 100).unwrap().is_empty());
    assert!(!fixture.data().exists());
    fs::create_dir_all(fixture.data()).unwrap();
    let mut store = fixture.store();
    assert!(store.load_recent("/tmp/workspace", 100).unwrap().is_empty());
    assert!(!fixture.history().exists());
}

#[test]
fn the_first_append_creates_a_private_layout() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    store.append(1, "/tmp/workspace", "first").unwrap();
    assert_eq!(
        fs::metadata(fixture.data()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(fixture.mode(HISTORY_FILE), 0o600);
    assert_eq!(fixture.mode(HISTORY_LOCK_FILE), 0o600);
}

#[test]
fn shared_history_files_fail_reads_until_an_append_makes_them_private() {
    let fixture = Fixture::new();
    fixture.write(line(1, "/tmp/workspace", "kept").as_bytes());
    fs::set_permissions(fixture.history(), fs::Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(fixture.data(), fs::Permissions::from_mode(0o755)).unwrap();
    let mut store = fixture.store();
    assert_eq!(
        store.load_recent("/tmp/workspace", 100),
        Err(PromptHistoryError::PermissionsUnsupported)
    );
    store.append(2, "/tmp/workspace", "next").unwrap();
    assert_eq!(fixture.mode(HISTORY_FILE), 0o600);
    assert_eq!(
        fs::metadata(fixture.data()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        store.load_recent("/tmp/workspace", 100).unwrap(),
        ["kept", "next"]
    );
}

#[test]
fn linked_history_paths_are_refused() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.path().join("elsewhere")).unwrap();
    fs::create_dir_all(fixture.root.path().join("data")).unwrap();
    symlink(fixture.root.path().join("elsewhere"), fixture.data()).unwrap();
    assert!(matches!(
        PromptHistoryStore::open(&fixture.data()),
        Err(PromptHistoryError::PathUnsafe)
    ));

    let fixture = Fixture::new();
    fixture.write(b"");
    let outside = fixture.root.path().join("outside.jsonl");
    fs::write(&outside, line(1, "/tmp/workspace", "outside")).unwrap();
    fs::remove_file(fixture.history()).unwrap();
    symlink(&outside, fixture.history()).unwrap();
    let mut store = fixture.store();
    assert_eq!(
        store.load_recent("/tmp/workspace", 100),
        Err(PromptHistoryError::PathUnsafe)
    );
    assert_eq!(
        store.append(2, "/tmp/workspace", "x"),
        Err(PromptHistoryError::PathUnsafe)
    );
    fs::remove_file(fixture.history()).unwrap();
    fs::hard_link(&outside, fixture.history()).unwrap();
    assert_eq!(
        store.append(2, "/tmp/workspace", "x"),
        Err(PromptHistoryError::PathUnsafe)
    );
    assert_eq!(
        fs::read_to_string(&outside).unwrap(),
        line(1, "/tmp/workspace", "outside")
    );
}

#[test]
fn workspace_roots_must_be_absolute_and_bounded() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    for root in ["", "relative", &format!("/{}", "a".repeat(MAX_PATH_BYTES))] {
        assert_eq!(
            store.append(1, root, "x"),
            Err(PromptHistoryError::InvalidDurableField)
        );
        assert_eq!(
            store.load_recent(root, 1),
            Err(PromptHistoryError::InvalidDurableField)
        );
    }
}
