use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use ofx_testkit::{FakeServer, RecordedRequest, Reply, chat_text_events};
use serde_json::{Value, json};

const KEY: [(&str, &str); 1] = [("PORTKEY_API_KEY", "pk-test-0123456789")];
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
    assert_eq!(texts.len(), 5);
    assert!(texts[0].starts_with("# Identity and context\n"));
    assert_eq!(
        texts[1],
        format!(
            "{GUIDANCE}\n\n<global-rules from=\"{}\">\nGLOBAL RULE\n</global-rules>\n\n<scoped-rules from=\"{}\" scope=\"{}\">\nPARENT RULE\n</scoped-rules>\n\n<project-rules from=\"{}\">\nWORKSPACE RULE\n</project-rules>",
            display(&global),
            display(&parent),
            display(parent.parent().unwrap()),
            display(&project),
        )
    );
    assert!(texts[2].starts_with("<fx-turn-context>\n"));
    assert!(texts[3].starts_with("Runtime context: permission mode is auto."));
    assert!(texts[4].starts_with("<response_language_control>"));
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
        assert_eq!(texts.len(), 4);
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
        texts[1],
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
        texts[1],
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
            system_texts(request)[1],
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
    assert!(system_texts(&requests[0])[1].contains("source=\"command line\""));
    assert_eq!(
        system_texts(&requests[1])[1],
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
        &["--context-limit", "skill_chunk_bytes=1", "ask", "hi"][..],
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
