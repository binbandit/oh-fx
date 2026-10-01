use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::repository::{self, GIT_REPOSITORY_VARIABLES};

pub(crate) const TARGET: &str = "x86_64-unknown-linux-musl";
pub(crate) const BINARY: &str = "oh-fx";
pub(crate) const ARCHIVE_FILES: [&str; 3] = [BINARY, "LICENSE", "NOTICE"];
const RELEASE_VERSION: &str = "0.1.0-dev.1";

pub(crate) struct Builds {
    target_dir: PathBuf,
    staging: PathBuf,
}

impl Builds {
    pub(crate) fn prepare(invoked_from: &Path) -> Result<Self, String> {
        let target_dir = env::var_os("CARGO_TARGET_DIR")
            .map_or_else(|| PathBuf::from("target"), |path| invoked_from.join(path));
        let target_dir = env::current_dir()
            .map_err(|error| format!("read the current directory: {error}"))?
            .join(target_dir);
        let staging = target_dir.join("footprint");
        match fs::remove_dir_all(&staging) {
            Err(error) if error.kind() != ErrorKind::NotFound => {
                return Err(format!("clear {}: {error}", staging.display()));
            }
            _ => {}
        }
        create_dir(&staging)?;
        Ok(Self {
            target_dir,
            staging,
        })
    }

    pub(crate) fn head(&self) -> Result<PathBuf, String> {
        cargo_build(Path::new("."), &self.target_dir)?;
        self.stage(Path::new("."), "head")
    }

    pub(crate) fn base(&self, commit: &str) -> Result<PathBuf, String> {
        let source = self.staging.join("base-source");
        let source_text = source.to_string_lossy().into_owned();
        repository::git(&["worktree", "prune"])?;
        repository::git(&[
            "worktree",
            "add",
            "--detach",
            "--quiet",
            &source_text,
            commit,
        ])?;
        let staged =
            cargo_build(&source, &self.target_dir).and_then(|()| self.stage(&source, "base"));
        if repository::git(&["worktree", "remove", "--force", &source_text]).is_err() {
            let _ = fs::remove_dir_all(&source);
            let _ = repository::git(&["worktree", "prune"]);
        }
        staged
    }

    fn stage(&self, source: &Path, name: &str) -> Result<PathBuf, String> {
        let staged = self.staging.join(name);
        create_dir(&staged)?;
        copy(
            &self.target_dir.join(TARGET).join("release").join(BINARY),
            &staged.join(BINARY),
        )?;
        for file in &ARCHIVE_FILES[1..] {
            copy(&source.join(file), &staged.join(file))?;
        }
        Ok(staged)
    }
}

fn cargo_build(source: &Path, target_dir: &Path) -> Result<(), String> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut command = Command::new(cargo);
    command
        .args([
            "build",
            "--release",
            "--locked",
            "--package",
            BINARY,
            "--target",
            TARGET,
        ])
        .current_dir(source)
        .env("CARGO_TARGET_DIR", target_dir)
        .env("OH_FX_RELEASE_VERSION", RELEASE_VERSION);
    for variable in GIT_REPOSITORY_VARIABLES {
        command.env_remove(variable);
    }
    let status = command
        .status()
        .map_err(|error| format!("run cargo build: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo build failed in {}", source.display()))
    }
}

fn create_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|error| format!("create {}: {error}", path.display()))
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    fs::copy(from, to)
        .map(drop)
        .map_err(|error| format!("copy {} to {}: {error}", from.display(), to.display()))
}
