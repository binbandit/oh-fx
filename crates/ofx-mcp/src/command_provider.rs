use crate::streamable_http::validate_endpoint;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddIntent {
    Local {
        name: String,
        command: String,
        args: Vec<String>,
    },
    Http {
        name: String,
        url: String,
    },
}

impl AddIntent {
    pub fn name(&self) -> &str {
        match self {
            Self::Local { name, .. } | Self::Http { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AddIntentError {
    #[error("McpAddUsage")]
    McpAddUsage,
    #[error("McpInvalidServerName")]
    McpInvalidServerName,
    #[error("McpConfigInvalidUrl")]
    McpConfigInvalidUrl,
}

pub fn parse_add_intent<S: AsRef<str>>(tokens: &[S]) -> Result<AddIntent, AddIntentError> {
    let tokens: Vec<&str> = tokens.iter().map(AsRef::as_ref).collect();
    match tokens.as_slice() {
        [] | [_] => Err(AddIntentError::McpAddUsage),
        ["--transport", rest @ ..] => {
            let ["http", name, url] = rest else {
                return Err(AddIntentError::McpAddUsage);
            };
            if !is_valid_server_name(name) {
                return Err(AddIntentError::McpInvalidServerName);
            }
            validate_endpoint(url).map_err(|_| AddIntentError::McpConfigInvalidUrl)?;
            Ok(AddIntent::Http {
                name: (*name).to_owned(),
                url: (*url).to_owned(),
            })
        }
        [name, command, args @ ..] => {
            if !is_valid_server_name(name) {
                return Err(AddIntentError::McpInvalidServerName);
            }
            Ok(AddIntent::Local {
                name: (*name).to_owned(),
                command: (*command).to_owned(),
                args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            })
        }
    }
}

pub fn is_valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_add_intent_parses_local_and_http_argv_without_allocation() {
        assert_eq!(
            parse_add_intent(&["fixture", "node", "server.js", "--stdio"]),
            Ok(AddIntent::Local {
                name: "fixture".to_owned(),
                command: "node".to_owned(),
                args: vec!["server.js".to_owned(), "--stdio".to_owned()],
            })
        );
        assert_eq!(
            parse_add_intent(&["--transport", "http", "docs", "https://example.test/mcp"]),
            Ok(AddIntent::Http {
                name: "docs".to_owned(),
                url: "https://example.test/mcp".to_owned(),
            })
        );
    }

    #[test]
    fn mcp_add_intent_rejects_invalid_syntax_names_and_urls() {
        let empty: [&str; 0] = [];
        assert_eq!(parse_add_intent(&empty), Err(AddIntentError::McpAddUsage));
        assert_eq!(
            parse_add_intent(&["only-name"]),
            Err(AddIntentError::McpAddUsage)
        );
        assert_eq!(
            parse_add_intent(&["--transport", "sse", "docs", "https://example.test/mcp"]),
            Err(AddIntentError::McpAddUsage)
        );
        assert_eq!(
            parse_add_intent(&["bad/name", "node"]),
            Err(AddIntentError::McpInvalidServerName)
        );
        assert_eq!(
            parse_add_intent(&["--transport", "http", "docs", "file:///tmp/socket"]),
            Err(AddIntentError::McpConfigInvalidUrl)
        );
    }

    #[test]
    fn mcp_add_intent_rejects_invalid_server_names() {
        for name in ["", "has space", "dot.name", "slash/name"] {
            assert!(!is_valid_server_name(name), "{name}");
        }
        assert!(is_valid_server_name("Fixture_server-1"));
    }
}
