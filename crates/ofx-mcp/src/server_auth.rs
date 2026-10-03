use crate::error::McpError;
use crate::mcp_contract::{HttpHeader, McpServerConfig};
use crate::streamable_http::validate_static_headers;

pub(crate) fn resolve_headers(
    config: &McpServerConfig,
    environment: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<HttpHeader>, McpError> {
    let mut headers = config.headers.clone();
    for reference in &config.header_env {
        let value = environment(&reference.env).ok_or(McpError::McpHeaderEnvironmentMissing)?;
        headers.push(HttpHeader {
            name: reference.name.clone(),
            value,
        });
    }
    if let Some(env_name) = &config.bearer_token_env {
        let token = environment(env_name).ok_or(McpError::McpBearerEnvironmentMissing)?;
        headers.push(HttpHeader {
            name: "Authorization".to_owned(),
            value: format!("Bearer {token}"),
        });
    }
    validate_static_headers(&headers)?;
    Ok(headers)
}

#[cfg(test)]
mod tests {
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
        let headers = resolve_headers(&config(), &lookup).unwrap();
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
            resolve_headers(&config(), &|_| None),
            Err(McpError::McpHeaderEnvironmentMissing)
        );
        assert_eq!(
            resolve_headers(&config(), &only_org),
            Err(McpError::McpBearerEnvironmentMissing)
        );
        let injected = |name: &str| match name {
            "ORG_ENV" => Some("a\r\nb".to_owned()),
            _ => Some("token".to_owned()),
        };
        assert_eq!(
            resolve_headers(&config(), &injected),
            Err(McpError::Header(HeaderError::InvalidHeaderValue))
        );
    }
}
