use crate::error::McpError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OAuthUri<'a> {
    pub(crate) scheme: &'a str,
    pub(crate) has_userinfo: bool,
    pub(crate) host: Option<String>,
    pub(crate) port: Option<u16>,
    pub(crate) path: String,
    pub(crate) fragment: Option<&'a str>,
}

impl<'a> OAuthUri<'a> {
    pub(crate) fn parse(text: &'a str) -> Option<Self> {
        let (scheme, rest) = text.split_once(':')?;
        let mut letters = scheme.bytes();
        if !letters.next()?.is_ascii_alphabetic()
            || !letters
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
        {
            return None;
        }
        let (rest, fragment) = match rest.split_once('#') {
            Some((rest, fragment)) => (rest, Some(fragment)),
            None => (rest, None),
        };
        let rest = rest.split_once('?').map_or(rest, |(rest, _)| rest);
        let Some(after) = rest.strip_prefix("//") else {
            return Some(Self {
                scheme,
                has_userinfo: false,
                host: None,
                port: None,
                path: percent_decoded(rest),
                fragment,
            });
        };
        let (authority, path) = match after.find('/') {
            Some(index) => after.split_at(index),
            None => (after, ""),
        };
        let (has_userinfo, host_port) = match authority.rsplit_once('@') {
            Some((_, host_port)) => (true, host_port),
            None => (false, authority),
        };
        let (host, port) = split_port(host_port)?;
        Some(Self {
            scheme,
            has_userinfo,
            host: Some(percent_decoded(host)),
            port,
            path: percent_decoded(path),
            fragment,
        })
    }

    pub(crate) fn is_loopback(&self) -> bool {
        self.scheme.eq_ignore_ascii_case("http") && self.port.is_some() && self.has_loopback_host()
    }

    pub(crate) fn has_loopback_host(&self) -> bool {
        self.host.as_deref().is_some_and(|host| {
            host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost") || host == "[::1]"
        })
    }

    pub(crate) fn is_secure_or_loopback(&self) -> bool {
        self.scheme.eq_ignore_ascii_case("https") || self.is_loopback()
    }

    pub(crate) fn origin(&self) -> Result<String, McpError> {
        let host = self
            .host
            .as_deref()
            .filter(|host| !host.is_empty())
            .ok_or(McpError::InvalidMcpAuthEndpoint)?;
        let scheme = self.scheme.to_ascii_lowercase();
        let host = host.to_ascii_lowercase();
        let default_port = matches!(
            (scheme.as_str(), self.port),
            ("https", Some(443)) | ("http", Some(80))
        );
        Ok(match self.port {
            Some(port) if !default_port => format!("{scheme}://{host}:{port}"),
            _ => format!("{scheme}://{host}"),
        })
    }

    pub(crate) fn raw_path(&self) -> &str {
        if self.path.is_empty() {
            "/"
        } else {
            &self.path
        }
    }
}

fn split_port(host_port: &str) -> Option<(&str, Option<u16>)> {
    let port_start = if host_port.starts_with('[') {
        let close = host_port.find(']')?;
        let after = &host_port[close + 1..];
        if after.is_empty() {
            return Some((host_port, None));
        }
        after.strip_prefix(':')?;
        close + 1
    } else {
        match host_port.rfind(':') {
            Some(index) => index,
            None => return Some((host_port, None)),
        }
    };
    let (host, port) = host_port.split_at(port_start);
    Some((host, Some(port[1..].parse().ok()?)))
}

fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let pair = bytes
            .get(index + 1..index + 3)
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match (bytes[index], pair) {
            (b'%', Some(byte)) => {
                decoded.push(byte);
                index += 3;
            }
            (byte, _) => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

pub(crate) fn canonical_resource(endpoint: &str) -> Result<String, McpError> {
    let uri = OAuthUri::parse(endpoint).ok_or(McpError::InvalidMcpAuthEndpoint)?;
    if uri.has_userinfo || uri.fragment.is_some() {
        return Err(McpError::InvalidMcpAuthEndpoint);
    }
    if !uri.is_secure_or_loopback() {
        return Err(McpError::InsecureMcpAuthEndpoint);
    }
    Ok(format!("{}{}", uri.origin()?, uri.raw_path()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_resource_normalizes_origin_identity_and_removes_query() {
        for (endpoint, canonical) in [
            (
                "HTTPS://MCP.Example.COM:443/Path?x=1",
                "https://mcp.example.com/Path",
            ),
            ("https://mcp.example.com", "https://mcp.example.com/"),
            (
                "https://mcp.example.com:8443/a/",
                "https://mcp.example.com:8443/a/",
            ),
            ("http://127.0.0.1:3000/mcp", "http://127.0.0.1:3000/mcp"),
            ("http://LOCALHOST:9/", "http://localhost:9/"),
            ("http://[::1]:7/mcp", "http://[::1]:7/mcp"),
            (
                "https://MCP%2Eexample.com/a%20b/%zz",
                "https://mcp.example.com/a b/%zz",
            ),
        ] {
            assert_eq!(
                canonical_resource(endpoint).unwrap(),
                canonical,
                "{endpoint}"
            );
        }
        for (endpoint, error) in [
            (
                "https://user:pass@mcp.example.com/",
                McpError::InvalidMcpAuthEndpoint,
            ),
            (
                "https://mcp.example.com/#frag",
                McpError::InvalidMcpAuthEndpoint,
            ),
            ("not a url", McpError::InvalidMcpAuthEndpoint),
            ("http://mcp.example.com/", McpError::InsecureMcpAuthEndpoint),
            ("http://localhost/mcp", McpError::InsecureMcpAuthEndpoint),
            (
                "http://192.168.1.2:80/mcp",
                McpError::InsecureMcpAuthEndpoint,
            ),
        ] {
            assert_eq!(canonical_resource(endpoint), Err(error), "{endpoint}");
        }
    }

    #[test]
    fn loopback_needs_http_an_explicit_port_and_a_loopback_host() {
        let loopback = |text| OAuthUri::parse(text).unwrap().is_loopback();
        assert!(loopback("http://127.0.0.1:1/"));
        assert!(loopback("HTTP://localhost:80"));
        assert!(loopback("http://[::1]:5/x"));
        assert!(loopback("http://local%68ost:5/x"));
        assert!(!loopback("http://localhost/"));
        assert!(!loopback("https://localhost:443/"));
        assert!(!loopback("http://127.0.0.2:1/"));
    }
}
