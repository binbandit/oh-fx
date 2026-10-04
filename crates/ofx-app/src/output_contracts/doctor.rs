use std::fmt::Write as _;
use std::path::PathBuf;

use ofx_cli::OutputFormat;
use ofx_contract::PermissionMode;
use ofx_mcp::LocalConfigInspection;
use serde_json::{Map, Value, json};

use crate::output_contracts::status::{AuthStatus, mcp_json, safe, write_mcp_text};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

impl CheckStatus {
    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Check {
    pub(crate) name: &'static str,
    pub(crate) status: CheckStatus,
    pub(crate) detail: String,
}

pub(crate) struct DoctorReport {
    pub(crate) workspace_root: PathBuf,
    pub(crate) model: String,
    pub(crate) model_source: Option<String>,
    pub(crate) auth: AuthStatus,
    pub(crate) permission_mode: PermissionMode,
    pub(crate) agent_step_limit: u64,
    pub(crate) checks: Vec<Check>,
    pub(crate) mcp: LocalConfigInspection,
}

impl DoctorReport {
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

    fn count(&self, status: CheckStatus) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == status)
            .count()
    }

    fn render_text(&self) -> String {
        let mut out = format!(
            "[doctor] ok={} warn={} fail={}\n",
            self.count(CheckStatus::Ok),
            self.count(CheckStatus::Warn),
            self.count(CheckStatus::Fail)
        );
        let mut line = |text: std::fmt::Arguments<'_>| {
            let _ = writeln!(out, "[doctor] {text}");
        };
        line(format_args!(
            "workspace={}",
            safe(&self.workspace_root.to_string_lossy())
        ));
        line(format_args!("model={}", safe(&self.model)));
        if let Some(source) = &self.model_source {
            line(format_args!("model_source={source}"));
        }
        line(format_args!("auth={}", self.auth.label()));
        line(format_args!("auth_refreshable={}", self.auth.refreshable()));
        if self.auth.expired() {
            line(format_args!("auth_expired=true"));
        }
        line(format_args!(
            "permission_mode={}",
            self.permission_mode.display_label()
        ));
        line(format_args!("agent_step_limit={}", self.agent_step_limit));
        write_mcp_text(&mut out, "doctor", &self.mcp);
        for check in &self.checks {
            let _ = writeln!(
                out,
                "[{}] {}: {}",
                check.status.label(),
                check.name,
                safe(&check.detail)
            );
        }
        out
    }

    fn json(&self) -> Map<String, Value> {
        let mut object = Map::new();
        object.insert("kind".to_owned(), json!("doctor"));
        object.insert("ok_count".to_owned(), json!(self.count(CheckStatus::Ok)));
        object.insert(
            "warn_count".to_owned(),
            json!(self.count(CheckStatus::Warn)),
        );
        object.insert(
            "fail_count".to_owned(),
            json!(self.count(CheckStatus::Fail)),
        );
        object.insert(
            "workspace".to_owned(),
            json!(self.workspace_root.to_string_lossy()),
        );
        object.insert("model".to_owned(), json!(self.model));
        if let Some(source) = &self.model_source {
            object.insert("model_source".to_owned(), json!(source));
        }
        object.insert("auth".to_owned(), json!(self.auth.label()));
        object.insert(
            "auth_refreshable".to_owned(),
            json!(self.auth.refreshable()),
        );
        if self.auth.expired() {
            object.insert("auth_expired".to_owned(), json!(true));
        }
        object.insert(
            "permission_mode".to_owned(),
            json!(self.permission_mode.label()),
        );
        object.insert("agent_step_limit".to_owned(), json!(self.agent_step_limit));
        let checks: Vec<Value> = self
            .checks
            .iter()
            .map(|check| {
                json!({
                    "name": check.name,
                    "status": check.status.label(),
                    "detail": check.detail,
                })
            })
            .collect();
        object.insert("checks".to_owned(), json!(checks));
        object.insert("mcp".to_owned(), mcp_json(&self.mcp));
        object
    }
}

#[cfg(test)]
mod tests;
