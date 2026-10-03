mod ask_user_question;
mod capability_search;
mod file_mutation;
mod file_mutation_execution;
mod filesystem;
mod shell;
mod skill;
mod subagent;
mod tool_admission;
mod tool_args;
mod tool_runtime;
mod web;

pub use ask_user_question::{AskUserQuestion, answered_questions};
pub use capability_search::CapabilitySearch;
pub use filesystem::{EditFile, GlobFiles, GrepFiles, ReadFile, WriteFile};
pub use shell::Shell;
pub use skill::SkillTool;
pub use subagent::SubagentTool;
pub use web::{WebFetch, WebFetchProgress, WebSearch};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ofx_config::ContextLimits;
    use ofx_contract::{
        BoxFuture, SubagentProvider, SubagentRequest, Tool, ToolContext, ToolOutput,
    };
    use ofx_exec::{ManagedExecutions, SessionSupervisor};
    use ofx_skills::{RootPolicy, SkillDiscoveryContext, SymlinkAuthorities};
    use serde_json::Value;

    use super::*;

    struct NoProvider;

    impl SubagentProvider for NoProvider {
        fn execute(
            &self,
            _request: SubagentRequest,
            _context: ToolContext,
        ) -> BoxFuture<'static, ToolOutput> {
            Box::pin(async { ToolOutput::failure("unused") })
        }
    }

    #[test]
    fn every_tool_has_a_bounded_description_and_a_schema_that_round_trips() {
        let shell = Shell::new(
            "/",
            ManagedExecutions::new(SessionSupervisor::new("/nonexistent")),
            None,
        );
        let skill = SkillTool::new(
            SkillDiscoveryContext {
                workspace_root: None,
                home: None,
                managed_root: "/nonexistent".into(),
                symlink_authorities: SymlinkAuthorities::default(),
            },
            RootPolicy {
                workspace_roots: &[],
                managed_root_source: None,
                global_roots: &[],
            },
            ContextLimits::default(),
        );
        let subagent = SubagentTool::new(Arc::new(NoProvider));
        let ask = AskUserQuestion::new(None);
        let tools: [&dyn Tool; 11] = [
            &ReadFile::new("/"),
            &GlobFiles::new("/"),
            &GrepFiles::new("/"),
            &WriteFile::new("/"),
            &EditFile::new("/"),
            &shell,
            &skill,
            &subagent,
            &ask,
            &WebFetch::default(),
            &WebSearch::default(),
        ];
        for tool in tools {
            let spec = tool.spec();
            assert!(spec.description.len() <= 1024, "{}", spec.name);
            let schema: Value = serde_json::from_str(spec.input_schema).unwrap();
            assert!(schema.is_object(), "{}", spec.name);
            assert_eq!(schema.to_string(), spec.input_schema, "{}", spec.name);
        }
    }
}
