use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;

use ofx_config::PrivateDir;

use super::*;
use crate::session_manager::Manager;
use crate::session_manager::log::check_line;

const BLOB: &str = "a7a3b7c8bf7e5f4b6d1f1b4b42b2e0b5b5d3c2c95e4c1b0dbcb31c9c2bd6d1f7";

struct Root {
    directory: tempfile::TempDir,
}

impl Root {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> PathBuf {
        self.directory.path().join("sessions").join("v2")
    }

    fn open(&self) -> Session {
        Session::open_new(&self.path(), "/w", Host::Ask).unwrap()
    }

    fn manager(&self) -> Manager {
        Manager::new(PrivateDir::open_existing(&self.path()).unwrap().unwrap())
    }

    fn log(&self, id: &str) -> Vec<u8> {
        fs::read(self.path().join(id).join(LOG_NAME)).unwrap()
    }

    fn kinds(&self, id: &str) -> Vec<(u64, &'static str)> {
        self.log(id)
            .split_inclusive(|&byte| byte == b'\n')
            .map(|line| {
                let header = check_line(line).unwrap();
                (header.seq, header.kind.unwrap().name())
            })
            .collect()
    }

    fn index_puts(&self) -> Vec<String> {
        fs::read_to_string(self.path().join("index.jsonl"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

fn user_item(data: &[u8]) -> Event<'_> {
    Event::Item {
        item_type: "user",
        data,
        blobs: &[],
    }
}

#[test]
fn a_new_session_touches_nothing_until_its_first_turn_publishes_it_whole() {
    let root = Root::new();
    let mut session = root.open();
    let seeded = session.append(&[
        Event::Set(SetKey::Prefs, b"{\"model\":\"m\"}"),
        Event::Set(SetKey::Language, b"\"und-Latn\""),
    ]);
    assert_eq!(seeded, Ok(3));
    assert!(!root.directory.path().join("sessions").exists());
    let published = session.append(&[
        Event::TurnStarted,
        user_item(b"{\"text\":\"hi\"}"),
        Event::TurnCommitted,
    ]);
    assert_eq!(published, Ok(6));
    let id = session.id().to_owned();
    assert_eq!(
        root.kinds(&id),
        [
            (1, "session_created"),
            (2, "set"),
            (3, "set"),
            (4, "turn_started"),
            (5, "item"),
            (6, "turn_committed")
        ]
    );
    let folder = root.path().join(&id);
    assert_eq!(mode(&root.path()), 0o700);
    assert_eq!(mode(&folder), 0o700);
    assert_eq!(mode(&folder.join("blobs")), 0o700);
    assert_eq!(mode(&folder.join(LOG_NAME)), 0o600);
    assert_eq!(mode(&folder.join(LOCK_NAME)), 0o600);
    assert_eq!(fs::read_dir(root.path().join(TMP_DIR)).unwrap().count(), 0);
    let verified = root.manager().verify(&id).unwrap();
    assert_eq!(verified.damaged_at, None);
    let peeked = root.manager().peek(&id).unwrap();
    assert_eq!(peeked.state.turns(), 1);
    assert_eq!(
        peeked.state.setting(SetKey::Prefs),
        Some(&b"{\"model\":\"m\"}"[..])
    );
    let listed = root.manager().list(None, 10).unwrap();
    assert_eq!(listed.items.len(), 1);
    let summary = &listed.items[0];
    assert_eq!((summary.id.as_str(), summary.turns), (id.as_str(), 1));
    assert_eq!(summary.host, Host::Ask);
    assert_eq!(summary.opened_ms[Host::Ask as usize], summary.updated_ms);
    assert_eq!(summary.language.as_deref(), Some(&b"\"und-Latn\""[..]));
}

#[test]
fn closing_a_held_session_drops_its_settings_and_writes_nothing() {
    let root = Root::new();
    let mut session = root.open();
    session
        .append(&[Event::Set(SetKey::Title, b"\"kept in memory\"")])
        .unwrap();
    assert_eq!(session.close(), Ok(()));
    assert_eq!(session.close(), Ok(()));
    assert!(!root.directory.path().join("sessions").exists());
    assert_eq!(
        session.append(&[Event::TurnStarted]),
        Err(AppendError::SessionClosed)
    );
}

#[test]
fn close_interrupts_an_open_turn_writes_closed_and_refuses_later_calls() {
    let root = Root::new();
    let mut session = root.open();
    session
        .append(&[Event::TurnStarted, user_item(b"{}")])
        .unwrap();
    let id = session.id().to_owned();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.path().join(&id).join(LOCK_NAME))
        .unwrap();
    assert_eq!(storage::try_lock(&lock), Ok(false));
    session.close().unwrap();
    assert_eq!(storage::try_lock(&lock), Ok(true));
    assert_eq!(
        root.kinds(&id)[3..],
        [(4, "turn_interrupted"), (5, "closed")]
    );
    assert!(
        String::from_utf8(root.log(&id))
            .unwrap()
            .contains("\"reason\":\"closed\"")
    );
    assert_eq!(
        session.append(&[user_item(b"{}")]),
        Err(AppendError::SessionClosed)
    );
    assert_eq!(session.failure(), Some(AppendError::SessionClosed));
    let puts = root.index_puts();
    assert_eq!(puts.len(), 2);
    assert!(puts[1].contains("\"turns\":1,\"opened\":[0,0,0,0,0]"));
}

#[test]
fn the_index_gets_a_put_when_the_session_publishes_changes_its_listing_and_closes() {
    let root = Root::new();
    let mut session = root.open();
    session.append(&[Event::TurnStarted]).unwrap();
    session.append(&[user_item(b"{}")]).unwrap();
    assert_eq!(root.index_puts().len(), 1);
    session
        .append(&[
            Event::TurnCommitted,
            Event::Set(SetKey::Title, b"\"Named\""),
        ])
        .unwrap();
    session.close().unwrap();
    let puts = root.index_puts();
    assert_eq!(puts.len(), 3);
    assert!(puts[1].contains("\"title\":\"Named\""));
    assert!(puts[0].contains("\"turns\":0,\"opened\":[0,"));
}

#[test]
fn a_held_batch_holds_only_settings_and_names_no_blob() {
    let root = Root::new();
    let mut session = root.open();
    let blobs = [BLOB.to_owned()];
    for refused in [
        &[user_item(b"{}")][..],
        &[Event::TurnCommitted],
        &[
            Event::TurnStarted,
            Event::Item {
                item_type: "user",
                data: b"{}",
                blobs: &blobs,
            },
        ],
    ] {
        assert_eq!(session.append(refused), Err(AppendError::InvalidTransition));
    }
    assert_eq!(session.put_blob(b"x"), Err(AppendError::InvalidTransition));
    assert!(!root.directory.path().join("sessions").exists());
}

#[test]
fn blobs_are_stored_read_only_under_their_hash_once_and_items_name_only_stored_ones() {
    let root = Root::new();
    let mut session = root.open();
    session.append(&[Event::TurnStarted]).unwrap();
    let hash = session.put_blob(b"payload").unwrap();
    assert_eq!(hash, lowercase_hex(&Sha256::digest(b"payload")));
    assert_eq!(session.put_blob(b"payload"), Ok(hash.clone()));
    let blobs = root.path().join(session.id()).join(BLOBS_DIR);
    let names: Vec<_> = fs::read_dir(&blobs)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, std::slice::from_ref(&hash));
    assert_eq!(mode(&blobs.join(&hash)), 0o400);
    assert_eq!(fs::read(blobs.join(&hash)).unwrap(), b"payload");
    let missing = [BLOB.to_owned()];
    assert_eq!(
        session.append(&[Event::Item {
            item_type: "tool_result",
            data: b"{}",
            blobs: &missing,
        }]),
        Err(AppendError::InvalidTransition)
    );
    let stored = [hash];
    session
        .append(&[
            Event::Item {
                item_type: "tool_result",
                data: b"{}",
                blobs: &stored,
            },
            Event::TurnCommitted,
        ])
        .unwrap();
    let id = session.id().to_owned();
    session.close().unwrap();
    let verified = root.manager().verify(&id).unwrap();
    assert_eq!((verified.damaged_at, verified.bad_blobs), (None, 0));
}

#[test]
fn hosts_may_send_only_valid_json_item_types_and_their_own_reasons() {
    let root = Root::new();
    let mut session = root.open();
    session.append(&[Event::TurnStarted]).unwrap();
    let long_language = format!("\"{}\"", "x".repeat(MAX_LANGUAGE_BYTES + 1));
    let long_type = "x".repeat(MAX_ITEM_TYPE_BYTES + 1);
    let item = |item_type, data| Event::Item {
        item_type,
        data,
        blobs: &[],
    };
    for refused in [
        vec![],
        vec![item("user", &b"{\"a\":}"[..])],
        vec![item("user", b" {}")],
        vec![item("user", b"{}\n")],
        vec![item("user", b"")],
        vec![item("User", b"{}")],
        vec![item("", b"{}")],
        vec![item(&long_type, b"{}")],
        vec![Event::TurnInterrupted(Reason::Closed)],
        vec![Event::TurnInterrupted(Reason::Crash)],
        vec![Event::Set(SetKey::Title, b"7")],
        vec![Event::Set(SetKey::Workspace, b"null")],
        vec![Event::Set(SetKey::Language, b"\"\"")],
        vec![Event::Set(SetKey::Language, long_language.as_bytes())],
        vec![Event::Set(SetKey::Prefs, b"{")],
    ] {
        assert_eq!(
            session.append(&refused),
            Err(AppendError::InvalidArgument),
            "{refused:?}"
        );
    }
    let huge = vec![b'1'; MAX_VALUE_BYTES + 1];
    assert_eq!(
        session.append(&[item("user", &huge)]),
        Err(AppendError::TooLarge)
    );
    assert_eq!(
        session.append(&[
            item("tool_result_2", b"[1, {\"a\": null}]"),
            Event::Set(SetKey::Prefs, b"null"),
            Event::Set(SetKey::Language, b"\"und-Cyrl\""),
        ]),
        Ok(5)
    );
}

#[test]
fn a_failed_write_fails_the_session_and_every_later_call_names_its_first_cause() {
    let root = Root::new();
    let mut session = root.open();
    session.append(&[Event::TurnStarted]).unwrap();
    let id = session.id().to_owned();
    session.fail_next_write = Some(IoFault::NoSpaceLeft);
    let failed = AppendError::Fault(IoFault::NoSpaceLeft);
    assert_eq!(session.append(&[user_item(b"{}")]), Err(failed));
    session.fail_next_write = Some(IoFault::Io);
    assert_eq!(session.append(&[user_item(b"{}")]), Err(failed));
    assert_eq!(session.put_blob(b"x"), Err(failed));
    assert_eq!(session.failure(), Some(failed));
    assert_eq!(session.close(), Ok(()));
    assert_eq!(root.kinds(&id).len(), 2);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.path().join(&id).join(LOCK_NAME))
        .unwrap();
    assert_eq!(storage::try_lock(&lock), Ok(true));
}

#[test]
fn a_root_that_cannot_be_created_fails_the_publish_with_its_cause_and_keeps_the_session_held() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("sessions"), b"a file").unwrap();
    let mut session = Session::open_new(
        &directory.path().join("sessions").join("v2"),
        "/w",
        Host::Ask,
    )
    .unwrap();
    session
        .append(&[Event::Set(SetKey::Title, b"\"t\"")])
        .unwrap();
    assert!(matches!(
        session.append(&[Event::TurnStarted]),
        Err(AppendError::Fault(_))
    ));
    assert_eq!(session.failure(), None);
}
