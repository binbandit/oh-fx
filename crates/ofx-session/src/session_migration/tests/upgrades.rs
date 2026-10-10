use super::replacements::{prompts, reply_007, written};
use super::*;

const V4_FRAME: &str = "{\"schema_version\":3,\"seq\":1,\"timestamp_ms\":20,\"event\":{\"user\":{\"text\":\"one\"}}}\n";

fn interrupted_upgrade(fixture: &Fixture, id: &str, new_log: Option<&str>) {
    let log = LegacyLog::started_007(id)
        .turn(&reply_007("one", "first"))
        .turn(&reply_007("two", "second"));
    let dir = log.write(fixture.root.path());
    fs::rename(dir.join("events.jsonl"), dir.join("events.v3.backup")).unwrap();
    if let Some(new_log) = new_log {
        fs::write(dir.join("events.jsonl"), new_log).unwrap();
        fs::set_permissions(dir.join("events.jsonl"), fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn opened(fixture: &Fixture, id: &str) -> PrivateDir {
    PrivateDir::open_existing(fixture.root.path())
        .unwrap()
        .unwrap()
        .open_child(id)
        .unwrap()
        .unwrap()
}

#[test]
fn an_upgrade_fx_left_unfinished_is_read_from_the_log_it_set_aside() {
    let fixture = Fixture::new();
    for (id, new_log) in [
        ("legacy-swapped", Some(V4_FRAME)),
        ("legacy-moved-aside", None),
    ] {
        interrupted_upgrade(&fixture, id, new_log);
        let dir = opened(&fixture, id);
        assert_eq!(source_log(&dir, id).unwrap(), "events.v3.backup");
        let summary = summarize_schema_v3(&dir, id).unwrap().unwrap();
        assert_eq!(summary.history_len, 2);
        assert_eq!(
            schema_v3_watermark(&dir, id).unwrap(),
            Some(format!("commit.{GENERATION}.json"))
        );
        let converted = read_schema_v3(&dir, id).unwrap().unwrap();
        let copy = fixture.root.path().join("copies").join(id);
        fs::create_dir_all(&copy).unwrap();
        for path in [copy.parent().unwrap(), copy.as_path()] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        converted
            .write(&PrivateDir::open_existing(&copy).unwrap().unwrap())
            .unwrap();
        let events: Vec<ConversationEvent> = fs::read_to_string(copy.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| {
                crate::session_event::decode_conversation_frame(format!("{line}\n").as_bytes())
                    .unwrap()
                    .event
            })
            .collect();
        assert_eq!(prompts(&events), ["one", "two"]);
    }
}

#[test]
fn an_upgrade_fx_finished_is_read_from_its_new_log() {
    let fixture = Fixture::new();
    let log = LegacyLog::started_007("legacy-upgraded").turn(&reply_007("one", "first"));
    let (_, metadata) = written(&fixture, &log);
    let dir = log.write(fixture.root.path());
    fs::rename(dir.join("events.jsonl"), dir.join("events.v3.backup")).unwrap();
    fs::copy(
        fixture
            .root
            .path()
            .join("copies/legacy-upgraded/events.jsonl"),
        dir.join("events.jsonl"),
    )
    .unwrap();
    fs::write(
        dir.join("session.json"),
        crate::session_codec::encode_session_metadata(&metadata).unwrap(),
    )
    .unwrap();
    let opened = opened(&fixture, "legacy-upgraded");
    assert!(!holds_schema_v3(&opened, "legacy-upgraded").unwrap());
    assert_eq!(
        source_log(&opened, "legacy-upgraded").unwrap(),
        "events.jsonl"
    );
    assert_eq!(
        schema_v3_watermark(&opened, "legacy-upgraded").unwrap(),
        None
    );
    let saved = fs::read_to_string(dir.join("session.json")).unwrap();
    fs::write(
        dir.join("session.json"),
        saved.replacen('{', "{\"from_a_later_fx\":1,", 1),
    )
    .unwrap();
    assert!(!holds_schema_v3(&opened, "legacy-upgraded").unwrap());
    assert_eq!(
        source_log(&opened, "legacy-upgraded").unwrap(),
        "events.jsonl"
    );
}
