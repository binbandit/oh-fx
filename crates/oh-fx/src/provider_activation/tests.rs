use std::os::unix::fs::{PermissionsExt, symlink};

use ofx_auth::ChatGptEndpoints;
use ofx_gateway::{CodexEndpoints, CodexModelsEndpoints};
use ofx_testkit::{FakeServer, Reply};
use serde_json::{Value, json};

use super::*;

pub(crate) const ACCESS_TOKEN: &str = "eyJhbGciOiJub25lIn0.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF90ZXN0In0sImV4cCI6NDEwMjQ0NDgwMCwibWFya2VyIjoiZnJlc2gifQ.c2lnbmF0dXJl";
const REFRESH_TOKEN: &str = "rt-refresh-secret-0123456789";
const ACCOUNT: &str = "acct_test";
const FAR_FUTURE_MS: i64 = 4_102_444_800_000;

pub(crate) struct Fixture {
    _directory: tempfile::TempDir,
    pub(crate) paths: ProfilePaths,
    pub(crate) workspace: PathBuf,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        Self {
            paths: ProfilePaths {
                config: root.join("config/oh-fx"),
                data: root.join("data/oh-fx"),
                state: root.join("state/oh-fx"),
                cache: root.join("cache/oh-fx"),
            },
            workspace: fs::canonicalize(workspace).unwrap(),
            _directory: directory,
        }
    }

    pub(crate) fn profile(&self, auth: &FakeServer, catalog: &FakeServer) -> Profile {
        Profile {
            paths: Some(self.paths.clone()),
            workspace: Ok(self.workspace.clone()),
            endpoints: SubscriptionEndpoints {
                chatgpt: ChatGptEndpoints {
                    issuer: auth.base_url(),
                    token_url: format!("{}/oauth/token", auth.base_url()),
                    callback_ports: vec![0],
                },
                codex: CodexEndpoints::default(),
                models: CodexModelsEndpoints {
                    models: format!("{}/backend-api/codex/models", catalog.base_url()),
                    client_version: format!("{}/@openai/codex/latest", catalog.base_url()),
                },
            },
            lookup: |_| None,
            grok: ofx_auth::GrokEndpoints::default(),
        }
    }

    pub(crate) fn settings_file(&self) -> PathBuf {
        self.paths.config.join("settings.json")
    }

    pub(crate) fn write_settings(&self, text: &str) {
        fs::create_dir_all(&self.paths.config).unwrap();
        fs::write(self.settings_file(), text).unwrap();
    }

    pub(crate) fn settings(&self) -> Option<String> {
        fs::read_to_string(self.settings_file()).ok()
    }

    pub(crate) fn credential_file(&self) -> PathBuf {
        self.paths.data.join("chatgpt-auth.json")
    }

    fn write_session(&self, expires_at_ms: i64, mode: u32) {
        fs::create_dir_all(&self.paths.data).unwrap();
        fs::set_permissions(&self.paths.data, fs::Permissions::from_mode(0o700)).unwrap();
        let session = json!({
            "version": 1,
            "access_token": ACCESS_TOKEN,
            "refresh_token": REFRESH_TOKEN,
            "expires_at_ms": expires_at_ms,
            "account_id": ACCOUNT,
        });
        fs::write(self.credential_file(), format!("{session}\n")).unwrap();
        fs::set_permissions(self.credential_file(), fs::Permissions::from_mode(mode)).unwrap();
    }

    pub(crate) fn signed_in(&self) {
        self.write_session(FAR_FUTURE_MS, 0o600);
    }
}

pub(crate) fn listed(slug: &str) -> Value {
    json!({"slug": slug, "visibility": "list", "supported_in_api": true})
}

pub(crate) fn release() -> Reply {
    Reply::status(200, "{\"version\":\"0.153.1\"}")
}

pub(crate) fn catalog(slugs: &[&str]) -> Reply {
    let models: Vec<Value> = slugs.iter().map(|slug| listed(slug)).collect();
    Reply::status(200, json!({ "models": models }).to_string())
}

async fn activate(profile: &Profile) -> Result<(), ActivationFailure> {
    activate_codex(profile, Caller::Login, false)
        .await
        .map(drop)
}

fn failure(text: &str) -> Result<(), ActivationFailure> {
    Err(ActivationFailure::Detail(text.to_owned()))
}

#[tokio::test]
async fn activation_saves_codex_with_the_first_catalog_model() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let auth = FakeServer::start([]);
    let catalog_server = FakeServer::start([
        release(),
        catalog(&["gpt-6.1-sol", "gpt-5.6-terra", "gpt-5.6-luna"]),
    ]);
    assert_eq!(
        activate(&fixture.profile(&auth, &catalog_server)).await,
        Ok(())
    );
    assert_eq!(
        fixture.settings().as_deref(),
        Some("{\"models\":{\"codex\":\"gpt-6.1-sol\"},\"provider\":\"codex\"}\n")
    );
    let requests = catalog_server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path, "/v1/@openai/codex/latest");
    assert_eq!(
        requests[1].path,
        "/v1/backend-api/codex/models?client_version=0.153.1"
    );
    assert_eq!(
        requests[1].header("authorization"),
        Some(format!("Bearer {ACCESS_TOKEN}").as_str())
    );
    assert_eq!(requests[1].header("chatgpt-account-id"), Some(ACCOUNT));
    assert!(auth.requests().is_empty());
    assert!(
        fixture
            .paths
            .cache
            .join("provider-versions/codex.json")
            .is_file()
    );
}

#[tokio::test]
async fn a_saved_model_stays_selected_while_the_catalog_lists_it() {
    for (saved, expected) in [
        (
            r#"{"theme":"dark","models":{"codex":"gpt-5.6-luna"}}"#,
            "{\"theme\":\"dark\",\"models\":{\"codex\":\"gpt-5.6-luna\"},\"provider\":\"codex\"}\n",
        ),
        (
            r#"{"provider":"portkey","codex_model":"gpt-5.6-sol","models":{"portkey":"m"},"providers":{"portkey":{"protocol":"openai-chat-completions","base_url":"https://portkey.example.com/v1","auth":{"type":"none"}}}}"#,
            "{\"provider\":\"codex\",\"models\":{\"portkey\":\"m\",\"codex\":\"gpt-6.1-sol\"},\"providers\":{\"portkey\":{\"protocol\":\"openai-chat-completions\",\"base_url\":\"https://portkey.example.com/v1\",\"auth\":{\"type\":\"none\"}}}}\n",
        ),
    ] {
        let fixture = Fixture::new();
        fixture.signed_in();
        fixture.write_settings(saved);
        let auth = FakeServer::start([]);
        let catalog_server = FakeServer::start([
            release(),
            catalog(&["gpt-6.1-sol", "gpt-5.6-terra", "gpt-5.6-luna"]),
        ]);
        assert_eq!(
            activate(&fixture.profile(&auth, &catalog_server)).await,
            Ok(())
        );
        assert_eq!(fixture.settings().as_deref(), Some(expected));
    }
}

#[tokio::test]
async fn catalog_failures_name_their_category_and_save_nothing() {
    for (replies, expected) in [
        (
            vec![release(), Reply::status(401, "{}")],
            "could not load the target model catalog (authentication)",
        ),
        (
            vec![release(), Reply::status(200, "{\"models\":{}}")],
            "could not load the target model catalog (malformed_response)",
        ),
        (
            vec![Reply::status(503, "")],
            "could not load the target model catalog (transport)",
        ),
        (
            vec![release(), catalog(&[])],
            "target model catalog is empty",
        ),
    ] {
        let fixture = Fixture::new();
        fixture.signed_in();
        fixture.write_settings("{\"theme\":\"dark\"}");
        let auth = FakeServer::start([]);
        let catalog_server = FakeServer::start(replies);
        assert_eq!(
            activate(&fixture.profile(&auth, &catalog_server)).await,
            failure(expected)
        );
        assert_eq!(fixture.settings().as_deref(), Some("{\"theme\":\"dark\"}"));
    }
}

#[tokio::test]
async fn missing_or_unsafe_logins_stop_before_the_catalog() {
    let auth = FakeServer::start([]);
    let catalog_server = FakeServer::start([]);
    let fixture = Fixture::new();
    assert_eq!(
        activate(&fixture.profile(&auth, &catalog_server)).await,
        failure("Codex credential is unavailable")
    );
    fixture.write_session(FAR_FUTURE_MS, 0o644);
    assert_eq!(
        activate(&fixture.profile(&auth, &catalog_server)).await,
        failure(
            "Codex subscription: Saved credential storage is unavailable. Check the saved credential, then retry."
        )
    );
    assert!(catalog_server.requests().is_empty());
    assert_eq!(fixture.settings(), None);
}

#[tokio::test]
async fn an_expiring_login_is_refreshed_before_the_catalog_request() {
    let fixture = Fixture::new();
    fixture.write_session(1, 0o600);
    let auth = FakeServer::start([Reply::status(
        200,
        json!({"access_token": ACCESS_TOKEN, "refresh_token": "rt-rotated-0123456789", "expires_in": 3600})
            .to_string(),
    )]);
    let catalog_server = FakeServer::start([release(), catalog(&["gpt-6.1-sol"])]);
    assert_eq!(
        activate(&fixture.profile(&auth, &catalog_server)).await,
        Ok(())
    );
    assert_eq!(auth.requests().len(), 1);
    assert_eq!(catalog_server.requests().len(), 2);
}

#[tokio::test]
async fn unusable_settings_stop_before_the_login_is_read() {
    let auth = FakeServer::start([]);
    let catalog_server = FakeServer::start([]);
    for settings in ["{\"provider\":", "{\"models\":{\"codex\":\" bad\"}}"] {
        let fixture = Fixture::new();
        fixture.signed_in();
        fixture.write_settings(settings);
        assert_eq!(
            activate(&fixture.profile(&auth, &catalog_server)).await,
            failure("could not load settings")
        );
        assert_eq!(fixture.settings().as_deref(), Some(settings));
    }
    let fixture = Fixture::new();
    fixture.signed_in();
    let mut profile = fixture.profile(&auth, &catalog_server);
    profile.lookup = |name| (name == "OH_FX_PROVIDER").then(|| "not a provider".to_owned());
    assert_eq!(activate(&profile).await, failure("could not load settings"));
    profile.paths = None;
    assert_eq!(activate(&profile).await, failure("could not load settings"));
    profile.workspace = Err(io::Error::from(io::ErrorKind::NotFound));
    assert_eq!(
        activate(&profile).await,
        Err(ActivationFailure::Fatal("WorkspaceUnavailable"))
    );
    assert!(catalog_server.requests().is_empty());
    assert!(auth.requests().is_empty());
}

#[tokio::test]
async fn an_unwritable_selection_fails_after_the_catalog() {
    let fixture = Fixture::new();
    fixture.signed_in();
    let outside = fixture.paths.config.parent().unwrap().join("dotfiles.json");
    fs::create_dir_all(&fixture.paths.config).unwrap();
    fs::write(&outside, "{}").unwrap();
    symlink(&outside, fixture.settings_file()).unwrap();
    let auth = FakeServer::start([]);
    let catalog_server = FakeServer::start([release(), catalog(&["gpt-6.1-sol"])]);
    assert_eq!(
        activate(&fixture.profile(&auth, &catalog_server)).await,
        failure("failed to save provider selection")
    );
    assert_eq!(fs::read_to_string(outside).unwrap(), "{}");
}

#[test]
fn the_saved_model_wins_only_when_the_catalog_lists_it() {
    let models = ["gpt-6.1-sol".to_owned(), "gpt-5.6-luna".to_owned()];
    assert_eq!(
        select_catalog_model(&models, Some("gpt-5.6-luna")),
        Some("gpt-5.6-luna")
    );
    assert_eq!(
        select_catalog_model(&models, Some("gpt-5.6-sol")),
        Some("gpt-6.1-sol")
    );
    assert_eq!(select_catalog_model(&models, None), Some("gpt-6.1-sol"));
    assert_eq!(select_catalog_model(&[], Some("gpt-6.1-sol")), None);
}

impl Fixture {
    pub(crate) fn grok_profile(&self, auth: &FakeServer, catalog: &FakeServer) -> Profile {
        let mut profile = self.profile(auth, catalog);
        profile.grok = ofx_auth::GrokEndpoints {
            issuer: auth.base_url(),
            token_url: format!("{}/oauth/token", auth.base_url()),
            userinfo_url: format!("{}/userinfo", auth.base_url()),
            revoke_url: format!("{}/revoke", auth.base_url()),
        };
        profile
    }

    pub(crate) fn grok_credential_file(&self) -> PathBuf {
        self.paths.data.join("grok-auth.json")
    }

    pub(crate) fn grok_signed_in(&self) {
        fs::create_dir_all(&self.paths.data).unwrap();
        fs::set_permissions(&self.paths.data, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(self.grok_credential_file(),json!({"version":1,"access_token":"grok-access-secret","refresh_token":"grok-refresh-secret","expires_at_ms":FAR_FUTURE_MS,"account_id":"grok-account"}).to_string()).unwrap();
        fs::set_permissions(
            self.grok_credential_file(),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
}
