use std::ffi::OsString;
use std::path::{Path, PathBuf};

use nix::unistd::{Uid, User};

use crate::command_environment::{Environment, Profile};
use crate::native_session::{CONTROL_MODE, MarkerKind};

const MAX_LOGIN_SHELL_BYTES: usize = 4096;
const CAPTURED_ZSH_USER_PRELUDE: &str = "\\builtin trap - TERM; ";
const BOOTSTRAP_HEAD: &str = "set +x; oh_fx_terminal_command=$(< ";
const BOOTSTRAP_TAIL: &str = " || exit 125; builtin eval -- \"$oh_fx_terminal_command\"; oh_fx_terminal_status=$?; exit \"$oh_fx_terminal_status\"\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("MissingLoginShell")]
    MissingLoginShell,
    #[error("RelativeShellPath")]
    RelativeShellPath,
    #[error("UnsupportedShell")]
    UnsupportedShell,
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

pub(crate) fn interactive_invocation(
    path: &Path,
    clean_start: bool,
) -> Result<Vec<OsString>, ResolveError> {
    if !path.is_absolute() {
        return Err(ResolveError::RelativeShellPath);
    }
    let kind = shell_kind(path).ok_or(ResolveError::UnsupportedShell)?;
    let flags: &[&str] = match (kind, clean_start) {
        (ShellKind::Bash, true) => &["--noprofile", "--norc", "-i"],
        (ShellKind::Bash, false) => &["--login", "-i"],
        (ShellKind::Zsh, true) => &["-f", "-i"],
        (ShellKind::Zsh, false) => &["-l", "-i"],
    };
    let mut argv: Vec<OsString> = vec![path.into()];
    argv.extend(flags.iter().map(OsString::from));
    Ok(argv)
}

pub(crate) fn build_bootstrap(
    executable: &[u8],
    control_path: &str,
    nonce: &str,
    command_path: &str,
) -> Vec<u8> {
    let mut output = BOOTSTRAP_HEAD.as_bytes().to_vec();
    push_shell_word(&mut output, command_path.as_bytes());
    output.extend_from_slice(b") || exit 125; ");
    push_marker(
        &mut output,
        executable,
        control_path,
        nonce,
        MarkerKind::ShellReady,
    );
    output.extend_from_slice(b" || exit 125; ");
    push_marker(
        &mut output,
        executable,
        control_path,
        nonce,
        MarkerKind::CommandStarted,
    );
    output.extend_from_slice(BOOTSTRAP_TAIL.as_bytes());
    output
}

pub(crate) fn build_source_command(bootstrap_path: &str) -> Vec<u8> {
    let mut output = b". ".to_vec();
    push_shell_word(&mut output, bootstrap_path.as_bytes());
    output.push(b'\n');
    output
}

fn push_marker(
    output: &mut Vec<u8>,
    executable: &[u8],
    control_path: &str,
    nonce: &str,
    kind: MarkerKind,
) {
    push_shell_word(output, executable);
    for word in [CONTROL_MODE, control_path, nonce, kind.event()] {
        output.push(b' ');
        push_shell_word(output, word.as_bytes());
    }
}

fn push_shell_word(output: &mut Vec<u8>, word: &[u8]) {
    output.push(b'\'');
    for &byte in word {
        if byte == b'\'' {
            output.extend_from_slice(b"'\"'\"'");
        } else {
            output.push(byte);
        }
    }
    output.push(b'\'');
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

    fn interactive(path: &str, clean_start: bool) -> Result<Vec<String>, ResolveError> {
        interactive_invocation(Path::new(path), clean_start).map(|argv| {
            argv.into_iter()
                .map(|word| word.into_string().unwrap())
                .collect()
        })
    }

    #[test]
    fn resolver_builds_bash_and_zsh_interactive_argv() {
        assert_eq!(
            interactive("/bin/bash", false),
            Ok(vec!["/bin/bash".to_owned(), "--login".into(), "-i".into()])
        );
        assert_eq!(
            interactive("/bin/zsh", false),
            Ok(vec!["/bin/zsh".to_owned(), "-l".into(), "-i".into()])
        );
    }

    #[test]
    fn resolver_makes_clean_startup_explicit() {
        assert_eq!(
            interactive("/usr/local/bin/bash", true),
            Ok(vec![
                "/usr/local/bin/bash".to_owned(),
                "--noprofile".into(),
                "--norc".into(),
                "-i".into()
            ])
        );
        assert_eq!(
            interactive("/bin/zsh", true),
            Ok(vec!["/bin/zsh".to_owned(), "-f".into(), "-i".into()])
        );
    }

    #[test]
    fn interactive_resolution_rejects_relative_and_unsupported_shells() {
        assert_eq!(
            interactive("zsh", false),
            Err(ResolveError::RelativeShellPath)
        );
        assert_eq!(
            interactive("/bin/fish", false),
            Err(ResolveError::UnsupportedShell)
        );
        assert_eq!(
            interactive("/opt/homebrew/bin/fish", true),
            Err(ResolveError::UnsupportedShell)
        );
    }

    #[test]
    fn bootstrap_quotes_private_paths_and_separates_command_completion() {
        assert_eq!(
            String::from_utf8(build_bootstrap(
                b"/tmp/fx'bin",
                "/tmp/control",
                "nonce",
                "/tmp/command"
            ))
            .unwrap(),
            concat!(
                "set +x; oh_fx_terminal_command=$(< '/tmp/command') || exit 125; ",
                "'/tmp/fx'\"'\"'bin' '--oh-fx-internal-terminal-control' '/tmp/control' 'nonce' 'shell-ready' || exit 125; ",
                "'/tmp/fx'\"'\"'bin' '--oh-fx-internal-terminal-control' '/tmp/control' 'nonce' 'command-started' || exit 125; ",
                "builtin eval -- \"$oh_fx_terminal_command\"; oh_fx_terminal_status=$?; exit \"$oh_fx_terminal_status\"\n",
            )
        );
        assert_eq!(
            build_source_command("/tmp/bootstrap'file"),
            b". '/tmp/bootstrap'\"'\"'file'\n"
        );
        let raw = build_bootstrap(b"/opt/\xffbin", "/c", "n", "/m");
        assert!(raw.windows(11).any(|window| window == b"'/opt/\xffbin'"));
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
