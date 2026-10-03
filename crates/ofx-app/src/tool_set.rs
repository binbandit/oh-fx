use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ofx_contract::{LivePermissionMode, Tool};
use ofx_exec::ManagedExecutions;
use ofx_tools::{
    EditFile, GlobFiles, GrepFiles, ReadFile, Shell, SkillTool, WebFetch, WebFetchProgress,
    WriteFile,
};
use ofx_workspace::ChangeTracker;

pub(crate) fn ask_tools(
    workspace_root: &Path,
    executions: &ManagedExecutions,
    command_timeout: Option<Duration>,
    permission_mode: &LivePermissionMode,
    skill: Arc<SkillTool>,
    web_fetch_progress: Option<WebFetchProgress>,
    change_tracker: Option<&ChangeTracker>,
) -> Vec<Arc<dyn Tool>> {
    let mut edit_file = EditFile::new(workspace_root).with_permission_mode(permission_mode.clone());
    let mut write_file =
        WriteFile::new(workspace_root).with_permission_mode(permission_mode.clone());
    if let Some(tracker) = change_tracker {
        edit_file = edit_file.with_change_tracker(tracker.clone());
        write_file = write_file.with_change_tracker(tracker.clone());
    }
    vec![
        Arc::new(ReadFile::new(workspace_root)),
        Arc::new(GlobFiles::new(workspace_root)),
        Arc::new(GrepFiles::new(workspace_root)),
        Arc::new(edit_file),
        Arc::new(write_file),
        Arc::new(Shell::new(
            workspace_root,
            executions.clone(),
            command_timeout,
        )),
        skill,
        Arc::new(web_fetch_progress.map_or_else(WebFetch::default, WebFetch::reporting_progress)),
    ]
}

#[cfg(test)]
mod tests;
