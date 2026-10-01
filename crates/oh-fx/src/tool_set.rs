use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ofx_contract::Tool;
use ofx_exec::ManagedExecutions;
use ofx_tools::{EditFile, GlobFiles, GrepFiles, ReadFile, Shell, WriteFile};

pub(crate) fn ask_tools(
    workspace_root: &Path,
    executions: &ManagedExecutions,
    command_timeout: Option<Duration>,
) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFile::new(workspace_root)),
        Arc::new(GlobFiles::new(workspace_root)),
        Arc::new(GrepFiles::new(workspace_root)),
        Arc::new(EditFile::new(workspace_root)),
        Arc::new(WriteFile::new(workspace_root)),
        Arc::new(Shell::new(
            workspace_root,
            executions.clone(),
            command_timeout,
        )),
    ]
}
