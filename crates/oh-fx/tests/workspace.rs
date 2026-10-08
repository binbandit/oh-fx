use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

struct Home {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create a temporary home");
        let root = fs::canonicalize(directory.path()).expect("canonicalize the home");
        for name in ["workspace", "shared", "other"] {
            fs::create_dir_all(root.join(name)).expect("create a directory");
        }
        fs::write(root.join("file"), "x").expect("write a file");
        Self {
            _directory: directory,
            root,
        }
    }

    fn path(&self, name: &str) -> String {
        self.root.join(name).display().to_string()
    }

    fn settings_path(&self) -> PathBuf {
        self.root.join("config/oh-fx/settings.json")
    }

    fn write_settings(&self, settings: &Value) {
        fs::create_dir_all(self.root.join("config/oh-fx")).expect("create the config directory");
        fs::write(self.settings_path(), settings.to_string()).expect("write settings");
    }

    fn settings(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.settings_path()).expect("read settings"))
            .expect("settings are JSON")
    }

    fn run(&self, args: &[&str], home: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oh-fx"));
        command
            .args(args)
            .current_dir(self.root.join("workspace"))
            .env_clear()
            .env("OH_FX_AUTO_UPGRADE", "0")
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .stdin(Stdio::null());
        if home {
            command.env("HOME", &self.root);
        }
        command.output().expect("run oh-fx")
    }

    fn workspace(&self, args: &[&str]) -> (String, String) {
        let mut full = vec!["workspace"];
        full.extend_from_slice(args);
        let output = self.run(&full, true);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            text(&output.stderr)
        );
        (text(&output.stdout), text(&output.stderr))
    }

    fn fails(&self, args: &[&str]) -> (String, String) {
        let mut full = vec!["workspace"];
        full.extend_from_slice(args);
        let output = self.run(&full, true);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        (text(&output.stdout), text(&output.stderr))
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn header(home: &Home) -> String {
    format!(
        "[workspace] primary={}\n[workspace] saved_suppressed=false limit=16\n",
        home.path("workspace")
    )
}

fn entry(path: &str, available: bool) -> String {
    format!(" - {path} saved=true command_line=false available={available} active={available}\n")
}

#[test]
fn listing_without_saved_directories_shows_only_the_primary_workspace() {
    let home = Home::new();
    let (stdout, stderr) = home.workspace(&[]);
    assert_eq!(stderr, "");
    assert_eq!(
        stdout,
        format!(
            "{}[workspace] additional directories: (none)\n",
            header(&home)
        )
    );
    assert_eq!(home.workspace(&["list"]).0, stdout);
    let (stdout, _) = home.workspace(&["--json"]);
    assert_eq!(
        stdout,
        format!(
            "{{\"kind\":\"workspace\",\"action\":\"list\",\"changed\":false,\"primary_directory\":\"{}\",\"saved_suppressed\":false,\"limit\":16,\"additional_directories\":[]}}\n",
            home.path("workspace")
        )
    );
}

#[test]
fn adding_saves_the_real_path_for_this_workspace_and_reports_the_change() {
    let home = Home::new();
    let shared = home.path("shared");
    let (stdout, _) = home.workspace(&["add", "../shared"]);
    assert_eq!(
        stdout,
        format!(
            "{}[workspace] add ../shared saved_changed=true runtime_changed=true launch_flag_can_restore=false\n[workspace] additional directories:\n{}",
            header(&home),
            entry(&shared, true)
        )
    );
    assert_eq!(
        home.settings(),
        json!({"workspaces": {home.path("workspace"): {"additional_directories": [shared]}}})
    );

    let (stdout, _) = home.workspace(&["add", &shared, "--json"]);
    assert_eq!(
        stdout,
        format!(
            "{{\"kind\":\"workspace\",\"action\":\"add\",\"changed\":false,\"primary_directory\":\"{}\",\"saved_suppressed\":false,\"limit\":16,\"path\":\"{shared}\",\"saved_changed\":false,\"runtime_changed\":false,\"launch_flag_can_restore\":false,\"additional_directories\":[{{\"path\":\"{shared}\",\"saved\":true,\"command_line\":false,\"available\":true,\"active\":true}}]}}\n",
            home.path("workspace")
        )
    );

    fs::remove_dir(home.root.join("shared")).expect("remove the shared directory");
    let (stdout, _) = home.workspace(&[]);
    assert_eq!(
        stdout,
        format!(
            "{}[workspace] additional directories:\n{}",
            header(&home),
            entry(&shared, false)
        )
    );
}

#[test]
fn removing_and_clearing_drop_saved_directories_and_the_empty_entry() {
    let home = Home::new();
    let shared = home.path("shared");
    let other = home.path("other");
    home.write_settings(&json!({
        "model": "kept",
        "workspaces": {home.path("workspace"): {"additional_directories": [shared, other]}},
    }));
    let (stdout, _) = home.workspace(&["remove", "../shared"]);
    assert_eq!(
        stdout,
        format!(
            "{}[workspace] remove ../shared saved_changed=true runtime_changed=true launch_flag_can_restore=false\n[workspace] additional directories:\n{}",
            header(&home),
            entry(&other, true)
        )
    );
    let (stdout, _) = home.workspace(&["clear", "--json"]);
    assert_eq!(
        stdout,
        format!(
            "{{\"kind\":\"workspace\",\"action\":\"clear\",\"changed\":true,\"primary_directory\":\"{}\",\"saved_suppressed\":false,\"limit\":16,\"saved_changed\":true,\"runtime_changed\":true,\"launch_flag_can_restore\":false,\"additional_directories\":[]}}\n",
            home.path("workspace")
        )
    );
    assert_eq!(home.settings(), json!({"model": "kept"}));
    let (stdout, _) = home.workspace(&["clear"]);
    assert!(
        stdout.contains("[workspace] clear saved_changed=false runtime_changed=false launch_flag_can_restore=false\n"),
        "{stdout}"
    );
}

#[test]
fn rejected_paths_explain_themselves_in_text_and_json() {
    let home = Home::new();
    let shared = home.path("shared");
    for (args, message, code) in [
        (
            &["add", "../missing"][..],
            "directory does not exist",
            "PathNotFound",
        ),
        (
            &["add", "../file"],
            "path is not a directory",
            "NotDirectory",
        ),
        (
            &["add", "."],
            "the primary workspace cannot be added or removed",
            "PrimaryDirectory",
        ),
        (
            &["remove", &shared],
            "directory is not configured as an additional workspace",
            "UnknownAdditionalDirectory",
        ),
        (&["remove", "../shared"], "path is invalid", "InvalidPath"),
    ] {
        let (stdout, stderr) = home.fails(args);
        assert_eq!(stdout, "", "{args:?}");
        assert_eq!(stderr, format!("oh-fx workspace: {message}\n"), "{args:?}");
        let mut json = args.to_vec();
        json.push("--json");
        let (stdout, stderr) = home.fails(&json);
        assert_eq!(stderr, "", "{args:?}");
        assert_eq!(
            stdout,
            format!("{{\"kind\":\"workspace\",\"error\":\"{message}\",\"code\":\"{code}\"}}\n"),
            "{args:?}"
        );
    }
}

#[test]
fn the_seventeenth_directory_is_refused() {
    let home = Home::new();
    let saved: Vec<String> = (0..16)
        .map(|index| {
            let path = home.root.join(format!("d{index}"));
            fs::create_dir_all(&path).expect("create a directory");
            path.display().to_string()
        })
        .collect();
    home.write_settings(&json!({
        "workspaces": {home.path("workspace"): {"additional_directories": saved}},
    }));
    let (_, stderr) = home.fails(&["add", "../shared"]);
    assert_eq!(
        stderr,
        "oh-fx workspace: additional directory limit reached\n"
    );
    let (stdout, _) = home.fails(&["add", "../shared", "--json"]);
    assert_eq!(
        stdout,
        "{\"kind\":\"workspace\",\"error\":\"additional directory limit reached\",\"code\":\"TooManyDirectories\"}\n"
    );
}

#[test]
fn invalid_saved_directories_are_diagnosed_and_ignored() {
    let home = Home::new();
    for saved in ["relative".to_owned(), home.path("workspace")] {
        home.write_settings(&json!({
            "workspaces": {home.path("workspace"): {"additional_directories": [saved]}},
        }));
        let (stdout, stderr) = home.workspace(&[]);
        assert_eq!(
            stderr,
            "oh-fx: config user: invalid_additional_directories; key=additional_directories; additional_directories must be an array of at most 16 unique absolute directory paths for the current primary workspace\n",
            "{saved}"
        );
        assert_eq!(
            stdout,
            format!(
                "{}[workspace] additional directories: (none)\n",
                header(&home)
            ),
            "{saved}"
        );
    }
}

#[test]
fn saving_needs_a_profile_directory() {
    let home = Home::new();
    let output = home.run(&["workspace"], false);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let output = home.run(&["workspace", "add", "../shared"], false);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stderr), "oh-fx workspace: HOME is not set\n");
    let output = home.run(&["workspace", "add", "../shared", "--json"], false);
    assert_eq!(
        text(&output.stdout),
        "{\"kind\":\"workspace\",\"error\":\"HOME is not set\",\"code\":\"HomeNotSet\"}\n"
    );
    assert!(!home.settings_path().exists());
}

#[test]
fn without_home_the_saved_directories_still_list_but_cannot_change() {
    let home = Home::new();
    let shared = home.path("shared");
    let settings =
        json!({"workspaces": {home.path("workspace"): {"additional_directories": [shared]}}});
    home.write_settings(&settings);
    let output = home.run(&["workspace", "--json"], false);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        format!(
            "{{\"kind\":\"workspace\",\"action\":\"list\",\"changed\":false,\"primary_directory\":\"{}\",\"saved_suppressed\":false,\"limit\":16,\"additional_directories\":[{{\"path\":\"{shared}\",\"saved\":true,\"command_line\":false,\"available\":true,\"active\":true}}]}}\n",
            home.path("workspace")
        )
    );
    let output = home.run(&["workspace", "remove", &shared, "--json"], false);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stdout),
        "{\"kind\":\"workspace\",\"error\":\"HOME is not set\",\"code\":\"HomeNotSet\"}\n"
    );
    assert_eq!(home.settings(), settings);
}
