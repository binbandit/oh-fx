mod file_mutation;
mod file_mutation_execution;
mod filesystem;
mod shell;
mod skill;
mod tool_admission;
mod tool_args;
mod tool_runtime;
mod web;

pub use filesystem::{EditFile, GlobFiles, GrepFiles, ReadFile, WriteFile};
pub use shell::Shell;
pub use skill::SkillTool;

#[cfg(test)]
mod tests {
    use ofx_config::ContextLimits;
    use ofx_contract::Tool;
    use ofx_exec::{ManagedExecutions, SessionSupervisor};
    use ofx_skills::{RootPolicy, SkillDiscoveryContext, SymlinkAuthorities};
    use serde_json::Value;

    use super::*;

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
        let tools: [&dyn Tool; 7] = [
            &ReadFile::new("/"),
            &GlobFiles::new("/"),
            &GrepFiles::new("/"),
            &WriteFile::new("/"),
            &EditFile::new("/"),
            &shell,
            &skill,
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
