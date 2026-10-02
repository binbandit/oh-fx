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

const HEAD: &str = "head";
const BASE: &str = "base";

pub(crate) struct Builds {
    repository: PathBuf,
    target_dir: PathBuf,
    footprint: PathBuf,
    cargo_home: Option<PathBuf>,
}

impl Builds {
    pub(crate) fn prepare(invoked_from: &Path) -> Result<Self, String> {
        let target_dir = env::var_os("CARGO_TARGET_DIR")
            .map_or_else(|| PathBuf::from("target"), |path| invoked_from.join(path));
        let repository =
            env::current_dir().map_err(|error| format!("read the current directory: {error}"))?;
        let target_dir = repository.join(target_dir);
        Self::at(repository, target_dir, cargo_home())
    }

    fn at(
        repository: PathBuf,
        target_dir: PathBuf,
        cargo_home: Option<PathBuf>,
    ) -> Result<Self, String> {
        let builds = Self {
            repository,
            footprint: target_dir.join("footprint"),
            target_dir,
            cargo_home,
        };
        for side in [HEAD, BASE] {
            remove_if_present(&builds.staged(side))?;
        }
        Ok(builds)
    }

    pub(crate) fn head(&self) -> Result<PathBuf, String> {
        let mut command = self.command(Path::new("."), HEAD, |name| env::var_os(name))?;
        cargo_build(&mut command, Path::new("."))?;
        self.stage(Path::new("."), HEAD)
    }

    pub(crate) fn base(&self, commit: &str) -> Result<PathBuf, String> {
        let source = base_source(&temp_dir()?, &self.repository, &self.target_dir)?;
        let mut command = self.command(&source, BASE, |name| env::var_os(name))?;
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
        let staged = cargo_build(&mut command, &source).and_then(|()| self.stage(&source, BASE));
        if repository::git(&["worktree", "remove", "--force", &source_text]).is_err() {
            let _ = fs::remove_dir_all(&source);
            let _ = repository::git(&["worktree", "prune"]);
        }
        staged
    }

    fn command(
        &self,
        source: &Path,
        side: &str,
        current: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Command, String> {
        let exported = config_variables(&self.repository, self.cargo_home.as_deref())?;
        let hidden = set_by_head(&exported, current);
        Ok(cargo_command(source, &self.build_dir(side), &hidden))
    }

    fn build_dir(&self, side: &str) -> PathBuf {
        self.footprint.join("targets").join(side)
    }

    fn staged(&self, side: &str) -> PathBuf {
        self.footprint.join(side)
    }

    fn stage(&self, source: &Path, side: &str) -> Result<PathBuf, String> {
        let staged = self.staged(side);
        create_dir(&staged)?;
        copy(
            &self
                .build_dir(side)
                .join(TARGET)
                .join("release")
                .join(BINARY),
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

fn cargo_command(source: &Path, build_dir: &Path, hidden: &[String]) -> Command {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut command = Command::new(cargo);
    for variable in hidden {
        command.env_remove(variable);
    }
    build_in(&mut command, build_dir)
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
        .env("OH_FX_RELEASE_VERSION", RELEASE_VERSION);
    for variable in GIT_REPOSITORY_VARIABLES {
        command.env_remove(variable);
    }
    command
}

fn build_in<'a>(command: &'a mut Command, dir: &Path) -> &'a mut Command {
    command
        .env("CARGO_TARGET_DIR", dir)
        .env("CARGO_BUILD_BUILD_DIR", dir)
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
    use std::time::{Duration, SystemTime};

    use super::*;

    fn package(name: &str, dependency: Option<&str>) -> String {
        let dependencies = dependency.map_or_else(String::new, |dependency| {
            format!("[dependencies]\n{dependency} = {{ path = \"../{dependency}\" }}\n")
        });
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{dependencies}"
        )
    }

    fn checkout(root: &Path, contract: &str, session: &str) {
        let checked_out = SystemTime::UNIX_EPOCH + Duration::from_hours(24);
        let files = [
            (
                "Cargo.toml",
                "[workspace]\nresolver = \"3\"\nmembers = [\"contract\", \"session\", \"app\"]\n"
                    .to_owned(),
            ),
            ("contract/Cargo.toml", package("contract", None)),
            ("contract/src/lib.rs", contract.to_owned()),
            ("session/Cargo.toml", package("session", Some("contract"))),
            (
                "session/src/lib.rs",
                format!("pub fn describe() -> u32 {{\n    {session}\n}}\n"),
            ),
            ("app/Cargo.toml", package("app", Some("session"))),
            (
                "app/src/main.rs",
                "fn main() {\n    print!(\"{}\", session::describe());\n}\n".to_owned(),
            ),
        ];
        for (path, text) in files {
            let path = root.join(path);
            create_dir(path.parent().expect("a parent")).expect("a fixture directory");
            fs::write(&path, text).expect("a fixture file");
            fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(checked_out))
                .expect("a fixed modification time");
        }
    }

    fn build_and_run(
        builds: &Builds,
        source: &Path,
        side: &str,
        shared_build_dir: &Path,
    ) -> String {
        let mut command = Command::new(env!("CARGO"));
        command
            .args(["run", "--quiet", "--package", "app"])
            .current_dir(source)
            .env("CARGO_BUILD_BUILD_DIR", shared_build_dir);
        let output = build_in(&mut command, &builds.build_dir(side))
            .output()
            .expect("cargo runs");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("UTF-8 output")
    }

    #[test]
    fn alternating_builds_of_two_trees_each_run_their_own_crates() {
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let head = scratch.path().join("head");
        let base = scratch.path().join("base");
        checkout(
            &head,
            "pub fn value() -> u32 {\n    2\n}\n\npub fn added() -> u32 {\n    40\n}\n",
            "contract::value() + contract::added()",
        );
        checkout(
            &base,
            "pub fn value() -> u32 {\n    1\n}\n",
            "contract::value()",
        );
        let builds = Builds::at(
            scratch.path().to_path_buf(),
            scratch.path().join("target"),
            None,
        )
        .expect("prepared builds");
        let shared = scratch.path().join("shared-build-dir");
        let outputs: Vec<_> = [(&head, HEAD), (&base, BASE), (&head, HEAD)]
            .into_iter()
            .map(|(source, side)| build_and_run(&builds, source, side, &shared))
            .collect();
        assert_eq!(outputs, ["42", "1", "42"]);
    }

    #[test]
    fn each_side_builds_in_a_directory_of_its_own_that_outlives_the_staged_releases() {
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let prepare = || {
            Builds::at(
                scratch.path().to_path_buf(),
                scratch.path().join("target"),
                None,
            )
            .expect("prepared builds")
        };
        let builds = prepare();
        let (head, base) = (builds.build_dir(HEAD), builds.build_dir(BASE));
        assert!(!head.starts_with(&base) && !base.starts_with(&head));
        for (side, build_dir) in [(HEAD, &head), (BASE, &base)] {
            let command = builds
                .command(Path::new("."), side, |_| None)
                .expect("a build command");
            let envs: Vec<_> = command.get_envs().collect();
            for variable in ["CARGO_TARGET_DIR", "CARGO_BUILD_BUILD_DIR"] {
                assert!(envs.contains(&(OsStr::new(variable), Some(build_dir.as_os_str()))));
            }
            for dir in [build_dir.clone(), builds.staged(side)] {
                create_dir(&dir).expect("a directory");
                fs::write(dir.join("marker"), side).expect("a marker");
            }
        }
        let builds = prepare();
        for side in [HEAD, BASE] {
            assert!(builds.build_dir(side).join("marker").exists(), "{side}");
            assert!(!builds.staged(side).exists(), "{side}");
        }
    }

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
        let hidden = vec![
            "FROM_HEAD".to_owned(),
            "CARGO_TARGET_DIR".to_owned(),
            "CARGO_BUILD_BUILD_DIR".to_owned(),
        ];
        let command = cargo_command(Path::new("/base"), Path::new("/side/target"), &hidden);
        let envs: Vec<_> = command.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("FROM_HEAD"), None)));
        for variable in ["CARGO_TARGET_DIR", "CARGO_BUILD_BUILD_DIR"] {
            assert!(envs.contains(&(OsStr::new(variable), Some(OsStr::new("/side/target")))));
        }
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
            target_dir: PathBuf::from("/work/target"),
            footprint: PathBuf::from("/work/target/footprint"),
            cargo_home: None,
        };
        let current = |name: &str| match name {
            "EXPORTED" => Some(OsString::from("head")),
            "USER_SET" => Some(OsString::from("mine")),
            _ => None,
        };
        for (source, side) in [(Path::new("."), HEAD), (Path::new("/base"), BASE)] {
            let command = builds
                .command(source, side, current)
                .expect("a build command");
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
