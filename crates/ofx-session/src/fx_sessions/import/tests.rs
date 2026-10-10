use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use ofx_config::{PrivateDir, ProviderId};
use ofx_contract::ReasoningEffort;

use super::super::import_from_fx;
use super::super::tests::{
    Home, Saved, ids, shell_turn, shell_turn_with_an_unknown_event, snapshot,
};
use super::Imported;
use crate::result_store::make_handle;
use crate::session_codec::SavedProvider;
use crate::session_error::SessionError;
use crate::session_event::{AssistantEvent, ConversationEvent, TurnCompletedEvent, UserEvent};
use crate::session_log::WritableSession;
use crate::session_migration::tests::{GENERATION, LegacyLog, command_result, command_turn, reply};
use crate::session_store::{ListScope, SessionStore};

type OwnWork = fn(&Home, WritableSession);

const WORKSPACE: &str = "/work";
const ID: &str = "fx-imported";

fn saved(prompts: &'static [&'static str], modified_s: u64) -> Saved<'static> {
    Saved {
        id: ID,
        workspace: WORKSPACE,
        title: Some("From fx"),
        prompts,
        modified_s,
    }
}

fn add(dir: &Path, relative: &str, bytes: &[u8]) {
    let path = dir.join(relative);
    let mut parent = path.parent().unwrap().to_path_buf();
    fs::create_dir_all(&parent).unwrap();
    while parent != dir {
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        parent = parent.parent().unwrap().to_path_buf();
    }
    fs::write(&path, bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn importing(home: &Home) -> SessionStore {
    home.store(WORKSPACE)
        .with_fx_home(home.path().to_path_buf())
}

fn copy_of(home: &Home, relative: &str) -> Vec<u8> {
    fs::read(home.own_sessions().join(ID).join(relative)).unwrap()
}

fn fx_file(home: &Home, relative: &str) -> Vec<u8> {
    fs::read(home.fx_sessions().join(ID).join(relative)).unwrap()
}

fn staging_left(home: &Home) -> Vec<String> {
    fs::read_dir(home.own_sessions())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("creating+"))
        .collect()
}

fn own_turn(prompt: &str) -> Vec<ConversationEvent> {
    vec![
        ConversationEvent::User(UserEvent::new(prompt)),
        ConversationEvent::Assistant(AssistantEvent {
            text: "ok".to_owned(),
            provider_replay: None,
            standalone_response: false,
        }),
        ConversationEvent::TurnCompleted(TurnCompletedEvent::default()),
    ]
}

#[test]
fn resuming_an_fx_session_copies_what_resume_needs_and_leaves_fx_untouched() {
    let home = Home::new();
    saved(&["one", "two"], 100).write(&home.fx_sessions());
    let source = home.fx_sessions().join(ID);
    for (relative, bytes) in [
        ("tool-results/result-shell-00-11.txt", &b"output"[..]),
        ("logs/commands/fx-command-replay-00112233.bin", b"FXRPLY01"),
        ("images/image-1-abc.bin", b"png"),
        ("artifacts/web-fetch/artifact-1-ab.html", b"<html>"),
        ("permissions.json", b"{\"schema_version\":1}"),
        ("terminal/state/record-1.json", b"{}"),
        ("client/system-prompt.txt", b"prompt"),
        ("owner.live", b"{\"pid\":1}"),
        ("history-cache.bin", b"stale fx cache"),
    ] {
        add(&source, relative, bytes);
    }
    let before = snapshot(&home.fx_profile());

    let session = importing(&home).resume(ID).unwrap();
    assert_eq!(session.id(), ID);
    assert_eq!(session.title(), Some("From fx"));
    drop(session);

    for relative in [
        "session.json",
        "events.jsonl",
        "tool-results/result-shell-00-11.txt",
        "logs/commands/fx-command-replay-00112233.bin",
        "images/image-1-abc.bin",
        "artifacts/web-fetch/artifact-1-ab.html",
        "permissions.json",
    ] {
        assert_eq!(
            copy_of(&home, relative),
            fx_file(&home, relative),
            "{relative}"
        );
    }
    let copy = home.own_sessions().join(ID);
    for left_out in ["terminal", "client"] {
        assert!(!copy.join(left_out).exists(), "{left_out}");
    }
    assert!(copy.join("fx-import.json").exists());
    assert_ne!(copy_of(&home, "history-cache.bin"), b"stale fx cache");
    assert_eq!(
        fs::metadata(copy.join("events.jsonl")).unwrap().mode() & 0o777,
        0o600
    );
    assert_eq!(snapshot(&home.fx_profile()), before);
    assert!(staging_left(&home).is_empty());

    let listed = home.listed(WORKSPACE, ListScope::AllWorkspaces);
    assert_eq!(ids(&listed), [ID]);
    assert_eq!(
        listed.summaries[0].source,
        crate::session_summary_codec::SessionSource::OhFx
    );
}

#[test]
fn resuming_again_reuses_the_copy_while_fx_is_unchanged() {
    let home = Home::new();
    saved(&["one"], 100).write(&home.fx_sessions());
    drop(importing(&home).resume(ID).unwrap());
    let inode = fs::metadata(home.own_sessions().join(ID).join("events.jsonl"))
        .unwrap()
        .ino();
    drop(importing(&home).resume(ID).unwrap());
    assert_eq!(
        fs::metadata(home.own_sessions().join(ID).join("events.jsonl"))
            .unwrap()
            .ino(),
        inode
    );
    assert!(home.store(WORKSPACE).resume(ID).is_ok());
}

#[test]
fn a_newer_fx_session_refreshes_a_copy_without_turns_of_its_own() {
    let home = Home::new();
    saved(&["one"], 100).write(&home.fx_sessions());
    drop(importing(&home).resume(ID).unwrap());
    drop(importing(&home).resume(ID).unwrap());
    saved(&["one", "two in fx"], 200).write(&home.fx_sessions());

    drop(importing(&home).resume(ID).unwrap());
    assert_eq!(
        copy_of(&home, "events.jsonl"),
        fx_file(&home, "events.jsonl")
    );
    assert!(staging_left(&home).is_empty());
    saved(&["one", "two in fx", "three in fx"], 300).write(&home.fx_sessions());
    drop(importing(&home).resume(ID).unwrap());
    assert_eq!(
        copy_of(&home, "events.jsonl"),
        fx_file(&home, "events.jsonl")
    );
}

#[test]
fn a_copy_with_turns_of_its_own_is_kept_when_fx_moves_on() {
    let home = Home::new();
    saved(&["one"], 100).write(&home.fx_sessions());
    let mut session = importing(&home).resume(ID).unwrap();
    session.append(3, &own_turn("asked in oh-fx")).unwrap();
    drop(session);
    saved(&["one", "two in fx"], 200).write(&home.fx_sessions());

    drop(importing(&home).resume(ID).unwrap());
    let log = String::from_utf8(copy_of(&home, "events.jsonl")).unwrap();
    assert!(log.contains("asked in oh-fx"), "{log}");
    assert!(!log.contains("two in fx"), "{log}");
}

#[test]
fn an_unfinished_fx_turn_is_closed_in_the_copy_and_still_refreshes() {
    let home = Home::new();
    let open_turn = saved(&[], 100);
    let unfinished: String = shell_turn("null", "null")
        .lines()
        .take(3)
        .map(|line| line.to_owned() + "\n")
        .collect();
    open_turn.write_files(&home.fx_sessions(), &open_turn.manifest(), &unfinished);
    drop(importing(&home).resume(ID).unwrap());
    assert_ne!(
        copy_of(&home, "events.jsonl"),
        fx_file(&home, "events.jsonl")
    );

    saved(&["finished in fx"], 200).write(&home.fx_sessions());
    drop(importing(&home).resume(ID).unwrap());
    assert_eq!(
        copy_of(&home, "events.jsonl"),
        fx_file(&home, "events.jsonl")
    );
}

#[test]
fn a_session_fx_holds_open_is_refused_without_a_trace() {
    let home = Home::new();
    saved(&["one"], 100).write(&home.fx_sessions());
    let source = PrivateDir::open_existing(&home.fx_sessions().join(ID))
        .unwrap()
        .unwrap();
    let held = source.try_lock("session.lock").unwrap().unwrap();
    assert_eq!(
        importing(&home).resume(ID).err(),
        Some(SessionError::FxSessionOpen)
    );
    assert!(!home.own_sessions().join(ID).exists());
    assert!(staging_left(&home).is_empty());
    drop(held);
    assert!(importing(&home).resume(ID).is_ok());

    saved(&["one", "two in fx"], 200).write(&home.fx_sessions());
    let held = source.try_lock("session.lock").unwrap().unwrap();
    assert_eq!(
        importing(&home).resume(ID).err(),
        Some(SessionError::FxSessionOpen)
    );
    drop(held);
}

#[test]
fn an_fx_session_without_a_lock_file_is_imported() {
    let home = Home::new();
    saved(&["one"], 100).write(&home.fx_sessions());
    fs::remove_file(home.fx_sessions().join(ID).join("session.lock")).unwrap();
    assert!(importing(&home).resume(ID).is_ok());
    assert!(!home.fx_sessions().join(ID).join("session.lock").exists());
}

#[test]
fn an_unfinished_fx_log_rewrite_is_refused_but_its_freshness_pin_is_not() {
    for leftover in ["events-compaction.pending", "events.jsonl.compact-tmp"] {
        let home = Home::new();
        saved(&["one"], 100).write(&home.fx_sessions());
        add(&home.fx_sessions().join(ID), leftover, b"");
        assert_eq!(
            importing(&home).resume(ID).err(),
            Some(SessionError::FxCompactionUnfinished),
            "{leftover}"
        );
        assert!(!home.own_sessions().join(ID).exists());
    }
    let home = Home::new();
    saved(&["one"], 100).write(&home.fx_sessions());
    add(
        &home.fx_sessions().join(ID),
        "events-compaction.marker",
        b"4097\n123\n",
    );
    assert!(importing(&home).resume(ID).is_ok());
    assert!(
        !home
            .own_sessions()
            .join(ID)
            .join("events-compaction.marker")
            .exists()
    );
}

fn codex() -> SavedProvider {
    SavedProvider::new(ProviderId::Codex, None).unwrap()
}

#[test]
fn work_done_in_oh_fx_keeps_the_copy_when_fx_moves_on() {
    let keep: [(&str, OwnWork); 8] = [
        ("paused", |home, session| {
            drop(session);
            add(
                &home.own_sessions().join(ID),
                "recovery.json",
                b"{\"conversation_seq\":0,\"checkpoint\":{}}",
            );
        }),
        ("renamed", |_, mut session| {
            session.rename("Renamed in oh-fx").unwrap();
        }),
        ("child", |home, session| {
            drop(session);
            fs::create_dir(home.own_sessions().join(ID).join("subagent")).unwrap();
        }),
        ("model", |_, mut session| {
            session
                .select_model("openai/gpt-5-mini", None, false, None)
                .unwrap();
        }),
        ("effort", |_, mut session| {
            let low = ReasoningEffort::parse("low").unwrap();
            session
                .select_model("openai/gpt-5", Some(&low), false, None)
                .unwrap();
        }),
        ("fast mode", |_, mut session| {
            session
                .select_model("openai/gpt-5", None, true, None)
                .unwrap();
        }),
        ("provider", |_, mut session| {
            session.select_provider(codex(), "gpt-5.5").unwrap();
        }),
        ("ultra request", |_, mut session| {
            session
                .select_model("openai/gpt-5", None, false, Some(true))
                .unwrap();
        }),
    ];
    for (case, own_work) in keep {
        let home = Home::new();
        saved(&["one"], 100).write(&home.fx_sessions());
        own_work(&home, importing(&home).resume(ID).unwrap());
        let kept = copy_of(&home, "events.jsonl");
        saved(&["one", "two in fx"], 200).write(&home.fx_sessions());
        drop(importing(&home).resume(ID).unwrap());
        assert_eq!(copy_of(&home, "events.jsonl"), kept, "{case}");
    }
}

#[test]
fn a_choice_that_changes_nothing_still_lets_fx_refresh_the_copy() {
    let home = Home::new();
    saved(&["one"], 100).write(&home.fx_sessions());
    let mut session = importing(&home).resume(ID).unwrap();
    session
        .select_model("openai/gpt-5", None, false, Some(false))
        .unwrap();
    drop(session);
    let record = String::from_utf8(copy_of(&home, "fx-import.json")).unwrap();
    assert!(!record.contains("ultrafast_mode"), "{record}");
    saved(&["one", "two in fx"], 200).write(&home.fx_sessions());

    drop(importing(&home).resume(ID).unwrap());
    assert_eq!(
        copy_of(&home, "events.jsonl"),
        fx_file(&home, "events.jsonl")
    );
}

#[test]
fn a_provider_rebind_on_resume_keeps_the_copy_refreshable_until_oh_fx_works_in_it() {
    let home = Home::new();
    saved(&["one"], 100).write(&home.fx_sessions());
    let mut session = importing(&home).resume(ID).unwrap();
    session.rebind_provider(codex(), "gpt-5.5").unwrap();
    drop(session);
    saved(&["one", "two in fx"], 200).write(&home.fx_sessions());

    let mut session = importing(&home).resume(ID).unwrap();
    assert_eq!(
        copy_of(&home, "events.jsonl"),
        fx_file(&home, "events.jsonl")
    );
    assert_eq!(
        copy_of(&home, "session.json"),
        fx_file(&home, "session.json")
    );
    session.rebind_provider(codex(), "gpt-5.5").unwrap();
    session.append(3, &own_turn("asked in oh-fx")).unwrap();
    session.rebind_provider(codex(), "gpt-5.5").unwrap();
    drop(session);
    saved(&["one", "two in fx", "three in fx"], 300).write(&home.fx_sessions());

    drop(importing(&home).resume(ID).unwrap());
    let log = String::from_utf8(copy_of(&home, "events.jsonl")).unwrap();
    assert!(log.contains("asked in oh-fx"), "{log}");
    assert!(!log.contains("three in fx"), "{log}");
}

#[test]
fn a_newer_fx_session_oh_fx_cannot_read_leaves_the_copy_to_resume() {
    let home = Home::new();
    saved(&["one"], 100).write(&home.fx_sessions());
    drop(importing(&home).resume(ID).unwrap());
    let kept = copy_of(&home, "events.jsonl");
    let replayed = saved(&[], 200);
    replayed.write_files(
        &home.fx_sessions(),
        &replayed.manifest(),
        &shell_turn_with_an_unknown_event(),
    );
    assert!(importing(&home).resume(ID).is_ok());
    assert_eq!(copy_of(&home, "events.jsonl"), kept);
    assert!(staging_left(&home).is_empty());
}

#[test]
fn an_fx_session_oh_fx_cannot_resume_is_refused() {
    let home = Home::new();
    let replayed = saved(&[], 100);
    replayed.write_files(
        &home.fx_sessions(),
        &replayed.manifest(),
        &shell_turn_with_an_unknown_event(),
    );
    assert_eq!(
        importing(&home).resume(ID).err(),
        Some(SessionError::FxSessionUnreadable)
    );
    assert!(!home.own_sessions().join(ID).exists());
    assert!(staging_left(&home).is_empty());
}

#[test]
fn a_copy_that_fails_partway_leaves_no_staging() {
    let home = Home::new();
    saved(&["one"], 100).write(&home.fx_sessions());
    let unreadable = home
        .fx_sessions()
        .join(ID)
        .join("tool-results/result-x.txt");
    add(
        &home.fx_sessions().join(ID),
        "tool-results/result-x.txt",
        b"x",
    );
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(importing(&home).resume(ID).is_err());
    assert!(!home.own_sessions().join(ID).exists());
    assert!(staging_left(&home).is_empty());
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn without_an_fx_home_or_an_fx_session_nothing_is_imported() {
    let home = Home::new();
    saved(&["one"], 100).write(&home.fx_sessions());
    assert_eq!(
        home.store(WORKSPACE).resume(ID).err(),
        Some(SessionError::SessionNotFound)
    );
    assert_eq!(
        importing(&home).resume("not-in-fx").err(),
        Some(SessionError::SessionNotFound)
    );
    assert!(staging_left(&home).is_empty());
}

fn own_dir(home: &Home) -> PrivateDir {
    PrivateDir::open_existing(&home.own_sessions())
        .unwrap()
        .unwrap()
}

fn imported_from_fx(home: &Home) -> Imported {
    import_from_fx(home.path(), &own_dir(home), ID)
        .unwrap()
        .unwrap()
}

#[test]
fn no_other_resume_can_save_into_a_copy_before_its_import_baseline_is_recorded() {
    let home = Home::new();
    saved(&["one"], 100).write(&home.fx_sessions());
    let store = importing(&home);
    let published = imported_from_fx(&home);
    assert_eq!(
        store.open_without_waiting(ID).err(),
        Some(SessionError::SessionBusy)
    );
    drop(store.open_imported(ID, published).unwrap());

    saved(&["one", "two in fx"], 200).write(&home.fx_sessions());
    let refreshed = imported_from_fx(&home);
    assert_eq!(
        store.open_without_waiting(ID).err(),
        Some(SessionError::SessionBusy)
    );
    drop(store.open_imported(ID, refreshed).unwrap());
    let mut other = store.open_without_waiting(ID).unwrap();
    other
        .append(3, &own_turn("asked in another oh-fx"))
        .unwrap();
    drop(other);

    saved(&["one", "two in fx", "three in fx"], 300).write(&home.fx_sessions());
    drop(importing(&home).resume(ID).unwrap());
    let log = String::from_utf8(copy_of(&home, "events.jsonl")).unwrap();
    assert!(log.contains("two in fx"), "{log}");
    assert!(log.contains("asked in another oh-fx"), "{log}");
    assert!(!log.contains("three in fx"), "{log}");
}

#[test]
fn a_copy_published_without_its_import_baseline_is_never_refreshed() {
    let home = Home::new();
    saved(&["one"], 100).write(&home.fx_sessions());
    let store = importing(&home);
    drop(imported_from_fx(&home));
    let kept = copy_of(&home, "events.jsonl");
    assert!(!home.own_sessions().join(ID).join("fx-import.json").exists());

    saved(&["one", "two in fx"], 200).write(&home.fx_sessions());
    drop(store.resume(ID).unwrap());
    assert_eq!(copy_of(&home, "events.jsonl"), kept);
    assert!(!home.own_sessions().join(ID).join("fx-import.json").exists());
}

const FX_HANDLE: &str = "result-run_command-0123456789abcdef-fedcba9876543210.txt";

fn legacy() -> LegacyLog {
    LegacyLog::started(ID, WORKSPACE)
        .turn(&reply("first prompt here", "first answer"))
        .turn(&command_turn(
            "list files",
            "call_1",
            &command_result("call_1", "a b", "null"),
            "done",
        ))
        .turn(&command_turn(
            "show more",
            "call_2",
            &command_result("call_2", "head", &format!("\"{FX_HANDLE}\"")),
            "shown",
        ))
}

fn tool_result(event: &ConversationEvent) -> &crate::session_event::ToolResultEvent {
    match event {
        ConversationEvent::ToolResult(result) => result,
        other => panic!("{other:?}"),
    }
}

#[test]
fn resuming_a_session_fx_saved_before_0_0_8_converts_it_into_the_copy() {
    let home = Home::new();
    let source = legacy().titled("Old work").write(&home.fx_sessions());
    add(
        &source,
        &format!("tool-results/{FX_HANDLE}"),
        b"head and the rest",
    );
    let before = snapshot(&home.fx_profile());

    let session = importing(&home).resume(ID).unwrap();
    assert_eq!(session.id(), ID);
    assert_eq!(session.title(), Some("first prompt here"));
    drop(session);
    assert_eq!(snapshot(&home.fx_profile()), before);
    assert!(staging_left(&home).is_empty());

    let copy = home.own_sessions().join(ID);
    for left_out in [
        "authority.json".to_owned(),
        "commit.lock".to_owned(),
        "display.json".to_owned(),
        format!("commit.{GENERATION}.json"),
    ] {
        assert!(!copy.join(&left_out).exists(), "{left_out}");
    }
    assert_eq!(
        copy_of(&home, &format!("tool-results/{FX_HANDLE}")),
        b"head and the rest"
    );
    let saved = home.store(WORKSPACE).load(ID).unwrap();
    let metadata = &saved.metadata;
    assert_eq!(metadata.preferences.provider.id(), &ProviderId::Gateway);
    assert_eq!(metadata.preferences.model, "openai/gpt-5");
    assert_eq!(metadata.preferences.effort, ReasoningEffort::Auto);
    assert_eq!(metadata.created_at_ms, 10);
    assert_eq!(metadata.updated_at_ms, 100);
    assert_eq!(metadata.workspace_root, WORKSPACE);
    assert_eq!(metadata.title.as_deref(), Some("first prompt here"));
    let turns: Vec<_> = saved
        .history
        .turns
        .iter()
        .map(|turn| &turn.events)
        .collect();
    assert_eq!(turns.len(), 3);
    assert_eq!(
        turns[0],
        &[
            ConversationEvent::User(UserEvent::new("first prompt here")),
            ConversationEvent::Assistant(AssistantEvent {
                text: "first answer".to_owned(),
                provider_replay: None,
                standalone_response: false,
            }),
            ConversationEvent::TurnCompleted(TurnCompletedEvent::default()),
        ]
    );
    let inline = tool_result(&turns[1][3]);
    let handle = make_handle("call_1", "run_command", "a b");
    assert_eq!(inline.artifact_ref, handle);
    assert_eq!(inline.stored_bytes, 3);
    assert_eq!(inline.output_bytes, Some(3));
    assert_eq!(
        inline.completeness,
        crate::session_event::ArtifactCompleteness::Partial
    );
    assert_eq!(inline.preview.as_deref(), Some("a b"));
    assert_eq!(inline.created_at_ms, 15);
    assert_eq!(copy_of(&home, &format!("tool-results/{handle}")), b"a b");
    let ConversationEvent::TurnCompleted(closed) = &turns[1][5] else {
        panic!("{:?}", turns[1]);
    };
    assert_eq!(closed.files.len(), 1);
    assert_eq!(closed.files[0].path, "src/main.rs");
    let spilled = tool_result(&turns[2][3]);
    assert_eq!(spilled.artifact_ref, FX_HANDLE);
    assert_eq!(spilled.stored_bytes, 4);
    assert_eq!(
        spilled.completeness,
        crate::session_event::ArtifactCompleteness::Complete
    );

    drop(importing(&home).resume(ID).unwrap());
    assert_eq!(home.store(WORKSPACE).load(ID).unwrap(), saved);
    assert_eq!(snapshot(&home.fx_profile()), before);
    let listed = home.listed(WORKSPACE, ListScope::AllWorkspaces);
    assert_eq!(ids(&listed), [ID]);
}

#[test]
fn a_schema_v3_session_without_its_manifest_imports_and_follows_fx_until_used() {
    let home = Home::new();
    let source = legacy().write(&home.fx_sessions());
    fs::remove_file(source.join("session.json")).unwrap();

    drop(importing(&home).resume(ID).unwrap());
    let imported = copy_of(&home, "events.jsonl");
    drop(importing(&home).resume(ID).unwrap());
    assert_eq!(copy_of(&home, "events.jsonl"), imported);

    let source = legacy()
        .turn(&reply("asked later in fx", "answered"))
        .write(&home.fx_sessions());
    fs::remove_file(source.join("session.json")).unwrap();
    drop(importing(&home).resume(ID).unwrap());
    let refreshed = home.store(WORKSPACE).load(ID).unwrap();
    assert_eq!(refreshed.history.turns.len(), 4);
}

#[test]
fn a_schema_v3_session_oh_fx_cannot_convert_yet_is_refused() {
    let home = Home::new();
    legacy()
        .turn(&reply("look", "seen").replace(
                "\"images\":[]",
                "\"images\":[{\"id\":1,\"path\":\"/tmp/a.png\",\"media_type\":\"image/png\",\"snapshot_path\":null,\"snapshot_sha256\":null}]",
            ))
        .write(&home.fx_sessions());
    let before = snapshot(&home.fx_profile());
    assert_eq!(
        importing(&home).resume(ID).err(),
        Some(SessionError::FxSessionUnreadable)
    );
    assert!(!home.own_sessions().join(ID).exists());
    assert!(staging_left(&home).is_empty());
    assert_eq!(snapshot(&home.fx_profile()), before);
}

#[test]
fn a_watermark_fx_publishes_later_refreshes_an_untouched_copy() {
    let home = Home::new();
    let pending = || {
        LegacyLog::started(ID, WORKSPACE)
            .turn(&reply("acknowledged", "kept"))
            .turn(&reply("written before fx stopped", "published later"))
    };
    pending().committed_through(4).write(&home.fx_sessions());
    drop(importing(&home).resume(ID).unwrap());
    assert_eq!(
        home.store(WORKSPACE).load(ID).unwrap().history.turns.len(),
        1
    );
    let events = fx_file(&home, "events.jsonl");
    let manifest = fx_file(&home, "session.json");

    let watermark = home
        .fx_sessions()
        .join(ID)
        .join(format!("commit.{GENERATION}.json"));
    fs::write(&watermark, pending().watermark()).unwrap();
    assert_eq!(fx_file(&home, "events.jsonl"), events);
    assert_eq!(fx_file(&home, "session.json"), manifest);
    drop(importing(&home).resume(ID).unwrap());
    assert_eq!(
        home.store(WORKSPACE).load(ID).unwrap().history.turns.len(),
        2
    );
}

#[test]
fn resuming_a_compacted_schema_v3_session_restores_its_summary() {
    let home = Home::new();
    LegacyLog::started(ID, WORKSPACE)
        .turn(&reply("one", "first"))
        .turn(&reply("two", "second"))
        .turn("{\"kind\":\"compacted_summary\",\"summary\":\"Earlier: one and two.\",\"removed_turn_count\":2,\"compaction_count\":1}")
        .turn(&reply("three", "third"))
        .write(&home.fx_sessions());
    let before = snapshot(&home.fx_profile());

    drop(importing(&home).resume(ID).unwrap());
    let history = home.store(WORKSPACE).load(ID).unwrap().history;
    let compacted = history.compacted.unwrap();
    assert_eq!(compacted.summary, "Earlier: one and two.");
    let prompts: Vec<&str> = history
        .turns
        .iter()
        .filter_map(|turn| match turn.events.first() {
            Some(ConversationEvent::User(user)) => Some(user.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(prompts, ["three"]);
    assert_eq!(snapshot(&home.fx_profile()), before);
}

#[test]
fn resuming_a_schema_v3_edit_keeps_its_diff_and_command_replay() {
    let home = Home::new();
    let previous = "a\n".repeat(3_000);
    let presentation = format!(
        "{{\"path\":\"src/main.rs\",\"kind\":\"edited\",\"lines\":[{{\"kind\":\"addition\",\"old_line\":null,\"new_line\":3001,\"text\":\"b\"}}],\"additions\":1,\"deletions\":0,\"truncated\":false,\"previous_content\":{},\"after_content\":{},\"lifecycle_id\":null}}",
        serde_json::to_string(&previous).unwrap(),
        serde_json::to_string(&format!("{previous}b\n")).unwrap()
    );
    let edit = command_result("call_1", "edited", "null")
        .replace("\"run_command\"", "\"edit_file\"")
        .replace(
            "\"committed_file_presentation\":null",
            &format!("\"committed_file_presentation\":{presentation}"),
        )
        .replace(
            "\"command_output_replay\":null",
            "\"command_output_replay\":{\"kind\":\"available\",\"handle\":\"fx-command-replay-00112233.bin\",\"framed_bytes\":8}",
        );
    LegacyLog::started(ID, WORKSPACE)
        .turn(
            &command_turn("edit it", "call_1", &edit, "done")
                .replace("\"name\":\"run_command\"", "\"name\":\"edit_file\""),
        )
        .write(&home.fx_sessions());
    add(
        &home.fx_sessions().join(ID),
        "logs/commands/fx-command-replay-00112233.bin",
        b"FXRPLY01",
    );
    let before = snapshot(&home.fx_profile());

    drop(importing(&home).resume(ID).unwrap());
    let history = home.store(WORKSPACE).load(ID).unwrap().history;
    let result = tool_result(&history.turns[0].events[3]);
    let change = result.file_change().unwrap();
    assert_eq!(change.path, "src/main.rs");
    let pack_handle = result
        .committed_file_presentation
        .as_ref()
        .and_then(|presentation| presentation.content_handle.clone())
        .unwrap();
    let pack = copy_of(&home, &format!("tool-results/{pack_handle}"));
    assert!(pack.starts_with(b"{\"previous_content\":\"a\\na\\n"));
    assert_eq!(
        result.command_replay_ref.as_deref(),
        Some("fx-command-replay-00112233.bin")
    );
    assert_eq!(
        copy_of(&home, "logs/commands/fx-command-replay-00112233.bin"),
        b"FXRPLY01"
    );
    assert_eq!(snapshot(&home.fx_profile()), before);
}

#[test]
fn resuming_a_schema_v3_session_fx_was_upgrading_reads_the_log_it_set_aside() {
    let home = Home::new();
    let dir = LegacyLog::started(ID, WORKSPACE)
        .turn(&reply("one", "first"))
        .write(&home.fx_sessions());
    fs::rename(dir.join("events.jsonl"), dir.join("events.v3.backup")).unwrap();
    add(
        &dir,
        "events.jsonl",
        b"{\"schema_version\":3,\"seq\":1,\"timestamp_ms\":20,\"event\":{\"user\":{\"text\":\"one\"}}}\n",
    );
    let before = snapshot(&home.fx_profile());

    drop(importing(&home).resume(ID).unwrap());
    let history = home.store(WORKSPACE).load(ID).unwrap().history;
    assert_eq!(
        history.turns[0].events.first(),
        Some(&ConversationEvent::User(UserEvent::new("one")))
    );
    assert_eq!(snapshot(&home.fx_profile()), before);
}
