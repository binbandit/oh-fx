use std::env;
use std::ffi::OsString;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::ErrorKind;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cargo_env::{cargo_home, config_variables, set_by_head};
use crate::repository::{self, GIT_REPOSITORY_VARIABLES};

pub(crate) const TARGET: &str = "x86_64-unknown-linux-musl";
pub(crate) const BINARY: &str = "oh-fx";
pub(crate) const ARCHIVE_FILES: [&str; 3] = [BINARY, "LICENSE", "NOTICE"];
const RELEASE_VERSION: &str = "0.1.0-dev.1";

pub(crate) struct Builds {
    repository: PathBuf,
    target_dir: PathBuf,
    staging: PathBuf,
    cargo_home: Option<PathBuf>,
}

impl Builds {
    pub(crate) fn prepare(invoked_from: &Path) -> Result<Self, String> {
        let target_dir = env::var_os("CARGO_TARGET_DIR")
            .map_or_else(|| PathBuf::from("target"), |path| invoked_from.join(path));
        let repository =
            env::current_dir().map_err(|error| format!("read the current directory: {error}"))?;
        let target_dir = repository.join(target_dir);
        let staging = target_dir.join("footprint");
        remove_if_present(&staging)?;
        create_dir(&staging)?;
        Ok(Self {
            repository,
            target_dir,
            staging,
            cargo_home: cargo_home(),
        })
    }

    pub(crate) fn head(&self) -> Result<PathBuf, String> {
        let mut command = self.command(Path::new("."), |name| env::var_os(name))?;
        cargo_build(&mut command, Path::new("."))?;
        self.stage(Path::new("."), "head")
    }

    pub(crate) fn base(&self, commit: &str) -> Result<PathBuf, String> {
        let source = base_source(&temp_dir()?, &self.repository, &self.target_dir)?;
        let mut command = self.command(&source, |name| env::var_os(name))?;
        let source_text = source.to_string_lossy().into_owned();
        remove_if_present(&source)?;
        repository::git(&["worktree", "prune"])?;
        claim(&source)?;
        repository::git(&[
            "worktree",
            "add",
            "--detach",
            "--quiet",
            &source_text,
            commit,
        ])?;
        let staged = cargo_build(&mut command, &source).and_then(|()| self.stage(&source, "base"));
        if repository::git(&["worktree", "remove", "--force", &source_text]).is_err() {
            let _ = fs::remove_dir_all(&source);
            let _ = repository::git(&["worktree", "prune"]);
        }
        staged
    }

    fn command(
        &self,
        source: &Path,
        current: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Command, String> {
        let exported = config_variables(&self.repository, self.cargo_home.as_deref())?;
        let hidden = set_by_head(&exported, current);
        Ok(cargo_command(source, &self.target_dir, &hidden))
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

fn cargo_build(command: &mut Command, source: &Path) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|error| format!("run cargo build: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo build failed in {}", source.display()))
    }
}

fn cargo_command(source: &Path, target_dir: &Path, hidden: &[String]) -> Command {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut command = Command::new(cargo);
    for variable in hidden {
        command.env_remove(variable);
    }
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
    command
}

fn temp_dir() -> Result<PathBuf, String> {
    let temp_dir = env::temp_dir();
    fs::canonicalize(&temp_dir).map_err(|error| {
        format!(
            "resolve the temp directory \"{}\": {error}",
            temp_dir.display()
        )
    })
}

fn base_source(temp_dir: &Path, repository: &Path, target_dir: &Path) -> Result<PathBuf, String> {
    if temp_dir.starts_with(repository) {
        return Err(format!(
            "the temp directory {} is inside the repository, where the base would build with the head's cargo config; set TMPDIR outside {}",
            temp_dir.display(),
            repository.display()
        ));
    }
    let mut hasher = DefaultHasher::new();
    target_dir.hash(&mut hasher);
    Ok(temp_dir.join(format!("oh-fx-footprint-base-{:016x}", hasher.finish())))
}

fn claim(path: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|error| format!("create {}: {error}", path.display()))
}

fn remove_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => {
            Err(format!("clear {}: {error}", path.display()))
        }
        _ => Ok(()),
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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    #[test]
    fn the_base_checkout_sits_in_the_temp_directory_keyed_by_the_target_dir() {
        let temp_dir = Path::new("/tmp");
        let repository = Path::new("/work/oh-fx");
        let target_dir = repository.join("target");
        let source = base_source(temp_dir, repository, &target_dir).expect("a separate temp dir");
        assert_eq!(source.parent(), Some(temp_dir));
        assert_eq!(
            base_source(temp_dir, repository, &target_dir),
            Ok(source.clone())
        );
        assert_ne!(
            base_source(temp_dir, repository, Path::new("/elsewhere/target")),
            Ok(source)
        );
    }

    #[test]
    fn refuses_a_temp_directory_inside_the_repository() {
        let repository = Path::new("/work/oh-fx");
        for temp_dir in [repository.to_path_buf(), repository.join("tmp")] {
            assert!(
                base_source(&temp_dir, repository, &repository.join("target")).is_err(),
                "{}",
                temp_dir.display()
            );
        }
    }

    #[test]
    fn a_build_drops_the_hidden_variables_but_keeps_its_own() {
        let hidden = vec!["FROM_HEAD".to_owned(), "CARGO_TARGET_DIR".to_owned()];
        let command = cargo_command(Path::new("/base"), Path::new("/shared/target"), &hidden);
        let envs: Vec<_> = command.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("FROM_HEAD"), None)));
        assert!(envs.contains(&(
            OsStr::new("CARGO_TARGET_DIR"),
            Some(OsStr::new("/shared/target"))
        )));
        assert_eq!(command.get_current_dir(), Some(Path::new("/base")));
    }

    #[test]
    fn both_builds_drop_what_the_head_config_exported() {
        let repository = tempfile::tempdir().expect("a scratch repository");
        let config = repository.path().join(".cargo/config.toml");
        create_dir(config.parent().expect("a parent")).expect("a config directory");
        fs::write(&config, "[env]\nEXPORTED = \"head\"\nUSER_SET = \"head\"\n")
            .expect("a config file");
        let builds = Builds {
            repository: repository.path().to_path_buf(),
            target_dir: PathBuf::from("/shared/target"),
            staging: PathBuf::from("/shared/target/footprint"),
            cargo_home: None,
        };
        let current = |name: &str| match name {
            "EXPORTED" => Some(OsString::from("head")),
            "USER_SET" => Some(OsString::from("mine")),
            _ => None,
        };
        for source in [Path::new("."), Path::new("/base")] {
            let command = builds.command(source, current).expect("a build command");
            let envs: Vec<_> = command.get_envs().collect();
            assert!(envs.contains(&(OsStr::new("EXPORTED"), None)));
            assert!(!envs.iter().any(|(name, _)| *name == "USER_SET"));
        }
    }

    #[test]
    fn claims_only_a_path_nothing_else_holds() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let elsewhere = dir.path().join("elsewhere");
        create_dir(&elsewhere).expect("a directory to point at");
        let planted = dir.path().join("planted");
        symlink(&elsewhere, &planted).expect("a planted symlink");
        assert!(claim(&planted).is_err());
        assert!(claim(&elsewhere).is_err());
        let claimed = dir.path().join("claimed");
        claim(&claimed).expect("a fresh directory");
        let mode = fs::metadata(&claimed)
            .expect("the claimed directory")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
