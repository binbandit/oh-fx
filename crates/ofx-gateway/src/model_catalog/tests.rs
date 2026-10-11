use super::*;

#[test]
fn catalog_http_failures_follow_the_upstream_categories() {
    let unavailable = |retryable| CatalogFailure::GatewayUnavailable { retryable };
    for (status, expected) in [
        (401, CatalogFailure::Authentication),
        (403, CatalogFailure::Authentication),
        (429, CatalogFailure::RateLimited),
        (500, unavailable(true)),
        (501, unavailable(false)),
        (502, unavailable(true)),
        (503, unavailable(true)),
        (504, unavailable(true)),
        (505, unavailable(false)),
        (599, unavailable(false)),
        (400, CatalogFailure::HttpStatus),
        (302, CatalogFailure::HttpStatus),
    ] {
        assert_eq!(failure_for_http_status(status), expected, "{status}");
    }
}

#[test]
fn only_rate_limits_transport_failures_and_passing_gateway_errors_are_retried() {
    let retried = [
        CatalogFailure::RateLimited,
        CatalogFailure::Transport,
        CatalogFailure::GatewayUnavailable { retryable: true },
    ];
    for failure in retried {
        assert!(failure.retryable(), "{failure:?}");
    }
    for failure in [
        CatalogFailure::Authentication,
        CatalogFailure::GatewayUnavailable { retryable: false },
        CatalogFailure::Cancellation,
        CatalogFailure::MalformedResponse,
        CatalogFailure::HttpStatus,
    ] {
        assert!(!failure.retryable(), "{failure:?}");
    }
    assert_eq!(
        CatalogFailure::GatewayUnavailable { retryable: false }.error_name(),
        "GatewayUnavailable"
    );
}

fn keyed() -> CatalogAccess {
    CatalogAccess::Authenticated {
        source: "stored_key",
        credential: Zeroizing::new("secret-key".to_owned()),
        login_team: None,
        explicit: false,
    }
}

#[test]
fn access_metadata_names_the_level_source_and_public_reason() {
    assert_eq!(
        AccessMetadata::of(&keyed()),
        AccessMetadata {
            source: Some("stored_key"),
            public_only: None,
        }
    );
    assert_eq!(
        AccessMetadata::of(&CatalogAccess::HostManaged),
        AccessMetadata {
            source: Some("host_managed"),
            public_only: None,
        }
    );
    let public = AccessMetadata::of(&CatalogAccess::PublicOnly(PublicOnlyReason::NoCredential));
    assert_eq!(public.source, None);
    assert!(public.public_only.is_some());
    assert!(!format!("{:?}", keyed()).contains("secret-key"));
}

#[test]
fn only_a_rejected_key_falls_back_to_the_public_catalog() {
    let rejected = CatalogFailed::for_http_status(403);
    assert!(matches!(
        keyed().public_fallback_after(rejected),
        Some(CatalogAccess::PublicOnly(
            PublicOnlyReason::CredentialRejected("stored_key")
        ))
    ));
    for failure in [
        CatalogFailed::for_http_status(429),
        CatalogFailed::without_status(CatalogFailure::Authentication, false),
    ] {
        assert!(
            keyed().public_fallback_after(failure).is_none(),
            "{failure:?}"
        );
    }
    let explicit = CatalogAccess::Authenticated {
        source: "stored_key",
        credential: Zeroizing::new("secret-key".to_owned()),
        login_team: None,
        explicit: true,
    };
    for access in [
        CatalogAccess::HostManaged,
        CatalogAccess::PublicOnly(PublicOnlyReason::NoCredential),
        explicit,
    ] {
        assert!(access.public_fallback_after(rejected).is_none());
    }
}

#[test]
fn the_load_trace_line_carries_upstream_fields_in_upstream_order() {
    let requested = AccessMetadata::of(&keyed());
    let fallback: CatalogFetch<()> = CatalogFetch::Loaded {
        models: Vec::new(),
        access: AccessMetadata::of(&CatalogAccess::PublicOnly(
            PublicOnlyReason::CredentialRejected("stored_key"),
        )),
        anonymous_fallback: true,
        fallback_failure: Some(CatalogFailed::for_http_status(401)),
    };
    assert_eq!(
        load_line(requested, &fallback),
        "requested_access=authenticated credential_source=stored_key effective_access=public_only public_only_reason=authenticated_credential_rejected anonymous_fallback=true outcome=loaded failure_category=authentication http_status=401 retryable=false"
    );
    let loaded: CatalogFetch<()> = CatalogFetch::Loaded {
        models: Vec::new(),
        access: requested,
        anonymous_fallback: false,
        fallback_failure: None,
    };
    assert_eq!(
        load_line(requested, &loaded),
        "requested_access=authenticated credential_source=stored_key effective_access=authenticated public_only_reason=none anonymous_fallback=false outcome=loaded failure_category=none http_status=none retryable=none"
    );
    let public = AccessMetadata::of(&CatalogAccess::PublicOnly(PublicOnlyReason::NoCredential));
    let failed: CatalogFetch<()> = CatalogFetch::Failed {
        access: public,
        anonymous_fallback: false,
        failure: CatalogFailed::without_status(CatalogFailure::Transport, true),
    };
    assert_eq!(
        load_line(public, &failed),
        "requested_access=public_only credential_source=none effective_access=public_only public_only_reason=no_credential anonymous_fallback=false outcome=failed failure_category=transport http_status=none retryable=true"
    );
}
