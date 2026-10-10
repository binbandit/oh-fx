use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use ofx_config::{SettingsWriteError, WorkspaceSaveError, save_workspace_entry};
use ofx_contract::NoticeTone;
use ofx_mcp::{
    AddIntentError, CompletionArgument, CompletionResult, FeatureFailure, McpError, McpRuntime,
    ProfileConfigWarning, ProjectMcpAction, PromptGetResult, PromptSummary, ResourceContent,
    ResourceData, ResourceSummary, add_profile_server, apply_project_mcp_action_to_entry,
    load_profile_document, parse_add_intent, remove_profile_server,
};
use ofx_text::{encode_terminal_safe, mask_secrets};

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
    ReadResource {
        server: String,
        uri: String,
    },
    ListPrompts {
        server: String,
    },
    GetPrompt {
        server: String,
        name: String,
        arguments: String,
    },
    CompletePrompt(Completion),
    CompleteResource(Completion),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Completion {
    pub(crate) server: String,
    pub(crate) target: String,
    pub(crate) argument: String,
    pub(crate) value: String,
}

impl Completion {
    pub(crate) fn argument(&self) -> CompletionArgument<'_> {
        CompletionArgument {
            name: &self.argument,
            value: &self.value,
        }
    }
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
        return prompt(rest);
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
        Outcome::ReadResource { server, uri } => host.read_resource(server, uri),
        Outcome::ListPrompts { server } => host.list_prompts(server),
        Outcome::GetPrompt {
            server,
            name,
            arguments,
        } => host.get_prompt(server, name, arguments),
        Outcome::CompletePrompt(completion) => host.complete_prompt(completion),
        Outcome::CompleteResource(completion) => host.complete_resource(completion),
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

pub(crate) fn render_resource_read(
    server: &str,
    uri: &str,
    read: Result<Arc<[ResourceContent]>, FeatureFailure>,
) -> String {
    let contents = match read {
        Ok(contents) => contents,
        Err(failure) => return render_failure("MCP resource read failed", failure),
    };
    let mut out = format!("[untrusted MCP resource content] {server} :: {uri}\n");
    for content in contents.iter() {
        let _ = write!(out, "\n{}", content.uri);
        if let Some(mime_type) = &content.mime_type {
            let _ = write!(out, " ({mime_type})");
        }
        out.push('\n');
        match &content.data {
            ResourceData::Text(text) => out.push_str(text),
            ResourceData::Blob(blob) => {
                let _ = write!(out, "<base64 blob: {} bytes encoded>", blob.len());
            }
        }
        out.push('\n');
    }
    out
}

pub(crate) fn render_prompt_get(
    server: &str,
    name: &str,
    result: Result<PromptGetResult, FeatureFailure>,
) -> String {
    let result = match result {
        Ok(result) => result,
        Err(failure) => return render_failure("MCP prompt invocation failed", failure),
    };
    let mut out = format!("[untrusted MCP prompt content] {server} :: {name}\n");
    if let Some(description) = &result.description {
        let _ = writeln!(out, "{description}");
    }
    for message in &result.messages {
        let _ = write!(
            out,
            "\n{} ({}):\n{}\n",
            message.role.as_str(),
            message.content_kind.as_str(),
            message.content_json
        );
    }
    out
}

pub(crate) fn render_completions(
    server: &str,
    failure: &str,
    result: Result<CompletionResult, McpError>,
) -> String {
    let result = match result {
        Ok(result) => result,
        Err(error) => return format!("{failure}: {error}."),
    };
    let mut out = format!("MCP completions from {server} ({}", result.values.len());
    if let Some(total) = result.total {
        let _ = write!(out, " of {total}");
    }
    out.push_str("):\n");
    for value in &result.values {
        let _ = writeln!(out, "  {value}");
    }
    if result.has_more == Some(true) {
        out.push_str("  \u{2026} more available\n");
    }
    out
}

fn render_failure(prefix: &str, failure: FeatureFailure) -> String {
    match failure {
        FeatureFailure::Error(error) => format!("{prefix}: {error}."),
        FeatureFailure::Diagnostic(diagnostic) => mask_secrets(&diagnostic).into_owned(),
    }
}

pub(crate) fn render_prompt_listing(
    server: &str,
    listing: Result<Vec<PromptSummary>, McpError>,
) -> String {
    let items = match listing {
        Ok(items) => items,
        Err(error) => return format!("MCP prompt listing failed: {error}."),
    };
    let mut out = format!("MCP prompts from {server} ({}):\n", items.len());
    for item in &items {
        let _ = write!(out, "  {server} :: {}", item.name);
        if let Some(label) = item.title.as_ref().or(item.description.as_ref()) {
            let _ = write!(out, " — {label}");
        }
        if !item.arguments.is_empty() {
            out.push_str(" [");
            for (index, argument) in item.arguments.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push_str(&argument.name);
                if argument.required {
                    out.push('*');
                }
            }
            out.push(']');
        }
        out.push('\n');
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
        Some("read") => read_resource(rest),
        Some("complete") => completion(rest)
            .map_or_else(|| show(RESOURCE_COMPLETE_USAGE), Outcome::CompleteResource),
        _ => show(RESOURCE_USAGE),
    }
}

fn read_resource(rest: &str) -> Outcome {
    let mut input = rest;
    take_token(&mut input);
    let Some(server) = take_token(&mut input) else {
        return show(RESOURCE_READ_USAGE);
    };
    let uri = input.trim_matches(TRIMMED);
    if uri.is_empty() {
        return show(RESOURCE_READ_USAGE);
    }
    Outcome::ReadResource {
        server: server.to_owned(),
        uri: uri.to_owned(),
    }
}

fn take_token<'a>(input: &mut &'a str) -> Option<&'a str> {
    let trimmed = input.trim_start_matches(TRIMMED);
    if trimmed.is_empty() {
        *input = trimmed;
        return None;
    }
    let end = trimmed.find(TRIMMED).unwrap_or(trimmed.len());
    *input = &trimmed[end..];
    Some(&trimmed[..end])
}

fn prompt(rest: &str) -> Outcome {
    let mut tokens = rest.split(TRIMMED).filter(|token| !token.is_empty());
    match tokens.next() {
        Some("list") => match (tokens.next(), tokens.next()) {
            (Some(server), None) => Outcome::ListPrompts {
                server: server.to_owned(),
            },
            _ => show(PROMPT_LIST_USAGE),
        },
        Some("get") => get_prompt(rest),
        Some("complete") => {
            completion(rest).map_or_else(|| show(PROMPT_COMPLETE_USAGE), Outcome::CompletePrompt)
        }
        _ => show(PROMPT_USAGE),
    }
}

fn get_prompt(rest: &str) -> Outcome {
    let mut input = rest;
    take_token(&mut input);
    let (Some(server), Some(name)) = (take_token(&mut input), take_token(&mut input)) else {
        return show(PROMPT_GET_USAGE);
    };
    let arguments = match input.trim_matches(TRIMMED) {
        "" => "{}",
        arguments => arguments,
    };
    Outcome::GetPrompt {
        server: server.to_owned(),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

fn completion(rest: &str) -> Option<Completion> {
    let mut input = rest;
    take_token(&mut input);
    let server = take_token(&mut input)?;
    let target = take_token(&mut input)?;
    let argument = take_token(&mut input)?;
    Some(Completion {
        server: server.to_owned(),
        target: target.to_owned(),
        argument: argument.to_owned(),
        value: input.trim_matches(TRIMMED).to_owned(),
    })
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
    use ofx_mcp::{
        ConnectOptions, NativeConfigLoad, PromptArgument, PromptContentKind, PromptMessage,
        PromptRole, SchemaLimits,
    };

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
            ("resource read", RESOURCE_READ_USAGE),
            ("resource read docs \t ", RESOURCE_READ_USAGE),
            ("resource complete docs uri", RESOURCE_COMPLETE_USAGE),
            ("resource complete", RESOURCE_COMPLETE_USAGE),
            ("prompt", USAGE),
            ("prompt wat", PROMPT_USAGE),
            ("prompt list", PROMPT_LIST_USAGE),
            ("prompt list docs extra", PROMPT_LIST_USAGE),
            ("prompt get docs", PROMPT_GET_USAGE),
            ("prompt get", PROMPT_GET_USAGE),
            ("prompt get \t docs \t", PROMPT_GET_USAGE),
            ("prompt complete docs review", PROMPT_COMPLETE_USAGE),
            ("prompt complete docs \t", PROMPT_COMPLETE_USAGE),
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
            description: None,
            mime_type: None,
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
    fn resource_reads_keep_the_rest_of_the_line_as_the_uri_and_show_each_content() {
        let (_home, path) = profile();
        for (command, uri) in [
            ("resource read docs file:///a b", "file:///a b"),
            ("resource \tread  docs   memory://x \t", "memory://x"),
        ] {
            assert_eq!(
                respond(command, Some(&path), &runtime()),
                Outcome::ReadResource {
                    server: "docs".to_owned(),
                    uri: uri.to_owned()
                },
                "{command}"
            );
        }
        let content = |uri: &str, mime_type: Option<&str>, data: ResourceData| ResourceContent {
            uri: uri.to_owned(),
            mime_type: mime_type.map(str::to_owned),
            annotations_json: None,
            metadata_json: None,
            data,
        };
        assert_eq!(
            render_resource_read(
                "docs",
                "memory://notes",
                Ok(Arc::from(vec![
                    content(
                        "memory://notes",
                        Some("text/plain"),
                        ResourceData::Text("hello".to_owned())
                    ),
                    content(
                        "memory://image",
                        None,
                        ResourceData::Blob("aGVsbG8=".to_owned())
                    ),
                ]))
            ),
            "[untrusted MCP resource content] docs :: memory://notes\n\nmemory://notes (text/plain)\nhello\n\nmemory://image\n<base64 blob: 8 bytes encoded>\n"
        );
        assert_eq!(
            render_resource_read("docs", "memory://x", Ok(Arc::from(Vec::new()))),
            "[untrusted MCP resource content] docs :: memory://x\n"
        );
        assert_eq!(
            render_resource_read(
                "docs",
                "memory://x",
                Err(FeatureFailure::Error(McpError::McpResourceNotFound))
            ),
            "MCP resource read failed: McpResourceNotFound."
        );
        assert_eq!(
            render_resource_read(
                "docs",
                "memory://x",
                Err(FeatureFailure::Diagnostic(
                    "MCP protocol error -32002: denied; data={\"token\":\"sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789\"}".to_owned()
                ))
            ),
            "MCP protocol error -32002: denied; data={\"token\":\"[redacted]\"}"
        );
    }

    #[test]
    fn prompt_listings_label_each_prompt_and_mark_required_arguments() {
        let (_home, path) = profile();
        for config_path in [Some(path.as_path()), None] {
            assert_eq!(
                respond(" prompt \tlist  docs ", config_path, &runtime()),
                Outcome::ListPrompts {
                    server: "docs".to_owned()
                }
            );
        }
        let argument = |name: &str, required: bool| PromptArgument {
            name: name.to_owned(),
            description: None,
            required,
        };
        let item = |name: &str, title: Option<&str>, description: Option<&str>| PromptSummary {
            name: name.to_owned(),
            title: title.map(str::to_owned),
            description: description.map(str::to_owned),
            arguments: Vec::new(),
        };
        assert_eq!(
            render_prompt_listing(
                "docs",
                Ok(vec![
                    item("explain", None, None),
                    PromptSummary {
                        arguments: vec![argument("focus", true), argument("depth", false)],
                        ..item("review", Some("Review"), Some("Review code"))
                    },
                    PromptSummary {
                        arguments: vec![argument("topic", false)],
                        ..item("summarize", None, Some("Summarize a topic"))
                    },
                ])
            ),
            "MCP prompts from docs (3):\n  docs :: explain\n  docs :: review — Review [focus*, depth]\n  docs :: summarize — Summarize a topic [topic]\n"
        );
        assert_eq!(
            render_prompt_listing("docs", Ok(Vec::new())),
            "MCP prompts from docs (0):\n"
        );
        assert_eq!(
            render_prompt_listing("docs", Err(McpError::McpPromptsUnsupported)),
            "MCP prompt listing failed: McpPromptsUnsupported."
        );
    }

    #[test]
    fn prompt_gets_take_the_rest_of_the_line_as_arguments_and_show_each_message() {
        let (_home, path) = profile();
        for (command, arguments) in [
            (
                "prompt get server-b shared {\"tone\":\"very brief\"}",
                "{\"tone\":\"very brief\"}",
            ),
            ("prompt \tget  server-b   shared \t", "{}"),
            ("prompt get server-b shared  {} \t", "{}"),
            ("prompt get server-b shared not json", "not json"),
        ] {
            for config_path in [Some(path.as_path()), None] {
                assert_eq!(
                    respond(command, config_path, &runtime()),
                    Outcome::GetPrompt {
                        server: "server-b".to_owned(),
                        name: "shared".to_owned(),
                        arguments: arguments.to_owned(),
                    },
                    "{command}"
                );
            }
        }
        let message = |role, content_kind, content_json: &str| PromptMessage {
            role,
            content_kind,
            content_json: content_json.to_owned(),
        };
        assert_eq!(
            render_prompt_get(
                "docs",
                "review",
                Ok(PromptGetResult {
                    description: Some("Review code".to_owned()),
                    messages: vec![
                        message(
                            PromptRole::User,
                            PromptContentKind::Text,
                            r#"{"type":"text","text":"hello"}"#
                        ),
                        message(
                            PromptRole::Assistant,
                            PromptContentKind::ResourceLink,
                            r#"{"type":"resource_link","uri":"git://repo","name":"repo"}"#
                        ),
                    ],
                })
            ),
            "[untrusted MCP prompt content] docs :: review\nReview code\n\nuser (text):\n{\"type\":\"text\",\"text\":\"hello\"}\n\nassistant (resource_link):\n{\"type\":\"resource_link\",\"uri\":\"git://repo\",\"name\":\"repo\"}\n"
        );
        assert_eq!(
            render_prompt_get(
                "docs",
                "empty",
                Ok(PromptGetResult {
                    description: None,
                    messages: Vec::new(),
                })
            ),
            "[untrusted MCP prompt content] docs :: empty\n"
        );
        for (kind, name) in [
            (PromptContentKind::Image, "image"),
            (PromptContentKind::Audio, "audio"),
            (PromptContentKind::Resource, "resource"),
        ] {
            assert_eq!(kind.as_str(), name);
        }
        assert_eq!(
            render_prompt_get(
                "docs",
                "review",
                Err(FeatureFailure::Error(McpError::InvalidArguments))
            ),
            "MCP prompt invocation failed: InvalidArguments."
        );
        assert_eq!(
            render_prompt_get(
                "docs",
                "review",
                Err(FeatureFailure::Diagnostic(
                    "MCP protocol error -32603: rejected SERVICE_TOKEN=fixture-token-123456"
                        .to_owned()
                ))
            ),
            "MCP protocol error -32603: rejected SERVICE_TOKEN=[redacted]"
        );
    }

    #[test]
    fn completions_take_three_tokens_and_the_rest_of_the_line_as_the_value() {
        let (_home, path) = profile();
        let completion = |target: &str, argument: &str, value: &str| Completion {
            server: "server-b".to_owned(),
            target: target.to_owned(),
            argument: argument.to_owned(),
            value: value.to_owned(),
        };
        for config_path in [Some(path.as_path()), None] {
            let respond = |command| respond(command, config_path, &runtime());
            assert_eq!(
                respond("prompt complete server-b shared tone very brief"),
                Outcome::CompletePrompt(completion("shared", "tone", "very brief"))
            );
            assert_eq!(
                respond("prompt \tcomplete server-b  shared\ttone \t"),
                Outcome::CompletePrompt(completion("shared", "tone", ""))
            );
            assert_eq!(
                respond("resource complete server-b custom://project/{path} path  src/ \t"),
                Outcome::CompleteResource(completion("custom://project/{path}", "path", "src/"))
            );
        }
        assert_eq!(
            completion("shared", "tone", "b").argument(),
            CompletionArgument {
                name: "tone",
                value: "b"
            }
        );
        let result = |values: &[&str], total, has_more| {
            Ok(CompletionResult {
                values: values.iter().map(|value| (*value).to_owned()).collect(),
                total,
                has_more,
            })
        };
        let failure = "MCP prompt completion failed";
        assert_eq!(
            render_completions(
                "docs",
                failure,
                result(&["balpha", "bbeta"], Some(5), Some(true))
            ),
            "MCP completions from docs (2 of 5):\n  balpha\n  bbeta\n  \u{2026} more available\n"
        );
        assert_eq!(
            render_completions("docs", failure, result(&["src/alpha"], None, Some(false))),
            "MCP completions from docs (1):\n  src/alpha\n"
        );
        assert_eq!(
            render_completions("docs", failure, result(&[], None, None)),
            "MCP completions from docs (0):\n"
        );
        assert_eq!(
            render_completions(
                "docs",
                "MCP resource completion failed",
                Err(McpError::McpResourceTemplateNotFound)
            ),
            "MCP resource completion failed: McpResourceTemplateNotFound."
        );
    }

    #[test]
    fn usage_replies_carry_no_topic() {
        assert_eq!(topic(USAGE), "");
        assert_eq!(topic(HOME_UNAVAILABLE), "mcp");
    }
}
