use std::fmt::Write as _;
use std::path::PathBuf;

use ofx_cli::OutputFormat;
use ofx_contract::PermissionMode;
use ofx_mcp::{LocalConfigInspection, ProfileConfigDiagnostic, WorkspaceAdmission};
use ofx_text::encode_terminal_safe;
use serde_json::{Map, Value, json};

use crate::app_bootstrap_runtime::CredentialSource;

const HOST_MANAGED_SOURCE_LABEL: &str = "host managed";
const CODEX_CONNECTION: &str = "Codex";
const CODEX_CONNECTION_ID: &str = "codex";
const GROK_CONNECTION: &str = "Grok";
const GROK_CONNECTION_ID: &str = "grok";
const NOT_CHECKED: &str = "not_checked";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Credential {
    Connection,
    Codex { expired: bool },
    HostManaged,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SavedLogins {
    pub(crate) codex: bool,
    pub(crate) grok: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthStatus {
    pub(crate) active: Option<Credential>,
    pub(crate) help: Option<String>,
    pub(crate) logins: SavedLogins,
}

impl AuthStatus {
    fn label(&self) -> &'static str {
        match self.active {
            None => "missing",
            Some(Credential::Connection) => CredentialSource::Configured.label(),
            Some(Credential::Codex { .. }) => CredentialSource::Codex.label(),
            Some(Credential::HostManaged) => HOST_MANAGED_SOURCE_LABEL,
        }
    }

    fn refreshable(&self) -> bool {
        matches!(self.active, Some(Credential::Codex { .. }))
    }

    fn expired(&self) -> bool {
        self.active == Some(Credential::Codex { expired: true })
    }

    fn connected<'a>(&self, connection: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
        let mut connected = Vec::new();
        let connection_active = self.active == Some(Credential::Connection);
        if let Some(id) = connection.filter(|_| connection_active) {
            connected.push((id, id));
        }
        if self.logins.codex {
            connected.push((CODEX_CONNECTION, CODEX_CONNECTION_ID));
        }
        if self.logins.grok {
            connected.push((GROK_CONNECTION, GROK_CONNECTION_ID));
        }
        connected
    }
}

pub(crate) struct StatusReport {
    pub(crate) model: String,
    pub(crate) model_origin: &'static str,
    pub(crate) model_source: String,
    pub(crate) connection: Option<String>,
    pub(crate) provider_endpoint: Option<String>,
    pub(crate) auth: AuthStatus,
    pub(crate) permission_mode: PermissionMode,
    pub(crate) workspace_root: PathBuf,
    pub(crate) agent_step_limit: u64,
    pub(crate) mcp: LocalConfigInspection,
}

impl StatusReport {
    pub(crate) fn render(&self, format: OutputFormat) -> String {
        match format {
            OutputFormat::Text => self.render_text(),
            OutputFormat::Json => {
                let mut line = Value::Object(self.json()).to_string();
                line.push('\n');
                line
            }
        }
    }

    fn render_text(&self) -> String {
        let mut out = String::new();
        let mut line = |text: std::fmt::Arguments<'_>| {
            let _ = writeln!(out, "[status] {text}");
        };
        line(format_args!("model={}", safe(&self.model)));
        line(format_args!("model_origin={}", self.model_origin));
        line(format_args!("model_source={}", self.model_source));
        if let Some(endpoint) = &self.provider_endpoint {
            line(format_args!("provider_endpoint={endpoint}"));
        }
        match &self.mcp.profile_diagnostic {
            ProfileConfigDiagnostic::Clear => {}
            ProfileConfigDiagnostic::Failed(name) => line(format_args!("mcp_config_error={name}")),
            ProfileConfigDiagnostic::Warning(warning) => {
                let key = warning.key.as_ref().map_or_else(String::new, |key| {
                    format!(
                        " key={}",
                        encode_terminal_safe(key.as_bytes(), usize::MAX).text
                    )
                });
                line(format_args!(
                    "mcp_config_warning={}{key} additional_matches={}",
                    warning.cause.as_str(),
                    warning.additional_matches
                ));
            }
        }
        line(format_args!("auth={}", self.auth.label()));
        let connected = self.auth.connected(self.connection.as_deref());
        let names: Vec<&str> = connected.iter().map(|(name, _)| *name).collect();
        let names = if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        };
        line(format_args!("connected_providers={names}"));
        line(format_args!("auth_refreshable={}", self.auth.refreshable()));
        if self.auth.expired() {
            line(format_args!("auth_expired=true"));
        }
        if let Some(help) = &self.auth.help {
            line(format_args!("auth_help={help}"));
        }
        line(format_args!(
            "permission_mode={}",
            self.permission_mode.display_label()
        ));
        line(format_args!(
            "workspace={}",
            safe(&self.workspace_root.to_string_lossy())
        ));
        line(format_args!("history_turns=0"));
        line(format_args!("session_permission_grants=0"));
        line(format_args!("agent_step_limit={}", self.agent_step_limit));
        write_mcp_text(&mut out, "status", &self.mcp);
        out
    }

    fn json(&self) -> Map<String, Value> {
        let mut object = Map::new();
        object.insert("kind".to_owned(), json!("status"));
        object.insert("model".to_owned(), json!(self.model));
        object.insert("model_origin".to_owned(), json!(self.model_origin));
        object.insert("model_source".to_owned(), json!(self.model_source));
        if let Some(endpoint) = &self.provider_endpoint {
            object.insert("provider_endpoint".to_owned(), json!(endpoint));
        }
        match &self.mcp.profile_diagnostic {
            ProfileConfigDiagnostic::Clear => {}
            ProfileConfigDiagnostic::Failed(name) => {
                object.insert("mcp_config_error".to_owned(), json!(name));
            }
            ProfileConfigDiagnostic::Warning(warning) => {
                object.insert(
                    "mcp_config_warning".to_owned(),
                    json!({
                        "cause": warning.cause.as_str(),
                        "key": warning.key,
                        "additional_matches": warning.additional_matches,
                    }),
                );
            }
        }
        object.insert("auth".to_owned(), json!(self.auth.label()));
        let connected: Vec<&str> = self
            .auth
            .connected(self.connection.as_deref())
            .into_iter()
            .map(|(_, id)| id)
            .collect();
        object.insert("connected_providers".to_owned(), json!(connected));
        object.insert(
            "auth_refreshable".to_owned(),
            json!(self.auth.refreshable()),
        );
        if self.auth.expired() {
            object.insert("auth_expired".to_owned(), json!(true));
        }
        if let Some(help) = &self.auth.help {
            object.insert("auth_help".to_owned(), json!(help));
        }
        object.insert(
            "permission_mode".to_owned(),
            json!(self.permission_mode.label()),
        );
        object.insert(
            "workspace".to_owned(),
            json!(self.workspace_root.to_string_lossy()),
        );
        object.insert("history_turns".to_owned(), json!(0));
        object.insert("session_permission_grants".to_owned(), json!(0));
        object.insert("agent_step_limit".to_owned(), json!(self.agent_step_limit));
        object.insert("mcp".to_owned(), mcp_json(&self.mcp));
        object
    }
}

fn safe(raw: &str) -> String {
    encode_terminal_safe(raw.as_bytes(), usize::MAX).text
}

pub(crate) fn write_mcp_text(out: &mut String, prefix: &str, mcp: &LocalConfigInspection) {
    let _ = writeln!(out, "[{prefix}] mcp_connection_check={NOT_CHECKED}");
    let _ = writeln!(
        out,
        "[{prefix}] mcp_servers={} mcp_configuration_issues={}",
        mcp.servers.len(),
        mcp.configuration_issues.len()
    );
    for server in &mcp.servers {
        let _ = writeln!(
            out,
            "[{prefix}] mcp_server={} source={} scope={} admission={} transport={} connection={NOT_CHECKED} authentication={NOT_CHECKED}",
            encode_terminal_safe(server.name.as_bytes(), usize::MAX).text,
            server.source.as_str(),
            server.scope.as_str(),
            server
                .workspace_admission
                .map_or("not_applicable", WorkspaceAdmission::as_str),
            server.transport.as_str(),
        );
    }
    for issue in &mcp.configuration_issues {
        let _ = writeln!(
            out,
            "[{prefix}] mcp_configuration_issue={}",
            encode_terminal_safe(issue.as_bytes(), usize::MAX).text
        );
    }
    if let Some(error) = &mcp.inspection_error {
        let _ = writeln!(out, "[{prefix}] mcp_inspection_error={error}");
    }
}

pub(crate) fn mcp_json(mcp: &LocalConfigInspection) -> Value {
    let servers: Vec<Value> = mcp
        .servers
        .iter()
        .map(|server| {
            json!({
                "name": server.name,
                "source": server.source.as_str(),
                "scope": server.scope.as_str(),
                "admission": server.workspace_admission.map(WorkspaceAdmission::as_str),
                "required": server.required,
                "transport": server.transport.as_str(),
                "connection": NOT_CHECKED,
                "authentication": NOT_CHECKED,
            })
        })
        .collect();
    json!({
        "connection_check": NOT_CHECKED,
        "servers": servers,
        "configuration_issues": mcp.configuration_issues,
        "inspection_error": mcp.inspection_error,
    })
}

#[cfg(test)]
mod tests;
