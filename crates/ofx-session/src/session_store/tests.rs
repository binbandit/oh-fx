use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use ofx_config::ProviderId;
use ofx_contract::ReasoningEffort;

use super::*;
use crate::session_codec::SavedProvider;
use crate::session_layout::is_valid_session_id;

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

    fn store(&self, workspace: &str) -> SessionStore {
        SessionStore::open(&self.data(), workspace).unwrap()
    }

    fn reader(&self, workspace: &str) -> SessionStore {
        SessionStore::open_read_only(&self.data(), workspace).unwrap()
    }
}

fn preferences() -> SessionPreferences {
    SessionPreferences {
        provider: SavedProvider::new(ProviderId::Gateway, None).unwrap(),
        model: "openai/gpt-5".to_owned(),
        effort: ReasoningEffort::Auto,
        fast_mode: false,
    }
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn opening_a_store_creates_a_private_layout_and_reading_creates_nothing() {
    let fixture = Fixture::new();
    let reader = fixture.reader("/workspace");
    assert_eq!(
        reader.load("anything").err(),
        Some(SessionError::SessionNotFound)
    );
    assert_eq!(
        reader.resume("anything").err(),
        Some(SessionError::SessionNotFound)
    );
    assert_eq!(
        reader.start(preferences()).err(),
        Some(SessionError::SessionStoreUnavailable)
    );
    assert!(!fixture.root.path().join("data").exists());

    let store = fixture.store("/workspace//");
    assert_eq!(mode(&fixture.data()), 0o700);
    assert_eq!(mode(&fixture.data().join("sessions")), 0o700);
    let session = store.start(preferences()).unwrap();
    assert_eq!(session.metadata().workspace_root, "/workspace");
    assert_eq!(session.metadata().origin_workspace_root, "/workspace");
    assert_eq!(session.metadata().conversation_language, "und");
    assert_eq!(
        session.metadata().created_at_ms,
        session.metadata().updated_at_ms
    );
    assert!(is_valid_session_id(session.id()));
    assert_eq!(session.id().len(), 12);
    let names: Vec<_> = fs::read_dir(fixture.data().join("sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, [session.id()]);

    for invalid in ["", "relative", "./x"] {
        assert_eq!(
            SessionStore::open(&fixture.data(), invalid).err(),
            Some(SessionError::InvalidWorkspaceRoot)
        );
    }
}

#[test]
fn symlinked_profile_directories_are_refused() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.path().join("elsewhere")).unwrap();
    fs::create_dir_all(fixture.data()).unwrap();
    symlink(
        fixture.root.path().join("elsewhere"),
        fixture.data().join("sessions"),
    )
    .unwrap();
    assert_eq!(
        SessionStore::open(&fixture.data(), "/workspace").err(),
        Some(SessionError::SessionPathUnsafe)
    );
    assert_eq!(
        SessionStore::open_read_only(&fixture.data(), "/workspace").err(),
        Some(SessionError::SessionPathUnsafe)
    );
}

#[test]
fn explicit_resume_rebinds_the_workspace_and_keeps_the_origin() {
    let fixture = Fixture::new();
    let id = fixture
        .store("/workspace-a")
        .start(preferences())
        .unwrap()
        .id()
        .to_owned();
    let target = fixture.store("/workspace-b");
    let resumed = target.resume(&id).unwrap();
    assert_eq!(resumed.metadata().workspace_root, "/workspace-b");
    assert_eq!(resumed.metadata().origin_workspace_root, "/workspace-a");
    drop(resumed);
    let reopened = target.load(&id).unwrap();
    assert_eq!(reopened.metadata.workspace_root, "/workspace-b");
    assert_eq!(reopened.metadata.origin_workspace_root, "/workspace-a");
}
