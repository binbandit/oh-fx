use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

pub(crate) fn enter_repository_root() -> Result<(), String> {
    let mut command = Command::new("git");
    command.args(["rev-parse", "--show-toplevel"]);
    for (variable, _) in env::vars_os() {
        if variable.to_string_lossy().starts_with("GIT_") {
            command.env_remove(variable);
        }
    }
    let root = git_output(command)?;
    env::set_current_dir(root.trim()).map_err(|error| format!("failed to enter {root}: {error}"))
}

pub(crate) fn listed_files() -> Result<Vec<String>, String> {
    let output = git(&["ls-files", "--cached", "--others", "--exclude-standard"])?;
    Ok(output.lines().map(str::to_owned).collect())
}

pub(crate) fn git(args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    command.args(args);
    git_output(command)
}

fn git_output(mut command: Command) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("failed to run git: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

pub(crate) fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("failed to read {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_root_ignores_inherited_git_repository_overrides() {
        const ROOT: &str = "OH_FX_TEST_REPOSITORY_ROOT";
        if let Some(root) = env::var_os(ROOT) {
            enter_repository_root().unwrap();
            assert_eq!(
                env::current_dir().unwrap(),
                Path::new(&root).canonicalize().unwrap()
            );
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let mut init = Command::new("git");
        for (variable, _) in env::vars_os() {
            if variable.to_string_lossy().starts_with("GIT_") {
                init.env_remove(variable);
            }
        }
        assert!(
            init.current_dir(root.path())
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        fs::create_dir(root.path().join("nested")).unwrap();
        let output = Command::new(env::current_exe().unwrap())
            .current_dir(root.path().join("nested"))
            .args(["--exact", "workspace_files::tests::repository_root_ignores_inherited_git_repository_overrides", "--nocapture"])
            .env(ROOT, root.path())
            .env("GIT_DIR", "/not/a/repository")
            .env("GIT_WORK_TREE", "/not/a/worktree")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.worktree")
            .env("GIT_CONFIG_VALUE_0", "/not/a/worktree")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
