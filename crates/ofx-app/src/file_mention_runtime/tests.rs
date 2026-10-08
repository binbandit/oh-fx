use std::fs;
use std::time::{Duration, Instant};

use super::*;

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::create_dir_all(workspace.join("docs")).unwrap();
        for file in ["src/main.rs", "src/mailbox.rs", "docs/notes.md"] {
            fs::write(workspace.join(file), "").unwrap();
        }
        Self { root }
    }

    fn workspace(&self) -> PathBuf {
        fs::canonicalize(self.root.path())
            .unwrap()
            .join("workspace")
    }

    fn ready(&self) -> WorkspaceFileMentions {
        self.ready_with(&[])
    }

    fn ready_with(&self, additional_roots: &[PathBuf]) -> WorkspaceFileMentions {
        self.ready_following(LiveAdditionalRoots::from(additional_roots.to_vec()))
    }

    fn ready_following(&self, roots: LiveAdditionalRoots) -> WorkspaceFileMentions {
        let cache = self.root.path().join("cache");
        let mut mentions = WorkspaceFileMentions::start(&self.workspace(), roots, Some(&cache));
        settle(&mut mentions);
        mentions
    }

    fn shared(&self, file: &str) -> PathBuf {
        let shared = fs::canonicalize(self.root.path()).unwrap().join("shared");
        fs::create_dir_all(&shared).unwrap();
        fs::write(shared.join(file), "").unwrap();
        shared
    }
}

fn settle(mentions: &mut WorkspaceFileMentions) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while mentions.is_loading() {
        assert!(Instant::now() < deadline);
        mentions.poll();
        std::thread::yield_now();
    }
    mentions.poll();
}

fn paths(rows: &[FileMatch]) -> Vec<&str> {
    rows.iter().map(|row| row.path.as_str()).collect()
}

#[test]
fn the_index_answers_fuzzy_queries_only_at_its_current_revision() {
    let fixture = Fixture::new();
    let mentions = fixture.ready();
    let revision = mentions.revision();
    assert_eq!(revision.state, IndexState::Ready);
    let rows = mentions.search(revision, "ma", 32).unwrap();
    assert_eq!(paths(&rows)[..2], ["src/main.rs", "src/mailbox.rs"]);
    assert_eq!(rows[0].kind, MentionKind::File);
    assert_eq!(rows[0].spans.first(), Some(&(4..6)));
    assert_eq!(rows[0].spans.len(), 1);
    let stale = IndexRevision {
        generation: revision.generation + 1,
        ..revision
    };
    assert_eq!(mentions.search(stale, "ma", 32), None);
    assert_eq!(mentions.search(revision, "src/", 32), None);
}

#[test]
fn additional_directories_are_indexed_after_the_workspace_with_absolute_paths() {
    let fixture = Fixture::new();
    let shared = fixture.shared("manual.md");
    let mentions = fixture.ready_with(std::slice::from_ref(&shared));
    let revision = mentions.revision();
    let rows = mentions.search(revision, "manual", 32).unwrap();
    let absolute = format!("{}/manual.md", shared.display());
    assert_eq!(paths(&rows), [absolute.as_str()]);
    assert!(
        mentions
            .search(revision, "ma", 32)
            .unwrap()
            .iter()
            .any(|row| row.path == "src/main.rs")
    );
}

#[test]
fn the_index_follows_the_installed_scope_and_serves_no_rows_from_the_old_one() {
    let fixture = Fixture::new();
    let shared = fixture.shared("manual.md");
    let manual = format!("{}/manual.md", shared.display());
    let roots = LiveAdditionalRoots::default();
    let mut mentions = fixture.ready_following(roots.clone());
    let before = mentions.revision();
    assert_eq!(mentions.search(before, "manual", 32), Some(Vec::new()));
    assert!(!mentions.poll());

    roots.set(vec![shared.clone()]);
    assert!(mentions.poll());
    let installing = mentions.revision();
    assert_ne!(installing, before);
    assert_eq!(installing.scope_epoch, before.scope_epoch + 1);
    assert_eq!(mentions.search(before, "ma", 32), None);
    assert!(mentions.is_current("manual", &manual, MentionKind::File));
    settle(&mut mentions);
    let added = mentions.revision();
    assert_eq!(added.scope_epoch, installing.scope_epoch);
    assert_eq!(
        paths(&mentions.search(added, "manual", 32).unwrap()),
        [manual.as_str()]
    );

    roots.set(vec![shared.clone()]);
    mentions.refresh();
    settle(&mut mentions);
    let reinstalled = mentions.revision();
    assert_eq!(reinstalled.scope_epoch, added.scope_epoch + 1);

    roots.set(Vec::new());
    mentions.refresh();
    assert!(!mentions.is_current("manual", &manual, MentionKind::File));
    settle(&mut mentions);
    let removed = mentions.revision();
    assert_eq!(removed.scope_epoch, reinstalled.scope_epoch + 1);
    assert_eq!(mentions.search(removed, "manual", 32), Some(Vec::new()));
    assert!(
        mentions
            .search(removed, "ma", 32)
            .unwrap()
            .iter()
            .any(|row| row.path == "src/main.rs")
    );
}

#[test]
fn a_row_presented_before_a_scope_change_is_rejected_until_the_index_follows() {
    let fixture = Fixture::new();
    let shared = fixture.shared("manual.md");
    let manual = format!("{}/manual.md", shared.display());
    let roots = LiveAdditionalRoots::from(vec![shared.clone()]);
    let mut mentions = fixture.ready_following(roots.clone());
    assert!(mentions.is_current("manual", &manual, MentionKind::File));
    assert!(mentions.is_current("src/m", "src/main.rs", MentionKind::File));
    roots.set(Vec::new());
    assert!(!mentions.is_current("manual", &manual, MentionKind::File));
    assert!(!mentions.is_current("ma", "src/main.rs", MentionKind::File));
    assert!(!mentions.is_current("src/m", "src/main.rs", MentionKind::File));
    settle(&mut mentions);
    assert!(!mentions.is_current("manual", &manual, MentionKind::File));
    assert!(mentions.is_current("ma", "src/main.rs", MentionKind::File));
    assert!(mentions.is_current("src/m", "src/main.rs", MentionKind::File));
}

#[test]
fn explicit_paths_bypass_the_index_and_list_the_directory() {
    let fixture = Fixture::new();
    let mentions = fixture.ready();
    assert!(mentions.depends_on_index("ma"));
    assert!(!mentions.depends_on_index("src/"));
    assert!(!mentions.depends_on_index("~"));
    let lister = mentions.directory_lister();
    let rows = lister("src/m", 32, &AtomicBool::new(false)).unwrap();
    assert_eq!(paths(&rows), ["src/main.rs", "src/mailbox.rs"]);
    assert_eq!(lister("missing/", 32, &AtomicBool::new(false)), None);
    assert_eq!(lister("src/", 32, &AtomicBool::new(true)), None);
}

#[test]
fn selections_are_checked_against_the_file_system_before_they_are_inserted() {
    let fixture = Fixture::new();
    let mentions = fixture.ready();
    assert!(mentions.is_current("ma", "src/main.rs", MentionKind::File));
    assert!(mentions.is_current("sr", "src", MentionKind::Directory));
    assert!(!mentions.is_current("sr", "src", MentionKind::File));
    assert!(mentions.is_current("src/", "src/main.rs", MentionKind::File));
    fs::remove_file(fixture.workspace().join("src/main.rs")).unwrap();
    assert!(!mentions.is_current("ma", "src/main.rs", MentionKind::File));
    assert!(!mentions.is_current("src/", "src/main.rs", MentionKind::File));
}
