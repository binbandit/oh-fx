use std::ffi::OsStr;
use std::fs;
use std::io::{ErrorKind, Read};
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};

use ofx_text::is_terminal_safe;
use rustix::fs::{Mode, OFlags};

use crate::regular_file::{DIRECTORY_FLAGS, open_regular_file_at};

const MAX_BRANCH_BYTES: usize = 255;
const MAX_METADATA_BYTES: usize = 4096;
const HEAD_PREFIX: &str = "ref: refs/heads/";
const GITDIR_PREFIX: &str = "gitdir:";
const METADATA_WHITESPACE: [char; 4] = [' ', '\t', '\r', '\n'];

enum GitDirResolution {
    Missing,
    Invalid,
    Found(PathBuf),
}

pub fn current_branch(cwd: &Path) -> Option<String> {
    if !cwd.is_absolute() {
        return None;
    }
    let git_dir = resolve_git_dir(cwd)?;
    let directory = open_directory_no_follow(&git_dir)?;
    let head = read_metadata(&directory, OsStr::new("HEAD"))?;
    parse_head(&head).map(str::to_owned)
}

fn parse_head(head: &str) -> Option<&str> {
    let branch = head
        .trim_matches(METADATA_WHITESPACE)
        .strip_prefix(HEAD_PREFIX)?;
    (!branch.is_empty() && branch.len() <= MAX_BRANCH_BYTES && is_terminal_safe(branch.as_bytes()))
        .then_some(branch)
}

fn resolve_git_dir(cwd: &Path) -> Option<PathBuf> {
    for candidate in cwd.ancestors() {
        match resolve_git_dir_at(candidate) {
            GitDirResolution::Found(path) => return Some(path),
            GitDirResolution::Invalid => return None,
            GitDirResolution::Missing => {}
        }
    }
    None
}

fn resolve_git_dir_at(candidate: &Path) -> GitDirResolution {
    let dot_git = candidate.join(".git");
    let metadata = match fs::symlink_metadata(&dot_git) {
        Ok(metadata) => metadata,
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return GitDirResolution::Missing;
        }
        Err(_) => return GitDirResolution::Invalid,
    };
    if metadata.is_dir() {
        return match open_directory_no_follow(&dot_git) {
            Some(_) => GitDirResolution::Found(dot_git),
            None => GitDirResolution::Invalid,
        };
    }
    if !metadata.is_file() || metadata.len() > MAX_METADATA_BYTES as u64 {
        return GitDirResolution::Invalid;
    }
    let Some(content) =
        open_directory(candidate).and_then(|parent| read_metadata(&parent, OsStr::new(".git")))
    else {
        return GitDirResolution::Invalid;
    };
    let Some(raw) = content
        .trim_matches(METADATA_WHITESPACE)
        .strip_prefix(GITDIR_PREFIX)
        .map(|raw| raw.trim_matches(METADATA_WHITESPACE))
        .filter(|raw| !raw.is_empty())
    else {
        return GitDirResolution::Invalid;
    };
    let git_dir = lexically_resolved(&candidate.join(raw));
    match open_directory_no_follow(&git_dir) {
        Some(_) => GitDirResolution::Found(git_dir),
        None => GitDirResolution::Invalid,
    }
}

fn lexically_resolved(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => resolved.push(other),
        }
    }
    resolved
}

fn open_directory(path: &Path) -> Option<OwnedFd> {
    rustix::fs::open(
        path,
        DIRECTORY_FLAGS.difference(OFlags::NOFOLLOW),
        Mode::empty(),
    )
    .ok()
}

fn open_directory_no_follow(path: &Path) -> Option<OwnedFd> {
    rustix::fs::open(path, DIRECTORY_FLAGS, Mode::empty()).ok()
}

fn read_metadata(directory: &OwnedFd, name: &OsStr) -> Option<String> {
    let (file, metadata) = open_regular_file_at(directory, name).ok()?;
    if metadata.len() > MAX_METADATA_BYTES as u64 {
        return None;
    }
    let mut content = Vec::new();
    file.take(MAX_METADATA_BYTES as u64 + 1)
        .read_to_end(&mut content)
        .ok()?;
    if content.len() > MAX_METADATA_BYTES {
        return None;
    }
    String::from_utf8(content).ok()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn write(root: &Path, path: &str, content: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn root() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        (temp, root)
    }

    #[test]
    fn current_branch_parses_only_bounded_local_branch_refs() {
        assert_eq!(parse_head("ref: refs/heads/main\n"), Some("main"));
        assert_eq!(
            parse_head("ref: refs/heads/feature/media-ui\n"),
            Some("feature/media-ui")
        );
        assert_eq!(
            parse_head("0123456789abcdef0123456789abcdef01234567\n"),
            None
        );
        assert_eq!(parse_head("ref: refs/tags/v1\n"), None);
        assert_eq!(parse_head("ref: refs/heads/bad\u{1b}[31m\n"), None);
        assert_eq!(
            parse_head(&format!("ref: refs/heads/{}\n", "x".repeat(256))),
            None
        );
    }

    #[test]
    fn current_branch_reads_nested_worktree_head_without_spawning_git() {
        let (_temp, root) = root();
        write(
            &root,
            "repo/.git/HEAD",
            "ref: refs/heads/feature/media-ui\n",
        );
        fs::create_dir_all(root.join("repo/src/nested")).unwrap();
        assert_eq!(
            current_branch(&root.join("repo/src/nested")).as_deref(),
            Some("feature/media-ui")
        );
    }

    #[test]
    fn current_branch_follows_a_linked_worktree_gitdir_file() {
        let (_temp, root) = root();
        write(
            &root,
            "workspace/.git",
            "gitdir: ../git-data/worktrees/workspace\n",
        );
        write(
            &root,
            "git-data/worktrees/workspace/HEAD",
            "ref: refs/heads/worktree-branch\n",
        );
        assert_eq!(
            current_branch(&root.join("workspace")).as_deref(),
            Some("worktree-branch")
        );
    }

    #[test]
    fn current_branch_rejects_missing_detached_malformed_oversized_and_symlinked_metadata() {
        let (_temp, root) = root();
        fs::create_dir_all(root.join("repo/.git")).unwrap();
        let repo = root.join("repo");
        assert_eq!(current_branch(&repo), None);
        write(&root, "repo/.git/HEAD", "0123456789abcdef\n");
        assert_eq!(current_branch(&repo), None);
        write(&root, "repo/.git/HEAD", "ref: refs/tags/v1\n");
        assert_eq!(current_branch(&repo), None);
        write(
            &root,
            "repo/.git/HEAD",
            &format!("ref: refs/heads/{}", "x".repeat(MAX_METADATA_BYTES + 1)),
        );
        assert_eq!(current_branch(&repo), None);

        write(
            &root,
            "linked/actual-git/HEAD",
            "ref: refs/heads/symlinked\n",
        );
        symlink("actual-git", root.join("linked/.git")).unwrap();
        assert_eq!(current_branch(&root.join("linked")), None);
        assert_eq!(current_branch(Path::new("relative/path")), None);
    }
}
