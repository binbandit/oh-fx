use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};

use ofx_config::ContextLimit;
use ofx_contract::McpSearchResult;
use ofx_text::{
    Document, Domain, PreparedQuery, Request, is_model_safe_text, retrieve, write_scalar,
};
use serde_json::Value;

use crate::features::tools::{Tool, ToolCatalog};
use crate::server_lifecycle::{Lifecycle, Server};
use crate::tool_mcp_registry::{Projection, SchemaLimits, project, selected_schema};
use crate::tool_names::{ToolNames, is_identifier_byte, tags_for};

const DESCRIPTION_SEARCH_BYTES: usize = 2 * 1024;
const SCHEMA_SEARCH_BYTES: usize = 4 * 1024;
const INSTRUCTIONS_SEARCH_BYTES: usize = 2 * 1024;
const DESCRIPTION_LIMIT: &str = "mcp_description_bytes";
const RESULT_LIMIT: &str = "mcp_search_result_bytes";
const BUDGET_NOTICE: &str = "[context] Additional MCP schemas exceed the search loading budget; narrow the search or select a tool explicitly.";
const SERVER_NOT_FOUND: &str = r#"{"tools":[],"count":0,"total_matches":0,"more_available":false,"next_cursor":null,"state":"server_not_found"}"#;

#[derive(Debug, Clone, Copy)]
pub(crate) struct SearchLimits {
    pub(crate) description: ContextLimit,
    pub(crate) search_result: ContextLimit,
    pub(crate) schema: SchemaLimits,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Search<'a> {
    pub(crate) query: &'a PreparedQuery,
    pub(crate) server: Option<&'a str>,
}

struct Published<'a> {
    server: &'a str,
    catalog: Arc<ToolCatalog>,
    instructions: Option<Arc<str>>,
}

struct Candidate<'a> {
    server: &'a str,
    instructions: Option<&'a str>,
    tool: &'a Tool,
    name: String,
    schema: String,
    tags: Vec<String>,
}

pub(crate) fn search(
    servers: &[Arc<Server>],
    names: &Mutex<ToolNames>,
    reserved: &[String],
    request: Search<'_>,
    limits: SearchLimits,
) -> McpSearchResult {
    if let Some(output) =
        authentication_required(servers, request.server.unwrap_or(request.query.raw()))
    {
        return McpSearchResult::plain(output);
    }
    if let Some(output) = request
        .server
        .and_then(|name| server_failure(servers, name))
    {
        return McpSearchResult::plain(output);
    }
    let scoped: Vec<&Arc<Server>> = servers
        .iter()
        .filter(|server| request.server.is_none_or(|name| server.config.name == name))
        .collect();
    if request.server.is_some() && scoped.is_empty() {
        return McpSearchResult::plain(SERVER_NOT_FOUND);
    }
    let catalogs: Vec<Published<'_>> = scoped
        .into_iter()
        .filter_map(|server| {
            server.catalog().map(|(catalog, instructions)| Published {
                server: &server.config.name,
                catalog,
                instructions,
            })
        })
        .collect();
    let mut candidates = Vec::new();
    let mut names = names.lock().unwrap_or_else(PoisonError::into_inner);
    for published in &catalogs {
        let server_name = published.server;
        for tool in &published.catalog.tools {
            let Ok(name) = names.name(reserved, server_name, &tool.name) else {
                continue;
            };
            if !is_model_safe_text(server_name.as_bytes()) || !is_model_safe_text(name.as_bytes()) {
                continue;
            }
            candidates.push(Candidate {
                server: server_name,
                instructions: published.instructions.as_deref(),
                tool,
                schema: tool.input_schema.to_string(),
                tags: tags_for(server_name, &tool.name),
                name,
            });
        }
    }
    drop(names);
    let documents: Vec<Document<'_>> = candidates.iter().map(document).collect();
    let page = retrieve(
        Request {
            query: request.query,
            server: request.server,
        },
        Domain::Mcp,
        &documents,
    );
    let matches: Vec<&Candidate<'_>> = page
        .matches
        .iter()
        .map(|index| &candidates[*index])
        .collect();
    let render = |count: usize, observed: Option<usize>| {
        render_result(
            &matches[..count],
            page.total_matches,
            page.cursor_after(count).as_deref(),
            limits,
            observed.map(|observed| (matches.len() - count, observed)),
        )
    };
    let full = render(matches.len(), None);
    let effective = limits.search_result.effective_bytes();
    let (output, retained, observed) = if full.len() <= effective {
        (full, matches.len(), None)
    } else {
        let observed = full.len();
        let mut retained = matches.len();
        loop {
            let candidate = render(retained, Some(observed));
            if candidate.len() <= effective || retained == 0 {
                break (candidate, retained, Some(observed));
            }
            retained -= 1;
        }
    };
    let mut notice = search_notice(&matches, retained, limits, observed);
    select_schemas(&matches[..retained], limits.schema, &mut notice);
    McpSearchResult {
        model_output: output,
        notice,
    }
}

fn document<'a>(candidate: &'a Candidate<'a>) -> Document<'a> {
    Document {
        identities: [&candidate.tool.name, &candidate.name],
        stable_key: &candidate.name,
        primary: [
            candidate.server,
            &candidate.tool.name,
            &candidate.name,
            candidate.tool.title.as_deref().unwrap_or_default(),
        ],
        primary_extra: &candidate.tags,
        secondary: [
            prefix(
                candidate.tool.catalog_description(),
                DESCRIPTION_SEARCH_BYTES,
            ),
            prefix(&candidate.schema, SCHEMA_SEARCH_BYTES),
            prefix(
                candidate.instructions.unwrap_or_default(),
                INSTRUCTIONS_SEARCH_BYTES,
            ),
        ],
    }
}

fn prefix(text: &str, max_bytes: usize) -> &str {
    &text[..text.floor_char_boundary(max_bytes.min(text.len()))]
}

fn select_schemas(matches: &[&Candidate<'_>], limits: SchemaLimits, notice: &mut Option<String>) {
    let mut remaining = limits.selected_schema.effective_bytes();
    for candidate in matches {
        match project(
            &candidate.name,
            candidate.tool,
            candidate.instructions,
            limits,
        ) {
            Projection::Rejected(message) => append_notice(notice, &message),
            Projection::Selected {
                spec,
                notice: truncated,
            } => {
                if let Some(message) = truncated {
                    append_notice(notice, &message);
                }
                let schema = selected_schema(&spec);
                if schema.len() > remaining {
                    append_notice(notice, BUDGET_NOTICE);
                    break;
                }
                remaining -= schema.len();
            }
        }
    }
}

fn append_notice(notice: &mut Option<String>, message: &str) {
    match notice {
        Some(current) => {
            current.push('\n');
            current.push_str(message);
        }
        None => *notice = Some(message.to_owned()),
    }
}

fn render_result(
    matches: &[&Candidate<'_>],
    total_matches: usize,
    next_cursor: Option<&str>,
    limits: SearchLimits,
    omitted: Option<(usize, usize)>,
) -> String {
    let mut output = String::from(r#"{"tools":["#);
    for (index, candidate) in matches.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write_tool(&mut output, candidate, limits.description);
    }
    let _ = write!(
        output,
        r#"],"count":{},"total_matches":{total_matches},"more_available":{},"next_cursor":{}"#,
        matches.len(),
        next_cursor.is_some(),
        next_cursor.map_or_else(|| "null".to_owned(), quote),
    );
    if let Some((omitted, observed)) = omitted {
        let limit = limits.search_result;
        let _ = write!(
            output,
            r#","context_limit":{{"name":"{RESULT_LIMIT}","action":"omitted","omitted_count":{omitted},"observed_bytes":{observed},"effective_bytes":{},"source":{},"override":"--context-limit {RESULT_LIMIT}=BYTES|off"}}"#,
            limit.effective_bytes(),
            quote(limit.source.label()),
        );
    }
    output.push('}');
    output
}

fn write_tool(output: &mut String, candidate: &Candidate<'_>, limit: ContextLimit) {
    let (description, observed) = bounded_encoded(
        candidate.tool.catalog_description(),
        limit.effective_bytes(),
    );
    let description = quote(&description);
    let _ = write!(
        output,
        r#"{{"name":{},"server":{},"description":{description},"purpose":{description},"usage":[{}]"#,
        encoded_json(&candidate.name),
        encoded_json(candidate.server),
        candidate
            .tags
            .iter()
            .map(|tag| encoded_json(tag))
            .collect::<Vec<_>>()
            .join(","),
    );
    if observed > limit.effective_bytes() {
        let _ = write!(
            output,
            r#","context_limit":{{"name":"{DESCRIPTION_LIMIT}","action":"truncated","observed_bytes":{observed},"effective_bytes":{},"source":{},"override":"--context-limit {DESCRIPTION_LIMIT}=BYTES|off"}}"#,
            limit.effective_bytes(),
            quote(limit.source.label()),
        );
    }
    output.push('}');
}

fn search_notice(
    matches: &[&Candidate<'_>],
    retained: usize,
    limits: SearchLimits,
    observed: Option<usize>,
) -> Option<String> {
    let mut notice = String::new();
    let description = limits.description;
    for candidate in &matches[..retained] {
        let length = encoded(candidate.tool.catalog_description()).len();
        if length <= description.effective_bytes() {
            continue;
        }
        notice.push_str("[context] MCP description for \"");
        write_scalar(&mut notice, &candidate.name);
        let _ = writeln!(
            notice,
            "\" truncated: observed={length} bytes effective={} bytes source={}; override with --context-limit {DESCRIPTION_LIMIT}=BYTES|off",
            description.effective_bytes(),
            description.source.label(),
        );
    }
    if let Some(observed) = observed {
        let _ = write!(
            notice,
            "[context] MCP search omitted {} tool(s) (",
            matches.len() - retained
        );
        for (index, candidate) in matches[retained..].iter().enumerate() {
            if index > 0 {
                notice.push_str(", ");
            }
            write_scalar(&mut notice, &candidate.name);
        }
        let limit = limits.search_result;
        let _ = write!(
            notice,
            "): observed={observed} bytes effective={} bytes source={}; override with --context-limit {RESULT_LIMIT}=BYTES|off",
            limit.effective_bytes(),
            limit.source.label(),
        );
    }
    (!notice.is_empty()).then_some(notice)
}

fn bounded_encoded(value: &str, max_bytes: usize) -> (String, usize) {
    let mut text = encoded(value);
    let observed = text.len();
    if observed > max_bytes {
        let mut end = text.floor_char_boundary(max_bytes);
        if let Some(ampersand) = text[..end].rfind('&')
            && !text[ampersand..end].contains(';')
        {
            end = ampersand;
        }
        text.truncate(end);
    }
    (text, observed)
}

fn authentication_required(servers: &[Arc<Server>], query: &str) -> Option<String> {
    servers.iter().find_map(|server| {
        let config = &server.config;
        if !contains_complete_identity(query, &config.name)
            || !matches!(server.lifecycle(), Lifecycle::Failed(_))
        {
            return None;
        }
        let environment = config
            .bearer_token_env
            .as_deref()
            .filter(|name| std::env::var_os(name).is_none())?;
        Some(format!(
            r#"{{"tools":[],"count":0,"authentication_required":{{"server":{},"interactive":false,"environment":{},"message":"Set this environment variable before starting oh-fx."}}}}"#,
            encoded_json(&config.name),
            encoded_json(environment),
        ))
    })
}

fn server_failure(servers: &[Arc<Server>], name: &str) -> Option<String> {
    servers
        .iter()
        .filter(|server| server.config.name == name)
        .find_map(|server| match server.lifecycle() {
            Lifecycle::Failed(failure) => Some(format!(
                r#"{{"tools":[],"count":0,"total_matches":0,"more_available":false,"next_cursor":null,"state":"server_failed","error":{}}}"#,
                encoded_json(&format!("MCP server '{name}' is unavailable: {failure}"))
            )),
            _ => None,
        })
}

fn contains_complete_identity(query: &str, identity: &str) -> bool {
    let query = query.as_bytes();
    let identity = identity.as_bytes();
    !identity.is_empty()
        && query
            .windows(identity.len())
            .enumerate()
            .any(|(start, value)| {
                value.eq_ignore_ascii_case(identity)
                    && (start == 0 || !is_identifier_byte(query[start - 1]))
                    && query
                        .get(start + identity.len())
                        .is_none_or(|byte| !is_identifier_byte(*byte))
            })
}

fn encoded(value: &str) -> String {
    let mut output = String::new();
    write_scalar(&mut output, value);
    output
}

fn encoded_json(value: &str) -> String {
    quote(&encoded(value))
}

fn quote(value: &str) -> String {
    Value::from(value).to_string()
}

#[cfg(test)]
mod tests;
