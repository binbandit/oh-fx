mod git_context;
mod github_workflows;

pub use git_context::{GitSnapshot, snapshot};
pub use github_workflows::{NotGitRepository, Workflow, draft_prompt};
