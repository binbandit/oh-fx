use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use ofx_contract::{WorkspaceIdentity, WorkspaceIdentitySource};
use ofx_text::{encode_terminal_safe, encode_terminal_safe_path_tail};

use crate::pathing::MAX_PATH_BYTES;

const MAX_GIT_METADATA_BYTES: usize = 4096;
const MAX_ENCODED_WORKSPACE_BYTES: usize = MAX_PATH_BYTES * 4;
const MAX_ENCODED_BRANCH_BYTES: usize = 512;
const TRIMMED: [u8; 4] = *b" \t\r\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeadSignature {
    inode: u64,
    modified_ns: i128,
    size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedHead<'a> {
    Branch(&'a [u8]),
    Detached(&'a [u8]),
}

enum HeadPathResolution {
    Missing,
    Invalid,
    Found(PathBuf),
}

#[derive(Debug)]
struct Resolved {
    label: String,
    head_path: Option<PathBuf>,
}

#[derive(Debug)]
pub struct StatuslineIdentity {
    workspace_root: PathBuf,
    resolved: Option<Resolved>,
    branch: Option<String>,
    head_signature: Option<HeadSignature>,
}

impl WorkspaceIdentitySource for StatuslineIdentity {
    fn refresh(&mut self) -> WorkspaceIdentity {
        self.snapshot()
    }
}

impl StatuslineIdentity {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            resolved: None,
            branch: None,
            head_signature: None,
        }
    }

    fn snapshot(&mut self) -> WorkspaceIdentity {
        if self.workspace_root.as_os_str().is_empty() {
            return WorkspaceIdentity::default();
        }
        let root = &self.workspace_root;
        let resolved = self.resolved.get_or_insert_with(|| Resolved {
            label: encode_workspace_path(root),
            head_path: resolve_head_path(root),
        });
        let label = resolved.label.clone();
        if let Some(head_path) = resolved.head_path.clone() {
            self.refresh_branch(&head_path);
        }
        WorkspaceIdentity {
            label,
            branch: self.branch.clone(),
        }
    }

    fn refresh_branch(&mut self, head_path: &Path) {
        let metadata = match fs::symlink_metadata(head_path) {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => {
                self.branch = None;
                self.head_signature = None;
                return;
            }
        };
        let signature = HeadSignature {
            inode: metadata.ino(),
            modified_ns: i128::from(metadata.mtime()) * 1_000_000_000
                + i128::from(metadata.mtime_nsec()),
            size: metadata.size(),
        };
        if self.head_signature == Some(signature) {
            return;
        }
        self.head_signature = Some(signature);
        self.branch = read_small_file(head_path)
            .ok()
            .and_then(|head| branch_label(&head));
    }
}

fn branch_label(head: &[u8]) -> Option<String> {
    let raw = match parse_head(head)? {
        ParsedHead::Branch(branch) => branch.to_vec(),
        ParsedHead::Detached(sha) => [b"detached:".as_slice(), sha].concat(),
    };
    Some(encode_terminal_safe(&raw, MAX_ENCODED_BRANCH_BYTES).text)
}

fn encode_workspace_path(workspace_root: &Path) -> String {
    let raw = workspace_root.as_os_str().as_bytes();
    encode_terminal_safe_path_tail(raw, MAX_ENCODED_WORKSPACE_BYTES)
        .unwrap_or_else(|| encode_terminal_safe(raw, MAX_ENCODED_WORKSPACE_BYTES).text)
}

fn resolve_head_path(workspace_root: &Path) -> Option<PathBuf> {
    let mut candidate = workspace_root;
    loop {
        match resolve_head_path_at(candidate) {
            HeadPathResolution::Found(head_path) => return Some(head_path),
            HeadPathResolution::Invalid => return None,
            HeadPathResolution::Missing => {}
        }
        let parent = candidate.parent()?;
        if parent == candidate {
            return None;
        }
        candidate = parent;
    }
}

fn resolve_head_path_at(candidate_root: &Path) -> HeadPathResolution {
    let dot_git = candidate_root.join(".git");
    let metadata = match fs::symlink_metadata(&dot_git) {
        Ok(metadata) => metadata,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.kind() == io::ErrorKind::NotADirectory =>
        {
            return HeadPathResolution::Missing;
        }
        Err(_) => return HeadPathResolution::Invalid,
    };
    if metadata.is_dir() {
        return HeadPathResolution::Found(dot_git.join("HEAD"));
    }
    if !metadata.is_file() {
        return HeadPathResolution::Invalid;
    }
    let Ok(content) = read_small_file(&dot_git) else {
        return HeadPathResolution::Invalid;
    };
    let Some(raw) = trim(&content)
        .strip_prefix(b"gitdir:")
        .map(trim)
        .filter(|raw| !raw.is_empty())
    else {
        return HeadPathResolution::Invalid;
    };
    let raw = Path::new(OsStr::from_bytes(raw));
    let git_dir = if raw.is_absolute() {
        raw.to_owned()
    } else {
        resolve_lexically(&candidate_root.join(raw))
    };
    HeadPathResolution::Found(git_dir.join("HEAD"))
}

fn resolve_lexically(path: &Path) -> PathBuf {
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

fn read_small_file(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_GIT_METADATA_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_GIT_METADATA_BYTES {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    Ok(bytes)
}

fn trim(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !TRIMMED.contains(byte))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !TRIMMED.contains(byte))
        .map_or(start, |last| last + 1);
    &bytes[start..end]
}

fn parse_head(head: &[u8]) -> Option<ParsedHead<'_>> {
    let trimmed = trim(head);
    if trimmed.is_empty() {
        return None;
    }
    if let Some(reference) = trimmed.strip_prefix(b"ref:") {
        let reference = trim(reference);
        let branch = reference.strip_prefix(b"refs/heads/").unwrap_or(reference);
        return (!branch.is_empty()).then_some(ParsedHead::Branch(branch));
    }
    if trimmed.len() < 7 || !trimmed.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    Some(ParsedHead::Detached(&trimmed[..trimmed.len().min(12)]))
}

#[cfg(test)]
mod tests;
