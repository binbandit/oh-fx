use super::*;
use crate::mcp_contract::{HttpHeaderEnv, TransportType};
use crate::streamable_http::HeaderError;

fn config() -> McpServerConfig {
    McpServerConfig {
        headers: vec![HttpHeader {
            name: "X-Workspace".to_owned(),
            value: "one".to_owned(),
        }],
        header_env: vec![HttpHeaderEnv {
            name: "X-Org".to_owned(),
            env: "ORG_ENV".to_owned(),
        }],
        bearer_token_env: Some("MCP_TOKEN".to_owned()),
        ..McpServerConfig::remote("api", TransportType::Http, "https://api.example.com/mcp")
    }
}

#[test]
fn resolved_headers_append_environment_values_and_the_bearer_token() {
    let lookup = |name: &str| match name {
        "ORG_ENV" => Some("acme".to_owned()),
        "MCP_TOKEN" => Some("secret".to_owned()),
        _ => None,
    };
    let headers = resolve_headers(&config(), &lookup, None).unwrap();
    let pairs: Vec<_> = headers
        .iter()
        .map(|header| (header.name.as_str(), header.value.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("X-Workspace", "one"),
            ("X-Org", "acme"),
            ("Authorization", "Bearer secret")
        ]
    );
}

#[test]
fn missing_environment_values_fail_before_any_request() {
    let only_org = |name: &str| (name == "ORG_ENV").then(|| "acme".to_owned());
    assert_eq!(
        resolve_headers(&config(), &|_| None, None),
        Err(McpError::McpHeaderEnvironmentMissing)
    );
    assert_eq!(
        resolve_headers(&config(), &only_org, None),
        Err(McpError::McpBearerEnvironmentMissing)
    );
    let injected = |name: &str| match name {
        "ORG_ENV" => Some("a\r\nb".to_owned()),
        _ => Some("token".to_owned()),
    };
    assert_eq!(
        resolve_headers(&config(), &injected, None),
        Err(McpError::Header(HeaderError::InvalidHeaderValue))
    );
}
