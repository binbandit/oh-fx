use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ofx_testkit::{FakeServer, RecordedRequest, Reply, chat_text_events, chat_tool_call_events};
use serde_json::{Value, json};

const PORTKEY_KEY: &str = "pk-test-0123456789";
const UPSTREAM_READ_FILE_TOOL: &str = r#"{"type":"function","function":{"name":"read_file","description":"Read one file with bounded line-numbered output and optional start_line/line_count range. UTF-8 text returns as numbered lines; image files (PNG, JPEG, GIF, WebP up to 3.9MB) attach to the result so you can see them. Paths may be workspace-relative or external using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy. When to use: inspect an exact known path before editing or explaining code, or view an image file. When NOT to use: list directories, search many files, read non-image binary data, or bypass dedicated search tools.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"File path relative to the workspace root, or an external path using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy."},"start_line":{"type":"integer","description":"Optional 1-based first line to return. Defaults to 1."},"line_count":{"type":"integer","description":"Optional positive number of lines to return. Defaults to the normal read cap and is bounded."}},"required":["path"]}}}"#;
const ASK_USAGE: &str = "usage: oh-fx ask [--auto|--full-access] [--model <id>] [--effort <level>] [--fast|--no-fast] [--provider-order <a,b,...>] [--provider-strict|--no-provider-strict] [--image PATH] [--system TEXT] [--json] [--quiet] [--prompt-permissions] [--no-save] [--no-color] [--resume <last|id>|--resume-id <id>] [--continue-recovery] [--] <prompt>\n";
const KEY: [(&str, &str); 1] = [("PORTKEY_API_KEY", PORTKEY_KEY)];
const UPSTREAM_GLOB_FILES_TOOL: &str = r#"{"type":"function","function":{"name":"glob_files","description":"Find file paths matching a glob pattern, with mode=count for exact path counts without listing entries. Paths may be workspace-relative or external using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy. When to use: locate files by name, extension, or directory pattern; narrow path or pattern if candidate caps appear. When NOT to use: search file contents, read files, run find, or count non-file concepts.","parameters":{"type":"object","properties":{"pattern":{"type":"string","description":"Glob pattern to match, such as src/**/*.zig or *.md."},"path":{"type":"string","minLength":1,"description":"Optional search root relative to the workspace root, or an external path using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy. Omit this field to use the current directory; never send an empty string. Narrow it when possible."},"mode":{"type":"string","enum":["matches","count"],"description":"Use matches to return sample paths, or count to return an exact matching path count without listing entries."}},"required":["pattern"]}}}"#;
const UPSTREAM_GREP_FILES_TOOL: &str = r#"{"type":"function","function":{"name":"grep_files","description":"Search text files for a literal substring, optionally narrowed by path/include, with output modes for matching lines, files-with-matches, or counts plus head_limit/offset pagination and bounded context_lines for matches mode. Paths may be workspace-relative or external using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy. Use include as the type/path filter, such as *.zig. When to use: find exact symbols, strings, TODOs, or usage sites. When NOT to use: regex is not supported; avoid unknown-concept exploration, filename lookup, known-path reads, and shell grep; do not repeat the same or equivalent search after a caller search only finds a definition.","parameters":{"type":"object","properties":{"pattern":{"type":"string","description":"Literal plain-text pattern to search for."},"path":{"type":"string","minLength":1,"description":"Optional search root relative to the workspace root, or an external path using an absolute path, ~/..., or a relative workspace escape such as ../...; external access is subject to permission policy. Omit this field to use the current directory; never send an empty string. Narrow it when possible."},"include":{"type":"string","description":"Optional glob pattern applied to candidate file paths before reading files, such as *.zig or src/**/*.ts."},"case_insensitive":{"type":"boolean","description":"Search case-insensitively when true."},"mode":{"type":"string","enum":["matches","files_with_matches","count"],"description":"Use matches for line matches, files_with_matches for unique matching paths, or count for exact matching-line and matching-file counts."},"head_limit":{"type":"integer","description":"Optional positive maximum results to return for matches or files_with_matches. Defaults to the normal output cap."},"offset":{"type":"integer","description":"Optional zero-based result offset for matches or files_with_matches pagination. Defaults to 0."},"context_lines":{"type":"integer","description":"Optional non-negative number of lines before and after each emitted match in matches mode. Bounded by the tool."}},"required":["pattern"]}}}"#;

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

    fn command<S: AsRef<OsStr>>(&self, args: &[S]) -> Command {
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

    fn ask<S: AsRef<OsStr>>(&self, args: &[S], environment: &[(&str, &str)]) -> Output {
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
    assert_eq!(
        fields,
        ["model", "stream", "stream_options", "messages", "tools"]
    );
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
        format!("oh-fx ask: missing prompt\n{ASK_USAGE}")
    );
    let output = home.ask(&["ask", "--bogus", "hi"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), ASK_USAGE);
    let output = home.ask(&["ask", "--json", "--bogus"], &[]);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"], "InvalidAskArgs");
    assert_eq!(stderr(&output), "");
    let output = home.ask(&["ask", "--no-save", "--resume", "last", "hi"], &[]);
    assert_eq!(
        stderr(&output),
        format!("oh-fx ask: --no-save cannot be used with --resume or --resume-id\n{ASK_USAGE}")
    );
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

fn messages(request: &RecordedRequest) -> Vec<Value> {
    request.json()["messages"]
        .as_array()
        .expect("the request carries messages")
        .clone()
}

fn system_texts(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message["role"] == "system")
        .map(|message| {
            message["content"]
                .as_str()
                .expect("system messages carry text")
                .to_owned()
        })
        .collect()
}

#[test]
fn permission_flags_override_the_configured_mode_for_one_request() {
    let replies: Vec<Reply> = (0..4)
        .map(|_| Reply::sse(&chat_text_events(&["ok"])))
        .collect();
    let server = FakeServer::start(replies);
    let mut settings = portkey_settings(&server.base_url());
    settings["permission_mode"] = json!("ask");
    let home = Home::with_settings(&settings);
    let warning = "Full access enabled: oh-fx permission checks disabled\n";
    for (flags, expected_stderr) in [
        (&[][..], ""),
        (&["--auto"], ""),
        (&["--full-access"], warning),
        (&["--yolo", "--no-color"], warning),
    ] {
        let args = [&["ask"], flags, &["hi"]].concat();
        let output = home.ask(&args, &KEY);
        assert!(output.status.success(), "{flags:?}: {}", stderr(&output));
        assert_eq!(stdout(&output), "ok", "{flags:?}");
        assert_eq!(stderr(&output), expected_stderr, "{flags:?}");
    }
    let modes: Vec<String> = server
        .requests()
        .iter()
        .map(|request| system_texts(&messages(request))[2].clone())
        .collect();
    for (mode, expected) in modes
        .iter()
        .zip(["ask", "auto", "full access", "full access"])
    {
        assert!(
            mode.starts_with(&format!("Runtime context: permission mode is {expected}.")),
            "{mode}"
        );
    }
}

#[test]
fn system_flag_replaces_only_the_base_prompt() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["ok"])),
        Reply::sse(&chat_text_events(&["ok"])),
    ]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    for system in ["Answer in one word.", ""] {
        let output = home.ask(&["ask", "--system", system, "hi"], &KEY);
        assert!(output.status.success(), "{}", stderr(&output));
    }
    let requests = server.requests();
    let replaced = system_texts(&messages(&requests[0]));
    assert_eq!(replaced.len(), 4);
    assert_eq!(replaced[0], "Answer in one word.");
    assert!(replaced[1].starts_with("<fx-turn-context>\n"));
    assert!(replaced[2].starts_with("Runtime context: permission mode is auto."));
    assert!(replaced[3].starts_with("<response_language_control>"));
    assert_eq!(system_texts(&messages(&requests[1])), replaced[1..]);
    assert_eq!(
        messages(&requests[1]).last().unwrap(),
        &json!({"role": "user", "content": "hi"})
    );
}

#[test]
fn quiet_suppresses_assistant_output_and_retry_notices_but_not_json() {
    let failure = r#"{"error":{"message":"boom"}}"#;
    let server = FakeServer::start([
        Reply::status(500, failure),
        Reply::sse(&chat_text_events(&["Hi"])),
        Reply::sse(&chat_text_events(&["Hi"])),
    ]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let output = home.ask(&["ask", "--quiet", "hi"], &KEY);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "");
    let output = home.ask(&["ask", "--quiet", "--json", "hi"], &KEY);
    assert!(output.status.success(), "{}", stderr(&output));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["output"], "Hi");
    assert_eq!(result["final_output"], "Hi");
}

#[test]
fn quiet_failures_still_report_on_stderr() {
    let server = FakeServer::start([Reply::status(401, r#"{"error":{"message":"no"}}"#)]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let output = home.ask(&["ask", "--quiet", "hi"], &KEY);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output)
            .starts_with("oh-fx ask: configured provider authentication failed · HTTP 401\n")
    );
}

#[test]
fn flags_that_request_the_current_defaults_run_normally() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["ok"]))]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let output = home.ask(
        &[
            "ask",
            "--effort",
            "auto",
            "--no-fast",
            "--no-provider-strict",
            "--no-save",
            "--verbose",
            "--timeout",
            "never",
            "hi",
        ],
        &KEY,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "ok");
    let body = server.requests()[0].json();
    let fields: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        fields,
        ["model", "stream", "stream_options", "messages", "tools"]
    );
}

#[test]
fn ask_flags_the_binary_cannot_honor_yet_fail_before_any_request() {
    let server = FakeServer::start([]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    for (args, feature) in [
        (&["ask", "--image", "shot.png", "hi"][..], "ask --image"),
        (
            &["ask", "--prompt-permissions", "hi"],
            "ask --prompt-permissions",
        ),
        (&["ask", "--timeout", "5", "hi"], "ask --timeout"),
        (&["ask", "--resume", "last", "hi"], "ask --resume"),
        (
            &["ask", "--resume-id", "session.v3", "hi"],
            "ask --resume-id",
        ),
        (
            &["ask", "--resume", "last", "--continue-recovery"],
            "ask --continue-recovery",
        ),
        (&["--add-dir", "/tmp", "ask", "--fast", "hi"], "--add-dir"),
    ] {
        let output = home.ask(args, &KEY);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stdout(&output), "", "{args:?}");
        assert_eq!(
            stderr(&output),
            format!("oh-fx: {feature} is not available yet\n"),
            "{args:?}"
        );
    }
    let output = home.ask(&["ask", "--json", "--image", "shot.png", "hi"], &KEY);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "oh-fx: ask --image is not available yet\n");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"], "NotAvailableYet");
    assert_eq!(result["exit_code"], 1);
    assert!(server.requests().is_empty());
}

#[test]
fn model_routing_flags_are_validated_and_leave_the_custom_connection_request_unchanged() {
    let cases: [&[&str]; 6] = [
        &[],
        &["--effort", "high"],
        &["--fast"],
        &["--provider-order", "azure,anthropic"],
        &["--provider-order=bedrock", "--provider-strict"],
        &["--effort", "xhigh", "--fast", "--provider-strict"],
    ];
    let replies: Vec<Reply> = cases
        .iter()
        .map(|_| Reply::sse(&chat_text_events(&["ok"])))
        .collect();
    let server = FakeServer::start(replies);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    for flags in cases {
        let args = [&["ask"], flags, &["hi"]].concat();
        let output = home.ask(&args, &KEY);
        assert!(output.status.success(), "{flags:?}: {}", stderr(&output));
        assert_eq!(stdout(&output), "ok", "{flags:?}");
        assert_eq!(stderr(&output), "", "{flags:?}");
    }
    let requests = server.requests();
    assert_eq!(requests.len(), cases.len());
    for request in &requests[1..] {
        assert_eq!(request.body, requests[0].body);
    }
    for args in [
        &["ask", "--effort", "not an effort", "hi"][..],
        &["ask", "--provider-order", "Bad Slug", "hi"],
        &["ask", "--fast", "--no-fast", "hi"],
        &["ask", "--provider-strict", "--no-provider-strict", "hi"],
    ] {
        let output = home.ask(args, &KEY);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stderr(&output), ASK_USAGE, "{args:?}");
    }
    assert_eq!(server.requests().len(), cases.len());
}

#[test]
fn non_utf8_models_fail_as_invalid_models_before_any_request() {
    let server = FakeServer::start([]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let model = OsString::from_vec(b" m\xff ".to_vec());
    let args = |json: bool| {
        let mut args = vec![
            OsString::from("ask"),
            OsString::from("--model"),
            model.clone(),
        ];
        if json {
            args.push(OsString::from("--json"));
        }
        args.push(OsString::from("hi"));
        args
    };
    let output = home.ask(&args(false), &KEY);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "oh-fx: InvalidModel\n");
    let output = home.ask(&args(true), &KEY);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "");
    assert_eq!(
        stdout(&output),
        "{\"output\":\"\",\"final_output\":\"\",\"exit_code\":1,\"model\":[109,255],\"resolved_provider\":null,\"session_id\":\"\",\"steps\":0,\"tool_calls\":[],\"usage\":{\"input_tokens\":null,\"output_tokens\":null},\"error\":\"InvalidModel\"}\n"
    );
    assert!(server.requests().is_empty());
}

#[test]
fn non_utf8_system_prompts_fail_as_invalid_arguments_before_any_request() {
    let server = FakeServer::start([]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    let args = |json: bool| {
        let mut args = vec![OsString::from("ask")];
        if json {
            args.push(OsString::from("--json"));
        }
        args.extend([
            OsString::from("--system"),
            OsString::from_vec(b"s\xff".to_vec()),
            OsString::from("hi"),
        ]);
        args
    };
    let output = home.ask(&args(false), &KEY);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), ASK_USAGE);
    let output = home.ask(&args(true), &KEY);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"], "InvalidAskArgs");
    assert!(server.requests().is_empty());
}

#[test]
#[cfg(target_os = "linux")]
fn json_results_name_a_full_disk_like_upstream() {
    let home = Home::with_settings(&json!({}));
    let full = fs::File::create("/dev/full").expect("open /dev/full");
    let output = home
        .command(&["ask", "--json", "--bogus"])
        .stdout(full)
        .output()
        .expect("run oh-fx");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "oh-fx: NoSpaceLeft\n");
}

#[test]
fn json_results_name_a_closed_pipe_like_upstream() {
    let home = Home::with_settings(&json!({}));
    let (reader, writer) = io::pipe().expect("create a pipe");
    drop(reader);
    let output = home
        .command(&["ask", "--json", "--bogus"])
        .stdout(writer)
        .output()
        .expect("run oh-fx");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "oh-fx: BrokenPipe\n");
}

fn tool_messages(request: &RecordedRequest) -> Vec<Value> {
    request.json()["messages"]
        .as_array()
        .expect("the request carries messages")
        .iter()
        .filter(|message| message["role"] == "tool")
        .cloned()
        .collect()
}

#[test]
fn ask_runs_read_file_and_sends_its_result_to_the_model() {
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "read_file",
            r#"{"path":"notes.txt","start_line":2}"#,
        )),
        Reply::sse(&chat_tool_call_events(
            "call_2",
            "read_file",
            r#"{"path":"missing.txt"}"#,
        )),
        Reply::sse(&chat_text_events(&["The second line is beta."])),
    ]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    fs::write(home.workspace.join("notes.txt"), "alpha\nbeta\n").unwrap();
    let output = home.ask(
        &["ask", "--json", "what is on line two?"],
        &[("PORTKEY_API_KEY", PORTKEY_KEY)],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stderr(&output), "Reading notes.txt\nReading missing.txt\n");
    assert_eq!(
        stdout(&output),
        "{\"output\":\"The second line is beta.\",\"final_output\":\"The second line is beta.\",\"exit_code\":0,\"model\":\"@openai/gpt-4o\",\"resolved_provider\":null,\"session_id\":\"\",\"steps\":2,\"tool_calls\":[{\"name\":\"read_file\",\"status\":\"success\"},{\"name\":\"read_file\",\"status\":\"error\"}],\"usage\":{\"input_tokens\":36,\"output_tokens\":9}}\n"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert!(
            request
                .body_text()
                .contains(&format!(",\"tools\":[{UPSTREAM_READ_FILE_TOOL},{UPSTREAM_GLOB_FILES_TOOL},{UPSTREAM_GREP_FILES_TOOL}]")),
            "{}",
            request.body_text()
        );
        let roles: Vec<&str> = request.json()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "system")
            .map(|_| "system")
            .collect();
        assert_eq!(roles.len(), 4);
    }
    assert_eq!(
        tool_messages(&requests[2]),
        [
            json!({
                "role": "tool",
                "content": "<path>notes.txt</path>\n<content>\n2\tbeta\n... [showing 1 of 2 lines; use start_line/line_count to read more.]\n</content>",
                "tool_call_id": "call_1",
            }),
            json!({
                "role": "tool",
                "content": "Path not found: missing.txt",
                "tool_call_id": "call_2",
            }),
        ]
    );
}

fn parallel_tool_call_events(calls: &[(&str, &str, &str)]) -> Vec<String> {
    let chunk = |delta: Value, finish_reason: Value| {
        json!({
            "id": "chatcmpl-parallel",
            "object": "chat.completion.chunk",
            "model": "testkit-model",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        })
        .to_string()
    };
    let tool_calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (call_id, name, arguments))| {
            json!({
                "index": index,
                "id": call_id,
                "type": "function",
                "function": {"name": name, "arguments": arguments},
            })
        })
        .collect();
    vec![
        chunk(
            json!({"role": "assistant", "tool_calls": tool_calls}),
            Value::Null,
        ),
        chunk(json!({}), json!("tool_calls")),
        "[DONE]".to_owned(),
    ]
}

#[test]
fn ask_shows_invalid_parallel_read_file_calls_after_the_calls_that_run() {
    let server = FakeServer::start([
        Reply::sse(&parallel_tool_call_events(&[
            ("call_1", "read_file", r#"{"path":"a.txt","start_line":0}"#),
            ("call_2", "read_file", r#"{"path":"b.txt"}"#),
            ("call_3", "read_file", r#"{"path":"missing.txt"}"#),
        ])),
        Reply::sse(&chat_text_events(&["Read b."])),
    ]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    fs::write(home.workspace.join("a.txt"), "alpha\n").unwrap();
    fs::write(home.workspace.join("b.txt"), "beta\n").unwrap();
    let output = home.ask(
        &["ask", "--json", "read the files"],
        &[("PORTKEY_API_KEY", PORTKEY_KEY)],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stderr(&output),
        "Reading b.txt\nReading missing.txt\nReading a.txt\n"
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["steps"], 3);
    assert_eq!(
        result["tool_calls"],
        json!([
            {"name": "read_file", "status": "error"},
            {"name": "read_file", "status": "success"},
            {"name": "read_file", "status": "error"},
        ])
    );
    assert_eq!(
        tool_messages(&server.requests()[1]),
        [
            json!({
                "role": "tool",
                "content": "read_file field \"start_line\" must be a positive integer",
                "tool_call_id": "call_1",
            }),
            json!({
                "role": "tool",
                "content": "<path>b.txt</path>\n<content>\n1\tbeta\n</content>",
                "tool_call_id": "call_2",
            }),
            json!({
                "role": "tool",
                "content": "Path not found: missing.txt",
                "tool_call_id": "call_3",
            }),
        ]
    );
}

#[test]
fn ask_prints_read_file_progress_and_the_final_answer_in_raw_mode() {
    let read = chat_tool_call_events("call_1", "read_file", r#"{"path":"notes.txt"}"#);
    let server = FakeServer::start([
        Reply::sse(&read),
        Reply::sse(&chat_text_events(&["It says alpha."])),
        Reply::sse(&read),
        Reply::sse(&chat_text_events(&["It says alpha."])),
    ]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    fs::write(home.workspace.join("notes.txt"), "alpha\n").unwrap();
    let output = home.ask(
        &["ask", "read notes.txt"],
        &[("PORTKEY_API_KEY", PORTKEY_KEY)],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "It says alpha.");
    assert_eq!(stderr(&output), "Reading notes.txt\n");
    assert_eq!(
        tool_messages(&server.requests()[1]),
        [json!({
            "role": "tool",
            "content": "<path>notes.txt</path>\n<content>\n1\talpha\n</content>",
            "tool_call_id": "call_1",
        })]
    );

    let quiet = home.ask(&["ask", "--quiet", "read notes.txt"], &KEY);
    assert!(quiet.status.success(), "{}", stderr(&quiet));
    assert_eq!(stdout(&quiet), "");
    assert_eq!(stderr(&quiet), "Reading notes.txt\n");
}

fn settings_in_mode(base_url: &str, mode: &str) -> Value {
    let mut settings = portkey_settings(base_url);
    settings["permission_mode"] = json!(mode);
    settings["yolo_acknowledged"] = json!(true);
    settings
}

struct OutsideFile {
    _directory: tempfile::TempDir,
    path: String,
}

impl OutsideFile {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create a directory outside the workspace");
        let path = directory.path().join("secret.txt");
        fs::write(&path, "outside secret\n").expect("write the outside file");
        Self {
            path: canonical(&path),
            _directory: directory,
        }
    }

    fn read_call(&self) -> String {
        json!({ "path": self.path }).to_string()
    }
}

fn blocked_read_stderr(path: &str, hint: &str) -> String {
    format!(
        "Reading {path}\noh-fx ask: permission required for tool execution in noninteractive mode\noh-fx ask: blocked action: Reading {path}\noh-fx ask: reason=noninteractive_permission_prompt_unavailable\noh-fx ask: {hint}\n"
    )
}

fn never_sees_the_secret(server: &FakeServer) {
    assert!(
        server
            .requests()
            .iter()
            .all(|request| !request.body_text().contains("outside secret"))
    );
}

#[test]
fn ask_mode_fails_the_run_instead_of_reading_an_external_path() {
    let outside = OutsideFile::new();
    let read = chat_tool_call_events("call_1", "read_file", &outside.read_call());
    let server = FakeServer::start([
        Reply::sse(&read),
        Reply::sse(&read),
        Reply::sse(&chat_text_events(&["never"])),
    ]);
    let home = Home::with_settings(&settings_in_mode(&server.base_url(), "ask"));
    let hint = "rerun with --auto to review this exact action automatically, or use the interactive shell to approve it";

    let raw = home.ask(&["ask", "read it"], &[("PORTKEY_API_KEY", PORTKEY_KEY)]);
    assert_eq!(raw.status.code(), Some(1));
    assert_eq!(stdout(&raw), "");
    assert_eq!(stderr(&raw), blocked_read_stderr(&outside.path, hint));

    let json = home.ask(
        &["ask", "--json", "read it"],
        &[("PORTKEY_API_KEY", PORTKEY_KEY)],
    );
    assert_eq!(json.status.code(), Some(1));
    assert_eq!(stderr(&json), blocked_read_stderr(&outside.path, hint));
    assert_eq!(
        stdout(&json),
        "{\"output\":\"\",\"final_output\":\"\",\"exit_code\":1,\"model\":\"\",\"resolved_provider\":null,\"session_id\":\"\",\"steps\":0,\"tool_calls\":[{\"name\":\"read_file\",\"status\":\"error\"}],\"usage\":{\"input_tokens\":12,\"output_tokens\":3},\"error\":\"NonInteractivePermissionRequired\"}\n"
    );
    assert_eq!(server.requests().len(), 2);
    never_sees_the_secret(&server);
}

#[test]
fn auto_mode_runs_earlier_calls_then_fails_the_run_on_an_external_read() {
    let outside = OutsideFile::new();
    let server = FakeServer::start([
        Reply::sse(&parallel_tool_call_events(&[
            ("call_1", "read_file", r#"{"path":"notes.txt"}"#),
            ("call_2", "read_file", &outside.read_call()),
            ("call_3", "read_file", r#"{"path":"notes.txt"}"#),
        ])),
        Reply::sse(&chat_text_events(&["never"])),
    ]);
    let home = Home::with_settings(&settings_in_mode(&server.base_url(), "auto"));
    fs::write(home.workspace.join("notes.txt"), "alpha\n").unwrap();

    let output = home.ask(
        &["ask", "--json", "read them"],
        &[("PORTKEY_API_KEY", PORTKEY_KEY)],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        format!(
            "Reading notes.txt\n{}",
            blocked_read_stderr(
                &outside.path,
                "human approval is required for this action; use the interactive shell to approve it, or add a narrow matching permission rule"
            )
        )
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"], "NonInteractivePermissionRequired");
    assert_eq!(result["exit_code"], 1);
    assert_eq!(
        result["tool_calls"],
        json!([
            {"name": "read_file", "status": "success"},
            {"name": "read_file", "status": "error"},
        ])
    );
    assert_eq!(server.requests().len(), 1);
    never_sees_the_secret(&server);
}

#[test]
fn full_access_reads_an_external_path() {
    let outside = OutsideFile::new();
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "read_file",
            &outside.read_call(),
        )),
        Reply::sse(&chat_text_events(&["It is a secret."])),
    ]);
    let home = Home::with_settings(&settings_in_mode(&server.base_url(), "yolo"));

    let output = home.ask(&["ask", "read it"], &[("PORTKEY_API_KEY", PORTKEY_KEY)]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "It is a secret.");
    assert_eq!(stderr(&output), format!("Reading {}\n", outside.path));
    assert_eq!(
        tool_messages(&server.requests()[1]),
        [json!({
            "role": "tool",
            "content": format!("<path>{}</path>\n<content>\n1\toutside secret\n</content>", outside.path),
            "tool_call_id": "call_1",
        })]
    );
}

#[test]
fn ask_mode_reads_workspace_files_without_approval() {
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "read_file",
            r#"{"path":"notes.txt"}"#,
        )),
        Reply::sse(&chat_text_events(&["It says alpha."])),
    ]);
    let home = Home::with_settings(&settings_in_mode(&server.base_url(), "ask"));
    fs::write(home.workspace.join("notes.txt"), "alpha\n").unwrap();

    let output = home.ask(
        &["ask", "--json", "read notes.txt"],
        &[("PORTKEY_API_KEY", PORTKEY_KEY)],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stderr(&output), "Reading notes.txt\n");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["final_output"], "It says alpha.");
    assert_eq!(
        result["tool_calls"],
        json!([{"name": "read_file", "status": "success"}])
    );
    assert_eq!(
        tool_messages(&server.requests()[1]),
        [json!({
            "role": "tool",
            "content": "<path>notes.txt</path>\n<content>\n1\talpha\n</content>",
            "tool_call_id": "call_1",
        })]
    );
}

#[test]
fn permission_flags_decide_whether_an_external_read_needs_approval() {
    let outside = OutsideFile::new();
    let read = chat_tool_call_events("call_1", "read_file", &outside.read_call());
    let server = FakeServer::start([
        Reply::sse(&read),
        Reply::sse(&chat_text_events(&["It is a secret."])),
        Reply::sse(&read),
        Reply::sse(&read),
    ]);
    let home = Home::with_settings(&settings_in_mode(&server.base_url(), "ask"));

    let full_access = home.ask(&["ask", "--full-access", "read it"], &KEY);
    assert!(full_access.status.success(), "{}", stderr(&full_access));
    assert_eq!(stdout(&full_access), "It is a secret.");

    let home = Home::with_settings(&settings_in_mode(&server.base_url(), "yolo"));
    let auto = home.ask(&["ask", "--auto", "read it"], &KEY);
    assert_eq!(auto.status.code(), Some(1));
    assert_eq!(
        stderr(&auto),
        blocked_read_stderr(
            &outside.path,
            "human approval is required for this action; use the interactive shell to approve it, or add a narrow matching permission rule"
        )
    );

    let quiet = home.ask(&["ask", "--auto", "--quiet", "read it"], &KEY);
    assert_eq!(quiet.status.code(), Some(1));
    assert_eq!(stdout(&quiet), "");
    assert_eq!(stderr(&quiet), stderr(&auto));
    assert_eq!(server.requests().len(), 4);
    assert!(
        server.requests()[2..]
            .iter()
            .all(|request| !request.body_text().contains("outside secret"))
    );
}

#[test]
fn ask_runs_glob_files_and_grep_files_and_sends_their_results_to_the_model() {
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "glob_files",
            r#"{"pattern":"**/*.rs"}"#,
        )),
        Reply::sse(&chat_tool_call_events(
            "call_2",
            "grep_files",
            r#"{"pattern":"needle","context_lines":1}"#,
        )),
        Reply::sse(&chat_tool_call_events(
            "call_3",
            "grep_files",
            r#"{"pattern":"needle","path":"nope"}"#,
        )),
        Reply::sse(&chat_text_events(&["Found one needle."])),
    ]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    fs::create_dir_all(home.workspace.join("src/nested")).unwrap();
    fs::write(
        home.workspace.join("src/lib.rs"),
        "fn a() {}\nlet needle = 1;\nfn b() {}\n",
    )
    .unwrap();
    fs::write(home.workspace.join("src/nested/mod.rs"), "pub fn c() {}\n").unwrap();
    fs::write(home.workspace.join("notes.md"), "no match here\n").unwrap();
    let output = home.ask(
        &["ask", "--json", "where is the needle?"],
        &[("PORTKEY_API_KEY", PORTKEY_KEY)],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stderr(&output),
        "Matching **/*.rs\nSearching needle\nSearching needle\n"
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["final_output"], "Found one needle.");
    assert_eq!(result["steps"], 3);
    assert_eq!(
        result["tool_calls"],
        json!([
            {"name": "glob_files", "status": "success"},
            {"name": "grep_files", "status": "success"},
            {"name": "grep_files", "status": "error"},
        ])
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        tool_messages(&requests[3]),
        [
            json!({
                "role": "tool",
                "content": "[glob] 2 matches for **/*.rs\n - src/lib.rs\n - src/nested/mod.rs\n",
                "tool_call_id": "call_1",
            }),
            json!({
                "role": "tool",
                "content": "[grep] 1 matches for needle\n   src/lib.rs:1- fn a() {}\n - src/lib.rs:2: let needle = 1;\n   src/lib.rs:3- fn b() {}\n",
                "tool_call_id": "call_2",
            }),
            json!({
                "role": "tool",
                "content": "Path not found: nope",
                "tool_call_id": "call_3",
            }),
        ]
    );
}

#[test]
fn file_searches_ignore_hostile_git_config() {
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "grep_files",
            r#"{"pattern":"needle"}"#,
        )),
        Reply::sse(&chat_tool_call_events(
            "call_2",
            "grep_files",
            r#"{"pattern":"needle","path":"sub","mode":"count"}"#,
        )),
        Reply::sse(&chat_tool_call_events(
            "call_3",
            "glob_files",
            r#"{"pattern":"**/*.txt"}"#,
        )),
        Reply::sse(&chat_text_events(&["Found it."])),
    ]);
    let home = Home::with_settings(&portkey_settings(&server.base_url()));
    fs::create_dir_all(home.workspace.join("sub")).unwrap();
    fs::write(home.workspace.join("sub/a.txt"), "alpha\nneedle tracked\n").unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&home.workspace)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home.root)
            .output()
            .is_ok_and(|output| output.status.success())
    };
    if !git(&["init", "--quiet"]) || !git(&["add", "sub/a.txt"]) {
        return;
    }
    let marker = home.root.join("hostile-ran");
    let script = home.root.join("hostile.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\necho \"$0 $*\" >> '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let global = home.root.join("hostile.gitconfig");
    fs::write(
        &global,
        format!(
            "[core]\n\tfsmonitor = {}\n\tquotePath = true\n[color]\n\tgrep = always\n\tui = always\n[grep]\n\tcolumn = true\n\tfullName = true\n\tlineNumber = true\n\tpatternType = perl\n\textendedRegexp = true\n\tfallbackToNoIndex = true\n",
            script.display()
        ),
    )
    .unwrap();

    let global = global.to_str().unwrap();
    let output = home.ask(
        &["ask", "--json", "where is the needle?"],
        &[
            ("PORTKEY_API_KEY", PORTKEY_KEY),
            ("PATH", "/usr/bin:/bin"),
            ("GIT_CONFIG_GLOBAL", global),
        ],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        !marker.exists(),
        "{}",
        fs::read_to_string(&marker).unwrap_or_default()
    );
    assert_eq!(
        tool_messages(&server.requests()[3]),
        [
            json!({
                "role": "tool",
                "content": "[grep] 1 matches for needle\n - sub/a.txt:2: needle tracked\n",
                "tool_call_id": "call_1",
            }),
            json!({
                "role": "tool",
                "content": "[grep] count 1 matching lines in 1 files for needle\n",
                "tool_call_id": "call_2",
            }),
            json!({
                "role": "tool",
                "content": "[glob] 1 matches for **/*.txt\n - sub/a.txt\n",
                "tool_call_id": "call_3",
            }),
        ]
    );
}

#[test]
fn ask_mode_fails_the_run_instead_of_searching_an_external_directory() {
    let outside = OutsideFile::new();
    let directory = canonical(Path::new(&outside.path).parent().unwrap());
    let searches = [
        (
            "glob_files",
            json!({"pattern": "*.txt", "path": directory}),
            "Matching *.txt",
        ),
        (
            "grep_files",
            json!({"pattern": "outside", "path": directory}),
            "Searching outside",
        ),
    ];
    for (name, arguments, label) in searches {
        let server = FakeServer::start([
            Reply::sse(&chat_tool_call_events(
                "call_1",
                name,
                &arguments.to_string(),
            )),
            Reply::sse(&chat_text_events(&["never"])),
        ]);
        let home = Home::with_settings(&settings_in_mode(&server.base_url(), "ask"));

        let output = home.ask(
            &["ask", "--json", "search it"],
            &[("PORTKEY_API_KEY", PORTKEY_KEY)],
        );
        assert_eq!(output.status.code(), Some(1), "{name}");
        assert_eq!(
            stderr(&output),
            format!(
                "{label}\noh-fx ask: permission required for tool execution in noninteractive mode\noh-fx ask: blocked action: {label}\noh-fx ask: reason=noninteractive_permission_prompt_unavailable\noh-fx ask: rerun with --auto to review this exact action automatically, or use the interactive shell to approve it\n"
            )
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["error"], "NonInteractivePermissionRequired");
        assert_eq!(
            result["tool_calls"],
            json!([{"name": name, "status": "error"}])
        );
        assert_eq!(server.requests().len(), 1);
        never_sees_the_secret(&server);
    }
}
