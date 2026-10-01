#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Uri<'a> {
    pub(crate) scheme: &'a str,
    pub(crate) has_userinfo: bool,
    pub(crate) host: Option<&'a str>,
    pub(crate) port: Option<u16>,
    pub(crate) path: &'a str,
    pub(crate) has_fragment: bool,
}

impl<'a> Uri<'a> {
    pub(crate) fn parse(text: &'a str) -> Option<Self> {
        let (scheme, rest) = text.split_once(':')?;
        if !is_scheme(scheme) {
            return None;
        }
        let (rest, fragment) = match rest.split_once('#') {
            Some((before, _)) => (before, true),
            None => (rest, false),
        };
        let rest = rest.split_once('?').map_or(rest, |(before, _)| before);
        let Some(after_slashes) = rest.strip_prefix("//") else {
            return Some(Self {
                scheme,
                has_userinfo: false,
                host: None,
                port: None,
                path: rest,
                has_fragment: fragment,
            });
        };
        let authority_end = after_slashes.find('/').unwrap_or(after_slashes.len());
        let (authority, path) = after_slashes.split_at(authority_end);
        let (has_userinfo, host_port) = match authority.rsplit_once('@') {
            Some((_, host_port)) => (true, host_port),
            None => (false, authority),
        };
        let (host, port) = split_host_port(host_port)?;
        Some(Self {
            scheme,
            has_userinfo,
            host: Some(host),
            port,
            path,
            has_fragment: fragment,
        })
    }

    pub(crate) fn is_loopback_host(&self) -> bool {
        self.host.is_some_and(|host| {
            host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost") || host == "[::1]"
        })
    }
}

fn is_scheme(scheme: &str) -> bool {
    let mut bytes = scheme.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

fn split_host_port(host_port: &str) -> Option<(&str, Option<u16>)> {
    let port_separator = if host_port.starts_with('[') {
        let close = host_port.find(']')?;
        match &host_port[close + 1..] {
            "" => None,
            rest if rest.starts_with(':') => Some(close + 1),
            _ => return None,
        }
    } else {
        host_port.rfind(':')
    };
    let Some(separator) = port_separator else {
        return Some((host_port, None));
    };
    let port = host_port[separator + 1..].parse().ok()?;
    Some((&host_port[..separator], Some(port)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_authority_components() {
        let uri = Uri::parse("https://user@Example.com:8443/a/b?x=1#frag").unwrap();
        assert_eq!(uri.scheme, "https");
        assert!(uri.has_userinfo);
        assert_eq!(uri.host, Some("Example.com"));
        assert_eq!(uri.port, Some(8443));
        assert_eq!(uri.path, "/a/b");
        assert!(uri.has_fragment);
    }

    #[test]
    fn keeps_bracketed_ipv6_hosts_and_rejects_bad_ports() {
        let uri = Uri::parse("http://[::1]:4321/mcp").unwrap();
        assert_eq!(uri.host, Some("[::1]"));
        assert_eq!(uri.port, Some(4321));
        assert!(uri.is_loopback_host());
        assert_eq!(Uri::parse("http://[::1]/").unwrap().port, None);
        assert!(Uri::parse("http://host:99999/").is_none());
        assert!(Uri::parse("no scheme").is_none());
    }

    #[test]
    fn parses_scheme_only_references_without_a_host() {
        let uri = Uri::parse("acp:server-1").unwrap();
        assert_eq!(uri.host, None);
        assert_eq!(uri.path, "server-1");
    }
}
