use std::path::PathBuf;
use std::sync::Arc;

use ofx_agent::{
    Agent, AgentConfig, ChildAgents, ChildDefaults, ChildSettings, ProjectContext,
    SkillContextProvider,
};
use ofx_config::ProviderDefinition;
use ofx_contract::{
    ApprovalRequest, CapabilityResolver, LivePermissionMode, ModelProvider, ReasoningEffort,
    ReviewTransport, Tool,
};
use ofx_permissions::{DEFAULT_REVIEW_TIMEOUT, PermissionPolicy, Reviewer};

use crate::app_bootstrap_runtime::output_tokens;
use crate::context::{HostProjectContext, HostRuntimeContext};
use crate::skills::HostSkills;

pub(crate) struct ChildFactory {
    pub(crate) provider: Arc<dyn ModelProvider>,
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) capabilities: Option<Arc<dyn CapabilityResolver>>,
    pub(crate) connection: Option<ProviderDefinition>,
    pub(crate) reviewer: Arc<dyn ReviewTransport>,
    pub(crate) project: Option<(Arc<HostProjectContext>, ProjectContext)>,
    pub(crate) skills: Arc<HostSkills>,
    pub(crate) workspace_root: PathBuf,
    pub(crate) permission_mode: LivePermissionMode,
    pub(crate) config: AgentConfig,
}

impl ChildAgents for ChildFactory {
    fn defaults(&self) -> ChildDefaults {
        ChildDefaults {
            settings: ChildSettings {
                model: self.config.model.clone(),
                effort: self
                    .config
                    .reasoning_effort
                    .as_deref()
                    .and_then(ReasoningEffort::parse)
                    .unwrap_or(ReasoningEffort::Auto),
                fast_mode: self.config.fast_mode,
            },
            permission_mode: self.permission_mode.get(),
        }
    }

    fn agent(&self, settings: &ChildSettings, permission_mode: LivePermissionMode) -> Agent {
        let config = AgentConfig {
            model: settings.model.clone(),
            max_output_tokens: output_tokens(self.connection.as_ref(), &settings.model),
            reasoning_effort: settings.effort.clone().into_named(),
            fast_mode: settings.fast_mode,
            ..self.config.clone()
        };
        let permissions =
            PermissionPolicy::new(permission_mode.clone(), self.workspace_root.clone())
                .with_reviewer(Reviewer::new(
                    Arc::clone(&self.reviewer),
                    DEFAULT_REVIEW_TIMEOUT,
                ));
        let mut agent = Agent::new(
            Arc::clone(&self.provider),
            self.tools.clone(),
            Arc::new(HostRuntimeContext::new(
                self.workspace_root.clone(),
                permission_mode,
                false,
            )),
            Arc::new(permissions),
            config,
        )
        .with_skills(Arc::clone(&self.skills) as Arc<dyn SkillContextProvider>);
        if let Some(capabilities) = &self.capabilities {
            agent = agent.with_capability_resolver(Arc::clone(capabilities));
        }
        match &self.project {
            Some((provider, snapshot)) => {
                agent.with_project_context(provider.clone(), snapshot.clone())
            }
            None => agent,
        }
    }

    fn approval_requested(&self, _request: ApprovalRequest) {}
}
