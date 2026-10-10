use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use ofx_agent::{
    Agent, AgentConfig, ChildAgents, ChildDefaults, ChildSettings, ProjectContext,
    SkillContextProvider, SubagentHost, WorkTools,
};
use ofx_config::ProviderDefinition;
use ofx_contract::{
    ActiveMode, ApprovalRequest, CapabilityResolver, DynamicTools, HookScope, HookView,
    LiveAdditionalRoots, LivePermissionMode, ModelProvider, ReasoningEffort, ReviewTransport,
    RootUserRequests, SubagentProvider, Tool, TurnId,
};
use ofx_exec::ManagedExecutions;
use ofx_mcp::McpRuntime;
use ofx_permissions::{
    DEFAULT_REVIEW_TIMEOUT, PermissionPolicy, Reviewer, canonical_root_user_context,
};
use ofx_tools::SubagentTool;

use crate::app_bootstrap_runtime::output_tokens;
use crate::approval_queue::ApprovalQueue;
use crate::context::{HostProjectContext, HostRuntimeContext};
use crate::mcp_model_catalog::McpServers;
use crate::skills::HostSkills;
use crate::tool_set::{self, ToolHooks};

#[derive(Clone)]
pub(crate) struct ChildRoute {
    pub(crate) provider: Arc<dyn ModelProvider>,
    pub(crate) capabilities: Arc<dyn CapabilityResolver>,
    pub(crate) connection: Option<ProviderDefinition>,
    pub(crate) reviewer: Arc<dyn ReviewTransport>,
}

pub(crate) struct ChildFactory {
    pub(crate) route: Mutex<ChildRoute>,
    pub(crate) executions: ManagedExecutions,
    pub(crate) command_timeout: Option<Duration>,
    pub(crate) parent_permissions: Arc<PermissionPolicy>,
    pub(crate) approvals: Option<Arc<ApprovalQueue>>,
    pub(crate) project: Option<(Arc<HostProjectContext>, ProjectContext)>,
    pub(crate) skills: Arc<HostSkills>,
    pub(crate) mcp: Option<Arc<McpRuntime>>,
    pub(crate) workspace_root: PathBuf,
    pub(crate) additional_roots: LiveAdditionalRoots,
    pub(crate) permission_mode: LivePermissionMode,
    pub(crate) parent: Mutex<AgentConfig>,
    pub(crate) mode: Option<ActiveMode>,
    pub(crate) hooks: OnceLock<HookView>,
}

pub(crate) struct ParentCatalog(Arc<dyn DynamicTools>);

impl ParentCatalog {
    pub(crate) fn shared(source: Option<Arc<dyn DynamicTools>>) -> Option<Arc<dyn DynamicTools>> {
        source.map(|source| Arc::new(Self(source)) as Arc<dyn DynamicTools>)
    }
}

impl DynamicTools for ParentCatalog {
    fn generation(&self) -> u64 {
        self.0.generation()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.0.tools()
    }

    fn take_notices(&self) -> Vec<String> {
        Vec::new()
    }
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

    pub(crate) fn attach_hooks(&self, hooks: HookView) {
        let _ = self.hooks.set(hooks);
    }

    pub(crate) fn reroute(&self, route: ChildRoute) {
        *self.route.lock().unwrap_or_else(PoisonError::into_inner) = route;
    }

    fn route(&self) -> ChildRoute {
        self.route
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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
        let route = self.route();
        let config = AgentConfig {
            model: settings.model.clone(),
            max_output_tokens: output_tokens(route.connection.as_ref(), &settings.model),
            reasoning_effort: settings.effort.clone().into_named(),
            fast_mode: settings.fast_mode,
            ultrafast_mode: false,
            ..self.parent_config().clone()
        };
        let permissions =
            PermissionPolicy::new(permission_mode.clone(), self.workspace_root.clone())
                .with_additional_roots(self.additional_roots.clone())
                .with_reviewer(Reviewer::new(route.reviewer, DEFAULT_REVIEW_TIMEOUT))
                .inheriting_grants_of(&self.parent_permissions);
        let mut agent = Agent::new(
            route.provider,
            Vec::new(),
            Arc::new(
                HostRuntimeContext::new(self.workspace_root.clone(), permission_mode, false)
                    .with_additional_roots(self.additional_roots.clone()),
            ),
            Arc::new(permissions),
            config,
        )
        .with_skills(Arc::clone(&self.skills) as Arc<dyn SkillContextProvider>)
        .with_capability_resolver(route.capabilities)
        .with_mcp_servers(Arc::new(McpServers::new(self.mcp.clone(), false)))
        .with_lifecycle(
            self.hooks.get().cloned().unwrap_or_default(),
            HookScope::Subagent,
        );
        if let Some(mcp) =
            ParentCatalog::shared(self.mcp.clone().map(|mcp| mcp as Arc<dyn DynamicTools>))
        {
            agent = agent.with_dynamic_tools(mcp);
        }
        if let Some(approvals) = &self.approvals {
            agent = agent.with_approvals(approvals.approvals().clone());
        }
        if let Some(mode) = self.mode {
            agent = agent.with_mode(mode);
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
            self.skills.search(),
            ToolHooks {
                additional_roots: self.additional_roots.clone(),
                ..ToolHooks::default()
            },
        );
        WorkTools {
            tools,
            release: Box::pin(async move { executions.shutdown().await }),
        }
    }

    fn approval_requested(&self, turn_id: Option<TurnId>, request: ApprovalRequest) {
        if let Some(approvals) = &self.approvals {
            approvals.child(turn_id, request);
        }
    }

    fn root_user_context(&self, requests: &RootUserRequests) -> String {
        canonical_root_user_context(requests)
    }

    fn approval_feedback(&self, turn_id: Option<TurnId>, text: String) {
        if let Some(approvals) = &self.approvals {
            approvals.child_feedback(turn_id, text);
        }
    }
}

#[cfg(test)]
mod tests;
