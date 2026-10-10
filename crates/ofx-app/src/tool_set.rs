use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ofx_contract::{LiveAdditionalRoots, LivePermissionMode, McpToolSearch, QuestionAsker, Tool};
use ofx_exec::ManagedExecutions;
use ofx_mcp::{McpFeatures, McpRuntime};
use ofx_tools::{
    AskUserQuestion, CapabilitySearch, EditFile, GlobFiles, GrepFiles, ReadFile, Shell, SkillTool,
    WebFetch, WebFetchProgress, WebSearch, WriteFile,
};
use ofx_workspace::ChangeTracker;

pub(crate) const READ_ONLY_TOOL_NAMES: [&str; 3] = ["read_file", "glob_files", "grep_files"];

#[derive(Default)]
pub(crate) struct ToolHooks<'a> {
    pub(crate) questions: Option<Arc<dyn QuestionAsker>>,
    pub(crate) web_fetch_progress: Option<WebFetchProgress>,
    pub(crate) change_tracker: Option<&'a ChangeTracker>,
    pub(crate) additional_roots: LiveAdditionalRoots,
}

pub(crate) fn ask_tools(
    workspace_root: &Path,
    executions: &ManagedExecutions,
    command_timeout: Option<Duration>,
    permission_mode: &LivePermissionMode,
    skill: Arc<SkillTool>,
    search: Arc<CapabilitySearch>,
    hooks: ToolHooks<'_>,
) -> Vec<Arc<dyn Tool>> {
    let mut edit_file = EditFile::new(workspace_root)
        .with_permission_mode(permission_mode.clone())
        .with_additional_roots(hooks.additional_roots.clone());
    let mut write_file = WriteFile::new(workspace_root)
        .with_permission_mode(permission_mode.clone())
        .with_additional_roots(hooks.additional_roots);
    if let Some(tracker) = hooks.change_tracker {
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
        search,
        skill,
        Arc::new(AskUserQuestion::new(hooks.questions)),
        Arc::new(
            hooks
                .web_fetch_progress
                .map_or_else(WebFetch::default, WebFetch::reporting_progress),
        ),
        Arc::new(WebSearch::default()),
    ]
}

pub(crate) fn with_mcp(
    mut tools: Vec<Arc<dyn Tool>>,
    search: &CapabilitySearch,
    runtime: Option<Arc<McpRuntime>>,
) -> Vec<Arc<dyn Tool>> {
    if let Some(runtime) = &runtime
        && let Some(slot) = tools
            .iter_mut()
            .find(|tool| tool.spec().name == search.spec().name)
    {
        *slot = Arc::new(search.searching_mcp(Arc::clone(runtime) as Arc<dyn McpToolSearch>));
    }
    let skill = tools
        .iter()
        .position(|tool| tool.spec().name == "skill")
        .map_or(tools.len(), |index| index + 1);
    tools.insert(skill, Arc::new(McpFeatures::new(runtime)));
    tools
}

pub(crate) fn with_subagent(
    tools: &[Arc<dyn Tool>],
    subagent: &Arc<dyn Tool>,
) -> Vec<Arc<dyn Tool>> {
    let shell = tools
        .iter()
        .position(|tool| tool.spec().name == "shell")
        .map_or(tools.len(), |index| index + 1);
    let mut delegating = tools.to_vec();
    delegating.insert(shell, Arc::clone(subagent));
    delegating
}

#[cfg(test)]
mod tests;
