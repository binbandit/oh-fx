use super::*;

fn address(text: &str) -> IpAddr {
    text.parse().unwrap()
}

fn follow(redirect: Redirect) -> ValidatedUrl {
    match redirect {
        Redirect::Follow(target) => target,
        Redirect::CrossHost(url) => panic!("expected a followed redirect, got {url}"),
    }
}

#[test]
fn rejects_non_http_schemes_credentials_single_label_hosts_and_overlong_urls() {
    assert_eq!(
        normalize("ftp://example.com/file"),
        Err(PolicyError::UnsupportedScheme)
    );
    assert_eq!(
        normalize("https://token@example.com/private"),
        Err(PolicyError::CredentialedUrl)
    );
    assert_eq!(
        normalize("https://intranet/path"),
        Err(PolicyError::SingleLabelHost)
    );
    assert_eq!(normalize(&"a".repeat(2001)), Err(PolicyError::UrlTooLong));
    assert_eq!(normalize(""), Err(PolicyError::EmptyUrl));
}

#[test]
fn canonicalizes_ascii_hostname_root_dot_and_rejects_malformed_percent_encoded_and_unicode_hosts() {
    let normalized = normalize("http://Example.COM./docs?q=1#frag").unwrap();
    assert_eq!(normalized.canonical_host, "example.com");
    assert_eq!(normalized.port, 443);
    assert_eq!(normalized.path_query, "/docs?q=1");
    assert_eq!(normalized.retrieval_url, "https://example.com/docs?q=1");

    assert_eq!(
        normalize("https://example.com/%zz"),
        Err(PolicyError::MalformedPercentEncoding)
    );
    assert_eq!(
        normalize("https://exa%6dple.com/"),
        Err(PolicyError::PercentEncodedHost)
    );
    assert_eq!(
        normalize("https://\u{e9}xample.com/"),
        Err(PolicyError::UnicodeHost)
    );
}

#[test]
fn upgrades_http_and_keeps_only_non_default_ports() {
    let cases = [
        (
            "http://example.com:8080/docs",
            "https://example.com:8080/docs",
            8080,
        ),
        ("http://example.com:80", "https://example.com/", 443),
        ("https://example.com:443/a", "https://example.com/a", 443),
        ("https://example.com:80/a", "https://example.com:80/a", 80),
        ("HTTPS://example.com?x=1", "https://example.com/?x=1", 443),
        (
            "https://[2606:4700:4700::1111]:8443/",
            "https://[2606:4700:4700::1111]:8443/",
            8443,
        ),
    ];
    for (raw, retrieval, port) in cases {
        let normalized = normalize(raw).unwrap();
        assert_eq!(normalized.retrieval_url, retrieval, "{raw}");
        assert_eq!(normalized.port, port, "{raw}");
    }
    assert_eq!(
        normalize("https://example.com:/"),
        Err(PolicyError::InvalidPort)
    );
    assert_eq!(
        normalize("https://example.com:70000/"),
        Err(PolicyError::InvalidPort)
    );
    assert_eq!(
        normalize("https://example.com:+1/"),
        Err(PolicyError::InvalidPort)
    );
}

#[test]
fn rejects_raw_spaces_in_request_targets_and_redirects() {
    assert_eq!(
        normalize("https://example.com/a b"),
        Err(PolicyError::RequestTargetWhitespace)
    );
    assert_eq!(
        normalize("https://example.com/search?q=a b"),
        Err(PolicyError::RequestTargetWhitespace)
    );
    let current = normalize("https://example.com/docs").unwrap();
    assert_eq!(
        redirect_target(&current, "/a b"),
        Err(PolicyError::RequestTargetWhitespace)
    );
}

#[test]
fn every_blocked_ipv4_range_ipv6_envelope_exclusion_and_embedded_ipv4_range() {
    for text in [
        "0.1.2.3",
        "10.0.0.1",
        "100.64.0.1",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "192.0.0.1",
        "192.0.2.1",
        "192.88.99.1",
        "192.168.0.1",
        "198.18.0.1",
        "198.51.100.1",
        "203.0.113.1",
        "224.0.0.1",
        "240.0.0.1",
    ] {
        assert!(!is_public_address(address(text)), "{text}");
    }
    assert!(is_public_address(address("93.184.216.34")));
    for text in [
        "::1",
        "fc00::1",
        "fe80::1",
        "ff00::1",
        "2001::1",
        "2001:db8::1",
        "2002::1",
        "3fff::1",
        "::ffff:127.0.0.1",
        "::ffff:192.168.0.1",
    ] {
        assert!(!is_public_address(address(text)), "{text}");
    }
    assert!(is_public_address(address("2606:4700:4700::1111")));
}

#[test]
fn rejects_public_and_private_ipv4_mapped_ipv6_literals() {
    for embedded in ["93.184.216.34", "127.0.0.1", "192.168.0.1"] {
        let literal = format!("::ffff:{embedded}");
        assert!(!is_public_address(address(&literal)));
        assert_eq!(
            normalize(&format!("https://[{literal}]/")),
            Err(PolicyError::NonPublicAddress)
        );
    }
}

#[test]
fn blocks_local_names_metadata_hosts_and_ambiguous_numeric_hosts() {
    for host in [
        "localhost.localdomain",
        "metadata.google.internal",
        "metadata.goog",
        "app.localhost",
        "printer.local",
        "box.localdomain",
        "db.internal",
    ] {
        assert_eq!(
            normalize(&format!("https://{host}/")),
            Err(PolicyError::NonPublicAddress),
            "{host}"
        );
    }
    assert_eq!(
        normalize("https://localhost/"),
        Err(PolicyError::SingleLabelHost)
    );
    for host in [
        "0x7f.0.0.1",
        "127.1",
        "1.2.3",
        "a.b.c.1",
        "01.example.com",
        "256.1.1.1",
    ] {
        assert_eq!(
            normalize(&format!("https://{host}/")),
            Err(PolicyError::InvalidIpv4),
            "{host}"
        );
    }
    for host in [
        "-bad.example.com",
        "bad-.example.com",
        "a..b.com",
        "a_b.example.com",
    ] {
        assert!(normalize(&format!("https://{host}/")).is_err(), "{host}");
    }
    assert_eq!(
        normalize("https://93.184.216.34/").unwrap().canonical_host,
        "93.184.216.34"
    );
}

#[test]
fn bracketed_hosts_reject_scope_ids_trailing_garbage_and_private_addresses() {
    assert_eq!(
        normalize("https://[fe80::1%25en0]/"),
        Err(PolicyError::ScopeIdRejected)
    );
    assert_eq!(
        normalize("https://[2606:4700:4700::1111]x/"),
        Err(PolicyError::MalformedHost)
    );
    assert_eq!(normalize("https://[]/"), Err(PolicyError::MissingHost));
    assert_eq!(
        normalize("https://[::1]/"),
        Err(PolicyError::NonPublicAddress)
    );
    assert_eq!(
        normalize("https://[2606:4700:4700::ABCD]/")
            .unwrap()
            .canonical_host,
        "2606:4700:4700::abcd"
    );
}

#[test]
fn bracketed_hosts_embed_ipv4_only_after_the_mapped_prefix() {
    for literal in [
        "2606::ffff:93.184.216.34",
        "2606:ffff::1.2.3.4",
        "::127.0.0.1",
        "::93.184.216.34",
        "0:0:0:0:0:ffff:127.0.0.1",
        "64:ff9b::127.0.0.1",
        "::ffff:0:93.184.216.34",
        "::ffff:000",
        "::ffff:1.2.3",
        "::ffff:01.2.3.4",
    ] {
        assert_eq!(
            normalize(&format!("https://[{literal}]/")),
            Err(PolicyError::MalformedHost),
            "{literal}"
        );
    }
    for literal in ["::ffff:93.184.216.34", "::FFFF:127.0.0.1"] {
        assert_eq!(
            normalize(&format!("https://[{literal}]/")),
            Err(PolicyError::NonPublicAddress),
            "{literal}"
        );
    }
}

#[test]
fn relative_redirect_resolves_before_admission_and_malformed_location_fails_closed() {
    let current = normalize("https://example.com/a/b/page").unwrap();
    let redirected = follow(redirect_target(&current, "../next?q=1").unwrap());
    assert_eq!(redirected.retrieval_url, "https://example.com/a/next?q=1");
    assert_eq!(
        redirect_target(&current, "javascript:alert(1)"),
        Err(PolicyError::UnsupportedScheme)
    );
    assert_eq!(
        redirect_target(&current, "https://exa mple.com/"),
        Err(PolicyError::MalformedLocation)
    );
    assert_eq!(
        redirect_target(&current, ""),
        Err(PolicyError::MalformedLocation)
    );
}

#[test]
fn redirect_normalization_rejects_credentials_scope_ids_control_bytes_and_port_changes() {
    let current = normalize("https://example.com/docs").unwrap();
    assert_eq!(
        redirect_target(&current, "https://token@example.com/"),
        Err(PolicyError::CredentialedUrl)
    );
    assert_eq!(
        redirect_target(&current, "https://[2606:4700:4700::1111%25en0]/"),
        Err(PolicyError::ScopeIdRejected)
    );
    assert_eq!(
        redirect_target(&current, "https://example.com/a\nb"),
        Err(PolicyError::ControlByte)
    );
    assert_eq!(
        redirect_target(&current, "https://example.com:8443/docs"),
        Err(PolicyError::PortChanged)
    );
    assert_eq!(
        redirect_target(&current, "http://127.0.0.1:3000/private"),
        Err(PolicyError::NonPublicAddress)
    );
}

#[test]
fn scheme_relative_and_fragment_redirect_behavior_is_deterministic() {
    let current = normalize("https://example.com/docs/index.html?x=1").unwrap();
    let scheme_relative = follow(redirect_target(&current, "//example.com/next#frag").unwrap());
    assert_eq!(scheme_relative.retrieval_url, "https://example.com/next");
    let fragment = follow(redirect_target(&current, "#section").unwrap());
    assert_eq!(
        fragment.retrieval_url,
        "https://example.com/docs/index.html?x=1"
    );
    let dot_segments = follow(redirect_target(&current, "./a/../b/").unwrap());
    assert_eq!(dot_segments.retrieval_url, "https://example.com/docs/b/");
}

#[test]
fn cross_host_redirect_returns_reinvocation_result_without_following() {
    let current = normalize("https://example.com/docs").unwrap();
    assert_eq!(
        redirect_target(&current, "https://example.org/docs"),
        Ok(Redirect::CrossHost("https://example.org/docs".to_owned()))
    );
    let www = follow(redirect_target(&current, "https://www.example.com/docs").unwrap());
    assert_eq!(www.canonical_host, "www.example.com");
    let explicit_port = normalize("https://example.com:8443/a").unwrap();
    let same_port = follow(redirect_target(&explicit_port, "/b").unwrap());
    assert_eq!(same_port.retrieval_url, "https://example.com:8443/b");
}
