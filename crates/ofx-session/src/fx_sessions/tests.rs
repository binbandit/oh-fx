use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use super::*;
use crate::session_migration::tests::{LegacyLog, reply};
use crate::session_store::{ListScope, SessionStore};
use crate::session_summary_codec::ResumablePage;

pub(super) struct Saved<'a> {
    pub(super) id: &'a str,
    pub(super) workspace: &'a str,
    pub(super) title: Option<&'a str>,
    pub(super) prompts: &'a [&'a str],
    pub(super) modified_s: u64,
}

impl Saved<'_> {
    pub(super) fn manifest(&self) -> String {
        let title = self
            .title
            .map(|title| format!("\"title\":\"{title}\","))
            .unwrap_or_default();
        format!(
            "{{\"schema_version\":4,\"id\":\"{}\",\"origin_workspace_root\":\"{workspace}\",\"workspace_root\":\"{workspace}\",\"created_at_ms\":1,\"updated_at_ms\":2,\"conversation_language\":\"en\",\"provider\":\"gateway\",\"model\":\"openai/gpt-5\",\"effort\":\"auto\",\"fast_mode\":false,{title}\"subagent_child\":false}}",
            self.id,
            workspace = self.workspace,
        )
    }

    pub(super) fn events(&self) -> String {
        let mut log = String::new();
        for (turn, prompt) in (0_u64..).zip(self.prompts) {
            for (offset, event) in [
                (1, format!("{{\"user\":{{\"text\":\"{prompt}\",\"images\":[],\"work_id\":null}}}}")),
                (2, "{\"assistant\":{\"text\":\"done\",\"provider_replay\":null,\"standalone_response\":false}}".to_owned()),
                (3, "{\"turn_completed\":{\"files\":[],\"turn_summary\":null}}".to_owned()),
            ] {
                let _ = writeln!(
                    log,
                    "{{\"schema_version\":3,\"seq\":{},\"timestamp_ms\":2,\"event\":{event}}}",
                    turn * 3 + offset
                );
            }
        }
        log
    }

    pub(super) fn write(&self, sessions: &Path) {
        self.write_with(sessions, &self.manifest());
    }

    pub(super) fn write_with(&self, sessions: &Path, manifest: &str) {
        self.write_files(sessions, manifest, &self.events());
    }

    pub(super) fn write_files(&self, sessions: &Path, manifest: &str, events: &str) {
        let dir = sessions.join(self.id);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        for (name, bytes) in [
            ("session.json", manifest.to_owned()),
            ("events.jsonl", events.to_owned()),
            ("session.lock", String::new()),
        ] {
            let path = dir.join(name);
            fs::write(&path, bytes).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        fs::File::options()
            .write(true)
            .open(dir.join("events.jsonl"))
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(self.modified_s))
            .unwrap();
    }
}

pub(super) struct Home {
    root: tempfile::TempDir,
}

impl Home {
    pub(super) fn new() -> Self {
        let home = Self {
            root: tempfile::tempdir().unwrap(),
        };
        for dir in [home.fx_profile(), home.fx_sessions()] {
            fs::create_dir_all(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        home
    }

    pub(super) fn path(&self) -> &Path {
        self.root.path()
    }

    pub(super) fn fx_profile(&self) -> PathBuf {
        self.path().join(".fx")
    }

    pub(super) fn fx_sessions(&self) -> PathBuf {
        self.fx_profile().join("sessions")
    }

    pub(super) fn own_sessions(&self) -> PathBuf {
        self.path().join("data/oh-fx/sessions")
    }

    pub(super) fn store(&self, workspace: &str) -> SessionStore {
        SessionStore::open(&self.path().join("data/oh-fx"), workspace).unwrap()
    }

    pub(super) fn listed(&self, workspace: &str, scope: ListScope) -> ResumablePage {
        self.store(workspace)
            .catalog_with_fx(&FxSessions::open(self.path()))
            .unwrap()
            .listed_page(scope, None, 50)
    }
}

pub(super) fn ids(page: &ResumablePage) -> Vec<&str> {
    page.summaries
        .iter()
        .map(|summary| summary.id.as_str())
        .collect()
}

fn sources(page: &ResumablePage) -> Vec<SessionSource> {
    page.summaries
        .iter()
        .map(|summary| summary.source)
        .collect()
}

#[test]
fn fx_sessions_are_listed_beside_oh_fx_sessions_newest_first_and_marked() {
    let home = Home::new();
    home.store("/work");
    Saved {
        id: "own-session",
        workspace: "/work",
        title: Some("Here"),
        prompts: &["one"],
        modified_s: 200,
    }
    .write(&home.own_sessions());
    Saved {
        id: "fx-newer",
        workspace: "/work",
        title: Some("Newer in fx"),
        prompts: &["two", "three"],
        modified_s: 300,
    }
    .write(&home.fx_sessions());
    Saved {
        id: "fx-older",
        workspace: "/work",
        title: None,
        prompts: &["four"],
        modified_s: 100,
    }
    .write(&home.fx_sessions());
    Saved {
        id: "fx-elsewhere",
        workspace: "/elsewhere",
        title: None,
        prompts: &["five"],
        modified_s: 400,
    }
    .write(&home.fx_sessions());

    let here = home.listed("/work", ListScope::CurrentWorkspace);
    assert_eq!(ids(&here), ["fx-newer", "own-session", "fx-older"]);
    assert_eq!(
        sources(&here),
        [SessionSource::Fx, SessionSource::OhFx, SessionSource::Fx]
    );
    let newer = &here.summaries[0];
    assert_eq!(newer.title.as_deref(), Some("Newer in fx"));
    assert_eq!(newer.history_len, 2);
    assert_eq!(newer.updated_at_ms, 300_000);
    assert_eq!(newer.workspace_root, "/work");

    let everywhere = home.listed("/work", ListScope::AllWorkspaces);
    assert_eq!(
        ids(&everywhere),
        ["fx-elsewhere", "fx-newer", "own-session", "fx-older"]
    );
    assert_eq!(SessionSource::Fx.marker(), Some("fx"));
    assert_eq!(SessionSource::OhFx.marker(), None);
}

#[test]
fn a_session_saved_in_both_stores_lists_only_the_oh_fx_copy() {
    let home = Home::new();
    home.store("/work");
    for (sessions, title) in [
        (home.own_sessions(), "Copied"),
        (home.fx_sessions(), "Original"),
    ] {
        Saved {
            id: "shared-id",
            workspace: "/work",
            title: Some(title),
            prompts: &["one"],
            modified_s: 100,
        }
        .write(&sessions);
    }
    let page = home.listed("/work", ListScope::AllWorkspaces);
    assert_eq!(ids(&page), ["shared-id"]);
    assert_eq!(page.summaries[0].title.as_deref(), Some("Copied"));
    assert_eq!(page.summaries[0].source, SessionSource::OhFx);
}

#[test]
fn an_oh_fx_session_folder_hides_the_fx_session_of_its_id_even_when_unreadable() {
    let home = Home::new();
    home.store("/work");
    let saved = Saved {
        id: "shared-id",
        workspace: "/work",
        title: Some("Original"),
        prompts: &["one"],
        modified_s: 100,
    };
    saved.write(&home.fx_sessions());
    saved.write_with(&home.own_sessions(), "{");
    let catalog = home
        .store("/work")
        .catalog_with_fx(&FxSessions::open(home.path()))
        .unwrap();
    assert!(
        catalog
            .listed_page(ListScope::AllWorkspaces, None, 50)
            .summaries
            .is_empty()
    );
    assert_eq!(catalog.skipped_invalid(), 1);
}

#[test]
fn fx_index_is_ignored_and_never_rewritten() {
    let home = Home::new();
    for (id, modified_s) in [("fx-kept", 100), ("fx-removed", 200)] {
        Saved {
            id,
            workspace: "/work",
            title: Some(id),
            prompts: &["one"],
            modified_s,
        }
        .write(&home.fx_sessions());
    }
    let orphan = Saved {
        id: "fx-orphan-result",
        workspace: "/work",
        title: None,
        prompts: &[],
        modified_s: 250,
    };
    let without_call = shell_turn_without_its_call();
    orphan.write_files(&home.fx_sessions(), &orphan.manifest(), &without_call);
    let fx_sessions = PrivateDir::open_existing(&home.fx_sessions())
        .unwrap()
        .unwrap();
    let names = session_directory_names(&fx_sessions).unwrap();
    let indexed = scan_catalog(
        &fx_sessions,
        &names,
        CatalogIndex::Maintained,
        Classification::Listing,
    );
    assert_eq!(indexed.summaries.len(), 3);
    let index = home.fx_sessions().join(".resume-catalog");
    let indexed = fs::read(&index).unwrap();
    fs::remove_dir_all(home.fx_sessions().join("fx-removed")).unwrap();
    Saved {
        id: "fx-added",
        workspace: "/work",
        title: Some("fx-added"),
        prompts: &["two"],
        modified_s: 300,
    }
    .write(&home.fx_sessions());
    let before = snapshot(&home.fx_profile());
    let page = home.listed("/work", ListScope::AllWorkspaces);
    assert_eq!(ids(&page), ["fx-added", "fx-kept"]);
    assert_eq!(page.summaries[1].title.as_deref(), Some("fx-kept"));
    assert_eq!(fs::read(&index).unwrap(), indexed);
    assert_eq!(snapshot(&home.fx_profile()), before);
}

#[test]
fn listing_fx_sessions_never_writes_into_fx_folder() {
    let home = Home::new();
    for (id, modified_s) in [("fx-a", 100), ("fx-b", 200)] {
        Saved {
            id,
            workspace: "/work",
            title: None,
            prompts: &["one"],
            modified_s,
        }
        .write(&home.fx_sessions());
    }
    let before = snapshot(&home.fx_profile());
    assert_eq!(
        home.listed("/work", ListScope::AllWorkspaces)
            .summaries
            .len(),
        2
    );
    assert_eq!(snapshot(&home.fx_profile()), before);
    assert!(!home.fx_sessions().join(".resume-catalog").exists());
    assert!(home.own_sessions().join(".resume-catalog").exists());
}

pub(super) fn snapshot(root: &Path) -> Vec<(PathBuf, u64, i64, i64, u32)> {
    let mut entries = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).unwrap();
        if metadata.is_dir() {
            pending.extend(
                fs::read_dir(&path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        }
        entries.push((
            path,
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.mode(),
        ));
    }
    entries.sort();
    entries
}

#[test]
fn a_missing_shared_or_linked_fx_folder_lists_nothing() {
    let saved = Saved {
        id: "fx-session",
        workspace: "/work",
        title: None,
        prompts: &["one"],
        modified_s: 100,
    };

    let home = Home::new();
    fs::remove_dir_all(home.fx_profile()).unwrap();
    assert!(
        home.listed("/work", ListScope::AllWorkspaces)
            .summaries
            .is_empty()
    );

    for shared in [Home::fx_profile, Home::fx_sessions] {
        let home = Home::new();
        saved.write(&home.fx_sessions());
        assert_eq!(
            home.listed("/work", ListScope::AllWorkspaces)
                .summaries
                .len(),
            1
        );
        fs::set_permissions(shared(&home), fs::Permissions::from_mode(0o750)).unwrap();
        assert!(
            home.listed("/work", ListScope::AllWorkspaces)
                .summaries
                .is_empty()
        );
    }

    let home = Home::new();
    let elsewhere = home.path().join("dotfiles/fx");
    fs::create_dir_all(elsewhere.join("sessions")).unwrap();
    for dir in [elsewhere.clone(), elsewhere.join("sessions")] {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    saved.write(&elsewhere.join("sessions"));
    fs::remove_dir_all(home.fx_profile()).unwrap();
    symlink(&elsewhere, home.fx_profile()).unwrap();
    assert!(
        home.listed("/work", ListScope::AllWorkspaces)
            .summaries
            .is_empty()
    );

    assert!(
        FxSessions::open(Path::new("relative-home"))
            .summaries()
            .is_empty()
    );
}

const REPLAY: &str = "\"fx-command-replay-00112233445566778899aabbccddeeff\"";

pub(super) fn frame(seq: u64, event: &str) -> String {
    format!("{{\"schema_version\":3,\"seq\":{seq},\"timestamp_ms\":2,\"event\":{event}}}\n")
}

pub(super) fn shell_events(replay_ref: &str, replay_bytes: &str) -> Vec<String> {
    let call = "{\"tool_call\":{\"call_id\":\"call-1\",\"tool_name\":\"shell\",\"arguments_json\":\"{\\\"command\\\":\\\"ls\\\"}\",\"argument_integrity\":\"valid\",\"provisional_id\":null,\"provider_result\":null,\"final_identity\":\"valid\",\"provenance\":\"fx_local\"}}";
    let result = format!(
        "{{\"tool_result\":{{\"call_id\":\"call-1\",\"tool_name\":\"shell\",\"status\":\"success\",\"artifact_ref\":\"result-shell-0011223344556677-8899aabbccddeeff.txt\",\"tool_image_handle\":null,\"output_bytes\":3,\"stored_bytes\":3,\"completeness\":\"complete\",\"preview\":\"a.txt\",\"provider_native\":false,\"created_at_ms\":2,\"permission_feedback\":[],\"committed_file_presentation\":null,\"command_replay_ref\":{replay_ref},\"command_replay_bytes\":{replay_bytes},\"command_process_presentation\":null,\"terminal_action_presentation\":null}}}}"
    );
    vec![
        "{\"user\":{\"text\":\"list files\",\"images\":[],\"work_id\":null}}".to_owned(),
        call.to_owned(),
        result,
        "{\"assistant\":{\"text\":\"done\",\"provider_replay\":null,\"standalone_response\":false}}".to_owned(),
        "{\"turn_completed\":{\"files\":[],\"turn_summary\":null}}".to_owned(),
    ]
}

pub(super) fn log_of(events: &[String]) -> String {
    events
        .iter()
        .zip(1_u64..)
        .map(|(event, seq)| frame(seq, event))
        .collect()
}

pub(super) fn shell_turn(replay_ref: &str, replay_bytes: &str) -> String {
    log_of(&shell_events(replay_ref, replay_bytes))
}

pub(super) fn shell_turn_without_its_call() -> String {
    let mut events = shell_events("null", "null");
    events.remove(1);
    log_of(&events)
}

fn shell_turn_cancelled_with_its_replay() -> String {
    let mut events = shell_events("null", "null");
    events.truncate(2);
    events.push(format!(
        "{{\"interrupted\":{{\"reason\":\"cancelled\",\"partial_text\":null,\"command_replay_ref\":{REPLAY},\"command_replay_bytes\":64,\"command_artifact_ref\":\"fx-command-artifact-1.log\",\"files\":[],\"turn_summary\":null}}}}"
    ));
    log_of(&events)
}

pub(super) fn shell_turn_with_an_unknown_event() -> String {
    let mut events = shell_events("null", "null");
    events.insert(3, "{\"unknown\":{}}".to_owned());
    log_of(&events)
}

#[test]
fn fx_sessions_oh_fx_could_not_resume_are_hidden_and_not_counted() {
    let home = Home::new();
    home.store("/work");
    let saved = |id| Saved {
        id,
        workspace: "/work",
        title: None,
        prompts: &[],
        modified_s: 100,
    };
    let unfinished = log_of(&shell_events("null", "null")[..3]);
    for (id, log) in [
        ("fx-plain", shell_turn("null", "null")),
        ("fx-replayed", shell_turn(REPLAY, "64")),
        (
            "fx-cancelled-command",
            shell_turn_cancelled_with_its_replay(),
        ),
        ("fx-unknown-event", shell_turn_with_an_unknown_event()),
        ("fx-recovery-ahead", shell_turn("null", "null")),
        ("fx-open-turn-recovery", unfinished.clone()),
        ("fx-open-turn", unfinished),
    ] {
        let session = saved(id);
        session.write_files(&home.fx_sessions(), &session.manifest(), &log);
    }
    for (id, preference) in [
        ("fx-unknown-key", "\"unknown\":true,"),
        ("fx-ultrafast", "\"ultrafast_mode\":true,"),
    ] {
        let session = saved(id);
        session.write_files(
            &home.fx_sessions(),
            &session.manifest().replace(
                "\"fast_mode\":false,",
                &format!("\"fast_mode\":false,{preference}"),
            ),
            &shell_turn("null", "null"),
        );
    }
    let without_call = shell_turn_without_its_call();
    for (id, sessions) in [
        ("fx-orphan-result", home.fx_sessions()),
        ("own-orphan-result", home.own_sessions()),
    ] {
        let orphan = saved(id);
        orphan.write_files(&sessions, &orphan.manifest(), &without_call);
    }
    for (id, recovery) in [
        (
            "fx-recovery-ahead",
            "{\"conversation_seq\":9,\"checkpoint\":{}}",
        ),
        (
            "fx-open-turn-recovery",
            "{\"conversation_seq\":0,\"checkpoint\":{\"version\":2}}",
        ),
    ] {
        let path = home.fx_sessions().join(id).join("recovery.json");
        fs::write(&path, recovery).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    let catalog = home
        .store("/work")
        .catalog_with_fx(&FxSessions::open(home.path()))
        .unwrap();
    let page = catalog.listed_page(ListScope::AllWorkspaces, None, 50);
    assert_eq!(
        ids(&page),
        [
            "own-orphan-result",
            "fx-ultrafast",
            "fx-replayed",
            "fx-plain",
            "fx-cancelled-command",
            "fx-open-turn"
        ]
    );
    let turns: Vec<usize> = page
        .summaries
        .iter()
        .map(|summary| summary.history_len)
        .collect();
    assert_eq!(turns, [1, 1, 1, 1, 1, 0]);
    assert_eq!(catalog.skipped_invalid(), 0);
}

#[test]
fn fx_sessions_holding_file_presentations_are_listed() {
    let home = Home::new();
    home.store("/work");
    let edited = Saved {
        id: "fx-edited",
        workspace: "/work",
        title: None,
        prompts: &[],
        modified_s: 100,
    };
    let presentation = "{\"path\":\"src/lib.rs\",\"kind\":\"edited\",\"lines\":[{\"kind\":\"deletion\",\"old_line\":1,\"new_line\":null,\"text\":\"old\"},{\"kind\":\"addition\",\"old_line\":null,\"new_line\":1,\"text\":\"new\"}],\"additions\":1,\"deletions\":1,\"truncated\":false,\"previous_content\":\"old\\n\",\"after_content\":\"new\\n\",\"lifecycle_id\":{\"turn_id\":1,\"call_id\":\"call-1\"},\"content_handle\":null}";
    let log = shell_turn("null", "null")
        .replace("\"tool_name\":\"shell\"", "\"tool_name\":\"edit_file\"")
        .replace(
            "\"committed_file_presentation\":null",
            &format!("\"committed_file_presentation\":{presentation}"),
        );
    edited.write_files(&home.fx_sessions(), &edited.manifest(), &log);

    let page = home.listed("/work", ListScope::AllWorkspaces);
    assert_eq!(ids(&page), ["fx-edited"]);
    assert_eq!(page.summaries[0].history_len, 1);
}

#[test]
fn fx_subagent_children_are_not_listed() {
    let home = Home::new();
    let child = Saved {
        id: "fx-child",
        workspace: "/work",
        title: None,
        prompts: &["one"],
        modified_s: 100,
    };
    let manifest = child
        .manifest()
        .replace("\"subagent_child\":false", "\"subagent_child\":true");
    child.write_with(&home.fx_sessions(), &manifest);
    assert!(
        home.listed("/work", ListScope::AllWorkspaces)
            .summaries
            .is_empty()
    );
}

#[test]
fn sessions_fx_saved_before_its_conversation_layout_are_listed_and_marked() {
    let home = Home::new();
    home.store("/work");
    LegacyLog::started("fx-legacy", "/work")
        .turn(&reply("old prompt", "old answer"))
        .titled("Old work")
        .write(&home.fx_sessions());
    LegacyLog::started("fx-legacy-elsewhere", "/elsewhere")
        .turn(&reply("one", "two"))
        .turn(&reply("three", "four"))
        .write(&home.fx_sessions());
    let fenced = LegacyLog::started("fx-legacy-fenced", "/work")
        .turn(&reply("one", "two"))
        .write(&home.fx_sessions());
    fs::write(fenced.join("authority.pending.json"), "{}").unwrap();
    let child = LegacyLog::started("fx-legacy-child", "/work")
        .turn(&reply("one", "two"))
        .write(&home.fx_sessions());
    fs::create_dir_all(child.join("subagent")).unwrap();
    fs::write(child.join("subagent/owner.json"), "{}").unwrap();
    LegacyLog::started("fx-legacy-later", "/work")
        .turn(&reply("look", "seen").replace(
            "\"images\":[]",
            "\"images\":[{\"id\":1,\"path\":\"/tmp/a.png\",\"media_type\":\"image/png\",\"snapshot_path\":null,\"snapshot_sha256\":null}]",
        ))
        .write(&home.fx_sessions());
    let before = snapshot(&home.fx_profile());

    let here = home.listed("/work", ListScope::CurrentWorkspace);
    assert_eq!(ids(&here), ["fx-legacy"]);
    let legacy = &here.summaries[0];
    assert_eq!(legacy.source, SessionSource::Fx);
    assert_eq!(legacy.title.as_deref(), Some("Old work"));
    assert_eq!(legacy.history_len, 1);
    assert_eq!(legacy.updated_at_ms, 40);
    let everywhere = home.listed("/work", ListScope::AllWorkspaces);
    assert_eq!(ids(&everywhere), ["fx-legacy-elsewhere", "fx-legacy"]);
    assert_eq!(snapshot(&home.fx_profile()), before);
}

mod migration;
mod recovery;
