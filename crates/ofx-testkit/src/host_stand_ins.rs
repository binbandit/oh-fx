use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

const HOST_COMMANDS: [&str; 5] = ["pbcopy", "xclip", "open", "xdg-open", "tmux"];
const HOST_SESSIONS: [&str; 4] = ["TMUX", "TMUX_PANE", "HERDR_SOCKET_PATH", "HERDR_PANE_ID"];
const CALLS: &str = "calls";
const MISSING_PATH: &str =
    "a PTY child needs an explicit PATH so host commands can be replaced by stand-ins";

pub(crate) struct HostStandIns {
    directory: TempDir,
}

impl HostStandIns {
    pub(crate) fn install(command: &mut Command) -> io::Result<Self> {
        Self::install_within(command, &env::temp_dir().canonicalize()?)
    }

    fn install_within(command: &mut Command, scratch: &Path) -> io::Result<Self> {
        let search = explicit_value(command, "PATH")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, MISSING_PATH))?;
        let directory = tempfile::Builder::new()
            .prefix("ofx-host-stand-ins-")
            .tempdir()?;
        let calls = directory.path().join(CALLS);
        for name in HOST_COMMANDS {
            if reaches_the_host(name, &search, scratch) {
                write_stand_in(&directory.path().join(name), &calls)?;
            }
        }
        let mut guarded = OsString::from(directory.path());
        guarded.push(":");
        guarded.push(&search);
        command.env("PATH", guarded);
        for name in HOST_SESSIONS {
            if !command.get_envs().any(|(key, _)| key == OsStr::new(name)) {
                command.env_remove(name);
            }
        }
        Ok(Self { directory })
    }

    pub(crate) fn calls(&self) -> String {
        fs::read_to_string(self.directory.path().join(CALLS)).unwrap_or_default()
    }
}

fn explicit_value(command: &Command, name: &str) -> Option<OsString> {
    command
        .get_envs()
        .find(|(key, _)| *key == OsStr::new(name))
        .and_then(|(_, value)| value.map(OsStr::to_owned))
}

fn reaches_the_host(name: &str, search: &OsStr, scratch: &Path) -> bool {
    for directory in env::split_paths(search) {
        if !directory.is_absolute() {
            return true;
        }
        let candidate = directory.join(name);
        if executable(&candidate) {
            return !candidate
                .canonicalize()
                .is_ok_and(|found| found.starts_with(scratch));
        }
    }
    false
}

fn executable(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn write_stand_in(path: &Path, calls: &Path) -> io::Result<()> {
    fs::write(
        path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"${{0##*/}} $*\" >> '{}'\nexit 1\n",
            calls.display()
        ),
    )?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use super::*;

    fn command(directory: &Path, name: &str, mode: u32) {
        let path = directory.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn joined(directories: &[&Path]) -> OsString {
        env::join_paths(directories).unwrap()
    }

    #[test]
    fn a_child_without_an_explicit_path_is_refused() {
        let mut inherited = Command::new("/bin/sh");
        let mut cleared = Command::new("/bin/sh");
        cleared.env_clear();
        let mut removed = Command::new("/bin/sh");
        removed.env_remove("PATH");
        for command in [&mut inherited, &mut cleared, &mut removed] {
            let Err(refused) = HostStandIns::install(command) else {
                panic!("a child without an explicit PATH was accepted");
            };
            assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
            assert_eq!(refused.to_string(), MISSING_PATH);
        }
    }

    #[test]
    fn only_a_command_that_resolves_outside_the_scratch_root_reaches_the_host() {
        let scratch = tempfile::tempdir().unwrap();
        let scratch_root = scratch.path().canonicalize().unwrap();
        let fakes = scratch_root.join("fakes");
        let blocked = scratch_root.join("blocked");
        let host = tempfile::tempdir().unwrap();
        let host_root = host.path().canonicalize().unwrap();
        for directory in [&fakes, &blocked] {
            fs::create_dir(directory).unwrap();
        }
        command(&fakes, "pbcopy", 0o755);
        command(&blocked, "pbcopy", 0o644);
        command(&host_root, "pbcopy", 0o755);
        for (search, reaches) in [
            (joined(&[&host_root]), true),
            (joined(&[&fakes, &host_root]), false),
            (joined(&[&host_root, &fakes]), true),
            (joined(&[&blocked, &host_root]), true),
            (joined(&[&blocked, &fakes]), false),
            (joined(&[&blocked]), false),
            (OsString::from(format!(":{}", fakes.display())), true),
            (OsString::from(format!("bin:{}", fakes.display())), true),
        ] {
            assert_eq!(
                reaches_the_host("pbcopy", &search, &scratch_root),
                reaches,
                "{search:?}"
            );
        }
        assert!(!reaches_the_host(
            "open",
            &joined(&[&host_root]),
            &scratch_root
        ));
    }

    #[test]
    fn the_guarded_path_puts_the_stand_ins_first_and_each_one_reaches_the_host() {
        let mut child = Command::new("/bin/sh");
        child.env("PATH", "/usr/bin:/bin");
        let stand_ins = HostStandIns::install(&mut child).unwrap();
        let guarded = explicit_value(&child, "PATH").unwrap();
        let mut entries = env::split_paths(&guarded);
        assert_eq!(entries.next().as_deref(), Some(stand_ins.directory.path()));
        assert_eq!(
            entries.collect::<Vec<_>>(),
            env::split_paths("/usr/bin:/bin").collect::<Vec<_>>()
        );
        let scratch = env::temp_dir().canonicalize().unwrap();
        for name in HOST_COMMANDS {
            assert_eq!(
                stand_ins.directory.path().join(name).exists(),
                reaches_the_host(name, OsStr::new("/usr/bin:/bin"), &scratch),
                "{name}"
            );
        }
    }

    #[test]
    fn a_host_command_on_the_path_is_answered_by_its_stand_in() {
        let scratch = tempfile::tempdir().unwrap();
        let scratch_root = scratch.path().canonicalize().unwrap();
        let host = tempfile::tempdir().unwrap();
        let host_root = host.path().canonicalize().unwrap();
        let reached = host_root.join("reached");
        let open = host_root.join("open");
        fs::write(&open, format!("#!/bin/sh\ntouch '{}'\n", reached.display())).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
        let mut child = Command::new("/bin/sh");
        child
            .args(["-c", "open 'https://example.invalid/a b'"])
            .env(
                "PATH",
                joined(&[&host_root, Path::new("/usr/bin"), Path::new("/bin")]),
            )
            .stdin(Stdio::null());
        let stand_ins = HostStandIns::install_within(&mut child, &scratch_root).unwrap();
        assert_eq!(child.status().unwrap().code(), Some(1));
        assert!(!reached.exists());
        assert_eq!(stand_ins.calls(), "open https://example.invalid/a b\n");
    }

    #[test]
    fn host_session_variables_are_removed_unless_the_test_sets_them() {
        let mut child = Command::new("/bin/sh");
        child
            .env("PATH", "/usr/bin:/bin")
            .env("TMUX", "/tmp/tmux-1/default,1,0")
            .env_remove("HERDR_PANE_ID");
        let _stand_ins = HostStandIns::install(&mut child).unwrap();
        let mut seen: Vec<(String, Option<String>)> = child
            .get_envs()
            .filter(|(key, _)| *key != OsStr::new("PATH"))
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            [
                ("HERDR_PANE_ID".to_owned(), None),
                ("HERDR_SOCKET_PATH".to_owned(), None),
                (
                    "TMUX".to_owned(),
                    Some("/tmp/tmux-1/default,1,0".to_owned())
                ),
                ("TMUX_PANE".to_owned(), None),
            ]
        );
    }
}
