use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

pub(crate) fn enter_repository_root() -> Result<(), String> {
    let root = git(&["rev-parse", "--show-toplevel"])?;
    env::set_current_dir(root.trim()).map_err(|error| format!("failed to enter {root}: {error}"))
}

pub(crate) fn listed_files() -> Result<Vec<String>, String> {
    let output = git(&["ls-files", "--cached", "--others", "--exclude-standard"])?;
    Ok(output.lines().map(str::to_owned).collect())
}

pub(crate) fn git(args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
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
