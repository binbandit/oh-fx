use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use super::*;
use crate::session_store::{ListScope, SessionStore};
use crate::session_summary_codec::ResumablePage;

struct Saved<'a> {
    id: &'a str,
    workspace: &'a str,
    title: Option<&'a str>,
    prompts: &'a [&'a str],
    modified_s: u64,
}

impl Saved<'_> {
    fn manifest(&self) -> String {
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

    fn events(&self) -> String {
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

    fn write(&self, sessions: &Path) {
        self.write_with(sessions, &self.manifest());
    }

    fn write_with(&self, sessions: &Path, manifest: &str) {
        let dir = sessions.join(self.id);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        for (name, bytes) in [
            ("session.json", manifest.to_owned()),
            ("events.jsonl", self.events()),
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

struct Home {
    root: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        let home = Self {
            root: tempfile::tempdir().unwrap(),
        };
        for dir in [home.fx_profile(), home.fx_sessions()] {
            fs::create_dir_all(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        home
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn fx_profile(&self) -> PathBuf {
        self.path().join(".fx")
    }

    fn fx_sessions(&self) -> PathBuf {
        self.fx_profile().join("sessions")
    }

    fn own_sessions(&self) -> PathBuf {
        self.path().join("data/oh-fx/sessions")
    }

    fn store(&self, workspace: &str) -> SessionStore {
        SessionStore::open(&self.path().join("data/oh-fx"), workspace).unwrap()
    }

    fn listed(&self, workspace: &str, scope: ListScope) -> ResumablePage {
        self.store(workspace)
            .catalog_with_fx(&FxSessions::open(self.path()))
            .unwrap()
            .listed_page(scope, None, 50)
    }
}

fn ids(page: &ResumablePage) -> Vec<&str> {
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
fn a_stale_fx_index_is_read_but_never_rewritten() {
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
    let fx_sessions = PrivateDir::open_existing(&home.fx_sessions())
        .unwrap()
        .unwrap();
    let names = session_directory_names(&fx_sessions).unwrap();
    assert_eq!(scan_catalog(&fx_sessions, &names, true).summaries.len(), 2);
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

fn snapshot(root: &Path) -> Vec<(PathBuf, u64, i64, i64, u32)> {
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

#[test]
fn fx_sessions_oh_fx_cannot_read_are_left_out_and_not_counted() {
    let home = Home::new();
    let readable = Saved {
        id: "fx-readable",
        workspace: "/work",
        title: None,
        prompts: &["one"],
        modified_s: 100,
    };
    readable.write(&home.fx_sessions());
    let ultrafast = Saved {
        id: "fx-ultrafast",
        ..readable
    };
    let manifest = ultrafast.manifest().replace(
        "\"fast_mode\":false,",
        "\"fast_mode\":true,\"ultrafast_mode\":true,",
    );
    ultrafast.write_with(&home.fx_sessions(), &manifest);
    let catalog = home
        .store("/work")
        .catalog_with_fx(&FxSessions::open(home.path()))
        .unwrap();
    assert_eq!(
        ids(&catalog.listed_page(ListScope::AllWorkspaces, None, 50)),
        ["fx-readable"]
    );
    assert_eq!(catalog.skipped_invalid(), 0);
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
