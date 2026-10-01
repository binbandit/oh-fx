use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ofx_testkit::{Reply, chat_text_events, chat_tool_call_events};
use tempfile::TempDir;

use crate::repository::GIT_REPOSITORY_VARIABLES;

pub(crate) const ASK_PROMPT: &str = "hi";
pub(crate) const TOOL_PROMPT: &str = "survey the workspace";
pub(crate) const STREAM_SHORT: u64 = 1_000;
pub(crate) const STREAM_LONG: u64 = 5_000;
const MODULES: usize = 40;
const TOOL_SCRIPT: [(&str, &str); 6] = [
    ("read_file", r#"{"path":"src/lib.rs"}"#),
    ("grep_files", r#"{"pattern":"needle"}"#),
    ("read_file", r#"{"path":"docs/guide.md"}"#),
    ("grep_files", r#"{"pattern":"fn value"}"#),
    ("read_file", r#"{"path":"src/module_7.rs"}"#),
    ("grep_files", r#"{"pattern":"footprint"}"#),
];

pub(crate) struct Profile {
    root: TempDir,
}

impl Profile {
    pub(crate) fn create() -> Result<Self, String> {
        let root = tempfile::tempdir().map_err(|error| format!("create a profile: {error}"))?;
        let profile = Self { root };
        for directory in ["home", "config/oh-fx", "data", "state", "cache"] {
            create_dir(&profile.path().join(directory))?;
        }
        write_workspace(&profile.workspace())?;
        Ok(profile)
    }

    pub(crate) fn path(&self) -> &Path {
        self.root.path()
    }

    pub(crate) fn connect(&self, base_url: &str) -> Result<(), String> {
        let settings = format!(
            r#"{{"provider":"footprint","models":{{"footprint":"footprint-model"}},"providers":{{"footprint":{{"protocol":"openai-chat-completions","base_url":"{base_url}","auth":{{"type":"none"}}}}}}}}"#
        );
        write(&self.path().join("config/oh-fx/settings.json"), &settings)
    }

    pub(crate) fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .current_dir(self.workspace())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path().join("home"))
            .env("XDG_CONFIG_HOME", self.path().join("config"))
            .env("XDG_DATA_HOME", self.path().join("data"))
            .env("XDG_STATE_HOME", self.path().join("state"))
            .env("XDG_CACHE_HOME", self.path().join("cache"))
            .env("LC_ALL", "C")
            .env("TERM", "dumb")
            .env("OH_FX_AUTO_UPGRADE", "0");
        command
    }

    fn workspace(&self) -> PathBuf {
        self.path().join("workspace")
    }
}

pub(crate) fn ask_replies() -> Vec<Reply> {
    vec![Reply::sse(&chat_text_events(&["Hello", " there."]))]
}

pub(crate) fn stream_replies(deltas: u64) -> Vec<Reply> {
    let pieces: Vec<String> = (0..deltas).map(stream_piece).collect();
    let pieces: Vec<&str> = pieces.iter().map(String::as_str).collect();
    vec![Reply::sse(&chat_text_events(&pieces))]
}

pub(crate) fn tool_replies() -> Vec<Reply> {
    let mut replies: Vec<Reply> = TOOL_SCRIPT
        .iter()
        .enumerate()
        .map(|(index, (name, arguments))| {
            Reply::sse(&chat_tool_call_events(
                &format!("call_{index}"),
                name,
                arguments,
            ))
        })
        .collect();
    replies.push(Reply::sse(&chat_text_events(&["Surveyed."])));
    replies
}

fn stream_piece(index: u64) -> String {
    if index.is_multiple_of(200) {
        format!("\n## Section {index}\n\n- item `code{index}` **bold**\n")
    } else if index % 25 == 24 {
        format!("word{index}\n\n")
    } else {
        format!("word{index} ")
    }
}

fn write_workspace(root: &Path) -> Result<(), String> {
    create_dir(&root.join("src"))?;
    create_dir(&root.join("docs"))?;
    write(
        &root.join("Cargo.toml"),
        "[package]\nname = \"footprint-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    let mut library = String::new();
    for module in 0..MODULES {
        let _ = writeln!(library, "mod module_{module};");
        write(
            &root.join(format!("src/module_{module}.rs")),
            &module_source(module),
        )?;
    }
    write(&root.join("src/lib.rs"), &library)?;
    let mut guide = String::from("# Guide\n\n");
    for section in 0..60 {
        let _ = writeln!(
            guide,
            "Section {section} explains how the footprint fixture keeps a needle in place.\n"
        );
    }
    write(&root.join("docs/guide.md"), &guide)?;
    git(root, &["init", "--quiet"])?;
    git(root, &["add", "--all"])
}

fn module_source(module: usize) -> String {
    let mut source = String::new();
    for function in 0..30 {
        let _ = writeln!(
            source,
            "pub fn value_{function}() -> usize {{\n    {module} * {function}\n}}\n"
        );
    }
    if module.is_multiple_of(4) {
        source.push_str("pub const NEEDLE: &str = \"needle\";\n");
    }
    source
}

fn git(root: &Path, args: &[&str]) -> Result<(), String> {
    let mut command = Command::new("git");
    command
        .args(["-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(root);
    for variable in GIT_REPOSITORY_VARIABLES {
        command.env_remove(variable);
    }
    let output = command
        .output()
        .map_err(|error| format!("run git {}: {error}", args.join(" ")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn create_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|error| format!("create {}: {error}", path.display()))
}

fn write(path: &Path, contents: &str) -> Result<(), String> {
    fs::write(path, contents).map_err(|error| format!("write {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_pieces_mix_prose_breaks_and_markdown() {
        assert!(stream_piece(0).starts_with("\n## Section 0"));
        assert_eq!(stream_piece(24), "word24\n\n");
        assert_eq!(stream_piece(1), "word1 ");
    }

    #[test]
    fn the_tool_script_ends_with_an_answer() {
        assert_eq!(tool_replies().len(), TOOL_SCRIPT.len() + 1);
        assert_eq!(stream_replies(3).len(), 1);
    }
}
