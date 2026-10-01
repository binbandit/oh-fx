use std::ffi::OsString;
use std::path::{Path, PathBuf};

use nix::unistd::{Uid, User};

use crate::command_environment::{Environment, Profile};

const MAX_LOGIN_SHELL_BYTES: usize = 4096;
const CAPTURED_ZSH_USER_PRELUDE: &str = "\\builtin trap - TERM; ";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("MissingLoginShell")]
    MissingLoginShell,
    #[error("RelativeShellPath")]
    RelativeShellPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Bash,
    Zsh,
}

pub fn configured_login_shell() -> Option<PathBuf> {
    let user = User::from_uid(Uid::current()).ok()??;
    let length = user.shell.as_os_str().len();
    (length != 0 && length <= MAX_LOGIN_SHELL_BYTES).then_some(user.shell)
}

pub fn environment(
    configured_login_shell: Option<&Path>,
    profile: Option<Profile>,
) -> Result<Environment, ResolveError> {
    let path = supported_login_shell(configured_login_shell)?;
    Ok(match profile.unwrap_or(Profile::User) {
        Profile::Clean => Environment::Clean(path),
        Profile::User => Environment::User(path),
    })
}

pub(crate) fn captured_invocation(environment: &Environment, command: &str) -> Vec<OsString> {
    let path = environment.shell_path();
    let kind = shell_kind(path).unwrap_or(fallback_kind());
    let mut argv: Vec<OsString> = vec![path.into()];
    let flags: &[&str] = match (environment, kind) {
        (Environment::Clean(_), ShellKind::Bash) => &["--noprofile", "--norc"],
        (Environment::Clean(_), ShellKind::Zsh) => &["-f"],
        (Environment::User(_), ShellKind::Bash) => &["--login", "-O", "expand_aliases"],
        (Environment::User(_), ShellKind::Zsh) => &["-l", "-i"],
    };
    argv.extend(flags.iter().map(OsString::from));
    argv.push("-c".into());
    argv.push(
        match (environment, kind) {
            (Environment::User(_), ShellKind::Zsh) => {
                format!("{CAPTURED_ZSH_USER_PRELUDE}{command}")
            }
            _ => command.to_owned(),
        }
        .into(),
    );
    argv
}

fn supported_login_shell(configured: Option<&Path>) -> Result<PathBuf, ResolveError> {
    let path = configured.ok_or(ResolveError::MissingLoginShell)?;
    if !path.is_absolute() {
        return Err(ResolveError::RelativeShellPath);
    }
    if shell_kind(path).is_some() {
        return Ok(path.to_path_buf());
    }
    Ok(PathBuf::from(match fallback_kind() {
        ShellKind::Zsh => "/bin/zsh",
        ShellKind::Bash => "/bin/bash",
    }))
}

fn shell_kind(path: &Path) -> Option<ShellKind> {
    match path.file_name()?.to_str()? {
        "bash" => Some(ShellKind::Bash),
        "zsh" => Some(ShellKind::Zsh),
        _ => None,
    }
}

fn fallback_kind() -> ShellKind {
    if cfg!(target_os = "macos") {
        ShellKind::Zsh
    } else {
        ShellKind::Bash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(environment: &Environment, command: &str) -> Vec<String> {
        captured_invocation(environment, command)
            .into_iter()
            .map(|word| word.into_string().unwrap())
            .collect()
    }

    #[test]
    fn captured_profiles_use_exact_non_pty_argv() {
        assert_eq!(
            argv(&Environment::Clean("/bin/bash".into()), "printf clean"),
            ["/bin/bash", "--noprofile", "--norc", "-c", "printf clean"]
        );
        assert_eq!(
            argv(&Environment::User("/bin/bash".into()), "printf user"),
            [
                "/bin/bash",
                "--login",
                "-O",
                "expand_aliases",
                "-c",
                "printf user"
            ]
        );
        assert_eq!(
            argv(&Environment::Clean("/bin/zsh".into()), "printf clean"),
            ["/bin/zsh", "-f", "-c", "printf clean"]
        );
        assert_eq!(
            argv(&Environment::User("/bin/zsh".into()), "printf user"),
            [
                "/bin/zsh",
                "-l",
                "-i",
                "-c",
                "\\builtin trap - TERM; printf user"
            ]
        );
    }

    #[test]
    fn profile_normalization_defaults_captured_execution_to_user() {
        let bash = Path::new("/bin/bash");
        let zsh = Path::new("/bin/zsh");
        assert_eq!(
            environment(Some(bash), None),
            Ok(Environment::User(bash.into()))
        );
        assert_eq!(
            environment(Some(zsh), None),
            Ok(Environment::User(zsh.into()))
        );
        assert_eq!(
            environment(Some(zsh), Some(Profile::Clean)),
            Ok(Environment::Clean(zsh.into()))
        );
        assert_eq!(
            environment(Some(zsh), Some(Profile::User)),
            Ok(Environment::User(zsh.into()))
        );
    }

    #[test]
    fn unsupported_login_shell_profiles_fall_back() {
        let fallback = PathBuf::from(if cfg!(target_os = "macos") {
            "/bin/zsh"
        } else {
            "/bin/bash"
        });
        let fish = Path::new("/opt/homebrew/bin/fish");
        assert_eq!(
            environment(Some(fish), Some(Profile::User)),
            Ok(Environment::User(fallback.clone()))
        );
        assert_eq!(
            environment(Some(fish), Some(Profile::Clean)),
            Ok(Environment::Clean(fallback))
        );
    }

    #[test]
    fn resolver_rejects_missing_and_relative_shells() {
        assert_eq!(
            environment(None, None),
            Err(ResolveError::MissingLoginShell)
        );
        assert_eq!(
            environment(Some(Path::new("bin/bash")), Some(Profile::Clean)),
            Err(ResolveError::RelativeShellPath)
        );
    }
}
