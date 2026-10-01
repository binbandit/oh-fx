use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ofx_testkit::{FakeServer, Reply, chat_text_events};
use serde_json::{Value, json};

const PORTKEY_KEY: &str = "pk-test-0123456789";

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
}

impl Home {
    fn with_settings(settings: &Value) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = directory.path().to_owned();
        let config = root.join("config/oh-fx");
        let workspace = root.join("workspace");
        fs::create_dir_all(&config).expect("create the config directory");
        fs::create_dir_all(&workspace).expect("create the workspace");
        fs::write(config.join("settings.json"), settings.to_string()).expect("write settings.json");
        Self {
            _directory: directory,
            root,
            workspace,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .args(args)
            .current_dir(&self.workspace)
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("SHELL", "/bin/sh")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .stdin(Stdio::null());
        command
    }

    fn ask(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        let mut command = self.command(args);
        command.envs(environment.iter().copied());
        command.output().expect("run oh-fx")
    }
}

fn portkey_settings(base_url: &str) -> Value {
    json!({
        "provider": "portkey",
        "model": "@openai/gpt-4o",
        "providers": {
            "portkey": {
                "protocol": "openai-chat-completions",
                "base_url": base_url,
                "auth": {"type": "none"},
                "headers": {
                    "x-portkey-api-key": "${PORTKEY_API_KEY}",
                    "x-portkey-provider": "${PORTKEY_PROVIDER:-openai}"
                },
                "models": ["@openai/gpt-4o"]
            }
        }
    })
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn canonical(path: &Path) -> String {
    fs::canonicalize(path)
        .expect("canonicalize the workspace")
        .to_string_lossy()
        .into_owned()
}

#[test]
fn ask_streams_a_portkey_reply_with_the_configured_header_and_body() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Hello", " from Portkey."]))]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let output = home.ask(&["ask", "hello"], &[("PORTKEY_API_KEY", PORTKEY_KEY)]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Hello from Portkey.");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(request.header("x-portkey-api-key"), Some(PORTKEY_KEY));
    assert_eq!(request.header("x-portkey-provider"), Some("openai"));
    assert_eq!(request.header("authorization"), None);
    assert_eq!(request.header("accept"), Some("text/event-stream"));
    assert!(request.header("user-agent").unwrap().starts_with("oh-fx/"));
    let body = request.json();
    let fields: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(fields, ["model", "stream", "stream_options", "messages"]);
    assert_eq!(body["model"], "@openai/gpt-4o");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 5);
    let roles: Vec<&str> = messages
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "system", "system", "system", "user"]);
    assert!(
        messages[0]["content"]
            .as_str()
            .unwrap()
            .starts_with("# Identity and context\n\n- You are oh-fx,")
    );
    let turn_context = messages[1]["content"].as_str().unwrap();
    assert!(turn_context.starts_with(&format!(
        "<fx-turn-context>\nworkspace_root: {}\n",
        canonical(&home.workspace)
    )));
    assert!(turn_context.contains("\nshell_path: /bin/sh\n"));
    assert!(turn_context.ends_with("Do not recommend or label one option as preferred."));
    assert!(
        messages[2]["content"]
            .as_str()
            .unwrap()
            .starts_with("Runtime context: permission mode is auto.")
    );
    assert!(
        messages[3]["content"]
            .as_str()
            .unwrap()
            .starts_with("<response_language_control>")
    );
    assert_eq!(messages[4], json!({"role": "user", "content": "hello"}));
}

#[test]
fn ask_json_reports_the_upstream_result_shape() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Hi", " there"]))]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let output = home.ask(
        &["ask", "--json", "--model", "@anthropic/claude", "hello"],
        &[("PORTKEY_API_KEY", PORTKEY_KEY)],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "{\"output\":\"Hi there\",\"final_output\":\"Hi there\",\"exit_code\":0,\"model\":\"@anthropic/claude\",\"resolved_provider\":null,\"session_id\":\"\",\"steps\":0,\"tool_calls\":[],\"usage\":{\"input_tokens\":12,\"output_tokens\":3}}\n"
    );
    assert_eq!(server.requests()[0].json()["model"], "@anthropic/claude");
}

#[test]
fn ask_reads_the_prompt_from_stdin() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["ok"]))]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let mut child = home
        .command(&["ask"])
        .env("PORTKEY_API_KEY", PORTKEY_KEY)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"  piped prompt\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let body = server.requests()[0].json();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.last().unwrap()["content"], "piped prompt");
}

#[test]
fn missing_header_variables_fail_before_any_request() {
    let server = FakeServer::start([]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let output = home.ask(&["ask", "hello"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "oh-fx ask: header x-portkey-api-key needs the environment variable PORTKEY_API_KEY, which is not set; export it or give a default with ${PORTKEY_API_KEY:-value}\n"
    );
    assert!(server.requests().is_empty());
    let output = home.ask(&["ask", "--json", "hello"], &[]);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"], "MissingCredentials");
    assert_eq!(result["exit_code"], 1);
}

#[test]
fn unauthorized_replies_report_the_configured_provider_failure() {
    let body = format!(r#"{{"error":{{"message":"bad key {PORTKEY_KEY}"}}}}"#);
    let server = FakeServer::start([Reply::status(401, body.clone()), Reply::status(401, body)]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let output = home.ask(&["ask", "hello"], &[("PORTKEY_API_KEY", PORTKEY_KEY)]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "oh-fx ask: configured provider authentication failed · HTTP 401\noh-fx ask: API access denied · HTTP 401 · bad key ******************\n"
    );
    let output = home.ask(
        &["ask", "--json", "hello"],
        &[("PORTKEY_API_KEY", PORTKEY_KEY)],
    );
    assert_eq!(
        stdout(&output),
        "{\"output\":\"configured provider authentication failed · HTTP 401\\n\",\"final_output\":\"\",\"exit_code\":1,\"model\":\"@openai/gpt-4o\",\"resolved_provider\":null,\"session_id\":\"\",\"steps\":0,\"tool_calls\":[],\"usage\":{\"input_tokens\":null,\"output_tokens\":null},\"auth_failure\":{\"source\":\"configured provider\",\"reason\":\"http_unauthorized\",\"http_status\":401}}\n"
    );
}

#[test]
fn ask_usage_errors_follow_the_upstream_shapes() {
    let home = Home::with_settings(&json!({}));
    let output = home.ask(&["ask"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "oh-fx ask: missing prompt\nusage: oh-fx ask [--model <id>] [--json] [--] <prompt>\n"
    );
    let output = home.ask(&["ask", "--json", "--bogus"], &[]);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"], "InvalidAskArgs");
    let output = home.ask(&["ask", "--help"], &[]);
    assert!(output.status.success());
    assert!(stdout(&output).starts_with("oh-fx ask\n\nRun one noninteractive request\n"));
}

#[test]
fn unselected_and_unknown_providers_explain_themselves() {
    let home = Home::with_settings(&json!({}));
    let output = home.ask(&["ask", "hello"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output)
            .starts_with("oh-fx ask: the gateway provider is not available in oh-fx yet;")
    );
    let output = home.ask(&["ask", "hello"], &[("OH_FX_PROVIDER", "nope")]);
    assert_eq!(stderr(&output), "oh-fx: UnknownConfiguredProvider\n");
}

fn text_chunk(content: &str) -> String {
    json!({"id":"c1","object":"chat.completion.chunk","model":"m","choices":[{"index":0,"delta":{"content":content},"finish_reason":null}]}).to_string()
}

fn stream_of(texts: &[&str]) -> Vec<String> {
    let mut events: Vec<String> = texts.iter().map(|text| text_chunk(text)).collect();
    events.push(
        r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#.to_owned(),
    );
    events.push(r#"{"id":"c1","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}"#.to_owned());
    events.push("[DONE]".to_owned());
    events
}

#[test]
fn raw_output_keeps_leading_whitespace_and_json_normalizes_the_final_output() {
    let events = stream_of(&["\n\n  Hello", "\n\n**bold** `x`  \n\n"]);
    let server = FakeServer::start([Reply::sse(&events), Reply::sse(&events)]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let key = [("PORTKEY_API_KEY", PORTKEY_KEY)];
    let output = home.ask(&["ask", "hi"], &key);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "\n\n  Hello\n\n**bold** `x`  \n\n");
    let output = home.ask(&["ask", "--json", "hi"], &key);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["output"], "\n\n  Hello\n\n**bold** `x`  \n\n");
    assert_eq!(result["final_output"], "Hello\n\nbold x");
}

#[test]
fn whitespace_only_answers_report_done_without_a_newline() {
    let events = stream_of(&["  ", "\n"]);
    let server = FakeServer::start([Reply::sse(&events)]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let output = home.ask(&["ask", "hi"], &[("PORTKEY_API_KEY", PORTKEY_KEY)]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), "  \n");
    assert_eq!(stderr(&output), "Done.");
}

#[test]
fn a_json_looking_model_value_does_not_switch_the_output_mode() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["plain"]))]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let output = home.ask(
        &["ask", "--model", "--json", "hi"],
        &[("PORTKEY_API_KEY", PORTKEY_KEY)],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "plain");
    assert_eq!(server.requests()[0].json()["model"], "--json");
}

#[test]
fn empty_prompts_fail_like_upstream_before_any_request() {
    let server = FakeServer::start([]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let output = home.ask(&["ask", ""], &[("PORTKEY_API_KEY", PORTKEY_KEY)]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "oh-fx: InvalidConversationEvent\n");
    let output = home.ask(&["ask", "--json", ""], &[("PORTKEY_API_KEY", PORTKEY_KEY)]);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"], "InvalidConversationEvent");
    assert!(server.requests().is_empty());
}

#[test]
fn protocol_failures_name_the_error_and_show_a_masked_excerpt() {
    let events = vec![text_chunk("Hello"), format!("{{not json {PORTKEY_KEY}")];
    let server = FakeServer::start([Reply::sse(&events), Reply::sse(&events)]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let key = [("PORTKEY_API_KEY", PORTKEY_KEY)];
    let output = home.ask(&["ask", "hi"], &key);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "Hello");
    assert_eq!(
        stderr(&output),
        "oh-fx: InvalidChunk\noh-fx ask: stream event: {not json ******************\n"
    );
    let output = home.ask(&["ask", "--json", "hi"], &key);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"], "InvalidChunk");
    assert_eq!(result["output"], "Hello");
    assert_eq!(
        stderr(&output),
        "oh-fx ask: stream event: {not json ******************\n"
    );
}

#[test]
fn transient_failures_retry_with_upstream_notices_and_recovery_json() {
    let failure = r#"{"error":{"message":"boom"}}"#;
    let server = FakeServer::start([
        Reply::status(500, failure),
        Reply::sse(&chat_text_events(&["Hi"])),
        Reply::status(500, failure),
        Reply::sse(&chat_text_events(&["Hi"])),
    ]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let key = [("PORTKEY_API_KEY", PORTKEY_KEY)];
    let output = home.ask(&["ask", "hi"], &key);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Hi");
    let notice = "[notice] ⚠ Provider unavailable · HTTP 500 · boom · retrying request\n";
    assert_eq!(
        stderr(&output),
        format!("{notice}{notice}[notice] ✓ recovered · succeeded on attempt 2\n")
    );
    let output = home.ask(&["ask", "--json", "hi"], &key);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["recovery"],
        json!({"state":"recovered","kind":"auto_recovered","attempt":2,"attempt_limit":10,"delay_seconds":0,"durable":false,"message":"✓ recovered · succeeded on attempt 2"})
    );
    assert_eq!(server.requests().len(), 4);
}

#[test]
fn sign_in_redirects_are_not_followed_and_explain_the_base_url() {
    let identity_provider = FakeServer::start([]);
    let location = format!("{}/authorize", identity_provider.base_url());
    let gateway = FakeServer::start([Reply::status_with_headers(
        302,
        &[("Location", &location)],
        "",
    )]);
    let home = Home::with_settings(&portkey_settings(&gateway.base_url()));
    let output = home.ask(&["ask", "hi"], &[("PORTKEY_API_KEY", PORTKEY_KEY)]);
    assert_eq!(output.status.code(), Some(1));
    let host = identity_provider
        .base_url()
        .trim_end_matches("/v1")
        .to_owned();
    assert_eq!(
        stderr(&output),
        format!(
            "oh-fx ask: HTTP 302: redirect to {host} was not followed; base_url must point at the gateway API itself, not at a sign-in page or a proxy that redirects\n"
        )
    );
    assert!(identity_provider.requests().is_empty());
}

#[test]
fn config_diagnostics_print_only_for_usable_profiles_in_every_mode() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["ok"]))]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    fs::write(home.workspace.join(".oh-fx.json"), r#"{"provider":"x"}"#).unwrap();
    let output = home.ask(
        &["ask", "--json", "hi"],
        &[("PORTKEY_API_KEY", PORTKEY_KEY)],
    );
    assert!(output.status.success());
    assert_eq!(
        stderr(&output),
        "oh-fx ask: config project: ignored_project_user_only_setting; key=provider\n"
    );
    let mut broken = portkey_settings(&server.base_url());
    broken["max_agent_steps"] = json!("many");
    let home = Home::with_settings(&broken);
    for (args, expected) in [
        (&["ask", "hi"][..], "oh-fx: InvalidProfileConfiguration\n"),
        (&["ask", "--json", "hi"], ""),
    ] {
        let output = home.ask(args, &[("PORTKEY_API_KEY", PORTKEY_KEY)]);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(stderr(&output), expected);
    }
}

#[test]
fn missing_credentials_explain_themselves_in_json_mode_too() {
    let server = FakeServer::start([]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let output = home.ask(&["ask", "--json", "hello"], &[]);
    assert!(stderr(&output).starts_with("oh-fx ask: header x-portkey-api-key needs"));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"], "MissingCredentials");
}

fn closed_pipe() -> io::PipeWriter {
    let (reader, writer) = io::pipe().expect("create a pipe");
    drop(reader);
    writer
}

#[test]
fn closed_stdout_follows_upstream_exit_behavior() {
    let home = Home::with_settings(&json!({}));
    for (args, by_signal) in [
        (&["--version"][..], true),
        (&["help"], true),
        (&["ask", "--help"], false),
        (&["upgrade", "--help"], false),
    ] {
        let output = home
            .command(args)
            .stdout(closed_pipe())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        if by_signal {
            assert_eq!(output.status.signal(), Some(13), "{args:?}");
        } else {
            assert_eq!(output.status.code(), Some(1), "{args:?}");
            assert_eq!(stderr(&output), "oh-fx: WriteFailed\n", "{args:?}");
        }
    }
}

#[test]
fn closed_stderr_still_reports_the_json_envelope() {
    let server = FakeServer::start([Reply::status(400, r#"{"error":{"message":"bad req"}}"#)]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let output = home
        .command(&["ask", "--json", "hi"])
        .env("PORTKEY_API_KEY", PORTKEY_KEY)
        .stdout(Stdio::piped())
        .stderr(closed_pipe())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"], "BrokenPipe");
    assert_eq!(result["output"], "");
}

#[test]
fn a_closed_stdout_while_streaming_stops_the_request_and_reports_the_write_error() {
    let server = FakeServer::start([Reply::held_sse(&[text_chunk("Hello")])]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let mut child = home
        .command(&["ask", "hi"])
        .env("PORTKEY_API_KEY", PORTKEY_KEY)
        .stdout(closed_pipe())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("ask kept waiting after stdout closed");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let mut errors = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut errors)
        .unwrap();
    assert_eq!(status.code(), Some(1));
    assert_eq!(errors, "oh-fx: BrokenPipe\n");
}

#[test]
fn interrupts_flush_partial_output_and_end_the_process_by_signal() {
    for (signal, number) in [("-INT", 2), ("-TERM", 15)] {
        let server = FakeServer::start([Reply::held_sse(&[text_chunk("Hello")])]);
        let home = Home::with_settings(&portkey_settings(&server.base_url()));
        let mut child = home
            .command(&["ask", "hi"])
            .env("PORTKEY_API_KEY", PORTKEY_KEY)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut first = [0_u8; 5];
        stdout.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"Hello");
        let status = Command::new("kill")
            .args([signal, &child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(number));
    }
}
