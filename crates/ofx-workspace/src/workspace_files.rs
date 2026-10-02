use std::collections::HashSet;
use std::ffi::{CStr, OsStr};
use std::fs;
use std::io;
use std::iter;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use memchr::{memchr, memchr_iter};
use rustix::fs::{AtFlags, Dir, DirEntry, FileType, Mode, OFlags, open, openat, statat};

use crate::bounded_process::run_bounded;
use crate::ignored_dirs::IGNORED_DIRECTORY_NAMES;
use crate::pathing::MAX_PATH_BYTES;

pub const DEFAULT_CANDIDATE_CAP: usize = 100_000;
pub const MAX_RELATIVE_PATH_BYTES: usize = 2048;

const GIT_STDOUT_LIMIT: usize = 32 * 1024 * 1024;
const TRACKED_FILES_ARGS: &[&str] = &["ls-files", "-z", "--cached"];
const TRACKED_AND_OTHER_FILES_ARGS: &[&str] = &[
    "ls-files",
    "-z",
    "--cached",
    "--others",
    "--exclude-standard",
];
const OTHER_FILES_ARGS: &[&str] = &["ls-files", "-z", "--others", "--exclude-standard"];
const IGNORED_DIRECTORIES_ARGS: &[&str] = &[
    "ls-files",
    "-z",
    "--others",
    "--ignored",
    "--directory",
    "--exclude-standard",
];
const GIT_METADATA_NAME: &str = ".git";
const WORK_TREE_ARGS: &[&str] = &["rev-parse", "--show-toplevel"];
const GIT_PRELUDE: &[&str] = &[
    "--no-pager",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    "--no-optional-locks",
];
const GIT_TRANSPORT_LOCKDOWN: &[(&str, &str)] =
    &[("GIT_NO_LAZY_FETCH", "1"), ("GIT_ALLOW_PROTOCOL", "")];
pub const GIT_REPOSITORY_VARIABLES: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];
const WALK_ROOT_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);
const WALK_CHILD_FLAGS: OFlags = WALK_ROOT_FLAGS.union(OFlags::NOFOLLOW);
const TRUSTED_GIT_EXECUTABLES: &[&str] = &[
    "/usr/bin/git",
    "/bin/git",
    "/usr/local/bin/git",
    "/opt/homebrew/bin/git",
    "/opt/local/bin/git",
    "/run/current-system/sw/bin/git",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Git,
    Recursive,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UntrackedFiles {
    #[default]
    Exclude,
    Include,
    Only,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryOptions<'a> {
    pub candidate_cap: usize,
    pub ignored_names: &'a [&'a str],
    pub force_fallback: bool,
    pub untracked: UntrackedFiles,
    pub include_hidden: bool,
    pub sort_paths: bool,
}

impl Default for DiscoveryOptions<'_> {
    fn default() -> Self {
        Self {
            candidate_cap: DEFAULT_CANDIDATE_CAP,
            ignored_names: IGNORED_DIRECTORY_NAMES,
            force_fallback: false,
            untracked: UntrackedFiles::Exclude,
            include_hidden: false,
            sort_paths: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CandidateStats {
    pub incomplete: bool,
    pub skipped_overlong: usize,
}

impl CandidateStats {
    pub(crate) fn absorb(&mut self, other: Self) {
        self.incomplete |= other.incomplete;
        self.skipped_overlong += other.skipped_overlong;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CandidatePaths {
    bytes: Vec<u8>,
    spans: Vec<Span>,
}

impl CandidatePaths {
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        self.spans.iter().map(|span| self.path(*span))
    }

    pub fn push(&mut self, path: &[u8]) {
        self.push_joined(&[], path);
    }

    pub fn sort(&mut self) {
        let bytes = &self.bytes;
        self.spans.sort_unstable_by(|left, right| {
            bytes[left.start..left.end].cmp(&bytes[right.start..right.end])
        });
    }

    fn path(&self, span: Span) -> &[u8] {
        &self.bytes[span.start..span.end]
    }

    fn push_joined(&mut self, prefix: &[u8], name: &[u8]) {
        let start = self.bytes.len();
        if !prefix.is_empty() {
            self.bytes.extend_from_slice(prefix);
            self.bytes.push(b'/');
        }
        self.bytes.extend_from_slice(name);
        self.spans.push(Span {
            start,
            end: self.bytes.len(),
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub files: CandidatePaths,
    pub source: Source,
    pub stats: CandidateStats,
}

impl Discovery {
    fn new(files: CandidatePaths, source: Source, stats: CandidateStats) -> Self {
        Self {
            files,
            source,
            stats,
        }
    }
}

pub fn discover(workspace_root: &Path, options: &DiscoveryOptions<'_>) -> Discovery {
    discover_with_git(workspace_root, options, trusted_git_executable())
}

pub(crate) fn discover_in_work_tree(
    workspace_root: &Path,
    options: &DiscoveryOptions<'_>,
) -> Discovery {
    discover_listed_by(workspace_root, options, trusted_git_executable(), |_| true)
}

pub fn path_contains_hidden_directory_component(path: &[u8]) -> bool {
    let Some(last_separator) = path.iter().rposition(|byte| *byte == b'/') else {
        return false;
    };
    path[..last_separator]
        .split(|byte| *byte == b'/')
        .any(is_hidden_name)
}

pub(crate) fn git_command(root: &Path) -> Option<Command> {
    trusted_git_executable().map(|git| git_command_at(git, root))
}

pub(crate) fn git_work_tree_contains(root: &Path) -> bool {
    trusted_git_executable().is_some_and(|git| work_tree_contains(git, root))
}

pub(crate) fn trim_trailing_carriage_returns(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|byte| *byte != b'\r')
        .map_or(0, |index| index + 1);
    &bytes[..end]
}

fn git_command_at(git: &Path, root: &Path) -> Command {
    let mut command = Command::new(git);
    command
        .args(GIT_PRELUDE)
        .current_dir(root)
        .envs(GIT_TRANSPORT_LOCKDOWN.iter().copied());
    for name in GIT_REPOSITORY_VARIABLES {
        command.env_remove(name);
    }
    command
}

fn work_tree_contains(git: &Path, root: &Path) -> bool {
    let Some(output) = run_bounded(
        git_command_at(git, root).args(WORK_TREE_ARGS),
        MAX_PATH_BYTES + 1,
    ) else {
        return false;
    };
    let toplevel = match output.stdout.strip_suffix(b"\n") {
        Some(toplevel) if output.status.success() && !toplevel.is_empty() => toplevel,
        _ => return false,
    };
    match (
        fs::canonicalize(OsStr::from_bytes(toplevel)),
        fs::canonicalize(root),
    ) {
        (Ok(toplevel), Ok(root)) => root.starts_with(toplevel),
        _ => false,
    }
}

fn is_hidden_name(name: &[u8]) -> bool {
    name.len() > 1 && name[0] == b'.'
}

pub(crate) fn discover_listing_files(
    workspace_root: &Path,
    options: &DiscoveryOptions<'_>,
) -> Option<Discovery> {
    if let Some(git) = trusted_git_executable() {
        if let Some(raw) = git_raw_list(workspace_root, options, git) {
            return Some(parse_raw_list(raw, options));
        }
        if has_git_metadata(workspace_root) {
            return None;
        }
    }
    Some(walk_workspace(workspace_root, options))
}

pub(crate) fn discover_listing_directories(
    workspace_root: &Path,
    options: &DiscoveryOptions<'_>,
) -> Option<Discovery> {
    if let Some(git) = trusted_git_executable() {
        if let Some(output) = run_bounded(
            git_command_at(git, workspace_root).args(IGNORED_DIRECTORIES_ARGS),
            GIT_STDOUT_LIMIT,
        )
        .filter(|output| output.status.success())
        {
            let ignored = parse_ignored_directories(&output.stdout);
            let git_options = DiscoveryOptions {
                ignored_names: &[GIT_METADATA_NAME],
                ..*options
            };
            return Some(walk_workspace_paths(
                workspace_root,
                &git_options,
                WalkTarget::Directories,
                Some(&ignored),
            ));
        }
        if has_git_metadata(workspace_root) {
            return None;
        }
    }
    Some(walk_workspace_paths(
        workspace_root,
        options,
        WalkTarget::Directories,
        None,
    ))
}

fn has_git_metadata(workspace_root: &Path) -> bool {
    workspace_root.ancestors().any(|directory| {
        !matches!(
            fs::symlink_metadata(directory.join(GIT_METADATA_NAME)),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        )
    })
}

fn parse_ignored_directories(raw: &[u8]) -> HashSet<Vec<u8>> {
    let separator = if memchr(0, raw).is_some() { 0 } else { b'\n' };
    raw.split(|byte| *byte == separator)
        .filter_map(|entry| {
            let entry = if separator == b'\n' {
                trim_trailing_carriage_returns(entry)
            } else {
                entry
            };
            if entry.last() != Some(&b'/') {
                return None;
            }
            let end = entry
                .iter()
                .rposition(|byte| *byte != b'/')
                .map(|index| index + 1)?;
            Some(entry[..end].to_vec())
        })
        .collect()
}

fn discover_with_git(
    workspace_root: &Path,
    options: &DiscoveryOptions<'_>,
    git_executable: Option<&Path>,
) -> Discovery {
    discover_listed_by(workspace_root, options, git_executable, |git| {
        work_tree_contains(git, workspace_root)
    })
}

fn discover_listed_by(
    workspace_root: &Path,
    options: &DiscoveryOptions<'_>,
    git_executable: Option<&Path>,
    work_tree_holds_root: impl Fn(&Path) -> bool,
) -> Discovery {
    if workspace_root.as_os_str().is_empty() {
        return Discovery::new(
            CandidatePaths::default(),
            Source::Recursive,
            CandidateStats::default(),
        );
    }
    if !options.force_fallback
        && let Some(raw) = git_executable
            .filter(|git| work_tree_holds_root(git))
            .and_then(|git| git_raw_list(workspace_root, options, git))
    {
        let parsed = parse_raw_list(raw, options);
        if options.untracked == UntrackedFiles::Only
            || !parsed.files.is_empty()
            || parsed.stats.incomplete
            || parsed.stats.skipped_overlong > 0
        {
            return parsed;
        }
    }
    walk_workspace(workspace_root, options)
}

fn git_list_arguments(options: &DiscoveryOptions<'_>) -> &'static [&'static str] {
    match options.untracked {
        UntrackedFiles::Exclude => TRACKED_FILES_ARGS,
        UntrackedFiles::Include => TRACKED_AND_OTHER_FILES_ARGS,
        UntrackedFiles::Only => OTHER_FILES_ARGS,
    }
}

fn git_raw_list(
    workspace_root: &Path,
    options: &DiscoveryOptions<'_>,
    git_executable: &Path,
) -> Option<Vec<u8>> {
    let output = run_bounded(
        git_command_at(git_executable, workspace_root).args(git_list_arguments(options)),
        GIT_STDOUT_LIMIT,
    )?;
    output.status.success().then_some(output.stdout)
}

fn trusted_git_executable() -> Option<&'static Path> {
    static TRUSTED: OnceLock<Option<&'static Path>> = OnceLock::new();
    *TRUSTED.get_or_init(|| {
        TRUSTED_GIT_EXECUTABLES
            .iter()
            .map(Path::new)
            .find(|candidate| fs::metadata(candidate).is_ok_and(|metadata| metadata.is_file()))
    })
}

fn parse_raw_list(raw: Vec<u8>, options: &DiscoveryOptions<'_>) -> Discovery {
    let separator = if memchr(0, &raw).is_some() { 0 } else { b'\n' };
    let mut spans = Vec::new();
    let mut stats = CandidateStats::default();
    let mut start = 0;
    for end in memchr_iter(separator, &raw).chain(iter::once(raw.len())) {
        let entry_start = start;
        start = end + 1;
        let entry = if separator == b'\n' {
            trim_trailing_carriage_returns(&raw[entry_start..end])
        } else {
            &raw[entry_start..end]
        };
        if entry.is_empty() {
            continue;
        }
        if !options.include_hidden && path_contains_hidden_directory_component(entry) {
            continue;
        }
        if entry.len() > MAX_RELATIVE_PATH_BYTES {
            stats.skipped_overlong += 1;
            continue;
        }
        if spans.len() >= options.candidate_cap {
            stats.incomplete = true;
            break;
        }
        spans.push(Span {
            start: entry_start,
            end: entry_start + entry.len(),
        });
    }
    let mut files = CandidatePaths { bytes: raw, spans };
    if options.sort_paths {
        files.sort();
    }
    Discovery::new(files, Source::Git, stats)
}

struct WalkFrame {
    entries: Dir,
    prefix: Vec<u8>,
}

fn open_walk_root(workspace_root: &Path) -> Option<Dir> {
    Dir::new(open(workspace_root, WALK_ROOT_FLAGS, Mode::empty()).ok()?).ok()
}

fn open_walk_child(parent: &Dir, name: &CStr) -> Option<Dir> {
    Dir::new(openat(parent.fd().ok()?, name, WALK_CHILD_FLAGS, Mode::empty()).ok()?).ok()
}

fn walk_entry_type(parent: &Dir, entry: &DirEntry) -> Option<FileType> {
    match entry.file_type() {
        FileType::Unknown => {
            let stat = statat(
                parent.fd().ok()?,
                entry.file_name(),
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .ok()?;
            Some(FileType::from_raw_mode(stat.st_mode))
        }
        known => Some(known),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalkTarget {
    Files,
    Directories,
}

fn walk_workspace(workspace_root: &Path, options: &DiscoveryOptions<'_>) -> Discovery {
    walk_workspace_paths(workspace_root, options, WalkTarget::Files, None)
}

fn walk_workspace_paths(
    workspace_root: &Path,
    options: &DiscoveryOptions<'_>,
    target: WalkTarget,
    ignored_paths: Option<&HashSet<Vec<u8>>>,
) -> Discovery {
    let mut files = CandidatePaths::default();
    let mut stats = CandidateStats::default();
    let Some(entries) = open_walk_root(workspace_root) else {
        return Discovery::new(files, Source::Recursive, stats);
    };
    let mut stack = vec![WalkFrame {
        entries,
        prefix: Vec::new(),
    }];

    while let Some(top) = stack.last_mut() {
        let Some(Ok(entry)) = top.entries.next() else {
            stack.pop();
            continue;
        };
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let Some(file_type) = walk_entry_type(&top.entries, &entry) else {
            continue;
        };

        if matches!(file_type, FileType::RegularFile | FileType::Symlink) {
            if target != WalkTarget::Files || is_ignored_name(options.ignored_names, name) {
                continue;
            }
            if files.len() >= options.candidate_cap {
                stats.incomplete = true;
                break;
            }
            if joined_len(&top.prefix, name) > MAX_RELATIVE_PATH_BYTES {
                stats.skipped_overlong += 1;
                continue;
            }
            files.push_joined(&top.prefix, name);
        } else if file_type == FileType::Directory {
            if !options.include_hidden && is_hidden_name(name) {
                continue;
            }
            if is_ignored_name(options.ignored_names, name) {
                continue;
            }
            let prefix = if top.prefix.is_empty() {
                name.to_vec()
            } else {
                [&top.prefix[..], b"/", name].concat()
            };
            if ignored_paths.is_some_and(|ignored| ignored.contains(&prefix)) {
                continue;
            }
            if target == WalkTarget::Directories {
                if files.len() >= options.candidate_cap {
                    stats.incomplete = true;
                    break;
                }
                if prefix.len() > MAX_RELATIVE_PATH_BYTES {
                    stats.skipped_overlong += 1;
                    continue;
                }
                files.push(&prefix);
            }
            let Some(entries) = open_walk_child(&top.entries, entry.file_name()) else {
                continue;
            };
            stack.push(WalkFrame { entries, prefix });
        }
    }

    if options.sort_paths {
        files.sort();
    }
    Discovery::new(files, Source::Recursive, stats)
}

fn joined_len(prefix: &[u8], name: &[u8]) -> usize {
    if prefix.is_empty() {
        name.len()
    } else {
        prefix.len() + 1 + name.len()
    }
}

fn is_ignored_name(ignored: &[&str], name: &[u8]) -> bool {
    ignored.iter().any(|entry| entry.as_bytes() == name)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    use rustix::fs::mkdirat;
    use tempfile::TempDir;

    use super::*;

    fn workspace() -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        (temp, root)
    }

    fn write_test_file(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn write_nested_test_file(root: &Path, directories: &[&str], name: &str, content: &str) {
        let mut directory: OwnedFd = open(root, WALK_ROOT_FLAGS, Mode::empty()).unwrap();
        for component in directories {
            mkdirat(&directory, *component, Mode::from_raw_mode(0o755)).unwrap();
            directory = openat(&directory, *component, WALK_CHILD_FLAGS, Mode::empty()).unwrap();
        }
        let file = openat(
            &directory,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        )
        .unwrap();
        fs::File::from(file).write_all(content.as_bytes()).unwrap();
    }

    fn contains_path(files: &CandidatePaths, needle: &str) -> bool {
        files.iter().any(|path| path == needle.as_bytes())
    }

    fn listed(files: &CandidatePaths) -> Vec<&str> {
        files
            .iter()
            .map(|path| std::str::from_utf8(path).unwrap())
            .collect()
    }

    pub(crate) fn run_git(root: &Path, args: &[&str]) -> bool {
        let mut command = Command::new("git");
        command.args(args).current_dir(root);
        for name in GIT_REPOSITORY_VARIABLES {
            command.env_remove(name);
        }
        command.output().is_ok_and(|output| output.status.success())
    }

    pub(crate) fn arm_hostile_git_config(root: &Path) -> PathBuf {
        let marker = root.join(".git/hostile-ran");
        let script = root.join(".git/hostile.sh");
        fs::write(
            &script,
            format!("#!/bin/sh\necho \"$0 $*\" >> '{}'\ncat\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let script = script.to_str().unwrap();
        let hostile = [
            "core.fsmonitor",
            "core.pager",
            "pager.grep",
            "pager.ls-files",
            "pager.check-ignore",
            "pager.rev-parse",
            "filter.hostile.clean",
            "filter.hostile.smudge",
            "diff.hostile.textconv",
        ];
        for key in hostile {
            assert!(run_git(root, &["config", key, script]), "{key}");
        }
        fs::write(
            root.join(".gitattributes"),
            "* filter=hostile diff=hostile\n",
        )
        .unwrap();
        marker
    }

    fn list_command_args(options: &DiscoveryOptions<'_>) -> Vec<String> {
        let mut command = git_command_at(Path::new("git"), Path::new("/workspace"));
        command.args(git_list_arguments(options));
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn git_commands_locate_the_repository_from_the_root_only() {
        let command = git_command_at(Path::new("git"), Path::new("/workspace"));
        let mut cleared: Vec<_> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        cleared.sort();
        let mut expected = GIT_REPOSITORY_VARIABLES.map(str::to_owned);
        expected.sort();

        assert_eq!(cleared, expected);
        assert_eq!(command.get_current_dir(), Some(Path::new("/workspace")));
    }

    #[test]
    fn git_commands_disable_repository_programs_and_lazy_fetching() {
        let command = git_command_at(Path::new("git"), Path::new("/workspace"));
        let set: Vec<_> = command
            .get_envs()
            .filter_map(|(name, value)| Some((name.to_str()?, value?.to_str()?)))
            .collect();

        assert_eq!(
            set,
            [("GIT_ALLOW_PROTOCOL", ""), ("GIT_NO_LAZY_FETCH", "1")]
        );
        assert_eq!(
            command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            GIT_PRELUDE
        );
    }

    #[test]
    fn workspace_file_provider_tracked_git_argv_uses_nul_separated_cached_files() {
        assert_eq!(
            list_command_args(&DiscoveryOptions::default()),
            [GIT_PRELUDE, &["ls-files", "-z", "--cached"]].concat()
        );
    }

    #[test]
    fn workspace_file_provider_optional_untracked_git_argv_includes_exclude_standard() {
        let options = DiscoveryOptions {
            untracked: UntrackedFiles::Include,
            ..DiscoveryOptions::default()
        };
        assert_eq!(
            list_command_args(&options),
            [
                GIT_PRELUDE,
                &[
                    "ls-files",
                    "-z",
                    "--cached",
                    "--others",
                    "--exclude-standard"
                ]
            ]
            .concat()
        );
    }

    #[test]
    fn workspace_file_provider_untracked_git_argv_uses_others_exclude_standard() {
        let options = DiscoveryOptions {
            untracked: UntrackedFiles::Only,
            ..DiscoveryOptions::default()
        };
        assert_eq!(
            list_command_args(&options),
            [
                GIT_PRELUDE,
                &["ls-files", "-z", "--others", "--exclude-standard"]
            ]
            .concat()
        );
    }

    #[test]
    fn workspace_file_provider_caps_candidates_and_reports_incomplete_status() {
        let options = DiscoveryOptions {
            candidate_cap: 2,
            ..DiscoveryOptions::default()
        };
        let result = parse_raw_list(b"a.zig\0b.zig\0c.zig\0".to_vec(), &options);

        assert_eq!(listed(&result.files), ["a.zig", "b.zig"]);
        assert!(result.stats.incomplete);
    }

    #[test]
    fn workspace_file_provider_applies_bytewise_ordering_to_git_results_when_requested() {
        let options = DiscoveryOptions {
            sort_paths: true,
            ..DiscoveryOptions::default()
        };
        let result = parse_raw_list(b"z.zig\0a.zig\0m.zig\0a-b/c\0a/b\0".to_vec(), &options);

        assert_eq!(
            listed(&result.files),
            ["a-b/c", "a.zig", "a/b", "m.zig", "z.zig"]
        );
    }

    #[test]
    fn workspace_file_provider_parses_newline_separated_lists_and_trims_carriage_returns() {
        let result = parse_raw_list(
            b"one.txt\r\ntwo.txt\n\nthree.txt".to_vec(),
            &DiscoveryOptions::default(),
        );

        assert_eq!(listed(&result.files), ["one.txt", "two.txt", "three.txt"]);
    }

    #[test]
    fn workspace_file_provider_git_discovery_includes_tracked_build_files_and_skips_hidden_directories()
     {
        let raw = b"build/script.ts\0.eslint-plugin-local/rule.ts\0src/.esbuild.ts\0src/main.ts\0";
        let result = parse_raw_list(raw.to_vec(), &DiscoveryOptions::default());

        assert_eq!(result.files.len(), 3);
        assert!(contains_path(&result.files, "build/script.ts"));
        assert!(contains_path(&result.files, "src/.esbuild.ts"));
        assert!(contains_path(&result.files, "src/main.ts"));
        assert!(!contains_path(
            &result.files,
            ".eslint-plugin-local/rule.ts"
        ));
    }

    #[test]
    fn workspace_file_provider_recursive_fallback_skips_ignored_directories_and_includes_nested_files()
     {
        let (_temp, root) = workspace();
        write_test_file(&root, "src/main.zig", "main\n");
        write_test_file(&root, "src/nested/lib.zig", "lib\n");
        write_test_file(&root, "node_modules/pkg/ignored.zig", "ignored\n");
        write_test_file(&root, ".zig-cache/o/ignored.o", "ignored\n");

        let options = DiscoveryOptions {
            force_fallback: true,
            ..DiscoveryOptions::default()
        };
        let result = discover(&root, &options);

        assert_eq!(result.source, Source::Recursive);
        assert!(contains_path(&result.files, "src/main.zig"));
        assert!(contains_path(&result.files, "src/nested/lib.zig"));
        assert!(!contains_path(
            &result.files,
            "node_modules/pkg/ignored.zig"
        ));
        assert!(!contains_path(&result.files, ".zig-cache/o/ignored.o"));
        assert_eq!(result.files.len(), 2);
    }

    #[test]
    fn workspace_file_provider_recursive_fallback_does_not_recurse_symlink_directories() {
        let (_temp, root) = workspace();
        write_test_file(&root, "real/inside.txt", "inside\n");
        symlink("real", root.join("linked")).unwrap();

        let options = DiscoveryOptions {
            force_fallback: true,
            ..DiscoveryOptions::default()
        };
        let result = discover(&root, &options);

        assert!(contains_path(&result.files, "linked"));
        assert!(!contains_path(&result.files, "linked/inside.txt"));
    }

    #[test]
    fn workspace_file_provider_overlong_paths_are_skipped_without_aborting() {
        let long_path = vec![b'a'; MAX_RELATIVE_PATH_BYTES + 1];
        let raw = [&b"kept.zig\0"[..], &long_path, b"\0also-kept.zig\0"].concat();

        let result = parse_raw_list(raw, &DiscoveryOptions::default());

        assert_eq!(result.files.len(), 2);
        assert_eq!(result.stats.skipped_overlong, 1);
        assert!(contains_path(&result.files, "kept.zig"));
        assert!(contains_path(&result.files, "also-kept.zig"));
    }

    #[test]
    fn workspace_file_provider_recursive_fallback_skips_overlong_nested_paths() {
        let (_temp, root) = workspace();
        let deep = "d".repeat(200);
        write_nested_test_file(&root, &[deep.as_str(); 11], "file.txt", "deep\n");
        write_test_file(&root, "short.txt", "short\n");

        let options = DiscoveryOptions {
            force_fallback: true,
            sort_paths: true,
            ..DiscoveryOptions::default()
        };
        let result = discover(&root, &options);

        assert_eq!(listed(&result.files), ["short.txt"]);
        assert_eq!(result.stats.skipped_overlong, 1);
    }

    #[test]
    fn workspace_file_provider_recursive_fallback_lists_files_past_the_platform_path_limit() {
        let (_temp, base) = workspace();
        let deep = "d".repeat(200);
        let directories = [deep.as_str(); 9];
        let kept = format!("{}/kept.txt", directories.join("/"));
        let padding_depth = MAX_PATH_BYTES.saturating_sub(base.as_os_str().len() + kept.len())
            / (deep.len() + 1)
            + 1;
        let padding = vec![deep.as_str(); padding_depth];
        write_nested_test_file(
            &base,
            &[&padding[..], &directories[..]].concat(),
            "kept.txt",
            "kept\n",
        );
        let root = base.join(padding.join("/"));
        fs::write(root.join("short.txt"), "short\n").unwrap();
        assert!(root.as_os_str().len() < MAX_PATH_BYTES);
        assert!(root.join(&kept).as_os_str().len() > MAX_PATH_BYTES);
        assert!(kept.len() <= MAX_RELATIVE_PATH_BYTES);

        let options = DiscoveryOptions {
            force_fallback: true,
            sort_paths: true,
            ..DiscoveryOptions::default()
        };
        let result = discover(&root, &options);

        assert_eq!(listed(&result.files), [kept.as_str(), "short.txt"]);
        assert_eq!(result.stats.skipped_overlong, 0);
    }

    #[test]
    fn workspace_file_provider_falls_back_when_git_is_skipped() {
        let (_temp, root) = workspace();
        write_test_file(&root, "nested/file.txt", "file\n");

        let options = DiscoveryOptions {
            force_fallback: true,
            ..DiscoveryOptions::default()
        };
        let result = discover(&root, &options);

        assert_eq!(result.source, Source::Recursive);
        assert!(contains_path(&result.files, "nested/file.txt"));
    }

    #[test]
    fn workspace_file_provider_walks_a_git_worktree_when_no_trusted_git_executable_is_available() {
        let (_temp, root) = workspace();
        fs::create_dir(root.join(".git")).unwrap();
        write_test_file(&root, "fallback.txt", "fallback\n");

        let result = discover_with_git(&root, &DiscoveryOptions::default(), None);

        assert_eq!(result.source, Source::Recursive);
        assert!(contains_path(&result.files, "fallback.txt"));
    }

    #[test]
    fn workspace_file_provider_git_result_contains_tracked_files_only() {
        let (_temp, root) = workspace();
        write_test_file(&root, ".gitignore", "ignored.txt\n");
        write_test_file(&root, "tracked.txt", "tracked\n");
        write_test_file(&root, ".hidden/tracked.txt", "hidden\n");
        write_test_file(&root, "ignored.txt", "ignored\n");
        write_test_file(&root, "untracked.txt", "untracked\n");
        if !run_git(&root, &["init", "--quiet"])
            || !run_git(
                &root,
                &["add", ".gitignore", "tracked.txt", ".hidden/tracked.txt"],
            )
        {
            return;
        }

        let options = DiscoveryOptions {
            include_hidden: true,
            ..DiscoveryOptions::default()
        };
        let result = discover_with_git(&root, &options, Some(Path::new("git")));

        assert_eq!(result.source, Source::Git);
        assert!(contains_path(&result.files, ".gitignore"));
        assert!(contains_path(&result.files, "tracked.txt"));
        assert!(contains_path(&result.files, ".hidden/tracked.txt"));
        assert!(!contains_path(&result.files, "ignored.txt"));
        assert!(!contains_path(&result.files, "untracked.txt"));
    }

    #[test]
    fn workspace_file_provider_walks_the_root_when_core_worktree_points_elsewhere() {
        let (_temp, root) = workspace();
        let (_elsewhere_temp, elsewhere) = workspace();
        write_test_file(&root, "kept.txt", "kept\n");
        write_test_file(&root, "elsewhere-only.txt", "elsewhere\n");
        if !run_git(&root, &["init", "--quiet"])
            || !run_git(&root, &["add", "kept.txt", "elsewhere-only.txt"])
        {
            return;
        }
        fs::remove_file(root.join("elsewhere-only.txt")).unwrap();
        assert!(run_git(
            &root,
            &["config", "core.worktree", elsewhere.to_str().unwrap()]
        ));

        let result = discover_with_git(&root, &DiscoveryOptions::default(), Some(Path::new("git")));

        assert_eq!(result.source, Source::Recursive);
        assert_eq!(listed(&result.files), ["kept.txt"]);
    }

    #[test]
    fn workspace_file_provider_walks_the_root_of_a_bare_repository() {
        let (_temp, root) = workspace();
        write_test_file(&root, "kept.txt", "kept\n");
        write_test_file(&root, "gone.txt", "gone\n");
        if !run_git(&root, &["init", "--quiet"])
            || !run_git(&root, &["add", "kept.txt", "gone.txt"])
        {
            return;
        }
        fs::remove_file(root.join("gone.txt")).unwrap();
        assert!(run_git(&root, &["config", "core.bare", "true"]));

        let result = discover_with_git(&root, &DiscoveryOptions::default(), Some(Path::new("git")));

        assert_eq!(result.source, Source::Recursive);
        assert_eq!(listed(&result.files), ["kept.txt"]);
    }

    #[test]
    fn workspace_file_provider_lists_git_files_below_the_work_tree_top_level() {
        let (_temp, root) = workspace();
        write_test_file(&root, "top.txt", "top\n");
        write_test_file(&root, "nested/inner.txt", "inner\n");
        if !run_git(&root, &["init", "--quiet"])
            || !run_git(&root, &["add", "top.txt", "nested/inner.txt"])
        {
            return;
        }

        let result = discover_with_git(
            &root.join("nested"),
            &DiscoveryOptions::default(),
            Some(Path::new("git")),
        );

        assert_eq!(result.source, Source::Git);
        assert_eq!(listed(&result.files), ["inner.txt"]);
    }

    #[test]
    fn git_listing_never_runs_programs_from_repository_config() {
        let (_temp, root) = workspace();
        write_test_file(&root, "tracked.txt", "tracked\n");
        if !run_git(&root, &["init", "--quiet"]) || !run_git(&root, &["add", "tracked.txt"]) {
            return;
        }
        let marker = arm_hostile_git_config(&root);
        write_test_file(&root, "untracked.txt", "untracked\n");

        for untracked in [
            UntrackedFiles::Exclude,
            UntrackedFiles::Include,
            UntrackedFiles::Only,
        ] {
            let options = DiscoveryOptions {
                untracked,
                ..DiscoveryOptions::default()
            };
            let result = discover_with_git(&root, &options, Some(Path::new("git")));
            assert_eq!(result.source, Source::Git);
        }

        assert!(
            !marker.exists(),
            "{}",
            fs::read_to_string(&marker).unwrap_or_default()
        );
    }

    #[test]
    fn candidate_paths_push_and_sort_bytewise() {
        let mut files = CandidatePaths::default();
        files.push(b"b.txt");
        files.push(b"a/b");
        files.push(b"a-b");
        files.sort();

        assert_eq!(listed(&files), ["a-b", "a/b", "b.txt"]);
    }

    #[test]
    fn path_contains_hidden_directory_component_checks_directories_only() {
        assert!(path_contains_hidden_directory_component(b".hidden/file"));
        assert!(path_contains_hidden_directory_component(b"src/.cache/file"));
        assert!(!path_contains_hidden_directory_component(
            b"src/.esbuild.ts"
        ));
        assert!(!path_contains_hidden_directory_component(b"./file"));
        assert!(!path_contains_hidden_directory_component(b".env"));
    }
}
