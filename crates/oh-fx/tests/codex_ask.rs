use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use ofx_testkit::{FakeServer, Reply};
use serde_json::{Value, json};

const ACCESS_TOKEN: &str = "eyJhbGciOiJub25lIn0.c2F2ZWQtYWNjZXNz.c2lnbmF0dXJl";
const REFRESH_TOKEN: &str = "rt-refresh-secret-0123456789";
const MISSING_LOGIN: &str =
    "oh-fx ask: oh-fx needs a Codex subscription login for this model. Run oh-fx login codex.\n";

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn with_settings(settings: Option<&Value>) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = directory.path().to_owned();
        fs::create_dir_all(root.join("workspace")).expect("create the workspace");
        if let Some(settings) = settings {
            let config = root.join("config/oh-fx");
            fs::create_dir_all(&config).expect("create the config directory");
            fs::write(config.join("settings.json"), settings.to_string())
                .expect("write settings.json");
        }
        Self {
            _directory: directory,
            root,
        }
    }

    fn data(&self) -> PathBuf {
        self.root.join("data/oh-fx")
    }

    fn credential_file(&self) -> PathBuf {
        self.data().join("chatgpt-auth.json")
    }

    fn write_credentials(&self, mode: u32, expires_at_ms: i64) -> String {
        fs::create_dir_all(self.data()).expect("create the data directory");
        fs::set_permissions(self.data(), fs::Permissions::from_mode(0o700))
            .expect("make the data directory private");
        let session = format!(
            "{}\n",
            json!({
                "version": 1,
                "access_token": ACCESS_TOKEN,
                "refresh_token": REFRESH_TOKEN,
                "expires_at_ms": expires_at_ms,
                "account_id": "acct_test",
            })
        );
        fs::write(self.credential_file(), &session).expect("write credentials");
        fs::set_permissions(self.credential_file(), fs::Permissions::from_mode(mode))
            .expect("set the credential mode");
        session
    }

    fn ask<S: AsRef<OsStr>>(&self, args: &[S], environment: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(args)
            .current_dir(self.root.join("workspace"))
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .envs(environment.iter().copied())
            .stdin(Stdio::null())
            .output()
            .expect("run oh-fx")
    }
}

fn codex_settings() -> Value {
    json!({"provider": "codex", "models": {"codex": "gpt-5.4"}})
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn error_json(code: &str) -> String {
    format!(
        "{{\"output\":\"\",\"final_output\":\"\",\"exit_code\":1,\"model\":\"\",\"resolved_provider\":null,\"session_id\":\"\",\"steps\":0,\"tool_calls\":[],\"usage\":{{\"input_tokens\":null,\"output_tokens\":null}},\"error\":\"{code}\"}}\n"
    )
}

#[test]
fn ask_without_a_codex_login_asks_the_user_to_sign_in() {
    let home = Home::with_settings(Some(&codex_settings()));
    let output = home.ask(&["ask", "hello"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), MISSING_LOGIN);

    let json = home.ask(&["ask", "--json", "hello"], &[]);
    assert_eq!(json.status.code(), Some(1));
    assert_eq!(stdout(&json), error_json("MissingCredentials"));
    assert_eq!(stderr(&json), MISSING_LOGIN);
}

#[test]
fn the_environment_selects_codex_and_a_run_model() {
    let home = Home::with_settings(None);
    let unselected = home.ask(&["ask", "hello"], &[("OH_FX_PROVIDER", "codex")]);
    assert_eq!(unselected.status.code(), Some(1));
    assert_eq!(
        stderr(&unselected),
        "oh-fx ask: no Codex model is selected; run `oh-fx provider codex` to choose one, or set a model for this run with --model or OH_FX_MODEL\n"
    );
    let json = home.ask(&["ask", "--json", "hello"], &[("OH_FX_PROVIDER", "Codex")]);
    assert_eq!(stdout(&json), error_json("CodexModelNotSelected"));

    for (args, environment) in [
        (
            &["ask", "--model", "gpt-5.4", "hello"][..],
            &[("OH_FX_PROVIDER", "codex")][..],
        ),
        (
            &["ask", "hello"][..],
            &[("OH_FX_PROVIDER", "codex"), ("OH_FX_MODEL", "gpt-5.4")][..],
        ),
    ] {
        let output = home.ask(args, environment);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(stderr(&output), MISSING_LOGIN);
    }
}

#[test]
fn ask_refuses_a_codex_login_readable_by_others_without_showing_it() {
    let home = Home::with_settings(Some(&codex_settings()));
    let session = home.write_credentials(0o644, 4_102_444_800_000);
    let output = home.ask(&["ask", "hello"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        "oh-fx ask: Saved credential storage is unavailable. Check the saved credential, then retry.\n"
    );
    let json = home.ask(&["ask", "--json", "hello"], &[]);
    assert_eq!(stdout(&json), error_json("CredentialStorageUnavailable"));
    assert_eq!(stderr(&json), "");
    for shown in [&output, &json].map(|output| format!("{}{}", stdout(output), stderr(output))) {
        assert!(!shown.contains(ACCESS_TOKEN));
        assert!(!shown.contains(REFRESH_TOKEN));
    }
    assert_eq!(
        fs::read_to_string(home.credential_file()).expect("read credentials"),
        session
    );
}

#[test]
fn quiet_codex_runs_still_report_a_missing_login() {
    let home = Home::with_settings(Some(&codex_settings()));
    let output = home.ask(&["ask", "--quiet", "hello"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), MISSING_LOGIN);
}

#[test]
fn non_utf8_codex_models_fail_as_invalid_models_before_an_expired_login_is_refreshed() {
    let home = Home::with_settings(Some(&codex_settings()));
    let session = home.write_credentials(0o600, 1);
    let model = OsString::from_vec(b" m\xff ".to_vec());
    for json in [false, true] {
        let mut args = vec![
            OsString::from("ask"),
            OsString::from("--model"),
            model.clone(),
        ];
        if json {
            args.push(OsString::from("--json"));
        }
        args.push(OsString::from("hello"));
        let output = home.ask(&args, &[]);
        assert_eq!(output.status.code(), Some(1));
        if json {
            assert_eq!(stderr(&output), "");
            assert_eq!(
                stdout(&output),
                "{\"output\":\"\",\"final_output\":\"\",\"exit_code\":1,\"model\":[109,255],\"resolved_provider\":null,\"session_id\":\"\",\"steps\":0,\"tool_calls\":[],\"usage\":{\"input_tokens\":null,\"output_tokens\":null},\"error\":\"InvalidModel\"}\n"
            );
        } else {
            assert_eq!(stdout(&output), "");
            assert_eq!(stderr(&output), "oh-fx: InvalidModel\n");
        }
    }
    assert_eq!(
        fs::read_to_string(home.credential_file()).expect("read credentials"),
        session
    );
}

fn codex_servers(modalities: &[&str]) -> (FakeServer, FakeServer) {
    codex_model_servers(&json!({
        "slug": "gpt-5.4",
        "visibility": "list",
        "supported_in_api": true,
        "supported_reasoning_levels": [{"effort": "low"}],
        "input_modalities": modalities,
    }))
}

fn codex_model_servers(model: &Value) -> (FakeServer, FakeServer) {
    let events = [
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","phase":"final_answer"}}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":"a pixel"}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":20,"output_tokens":3}}}),
    ]
    .map(|event| event.to_string());
    let catalog = FakeServer::start([
        Reply::status(200, json!({"version": "0.153.1"}).to_string()),
        Reply::status(200, json!({"models": [model]}).to_string()),
    ]);
    (catalog, FakeServer::start([Reply::sse(&events)]))
}

fn ask_codex_with_image(home: &Home, catalog: &FakeServer, codex: &FakeServer) -> Output {
    ask_codex_with_image_bytes(home, catalog, codex, b"\x89PNG\r\n\x1a\nrest")
}

fn ask_codex_with_image_bytes(
    home: &Home,
    catalog: &FakeServer,
    codex: &FakeServer,
    bytes: &[u8],
) -> Output {
    fs::write(home.root.join("workspace/shot.png"), bytes).expect("write the image");
    let models = format!("{}/backend-api/codex/models", catalog.base_url());
    let version = format!("{}/@openai/codex/latest", catalog.base_url());
    let responses = format!("{}/backend-api/codex/responses", codex.base_url());
    home.ask(
        &["ask", "--no-save", "--image", "shot.png", "look"],
        &[
            ("OH_FX_E2E_OPENAI_CODEX_MODELS_URL", &models),
            ("OH_FX_E2E_CODEX_VERSION_URL", &version),
            ("OH_FX_E2E_OPENAI_CODEX_RESPONSES_URL", &responses),
        ],
    )
}

#[test]
fn codex_asks_send_unsaved_images_as_input_images() {
    let home = Home::with_settings(Some(&codex_settings()));
    home.write_credentials(0o600, i64::MAX);
    let (catalog, codex) = codex_servers(&["text", "image"]);

    let output = ask_codex_with_image(&home, &catalog, &codex);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "a pixel");
    let requests = codex.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].json()["input"],
        json!([{"role": "user", "content": [
            {"type": "input_text", "text": "look"},
            {"type": "input_image", "detail": "auto", "image_url": "data:image/png;base64,iVBORw0KGgpyZXN0"},
        ]}])
    );
}

#[test]
fn codex_asks_send_a_large_image_to_a_model_whose_context_window_is_known() {
    let home = Home::with_settings(Some(&codex_settings()));
    home.write_credentials(0o600, i64::MAX);
    let (catalog, codex) = codex_model_servers(&json!({
        "slug": "gpt-5.4",
        "visibility": "list",
        "supported_in_api": true,
        "supported_reasoning_levels": [{"effort": "low"}],
        "input_modalities": ["text", "image"],
        "context_window": 128_000,
    }));
    let mut image =
        b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\x03\xe8\0\0\x03\xe8\x08\x06\0\0\0\0\0\0\0".to_vec();
    image.resize(3 * 1024 * 1024, 0);

    let output = ask_codex_with_image_bytes(&home, &catalog, &codex, &image);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "a pixel");
    let requests = codex.requests();
    assert_eq!(requests.len(), 1);
    let url = requests[0].json()["input"][0]["content"][1]["image_url"]
        .as_str()
        .expect("the image part")
        .to_owned();
    let encoded = url
        .strip_prefix("data:image/png;base64,")
        .expect("a PNG data URL");
    assert_eq!(encoded.len(), image.len() / 3 * 4);
}

#[test]
fn codex_asks_refuse_images_for_a_model_without_image_input() {
    let home = Home::with_settings(Some(&codex_settings()));
    home.write_credentials(0o600, i64::MAX);
    let (catalog, codex) = codex_servers(&["text"]);

    let output = ask_codex_with_image(&home, &catalog, &codex);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "oh-fx: SubscriptionNativeImageUnavailable\n"
    );
    assert!(codex.requests().is_empty());
}
