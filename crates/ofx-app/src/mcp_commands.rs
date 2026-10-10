use std::fmt::Write as _;
use std::path::Path;

use ofx_config::{SettingsWriteError, WorkspaceSaveError, save_workspace_entry};
use ofx_contract::NoticeTone;
use ofx_mcp::{
    AddIntentError, McpError, McpRuntime, ProfileConfigWarning, ProjectMcpAction, ResourceSummary,
    add_profile_server, apply_project_mcp_action_to_entry, load_profile_document, parse_add_intent,
    remove_profile_server,
};
use ofx_text::encode_terminal_safe;

use crate::app_agent_runtime::ControllerState;
use crate::app_mcp_runtime::{McpHost, RECONNECTING, TOPIC};

const TRIMMED: [char; 2] = [' ', '\t'];
const USAGE: &str = "usage: /mcp [list|resource|prompt|add|remove|path|reload|auth|logout|trust]";
const ADD_USAGE: &str =
    "usage: /mcp add NAME COMMAND [ARGS...] | mcp add --transport http NAME URL";
const HOME_UNAVAILABLE: &str = "HOME is not available.";
const EVALUATING: &str = "Evaluating trusted profile MCP configuration.";
const TRUST_USAGE: &str = "usage: /mcp trust approve|reject <server> | approve-all | reset";
const TRUST_SERVER_USAGE: &str = "usage: /mcp trust approve|reject <server>";
const AUTH_USAGE: &str = "usage: /mcp auth <name> [--open]";
const LOGOUT_USAGE: &str = "usage: /mcp logout <name>";
const REMOVE_USAGE: &str = "usage: /mcp remove <name>";
const RESOURCE_USAGE: &str = "usage: /mcp resource [list|templates|read|complete] ...";
const RESOURCE_LIST_USAGE: &str =
    "usage: /mcp resource list <server> or /mcp resource templates <server>";
const RESOURCE_READ_USAGE: &str = "usage: /mcp resource read <server> <uri>";
const RESOURCE_COMPLETE_USAGE: &str =
    "usage: /mcp resource complete <server> <uri-template> <variable> [value]";
const PROMPT_USAGE: &str = "usage: /mcp prompt [list|get|complete] ...";
const PROMPT_LIST_USAGE: &str = "usage: /mcp prompt list <server>";
const PROMPT_GET_USAGE: &str = "usage: /mcp prompt get <server> <name> [arguments-json]";
const PROMPT_COMPLETE_USAGE: &str =
    "usage: /mcp prompt complete <server> <name> <argument> [value]";
const WARNING_KEY_BYTES: usize = 128;
const CHOICES_UNCERTAIN: &str = "Project MCP choices may have been saved, so live MCP authority was retired. Run /mcp reload after checking settings.json.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Show(String),
    Reload {
        body: String,
        report: bool,
    },
    Trust {
        body: String,
        action: ProjectMcpAction,
    },
    ListResources {
        server: String,
        templates: bool,
    },
}

pub(crate) fn respond(rest: &str, config_path: Option<&Path>, runtime: &McpRuntime) -> Outcome {
    let command = rest.trim_matches(TRIMMED);
    if command.is_empty() {
        return Outcome::Show(with_profile_warning(runtime.render_summary(), config_path));
    }
    if command == "list" {
        return Outcome::Show(with_profile_warning(runtime.render_health(), config_path));
    }
    if let Some(rest) = command.strip_prefix("resource ") {
        return resource(rest);
    }
    if let Some(rest) = command.strip_prefix("prompt ") {
        return Outcome::Show(prompt(rest).to_owned());
    }
    let Some(config_path) = config_path else {
        return show(HOME_UNAVAILABLE);
    };
    if command == "path" {
        return Outcome::Show(config_path.display().to_string());
    }
    if command == "reload" {
        return reload(config_path);
    }
    if let Some(rest) = command.strip_prefix("trust ") {
        return trust(rest);
    }
    if command == "auth" || command.starts_with("auth ") {
        return show(auth(&command["auth".len()..]));
    }
    if let Some(name) = command.strip_prefix("logout ") {
        let name = name.trim_matches(TRIMMED);
        if name.is_empty() || name.contains(TRIMMED) {
            return show(LOGOUT_USAGE);
        }
        return show("MCP logout is not available yet.");
    }
    if let Some(name) = command.strip_prefix("remove ") {
        return remove(config_path, name.trim_matches(TRIMMED));
    }
    if let Some(rest) = command.strip_prefix("add ") {
        return add(config_path, rest);
    }
    show(USAGE)
}

pub(crate) fn handle_mcp(state: &ControllerState, rest: &str) {
    let Some(host) = state.mcp() else {
        return;
    };
    let config_path = host.sources().profile_path();
    match respond(rest, config_path.as_deref(), host.runtime()) {
        Outcome::Show(body) => state.notice(NoticeTone::Neutral, topic(&body), &body),
        Outcome::Reload { body, report } => {
            host.begin_reload();
            let body = if report {
                RECONNECTING.to_owned()
            } else {
                format!("{body}\n{RECONNECTING}")
            };
            state.notice(NoticeTone::Neutral, TOPIC, &body);
        }
        Outcome::Trust { body, action } => apply_project_action(state, host, &action, &body),
        Outcome::ListResources { server, templates } => host.list_resources(server, templates),
    }
}

pub(crate) fn render_resource_listing(
    server: &str,
    templates: bool,
    listing: Result<Vec<ResourceSummary>, McpError>,
) -> String {
    let items = match listing {
        Ok(items) => items,
        Err(error) => return format!("MCP resource listing failed: {error}."),
    };
    let mut out = format!(
        "MCP {} from {server} ({}):\n",
        if templates {
            "resource templates"
        } else {
            "resources"
        },
        items.len()
    );
    for item in &items {
        let _ = writeln!(
            out,
            "  {server} :: {} — {}",
            item.identity,
            item.title.as_deref().unwrap_or(&item.name)
        );
    }
    out
}

fn apply_project_action(
    state: &ControllerState,
    host: &McpHost,
    action: &ProjectMcpAction,
    success: &str,
) {
    let Some(paths) = host.sources().paths() else {
        state.notice(NoticeTone::Neutral, TOPIC, HOME_UNAVAILABLE);
        return;
    };
    let reducing = matches!(
        action,
        ProjectMcpAction::Reject(_) | ProjectMcpAction::Reset
    );
    let saved = save_workspace_entry(paths, host.sources().workspace_root(), |entry| {
        apply_project_mcp_action_to_entry(entry, action).map(|change| change.changed)
    });
    match saved {
        Ok(committed) => {
            if reducing {
                host.begin_authority_reduction(true);
            } else if committed {
                host.begin_reload();
            }
            state.notice(NoticeTone::Neutral, TOPIC, success);
        }
        Err(WorkspaceSaveError::Settings(failure))
            if reducing && failure.error == SettingsWriteError::CommitIndeterminate =>
        {
            host.begin_authority_reduction(false);
            state.notice(NoticeTone::Warning, TOPIC, CHOICES_UNCERTAIN);
        }
        Err(error) => {
            let name = match error {
                WorkspaceSaveError::Edit(error) => error.to_string(),
                WorkspaceSaveError::Settings(failure) => failure.error.to_string(),
            };
            state.notice(
                NoticeTone::Warning,
                TOPIC,
                &format!("Project MCP choices were not applied: {name}."),
            );
        }
    }
}

fn topic(body: &str) -> &'static str {
    if body.starts_with("usage:") {
        ""
    } else {
        TOPIC
    }
}

fn show(text: &str) -> Outcome {
    Outcome::Show(text.to_owned())
}

fn reload(config_path: &Path) -> Outcome {
    let warning = load_profile_document(config_path)
        .ok()
        .and_then(|document| document.diagnostic);
    let body = match warning {
        Some(warning) => format!("{}\n{EVALUATING}", render_profile_warning(&warning)),
        None => EVALUATING.to_owned(),
    };
    Outcome::Reload { body, report: true }
}

fn trust(rest: &str) -> Outcome {
    let mut tokens = rest.split(TRIMMED).filter(|token| !token.is_empty());
    let Some(operation) = tokens.next() else {
        return show(TRUST_USAGE);
    };
    let (body, action) = match operation {
        "approve-all" | "reset" => {
            if tokens.next().is_some() {
                return show(if operation == "reset" {
                    "usage: /mcp trust reset"
                } else {
                    "usage: /mcp trust approve-all"
                });
            }
            if operation == "reset" {
                (
                    "Resetting project MCP choices for this workspace.".to_owned(),
                    ProjectMcpAction::Reset,
                )
            } else {
                (
                    "Approving all project MCP servers for this workspace.".to_owned(),
                    ProjectMcpAction::ApproveAll,
                )
            }
        }
        _ => {
            let (Some(name), None) = (tokens.next(), tokens.next()) else {
                return show(TRUST_SERVER_USAGE);
            };
            match operation {
                "approve" => (
                    format!("Approving project MCP server '{name}'."),
                    ProjectMcpAction::Approve(name.to_owned()),
                ),
                "reject" => (
                    format!("Rejecting project MCP server '{name}'."),
                    ProjectMcpAction::Reject(name.to_owned()),
                ),
                _ => return show(TRUST_USAGE),
            }
        }
    };
    Outcome::Trust { body, action }
}

fn auth(rest: &str) -> &'static str {
    let mut tokens = rest.split(TRIMMED).filter(|token| !token.is_empty());
    let name = tokens.next();
    let confirmation = tokens.next();
    if name.is_none()
        || tokens.next().is_some()
        || confirmation.is_some_and(|value| value != "--open")
    {
        return AUTH_USAGE;
    }
    "Interactive MCP authentication is not available yet."
}

fn remove(config_path: &Path, name: &str) -> Outcome {
    if name.is_empty() {
        return show(REMOVE_USAGE);
    }
    match remove_profile_server(config_path, name) {
        Err(error) => Outcome::Show(format!("Failed to remove MCP server '{name}': {error}.")),
        Ok(outcome) if !outcome.removed => Outcome::Show(format!("MCP server '{name}' not found.")),
        Ok(_) => Outcome::Reload {
            body: format!("Removed MCP server '{name}'."),
            report: false,
        },
    }
}

fn add(config_path: &Path, rest: &str) -> Outcome {
    let tokens: Vec<&str> = rest
        .split(TRIMMED)
        .filter(|token| !token.is_empty())
        .collect();
    let intent = match parse_add_intent(&tokens) {
        Ok(intent) => intent,
        Err(AddIntentError::McpAddUsage) => return show(ADD_USAGE),
        Err(error) => {
            return Outcome::Show(format!("Failed to save MCP server config: {error}."));
        }
    };
    let name = intent.name().to_owned();
    match add_profile_server(config_path, intent) {
        Err(error) => Outcome::Show(format!("Failed to save MCP server config: {error}.")),
        Ok(warning) => {
            let saved = format!("Saved MCP server '{name}'.");
            let body = match warning {
                Some(warning) => format!("{}\n{saved}", render_profile_warning(&warning)),
                None => saved,
            };
            Outcome::Reload {
                body,
                report: false,
            }
        }
    }
}

fn resource(rest: &str) -> Outcome {
    let mut tokens = rest.split(TRIMMED).filter(|token| !token.is_empty());
    match tokens.next() {
        Some(action @ ("list" | "templates")) => match (tokens.next(), tokens.next()) {
            (Some(server), None) => Outcome::ListResources {
                server: server.to_owned(),
                templates: action == "templates",
            },
            _ => show(RESOURCE_LIST_USAGE),
        },
        Some("read") => show(match (tokens.next(), tokens.next()) {
            (Some(_), Some(_)) => "MCP resource reads are not available yet.",
            _ => RESOURCE_READ_USAGE,
        }),
        Some("complete") => show(match (tokens.next(), tokens.next(), tokens.next()) {
            (Some(_), Some(_), Some(_)) => "MCP resource completion is not available yet.",
            _ => RESOURCE_COMPLETE_USAGE,
        }),
        _ => show(RESOURCE_USAGE),
    }
}

fn prompt(rest: &str) -> &'static str {
    let mut tokens = rest.split(TRIMMED).filter(|token| !token.is_empty());
    match tokens.next() {
        Some("list") => match (tokens.next(), tokens.next()) {
            (Some(_), None) => "MCP prompts are not available yet.",
            _ => PROMPT_LIST_USAGE,
        },
        Some("get") => match (tokens.next(), tokens.next()) {
            (Some(_), Some(_)) => "MCP prompt invocation is not available yet.",
            _ => PROMPT_GET_USAGE,
        },
        Some("complete") => match (tokens.next(), tokens.next(), tokens.next()) {
            (Some(_), Some(_), Some(_)) => "MCP prompt completion is not available yet.",
            _ => PROMPT_COMPLETE_USAGE,
        },
        _ => PROMPT_USAGE,
    }
}

fn with_profile_warning(body: String, config_path: Option<&Path>) -> String {
    let warning = config_path
        .and_then(|path| load_profile_document(path).ok())
        .and_then(|document| document.diagnostic);
    let Some(warning) = warning else {
        return body;
    };
    let mut text = body;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&render_profile_warning(&warning));
    text.push('\n');
    text
}

fn render_profile_warning(warning: &ProfileConfigWarning) -> String {
    let key = warning.key.as_ref().map_or_else(String::new, |key| {
        format!(
            " key={}",
            encode_terminal_safe(key.as_bytes(), WARNING_KEY_BYTES).text
        )
    });
    format!(
        "MCP config warning: {}{key} additional_matches={}",
        warning.cause.as_str(),
        warning.additional_matches
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use ofx_config::{ContextLimitName, ContextLimits};
    use ofx_mcp::{ConnectOptions, NativeConfigLoad, SchemaLimits};

    use super::*;

    fn runtime() -> McpRuntime {
        let limits = ContextLimits::default();
        McpRuntime::new(
            NativeConfigLoad::default(),
            ConnectOptions::default(),
            Vec::new(),
            SchemaLimits {
                server_instructions: limits.get(ContextLimitName::McpServerInstructionsBytes),
                selected_schema: limits.get(ContextLimitName::McpSelectedSchemaBytes),
            },
        )
    }

    fn shown(rest: &str, config_path: Option<&Path>) -> String {
        match respond(rest, config_path, &runtime()) {
            Outcome::Show(text) => text,
            other => panic!("expected text for {rest:?}, got {other:?}"),
        }
    }

    fn profile() -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("mcp.json");
        (home, path)
    }

    #[test]
    fn the_summary_and_the_listing_describe_the_configured_servers() {
        assert_eq!(
            shown("", None),
            "MCP: no servers configured. Use /mcp add <name> <command> [args...]."
        );
        assert_eq!(shown(" \t", None), shown("", None));
        assert_eq!(shown("list", None), "No MCP servers configured.\n");
    }

    #[test]
    fn a_profile_warning_follows_the_summary_and_the_listing() {
        let (_home, path) = profile();
        fs::write(&path, r#"{"mcp":{},"mcpServers":{"old":{"command":"x"}}}"#).unwrap();
        let warning =
            "MCP config warning: ignored_mcp_servers_alias key=mcpServers additional_matches=0";
        assert_eq!(
            shown("", Some(&path)),
            format!(
                "MCP: no servers configured. Use /mcp add <name> <command> [args...].\n{warning}\n"
            )
        );
        assert_eq!(
            shown("list", Some(&path)),
            format!("No MCP servers configured.\n{warning}\n")
        );
        assert_eq!(
            respond("reload", Some(&path), &runtime()),
            Outcome::Reload {
                body: format!("{warning}\n{EVALUATING}"),
                report: true
            }
        );
    }

    #[test]
    fn commands_that_need_a_home_say_when_it_is_unavailable() {
        for command in [
            "path",
            "reload",
            "trust approve docs",
            "add docs node",
            "remove docs",
            "auth docs",
            "logout docs",
        ] {
            assert_eq!(shown(command, None), HOME_UNAVAILABLE, "{command}");
        }
        assert_eq!(shown("resource list", None), RESOURCE_LIST_USAGE);
    }

    #[test]
    fn path_and_reload_name_the_profile_configuration() {
        let (_home, path) = profile();
        assert_eq!(shown("path", Some(&path)), path.display().to_string());
        assert_eq!(
            respond("reload", Some(&path), &runtime()),
            Outcome::Reload {
                body: EVALUATING.to_owned(),
                report: true
            }
        );
    }

    #[test]
    fn trust_commands_name_the_project_choice_they_save() {
        let (_home, path) = profile();
        let trust = |command: &str| respond(command, Some(&path), &runtime());
        assert_eq!(
            trust("trust approve docs"),
            Outcome::Trust {
                body: "Approving project MCP server 'docs'.".to_owned(),
                action: ProjectMcpAction::Approve("docs".to_owned())
            }
        );
        assert_eq!(
            trust("trust  reject\tdocs"),
            Outcome::Trust {
                body: "Rejecting project MCP server 'docs'.".to_owned(),
                action: ProjectMcpAction::Reject("docs".to_owned())
            }
        );
        assert_eq!(
            trust("trust approve-all"),
            Outcome::Trust {
                body: "Approving all project MCP servers for this workspace.".to_owned(),
                action: ProjectMcpAction::ApproveAll
            }
        );
        assert_eq!(
            trust("trust reset"),
            Outcome::Trust {
                body: "Resetting project MCP choices for this workspace.".to_owned(),
                action: ProjectMcpAction::Reset
            }
        );
        for (command, usage) in [
            ("trust approve", TRUST_SERVER_USAGE),
            ("trust approve docs db", TRUST_SERVER_USAGE),
            ("trust maybe docs", TRUST_USAGE),
            ("trust reset now", "usage: /mcp trust reset"),
            ("trust approve-all now", "usage: /mcp trust approve-all"),
            ("trust", USAGE),
        ] {
            assert_eq!(trust(command), Outcome::Show(usage.to_owned()), "{command}");
        }
    }

    #[test]
    fn add_and_remove_change_the_profile_and_ask_for_a_reload() {
        let (_home, path) = profile();
        assert_eq!(
            respond("add docs node server.js", Some(&path), &runtime()),
            Outcome::Reload {
                body: "Saved MCP server 'docs'.".to_owned(),
                report: false
            }
        );
        assert!(fs::read_to_string(&path).unwrap().contains(r#""docs""#));
        assert_eq!(
            respond(
                "add --transport http remote https://example.com/mcp",
                Some(&path),
                &runtime()
            ),
            Outcome::Reload {
                body: "Saved MCP server 'remote'.".to_owned(),
                report: false
            }
        );
        assert_eq!(shown("add docs", Some(&path)), ADD_USAGE);
        assert_eq!(shown("add slack", Some(&path)), ADD_USAGE);
        assert_eq!(
            shown("add bad/name node", Some(&path)),
            "Failed to save MCP server config: McpInvalidServerName."
        );
        assert_eq!(
            respond("remove docs", Some(&path), &runtime()),
            Outcome::Reload {
                body: "Removed MCP server 'docs'.".to_owned(),
                report: false
            }
        );
        assert_eq!(
            shown("remove docs", Some(&path)),
            "MCP server 'docs' not found."
        );
        assert_eq!(shown("add", Some(&path)), USAGE);
        assert_eq!(shown("remove", Some(&path)), USAGE);
    }

    #[test]
    fn unported_features_check_their_arguments_then_say_they_are_not_available_yet() {
        let (_home, path) = profile();
        let path = Some(path.as_path());
        for (command, expected) in [
            ("auth", AUTH_USAGE),
            ("auth docs extra", AUTH_USAGE),
            ("auth docs --close", AUTH_USAGE),
            (
                "auth docs",
                "Interactive MCP authentication is not available yet.",
            ),
            (
                "auth docs --open",
                "Interactive MCP authentication is not available yet.",
            ),
            ("logout docs db", LOGOUT_USAGE),
            ("logout docs", "MCP logout is not available yet."),
            ("logout", USAGE),
            ("resource wat", RESOURCE_USAGE),
            ("resource list", RESOURCE_LIST_USAGE),
            ("resource templates docs extra", RESOURCE_LIST_USAGE),
            ("resource read docs", RESOURCE_READ_USAGE),
            (
                "resource read docs file:///a b",
                "MCP resource reads are not available yet.",
            ),
            ("resource complete docs uri", RESOURCE_COMPLETE_USAGE),
            (
                "resource complete docs uri name",
                "MCP resource completion is not available yet.",
            ),
            ("prompt", USAGE),
            ("prompt wat", PROMPT_USAGE),
            ("prompt list", PROMPT_LIST_USAGE),
            ("prompt list docs", "MCP prompts are not available yet."),
            ("prompt get docs", PROMPT_GET_USAGE),
            (
                "prompt get docs review {}",
                "MCP prompt invocation is not available yet.",
            ),
            ("prompt complete docs review", PROMPT_COMPLETE_USAGE),
            (
                "prompt complete docs review topic x",
                "MCP prompt completion is not available yet.",
            ),
            ("wat", USAGE),
        ] {
            assert_eq!(shown(command, path), expected, "{command}");
        }
    }

    #[test]
    fn resource_listings_name_the_server_and_each_identity() {
        let (_home, path) = profile();
        for (command, templates) in [
            ("resource list docs", false),
            ("resource  templates \tdocs ", true),
        ] {
            assert_eq!(
                respond(command, Some(&path), &runtime()),
                Outcome::ListResources {
                    server: "docs".to_owned(),
                    templates
                },
                "{command}"
            );
        }
        assert_eq!(
            respond("resource list docs", None, &runtime()),
            Outcome::ListResources {
                server: "docs".to_owned(),
                templates: false
            }
        );
        let item = |identity: &str, name: &str, title: Option<&str>| ResourceSummary {
            identity: identity.to_owned(),
            name: name.to_owned(),
            title: title.map(str::to_owned),
        };
        assert_eq!(
            render_resource_listing(
                "docs",
                false,
                Ok(vec![
                    item("memory://a", "a", Some("Alpha")),
                    item("memory://b", "b", None)
                ])
            ),
            "MCP resources from docs (2):\n  docs :: memory://a — Alpha\n  docs :: memory://b — b\n"
        );
        assert_eq!(
            render_resource_listing("docs", true, Ok(vec![item("memory://{id}", "row", None)])),
            "MCP resource templates from docs (1):\n  docs :: memory://{id} — row\n"
        );
        assert_eq!(
            render_resource_listing("docs", true, Ok(Vec::new())),
            "MCP resource templates from docs (0):\n"
        );
        assert_eq!(
            render_resource_listing("docs", false, Err(McpError::McpResourcesUnsupported)),
            "MCP resource listing failed: McpResourcesUnsupported."
        );
    }

    #[test]
    fn usage_replies_carry_no_topic() {
        assert_eq!(topic(USAGE), "");
        assert_eq!(topic(HOME_UNAVAILABLE), "mcp");
    }
}
