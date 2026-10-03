use std::env;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use ofx_text::lowercase_hex;
use rustix::fs::{FileType, Mode, OFlags};
use tokio::process::Command;
use tokio::time::timeout;

const MAX_CONTAINER_ID_BYTES: u64 = 128;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prepared {
    pub(crate) argv: Vec<String>,
    pub(crate) cleanup: Option<Cleanup>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cleanup {
    docker_command: String,
    cidfile_path: PathBuf,
    environment: Vec<(String, String)>,
}

impl Cleanup {
    pub(crate) fn with_environment(mut self, environment: Vec<(String, String)>) -> Self {
        self.environment = environment;
        self
    }

    pub(crate) async fn run(self) {
        let container_id = read_container_id(&self.cidfile_path);
        let _ = fs::remove_file(&self.cidfile_path);
        let Ok(container_id) = container_id else {
            return;
        };
        let mut command = Command::new(&self.docker_command);
        command
            .args(["rm", "-f", &container_id])
            .envs(self.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Ok(mut child) = command.spawn() {
            let _ = timeout(CLEANUP_TIMEOUT, child.wait()).await;
        }
    }
}

pub(crate) fn prepare(argv: Vec<String>) -> io::Result<Prepared> {
    if !is_direct_docker_run(&argv) || has_cidfile(&argv) {
        return Ok(Prepared {
            argv,
            cleanup: None,
        });
    }
    let temp_root = temporary_root();
    if !temp_root.is_absolute() {
        return Ok(Prepared {
            argv,
            cleanup: None,
        });
    }
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| io::Error::from(io::ErrorKind::Unsupported))?;
    let cidfile_path = temp_root.join(format!("oh-fx-mcp-{}.cid", lowercase_hex(&nonce)));
    let mut prepared_argv = Vec::with_capacity(argv.len() + 2);
    prepared_argv.extend_from_slice(&argv[..2]);
    prepared_argv.push("--cidfile".to_owned());
    prepared_argv.push(cidfile_path.to_string_lossy().into_owned());
    prepared_argv.extend_from_slice(&argv[2..]);
    Ok(Prepared {
        cleanup: Some(Cleanup {
            docker_command: argv[0].clone(),
            cidfile_path,
            environment: Vec::new(),
        }),
        argv: prepared_argv,
    })
}

fn is_direct_docker_run(argv: &[String]) -> bool {
    if argv.len() < 2 || argv[1] != "run" {
        return false;
    }
    let command = Path::new(&argv[0])
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    command.eq_ignore_ascii_case("docker") || command.eq_ignore_ascii_case("docker.exe")
}

fn has_cidfile(argv: &[String]) -> bool {
    argv[2..]
        .iter()
        .any(|arg| arg == "--cidfile" || arg.starts_with("--cidfile="))
}

fn temporary_root() -> PathBuf {
    env::var_os("TMPDIR")
        .or_else(|| env::var_os("TEMP"))
        .or_else(|| env::var_os("TMP"))
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
}

fn read_container_id(path: &Path) -> io::Result<String> {
    let invalid = || io::Error::from(io::ErrorKind::InvalidData);
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let fd = rustix::fs::open(path, flags, Mode::empty())?;
    let stat = rustix::fs::fstat(&fd)?;
    let oversized = u64::try_from(stat.st_size).map_or(true, |size| size > MAX_CONTAINER_ID_BYTES);
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_nlink > 1
        || oversized
    {
        return Err(invalid());
    }
    let mut text = String::new();
    File::from(fd)
        .take(MAX_CONTAINER_ID_BYTES)
        .read_to_string(&mut text)?;
    let id = text.trim_matches([' ', '\t', '\r', '\n']);
    let valid = (12..=64).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_hexdigit());
    if valid {
        Ok(id.to_owned())
    } else {
        Err(invalid())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn docker_mcp_launch_preparation_injects_one_private_cidfile() {
        let prepared = prepare(argv(&[
            "/usr/local/bin/docker",
            "run",
            "--rm",
            "-i",
            "fixture",
        ]))
        .unwrap();
        assert_eq!(prepared.argv.len(), 7);
        assert_eq!(prepared.argv[2], "--cidfile");
        let cleanup = prepared.cleanup.unwrap();
        assert_eq!(Path::new(&prepared.argv[3]), cleanup.cidfile_path);
        assert!(prepared.argv[3].contains("oh-fx-mcp-"));
        assert_eq!(&prepared.argv[4..], ["--rm", "-i", "fixture"]);

        let explicit = prepare(argv(&[
            "docker",
            "run",
            "--cidfile=/tmp/owned.cid",
            "fixture",
        ]))
        .unwrap();
        assert_eq!(explicit.cleanup, None);
        assert_eq!(explicit.argv.len(), 4);

        let unrelated = prepare(argv(&["podman", "run", "fixture"])).unwrap();
        assert_eq!(unrelated.cleanup, None);
    }

    #[test]
    fn docker_mcp_cleanup_accepts_only_bounded_hexadecimal_container_ids() {
        let directory = tempfile::tempdir().unwrap();
        let cidfile = directory.path().join("fixture.cid");
        fs::write(&cidfile, "0123456789abcdef\n").unwrap();
        assert_eq!(read_container_id(&cidfile).unwrap(), "0123456789abcdef");
        fs::write(&cidfile, "not-a-container-id\n").unwrap();
        assert!(read_container_id(&cidfile).is_err());
    }

    #[test]
    fn docker_mcp_cleanup_rejects_hard_linked_cidfiles() {
        let directory = tempfile::tempdir().unwrap();
        let cidfile = directory.path().join("fixture.cid");
        fs::write(&cidfile, "0123456789abcdef\n").unwrap();
        let linked = directory.path().join("linked.cid");
        fs::hard_link(&cidfile, &linked).unwrap();
        assert!(read_container_id(&linked).is_err());
        assert!(read_container_id(&cidfile).is_err());
        fs::remove_file(&linked).unwrap();
        assert_eq!(read_container_id(&cidfile).unwrap(), "0123456789abcdef");
    }

    #[test]
    fn docker_mcp_cleanup_never_follows_a_symlinked_cidfile() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.cid");
        fs::write(&target, "0123456789abcdef\n").unwrap();
        let cidfile = directory.path().join("fixture.cid");
        symlink(&target, &cidfile).unwrap();
        assert!(read_container_id(&cidfile).is_err());
    }

    #[test]
    fn docker_mcp_cleanup_rejects_special_cidfiles_without_blocking() {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("fifo.cid");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(read_container_id(&fifo).is_err());
        });
        assert!(
            receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("a FIFO cidfile is refused without waiting for a writer")
        );
        let nested = directory.path().join("directory.cid");
        fs::create_dir(&nested).unwrap();
        assert!(read_container_id(&nested).is_err());
    }

    #[tokio::test]
    async fn cleanup_removes_the_cidfile_even_without_docker() {
        let directory = tempfile::tempdir().unwrap();
        let cidfile_path = directory.path().join("fixture.cid");
        fs::write(&cidfile_path, "0123456789abcdef\n").unwrap();
        Cleanup {
            docker_command: directory
                .path()
                .join("missing-docker")
                .to_string_lossy()
                .into_owned(),
            cidfile_path: cidfile_path.clone(),
            environment: Vec::new(),
        }
        .run()
        .await;
        assert!(!cidfile_path.exists());
    }
}
