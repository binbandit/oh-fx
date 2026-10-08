use std::path::Path;

use ofx_config::ProfilePaths;
use ofx_contract::{DirectoryAccess, LiveAdditionalRoots, WorkspaceMenu, WorkspaceMenuEntry};
use ofx_workspace::{
    Action, Failure, MAX_ADDITIONAL_DIRECTORIES, Outcome, WorkspaceAccess, WorkspaceAccessError,
    execute,
};

pub(crate) struct WorkspaceRuntime {
    access: WorkspaceAccess,
    roots: LiveAdditionalRoots,
    paths: Option<ProfilePaths>,
}

impl WorkspaceRuntime {
    pub(crate) fn new(
        access: WorkspaceAccess,
        roots: LiveAdditionalRoots,
        paths: Option<ProfilePaths>,
    ) -> Self {
        Self {
            access,
            roots,
            paths,
        }
    }

    pub(crate) fn access(&self) -> &WorkspaceAccess {
        &self.access
    }

    pub(crate) fn live_roots(&self) -> LiveAdditionalRoots {
        self.roots.clone()
    }

    pub(crate) fn execute(&self, action: &Action) -> Result<Outcome, Failure> {
        execute(self.paths.as_ref(), &self.access, action)
    }

    pub(crate) fn install(&mut self, access: WorkspaceAccess) {
        let changed = access.entries() != self.access.entries();
        self.access = access;
        if changed {
            self.roots
                .set(self.access.active_roots().map(Path::to_path_buf).collect());
        }
    }

    pub(crate) fn refresh_availability(&mut self) -> Result<(), WorkspaceAccessError> {
        if let Some(replacement) = self.access.stage_availability_refresh()? {
            self.install(replacement);
        }
        Ok(())
    }

    pub(crate) fn menu(&self) -> WorkspaceMenu {
        WorkspaceMenu {
            primary: self.access.primary().to_path_buf(),
            saved_suppressed: self.access.saved_suppressed(),
            limit: MAX_ADDITIONAL_DIRECTORIES,
            entries: self
                .access
                .entries()
                .iter()
                .map(|entry| WorkspaceMenuEntry {
                    path: entry.path.clone(),
                    saved: entry.source.saved,
                    command_line: entry.source.command_line,
                    access: DirectoryAccess::of(entry.available, entry.active),
                })
                .collect(),
        }
    }
}
