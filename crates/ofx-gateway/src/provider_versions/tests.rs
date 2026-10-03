use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use ofx_http::{ConnectionOptions, build_connection_client};
use ofx_testkit::{FakeServer, Reply};

use super::*;

fn client() -> reqwest::Client {
    build_connection_client(&ConnectionOptions {
        user_agent: "oh-fx/test".to_owned(),
        ..ConnectionOptions::default()
    })
    .expect("build the client")
}

fn release(version: &str) -> Reply {
    Reply::status(
        200,
        format!("{{\"version\":\"{version}\",\"name\":\"@openai/codex\"}}"),
    )
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

fn cache_file(directory: &Path) -> PathBuf {
    directory.join(CACHE_DIRECTORY).join(CODEX_CACHE_FILE)
}

#[test]
fn provider_versions_accept_bounded_stable_releases_and_reject_header_or_url_data() {
    assert_eq!(
        Version::parse(" v0.153.1\n").map(|version| version.as_str().to_owned()),
        Some("0.153.1".to_owned())
    );
    for raw in [
        "",
        "latest",
        "1.2",
        "1.2.3.4",
        "1.2.3?x=y",
        "1.2.3\r\nHeader: value",
        "4294967296.1.2",
    ] {
        assert_eq!(Version::parse(raw), None, "{raw:?}");
    }
}

#[test]
fn codex_release_metadata_reads_the_npm_version_field() {
    assert_eq!(
        parse_codex_release(br#"{"version":"0.153.1","name":"@openai/codex"}"#),
        Version::parse("0.153.1")
    );
    for body in [
        &br#"{"version":"bad"}"#[..],
        br#"{"name":"@openai/codex"}"#,
        br#"{"version":1}"#,
        br#"{"version":"0.153.1","version":"0.153.2"}"#,
        br#"["0.153.1"]"#,
        b"0.153.1",
    ] {
        assert_eq!(parse_codex_release(body), None, "{body:?}");
    }
}

#[test]
fn provider_version_cache_round_trips_and_rejects_damaged_data() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("cache/oh-fx");
    assert_eq!(load_cache(&directory), None);
    let cached = Cached {
        version: Version::parse("0.153.1").unwrap(),
        checked_at_ms: 500,
    };
    save_cache(&directory, &cached).unwrap();
    assert_eq!(
        fs::read_to_string(cache_file(&directory)).unwrap(),
        "{\"version\":\"0.153.1\",\"checked_at_ms\":500}\n"
    );
    let loaded = load_cache(&directory).unwrap();
    assert_eq!(loaded, cached);
    assert!(loaded.fresh(500));
    assert!(!loaded.fresh(499));
    assert!(!loaded.fresh(500 + REFRESH_INTERVAL_MS));
    assert!(loaded.fresh(500 + REFRESH_INTERVAL_MS - 1));
    assert!(!loaded.fresh(i64::MIN));
    for damaged in [
        "{\"version\":\"bad\",\"checked_at_ms\":0}",
        "{\"version\":\"0.153.1\"}",
        "{\"version\":\"0.153.1\",\"checked_at_ms\":0,\"extra\":true}",
        "{\"version\":\"0.153.1\",\"checked_at_ms\":0.5}",
        "{\"version\":\"0.153.1\",\"checked_at_ms\":0,\"checked_at_ms\":1}",
        "not json",
    ] {
        PrivateDir::open_existing(&directory.join(CACHE_DIRECTORY))
            .unwrap()
            .unwrap()
            .replace(CODEX_CACHE_FILE, damaged.as_bytes())
            .unwrap();
        assert_eq!(load_cache(&directory), None, "{damaged}");
    }
}

#[test]
fn provider_version_cache_creates_private_directories_from_a_fresh_home() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("cache/oh-fx");
    save_cache(
        &directory,
        &Cached {
            version: Version::parse("1.0.13").unwrap(),
            checked_at_ms: 500,
        },
    )
    .unwrap();
    for path in [directory.clone(), directory.join(CACHE_DIRECTORY)] {
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert_eq!(
        fs::metadata(cache_file(&directory))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn provider_version_cache_rejects_directories_symlinks_and_hardlinks() {
    for kind in ["directory", "symlink", "hardlink"] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("oh-fx");
        let versions = PrivateDir::open_or_create(&directory.join(CACHE_DIRECTORY)).unwrap();
        versions
            .replace("target", b"{\"version\":\"1.2.3\",\"checked_at_ms\":500}")
            .unwrap();
        let target = directory.join(CACHE_DIRECTORY).join("target");
        let name = cache_file(&directory);
        match kind {
            "directory" => fs::create_dir(&name).unwrap(),
            "symlink" => symlink(&target, &name).unwrap(),
            _ => fs::hard_link(&target, &name).unwrap(),
        }
        assert_eq!(load_cache(&directory), None, "{kind}");
    }
}

#[tokio::test]
async fn provider_versions_refresh_after_a_minute_and_keep_the_last_valid_cache_on_failure() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("cache/oh-fx");
    let server = FakeServer::start([
        release("0.153.1"),
        release("0.154.0"),
        Reply::status(503, ""),
    ]);
    let client = client();
    let url = format!("{}/codex/latest", server.base_url());
    let lookup = VersionLookup {
        client: &client,
        url: &url,
        cache_directory: Some(&directory),
    };
    let cancel = CancellationToken::new();
    let resolve = |now_ms| lookup.resolve_at(&cancel, deadline(), now_ms);

    assert_eq!(resolve(0).await, Ok(Version::parse("0.153.1").unwrap()));
    assert_eq!(
        resolve(REFRESH_INTERVAL_MS - 1).await,
        Ok(Version::parse("0.153.1").unwrap())
    );
    assert_eq!(server.requests().len(), 1);
    assert_eq!(
        resolve(REFRESH_INTERVAL_MS).await,
        Ok(Version::parse("0.154.0").unwrap())
    );
    assert_eq!(
        resolve(2 * REFRESH_INTERVAL_MS).await,
        Ok(Version::parse("0.154.0").unwrap())
    );
    assert_eq!(
        resolve(2 * REFRESH_INTERVAL_MS + 1).await,
        Ok(Version::parse("0.154.0").unwrap())
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/v1/codex/latest");
    assert_eq!(requests[0].header("user-agent"), Some("oh-fx/test"));
    assert_eq!(
        fs::read_to_string(cache_file(&directory)).unwrap(),
        format!(
            "{{\"version\":\"0.154.0\",\"checked_at_ms\":{}}}\n",
            2 * REFRESH_INTERVAL_MS
        )
    );
}

#[tokio::test]
async fn provider_version_failures_do_not_fabricate_a_version_or_suppress_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let server = FakeServer::start([
        Reply::status(200, "{\"version\":\"latest\"}"),
        release("0.153.1"),
        Reply::status(200, "x".repeat(MAX_RESPONSE_BYTES + 1)),
    ]);
    let client = client();
    let url = format!("{}/codex/latest", server.base_url());
    let blocked = root.path().join("not-a-directory");
    fs::write(&blocked, "").unwrap();
    let lookup = VersionLookup {
        client: &client,
        url: &url,
        cache_directory: Some(&blocked),
    };
    let cancel = CancellationToken::new();
    assert_eq!(
        lookup.resolve_at(&cancel, deadline(), 0).await,
        Err(VersionError::Unavailable)
    );
    assert_eq!(
        lookup.resolve_at(&cancel, deadline(), 0).await,
        Ok(Version::parse("0.153.1").unwrap())
    );
    assert_eq!(
        lookup.resolve_at(&cancel, deadline(), 0).await,
        Err(VersionError::Unavailable)
    );

    let directory = root.path().join("cache/oh-fx");
    save_cache(
        &directory,
        &Cached {
            version: Version::parse("0.153.1").unwrap(),
            checked_at_ms: 0,
        },
    )
    .unwrap();
    let cached = VersionLookup {
        client: &client,
        url: &url,
        cache_directory: Some(&directory),
    };
    cancel.cancel();
    assert_eq!(
        cached
            .resolve_at(&cancel, deadline(), REFRESH_INTERVAL_MS)
            .await,
        Err(VersionError::Cancelled)
    );
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn provider_version_lookups_stop_at_the_outer_deadline() {
    let server = FakeServer::start([Reply::delayed_status(
        200,
        "{\"version\":\"0.153.1\"}",
        Duration::from_secs(5),
    )]);
    let client = client();
    let url = format!("{}/codex/latest", server.base_url());
    let lookup = VersionLookup {
        client: &client,
        url: &url,
        cache_directory: None,
    };
    let started = Instant::now();
    assert_eq!(
        lookup
            .resolve(
                &CancellationToken::new(),
                Instant::now() + Duration::from_millis(100)
            )
            .await,
        Err(VersionError::Unavailable)
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn grok_release_version_uses_plain_text_and_rejects_header_injection() {
    assert_eq!(
        parse_grok_release(b"1.0.13\n").map(|version| version.0),
        Some("1.0.13".to_owned())
    );
    assert_eq!(parse_grok_release(b"1.0.13\nHeader: injected"), None);
    assert_eq!(parse_grok_release(br#"{"version":"1.0.13"}"#), None);
}
