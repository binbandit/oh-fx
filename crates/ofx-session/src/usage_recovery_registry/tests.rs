use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::*;

struct Profile {
    root: tempfile::TempDir,
}

impl Profile {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> PathBuf {
        self.root.path().join("oh-fx")
    }

    fn data(&self) -> PrivateDir {
        PrivateDir::open_or_create(&self.path()).unwrap()
    }

    fn registry(&self) -> RecoveryRegistry {
        RecoveryRegistry::new(self.data())
    }

    fn markers(&self) -> PathBuf {
        self.path().join(USAGE_RECOVERY_DIR)
    }

    fn listed(&self) -> Result<Vec<MarkedSession>, SessionError> {
        marked_sessions(&self.data())
    }
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn markers_are_idempotent_replace_on_request_and_clear_durably() {
    let profile = Profile::new();
    let registry = profile.registry();
    assert_eq!(profile.listed(), Ok(Vec::new()));
    assert_eq!(registry.clear("recovery-marker"), Ok(()));
    registry.mark("recovery-marker", 1, true).unwrap();
    registry.mark("recovery-marker", 1, true).unwrap();
    let marker = profile.markers().join("recovery-marker");
    assert_eq!(fs::read_to_string(&marker).unwrap(), "v1 1\n");
    assert_eq!(mode(&profile.markers()), 0o700);
    assert_eq!(mode(&marker), 0o600);
    let listed = profile.listed().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "recovery-marker");
    assert_eq!(listed[0].protected_updated_at_ms, 1);

    registry.mark("recovery-marker", 7, false).unwrap();
    assert_eq!(fs::read_to_string(&marker).unwrap(), "v1 1\n");
    registry.mark("recovery-marker", 7, true).unwrap();
    assert_eq!(fs::read_to_string(&marker).unwrap(), "v1 7\n");

    registry.clear("recovery-marker").unwrap();
    registry.clear("recovery-marker").unwrap();
    assert!(!marker.exists());
    assert_eq!(profile.listed(), Ok(Vec::new()));
    assert_eq!(
        registry.mark("../escape", 1, true),
        Err(SessionError::InvalidUsageRecoveryIndex)
    );
    assert_eq!(
        registry.mark("recovery-marker", -1, true),
        Err(SessionError::InvalidUsageRecoveryIndex)
    );
}

#[test]
fn listing_sorts_markers_and_refuses_an_index_it_cannot_trust() {
    let profile = Profile::new();
    let registry = profile.registry();
    registry.mark("second", 2, true).unwrap();
    registry.mark("first", 1, true).unwrap();
    let ids: Vec<String> = profile
        .listed()
        .unwrap()
        .into_iter()
        .map(|marked| marked.id)
        .collect();
    assert_eq!(ids, ["first", "second"]);

    let marker = profile.markers().join("broken");
    for content in [
        "v2 1\n",
        "v1 \n",
        "v1 -1\n",
        "v1 1",
        "v1 x\n",
        "",
        "v1 123456789012345678901\n",
    ] {
        fs::write(&marker, content).unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            profile.listed(),
            Err(SessionError::InvalidUsageRecoveryIndex),
            "{content:?}"
        );
    }
    fs::write(&marker, "v1 3\n").unwrap();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        profile.listed(),
        Err(SessionError::InvalidUsageRecoveryIndex)
    );
    fs::remove_file(&marker).unwrap();
    fs::create_dir(profile.markers().join("folder")).unwrap();
    assert_eq!(
        profile.listed(),
        Err(SessionError::InvalidUsageRecoveryIndex)
    );
    fs::remove_dir(profile.markers().join("folder")).unwrap();
    assert_eq!(profile.listed().map(|listed| listed.len()), Ok(2));

    fs::set_permissions(profile.markers(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        profile.listed(),
        Err(SessionError::InvalidUsageRecoveryIndex)
    );
    assert_eq!(
        registry.clear("first"),
        Err(SessionError::InvalidUsageRecoveryIndex)
    );
}

#[test]
fn the_index_holds_at_most_512_sessions() {
    let profile = Profile::new();
    let registry = profile.registry();
    for index in 0..MAX_MARKED_SESSIONS {
        registry.mark(&format!("session-{index}"), 1, true).unwrap();
    }
    assert_eq!(
        profile.listed().map(|listed| listed.len()),
        Ok(MAX_MARKED_SESSIONS)
    );
    registry.mark("one-too-many", 1, true).unwrap();
    assert_eq!(
        profile.listed(),
        Err(SessionError::InvalidUsageRecoveryIndex)
    );
}
