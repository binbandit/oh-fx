use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ofx_agent::{
    Agent, AgentConfig, ChildAgents, ChildDefaults, ChildSettings, ProjectContext,
    SkillContextProvider, SubagentHost, WorkTools,
};
use ofx_config::ProviderDefinition;
use ofx_contract::{
    ApprovalRequest, CapabilityResolver, LivePermissionMode, ModelProvider, ReasoningEffort,
    ReviewTransport, SubagentProvider, Tool,
};
use ofx_exec::ManagedExecutions;
use ofx_permissions::{DEFAULT_REVIEW_TIMEOUT, PermissionPolicy, Reviewer};
use ofx_tools::SubagentTool;

use crate::app_bootstrap_runtime::output_tokens;
use crate::approval_queue::ApprovalQueue;
use crate::context::{HostProjectContext, HostRuntimeContext};
use crate::skills::HostSkills;
use crate::tool_set::{self, ToolHooks};

pub(crate) struct ChildFactory {
    pub(crate) provider: Arc<dyn ModelProvider>,
    pub(crate) executions: ManagedExecutions,
    pub(crate) command_timeout: Option<Duration>,
    pub(crate) capabilities: Option<Arc<dyn CapabilityResolver>>,
    pub(crate) connection: Option<ProviderDefinition>,
    pub(crate) reviewer: Arc<dyn ReviewTransport>,
    pub(crate) parent_permissions: Arc<PermissionPolicy>,
    pub(crate) approvals: Option<Arc<ApprovalQueue>>,
    pub(crate) project: Option<(Arc<HostProjectContext>, ProjectContext)>,
    pub(crate) skills: Arc<HostSkills>,
    pub(crate) workspace_root: PathBuf,
    pub(crate) permission_mode: LivePermissionMode,
    pub(crate) parent: Mutex<AgentConfig>,
}

pub(crate) struct Delegation {
    pub(crate) tool: Arc<dyn Tool>,
    pub(crate) host: Arc<SubagentHost>,
    pub(crate) children: Arc<ChildFactory>,
}

impl Delegation {
    pub(crate) fn new(children: ChildFactory) -> Self {
        let children = Arc::new(children);
        let host = Arc::new(SubagentHost::new(
            Arc::clone(&children) as Arc<dyn ChildAgents>
        ));
        Self {
            tool: Arc::new(SubagentTool::new(
                Arc::clone(&host) as Arc<dyn SubagentProvider>
            )),
            host,
            children,
        }
    }
}

impl ChildFactory {
    pub(crate) fn follow(&self, config: &AgentConfig) {
        *self.parent_config() = config.clone();
    }

    fn parent_config(&self) -> MutexGuard<'_, AgentConfig> {
        self.parent.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ChildAgents for ChildFactory {
    fn defaults(&self) -> ChildDefaults {
        let config = self.parent_config();
        ChildDefaults {
            settings: ChildSettings {
                model: config.model.clone(),
                effort: config
                    .reasoning_effort
                    .as_deref()
                    .and_then(ReasoningEffort::parse)
                    .unwrap_or(ReasoningEffort::Auto),
                fast_mode: config.fast_mode,
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
            ..self.parent_config().clone()
        };
        let permissions =
            PermissionPolicy::new(permission_mode.clone(), self.workspace_root.clone())
                .with_reviewer(Reviewer::new(
                    Arc::clone(&self.reviewer),
                    DEFAULT_REVIEW_TIMEOUT,
                ))
                .inheriting_grants_of(&self.parent_permissions);
        let mut agent = Agent::new(
            Arc::clone(&self.provider),
            Vec::new(),
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
        if let Some(approvals) = &self.approvals {
            agent = agent.with_approvals(approvals.approvals().clone());
        }
        match &self.project {
            Some((provider, snapshot)) => {
                agent.with_project_context(provider.clone(), snapshot.clone())
            }
            None => agent,
        }
    }

    fn work_tools(&self) -> WorkTools {
        let executions = self.executions.separate();
        let tools = tool_set::ask_tools(
            &self.workspace_root,
            &executions,
            self.command_timeout,
            &self.permission_mode,
            self.skills.tool(),
            ToolHooks::default(),
        );
        WorkTools {
            tools,
            release: Box::pin(async move { executions.shutdown().await }),
        }
    }

    fn approval_requested(&self, request: ApprovalRequest) {
        if let Some(approvals) = &self.approvals {
            approvals.child(request);
        }
    }
}
