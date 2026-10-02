use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use tempfile::TempDir;

use crate::skill_contract::{RootPolicy, SkillSource};
use crate::skill_runtime::{SkillDiscovery, SkillDiscoveryContext, SymlinkAuthorities};

pub(crate) const MANAGED_ROOT_POLICY: RootPolicy = RootPolicy {
    workspace_roots: &[],
    managed_root_source: Some(SkillSource::GlobalOhFx),
    global_roots: &[],
};

pub(crate) struct Fixture {
    _temp: TempDir,
    root: PathBuf,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        Self { _temp: temp, root }
    }

    pub(crate) fn path(&self, sub_path: &str) -> PathBuf {
        self.root.join(sub_path)
    }

    pub(crate) fn write(&self, sub_path: &str, content: impl AsRef<[u8]>) {
        let path = self.path(sub_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    pub(crate) fn mkdir(&self, sub_path: &str) {
        fs::create_dir_all(self.path(sub_path)).unwrap();
    }

    pub(crate) fn symlink(&self, target: &str, link: &str) {
        let link = self.path(link);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(target, link).unwrap();
    }

    pub(crate) fn real(&self, sub_path: &str) -> PathBuf {
        fs::canonicalize(self.path(sub_path)).unwrap()
    }

    pub(crate) fn managed_discovery(&self, managed: &str) -> SkillDiscovery {
        SkillDiscoveryContext {
            workspace_root: None,
            home: None,
            managed_root: self.path(managed),
            symlink_authorities: SymlinkAuthorities::default(),
        }
        .load_visible_skills(&MANAGED_ROOT_POLICY)
    }
}
