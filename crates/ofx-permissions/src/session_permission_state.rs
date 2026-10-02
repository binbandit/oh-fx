use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use ofx_contract::{CommandRequest, GatedAction, PathAccess};
use ofx_workspace::path_inside;

pub(crate) const EDIT_PERMISSION: &str = "edit";
const BASH_PERMISSION: &str = "bash";
const PATH_ALWAYS_PERMISSIONS: [&str; 4] = [EDIT_PERMISSION, "read", "glob", "grep"];

#[derive(Debug, Default)]
pub(crate) struct SessionGrants {
    grants: Mutex<Vec<Grant>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Grant {
    permission: String,
    scope: Scope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    Tree(PathBuf),
    Command(String),
}

pub(crate) fn permission_name(tool_name: &str) -> &str {
    match tool_name {
        "read_file" => "read",
        "write_file" | "edit_file" => EDIT_PERMISSION,
        "glob_files" => "glob",
        "grep_files" => "grep",
        "shell" => BASH_PERMISSION,
        other => other,
    }
}

impl SessionGrants {
    pub(crate) fn remember(
        &self,
        workspace_root: &Path,
        action: GatedAction<'_>,
        access: &PathAccess,
    ) {
        let grants = match action {
            GatedAction::Call(call) => match access {
                PathAccess::Within(root) => vec![Grant {
                    permission: permission_name(&call.name).to_owned(),
                    scope: Scope::Tree(root.clone()),
                }],
                PathAccess::WorkspaceOnly | PathAccess::WorkspaceOrExternal => Vec::new(),
            },
            GatedAction::FileMutation(mutation) => {
                path_grants(workspace_root, EDIT_PERMISSION, &mutation.target)
            }
            GatedAction::Command(CommandRequest::Run { command, .. }) => vec![Grant {
                permission: BASH_PERMISSION.to_owned(),
                scope: Scope::Command(command.clone()),
            }],
            GatedAction::Command(_) => Vec::new(),
        };
        self.lock().extend(grants);
    }

    pub(crate) fn granted_root(&self, permission: &str, target: &Path) -> Option<PathBuf> {
        self.lock()
            .iter()
            .filter(|grant| grant.permission == permission)
            .filter_map(|grant| match &grant.scope {
                Scope::Tree(root) if path_inside(root, target) => Some(root),
                _ => None,
            })
            .min_by_key(|root| root.as_os_str().len())
            .cloned()
    }

    pub(crate) fn allow_command(&self, request: &CommandRequest) -> bool {
        let CommandRequest::Run { command, .. } = request else {
            return false;
        };
        self.lock()
            .iter()
            .any(|grant| matches!(&grant.scope, Scope::Command(granted) if granted == command))
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Grant>> {
        self.grants.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn path_grants(workspace_root: &Path, permission: &str, target: &Path) -> Vec<Grant> {
    if path_inside(workspace_root, target) {
        return PATH_ALWAYS_PERMISSIONS
            .iter()
            .map(|permission| Grant {
                permission: (*permission).to_owned(),
                scope: Scope::Tree(workspace_root.to_path_buf()),
            })
            .collect();
    }
    external_grant_root(target)
        .map(|root| Grant {
            permission: permission.to_owned(),
            scope: Scope::Tree(root.to_path_buf()),
        })
        .into_iter()
        .collect()
}

pub(crate) fn external_grant_root(target: &Path) -> Option<&Path> {
    if target.is_dir() {
        Some(target)
    } else {
        target.parent()
    }
}
