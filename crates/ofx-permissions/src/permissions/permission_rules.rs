use std::net::Ipv6Addr;

use ofx_contract::PermissionRule;

pub const WEB_SEARCH_PERMISSION: &str = "web_search";
pub const WEB_FETCH_PERMISSION: &str = "web_fetch";
const DOMAIN_PREFIX: &str = "domain:";

pub fn permission_name_for_tool(tool_name: &str) -> &str {
    match tool_name {
        "read_file" => "read",
        "write_file" | "edit_file" => "edit",
        "glob_files" => "glob",
        "grep_files" => "grep",
        "run_command" => "bash",
        WEB_FETCH_PERMISSION => WEB_FETCH_PERMISSION,
        "skill" | "install_skill" => "skill",
        other => other,
    }
}

pub fn canonical_web_fetch_domain_pattern(raw: &str) -> Option<String> {
    let host_raw = raw.strip_prefix(DOMAIN_PREFIX).unwrap_or(raw);
    if host_raw.contains("://") || host_raw.contains(['/', '*', '?']) {
        return None;
    }
    let host = strip_one_root_dot(host_raw);
    is_canonicalizable_host(host).then(|| format!("{DOMAIN_PREFIX}{}", host.to_ascii_lowercase()))
}

pub fn is_canonical_web_fetch_domain_pattern(pattern: &str) -> bool {
    pattern.strip_prefix(DOMAIN_PREFIX).is_some_and(|host| {
        is_canonicalizable_host(host)
            && !host.ends_with('.')
            && !host.bytes().any(|byte| byte.is_ascii_uppercase())
    })
}

pub fn web_fetch_rule_warning_count(rules: &[PermissionRule]) -> usize {
    rules
        .iter()
        .filter(|rule| {
            rule.permission == WEB_FETCH_PERMISSION
                && !is_canonical_web_fetch_domain_pattern(&rule.pattern)
        })
        .count()
}

fn strip_one_root_dot(host: &str) -> &str {
    if host.len() > 1 {
        host.strip_suffix('.').unwrap_or(host)
    } else {
        host
    }
}

fn is_canonicalizable_host(host: &str) -> bool {
    if host.starts_with('[') {
        return is_canonicalizable_ipv6_literal(host);
    }
    if host.is_empty() || host.contains(':') {
        return false;
    }
    let mut label_len = 0;
    for byte in host.bytes() {
        if byte == b'.' {
            if label_len == 0 {
                return false;
            }
            label_len = 0;
            continue;
        }
        if !byte.is_ascii_alphanumeric() && byte != b'-' {
            return false;
        }
        label_len += 1;
    }
    label_len > 0
}

fn is_canonicalizable_ipv6_literal(host: &str) -> bool {
    let Some(inner) = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .filter(|inner| !inner.is_empty())
    else {
        return false;
    };
    inner.bytes().all(|byte| {
        !byte.is_ascii_uppercase() && (byte.is_ascii_hexdigit() || byte == b':' || byte == b'.')
    }) && inner.parse::<Ipv6Addr>().is_ok()
}

#[cfg(test)]
mod tests {
    use ofx_contract::PermissionAction;

    use super::*;

    #[test]
    fn tool_names_map_to_upstreams_permission_categories() {
        for (tool, permission) in [
            ("read_file", "read"),
            ("write_file", "edit"),
            ("edit_file", "edit"),
            ("glob_files", "glob"),
            ("grep_files", "grep"),
            ("run_command", "bash"),
            ("web_fetch", "web_fetch"),
            ("skill", "skill"),
            ("install_skill", "skill"),
            ("shell", "shell"),
            ("web_search", "web_search"),
        ] {
            assert_eq!(permission_name_for_tool(tool), permission, "{tool}");
        }
    }

    #[test]
    fn canonical_domain_patterns_lower_case_hosts_and_drop_one_root_dot() {
        for (raw, canonical) in [
            ("domain:Docs.Example.com", "domain:docs.example.com"),
            ("Example.COM.", "domain:example.com"),
            ("[2606:4700:4700::1111]", "domain:[2606:4700:4700::1111]"),
            ("localhost", "domain:localhost"),
        ] {
            assert_eq!(
                canonical_web_fetch_domain_pattern(raw).as_deref(),
                Some(canonical),
                "{raw}"
            );
        }
    }

    #[test]
    fn canonical_domain_patterns_reject_wildcards_paths_and_malformed_hosts() {
        for raw in [
            "*",
            "*.example.com",
            "example.com/path",
            "https://example.com",
            "exa?mple.com",
            "example..com",
            ".example.com",
            "example.com:443",
            "exa_mple.com",
            "",
            "domain:",
            ".",
            "[]",
            "[%25eth0]",
            "[2606:4700::ZZ]",
            "[2606:4700:4700::1111",
            "ex ample.com",
        ] {
            assert_eq!(canonical_web_fetch_domain_pattern(raw), None, "{raw}");
        }
    }

    #[test]
    fn only_lower_case_hosts_without_a_root_dot_are_canonical() {
        assert!(is_canonical_web_fetch_domain_pattern("domain:example.com"));
        assert!(is_canonical_web_fetch_domain_pattern(
            "domain:[2606:4700:4700::1111]"
        ));
        for pattern in [
            "domain:Example.com",
            "domain:example.com.",
            "example.com",
            "domain:*",
            "domain:[2606:4700:4700::AB]",
        ] {
            assert!(!is_canonical_web_fetch_domain_pattern(pattern), "{pattern}");
        }
    }

    #[test]
    fn warnings_count_only_web_fetch_rules_that_are_not_canonical_domains() {
        let rule = |permission: &str, pattern: &str| PermissionRule {
            permission: permission.to_owned(),
            pattern: pattern.to_owned(),
            action: PermissionAction::Allow,
        };
        let rules = [
            rule("web_fetch", "*"),
            rule("web_fetch", "domain:example.com"),
            rule("web_fetch", "domain:Example.com"),
            rule("read", "*"),
        ];
        assert_eq!(web_fetch_rule_warning_count(&rules), 2);
    }
}
