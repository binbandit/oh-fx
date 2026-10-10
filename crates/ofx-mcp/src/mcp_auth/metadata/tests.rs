use super::*;

const LOGIN: &str = r#"{"issuer":"https://login.example.com","authorization_endpoint":"https://login.example.com/authorize","token_endpoint":"https://login.example.com/token"}"#;

fn metadata(bytes: &str, issuer: &str) -> AuthorizationMetadata {
    match parse_authorization_metadata(bytes.as_bytes(), issuer).unwrap() {
        MetadataOutcome::Metadata(metadata) => *metadata,
        MetadataOutcome::IssuerMismatch(_) => panic!("the issuer matches"),
    }
}

#[test]
fn oauth_urls_stay_https_or_loopback_for_a_local_resource() {
    assert_eq!(
        validate_oauth_url_for_resource(
            "https://login.example.com/oauth",
            "https://api.example.com/mcp"
        ),
        Ok(())
    );
    assert_eq!(
        validate_oauth_url_for_resource("http://127.0.0.1:4321/oauth", "http://localhost:9876/mcp"),
        Ok(())
    );
    for (candidate, resource) in [
        ("http://127.0.0.1:4321/oauth", "https://api.example.com/mcp"),
        (
            "https://user@login.example.com/oauth",
            "https://api.example.com/mcp",
        ),
        (
            "https://login.example.com/oauth#x",
            "https://api.example.com/mcp",
        ),
        (
            "http://login.example.com/oauth",
            "http://localhost:9876/mcp",
        ),
    ] {
        assert_eq!(
            validate_oauth_url_for_resource(candidate, resource),
            Err(McpError::InsecureMcpAuthEndpoint),
            "{candidate}"
        );
    }
}

#[test]
fn discovery_urls_keep_the_issuer_and_resource_paths() {
    assert_eq!(
        protected_resource_metadata_urls("https://api.example.com/tenant/mcp").unwrap(),
        [
            "https://api.example.com/.well-known/oauth-protected-resource/tenant/mcp",
            "https://api.example.com/.well-known/oauth-protected-resource",
        ]
    );
    assert_eq!(
        protected_resource_metadata_urls("https://API.example.com:443").unwrap(),
        ["https://api.example.com/.well-known/oauth-protected-resource"]
    );
    assert_eq!(
        authorization_metadata_urls("https://login.example.com/tenant").unwrap(),
        [
            "https://login.example.com/.well-known/oauth-authorization-server/tenant",
            "https://login.example.com/.well-known/openid-configuration/tenant",
            "https://login.example.com/tenant/.well-known/openid-configuration",
        ]
    );
    assert_eq!(
        authorization_metadata_urls("https://login.example.com/").unwrap(),
        [
            "https://login.example.com/.well-known/oauth-authorization-server",
            "https://login.example.com/.well-known/openid-configuration",
        ]
    );
    for issuer in [
        "http://login.example.com",
        "https://login.example.com/?tenant=1",
        "https://login.example.com/#x",
        "issuer",
    ] {
        assert_eq!(
            authorization_metadata_urls(issuer),
            Err(McpError::InvalidAuthorizationIssuer),
            "{issuer}"
        );
    }
}

#[test]
fn omitted_token_endpoint_methods_default_to_client_secret_basic() {
    let parsed = metadata(LOGIN, "https://login.example.com");
    assert_eq!(
        parsed.token_endpoint_auth_methods_supported,
        ["client_secret_basic"]
    );
    assert!(!parsed.supports_s256());
    assert!(!parsed.client_id_metadata_document_supported);
    assert!(!parsed.authorization_response_iss_parameter_supported);
}

#[test]
fn issuers_match_exactly_apart_from_a_trailing_slash() {
    assert_eq!(
        metadata(LOGIN, "https://login.example.com/").issuer,
        "https://login.example.com"
    );
    let slashed = LOGIN.replace(
        "\"issuer\":\"https://login.example.com\"",
        "\"issuer\":\"https://login.example.com/\"",
    );
    assert_eq!(
        metadata(&slashed, "https://login.example.com").issuer,
        "https://login.example.com/"
    );
    for expected in [
        "https://login.evil.example/",
        "https://login.example.com/tenant",
    ] {
        assert_eq!(
            parse_authorization_metadata(LOGIN.as_bytes(), expected),
            Ok(MetadataOutcome::IssuerMismatch(IssuerMismatch {
                source: IssuerMismatchSource::AuthorizationMetadata,
                expected: expected.to_owned(),
                returned: "https://login.example.com".to_owned(),
            })),
            "{expected}"
        );
    }
}

#[test]
fn authorization_metadata_fields_are_validated() {
    let insecure_token = LOGIN.replace(
        "https://login.example.com/token",
        "http://login.example.com/token",
    );
    let numeric_revocation = LOGIN.replace('}', r#","revocation_endpoint":5}"#);
    let empty_scope = LOGIN.replace('}', r#","scopes_supported":["a",""]}"#);
    let null_grants = LOGIN.replace('}', r#","grant_types_supported":null}"#);
    for (bytes, error) in [
        ("[]", McpError::InvalidAuthorizationMetadata),
        ("{", McpError::UnexpectedEndOfInput),
        (
            r#"{"authorization_endpoint":"x"}"#,
            McpError::MissingMetadataField,
        ),
        (r#"{"issuer":""}"#, McpError::InvalidMetadataField),
        (insecure_token.as_str(), McpError::InvalidMetadataUrl),
        (numeric_revocation.as_str(), McpError::InvalidMetadataUrl),
        (empty_scope.as_str(), McpError::InvalidMetadataField),
        (null_grants.as_str(), McpError::InvalidMetadataField),
    ] {
        assert_eq!(
            parse_authorization_metadata(bytes.as_bytes(), "https://login.example.com"),
            Err(error),
            "{bytes}"
        );
    }
    let full = metadata(
        &LOGIN.replace(
            '}',
            r#","registration_endpoint":"https://login.example.com/register","revocation_endpoint":null,"code_challenge_methods_supported":["S256"],"grant_types_supported":["authorization_code","refresh_token"],"client_id_metadata_document_supported":true,"authorization_response_iss_parameter_supported":"yes"}"#,
        ),
        "https://login.example.com",
    );
    assert_eq!(
        full.registration_endpoint.as_deref(),
        Some("https://login.example.com/register")
    );
    assert_eq!(full.revocation_endpoint, None);
    assert!(full.supports_s256() && full.supports_refresh_token());
    assert!(full.client_id_metadata_document_supported);
    assert!(!full.authorization_response_iss_parameter_supported);
}

#[test]
fn resource_metadata_must_cover_the_resource() {
    let document = |resource: &str| {
        format!(
            r#"{{"resource":"{resource}","authorization_servers":["https://login.example.com"],"scopes_supported":["tools"]}}"#
        )
    };
    let parsed = parse_resource_metadata(
        document("https://API.example.com/").as_bytes(),
        "https://api.example.com/tenant/mcp",
    )
    .unwrap();
    assert_eq!(parsed.resource, "https://api.example.com/");
    assert_eq!(parsed.authorization_servers, ["https://login.example.com"]);
    assert_eq!(parsed.scopes_supported, ["tools"]);
    for resource in [
        "https://api.example.com/tenant",
        "https://api.example.com/tenant/mcp",
    ] {
        assert!(
            parse_resource_metadata(
                document(resource).as_bytes(),
                "https://api.example.com/tenant/mcp"
            )
            .is_ok()
        );
    }
    for (resource, expected) in [
        (
            "https://api.example.com/ten",
            "https://api.example.com/tenant/mcp",
        ),
        ("https://other.example.com/", "https://api.example.com/mcp"),
    ] {
        assert_eq!(
            parse_resource_metadata(document(resource).as_bytes(), expected),
            Err(McpError::McpAuthResourceMismatch),
            "{resource}"
        );
    }
    assert_eq!(
        parse_resource_metadata(
            br#"{"resource":"https://api.example.com/","authorization_servers":[]}"#,
            "https://api.example.com/"
        ),
        Err(McpError::InvalidProtectedResourceMetadata)
    );
    assert_eq!(
        parse_resource_metadata(
            br#"{"resource":"https://api.example.com/"}"#,
            "https://api.example.com/"
        ),
        Err(McpError::MissingMetadataField)
    );
}
