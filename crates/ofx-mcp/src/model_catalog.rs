use std::fmt::Write as _;

use ofx_text::write_scalar;

use crate::health::{AuthenticationState, ConnectionState, Status, classify};

const MAX_PROMPT_BYTES: usize = 4 * 1024;
const HEADER: &str = concat!(
    "Configured MCP servers visible to this model turn are listed below.\n",
    "A listed server is not a reason to use MCP. Use capability_search only when the task clearly needs a capability not already available locally. Pass the task and an optional exact server name. Call tools advertised after search directly. Use mcp_select_tool when a relevant result is not yet callable. Use returned identities and refine the query when needed.\n",
    "<mcp_servers>\n",
);
const FOOTER: &str = "</mcp_servers>\n";
const EMPTY_ENTRY: &str = "  <none />\n";
const CHANGE_HEADER: &str = "MCP server availability changed since earlier in this session:\n";
const CHANGE_FOOTER: &str = "The <mcp_servers> section above is current. Treat earlier claims in this conversation that a listed server required authentication or was unavailable as outdated; re-run capability_search before concluding a server cannot be used.\n";
const MAX_CHANGE_NOTICE_TRANSITIONS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Ready,
    Discovering,
    AuthenticationRequired,
    Disabled,
    Failed,
    Unavailable,
    AvailableOnDemand,
}

impl Availability {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Discovering => "discovering",
            Self::AuthenticationRequired => "authentication_required",
            Self::Disabled => "disabled",
            Self::Failed => "failed",
            Self::Unavailable => "unavailable",
            Self::AvailableOnDemand => "available_on_demand",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSummary {
    pub name: String,
    pub availability: Availability,
    pub tool_count: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSection {
    pub text: String,
    pub notice: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineEntry {
    pub name: String,
    pub availability: Availability,
}

impl From<&ServerSummary> for BaselineEntry {
    fn from(server: &ServerSummary) -> Self {
        Self {
            name: server.name.clone(),
            availability: server.availability,
        }
    }
}

pub(crate) fn classify_availability(
    connection: ConnectionState,
    authentication: AuthenticationState,
    deferred_for_ask: bool,
) -> Availability {
    if deferred_for_ask
        && connection == ConnectionState::Disconnected
        && authentication != AuthenticationState::Required
    {
        return Availability::AvailableOnDemand;
    }
    match classify(connection, authentication) {
        Status::Disabled => Availability::Disabled,
        Status::Connecting => Availability::Discovering,
        Status::Ready => Availability::Ready,
        Status::NeedsAuth => Availability::AuthenticationRequired,
        Status::Failed => Availability::Failed,
        Status::Unavailable => Availability::Unavailable,
    }
}

#[must_use]
pub fn render_model_catalog(servers: &[ServerSummary]) -> CatalogSection {
    render_with_limit(servers, MAX_PROMPT_BYTES)
}

fn render_with_limit(servers: &[ServerSummary], limit: usize) -> CatalogSection {
    if servers.is_empty() {
        let text = if entries_fit(limit, &[], Some(EMPTY_ENTRY)) {
            join(&[], Some(EMPTY_ENTRY))
        } else {
            String::new()
        };
        return CatalogSection { text, notice: None };
    }
    let mut sorted: Vec<&ServerSummary> = servers.iter().collect();
    sorted.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
    let entries: Vec<String> = sorted.into_iter().map(entry).collect();
    if entries_fit(limit, &entries, None) {
        return CatalogSection {
            text: join(&entries, None),
            notice: None,
        };
    }
    let mut retained = 0;
    for candidate in 0..=entries.len() {
        let marker = truncation_marker(entries.len() - candidate);
        if !entries_fit(limit, &entries[..candidate], Some(&marker)) {
            break;
        }
        retained = candidate;
    }
    let omitted = entries.len() - retained;
    let marker = truncation_marker(omitted);
    let text = if entries_fit(limit, &entries[..retained], Some(&marker)) {
        join(&entries[..retained], Some(&marker))
    } else {
        String::new()
    };
    CatalogSection {
        text,
        notice: Some(format!(
            "[context] omitted {omitted} MCP server{} from the model catalog because the fixed {limit}-byte budget was reached",
            if omitted == 1 { "" } else { "s" }
        )),
    }
}

fn entry(server: &ServerSummary) -> String {
    let mut line = String::from("  <server name=\"");
    write_scalar(&mut line, &server.name);
    let _ = write!(line, "\" state=\"{}\"", server.availability.as_str());
    if let Some(count) = server.tool_count {
        let _ = write!(line, " tools=\"{count}\"");
    }
    line.push_str(" />\n");
    line
}

fn truncation_marker(omitted: usize) -> String {
    format!("  <catalog_truncated omitted_count=\"{omitted}\" />\n")
}

fn entries_fit(limit: usize, entries: &[String], marker: Option<&str>) -> bool {
    let entries = entries.iter().map(String::len).sum::<usize>();
    [
        HEADER.len(),
        entries,
        marker.map_or(0, str::len),
        FOOTER.len(),
    ]
    .into_iter()
    .try_fold(limit, usize::checked_sub)
    .is_some()
}

fn join(entries: &[String], marker: Option<&str>) -> String {
    let mut text = String::from(HEADER);
    for entry in entries {
        text.push_str(entry);
    }
    text.push_str(marker.unwrap_or_default());
    text.push_str(FOOTER);
    text
}

#[must_use]
pub fn render_change_notice(
    baseline: &[BaselineEntry],
    current: &[ServerSummary],
) -> Option<String> {
    let mut lines = Lines::default();
    for server in current {
        let Some(entry) = baseline.iter().find(|entry| entry.name == server.name) else {
            lines.push(&server.name, |line| {
                let _ = writeln!(line, ": added ({})", server.availability.as_str());
            });
            continue;
        };
        if entry.availability == server.availability {
            continue;
        }
        lines.push(&server.name, |line| {
            let _ = write!(
                line,
                ": {} -> {}",
                entry.availability.as_str(),
                server.availability.as_str()
            );
            match server.tool_count {
                Some(count) => {
                    let _ = writeln!(line, " ({count} tools)");
                }
                None => line.push('\n'),
            }
        });
    }
    for entry in baseline {
        if !current.iter().any(|server| server.name == entry.name) {
            lines.push(&entry.name, |line| line.push_str(": removed\n"));
        }
    }
    if lines.transitions == 0 {
        return None;
    }
    if lines.omitted > 0 {
        let _ = writeln!(
            lines.body,
            "  and {} more change{}",
            lines.omitted,
            if lines.omitted == 1 { "" } else { "s" }
        );
    }
    Some([CHANGE_HEADER, &lines.body, CHANGE_FOOTER].concat())
}

#[derive(Default)]
struct Lines {
    body: String,
    transitions: usize,
    omitted: usize,
}

impl Lines {
    fn push(&mut self, name: &str, rest: impl FnOnce(&mut String)) {
        if self.transitions == MAX_CHANGE_NOTICE_TRANSITIONS {
            self.omitted += 1;
            return;
        }
        self.body.push_str("  ");
        write_scalar(&mut self.body, name);
        rest(&mut self.body);
        self.transitions += 1;
    }
}

#[cfg(test)]
mod tests;
