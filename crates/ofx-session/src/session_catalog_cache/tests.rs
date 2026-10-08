use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use ofx_config::{PrivateDir, ProviderId};
use ofx_contract::ReasoningEffort;
use ofx_text::lowercase_hex;
use sha2::{Digest, Sha256};

use super::catalog_codec::MAGIC;
use super::fingerprint::{Kind, Observed, Stamp};
use super::*;
use crate::session_codec::{SavedProvider, SessionMetadata, SessionPreferences};
use crate::session_event::{AssistantEvent, ConversationEvent, TurnCompletedEvent, UserEvent};
use crate::session_log::{LOCK_DEADLINE, resume_session, start_session};
use crate::spawn_gate::make_fifo;

struct Sessions {
    root: tempfile::TempDir,
    dir: PrivateDir,
}

impl Sessions {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let dir = PrivateDir::open_or_create(&root.path().join("sessions")).unwrap();
        Self { root, dir }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join("sessions").join(relative)
    }

    fn seed(&self, id: &str, turns: usize) {
        self.seed_session(id, turns, false);
    }

    fn seed_session(&self, id: &str, turns: usize, subagent_child: bool) {
        let metadata = SessionMetadata {
            id: id.to_owned(),
            origin_workspace_root: "/workspace".to_owned(),
            workspace_root: "/workspace".to_owned(),
            created_at_ms: 1,
            updated_at_ms: 2,
            conversation_language: "en".to_owned(),
            preferences: SessionPreferences {
                provider: SavedProvider::new(ProviderId::Gateway, None).unwrap(),
                model: "openai/gpt-5".to_owned(),
                effort: ReasoningEffort::Auto,
                fast_mode: false,
            },
            title: None,
            subagent_child,
        };
        let mut session = start_session(&self.dir, metadata).unwrap();
        for index in 0..turns {
            session
                .append(
                    3,
                    &[
                        ConversationEvent::User(UserEvent::new(format!("{id} {index}"))),
                        ConversationEvent::Assistant(AssistantEvent {
                            text: "ok".to_owned(),
                            provider_replay: None,
                            standalone_response: false,
                        }),
                        ConversationEvent::TurnCompleted(TurnCompletedEvent::default()),
                    ],
                )
                .unwrap();
        }
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.path(""))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| !name.starts_with('.'))
            .collect();
        names.sort();
        names
    }

    fn scan(&self, writable: bool) -> CatalogScan {
        let mut scan = scan_catalog(&self.dir, &self.names(), writable);
        scan.summaries.sort_by(|a, b| a.id.cmp(&b.id));
        scan
    }

    fn cached(&self) -> CachedCatalog {
        CachedCatalog::load(&self.dir)
    }

    fn write_catalog(&self, rows: &[Row]) {
        self.dir
            .replace(CATALOG_FILE, &encode_catalog(rows).unwrap())
            .unwrap();
    }

    fn titles(scan: &CatalogScan) -> Vec<(&str, Option<&str>)> {
        scan.summaries
            .iter()
            .map(|summary| (summary.id.as_str(), summary.title.as_deref()))
            .collect()
    }
}

fn visible(id: &str, fingerprint: Fingerprint) -> Row {
    Row {
        id: id.to_owned(),
        fingerprint,
        summary: Some(RowSummary {
            workspace_root: Some("/workspace".to_owned()),
            origin_workspace_root: Some("/origin".to_owned()),
            title: Some("Saved title".to_owned()),
            preview: None,
            flags: CHECKPOINT_FLAG,
            created_at_ms: 1,
            updated_at_ms: 2,
            history_len: 3,
            language: "en".to_owned(),
        }),
    }
}

fn excluded(id: &str, fingerprint: Fingerprint) -> Row {
    Row {
        id: id.to_owned(),
        fingerprint,
        summary: None,
    }
}

#[test]
fn rows_are_written_in_upstreams_v6_layout_and_read_back() {
    let rows = [visible("visible", [1; 32]), excluded("private", [2; 32])];
    let mut payload = Vec::new();
    payload.extend_from_slice(&2_u32.to_le_bytes());
    payload.extend_from_slice(&7_u32.to_le_bytes());
    payload.extend_from_slice(b"visible");
    payload.extend_from_slice(&[1; 32]);
    payload.extend_from_slice(&[1, 0b010]);
    payload.extend_from_slice(&1_i64.to_le_bytes());
    payload.extend_from_slice(&2_i64.to_le_bytes());
    payload.extend_from_slice(&3_u64.to_le_bytes());
    for text in ["/workspace", "/origin", "Saved title"] {
        payload.extend_from_slice(&u32::try_from(text.len()).unwrap().to_le_bytes());
        payload.extend_from_slice(text.as_bytes());
    }
    payload.extend_from_slice(&u32::MAX.to_le_bytes());
    payload.extend_from_slice(&2_u32.to_le_bytes());
    payload.extend_from_slice(b"en");
    payload.extend_from_slice(&7_u32.to_le_bytes());
    payload.extend_from_slice(b"private");
    payload.extend_from_slice(&[2; 32]);
    payload.push(0);
    let mut expected = b"fx-resume-catalog-v6\n".to_vec();
    expected.extend_from_slice(&Sha256::digest(&payload));
    expected.extend_from_slice(&payload);
    let encoded = encode_catalog(&rows).unwrap();
    assert_eq!(encoded, expected);
    assert_eq!(decode_catalog(&encoded).unwrap(), rows);
}

#[test]
fn damaged_foreign_and_out_of_contract_catalogs_are_misses() {
    let valid = encode_catalog(&[visible("visible", [1; 32])]).unwrap();
    let reseal = |payload: &[u8]| {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&Sha256::digest(payload));
        bytes.extend_from_slice(payload);
        bytes
    };
    let payload = valid[MAGIC.len() + 32..].to_vec();
    let mut older = valid.clone();
    older[MAGIC.len() - 2] = b'5';
    let mut tampered = valid.clone();
    *tampered.last_mut().unwrap() ^= 1;
    let mut trailing = payload.clone();
    trailing.push(0);
    let mut unknown_flag = payload.clone();
    unknown_flag[4 + 4 + 7 + 32 + 1] = 0b1000;
    let mut unknown_tag = payload.clone();
    unknown_tag[4 + 4 + 7 + 32] = 2;
    let skewed = encode_catalog(&[Row {
        summary: visible("visible", [1; 32])
            .summary
            .map(|summary| RowSummary {
                created_at_ms: 5,
                updated_at_ms: 4,
                ..summary
            }),
        ..visible("visible", [1; 32])
    }]);
    let relative = encode_catalog(&[Row {
        summary: visible("visible", [1; 32])
            .summary
            .map(|summary| RowSummary {
                workspace_root: Some("workspace".to_owned()),
                ..summary
            }),
        ..visible("visible", [1; 32])
    }]);
    let json = reseal(b"[{\"id\":\"legacy\"}]");
    let mut too_many = 100_001_u32.to_le_bytes().to_vec();
    too_many.extend_from_slice(&payload[4..]);
    let cases = [
        older,
        tampered,
        reseal(&trailing),
        reseal(&unknown_flag),
        reseal(&unknown_tag),
        skewed.unwrap(),
        relative.unwrap(),
        json,
        reseal(&too_many),
        encode_catalog(&[excluded("../escape", [3; 32])]).unwrap(),
        b"corrupt cache".to_vec(),
    ];
    for bytes in cases {
        assert_eq!(decode_catalog(&bytes), None, "{}", lowercase_hex(&bytes));
    }
    let duplicate = encode_catalog(&[excluded("same", [4; 32]), excluded("same", [4; 32])]);
    let sessions = Sessions::new();
    sessions
        .dir
        .replace(CATALOG_FILE, &duplicate.unwrap())
        .unwrap();
    assert!(!sessions.cached().present);
}

#[test]
fn a_catalog_that_is_not_a_private_single_regular_file_is_ignored() {
    let sessions = Sessions::new();
    sessions.write_catalog(&[excluded("valid", [1; 32])]);
    assert_eq!(sessions.cached().count(), 1);
    let path = sessions.path(CATALOG_FILE);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    assert!(!sessions.cached().present);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(&path, sessions.path("second-link")).unwrap();
    assert!(!sessions.cached().present);
    fs::remove_file(sessions.path("second-link")).unwrap();
    assert!(sessions.cached().present);
    fs::rename(&path, sessions.path("elsewhere")).unwrap();
    symlink(sessions.path("elsewhere"), &path).unwrap();
    assert!(!sessions.cached().present);
    fs::remove_file(&path).unwrap();
    assert!(make_fifo(&path));
    assert!(!sessions.cached().present);
}

#[test]
fn fingerprints_bind_every_input_listing_reads() {
    let sessions = Sessions::new();
    sessions.seed("session", 1);
    let first = fingerprint(&sessions.dir, "session").unwrap();
    assert_eq!(fingerprint(&sessions.dir, "session"), Some(first));
    sessions.seed("other", 1);
    assert_eq!(fingerprint(&sessions.dir, "session"), Some(first));
    let events = sessions.path("session/events.jsonl");
    let mut log = fs::read(&events).unwrap();
    log.extend_from_slice(b"\n");
    fs::write(&events, &log).unwrap();
    let appended = fingerprint(&sessions.dir, "session").unwrap();
    assert_ne!(appended, first);
    for name in ["authority.json", "authority.pending.json", "display.json"] {
        let absent = fingerprint(&sessions.dir, "session").unwrap();
        fs::write(sessions.path(&format!("session/{name}")), "{}\n").unwrap();
        assert_ne!(
            fingerprint(&sessions.dir, "session").unwrap(),
            absent,
            "{name}"
        );
        fs::remove_file(sessions.path(&format!("session/{name}"))).unwrap();
    }
    fs::create_dir(sessions.path("session/subagent")).unwrap();
    let child = fingerprint(&sessions.dir, "session").unwrap();
    fs::set_permissions(
        sessions.path("session/subagent"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let private = fingerprint(&sessions.dir, "session").unwrap();
    assert_ne!(private, child);
    fs::write(sessions.path("session/subagent/owner.json"), "{}").unwrap();
    assert_ne!(fingerprint(&sessions.dir, "session").unwrap(), private);
    fs::hard_link(
        sessions.path("session/session.json"),
        sessions.path("linked.json"),
    )
    .unwrap();
    assert_eq!(fingerprint(&sessions.dir, "session"), None);
    fs::remove_file(sessions.path("linked.json")).unwrap();
    symlink(sessions.path("other"), sessions.path("alias")).unwrap();
    for id in ["alias", "missing", "../session"] {
        assert_eq!(fingerprint(&sessions.dir, id), None, "{id}");
    }
}

#[test]
fn the_digest_hashes_stats_in_upstreams_order_and_widths() {
    let stamp = |inode: u64, kind: Kind, mode: u32| Stamp {
        inode,
        nlink: 1,
        size: 42,
        kind,
        mode,
        mtime_ns: 1_700_000_000_123_456_789,
        ctime_ns: -5,
    };
    let observed = Observed {
        directory: Stamp {
            nlink: 3,
            ..stamp(7, Kind::Directory, 0o40_700)
        },
        files: [
            Some(stamp(8, Kind::File, 0o100_600)),
            Some(stamp(9, Kind::File, 0o100_600)),
            None,
            None,
            Some(stamp(10, Kind::File, 0o100_644)),
        ],
        child: Some((
            stamp(11, Kind::Directory, 0o40_755),
            [None, Some(stamp(12, Kind::File, 0o100_600))],
        )),
    };
    assert_eq!(
        lowercase_hex(&observed.digest()),
        "d8028819a587a8ab83275d1556bde7d5a61ce93b062925195984a509efb8104d"
    );
    let without_child = Observed {
        child: None,
        ..observed
    };
    assert_eq!(
        lowercase_hex(&without_child.digest()),
        "8ef6cee8a637be65dd8dbb610dd8ad8c90350e31582e24e7ba2326f950a71fb5"
    );
}

#[test]
fn a_writable_listing_saves_the_catalog_and_later_listings_reuse_unchanged_rows() {
    let sessions = Sessions::new();
    sessions.seed("alpha", 2);
    sessions.seed("beta", 0);
    let built = sessions.scan(true);
    assert_eq!(Sessions::titles(&built), [("alpha", None), ("beta", None)]);
    assert_eq!(built.summaries[0].history_len, 2);
    let cached = sessions.cached();
    assert_eq!(cached.count(), 2);
    let alpha = fingerprint(&sessions.dir, "alpha").unwrap();
    let mut crafted = cached.rows.clone();
    for row in &mut crafted {
        if row.id == "alpha" {
            row.summary.as_mut().unwrap().title = Some("Cached title".to_owned());
            assert_eq!(row.fingerprint, alpha);
        }
    }
    sessions.write_catalog(&crafted);
    let reused = sessions.scan(true);
    assert_eq!(
        Sessions::titles(&reused),
        [("alpha", Some("Cached title")), ("beta", None)]
    );
    let mut session = resume_session(&sessions.dir, "alpha", LOCK_DEADLINE).unwrap();
    session.rename("Renamed").unwrap();
    drop(session);
    let renamed = sessions.scan(true);
    assert_eq!(
        Sessions::titles(&renamed),
        [("alpha", Some("Renamed")), ("beta", None)]
    );
    assert_eq!(
        sessions
            .cached()
            .reuse("alpha", &fingerprint(&sessions.dir, "alpha").unwrap()),
        Some(Reuse::Listed(renamed.summaries[0].clone()))
    );
}

#[test]
fn a_read_only_listing_reads_the_catalog_but_never_writes_it() {
    let sessions = Sessions::new();
    sessions.seed("alpha", 1);
    sessions.scan(false);
    assert!(!sessions.path(CATALOG_FILE).exists());
    sessions.scan(true);
    let mut crafted = sessions.cached().rows;
    crafted[0].summary.as_mut().unwrap().title = Some("Cached title".to_owned());
    sessions.write_catalog(&crafted);
    let before = fs::read(sessions.path(CATALOG_FILE)).unwrap();
    let mut session = resume_session(&sessions.dir, "alpha", LOCK_DEADLINE).unwrap();
    session.rename("Renamed").unwrap();
    drop(session);
    assert_eq!(
        Sessions::titles(&sessions.scan(false)),
        [("alpha", Some("Renamed"))]
    );
    assert_eq!(fs::read(sessions.path(CATALOG_FILE)).unwrap(), before);
}

#[test]
fn vanished_and_unreadable_sessions_leave_the_catalog_and_count_as_skipped() {
    let sessions = Sessions::new();
    sessions.seed("alpha", 1);
    sessions.seed("beta", 1);
    sessions.scan(true);
    fs::remove_dir_all(sessions.path("alpha")).unwrap();
    fs::write(sessions.path("beta/session.json"), "{\"schema_version\":1,").unwrap();
    let scan = sessions.scan(true);
    assert!(scan.summaries.is_empty());
    assert_eq!(scan.skipped_invalid, 1);
    let cached = sessions.cached();
    assert!(cached.present);
    assert_eq!(cached.count(), 0);
}

#[test]
fn rows_oh_fx_cannot_list_are_classified_again_and_excluded_rows_stay_hidden() {
    let sessions = Sessions::new();
    sessions.seed("alpha", 1);
    sessions.seed("beta", 1);
    sessions.seed("gamma", 1);
    sessions.seed("delta", 1);
    sessions.scan(true);
    let rows: Vec<Row> = sessions
        .cached()
        .rows
        .into_iter()
        .map(|mut row| {
            let summary = row.summary.as_mut().unwrap();
            summary.title = Some("Cached title".to_owned());
            match row.id.as_str() {
                "alpha" => summary.preview = Some("first prompt".to_owned()),
                "beta" => summary.flags |= DISPLAY_METADATA_FLAG,
                "gamma" => summary.workspace_root = None,
                _ => row.summary = None,
            }
            row
        })
        .collect();
    sessions.write_catalog(&rows);
    let scan = sessions.scan(true);
    assert_eq!(
        Sessions::titles(&scan),
        [("alpha", None), ("beta", None), ("gamma", None)]
    );
    assert_eq!(scan.skipped_invalid, 0);
    let rewritten = sessions.cached();
    assert_eq!(rewritten.count(), 4);
    assert!(rewritten.rows.iter().all(|row| {
        row.summary
            .as_ref()
            .is_none_or(|summary| summary.title.is_none())
    }));
}

#[test]
fn child_sessions_stay_out_of_the_listing_as_excluded_rows() {
    let sessions = Sessions::new();
    sessions.seed("alpha", 1);
    sessions.seed_session("child", 1, true);
    assert_eq!(Sessions::titles(&sessions.scan(true)), [("alpha", None)]);
    let child = fingerprint(&sessions.dir, "child").unwrap();
    assert_eq!(
        sessions.cached().reuse("child", &child),
        Some(Reuse::Excluded)
    );
    assert_eq!(Sessions::titles(&sessions.scan(true)), [("alpha", None)]);
}

fn inode(sessions: &Sessions) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(sessions.path(CATALOG_FILE)).unwrap().ino()
}

#[test]
fn rows_for_sessions_outside_the_current_layout_are_classified_again() {
    let sessions = Sessions::new();
    sessions.seed("current", 1);
    sessions.seed("stray", 1);
    fs::write(sessions.path("stray/display.json"), "{}").unwrap();
    fs::create_dir(sessions.path("upgraded")).unwrap();
    fs::set_permissions(sessions.path("upgraded"), fs::Permissions::from_mode(0o700)).unwrap();
    for (name, body) in [
        ("session.json", "{\"schema_version\":3}"),
        ("events.jsonl", ""),
        ("authority.json", "{}"),
    ] {
        fs::write(sessions.path(&format!("upgraded/{name}")), body).unwrap();
    }
    sessions.scan(true);
    let mut rows = sessions.cached().rows;
    let upgraded = Row {
        summary: visible("upgraded", [0; 32])
            .summary
            .map(|summary| RowSummary {
                title: Some("Untitled session".to_owned()),
                flags: 0,
                created_at_ms: 1,
                updated_at_ms: 9_999,
                history_len: 2,
                ..summary
            }),
        ..visible("upgraded", fingerprint(&sessions.dir, "upgraded").unwrap())
    };
    rows.push(upgraded);
    sessions.write_catalog(&rows);
    let listed = sessions.scan(true);
    assert_eq!(
        Sessions::titles(&listed),
        [("current", None), ("stray", None)]
    );
    assert_eq!(listed.skipped_invalid, 1);
    let written = inode(&sessions);
    let again = sessions.scan(true);
    assert_eq!(Sessions::titles(&again), Sessions::titles(&listed));
    assert_eq!(again.skipped_invalid, 1);
    assert_eq!(inode(&sessions), written);
    assert_eq!(sessions.cached().count(), 3);
}
