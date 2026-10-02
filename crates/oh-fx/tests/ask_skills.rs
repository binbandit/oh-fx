use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use ofx_testkit::{FakeServer, RecordedRequest, Reply, chat_text_events, chat_tool_call_events};
use serde_json::{Value, json};

const CATALOG_HEADER: &str = "Skills provide task instructions. Use named skills and clearly matching skills before substantive work.\nRead selected skills completely, including required references. Descriptions may be shortened; metadata is not loaded instructions.\n<available_skills>\n";
const EXPLICIT_HEADER: &str = "Explicitly invoked skill content for this query:\nUse every successfully loaded skill for this query. Report blocked or ambiguous requests.\nFollow each skill's complete instructions and required resources before substantive work.\nIf a skill cannot be followed, state the blocker instead of silently substituting another workflow.\n";

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
}

impl Home {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = fs::canonicalize(directory.path()).expect("canonicalize the home");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).expect("create the workspace");
        Self {
            _directory: directory,
            root,
            workspace,
        }
    }

    fn connected(server: &FakeServer) -> Self {
        let home = Self::new();
        home.connect(server);
        home
    }

    fn connect(&self, server: &FakeServer) {
        let settings = json!({
            "provider": "local",
            "providers": {
                "local": {
                    "protocol": "openai-chat-completions",
                    "base_url": server.base_url(),
                    "auth": {"type": "none"},
                    "models": ["model-a"]
                }
            }
        });
        write(
            &self.root.join("config/oh-fx/settings.json"),
            &settings.to_string(),
        );
    }

    fn managed(&self, name: &str) -> PathBuf {
        self.root.join("config/oh-fx/skills").join(name)
    }

    fn workspace_skill(&self, name: &str) -> PathBuf {
        self.workspace.join(".oh-fx/skills").join(name)
    }

    fn ask(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oh-fx"))
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
            .stdin(Stdio::null())
            .output()
            .expect("run oh-fx")
    }
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().expect("a parent directory")).expect("create directories");
    fs::write(path, content).expect("write the file");
}

fn skill_file(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n{body}")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn system_messages(request: &RecordedRequest) -> Vec<String> {
    request.json()["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] == "system")
        .map(|message| message["content"].as_str().expect("content").to_owned())
        .collect()
}

fn last_message(request: &RecordedRequest) -> Value {
    request.json()["messages"]
        .as_array()
        .expect("messages")
        .last()
        .expect("a message")
        .clone()
}

fn location_of(catalog: &str, name: &str) -> String {
    let prefix = format!("- {name}: ");
    let line = catalog
        .lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("{name} is not in the catalog:\n{catalog}"));
    let start = line.find("(location: ").expect("a location") + "(location: ".len();
    line[start..line.len() - 1].to_owned()
}

#[test]
fn ask_sends_the_catalog_and_named_skills_and_loads_skills_through_the_skill_tool() {
    let home = Home::new();
    let release = home.managed("release");
    write(
        &home.workspace_skill("review").join("SKILL.md"),
        &skill_file("review", "Review the change", "REVIEW STEPS\n"),
    );
    write(
        &release.join("SKILL.md"),
        &skill_file("release", "Cut a release", "RELEASE STEPS\n"),
    );
    write(&release.join("references/checklist.md"), "CHECKLIST\n");
    let location = release.to_str().expect("a UTF-8 path");
    let server = FakeServer::start([
        Reply::sse(&chat_tool_call_events(
            "call_1",
            "skill",
            &json!({"location": location}).to_string(),
        )),
        Reply::sse(&chat_tool_call_events(
            "call_2",
            "skill",
            &json!({"location": location, "resource": "references/checklist.md"}).to_string(),
        )),
        Reply::sse(&chat_text_events(&["Reviewed and released."])),
    ]);
    home.connect(&server);
    let output = home.ask(&["ask", "--json", "$review the release notes"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stderr(&output),
        "Loading skill release\nReading skill resource references/checklist.md\n"
    );
    let result: Value = serde_json::from_str(&stdout(&output)).expect("a JSON result");
    assert_eq!(result["output"], "Reviewed and released.");
    assert_eq!(
        result["tool_calls"],
        json!([
            {"name": "skill", "status": "success"},
            {"name": "skill", "status": "success"},
        ])
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let first = system_messages(&requests[0]);
    assert_eq!(first.len(), 6, "{first:#?}");
    assert!(first[0].starts_with("# Identity and context\n"));
    let catalog = &first[1];
    let review = home.workspace_skill("review");
    let review_location = location_of(catalog, "review");
    let release_location = location_of(catalog, "release");
    let namespace = &review_location["skill:".len().."skill:".len() + 16];
    assert_eq!(
        catalog,
        &format!(
            "{CATALOG_HEADER}Root 0: {}\nRoot 1: {}\n- review: Review the change (location: skill:{namespace}:0/review)\n- release: Cut a release (location: skill:{namespace}:1/release)\n</available_skills>\n",
            review.parent().unwrap().display(),
            release.parent().unwrap().display(),
        )
    );
    assert_eq!(release_location, format!("skill:{namespace}:1/release"));
    assert_eq!(
        first[2],
        format!(
            "{EXPLICIT_HEADER}<skill_content name=\"review\" location=\"{}\" resource=\"SKILL.md\" complete=\"true\">\n---\nname: review\ndescription: Review the change\n---\nREVIEW STEPS\n\n</skill_content>\n",
            review.display()
        )
    );
    assert!(first[3].starts_with("<fx-turn-context>\n"));
    assert!(first[4].starts_with("Runtime context: permission mode is auto."));
    assert!(first[5].starts_with("<response_language_control>"));
    for request in &requests[1..] {
        assert_eq!(system_messages(request), first);
    }
    let loaded = last_message(&requests[1]);
    assert_eq!(loaded["role"], "tool");
    assert_eq!(
        loaded["content"],
        format!(
            "<skill_content name=\"release\" location=\"{}\" resource=\"SKILL.md\" complete=\"true\">\n---\nname: release\ndescription: Cut a release\n---\nRELEASE STEPS\n\n</skill_content>",
            release.display()
        )
    );
    let resource = last_message(&requests[2]);
    assert_eq!(
        resource["content"],
        format!(
            "<skill_content name=\"release\" location=\"{}\" resource=\"references/checklist.md\" complete=\"true\">\nCHECKLIST\n\n</skill_content>",
            release.display()
        )
    );
}

#[test]
fn ask_without_skills_sends_no_catalog() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Hi."]))]);
    let home = Home::connected(&server);
    let output = home.ask(&["ask", "$review hello"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Hi.");
    let system = system_messages(&server.requests()[0]);
    assert_eq!(system.len(), 4, "{system:#?}");
    assert!(
        !system
            .iter()
            .any(|message| message.contains("<available_skills>"))
    );
}

#[test]
fn ask_reports_skipped_skills_once_and_warns_the_model() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Hi."]))]);
    let home = Home::connected(&server);
    let broken = home.workspace_skill("broken");
    write(
        &broken.join("SKILL.md"),
        "---\ndescription: nameless\n---\n",
    );
    let output = home.ask(&["ask", "--quiet", "hello"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stderr(&output),
        format!(
            "[notice] skill discovery warning: candidate \"{}\" was skipped because its metadata is invalid (missing_name); use one safe name and an optional inline description or a >, >-, or | block, then reload skills\n",
            broken.display()
        )
    );
    let system = system_messages(&server.requests()[0]);
    assert_eq!(
        system[1],
        "<skill_discovery_warning skipped_candidate_count=\"1\" incomplete_root_count=\"0\" missing_from_incomplete_roots=\"0\" />\n"
    );
}

#[test]
fn skill_context_limits_on_the_command_line_shape_the_catalog() {
    let server = FakeServer::start([Reply::sse(&chat_text_events(&["Hi."]))]);
    let home = Home::connected(&server);
    write(
        &home.managed("review").join("SKILL.md"),
        &skill_file("review", "Review the change", "REVIEW STEPS\n"),
    );
    let output = home.ask(&[
        "--context-limit",
        "skill_description_bytes=6",
        "ask",
        "--json",
        "hello",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stderr(&output),
        "[notice] [context] skill descriptions shortened: 1; source=command line\n\n"
    );
    let system = system_messages(&server.requests()[0]);
    assert!(
        system[1].contains("\n- review: Review (location: skill:"),
        "{}",
        system[1]
    );
}
