use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use ofx_testkit::{FakeServer, RecordedRequest, Reply, chat_text_events};
use serde_json::{Value, json};

const KEY: [(&str, &str); 1] = [("PORTKEY_API_KEY", "pk-test-0123456789")];
const DEFERRED: &str = "Scoped project instructions were added before execution. Review them and reissue this tool call if it is still appropriate.";
const MCP_SERVERS_NONE: &str = include_str!("../../../parity/goldens/mcp_servers_section.txt");
const GUIDANCE: &str = "<project-instructions-guidance>\nDirect user instructions take precedence over project instructions. When project instructions conflict, follow the narrowest applicable project scope.\n</project-instructions-guidance>";

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new(base_url: &str, extra: &Value) -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = fs::canonicalize(directory.path()).expect("canonicalize the home");
        let home = Self {
            _directory: directory,
            root,
        };
        let mut settings = json!({
            "provider": "portkey",
            "model": "@openai/gpt-4o",
            "providers": {
                "portkey": {
                    "protocol": "openai-chat-completions",
                    "base_url": base_url,
                    "auth": {"type": "none"},
                    "headers": {"x-portkey-api-key": "${PORTKEY_API_KEY}"},
                    "models": ["@openai/gpt-4o"]
                }
            }
        });
        for (key, value) in extra.as_object().expect("extra settings are an object") {
            settings[key] = value.clone();
        }
        home.write("config/oh-fx/settings.json", &settings.to_string());
        home
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().expect("files have a parent")).expect("create parents");
        fs::write(&path, contents).expect("write the file");
        path
    }

    fn ask(&self, workspace: &str, args: &[&str]) -> Output {
        let workspace = self.path(workspace);
        fs::create_dir_all(&workspace).expect("create the workspace");
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
            .args(args)
            .current_dir(&workspace)
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_DATA_HOME", self.path("data"))
            .env("XDG_CACHE_HOME", self.path("cache"))
            .env("SHELL", "/bin/sh")
            .env("OH_FX_AUTO_UPGRADE", "0")
            .envs(KEY)
            .stdin(Stdio::null())
            .output()
            .expect("run oh-fx")
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn system_texts(request: &RecordedRequest) -> Vec<String> {
    request.json()["messages"]
        .as_array()
        .expect("the request carries messages")
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

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[test]
fn ask_sends_global_ancestor_and_workspace_rules_after_the_system_prompt() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["done"]))]);
    let home = Home::new(&server.base_url(), &json!({}));
    let global = home.write("config/oh-fx/AGENTS.md", "GLOBAL RULE\n");
    let parent = home.write("projects/AGENTS.md", "PARENT RULE\n");
    let project = home.write("projects/work/AGENTS.md", "  \nWORKSPACE RULE\n\n");
    home.write("AGENTS.md", "HOME RULE\n");
    home.write("projects/work/nested/AGENTS.md", "NESTED RULE\n");
    let output = home.ask("projects/work", &["ask", "hi"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    let texts = system_texts(&server.requests()[0]);
    assert_eq!(texts.len(), 7);
    assert!(texts[0].starts_with("# Identity and context\n"));
    assert!(texts[1].starts_with("Search the current public web"));
    assert_eq!(
        texts[2],
        format!(
            "{GUIDANCE}\n\n<global-rules from=\"{}\">\nGLOBAL RULE\n</global-rules>\n\n<scoped-rules from=\"{}\" scope=\"{}\">\nPARENT RULE\n</scoped-rules>\n\n<project-rules from=\"{}\">\nWORKSPACE RULE\n</project-rules>",
            display(&global),
            display(&parent),
            display(parent.parent().unwrap()),
            display(&project),
        )
    );
    assert_eq!(texts[3], MCP_SERVERS_NONE);
    assert!(texts[4].starts_with("<fx-turn-context>\n"));
    assert!(texts[5].starts_with("Runtime context: permission mode is auto."));
    assert!(texts[6].starts_with("<response_language_control>"));
}

#[test]
fn the_context_setting_turns_project_instructions_off() {
    let server = FakeServer::start([
        Reply::sse(&chat_text_events(&["done"])),
        Reply::sse(&chat_text_events(&["done"])),
    ]);
    let home = Home::new(&server.base_url(), &json!({"context": false}));
    home.write("work/AGENTS.md", "WORKSPACE RULE\n");
    let output = home.ask("work", &["ask", "hi"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let enabled = Home::new(&server.base_url(), &json!({}));
    enabled.write("work/AGENTS.md", "WORKSPACE RULE\n");
    enabled.write("work/.oh-fx.json", r#"{"context":false}"#);
    let output = enabled.ask("work", &["ask", "hi"]);
    assert!(output.status.success(), "{}", stderr(&output));
    for request in server.requests() {
        let texts = system_texts(&request);
        assert_eq!(texts.len(), 6);
        assert!(texts.iter().all(|text| !text.contains("WORKSPACE RULE")));
    }
}

#[test]
fn limit_notices_print_on_stderr_in_raw_quiet_and_json_modes() {
    let replies = (0..3).map(|_| Reply::sse(&chat_text_events(&["done"])));
    let server = FakeServer::start(replies);
    let home = Home::new(
        &server.base_url(),
        &json!({"context_limits": {"project_instruction_file_bytes": 12}}),
    );
    let project = home.write("work/AGENTS.md", "LINE-ONE\nLINE-TWO\nLINE-THREE\n");
    let notice = format!(
        "[notice] [context] project instruction file \"{}\" truncated: observed=29 bytes effective=12 bytes source=global settings; override with --context-limit project_instruction_file_bytes=BYTES|off\n",
        display(&project)
    );
    for args in [
        &["ask", "hi"][..],
        &["ask", "--quiet", "hi"],
        &["ask", "--json", "hi"],
    ] {
        let output = home.ask("work", args);
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(stderr(&output), notice, "{args:?}");
    }
    let texts = system_texts(&server.requests()[0]);
    assert_eq!(
        texts[2],
        format!(
            "{GUIDANCE}\n\n<project-rules from=\"{0}\">\nLINE-ONE\n</project-rules>\n\n<context_limit name=\"project_instruction_file_bytes\" action=\"truncated\" source_file=\"{0}\" observed_bytes=\"29\" effective_bytes=\"12\" source=\"global settings\" override=\"--context-limit project_instruction_file_bytes=BYTES|off\" />",
            display(&project)
        )
    );
}

#[test]
fn a_workspace_outside_home_and_escaping_symlinks_are_reported_without_their_content() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["done"]))]);
    let home = Home::new(&server.base_url(), &json!({}));
    let secret = home.write("secret.txt", "DO_NOT_EXPOSE\n");
    let workspace = home.path("outside/work");
    fs::create_dir_all(&workspace).unwrap();
    symlink(&secret, workspace.join("AGENTS.md")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
    let output = command
        .args(["ask", "hi"])
        .current_dir(&workspace)
        .env_clear()
        .env("HOME", home.path("config"))
        .env("XDG_CONFIG_HOME", home.path("config"))
        .env("OH_FX_AUTO_UPGRADE", "0")
        .envs(KEY)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let source = display(&workspace.join("AGENTS.md"));
    assert_eq!(
        stderr(&output),
        format!(
            "[notice] [context] project instructions action=omitted reason=symlinked rule file source=\"{source}\"; repair=replace the symlink with a regular file\n"
        )
    );
    let texts = system_texts(&server.requests()[0]);
    assert_eq!(
        texts[2],
        format!(
            "<project-rules-omitted from=\"{}\" reason=\"workspace is not below home\" />\n\n<project-rules-omitted from=\"{source}\" reason=\"symlinked rule file\" />",
            display(&workspace)
        )
    );
    assert!(
        server.requests()[0]
            .body_text()
            .find("DO_NOT_EXPOSE")
            .is_none()
    );
}

#[test]
fn a_relative_home_resolves_against_the_working_directory() {
    let replies = (0..2).map(|_| Reply::sse(&chat_text_events(&["done"])));
    let server = FakeServer::start(replies);
    let home = Home::new(&server.base_url(), &json!({}));
    let settings = fs::read_to_string(home.path("config/oh-fx/settings.json")).unwrap();
    home.write("work/.config/oh-fx/settings.json", &settings);
    let below_working_directory = home.write("work/.config/oh-fx/AGENTS.md", "RELATIVE GLOBAL\n");
    let configured = home.write("config/oh-fx/AGENTS.md", "CONFIGURED GLOBAL\n");
    home.write("AGENTS.md", "HOME RULE\n");
    let project = home.write("work/AGENTS.md", "WORKSPACE RULE\n");
    let run = |relative_home: &str, config_home: Option<PathBuf>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .args(["ask", "hi"])
            .current_dir(home.path("work"))
            .env_clear()
            .env("HOME", relative_home)
            .env("OH_FX_AUTO_UPGRADE", "0")
            .envs(KEY)
            .stdin(Stdio::null());
        if let Some(config_home) = config_home {
            command.env("XDG_CONFIG_HOME", config_home);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(stderr(&output), "");
    };
    run(".", None);
    run("..", Some(home.path("config")));
    let requests = server.requests();
    for (request, global, body) in [
        (&requests[0], &below_working_directory, "RELATIVE GLOBAL"),
        (&requests[1], &configured, "CONFIGURED GLOBAL"),
    ] {
        assert_eq!(
            system_texts(request)[2],
            format!(
                "{GUIDANCE}\n\n<global-rules from=\"{}\">\n{body}\n</global-rules>\n\n<project-rules from=\"{}\">\nWORKSPACE RULE\n</project-rules>",
                display(global),
                display(&project)
            )
        );
    }
}

#[test]
fn command_line_limits_override_settings_for_project_instructions() {
    let replies = (0..2).map(|_| Reply::sse(&chat_text_events(&["done"])));
    let server = FakeServer::start(replies);
    let home = Home::new(
        &server.base_url(),
        &json!({"context_limits": {"project_instruction_file_bytes": 20}}),
    );
    let project = home.write("work/AGENTS.md", "LINE-ONE\nLINE-TWO\nLINE-THREE\n");
    let output = home.ask(
        "work",
        &[
            "--context-limit",
            "project_instruction_file_bytes=off",
            "--context-limit=project_instruction_file_bytes=12",
            "ask",
            "hi",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stderr(&output),
        format!(
            "[notice] [context] project instruction file \"{}\" truncated: observed=29 bytes effective=12 bytes source=command line; override with --context-limit project_instruction_file_bytes=BYTES|off\n",
            display(&project)
        )
    );
    let output = home.ask(
        "work",
        &[
            "--context-limit",
            "project_instructions_total_bytes=0",
            "ask",
            "hi",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let requests = server.requests();
    assert!(system_texts(&requests[0])[2].contains("source=\"command line\""));
    assert_eq!(
        system_texts(&requests[1])[2],
        format!(
            "<context_limit name=\"project_instructions_total_bytes\" action=\"omitted\" omitted_count=\"1\" observed_bytes=\"{}\" effective_bytes=\"0\" source=\"command line\" override=\"--context-limit project_instructions_total_bytes=BYTES|off\" />",
            GUIDANCE.len() + 2 + format!(
                "<project-rules from=\"{}\">\nLINE-ONE\nLINE-TWO\n</project-rules>\n\n<context_limit name=\"project_instruction_file_bytes\" action=\"truncated\" source_file=\"{}\" observed_bytes=\"29\" effective_bytes=\"20\" source=\"global settings\" override=\"--context-limit project_instruction_file_bytes=BYTES|off\" />",
                display(&project),
                display(&project)
            )
            .len()
        )
    );
}

#[test]
fn limits_that_ask_cannot_apply_still_fail_as_not_available() {
    let server = FakeServer::start([]);
    let home = Home::new(&server.base_url(), &json!({}));
    for args in [
        &[
            "--context-limit",
            "image_adapter_output_bytes=1",
            "ask",
            "hi",
        ][..],
        &[
            "--context-limit",
            "project_instruction_file_bytes=1",
            "--context-limit",
            "mcp_description_bytes=off",
            "ask",
            "hi",
        ],
    ] {
        let output = home.ask("work", args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(
            stderr(&output),
            "oh-fx: --context-limit is not available yet\n",
            "{args:?}"
        );
    }
    assert!(server.requests().is_empty());
}

fn tool_calls(calls: &[(&str, &str, &str)]) -> Reply {
    let chunk = |delta: Value, finish_reason: Value| {
        json!({
            "id": "chatcmpl-scoped",
            "object": "chat.completion.chunk",
            "model": "testkit-model",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        })
        .to_string()
    };
    let calls: Vec<Value> = calls
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
    Reply::sse(&[
        chunk(
            json!({"role": "assistant", "tool_calls": calls}),
            Value::Null,
        ),
        chunk(json!({}), json!("tool_calls")),
        "[DONE]".to_owned(),
    ])
}

fn tool_results(request: &RecordedRequest) -> Vec<(String, String)> {
    request.json()["messages"]
        .as_array()
        .expect("the request carries messages")
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| {
            (
                message["tool_call_id"]
                    .as_str()
                    .expect("tool results name their call")
                    .to_owned(),
                message["content"]
                    .as_str()
                    .expect("tool results carry text")
                    .to_owned(),
            )
        })
        .collect()
}

fn scoped(source: &Path) -> String {
    format!(
        "{GUIDANCE}\n\n<scoped-rules from=\"{}\" scope=\"{}\">\n{}\n</scoped-rules>",
        display(source),
        display(source.parent().expect("rule files have a directory")),
        fs::read_to_string(source)
            .expect("read the rule file")
            .trim()
    )
}

#[test]
fn tool_targets_add_scoped_rules_and_a_lone_write_waits_for_them() {
    let write = r#"{"path":"deep/new.txt","content":"hello\n"}"#;
    let server = FakeServer::start([
        tool_calls(&[("call_1", "read_file", r#"{"path":"sub/x.txt"}"#)]),
        tool_calls(&[("call_2", "write_file", write)]),
        tool_calls(&[("call_3", "write_file", write)]),
        Reply::sse(&chat_text_events(&["done"])),
    ]);
    let home = Home::new(&server.base_url(), &json!({}));
    let sub = home.write("work/sub/AGENTS.md", "SUB RULE\n");
    home.write("work/sub/x.txt", "x\n");
    let deep = home.write("work/deep/AGENTS.md", "DEEP RULE\n");
    let output = home.ask("work", &["ask", "--json", "go"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stderr(&output),
        "Reading sub/x.txt\nWriting file\nWriting deep/new.txt\n"
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["steps"], 3);
    assert_eq!(
        result["tool_calls"],
        json!([
            {"name": "read_file", "status": "success"},
            {"name": "write_file", "status": "success"},
        ])
    );
    let requests = server.requests();
    assert_eq!(system_texts(&requests[0]).len(), 6);
    assert_eq!(system_texts(&requests[1])[2], MCP_SERVERS_NONE);
    assert_eq!(system_texts(&requests[1])[3], scoped(&sub));
    let texts = system_texts(&requests[3]);
    assert_eq!(
        texts[2..5],
        [MCP_SERVERS_NONE.to_owned(), scoped(&sub), scoped(&deep)]
    );
    assert!(texts[5].starts_with("<fx-turn-context>\n"));
    assert_eq!(
        tool_results(&requests[3]),
        [
            (
                "call_1".to_owned(),
                "<path>sub/x.txt</path>\n<content>\n1\tx\n</content>".to_owned()
            ),
            ("call_2".to_owned(), DEFERRED.to_owned()),
            (
                "call_3".to_owned(),
                "wrote deep/new.txt (6 bytes)".to_owned()
            ),
        ]
    );
    assert_eq!(
        fs::read_to_string(home.path("work/deep/new.txt")).unwrap(),
        "hello\n"
    );
}

#[test]
fn batches_defer_only_the_writes_whose_own_scope_brings_new_rules() {
    let server = FakeServer::start([
        tool_calls(&[
            ("call_1", "read_file", r#"{"path":"a/x.txt"}"#),
            (
                "call_2",
                "write_file",
                r#"{"path":"b/new.txt","content":"b\n"}"#,
            ),
            (
                "call_3",
                "write_file",
                r#"{"path":"c/new.txt","content":"c\n"}"#,
            ),
            (
                "call_4",
                "write_file",
                r#"{"path":"a/new.txt","content":"a\n"}"#,
            ),
        ]),
        Reply::sse(&chat_text_events(&["done"])),
    ]);
    let home = Home::new(&server.base_url(), &json!({}));
    let a = home.write("work/a/AGENTS.md", "A RULE\n");
    home.write("work/a/x.txt", "x\n");
    let b = home.write("work/b/AGENTS.md", "B RULE\n");
    fs::create_dir_all(home.path("work/c")).unwrap();
    let output = home.ask("work", &["ask", "--json", "go"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stderr(&output),
        "Reading a/x.txt\nWriting file\nWriting c/new.txt\nWriting file\n"
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["steps"], 4);
    assert_eq!(
        result["tool_calls"],
        json!([
            {"name": "read_file", "status": "success"},
            {"name": "write_file", "status": "success"},
        ])
    );
    let requests = server.requests();
    let combined = format!(
        "{}\n\n<scoped-rules from=\"{}\" scope=\"{}\">\nB RULE\n</scoped-rules>",
        scoped(&a),
        display(&b),
        display(b.parent().unwrap())
    );
    assert_eq!(system_texts(&requests[1])[2], combined);
    assert_eq!(
        tool_results(&requests[1])
            .into_iter()
            .map(|(id, content)| (id, content == DEFERRED))
            .collect::<Vec<_>>(),
        [
            ("call_1".to_owned(), false),
            ("call_2".to_owned(), true),
            ("call_3".to_owned(), false),
            ("call_4".to_owned(), true),
        ]
    );
    assert!(!home.path("work/b/new.txt").exists());
    assert!(home.path("work/c/new.txt").exists());
}

#[test]
fn scoped_omission_notices_print_before_the_progress_lines_and_hide_link_targets() {
    let server = FakeServer::start([
        tool_calls(&[
            ("call_1", "read_file", r#"{"path":"sub/x.txt"}"#),
            ("call_2", "glob_files", r#"{"pattern":"*","path":"linked"}"#),
            ("call_3", "grep_files", r#"{"pattern":"x","path":"dir"}"#),
            ("call_4", "glob_files", r#"{"pattern":"*","path":"inv"}"#),
        ]),
        Reply::sse(&chat_text_events(&["done"])),
    ]);
    let home = Home::new(&server.base_url(), &json!({}));
    let secret = home.write("secret.txt", "DO_NOT_EXPOSE\n");
    home.write("work/sub/x.txt", "x\n");
    symlink(&secret, home.path("work/sub/AGENTS.md")).unwrap();
    home.write("work/linked/CLAUDE.md", "REAL LINKED\n");
    symlink("CLAUDE.md", home.path("work/linked/AGENTS.md")).unwrap();
    fs::create_dir_all(home.path("work/dir/AGENTS.md")).unwrap();
    fs::create_dir_all(home.path("work/inv")).unwrap();
    fs::write(home.path("work/inv/AGENTS.md"), b"ok\n\xff").unwrap();
    let output = home.ask("work", &["ask", "go"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let source = |relative: &str| display(&home.path(relative));
    assert_eq!(
        stderr(&output),
        format!(
            "[notice] [context] project instructions action=omitted reason=symlinked rule file source=\"{}\"; repair=replace the symlink with a regular file\n[notice] [context] project instructions action=omitted reason=non-regular rule file source=\"{}\"; repair=replace the source with a regular file\n[notice] [context] project instructions action=omitted reason=unreadable rule file source=\"{}\"; repair=make the rule file readable UTF-8\nReading sub/x.txt\nMatching *\nSearching x\nMatching *\n",
            source("work/sub/AGENTS.md"),
            source("work/dir/AGENTS.md"),
            source("work/inv/AGENTS.md"),
        )
    );
    let requests = server.requests();
    assert_eq!(
        system_texts(&requests[1])[2],
        format!(
            "{GUIDANCE}\n\n<scoped-rules from=\"{}\" scope=\"{}\">\nREAL LINKED\n</scoped-rules>\n\n<project-rules-omitted from=\"{}\" reason=\"non-regular rule file\" />\n\n<project-rules-omitted from=\"{}\" reason=\"unreadable rule file\" />\n\n<project-rules-omitted from=\"{}\" reason=\"symlinked rule file\" />",
            source("work/linked/AGENTS.md"),
            source("work/linked"),
            source("work/dir/AGENTS.md"),
            source("work/inv/AGENTS.md"),
            source("work/sub/AGENTS.md"),
        )
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.body_text().contains("DO_NOT_EXPOSE"))
    );
}

fn shell_run(cwd: Option<&str>) -> String {
    let mut request = json!({"action": "run", "command": "echo hi"});
    if let Some(cwd) = cwd {
        request["cwd"] = json!(cwd);
    }
    json!({ "request": request }).to_string()
}

fn yolo() -> Value {
    json!({"permission_mode": "yolo", "yolo_acknowledged": true})
}

#[test]
fn calls_whose_targets_appear_earlier_in_the_batch_are_not_executed() {
    let server = FakeServer::start([
        tool_calls(&[
            (
                "call_1",
                "write_file",
                r#"{"path":"n/new.txt","content":"hi\n"}"#,
            ),
            ("call_2", "read_file", r#"{"path":"n/new.txt"}"#),
            (
                "call_3",
                "edit_file",
                r#"{"path":"n/new.txt","old_string":"hi","new_string":"bye"}"#,
            ),
        ]),
        Reply::sse(&chat_text_events(&["done"])),
    ]);
    let home = Home::new(&server.base_url(), &json!({}));
    let output = home.ask("work", &["ask", "--json", "--auto", "go"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stderr(&output),
        "Writing n/new.txt\nReading n/new.txt\nEditing file\n"
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["steps"], 3);
    assert_eq!(
        result["tool_calls"],
        json!([
            {"name": "write_file", "status": "success"},
            {"name": "edit_file", "status": "error"},
        ])
    );
    assert_eq!(
        tool_results(&server.requests()[1]),
        [
            ("call_1".to_owned(), "wrote n/new.txt (3 bytes)".to_owned()),
            ("call_2".to_owned(), "Not executed".to_owned()),
            (
                "call_3".to_owned(),
                "file mutation target resolution failed: file_not_found".to_owned()
            ),
        ]
    );
    assert_eq!(
        fs::read_to_string(home.path("work/n/new.txt")).unwrap(),
        "hi\n"
    );
}

#[test]
fn file_changes_whose_targets_move_or_vanish_earlier_in_the_batch_are_not_executed() {
    let command = "rm sub/f.txt && rm -r d && ln -s other d";
    let server = FakeServer::start([
        tool_calls(&[
            (
                "call_1",
                "shell",
                &json!({"request": {"action": "run", "command": command}}).to_string(),
            ),
            (
                "call_2",
                "edit_file",
                r#"{"path":"sub/f.txt","old_string":"old","new_string":"new"}"#,
            ),
            (
                "call_3",
                "write_file",
                r#"{"path":"d/x.txt","content":"hi\n"}"#,
            ),
        ]),
        Reply::sse(&chat_text_events(&["done"])),
    ]);
    let home = Home::new(&server.base_url(), &yolo());
    home.write("work/sub/f.txt", "old\n");
    home.write("work/d/f.txt", "f\n");
    home.write("work/other/AGENTS.md", "OTHER RULE\n");
    let output = home.ask("work", &["ask", "--json", "go"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stderr(&output),
        format!("Running {command}\nEditing file\nWriting file\n")
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["steps"], 3);
    assert_eq!(result["tool_calls"].as_array().unwrap().len(), 1);
    let requests = server.requests();
    let results = tool_results(&requests[1]);
    assert_eq!(
        results[1..],
        [
            ("call_2".to_owned(), "Not executed".to_owned()),
            ("call_3".to_owned(), "Not executed".to_owned()),
        ]
    );
    assert!(!home.path("work/other/x.txt").exists());
    assert!(
        requests
            .iter()
            .all(|request| !request.body_text().contains("OTHER RULE"))
    );
}

#[test]
fn shell_runs_are_deferred_by_their_working_directory_rules_only() {
    let server = FakeServer::start([
        tool_calls(&[("call_1", "shell", &shell_run(Some("sub")))]),
        tool_calls(&[
            ("call_2", "read_file", r#"{"path":"deep/x.txt"}"#),
            ("call_3", "shell", &shell_run(None)),
            ("call_4", "shell", &shell_run(Some(".."))),
            (
                "call_5",
                "shell",
                r#"{"request":{"action":"interact","session_id":"missing"}}"#,
            ),
            (
                "call_6",
                "shell",
                r#"{"request":{"action":"stop","session_id":"missing"}}"#,
            ),
        ]),
        tool_calls(&[
            (
                "call_7",
                "write_file",
                r#"{"path":"build/x.txt","content":"x\n"}"#,
            ),
            ("call_8", "shell", &shell_run(Some("build"))),
        ]),
        Reply::sse(&chat_text_events(&["done"])),
    ]);
    let home = Home::new(&server.base_url(), &yolo());
    let sub = home.write("work/sub/AGENTS.md", "SUB RULE\n");
    home.write("work/deep/AGENTS.md", "DEEP RULE\n");
    home.write("work/deep/x.txt", "x\n");
    home.write("outside/AGENTS.md", "OUTSIDE RULE\n");
    let output = home.ask("work", &["ask", "--json", "go"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).starts_with("Running echo hi\nReading deep/x.txt\nRunning echo hi\n"),
        "{}",
        stderr(&output)
    );
    let requests = server.requests();
    assert_eq!(system_texts(&requests[1])[2], scoped(&sub));
    let results = tool_results(&requests[3]);
    assert_eq!(results[0], ("call_1".to_owned(), DEFERRED.to_owned()));
    for (call_id, content) in &results[2..6] {
        assert_ne!(content, DEFERRED, "{call_id}");
        assert_ne!(content, "Not executed", "{call_id}");
    }
    assert!(results[2].1.contains("hi"), "{}", results[2].1);
    assert!(results[3].1.contains("hi"), "{}", results[3].1);
    assert_eq!(
        results[7],
        (
            "call_8".to_owned(),
            "shell run cwd is invalid: FileNotFound".to_owned()
        )
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.body_text().contains("OUTSIDE RULE"))
    );
}
