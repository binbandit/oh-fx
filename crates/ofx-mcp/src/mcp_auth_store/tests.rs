use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::Arc;

use super::*;

const ONE: &str = r#"{"server_identity":"one","endpoint":"https://mcp.example/a","resource":"https://mcp.example/","issuer":"https://issuer.example","client_id":"client","client_secret":null,"access_token":"secret-one","refresh_token":"refresh-one","scope":"read","token_type":"Bearer","token_endpoint_auth_method":"none","expires_at_ms":123,"authorization_endpoint":"https://issuer.example/authorize","token_endpoint":"https://issuer.example/token","revocation_endpoint":null}"#;
const TWO: &str = r#"{"server_identity":"two","endpoint":"https://mcp.example/a","resource":"https://mcp.example/","issuer":"https://issuer.example","client_id":"client","client_secret":null,"access_token":"secret-two","refresh_token":null,"scope":"read","token_type":"Bearer","token_endpoint_auth_method":"none","expires_at_ms":456,"authorization_endpoint":"https://issuer.example/authorize","token_endpoint":"https://issuer.example/token","revocation_endpoint":null}"#;

fn store_json(entries: &[&str]) -> String {
    format!(r#"{{"version":1,"credentials":[{}]}}"#, entries.join(","))
}

fn credentials(endpoint: &str, access_token: &str) -> Credentials {
    Credentials {
        endpoint: endpoint.to_owned(),
        resource: endpoint.to_owned(),
        issuer: "https://issuer.example".to_owned(),
        client_id: "client".to_owned(),
        client_secret: None,
        access_token: Zeroizing::new(access_token.to_owned()),
        refresh_token: Some(Zeroizing::new("refresh-secret".to_owned())),
        scope: "tools.read".to_owned(),
        token_type: "Bearer".to_owned(),
        token_endpoint_auth_method: "none".to_owned(),
        expires_at_ms: 123_456,
        authorization_endpoint: "https://issuer.example/authorize".to_owned(),
        token_endpoint: "https://issuer.example/token".to_owned(),
        revocation_endpoint: None,
    }
}

fn lookup(
    identity: &str,
    endpoint: &str,
    resource: Option<&str>,
    issuer: Option<&str>,
) -> GrantLookup {
    GrantLookup::new(identity, endpoint, resource, issuer).unwrap()
}

fn save_for(
    store: &CredentialStore,
    identity: &str,
    credentials: &Credentials,
) -> Result<SaveResult, McpError> {
    store.save(
        &lookup(
            identity,
            &credentials.endpoint,
            Some(&credentials.resource),
            Some(&credentials.issuer),
        ),
        credentials,
    )
}

struct Home {
    root: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn data(&self) -> PathBuf {
        fs::canonicalize(self.root.path())
            .unwrap()
            .join("data/oh-fx")
    }

    fn store(&self) -> CredentialStore {
        CredentialStore::new(&self.data())
    }

    fn directory(&self) -> PathBuf {
        self.data().join(DIRECTORY_NAME)
    }

    fn file(&self) -> PathBuf {
        self.directory().join(FILE_NAME)
    }

    fn write(&self, bytes: &str) {
        fs::create_dir_all(self.directory()).unwrap();
        fs::write(self.file(), bytes).unwrap();
        fs::set_permissions(self.file(), fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn the_parser_keeps_distinct_server_and_oauth_identities() {
    let store = parse_store(store_json(&[ONE, TWO]).as_bytes()).unwrap();
    assert_eq!(store.credentials.len(), 2);
    assert_eq!(store.credentials[0].1.access_token.as_str(), "secret-one");
    assert_eq!(store.credentials[1].0, "two");
    assert_eq!(store.credentials[1].1.refresh_token, None);
    assert_eq!(store.rejected_entries, 0);
}

#[test]
fn a_round_trip_writes_upstreams_bytes_and_accepts_an_empty_scope() {
    let mut entry = credentials("https://mcp.example/no-scope", "empty-scope-secret");
    entry.scope = String::new();
    entry.client_secret = Some(Zeroizing::new("s\"\\\n\u{1}é".to_owned()));
    entry.revocation_endpoint = Some("https://issuer.example/revoke".to_owned());
    let store = Store {
        credentials: vec![("no-scope".to_owned(), entry.clone())],
        rejected_entries: 0,
    };
    let bytes = serialize_store(&store).unwrap();
    assert_eq!(
        bytes.as_str(),
        r#"{"version":1,"credentials":[{"server_identity":"no-scope","endpoint":"https://mcp.example/no-scope","resource":"https://mcp.example/no-scope","issuer":"https://issuer.example","client_id":"client","client_secret":"s\"\\\n\u0001é","access_token":"empty-scope-secret","refresh_token":"refresh-secret","scope":"","token_type":"Bearer","token_endpoint_auth_method":"none","expires_at_ms":123456,"authorization_endpoint":"https://issuer.example/authorize","token_endpoint":"https://issuer.example/token","revocation_endpoint":"https://issuer.example/revoke"}]}"#
    );
    let decoded = parse_store(bytes.as_bytes()).unwrap();
    assert_eq!(decoded.credentials, [("no-scope".to_owned(), entry)]);
}

#[test]
fn malformed_entries_are_isolated_from_valid_credentials() {
    let valid = TWO.replace("\"two\"", "\"valid\"");
    let store = parse_store(store_json(&["{}", "3", &valid]).as_bytes()).unwrap();
    assert_eq!(store.credentials.len(), 1);
    assert_eq!(store.credentials[0].0, "valid");
    assert_eq!(store.rejected_entries, 2);
    for broken in [
        TWO.replace("\"expires_at_ms\":456", "\"expires_at_ms\":\"soon\""),
        TWO.replace(",\"expires_at_ms\":456", ""),
        TWO.replace("\"expires_at_ms\":456", "\"expires_at_ms\":4.5"),
        TWO.replace("\"client_id\":\"client\"", "\"client_id\":\"\""),
        TWO.replace("\"refresh_token\":null", "\"refresh_token\":\"\""),
        TWO.replace("\"scope\":\"read\"", "\"scope\":null"),
        TWO.replace("\"server_identity\":\"two\"", "\"server_identity\":7"),
    ] {
        let store = parse_store(store_json(&[&broken]).as_bytes()).unwrap();
        assert!(store.credentials.is_empty(), "{broken}");
        assert_eq!(store.rejected_entries, 1, "{broken}");
    }
}

#[test]
fn a_null_expiry_is_a_grant_that_never_expires() {
    let never = TWO.replace("\"expires_at_ms\":456", "\"expires_at_ms\":null");
    let store = parse_store(store_json(&[&never]).as_bytes()).unwrap();
    let entry = &store.credentials[0].1;
    assert_eq!(entry.expires_at_ms, i64::MAX);
    assert!(!entry.needs_refresh(4_102_444_800_000));
}

#[test]
fn the_top_level_schema_version_is_strict() {
    for document in [
        r#"{"version":2,"credentials":[]}"#,
        r#"{"version":"1","credentials":[]}"#,
        r#"{"version":1.0,"credentials":[]}"#,
        r#"{"credentials":[]}"#,
        r#"{"version":1,"credentials":{}}"#,
        r#"{"version":1}"#,
        "[]",
    ] {
        assert_eq!(
            parse_store(document.as_bytes()).err(),
            Some(McpError::InvalidMcpCredentialStore),
            "{document}"
        );
    }
    assert_eq!(
        parse_store(b"{").err(),
        Some(McpError::UnexpectedEndOfInput)
    );
    assert_eq!(parse_store(b"{]").err(), Some(McpError::SyntaxError));
}

#[test]
fn the_store_is_private_and_atomic_and_loads_after_a_restart() {
    let home = Home::new();
    assert_eq!(
        home.store().load(&lookup(
            "server-one",
            "https://mcp.example/service",
            None,
            None
        )),
        Ok(None)
    );
    assert!(!home.data().exists());
    let saved = credentials("https://mcp.example/service", "access-secret");
    assert_eq!(
        save_for(&home.store(), "server-one", &saved),
        Ok(SaveResult {
            repaired_entries: 0
        })
    );
    let loaded = home
        .store()
        .load(&lookup(
            "server-one",
            "https://mcp.example/service",
            None,
            None,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(loaded, saved);
    assert_eq!(mode(&home.data()), 0o700);
    assert_eq!(mode(&home.directory()), 0o700);
    assert_eq!(mode(&home.file()), 0o600);
    assert_eq!(mode(&home.directory().join(LOCK_FILE_NAME)), 0o600);
    let names: Vec<_> = fs::read_dir(home.directory())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
}

#[test]
fn a_save_replaces_the_same_identity_and_keeps_the_others() {
    let home = Home::new();
    let store = home.store();
    save_for(
        &store,
        "one",
        &credentials("https://mcp.example/a", "first"),
    )
    .unwrap();
    save_for(
        &store,
        "two",
        &credentials("https://mcp.example/a", "second"),
    )
    .unwrap();
    let mut other_issuer = credentials("https://mcp.example/a", "third");
    other_issuer.issuer = "https://other.example".to_owned();
    save_for(&store, "one", &other_issuer).unwrap();
    save_for(
        &store,
        "one",
        &credentials("https://mcp.example/a", "renewed"),
    )
    .unwrap();
    let saved = parse_store(&fs::read(home.file()).unwrap()).unwrap();
    let tokens: Vec<_> = saved
        .credentials
        .iter()
        .map(|(identity, entry)| (identity.as_str(), entry.access_token.as_str()))
        .collect();
    assert_eq!(
        tokens,
        [("one", "renewed"), ("two", "second"), ("one", "third")]
    );
    assert_eq!(
        store.load(&lookup("one", "https://mcp.example/a", None, None)),
        Ok(None),
        "two issuers for one server are ambiguous"
    );
    let issued = store
        .load(&lookup(
            "one",
            "https://mcp.example/a",
            None,
            Some("https://other.example"),
        ))
        .unwrap()
        .unwrap();
    assert_eq!(issued.access_token.as_str(), "third");
    let by_resource = store
        .load(&lookup(
            "two",
            "HTTPS://MCP.EXAMPLE:443/a?x",
            Some("https://mcp.example/a"),
            None,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(by_resource.access_token.as_str(), "second");
    assert_eq!(
        store.load(&lookup(
            "two",
            "https://mcp.example/a",
            Some("https://mcp.example/b"),
            None
        )),
        Ok(None)
    );
}

#[test]
fn a_grant_is_found_under_the_identity_discovery_accepted() {
    let home = Home::new();
    let store = home.store();
    let mut enclosing = credentials("https://mcp.example/team/mcp", "enclosing");
    enclosing.resource = "https://mcp.example/team".to_owned();
    enclosing.issuer = "https://issuer.example".to_owned();
    save_for(&store, "one", &enclosing).unwrap();
    let found = |resource: Option<&str>, issuer: Option<&str>| {
        store
            .load(&lookup(
                "one",
                "https://mcp.example/team/mcp",
                resource,
                issuer,
            ))
            .unwrap()
            .map(|credentials| credentials.access_token.as_str().to_owned())
    };
    for (resource, issuer) in [
        (Some("https://mcp.example/team/mcp"), None),
        (Some("https://mcp.example/team"), None),
        (None, Some("https://issuer.example/")),
        (
            Some("https://MCP.example:443/team/mcp"),
            Some("https://issuer.example"),
        ),
    ] {
        assert_eq!(
            found(resource, issuer).as_deref(),
            Some("enclosing"),
            "{resource:?} {issuer:?}"
        );
    }
    for (resource, issuer) in [
        (Some("https://mcp.example/teams"), None),
        (Some("https://mcp.example/"), None),
        (Some("https://other.example/team/mcp"), None),
        (None, Some("https://issuer.example/other")),
        (None, Some("https://issuer.example//")),
    ] {
        assert_eq!(found(resource, issuer), None, "{resource:?} {issuer:?}");
    }
}

fn tokens(home: &Home) -> Vec<(String, String)> {
    parse_store(&fs::read(home.file()).unwrap())
        .unwrap()
        .credentials
        .iter()
        .map(|(identity, entry)| (identity.clone(), entry.access_token.as_str().to_owned()))
        .collect()
}

#[test]
fn a_save_supersedes_every_grant_its_lookup_would_find() {
    let home = Home::new();
    let store = home.store();
    let endpoint = "https://mcp.example/team/mcp";
    let mut slashed = credentials(endpoint, "slashed");
    slashed.issuer = "https://issuer.example/".to_owned();
    let mut enclosing = credentials(endpoint, "enclosing");
    enclosing.resource = "https://mcp.example/team".to_owned();
    let mut elsewhere = credentials(endpoint, "elsewhere");
    elsewhere.issuer = "https://other.example".to_owned();
    for entry in [&slashed, &enclosing, &elsewhere] {
        save_for(&store, "one", entry).unwrap();
    }
    save_for(&store, "two", &credentials(endpoint, "other server")).unwrap();
    let configured = lookup(
        "one",
        endpoint,
        Some(endpoint),
        Some("https://issuer.example"),
    );
    assert_eq!(store.load(&configured), Ok(None));
    store
        .save(&configured, &credentials(endpoint, "renewed"))
        .unwrap();
    let owned = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(identity, token)| ((*identity).to_owned(), (*token).to_owned()))
            .collect()
    };
    assert_eq!(
        tokens(&home),
        owned(&[
            ("one", "renewed"),
            ("one", "elsewhere"),
            ("two", "other server")
        ])
    );
    assert_eq!(
        store
            .load(&configured)
            .unwrap()
            .unwrap()
            .access_token
            .as_str(),
        "renewed"
    );
    let unconfigured = lookup("one", endpoint, None, None);
    store
        .save(&unconfigured, &credentials(endpoint, "only"))
        .unwrap();
    assert_eq!(
        tokens(&home),
        owned(&[("one", "only"), ("two", "other server")])
    );
    assert_eq!(
        store
            .load(&unconfigured)
            .unwrap()
            .unwrap()
            .access_token
            .as_str(),
        "only"
    );
}

#[test]
fn no_sequence_of_saves_and_refreshes_makes_a_saved_lookup_ambiguous() {
    let home = Home::new();
    let store = home.store();
    let endpoint = "https://mcp.example/team/mcp";
    let lookups = [
        lookup("one", endpoint, None, None),
        lookup("one", endpoint, None, Some("https://issuer.example")),
        lookup("one", endpoint, None, Some("https://issuer.example/")),
        lookup("one", endpoint, None, Some("https://other.example")),
        lookup("one", endpoint, Some(endpoint), None),
        lookup("one", endpoint, Some("https://mcp.example/team"), None),
        lookup("two", endpoint, None, None),
    ];
    let shapes = [
        ("https://issuer.example", endpoint),
        ("https://issuer.example/", endpoint),
        ("https://issuer.example", "https://mcp.example/team"),
        ("https://other.example", endpoint),
        ("https://other.example", "https://mcp.example/"),
    ];
    let matching = |lookup: &GrantLookup| -> usize {
        fs::read(home.file()).map_or(0, |bytes| {
            parse_store(&bytes)
                .unwrap()
                .credentials
                .iter()
                .filter(|(identity, entry)| lookup.matches(identity, entry))
                .count()
        })
    };
    let mut sessions: Vec<(String, Credentials)> = Vec::new();
    let mut seed: u64 = 0x5eed;
    let mut next = |bound: usize| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(seed >> 33).unwrap() % bound
    };
    for step in 0..400 {
        let counts: Vec<usize> = lookups.iter().map(&matching).collect();
        if sessions.is_empty() || next(3) > 0 {
            let chosen = &lookups[next(lookups.len())];
            let (issuer, resource) = shapes[next(shapes.len())];
            let mut grant = credentials(endpoint, &format!("token-{step}"));
            grant.issuer = issuer.to_owned();
            grant.resource = resource.to_owned();
            if !chosen.matches(&chosen.identity, &grant) {
                continue;
            }
            store.save(chosen, &grant).unwrap();
            assert_eq!(matching(chosen), 1, "step {step}");
            sessions.push((chosen.identity.clone(), grant));
        } else {
            let (identity, grant) = &sessions[next(sessions.len())];
            let mut refreshed = grant.clone();
            refreshed.access_token = Zeroizing::new(format!("refreshed-{step}"));
            store.save_refreshed(identity, &refreshed).unwrap();
            for (lookup, before) in lookups.iter().zip(&counts) {
                assert!(matching(lookup) <= *before, "step {step}");
            }
        }
    }
    assert!(sessions.len() > 100);
}

#[test]
fn a_save_drops_rejected_entries_and_reports_the_repair() {
    let home = Home::new();
    home.write(r#"{"version":1,"credentials":[{}]}"#);
    let result = save_for(
        &home.store(),
        "fixture",
        &credentials("https://mcp.example/service", "replacement"),
    )
    .unwrap();
    assert_eq!(result.repaired_entries, 1);
    let saved = parse_store(&fs::read(home.file()).unwrap()).unwrap();
    assert_eq!(saved.rejected_entries, 0);
    assert_eq!(saved.credentials.len(), 1);
    assert_eq!(saved.credentials[0].1.access_token.as_str(), "replacement");
}

#[test]
fn an_unsafe_or_invalid_file_is_refused() {
    let home = Home::new();
    let load = || {
        home.store()
            .load(&lookup("one", "https://mcp.example/a", None, None))
    };
    home.write(&store_json(&[ONE]));
    for mode in [0o644, 0o400, 0o700] {
        fs::set_permissions(home.file(), fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            load(),
            Err(McpError::Durable(DurableError::PermissionsUnsupported)),
            "{mode:o}"
        );
    }
    fs::remove_file(home.file()).unwrap();
    let elsewhere = home.data().join("elsewhere.json");
    fs::write(&elsewhere, store_json(&[ONE])).unwrap();
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&elsewhere, home.file()).unwrap();
    assert_eq!(load(), Err(McpError::Durable(DurableError::PathUnsafe)));
    fs::remove_file(home.file()).unwrap();
    home.write(&" ".repeat(MAX_STORE_BYTES + 1));
    assert_eq!(load(), Err(McpError::StreamTooLong));
    home.write(r#"{"version":3,"credentials":[]}"#);
    assert_eq!(load(), Err(McpError::InvalidMcpCredentialStore));
}

#[test]
fn a_held_lock_makes_the_store_busy() {
    let home = Home::new();
    let store = home.store();
    save_for(
        &store,
        "one",
        &credentials("https://mcp.example/a", "first"),
    )
    .unwrap();
    let held = store.open_existing().unwrap().unwrap();
    let started = Instant::now();
    assert_eq!(
        store.load(&lookup("one", "https://mcp.example/a", None, None)),
        Err(McpError::LockBusy)
    );
    assert!(started.elapsed() >= LOCK_DEADLINE);
    drop(held);
    assert!(
        store
            .load(&lookup("one", "https://mcp.example/a", None, None))
            .unwrap()
            .is_some()
    );
}

#[test]
fn concurrent_writers_keep_every_identity() {
    let home = Home::new();
    let store = Arc::new(home.store());
    let writers: Vec<_> = (0..8)
        .map(|index| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                save_for(
                    &store,
                    &format!("server-{index}"),
                    &credentials("https://mcp.example/a", &format!("token-{index}")),
                )
                .unwrap();
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    for index in 0..8 {
        let loaded = store
            .load(&lookup(
                &format!("server-{index}"),
                "https://mcp.example/a",
                None,
                None,
            ))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.access_token.as_str(), format!("token-{index}"));
    }
}

#[test]
fn debug_output_never_shows_a_secret() {
    let mut entry = credentials("https://mcp.example/a", "access-secret-value");
    entry.client_secret = Some(Zeroizing::new("client-secret-value".to_owned()));
    let shown = format!("{entry:?}");
    for secret in [
        "access-secret-value",
        "refresh-secret",
        "client-secret-value",
    ] {
        assert!(!shown.contains(secret), "{shown}");
    }
}
