#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpMenuSnapshot {
    pub servers: Vec<McpMenuServer>,
    pub configuration_issues: Vec<String>,
    pub profile_config: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpMenuServer {
    pub name: String,
    pub source: McpMenuSource,
    pub admission: Option<McpMenuAdmission>,
    pub transport: McpMenuTransport,
    pub required: bool,
    pub connection: McpMenuConnection,
    pub authentication: McpMenuAuthentication,
    pub status: McpMenuStatus,
    pub reloading: bool,
    pub protocol_version: Option<String>,
    pub counts: McpMenuCounts,
    pub failure: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpMenuSource {
    Profile,
    Project,
    Acp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpMenuAdmission {
    Pending,
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpMenuTransport {
    Stdio,
    Http,
    Sse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpMenuConnection {
    Disconnected,
    Disabled,
    Connecting,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpMenuAuthentication {
    None,
    Configured,
    Authenticated,
    Required,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpMenuStatus {
    Disabled,
    Connecting,
    Ready,
    NeedsAuth,
    Failed,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpMenuCounts {
    pub tools: Option<usize>,
    pub resources: Option<usize>,
    pub resource_templates: Option<usize>,
    pub prompts: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpMenuRequest {
    Refresh,
    Reload {
        generation: u64,
    },
    Authenticate {
        generation: u64,
        server: String,
    },
    Logout {
        generation: u64,
        server: String,
    },
    Add {
        generation: u64,
        form: McpMenuAddForm,
    },
    Remove {
        generation: u64,
        server: String,
    },
    Trust {
        generation: u64,
        change: McpMenuTrust,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpMenuAddForm {
    pub transport: McpMenuAddTransport,
    pub name: String,
    pub target: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpMenuAddTransport {
    Local,
    Http,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpMenuTrust {
    Approve(String),
    Reject(String),
    ApproveAll,
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpMenuSettled {
    pub generation: u64,
    pub outcome: McpMenuOutcome,
    pub feedback: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpMenuOutcome {
    Continuing,
    Returned,
    Succeeded,
    Failed,
}
