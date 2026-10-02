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
        self.root.path().join("workspace")
    }

    fn ready(&self) -> WorkspaceFileMentions {
        let cache = self.root.path().join("cache");
        let mut mentions = WorkspaceFileMentions::start(&self.workspace(), Some(&cache));
        let deadline = Instant::now() + Duration::from_secs(10);
        while mentions.is_loading() {
            assert!(Instant::now() < deadline);
            mentions.poll();
            std::thread::yield_now();
        }
        mentions.poll();
        mentions
    }
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
