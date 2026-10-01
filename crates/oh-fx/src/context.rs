use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ofx_agent::RuntimeContext;
use ofx_contract::{BoxFuture, PermissionMode};
use ofx_text::write_scalar;

pub(crate) const GATEWAY_SYSTEM_PROMPT: &str = include_str!("system_prompt.md");

const GIT_READ_BUDGET: Duration = Duration::from_millis(50);
const GIT_METADATA_FILE_BYTES: u64 = 4096;
const GIT_CONFIG_FILE_BYTES: u64 = 16 * 1024;
const GIT_INDEX_FILE_BYTES: u64 = 16 * 1024;
const GIT_INDEX_ENTRIES: u32 = 32;
const SECONDS_PER_DAY: u64 = 86_400;
const DIRTY_MARKERS: [&str; 5] = [
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "rebase-merge",
    "rebase-apply",
];
const NONINTERACTIVE_CONTEXT: &str = "Runtime context: this is a noninteractive run without live question UI; when a user-owned decision remains after inspection, stop and surface a concrete blocker in freeform text with the available options. Do not recommend or label one option as preferred.";
const ASK_MODE_CONTEXT: &str = "Runtime context: permission mode is ask. Sensitive tool calls may require user approval unless configured rules or session grants already decide them. Tool admission remains authoritative.";
const AUTO_MODE_CONTEXT: &str = "Runtime context: permission mode is auto. After configured rules, session grants, and deterministic safe-tool authority, oh-fx sends each unresolved action to a narrow safety reviewer. A clear result authorizes only that exact action. A caution or unavailable result holds only that action and returns advice without opening a permission screen, disabling tools, or ending the turn. Exact cautions are reused for this turn; choose a materially different safe action or explain why no safe path remains. Tool admission and exact live revalidation remain authoritative.";
const YOLO_MODE_CONTEXT: &str = "Runtime context: permission mode is full access. oh-fx permission policy is disabled. Tool lookup, argument validation, execution authority, cancellation, limits, operating-system permissions, and remote authentication remain authoritative.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostRuntimeContext {
    workspace_root: PathBuf,
    permission_mode: PermissionMode,
}

impl HostRuntimeContext {
    pub(crate) fn new(workspace_root: PathBuf, permission_mode: PermissionMode) -> Self {
        Self {
            workspace_root,
            permission_mode,
        }
    }
}

impl RuntimeContext for HostRuntimeContext {
    fn runtime_context(&self) -> BoxFuture<'_, Vec<String>> {
        let workspace_root = self.workspace_root.clone();
        let permission_mode = self.permission_mode;
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                noninteractive_runtime_context(&workspace_root, permission_mode)
            })
            .await
            .unwrap_or_default()
        })
    }
}

fn noninteractive_runtime_context(
    workspace_root: &Path,
    permission_mode: PermissionMode,
) -> Vec<String> {
    let fragment = build_turn_context_fragment(workspace_root);
    vec![
        format!("{fragment}\n{NONINTERACTIVE_CONTEXT}"),
        permission_mode_context(permission_mode).to_owned(),
    ]
}

fn permission_mode_context(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask => ASK_MODE_CONTEXT,
        PermissionMode::Auto => AUTO_MODE_CONTEXT,
        PermissionMode::Yolo => YOLO_MODE_CONTEXT,
    }
}

fn build_turn_context_fragment(workspace_root: &Path) -> String {
    let current_directory = env::current_dir().map_or_else(
        |_| "(unavailable)".to_owned(),
        |path| path.to_string_lossy().into_owned(),
    );
    let shell = env::var("SHELL")
        .or_else(|_| env::var("COMSPEC"))
        .unwrap_or_else(|_| "(unknown)".to_owned());
    let home = env::var("HOME")
        .or_else(|_| env::var("USERPROFILE"))
        .unwrap_or_else(|_| "(unknown)".to_owned());
    let root = workspace_root.to_string_lossy();
    let fragment = TurnFragment {
        workspace_root: if root.is_empty() {
            "(unavailable)"
        } else {
            &root
        },
        current_directory: &current_directory,
        operating_system: &operating_system_text(),
        shell_path: &shell,
        date_utc: &format_utc_date(unix_seconds()),
        home_directory: &home,
        git: &collect_git_info(workspace_root),
    };
    fragment.render()
}

struct TurnFragment<'a> {
    workspace_root: &'a str,
    current_directory: &'a str,
    operating_system: &'a str,
    shell_path: &'a str,
    date_utc: &'a str,
    home_directory: &'a str,
    git: &'a GitInfo,
}

impl TurnFragment<'_> {
    fn render(&self) -> String {
        let mut out = String::from("<fx-turn-context>\n");
        scalar_line(&mut out, "workspace_root", self.workspace_root);
        scalar_line(&mut out, "current_directory", self.current_directory);
        raw_line(&mut out, "operating_system", self.operating_system);
        scalar_line(&mut out, "shell_path", self.shell_path);
        raw_line(&mut out, "date_utc", self.date_utc);
        scalar_line(&mut out, "home_directory", self.home_directory);
        if let Some(branch) = &self.git.branch {
            scalar_line(&mut out, "git_branch", branch);
        }
        raw_line(&mut out, "git_worktree", self.git.worktree.label());
        if let Some(remote) = &self.git.remote {
            scalar_line(&mut out, "github_repo", &remote.repo);
            scalar_line(&mut out, "repo_name", &remote.repo_name);
            scalar_line(&mut out, "github_host", "github.com");
        }
        out.push_str("</fx-turn-context>");
        out
    }
}

fn scalar_line(out: &mut String, name: &str, value: &str) {
    out.push_str(name);
    out.push_str(": ");
    write_scalar(out, value);
    out.push('\n');
}

fn raw_line(out: &mut String, name: &str, value: &str) {
    out.push_str(name);
    out.push_str(": ");
    out.push_str(value);
    out.push('\n');
}

fn operating_system_text() -> String {
    let uname = rustix::system::uname();
    let sysname = uname.sysname().to_string_lossy();
    let release = uname.release().to_string_lossy();
    if release.is_empty() {
        sysname.into_owned()
    } else {
        format!("{sysname} {release}")
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn format_utc_date(seconds: u64) -> String {
    let days = i64::try_from(seconds / SECONDS_PER_DAY).unwrap_or(i64::MAX);
    let (year, month, day) = civil_from_unix_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_unix_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum GitWorktreeState {
    Dirty,
    #[default]
    Unknown,
}

impl GitWorktreeState {
    const fn label(self) -> &'static str {
        match self {
            Self::Dirty => "dirty",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitHubRepo {
    repo: String,
    repo_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GitInfo {
    branch: Option<String>,
    worktree: GitWorktreeState,
    remote: Option<GitHubRepo>,
}

struct GitBudget {
    started: Instant,
}

impl GitBudget {
    fn expired(&self) -> bool {
        self.started.elapsed() > GIT_READ_BUDGET
    }
}

fn collect_git_info(workspace_root: &Path) -> GitInfo {
    if workspace_root.as_os_str().is_empty() {
        return GitInfo::default();
    }
    let budget = GitBudget {
        started: Instant::now(),
    };
    let Some((repository_root, git_dir)) = find_repository(workspace_root, &budget) else {
        return GitInfo::default();
    };
    if budget.expired() {
        return GitInfo::default();
    }
    let Some(head) = read_small_file(&git_dir.join("HEAD"), GIT_METADATA_FILE_BYTES) else {
        return GitInfo::default();
    };
    if budget.expired() {
        return GitInfo::default();
    }
    let common_git_dir = resolve_common_git_dir(&git_dir);
    let remote = if budget.expired() {
        None
    } else {
        read_small_file(&common_git_dir.join("config"), GIT_CONFIG_FILE_BYTES)
            .filter(|_| !budget.expired())
            .and_then(|config| parse_github_repo_from_config(&String::from_utf8_lossy(&config)))
    };
    GitInfo {
        branch: branch_from_head(&String::from_utf8_lossy(&head)),
        worktree: detect_worktree_state(&repository_root, &git_dir, &budget),
        remote,
    }
}

fn find_repository(workspace_root: &Path, budget: &GitBudget) -> Option<(PathBuf, PathBuf)> {
    for directory in workspace_root.ancestors() {
        if budget.expired() {
            return None;
        }
        match fs::symlink_metadata(directory.join(".git")) {
            Ok(_) => {
                return resolve_git_dir(directory).map(|git_dir| (directory.to_owned(), git_dir));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    None
}

fn resolve_git_dir(repository_root: &Path) -> Option<PathBuf> {
    let dot_git = repository_root.join(".git");
    let metadata = fs::symlink_metadata(&dot_git).ok()?;
    if metadata.is_dir() {
        return Some(dot_git);
    }
    if !metadata.is_file() {
        return None;
    }
    let content = read_small_file(&dot_git, GIT_METADATA_FILE_BYTES)?;
    let content = String::from_utf8_lossy(&content);
    let raw = content.trim().strip_prefix("gitdir:")?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(repository_root.join(raw))
}

fn resolve_common_git_dir(git_dir: &Path) -> PathBuf {
    read_small_file(&git_dir.join("commondir"), GIT_METADATA_FILE_BYTES)
        .map(|content| String::from_utf8_lossy(&content).trim().to_owned())
        .filter(|raw| !raw.is_empty())
        .map_or_else(|| git_dir.to_owned(), |raw| git_dir.join(raw))
}

fn read_small_file(path: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= max_bytes).then_some(bytes)
}

fn branch_from_head(head: &str) -> Option<String> {
    let trimmed = head.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(reference) = trimmed.strip_prefix("ref:") {
        let reference = reference.trim();
        return Some(
            reference
                .strip_prefix("refs/heads/")
                .unwrap_or(reference)
                .to_owned(),
        );
    }
    let short: String = trimmed.chars().take(12).collect();
    Some(format!("detached:{short}"))
}

fn parse_github_repo_from_config(config: &str) -> Option<GitHubRepo> {
    let mut in_origin = false;
    for line in config.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.starts_with('[') {
            in_origin = is_origin_remote_section(trimmed);
            continue;
        }
        if !in_origin {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim() == "url" {
            return parse_github_remote_url(value.trim());
        }
    }
    None
}

fn is_origin_remote_section(line: &str) -> bool {
    let Some(section) = line
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    let Some(rest) = section.trim().strip_prefix("remote") else {
        return false;
    };
    let Some(quoted) = rest.trim().strip_prefix('"') else {
        return false;
    };
    quoted
        .split_once('"')
        .is_some_and(|(name, after)| name == "origin" && after.trim().is_empty())
}

fn parse_github_remote_url(raw: &str) -> Option<GitHubRepo> {
    if let Some(without_scheme) = raw.strip_prefix("https://") {
        let (host, path) = without_scheme.split_once('/')?;
        if host.contains('@') || host != "github.com" {
            return None;
        }
        return parse_github_repo_path(path);
    }
    raw.strip_prefix("git@github.com:")
        .or_else(|| raw.strip_prefix("ssh://git@github.com/"))
        .and_then(parse_github_repo_path)
}

fn parse_github_repo_path(raw_path: &str) -> Option<GitHubRepo> {
    let path = raw_path.trim();
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.is_empty() || path.contains(['?', '#']) {
        return None;
    }
    let (owner, name) = path.split_once('/')?;
    if name.contains('/') || !is_safe_component(owner) || !is_safe_component(name) {
        return None;
    }
    Some(GitHubRepo {
        repo: format!("{owner}/{name}"),
        repo_name: name.to_owned(),
    })
}

fn is_safe_component(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

enum PathPresence {
    Present,
    Missing,
    Unknown,
}

fn path_presence(git_dir: &Path, child: &str) -> PathPresence {
    match fs::symlink_metadata(git_dir.join(child)) {
        Ok(_) => PathPresence::Present,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => PathPresence::Missing,
        Err(_) => PathPresence::Unknown,
    }
}

fn detect_worktree_state(
    workspace_root: &Path,
    git_dir: &Path,
    budget: &GitBudget,
) -> GitWorktreeState {
    if budget.expired() {
        return GitWorktreeState::Unknown;
    }
    for marker in DIRTY_MARKERS {
        match path_presence(git_dir, marker) {
            PathPresence::Present => return GitWorktreeState::Dirty,
            PathPresence::Unknown => return GitWorktreeState::Unknown,
            PathPresence::Missing => {}
        }
    }
    if !matches!(path_presence(git_dir, "index.lock"), PathPresence::Missing) || budget.expired() {
        return GitWorktreeState::Unknown;
    }
    let index_path = git_dir.join("index");
    let Ok(metadata) = fs::symlink_metadata(&index_path) else {
        return GitWorktreeState::Unknown;
    };
    if !metadata.is_file() || metadata.len() > GIT_INDEX_FILE_BYTES {
        return GitWorktreeState::Unknown;
    }
    let Some(index) = read_small_file(&index_path, metadata.len()) else {
        return GitWorktreeState::Unknown;
    };
    if budget.expired() {
        return GitWorktreeState::Unknown;
    }
    worktree_state_from_small_index(workspace_root, &index, budget)
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..offset + 4)
        .and_then(|slice| slice.try_into().ok())
        .map(u32::from_be_bytes)
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    bytes
        .get(offset..offset + 2)
        .and_then(|slice| slice.try_into().ok())
        .map(u16::from_be_bytes)
}

fn worktree_state_from_small_index(
    workspace_root: &Path,
    index: &[u8],
    budget: &GitBudget,
) -> GitWorktreeState {
    scan_index(workspace_root, index, budget).unwrap_or(GitWorktreeState::Unknown)
}

fn scan_index(workspace_root: &Path, index: &[u8], budget: &GitBudget) -> Option<GitWorktreeState> {
    if index.len() < 12 || &index[..4] != b"DIRC" {
        return None;
    }
    let version = read_u32(index, 4)?;
    let entry_count = read_u32(index, 8)?;
    if !(2..=3).contains(&version) || entry_count == 0 || entry_count > GIT_INDEX_ENTRIES {
        return None;
    }
    let mut offset = 12;
    for _ in 0..entry_count {
        if budget.expired() || index.len() < offset + 62 {
            return None;
        }
        let flags = read_u16(index, offset + 60)?;
        if (flags >> 12) & 0x3 != 0 {
            return Some(GitWorktreeState::Dirty);
        }
        let path_start = offset + 62;
        let path_length = usize::from(flags & 0x0fff);
        let path_end = if path_length < 0x0fff {
            let end = path_start + path_length;
            (index.get(end) == Some(&0)).then_some(end)?
        } else {
            path_start + index[path_start..].iter().position(|byte| *byte == 0)?
        };
        let path = std::str::from_utf8(&index[path_start..path_end]).ok()?;
        if !is_safe_index_path(path) {
            return None;
        }
        if entry_matches_worktree(workspace_root, &index[offset..], path)?
            == GitWorktreeState::Dirty
        {
            return Some(GitWorktreeState::Dirty);
        }
        offset += (path_end - offset + 1 + 7) & !7;
        if offset > index.len() {
            return None;
        }
    }
    None
}

fn entry_matches_worktree(
    workspace_root: &Path,
    entry: &[u8],
    path: &str,
) -> Option<GitWorktreeState> {
    let mode = read_u32(entry, 24)?;
    if mode & 0o170_000 != 0o100_000 {
        return None;
    }
    let index_size = read_u32(entry, 36)?;
    let metadata = match fs::symlink_metadata(workspace_root.join(path)) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some(GitWorktreeState::Dirty);
        }
        Err(_) => return None,
    };
    if !metadata.is_file() || metadata.len() != u64::from(index_size) {
        return Some(GitWorktreeState::Dirty);
    }
    Some(GitWorktreeState::Unknown)
}

fn is_safe_index_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

#[cfg(test)]
mod tests;
