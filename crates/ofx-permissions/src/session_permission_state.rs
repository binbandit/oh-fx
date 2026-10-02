use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use ofx_contract::{CommandRequest, SessionGrant};
use ofx_workspace::path_inside;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TreePermission {
    Edit,
    Read,
    Glob,
    Grep,
}

impl TreePermission {
    pub(crate) fn of_tool(tool_name: &str) -> Option<Self> {
        match tool_name {
            "read_file" => Some(Self::Read),
            "glob_files" => Some(Self::Glob),
            "grep_files" => Some(Self::Grep),
            _ => None,
        }
    }

    pub(crate) fn grant_under(self, root: PathBuf) -> SessionGrant {
        match self {
            Self::Edit => SessionGrant::FileChangesUnder(root),
            Self::Read => SessionGrant::ReadsUnder(root),
            Self::Glob => SessionGrant::GlobsUnder(root),
            Self::Grep => SessionGrant::GrepsUnder(root),
        }
    }

    fn root_of<'a>(self, grant: &'a SessionGrant, workspace_root: &'a Path) -> Option<&'a Path> {
        match (grant, self) {
            (SessionGrant::WorkspaceFiles, _) => Some(workspace_root),
            (SessionGrant::FileChangesUnder(root), Self::Edit)
            | (SessionGrant::ReadsUnder(root), Self::Read)
            | (SessionGrant::GlobsUnder(root), Self::Glob)
            | (SessionGrant::GrepsUnder(root), Self::Grep) => Some(root),
            _ => None,
        }
    }
}

pub(crate) fn command_grant(request: &CommandRequest) -> Option<SessionGrant> {
    match request {
        CommandRequest::Run {
            command,
            profile,
            terminal,
            ..
        } => Some(SessionGrant::Command {
            command: command.clone(),
            profile: *profile,
            terminal: *terminal,
        }),
        CommandRequest::Observe | CommandRequest::SendInput { .. } | CommandRequest::Stop => None,
    }
}

#[derive(Debug, Default)]
pub(crate) struct SessionGrants {
    grants: Mutex<Vec<SessionGrant>>,
}

impl SessionGrants {
    pub(crate) fn remember(&self, grant: &SessionGrant) {
        let mut grants = self.lock();
        if !grants.contains(grant) {
            grants.push(grant.clone());
        }
    }

    pub(crate) fn granted_root(
        &self,
        workspace_root: &Path,
        permission: TreePermission,
        target: &Path,
    ) -> Option<PathBuf> {
        self.lock()
            .iter()
            .filter_map(|grant| permission.root_of(grant, workspace_root))
            .filter(|root| path_inside(root, target))
            .min_by_key(|root| root.as_os_str().len())
            .map(Path::to_path_buf)
    }

    pub(crate) fn allow_command(&self, request: &CommandRequest) -> bool {
        command_grant(request).is_some_and(|grant| self.lock().contains(&grant))
    }

    fn lock(&self) -> MutexGuard<'_, Vec<SessionGrant>> {
        self.grants.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
