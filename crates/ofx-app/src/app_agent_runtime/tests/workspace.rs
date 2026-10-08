use std::ffi::OsString;
use std::path::{Path, PathBuf};

use ofx_contract::{DirectoryAccess, Notice, WorkspaceMenu, WorkspaceMenuEntry};
use ofx_tui::{FileMentionSource, IndexRevision};

use crate::file_mention_runtime::WorkspaceFileMentions;

use super::*;

const USAGE: &str = "usage: /workspace [add PATH|remove PATH|clear]";
const BUSY: &str = "Workspace changes are unavailable until the active and queued work finishes.";

fn workspace(home: &Path) -> PathBuf {
    fs::canonicalize(home.join("workspace")).unwrap()
}

fn directory(home: &Path, name: &str) -> PathBuf {
    let path = home.join(name);
    fs::create_dir_all(&path).unwrap();
    fs::canonicalize(path).unwrap()
}

fn header(home: &Path) -> String {
    format!(
        "primary={}\nsaved_suppressed=false limit=16\n",
        workspace(home).display()
    )
}

fn saved_entry(path: &Path, available: bool) -> String {
    format!(
        " - {} saved=true command_line=false available={available} active={available}",
        path.display()
    )
}

fn saved_settings(home: &Path) -> Value {
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(home.join("config/settings.json")).unwrap())
            .unwrap();
    let key = workspace(home).display().to_string();
    settings["workspaces"][key]["additional_directories"].clone()
}

async fn launched(server: &FakeServer, directories: &[&Path]) -> Harness {
    let (home, setup) = connected(server, directories).await;
    Harness::with_setup(home, setup)
}

async fn connected(server: &FakeServer, directories: &[&Path]) -> (tempfile::TempDir, AgentSetup) {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("settings.json"),
        local_settings(server).to_string(),
    )
    .unwrap();
    let workspace = directory(home.path(), "workspace");
    let paths = ProfilePaths {
        config,
        data: home.path().join("data"),
        state: home.path().join("state"),
        cache: home.path().join("cache"),
    };
    let settings = Settings::load(&paths, &workspace).unwrap();
    let mut profile =
        Profile::new(workspace, Some(home.path().into()), Some(paths), settings).unwrap();
    let directories: Vec<OsString> = directories
        .iter()
        .map(|path| directory(home.path(), &path.to_string_lossy()).into_os_string())
        .collect();
    profile.apply_launch(&directories, false).unwrap();
    let executions = ManagedExecutions::new(SessionSupervisor::new("/nonexistent"));
    let setup = profile
        .connect_interactive(
            Launch {
                model: None,
                permission_mode: PermissionMode::Auto,
                system_prompt: None,
                reasoning_effort: None,
                fast_mode: None,
                context_limits: &[],
                command_timeout: None,
                executions: &executions,
                endpoints: SubscriptionEndpoints::default(),
                web_fetch_progress: None,
                mode: None,
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    (home, setup)
}

fn settled(mentions: &mut WorkspaceFileMentions) -> IndexRevision {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    mentions.poll();
    while mentions.is_loading() {
        assert!(std::time::Instant::now() < deadline);
        mentions.poll();
        std::thread::yield_now();
    }
    mentions.poll();
    mentions.revision()
}

impl Harness {
    async fn notice(&mut self, command: &str) -> Notice {
        self.command(command);
        let shown = self
            .until(|event| matches!(event, UiEvent::Notice { .. }))
            .await;
        let Some(UiEvent::Notice { notice }) = shown.last() else {
            unreachable!()
        };
        notice.clone()
    }

    async fn snapshot(&mut self, command: &str) -> String {
        let notice = self.notice(command).await;
        assert_eq!(
            (notice.tone, notice.topic.as_str()),
            (NoticeTone::Neutral, "workspace"),
            "{command}: {}",
            notice.body
        );
        notice.body
    }
}

#[tokio::test]
async fn workspace_lists_adds_removes_and_clears_saved_directories_with_upstreams_snapshot() {
    let server = FakeServer::start([]);
    let mut harness = Harness::start(&server).await;
    let home = harness.home.path().to_path_buf();
    let shared = directory(&home, "shared");
    let header = header(&home);
    assert_eq!(
        harness.snapshot("/workspace list").await,
        format!("{header}additional directories: (none)")
    );
    assert_eq!(
        harness.snapshot("/workspace add ../shared").await,
        format!(
            "{header}add ../shared saved_changed=true runtime_changed=true launch_flag_can_restore=false\nadditional directories:\n{}",
            saved_entry(&shared, true)
        )
    );
    assert_eq!(saved_settings(&home), json!([shared]));
    assert_eq!(
        harness
            .snapshot(&format!("/workspace remove {}", shared.display()))
            .await,
        format!(
            "{header}remove {} saved_changed=true runtime_changed=true launch_flag_can_restore=false\nadditional directories: (none)",
            shared.display()
        )
    );
    harness.snapshot("/workspace add ../shared").await;
    assert_eq!(
        harness.snapshot("/workspace clear").await,
        format!(
            "{header}clear saved_changed=true runtime_changed=true launch_flag_can_restore=false\nadditional directories: (none)"
        )
    );
    assert_eq!(saved_settings(&home), Value::Null);
}

#[tokio::test]
async fn directories_added_and_removed_in_the_shell_reach_the_next_request() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["one"])),
        Reply::sse(&chat_text_events(&["two"])),
    ]);
    let mut harness = Harness::start(&server).await;
    let shared = directory(harness.home.path(), "shared");
    let named = format!("- {}", shared.display());
    harness.snapshot("/workspace add ../shared").await;
    harness.submit("first");
    harness.until(finished(TurnOutcome::Completed)).await;
    harness
        .snapshot(&format!("/workspace remove {}", shared.display()))
        .await;
    harness.submit("second");
    harness.until(finished(TurnOutcome::Completed)).await;
    let requests = server.requests();
    assert!(requests[0].json().to_string().contains(&named));
    assert!(!requests[1].json().to_string().contains(&named));
}

#[tokio::test]
async fn the_mention_index_follows_directories_added_and_removed_in_the_shell() {
    let server = FakeServer::start([]);
    let (home, setup) = connected(&server, &[]).await;
    let shared = directory(home.path(), "shared");
    fs::write(shared.join("manual.md"), "").unwrap();
    let manual = format!("{}/manual.md", shared.display());
    let mut mentions = WorkspaceFileMentions::start(
        &workspace(home.path()),
        setup.workspace().live_roots(),
        None,
    );
    let mut harness = Harness::with_setup(home, setup);
    let launch = settled(&mut mentions);
    assert_eq!(mentions.search(launch, "manual", 32), Some(Vec::new()));
    harness.snapshot("/workspace add ../shared").await;
    let added = settled(&mut mentions);
    assert_ne!(added.scope_epoch, launch.scope_epoch);
    let rows = mentions.search(added, "manual", 32).unwrap();
    assert_eq!(
        rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(),
        [manual.as_str()]
    );
    harness
        .snapshot(&format!("/workspace remove {}", shared.display()))
        .await;
    let removed = settled(&mut mentions);
    assert_ne!(removed.scope_epoch, added.scope_epoch);
    assert_eq!(mentions.search(removed, "manual", 32), Some(Vec::new()));
}

#[tokio::test]
async fn malformed_workspace_commands_print_upstreams_usage() {
    let server = FakeServer::start([]);
    let mut harness = Harness::start(&server).await;
    for command in [
        "/workspace bogus",
        "/workspace add",
        "/workspace remove \t ",
        "/workspace addx",
        "/workspace list extra",
    ] {
        let notice = harness.notice(command).await;
        assert_eq!(
            (notice.tone, notice.topic.as_str(), notice.body.as_str()),
            (NoticeTone::Error, "", USAGE),
            "{command}"
        );
    }
}

#[tokio::test]
async fn failed_workspace_changes_name_the_phase_they_stopped_in() {
    let server = FakeServer::start([]);
    let mut harness = Harness::start(&server).await;
    let home = harness.home.path().to_path_buf();
    directory(&home, "shared");
    for (command, body) in [
        (
            "/workspace add ../missing",
            "Workspace update rejected: directory does not exist",
        ),
        (
            "/workspace add .",
            "Workspace update rejected: the primary workspace cannot be added or removed",
        ),
        (
            "/workspace remove ../shared",
            "Workspace update rejected: path is invalid",
        ),
    ] {
        let notice = harness.notice(command).await;
        assert_eq!(
            (notice.tone, notice.topic.as_str(), notice.body.as_str()),
            (NoticeTone::Error, "workspace", body),
            "{command}"
        );
    }
    fs::write(home.join("config/settings.json"), "{").unwrap();
    let notice = harness.notice("/workspace add ../shared").await;
    assert_eq!(
        (notice.tone, notice.body.as_str()),
        (
            NoticeTone::Error,
            "Workspace settings were not changed: workspace update failed"
        )
    );
}

#[tokio::test]
async fn workspace_changes_wait_for_the_running_turn_and_listing_still_answers() {
    let held = Reply::held_sse(&chat_text_events(&["partial\n"])[..2]);
    let server = FakeServer::start([held]);
    let mut harness = Harness::start(&server).await;
    let home = harness.home.path().to_path_buf();
    directory(&home, "shared");
    harness.submit("slow");
    harness
        .until(|event| matches!(event, UiEvent::AssistantText { .. }))
        .await;
    let notice = harness.notice("/workspace add ../shared").await;
    assert_eq!(
        (notice.tone, notice.topic.as_str(), notice.body.as_str()),
        (NoticeTone::Neutral, "workspace", BUSY)
    );
    assert_eq!(
        harness.snapshot("/workspace list").await,
        format!("{}additional directories: (none)", header(&home))
    );
    let turn_id = harness.running_turn();
    harness.send(UiCommand::Cancel { turn_id });
    harness.until(finished(TurnOutcome::Interrupted)).await;
    assert_eq!(saved_settings(&home), Value::Null);
}

fn saved_listing(home: &Path, shared: &Path, available: bool) -> String {
    format!(
        "{}additional directories:\n{}",
        header(home),
        saved_entry(shared, available)
    )
}

async fn refused_while_queued(harness: &mut Harness, shared: &Path) {
    let home = harness.home.path().to_path_buf();
    fs::remove_dir(shared).unwrap();
    directory(&home, "other");
    for command in [
        "/workspace add ../other",
        &format!("/workspace remove {}", shared.display()),
        "/workspace clear",
    ] {
        let notice = harness.notice(command).await;
        assert_eq!(
            (notice.tone, notice.topic.as_str(), notice.body.as_str()),
            (NoticeTone::Neutral, "workspace", BUSY),
            "{command}"
        );
    }
    assert_eq!(
        harness.snapshot("/workspace list").await,
        saved_listing(&home, shared, true)
    );
    assert_eq!(saved_settings(&home), json!([shared]));
}

#[tokio::test]
async fn workspace_changes_wait_for_a_prompt_held_for_sign_in() {
    let codex = FakeServer::start([]);
    let catalog = codex_catalog(false, 2);
    let mut harness = signed_out(&codex, &catalog).await;
    let home = harness.home.path().to_path_buf();
    let shared = directory(&home, "shared");
    harness.snapshot("/workspace add ../shared").await;
    held(&mut harness, "use the shared directory").await;
    assert!(harness.worker.has_waiting_prompts());
    refused_while_queued(&mut harness, &shared).await;
    harness.send(UiCommand::DropHeldPrompt);
    assert_eq!(
        harness.snapshot("/workspace list").await,
        saved_listing(&home, &shared, false)
    );
    assert!(codex.requests().is_empty());
}

#[tokio::test]
async fn workspace_changes_wait_for_a_prompt_queued_behind_a_skill_installation() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["done"]))]);
    let mut harness = Harness::start(&server).await;
    let home = harness.home.path().to_path_buf();
    write_skill(&harness.home, "install-pack", "new-skill");
    let source = fs::canonicalize(home.join("workspace/install-pack")).unwrap();
    let shared = directory(&home, "shared");
    harness.snapshot("/workspace add ../shared").await;
    let (release, worker) = held_install_lock(&harness.home);
    harness.command(&format!("/skills install {}", source.display()));
    harness.submit("use the shared directory");
    refused_while_queued(&mut harness, &shared).await;
    release.send(()).unwrap();
    within(harness.until(finished(TurnOutcome::Completed))).await;
    worker.join().unwrap();
    assert_eq!(
        harness.snapshot("/workspace list").await,
        saved_listing(&home, &shared, false)
    );
}

#[tokio::test]
async fn listing_refreshes_whether_saved_directories_are_available() {
    let server = FakeServer::start([]);
    let mut harness = Harness::start(&server).await;
    let home = harness.home.path().to_path_buf();
    let shared = directory(&home, "shared");
    harness.snapshot("/workspace add ../shared").await;
    fs::remove_dir(&shared).unwrap();
    assert_eq!(
        harness.snapshot("/workspace list").await,
        saved_listing(&home, &shared, false)
    );
    directory(&home, "shared");
    assert_eq!(
        harness.snapshot("/workspace list").await,
        saved_listing(&home, &shared, true)
    );
}

#[tokio::test]
async fn bare_workspace_opens_the_menu_with_every_directory() {
    let server = FakeServer::start([]);
    let mut harness = launched(&server, &[Path::new("launch")]).await;
    let home = harness.home.path().to_path_buf();
    let shared = directory(&home, "shared");
    let launch = directory(&home, "launch");
    harness.snapshot("/workspace add ../shared").await;
    harness.command("/workspace");
    let shown = harness
        .until(|event| matches!(event, UiEvent::WorkspaceMenuOpened { .. }))
        .await;
    let entry = |path: &Path, saved, command_line| WorkspaceMenuEntry {
        path: path.to_path_buf(),
        saved,
        command_line,
        access: DirectoryAccess::Active,
    };
    assert_eq!(
        shown.last(),
        Some(&UiEvent::WorkspaceMenuOpened {
            menu: WorkspaceMenu {
                primary: workspace(&home),
                saved_suppressed: false,
                limit: 16,
                entries: vec![entry(&shared, true, false), entry(&launch, false, true)],
            }
        })
    );
}

#[tokio::test]
async fn removing_a_launch_directory_warns_that_the_flag_can_restore_it() {
    let server = FakeServer::start([]);
    let mut harness = launched(&server, &[Path::new("launch")]).await;
    let home = harness.home.path().to_path_buf();
    let launch = directory(&home, "launch");
    assert_eq!(
        harness
            .snapshot(&format!("/workspace remove {}", launch.display()))
            .await,
        format!(
            "{}remove {} saved_changed=false runtime_changed=true launch_flag_can_restore=true\nwarning: repeating --add-dir can restore removed access on the next launch\nadditional directories: (none)",
            header(&home),
            launch.display()
        )
    );
}
