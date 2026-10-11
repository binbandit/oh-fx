use std::env;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) fn path_for_peer_reexec() -> io::Result<PathBuf> {
    let executable = env::current_exe();
    if cfg!(target_os = "linux") {
        return Ok(linux_peer_path(executable.ok(), std::process::id()));
    }
    executable
}

fn linux_peer_path(on_disk: Option<PathBuf>, pid: u32) -> PathBuf {
    on_disk
        .filter(|path| on_disk_path_is_executable(path))
        .unwrap_or_else(|| PathBuf::from(format!("/proc/{pid}/exe")))
}

fn on_disk_path_is_executable(path: &Path) -> bool {
    path.is_absolute() && File::open(path).is_ok()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn a_peer_reexecutes_the_running_program_by_a_path_it_can_open() {
        let path = path_for_peer_reexec().unwrap();
        assert!(path.is_absolute());
        assert_eq!(
            File::open(&path).unwrap().metadata().unwrap().len(),
            File::open(env::current_exe().unwrap())
                .unwrap()
                .metadata()
                .unwrap()
                .len()
        );
    }

    #[test]
    fn the_on_disk_probe_rejects_a_replaced_binary_so_the_peer_path_falls_back() {
        let directory = tempfile::tempdir().unwrap();
        let victim = fs::canonicalize(directory.path())
            .unwrap()
            .join("oh-fx-probe");
        fs::write(&victim, b"").unwrap();
        assert!(on_disk_path_is_executable(&victim));
        assert_eq!(linux_peer_path(Some(victim.clone()), 42), victim);
        fs::remove_file(&victim).unwrap();
        assert!(!on_disk_path_is_executable(&victim));
        assert_eq!(
            linux_peer_path(Some(victim), 42),
            PathBuf::from("/proc/42/exe")
        );
        assert!(!on_disk_path_is_executable(Path::new("oh-fx")));
        assert_eq!(linux_peer_path(None, 7), PathBuf::from("/proc/7/exe"));
    }
}
