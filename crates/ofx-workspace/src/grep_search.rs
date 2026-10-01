use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::io::Read;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use memchr::memmem::Finder;
use memchr::{memchr, memchr_iter, memrchr};
use ofx_text::is_model_safe_text;

use crate::bounded_process::run_bounded;
use crate::fs_path::basename;
use crate::glob_pattern::Pattern;
use crate::path_error::PathError;
use crate::pathing::{MAX_PATH_BYTES, path_inside};
use crate::regular_file::{RegularFileError, open_regular_file};
use crate::workspace_files::{
    CandidatePaths, CandidateStats, DiscoveryOptions, MAX_RELATIVE_PATH_BYTES, UntrackedFiles,
    discover, git_command, git_work_tree_contains, trim_trailing_carriage_returns,
};

pub const OUTPUT_CAP: usize = 200;
pub const COLLECTION_CAP: usize = 2000;

const FILE_BYTE_CAP: usize = 50 * 1024 * 4;
const GIT_GREP_STDOUT_LIMIT: usize = 8 * 1024 * 1024;
const GIT_GREP_OUTPUT_CONFIG: &[&str] = &[
    "-c",
    "color.grep=never",
    "-c",
    "grep.column=false",
    "-c",
    "grep.fullName=false",
];
const GIT_GREP_LITERAL_FLAGS: &[&str] = &["-I", "-F", "-z"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub absolute_path: PathBuf,
    pub read_path: PathBuf,
    pub line_number: usize,
    pub line: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedReason {
    CollectionCap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepResult {
    pub matches: Vec<Match>,
    pub truncated_reason: Option<TruncatedReason>,
    pub candidates: CandidateStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CountResult {
    pub matching_lines: usize,
    pub matching_files: usize,
    pub candidates: CandidateStats,
}

pub struct GrepQuery<'a> {
    pub workspace_root: &'a Path,
    pub pattern: &'a str,
    pub case_insensitive: bool,
    pub include: Option<&'a Pattern>,
    pub ignored_names: &'a [&'a str],
}

pub fn collect_directory_matches(query: &GrepQuery<'_>, absolute_root: &Path) -> GrepResult {
    collect_directory_matches_with_options(query, absolute_root, &discovery_options(query))
}

pub fn count_directory_matches(query: &GrepQuery<'_>, absolute_root: &Path) -> CountResult {
    let mut counter = LineCounter::default();
    let candidates = search_directory(
        query,
        absolute_root,
        &discovery_options(query),
        &mut counter,
    );
    counter.finish(candidates)
}

pub fn collect_regular_file_root(
    query: &GrepQuery<'_>,
    absolute_path: &Path,
) -> Result<GrepResult, PathError> {
    let mut collector = MatchCollector::default();
    let candidates = search_regular_file(query, absolute_path, &mut collector)?;
    Ok(collector.finish(candidates))
}

pub fn count_regular_file_root(
    query: &GrepQuery<'_>,
    absolute_path: &Path,
) -> Result<CountResult, PathError> {
    let mut counter = LineCounter::default();
    let candidates = search_regular_file(query, absolute_path, &mut counter)?;
    Ok(counter.finish(candidates))
}

pub fn read_model_safe(path: &Path, content: &mut Vec<u8>) -> Result<bool, PathError> {
    content.clear();
    let file = match open_regular_file(path) {
        Ok((file, _)) => file,
        Err(RegularFileError::NotRegularFile) => return Ok(false),
        Err(RegularFileError::Path(error)) => return Err(error),
    };
    file.take(FILE_BYTE_CAP as u64 + 1).read_to_end(content)?;
    Ok(content.len() <= FILE_BYTE_CAP && is_model_safe_text(content))
}

fn discovery_options<'a>(query: &GrepQuery<'a>) -> DiscoveryOptions<'a> {
    DiscoveryOptions {
        ignored_names: query.ignored_names,
        ..DiscoveryOptions::default()
    }
}

fn collect_directory_matches_with_options(
    query: &GrepQuery<'_>,
    absolute_root: &Path,
    options: &DiscoveryOptions<'_>,
) -> GrepResult {
    let mut collector = MatchCollector::default();
    let candidates = search_directory(query, absolute_root, options, &mut collector);
    collector.finish(candidates)
}

trait GrepSink {
    fn git_grep(&mut self, query: &GrepQuery<'_>, absolute_root: &Path) -> bool;

    fn scan(
        &mut self,
        scanner: &mut FileScanner,
        display_path: &Path,
        read_path: &Path,
    ) -> Result<(), PathError>;

    fn is_full(&self) -> bool;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RootStatus {
    Ignored,
    InRepository,
    OutsideRepository,
}

fn search_directory(
    query: &GrepQuery<'_>,
    absolute_root: &Path,
    options: &DiscoveryOptions<'_>,
    sink: &mut impl GrepSink,
) -> CandidateStats {
    let root_status = if options.force_fallback {
        RootStatus::OutsideRepository
    } else {
        probe_git_root(absolute_root)
    };
    let mut stats = CandidateStats::default();
    if root_status == RootStatus::InRepository && sink.git_grep(query, absolute_root) {
        if !sink.is_full() {
            let untracked = discover(
                absolute_root,
                &DiscoveryOptions {
                    untracked: UntrackedFiles::Only,
                    ..*options
                },
            );
            stats.absorb(untracked.stats);
            scan_candidates(query, absolute_root, &untracked.files, sink);
        }
        return stats;
    }
    let candidates = discover(
        absolute_root,
        &DiscoveryOptions {
            untracked: UntrackedFiles::Include,
            force_fallback: root_status == RootStatus::OutsideRepository,
            ..*options
        },
    );
    stats.absorb(candidates.stats);
    scan_candidates(query, absolute_root, &candidates.files, sink);
    stats
}

fn search_regular_file(
    query: &GrepQuery<'_>,
    absolute_path: &Path,
    sink: &mut impl GrepSink,
) -> Result<CandidateStats, PathError> {
    let matches_include = query.include.is_none_or(|include| {
        include.matches_basename(basename(absolute_path.as_os_str().as_bytes()))
    });
    if !matches_include {
        return Ok(CandidateStats::default());
    }
    sink.scan(&mut FileScanner::new(query), absolute_path, absolute_path)?;
    Ok(CandidateStats::default())
}

fn probe_git_root(absolute_root: &Path) -> RootStatus {
    if !git_work_tree_contains(absolute_root) {
        return RootStatus::OutsideRepository;
    }
    let status = git_command(absolute_root).and_then(|mut command| {
        command
            .args(["check-ignore", "-q", "--", "."])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()
    });
    match status.and_then(|status| status.code()) {
        Some(0) => RootStatus::Ignored,
        Some(1) => RootStatus::InRepository,
        _ => RootStatus::OutsideRepository,
    }
}

fn scan_candidates(
    query: &GrepQuery<'_>,
    provider_root: &Path,
    candidates: &CandidatePaths,
    sink: &mut impl GrepSink,
) {
    let mut scanner = FileScanner::new(query);
    for candidate in candidates.iter() {
        if sink.is_full() {
            return;
        }
        if query
            .include
            .is_some_and(|include| !include.matches_path(candidate))
        {
            continue;
        }
        let display_path = provider_root.join(OsStr::from_bytes(candidate));
        let Some((file, _)) = locate_file(query.workspace_root, provider_root, display_path) else {
            continue;
        };
        let _ = sink.scan(&mut scanner, &file.display_path, &file.read_path);
    }
}

#[derive(Clone)]
struct CandidateFile {
    display_path: PathBuf,
    read_path: PathBuf,
}

fn locate_file(
    workspace_root: &Path,
    search_root: &Path,
    display_path: PathBuf,
) -> Option<(CandidateFile, Metadata)> {
    if display_path.as_os_str().len() > MAX_PATH_BYTES {
        return None;
    }
    let metadata = fs::symlink_metadata(&display_path).ok()?;
    let is_symlink = metadata.file_type().is_symlink();
    if !metadata.is_file() && !is_symlink {
        return None;
    }
    let read_path = fs::canonicalize(&display_path).ok()?;
    let readable = path_inside(workspace_root, &read_path)
        || (!is_symlink && path_inside(search_root, &read_path));
    readable.then_some((
        CandidateFile {
            display_path,
            read_path,
        },
        metadata,
    ))
}

struct FileScanner {
    finder: Finder<'static>,
    case_insensitive: bool,
    searchable: bool,
    content: Vec<u8>,
    folded: Vec<u8>,
}

impl FileScanner {
    fn new(query: &GrepQuery<'_>) -> Self {
        let pattern = query.pattern.as_bytes();
        let finder = if query.case_insensitive {
            Finder::new(&pattern.to_ascii_lowercase()).into_owned()
        } else {
            Finder::new(pattern).into_owned()
        };
        Self {
            finder,
            case_insensitive: query.case_insensitive,
            searchable: memchr(b'\n', pattern).is_none(),
            content: Vec::new(),
            folded: Vec::new(),
        }
    }

    fn load(&mut self, read_path: &Path) -> Result<bool, PathError> {
        if !read_model_safe(read_path, &mut self.content)? {
            return Ok(false);
        }
        if self.case_insensitive {
            self.folded.clear();
            self.folded.extend_from_slice(&self.content);
            self.folded.make_ascii_lowercase();
        }
        Ok(true)
    }

    fn matching_lines(&self) -> MatchingLines<'_> {
        MatchingLines {
            haystack: if self.case_insensitive {
                &self.folded
            } else {
                &self.content
            },
            finder: &self.finder,
            position: self.searchable.then_some(0),
        }
    }
}

struct MatchingLines<'a> {
    haystack: &'a [u8],
    finder: &'a Finder<'static>,
    position: Option<usize>,
}

impl Iterator for MatchingLines<'_> {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Range<usize>> {
        let position = self.position.take()?;
        let hit = position + self.finder.find(&self.haystack[position..])?;
        let start = memrchr(b'\n', &self.haystack[position..hit])
            .map_or(position, |offset| position + offset + 1);
        let end =
            memchr(b'\n', &self.haystack[hit..]).map_or(self.haystack.len(), |offset| hit + offset);
        if end < self.haystack.len() {
            self.position = Some(end + 1);
        }
        Some(start..end)
    }
}

#[derive(Default)]
struct MatchCollector {
    matches: Vec<Match>,
    truncated_reason: Option<TruncatedReason>,
}

impl MatchCollector {
    fn push(&mut self, found: Match) {
        self.matches.push(found);
        if self.matches.len() >= COLLECTION_CAP {
            self.truncated_reason = Some(TruncatedReason::CollectionCap);
        }
    }

    fn finish(self, candidates: CandidateStats) -> GrepResult {
        GrepResult {
            matches: self.matches,
            truncated_reason: self.truncated_reason,
            candidates,
        }
    }

    fn parse_git_grep(&mut self, query: &GrepQuery<'_>, absolute_root: &Path, raw: &[u8]) {
        let mut validated_files: HashMap<&[u8], CandidateFile> = HashMap::new();
        let mut skipped_paths: HashSet<&[u8]> = HashSet::new();
        let mut scratch = Vec::new();

        let mut index = 0;
        while index < raw.len() && !self.is_full() {
            let Some(path_end) = find_byte(raw, index, 0) else {
                break;
            };
            let path = &raw[index..path_end];
            let line_start = path_end + 1;
            let Some(line_end) = find_byte(raw, line_start, 0) else {
                break;
            };
            let content_start = line_end + 1;
            let content_end = find_byte(raw, content_start, b'\n').unwrap_or(raw.len());
            index = content_end.saturating_add(1).min(raw.len());

            if !git_path_candidate(query, path) || skipped_paths.contains(path) {
                continue;
            }
            let Some(line_number) = parse_decimal(&raw[line_start..line_end]) else {
                continue;
            };
            let line = &raw[content_start..content_end];
            if !is_model_safe_text(line) {
                skipped_paths.insert(path);
                continue;
            }
            let file = if let Some(existing) = validated_files.get(path) {
                existing.clone()
            } else {
                let Some(validated) = validate_matched_git_file(
                    query.workspace_root,
                    absolute_root,
                    path,
                    &mut scratch,
                ) else {
                    skipped_paths.insert(path);
                    continue;
                };
                validated_files.insert(path, validated.clone());
                validated
            };
            self.push(Match {
                absolute_path: file.display_path,
                read_path: file.read_path,
                line_number,
                line: String::from_utf8_lossy(line).into_owned(),
            });
        }
    }
}

impl GrepSink for MatchCollector {
    fn git_grep(&mut self, query: &GrepQuery<'_>, absolute_root: &Path) -> bool {
        let Some(raw) = run_git_grep(query, absolute_root, GitGrepOutput::Lines) else {
            return false;
        };
        self.parse_git_grep(query, absolute_root, &raw);
        true
    }

    fn scan(
        &mut self,
        scanner: &mut FileScanner,
        display_path: &Path,
        read_path: &Path,
    ) -> Result<(), PathError> {
        if self.is_full() || !scanner.load(read_path)? {
            return Ok(());
        }
        let content = &scanner.content;
        let mut line_number = 1;
        let mut counted = 0;
        for line in scanner.matching_lines() {
            line_number += memchr_iter(b'\n', &content[counted..line.start]).count();
            counted = line.start;
            self.push(Match {
                absolute_path: display_path.to_path_buf(),
                read_path: read_path.to_path_buf(),
                line_number,
                line: String::from_utf8_lossy(&content[line]).into_owned(),
            });
            if self.is_full() {
                break;
            }
        }
        Ok(())
    }

    fn is_full(&self) -> bool {
        self.truncated_reason.is_some()
    }
}

#[derive(Default)]
struct LineCounter {
    matching_lines: usize,
    matching_files: usize,
}

impl LineCounter {
    fn add_file(&mut self, matching_lines: usize) {
        if matching_lines > 0 {
            self.matching_files += 1;
            self.matching_lines += matching_lines;
        }
    }

    fn finish(self, candidates: CandidateStats) -> CountResult {
        CountResult {
            matching_lines: self.matching_lines,
            matching_files: self.matching_files,
            candidates,
        }
    }

    fn parse_git_grep(&mut self, query: &GrepQuery<'_>, absolute_root: &Path, raw: &[u8]) {
        let mut skipped_paths: HashSet<&[u8]> = HashSet::new();
        let mut scratch = Vec::new();

        let mut index = 0;
        while index < raw.len() {
            let Some(path_end) = find_byte(raw, index, 0) else {
                break;
            };
            let path = &raw[index..path_end];
            let count_start = path_end + 1;
            let count_end = find_byte(raw, count_start, b'\n').unwrap_or(raw.len());
            index = count_end.saturating_add(1).min(raw.len());

            if !git_path_candidate(query, path) || skipped_paths.contains(path) {
                continue;
            }
            let raw_count = trim_trailing_carriage_returns(&raw[count_start..count_end]);
            let Some(line_count) = parse_decimal(raw_count) else {
                continue;
            };
            if line_count == 0 {
                continue;
            }
            if validate_matched_git_file(query.workspace_root, absolute_root, path, &mut scratch)
                .is_none()
            {
                skipped_paths.insert(path);
                continue;
            }
            self.add_file(line_count);
        }
    }
}

impl GrepSink for LineCounter {
    fn git_grep(&mut self, query: &GrepQuery<'_>, absolute_root: &Path) -> bool {
        let Some(raw) = run_git_grep(query, absolute_root, GitGrepOutput::Counts) else {
            return false;
        };
        self.parse_git_grep(query, absolute_root, &raw);
        true
    }

    fn scan(
        &mut self,
        scanner: &mut FileScanner,
        _display_path: &Path,
        read_path: &Path,
    ) -> Result<(), PathError> {
        if scanner.load(read_path)? {
            self.add_file(scanner.matching_lines().count());
        }
        Ok(())
    }

    fn is_full(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GitGrepOutput {
    Lines,
    Counts,
}

fn git_grep_arguments(query: &GrepQuery<'_>, output: GitGrepOutput) -> Vec<OsString> {
    let mode = match output {
        GitGrepOutput::Lines => "-n",
        GitGrepOutput::Counts => "--count",
    };
    let mut arguments: Vec<OsString> = GIT_GREP_OUTPUT_CONFIG.iter().map(OsString::from).collect();
    arguments.extend(["grep".into(), mode.into()]);
    arguments.extend(GIT_GREP_LITERAL_FLAGS.iter().map(OsString::from));
    if query.case_insensitive {
        arguments.push("-i".into());
    }
    arguments.extend([
        "-e".into(),
        c_string_argument(query.pattern.as_bytes()),
        "--".into(),
    ]);
    let pathspec = safe_git_include_pathspec(query.include).unwrap_or(b".");
    arguments.push(c_string_argument(pathspec));
    arguments
}

fn c_string_argument(bytes: &[u8]) -> OsString {
    let end = memchr(0, bytes).unwrap_or(bytes.len());
    OsStr::from_bytes(&bytes[..end]).to_os_string()
}

fn run_git_grep(
    query: &GrepQuery<'_>,
    absolute_root: &Path,
    output: GitGrepOutput,
) -> Option<Vec<u8>> {
    if query.pattern.is_empty() {
        return None;
    }
    let mut command = git_command(absolute_root)?;
    command.args(git_grep_arguments(query, output));
    let output = run_bounded(&mut command, GIT_GREP_STDOUT_LIMIT)?;
    match output.status.code() {
        Some(0) => Some(output.stdout),
        Some(1) => Some(Vec::new()),
        _ => None,
    }
}

fn git_path_candidate(query: &GrepQuery<'_>, path: &[u8]) -> bool {
    !path.is_empty()
        && path.len() <= MAX_RELATIVE_PATH_BYTES
        && query
            .include
            .is_none_or(|include| include.matches_path(path))
}

fn validate_matched_git_file(
    workspace_root: &Path,
    absolute_root: &Path,
    path: &[u8],
    scratch: &mut Vec<u8>,
) -> Option<CandidateFile> {
    let display_path = absolute_root.join(OsStr::from_bytes(path));
    let (file, metadata) = locate_file(workspace_root, absolute_root, display_path)?;
    if metadata.len() > FILE_BYTE_CAP as u64 {
        return None;
    }
    read_model_safe(&file.read_path, scratch)
        .ok()?
        .then_some(file)
}

fn safe_git_include_pathspec(include: Option<&Pattern>) -> Option<&[u8]> {
    let raw = include?.raw();
    let widens_to_every_basename_match = raw.first() == Some(&b'*')
        && !raw
            .iter()
            .any(|byte| matches!(byte, b'/' | b'\\' | b'[' | b'{' | b'}' | 0));
    widens_to_every_basename_match.then_some(raw)
}

fn find_byte(haystack: &[u8], start: usize, needle: u8) -> Option<usize> {
    memchr(needle, haystack.get(start..)?).map(|offset| start + offset)
}

fn parse_decimal(digits: &[u8]) -> Option<usize> {
    std::str::from_utf8(digits).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::ignored_dirs::IGNORED_DIRECTORY_NAMES;
    use crate::workspace_files::tests::{arm_hostile_git_config, run_git};

    struct Workspace {
        _temp: TempDir,
        root: PathBuf,
    }

    impl Workspace {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let root = fs::canonicalize(temp.path()).unwrap();
            Self { _temp: temp, root }
        }

        fn write(&self, relative: &str, content: impl AsRef<[u8]>) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            path
        }

        fn git(&self, args: &[&str]) -> bool {
            run_git(&self.root, args)
        }
    }

    fn query<'a>(workspace_root: &'a Path, include: Option<&'a Pattern>) -> GrepQuery<'a> {
        GrepQuery {
            workspace_root,
            pattern: "needle",
            case_insensitive: false,
            include,
            ignored_names: IGNORED_DIRECTORY_NAMES,
        }
    }

    fn lines(result: &GrepResult) -> Vec<&str> {
        result
            .matches
            .iter()
            .map(|found| found.line.as_str())
            .collect()
    }

    fn content_with_prefix(len: usize, prefix: &str) -> Vec<u8> {
        let mut content = vec![b'x'; len];
        content[..prefix.len()].copy_from_slice(prefix.as_bytes());
        content
    }

    fn split_line_matches(
        content: &[u8],
        pattern: &[u8],
        case_insensitive: bool,
    ) -> Vec<(usize, Vec<u8>)> {
        let fold = |bytes: &[u8]| {
            if case_insensitive {
                bytes.to_ascii_lowercase()
            } else {
                bytes.to_vec()
            }
        };
        let needle = fold(pattern);
        content
            .split(|byte| *byte == b'\n')
            .enumerate()
            .filter(|(_, line)| {
                let line = fold(line);
                needle.is_empty() || line.windows(needle.len()).any(|window| window == needle)
            })
            .map(|(index, line)| (index + 1, line.to_vec()))
            .collect()
    }

    fn scanned_line_matches(
        content: &[u8],
        pattern: &str,
        case_insensitive: bool,
    ) -> Vec<(usize, Vec<u8>)> {
        let query = GrepQuery {
            workspace_root: Path::new("/workspace"),
            pattern,
            case_insensitive,
            include: None,
            ignored_names: &[],
        };
        let mut scanner = FileScanner::new(&query);
        scanner.content = content.to_vec();
        scanner.folded = content.to_ascii_lowercase();
        scanner
            .matching_lines()
            .map(|line| {
                let line_number = 1 + memchr_iter(b'\n', &content[..line.start]).count();
                (line_number, content[line].to_vec())
            })
            .collect()
    }

    #[test]
    fn grep_search_whole_buffer_scan_matches_split_line_semantics() {
        let contents: [&[u8]; 9] = [
            b"",
            b"\n",
            b"a",
            b"a\n",
            b"ab\nab\n",
            b"xa\nb\nab",
            b"aaa\n\naaa",
            b"A\nb\nAB\n",
            b"needle needle\nno\nNEEDLE\n",
        ];
        let patterns = ["", "a", "ab", "a\nb", "\n", "needle", "B"];
        for content in contents {
            for pattern in patterns {
                for case_insensitive in [false, true] {
                    assert_eq!(
                        scanned_line_matches(content, pattern, case_insensitive),
                        split_line_matches(content, pattern.as_bytes(), case_insensitive),
                        "{content:?} {pattern:?} {case_insensitive}"
                    );
                }
            }
        }
    }

    #[test]
    fn grep_search_scanner_reuses_one_buffer_across_files() {
        let workspace = Workspace::new();
        let long = workspace.write("long.txt", "needle long\n".repeat(100));
        let short = workspace.write("short.txt", "short needle\n");
        let query = query(&workspace.root, None);
        let mut scanner = FileScanner::new(&query);
        let mut collector = MatchCollector::default();

        collector.scan(&mut scanner, &long, &long).unwrap();
        let capacity = scanner.content.capacity();
        collector.scan(&mut scanner, &short, &short).unwrap();

        assert_eq!(scanner.content.capacity(), capacity);
        assert_eq!(collector.matches.len(), 101);
        assert_eq!(collector.matches[100].line, "short needle");
        assert_eq!(collector.matches[100].line_number, 1);
    }

    #[test]
    fn grep_search_git_grep_argv_uses_literal_fixed_string_flags() {
        let query = GrepQuery {
            workspace_root: Path::new("/workspace"),
            pattern: "needle",
            case_insensitive: false,
            include: None,
            ignored_names: &[],
        };
        assert_eq!(
            git_grep_arguments(&query, GitGrepOutput::Lines),
            [
                GIT_GREP_OUTPUT_CONFIG,
                &["grep", "-n", "-I", "-F", "-z", "-e", "needle", "--", "."]
            ]
            .concat()
        );

        let include = Pattern::compile(b"*.zig").unwrap();
        let insensitive = GrepQuery {
            case_insensitive: true,
            include: Some(&include),
            ..query
        };
        assert_eq!(
            git_grep_arguments(&insensitive, GitGrepOutput::Counts),
            [
                GIT_GREP_OUTPUT_CONFIG,
                &[
                    "grep", "--count", "-I", "-F", "-z", "-i", "-e", "needle", "--", "*.zig"
                ]
            ]
            .concat()
        );
    }

    #[test]
    fn grep_search_passes_git_only_includes_that_keep_every_basename_match() {
        let pathspec = |include: &str| {
            let compiled = Pattern::compile(include.as_bytes()).unwrap();
            safe_git_include_pathspec(Some(&compiled)).map(<[u8]>::to_vec)
        };
        for kept in ["*.zig", "*", "*test?.rs", "**.txt", "*]x"] {
            assert_eq!(pathspec(kept), Some(kept.as_bytes().to_vec()), "{kept}");
        }
        for dropped in [
            "main.rs",
            "foo*.rs",
            "?.rs",
            "*[ab].txt",
            "*\\x",
            "*.{md,txt}",
            "*/x.rs",
            ":!*.rs",
            "*.rs\0x",
            "",
        ] {
            assert_eq!(pathspec(dropped), None, "{dropped:?}");
        }
        assert_eq!(safe_git_include_pathspec(None), None);
    }

    #[test]
    fn grep_search_includes_match_tracked_and_untracked_files_as_the_local_matcher_does() {
        let workspace = Workspace::new();
        workspace.write("foo[ab].txt", "needle tracked bracket\n");
        workspace.write("fooa.txt", "needle tracked class\n");
        workspace.write("main.rs", "needle top main\n");
        workspace.write("src/main.rs", "needle nested main\n");
        if !workspace.git(&["init", "--quiet"]) || !workspace.git(&["add", "."]) {
            return;
        }
        workspace.write("bar[ab].txt", "needle untracked bracket\n");
        workspace.write("src/untracked[ab].txt", "needle nested untracked bracket\n");

        for (include, expected) in [
            (
                "*[ab].txt",
                [
                    "needle nested untracked bracket",
                    "needle tracked bracket",
                    "needle untracked bracket",
                ]
                .as_slice(),
            ),
            (
                "main.rs",
                ["needle nested main", "needle top main"].as_slice(),
            ),
        ] {
            let include = Pattern::compile(include.as_bytes()).unwrap();
            let query = query(&workspace.root, Some(&include));
            let result = collect_directory_matches(&query, &workspace.root);
            let count = count_directory_matches(&query, &workspace.root);
            let mut found = lines(&result);
            found.sort_unstable();

            assert_eq!(found, expected);
            assert_eq!(
                (count.matching_lines, count.matching_files),
                (expected.len(), expected.len())
            );
        }
    }

    #[test]
    fn grep_search_git_grep_argv_cuts_the_pattern_at_nul_like_a_c_argument() {
        let query = GrepQuery {
            pattern: "needle\0zzz",
            ..query(Path::new("/workspace"), None)
        };
        assert_eq!(
            git_grep_arguments(&query, GitGrepOutput::Lines),
            [
                GIT_GREP_OUTPUT_CONFIG,
                &["grep", "-n", "-I", "-F", "-z", "-e", "needle", "--", "."]
            ]
            .concat()
        );
    }

    #[test]
    fn grep_search_searches_tracked_files_for_the_pattern_before_its_first_nul() {
        let workspace = Workspace::new();
        workspace.write("tracked.txt", "needle tracked\n");
        if !workspace.git(&["init", "--quiet"]) || !workspace.git(&["add", "tracked.txt"]) {
            return;
        }
        workspace.write("untracked.txt", "needle untracked\n");
        let query = GrepQuery {
            pattern: "needle\0zzz",
            ..query(&workspace.root, None)
        };

        let result = collect_directory_matches(&query, &workspace.root);
        let count = count_directory_matches(&query, &workspace.root);

        assert_eq!(lines(&result), ["needle tracked"]);
        assert_eq!((count.matching_lines, count.matching_files), (1, 1));
    }

    #[test]
    fn grep_search_scans_the_search_root_when_core_worktree_points_elsewhere() {
        let workspace = Workspace::new();
        let elsewhere = Workspace::new();
        elsewhere.write("a.txt", "needle outside\nneedle outside again\n");
        workspace.write("a.txt", "needle inside\n");
        if !workspace.git(&["init", "--quiet"]) || !workspace.git(&["add", "a.txt"]) {
            return;
        }
        let worktree = elsewhere.root.to_str().unwrap();
        assert!(workspace.git(&["config", "core.worktree", worktree]));

        let query = query(&workspace.root, None);
        let result = collect_directory_matches(&query, &workspace.root);
        let count = count_directory_matches(&query, &workspace.root);

        assert_eq!(lines(&result), ["needle inside"]);
        assert_eq!(result.matches[0].read_path, workspace.root.join("a.txt"));
        assert_eq!((count.matching_lines, count.matching_files), (1, 1));
    }

    #[test]
    fn grep_search_scans_the_search_root_when_a_git_file_names_a_work_tree_elsewhere() {
        let workspace = Workspace::new();
        let elsewhere = Workspace::new();
        elsewhere.write("a.txt", "needle outside\nneedle outside again\n");
        workspace.write("a.txt", "needle inside\n");
        if !elsewhere.git(&["init", "--quiet"]) || !elsewhere.git(&["add", "a.txt"]) {
            return;
        }
        let worktree = elsewhere.root.to_str().unwrap();
        assert!(elsewhere.git(&["config", "core.worktree", worktree]));
        let git_dir = elsewhere.root.join(".git");
        workspace.write(".git", format!("gitdir: {}\n", git_dir.display()));

        let query = query(&workspace.root, None);
        let result = collect_directory_matches(&query, &workspace.root);
        let count = count_directory_matches(&query, &workspace.root);

        assert_eq!(lines(&result), ["needle inside"]);
        assert_eq!((count.matching_lines, count.matching_files), (1, 1));
    }

    #[test]
    fn grep_search_reads_the_search_root_through_a_git_file_without_a_work_tree() {
        let workspace = Workspace::new();
        let elsewhere = Workspace::new();
        elsewhere.write("a.txt", "needle outside\nneedle outside again\n");
        workspace.write("a.txt", "needle inside\n");
        if !elsewhere.git(&["init", "--quiet"]) || !elsewhere.git(&["add", "a.txt"]) {
            return;
        }
        let git_dir = elsewhere.root.join(".git");
        workspace.write(".git", format!("gitdir: {}\n", git_dir.display()));

        let query = query(&workspace.root, None);
        let result = collect_directory_matches(&query, &workspace.root);

        assert_eq!(lines(&result), ["needle inside"]);
        assert_eq!(result.matches[0].read_path, workspace.root.join("a.txt"));
    }

    #[test]
    fn grep_search_scans_the_search_root_of_a_bare_repository() {
        let workspace = Workspace::new();
        workspace.write("a.txt", "needle inside\n");
        if !workspace.git(&["init", "--quiet"]) || !workspace.git(&["add", "a.txt"]) {
            return;
        }
        assert!(workspace.git(&["config", "core.bare", "true"]));
        workspace.write("untracked.txt", "needle untracked\n");

        let query = query(&workspace.root, None);
        let result = collect_directory_matches(&query, &workspace.root);
        let mut found = lines(&result);
        found.sort_unstable();

        assert_eq!(found, ["needle inside", "needle untracked"]);
    }

    #[test]
    fn grep_search_preserves_explicitly_requested_ignored_directory_roots() {
        let workspace = Workspace::new();
        workspace.write("node_modules/pkg/ignored.txt", "needle ignored\n");

        let query = query(&workspace.root, None);
        let result = collect_directory_matches(&query, &workspace.root.join("node_modules/pkg"));

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.truncated_reason, None);
        assert!(
            result.matches[0]
                .absolute_path
                .ends_with("node_modules/pkg/ignored.txt")
        );
        assert_eq!(result.matches[0].line, "needle ignored");
    }

    #[test]
    fn grep_search_preserves_explicitly_requested_ignored_file_roots() {
        let workspace = Workspace::new();
        let ignored = workspace.write("node_modules/pkg/ignored.txt", "needle ignored\n");

        let query = query(&workspace.root, None);
        let result = collect_regular_file_root(&query, &ignored).unwrap();

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.truncated_reason, None);
        assert_eq!(result.matches[0].absolute_path, ignored);
        assert_eq!(result.matches[0].line, "needle ignored");
    }

    #[test]
    fn grep_search_does_not_ignore_workspace_because_ignored_name_is_outside_workspace() {
        let workspace = Workspace::new();
        workspace.write("build/workspace/src/file.txt", "needle kept\n");
        let root = workspace.root.join("build/workspace");

        let query = query(&root, None);
        let result = collect_directory_matches(&query, &root);

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.truncated_reason, None);
        assert!(result.matches[0].absolute_path.ends_with("src/file.txt"));
    }

    #[test]
    fn grep_search_falls_back_to_zig_scanner_outside_git_repositories() {
        let workspace = Workspace::new();
        workspace.write("src/main.zig", "needle fallback\n");

        let query = query(&workspace.root, None);
        let result = collect_directory_matches(&query, &workspace.root);

        assert_eq!(result.matches.len(), 1);
        assert!(result.matches[0].absolute_path.ends_with("src/main.zig"));
        assert_eq!(result.matches[0].line, "needle fallback");
    }

    #[test]
    fn grep_search_scans_untracked_files_after_git_grep_tracked_backend() {
        let workspace = Workspace::new();
        if !workspace.git(&["init", "--quiet"]) {
            return;
        }
        workspace.write("tracked.txt", "no match\n");
        if !workspace.git(&["add", "tracked.txt"]) {
            return;
        }
        workspace.write("untracked.txt", "needle untracked\n");

        let query = query(&workspace.root, None);
        let result = collect_directory_matches(&query, &workspace.root);

        assert_eq!(result.matches.len(), 1);
        assert!(result.matches[0].absolute_path.ends_with("untracked.txt"));
        assert_eq!(result.matches[0].line, "needle untracked");
    }

    #[test]
    fn grep_search_git_grep_backend_skips_files_with_unsafe_bytes_outside_matched_line() {
        let workspace = Workspace::new();
        if !workspace.git(&["init", "--quiet"]) {
            return;
        }
        workspace.write("unsafe.txt", b"needle safe\ninvalid \xff bytes\n");
        if !workspace.git(&["add", "unsafe.txt"]) {
            return;
        }

        let query = query(&workspace.root, None);
        let result = collect_directory_matches(&query, &workspace.root);

        assert!(result.matches.is_empty());
    }

    #[test]
    fn grep_search_logs_skipped_non_model_safe_files() {
        let workspace = Workspace::new();
        let binary = workspace.write("binary.txt", b"needle\0binary\n");

        let query = query(&workspace.root, None);
        let result = collect_regular_file_root(&query, &binary).unwrap();

        assert!(result.matches.is_empty());
        assert_eq!(result.truncated_reason, None);
    }

    #[test]
    fn grep_search_logs_skipped_per_file_scan_errors_during_directory_traversal() {
        let workspace = Workspace::new();
        workspace.write("good.txt", "needle good\n");
        symlink("missing-target.txt", workspace.root.join("broken.txt")).unwrap();

        let include = Pattern::compile(b"*.txt").unwrap();
        let query = query(&workspace.root, Some(&include));
        let result = collect_directory_matches(&query, &workspace.root);

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.truncated_reason, None);
    }

    #[test]
    fn grep_search_directory_traversal_skips_external_symlink_targets_with_trace() {
        let workspace = Workspace::new();
        let external = Workspace::new();
        workspace.write("target/internal.txt", "needle internal\n");
        let external_target = external.write("outside.txt", "needle external\n");
        fs::create_dir_all(workspace.root.join("links")).unwrap();
        symlink(
            "../target/internal.txt",
            workspace.root.join("links/internal.txt"),
        )
        .unwrap();
        symlink(&external_target, workspace.root.join("links/external.txt")).unwrap();

        let query = query(&workspace.root, None);
        let result = collect_directory_matches(&query, &workspace.root.join("links"));

        assert_eq!(result.matches.len(), 1);
        assert!(
            result.matches[0]
                .absolute_path
                .ends_with("links/internal.txt")
        );
        assert_eq!(result.matches[0].line, "needle internal");
    }

    #[test]
    fn grep_search_never_reads_a_candidate_through_a_parent_swapped_after_it_was_located() {
        let workspace = Workspace::new();
        let external = Workspace::new();
        workspace.write("dir/notes.txt", "needle inside\n");
        external.write("notes.txt", "needle outside\n");
        let (file, _) = locate_file(
            &workspace.root,
            &workspace.root,
            workspace.root.join("dir/notes.txt"),
        )
        .unwrap();

        fs::rename(workspace.root.join("dir"), workspace.root.join("moved")).unwrap();
        symlink(&external.root, workspace.root.join("dir")).unwrap();
        let mut content = Vec::new();

        assert_eq!(read_model_safe(&file.read_path, &mut content), Ok(false));
        assert!(content.is_empty());
    }

    #[test]
    fn grep_search_scans_files_exactly_at_byte_cap() {
        let workspace = Workspace::new();
        let exact = workspace.write("exact.txt", content_with_prefix(FILE_BYTE_CAP, "needle\n"));

        let query = query(&workspace.root, None);
        let result = collect_regular_file_root(&query, &exact).unwrap();

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.truncated_reason, None);
        assert!(result.matches[0].absolute_path.ends_with("exact.txt"));
        assert_eq!(result.matches[0].line_number, 1);
        assert_eq!(result.matches[0].line, "needle");
    }

    #[test]
    fn grep_search_logs_oversized_files_and_continues_directory_traversal() {
        let workspace = Workspace::new();
        workspace.write("good.txt", "needle good\n");
        workspace.write(
            "large.txt",
            content_with_prefix(FILE_BYTE_CAP + 1, "needle hidden\n"),
        );

        let query = query(&workspace.root, None);
        let result = collect_directory_matches(&query, &workspace.root);

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.truncated_reason, None);
        assert!(result.matches[0].absolute_path.ends_with("good.txt"));
    }

    #[test]
    fn grep_search_skips_oversized_regular_file_roots_with_trace() {
        let workspace = Workspace::new();
        let large = workspace.write(
            "large.txt",
            content_with_prefix(FILE_BYTE_CAP + 1, "needle hidden\n"),
        );

        let query = query(&workspace.root, None);
        let result = collect_regular_file_root(&query, &large).unwrap();

        assert!(result.matches.is_empty());
        assert_eq!(result.truncated_reason, None);
    }

    #[test]
    fn grep_search_finds_match_beyond_former_traversal_cap() {
        let workspace = Workspace::new();
        for index in 0..2050 {
            workspace.write(&format!("many/file-{index:04}.txt"), "not here\n");
        }
        workspace.write("zzzz/match.txt", "needle late\n");

        let query = query(&workspace.root, None);
        let result = collect_directory_matches(&query, &workspace.root);

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.truncated_reason, None);
        assert!(result.matches[0].absolute_path.ends_with("zzzz/match.txt"));
    }

    #[test]
    fn grep_files_path_narrowing_applies_before_candidate_cap() {
        let workspace = Workspace::new();
        workspace.write("aaa/outside.txt", "needle outside\n");
        workspace.write("src/core/target.txt", "needle target\n");
        let narrowed_root = workspace.root.join("src/core");

        let query = query(&workspace.root, None);
        let options = DiscoveryOptions {
            candidate_cap: 1,
            force_fallback: true,
            ..discovery_options(&query)
        };
        let result = collect_directory_matches_with_options(&query, &narrowed_root, &options);

        assert!(!result.candidates.incomplete);
        assert_eq!(result.matches.len(), 1);
        assert!(
            result.matches[0]
                .absolute_path
                .ends_with("src/core/target.txt")
        );
        assert_eq!(result.matches[0].line, "needle target");
    }

    #[test]
    fn grep_search_collection_cap_saturation_reports_metadata() {
        let workspace = Workspace::new();
        let path = workspace.write("many.txt", "needle\n".repeat(COLLECTION_CAP + 1));

        let query = query(&workspace.root, None);
        let result = collect_regular_file_root(&query, &path).unwrap();

        assert_eq!(result.matches.len(), COLLECTION_CAP);
        assert_eq!(
            result.truncated_reason,
            Some(TruncatedReason::CollectionCap)
        );
    }

    #[test]
    fn grep_search_collection_cap_takes_precedence_at_traversal_boundary() {
        let workspace = Workspace::new();
        for index in 0..100 {
            workspace.write(&format!("file-{index:04}.txt"), "not here\n");
        }
        workspace.write("zzzz/many.txt", "needle\n".repeat(COLLECTION_CAP + 1));

        let query = query(&workspace.root, None);
        let result = collect_directory_matches(&query, &workspace.root);

        assert_eq!(result.matches.len(), COLLECTION_CAP);
        assert_eq!(
            result.truncated_reason,
            Some(TruncatedReason::CollectionCap)
        );
    }

    #[test]
    fn grep_search_git_grep_output_ignores_format_config() {
        let workspace = Workspace::new();
        workspace.write("sub/a.txt", "alpha\nneedle tracked\n");
        if !workspace.git(&["init", "--quiet"]) || !workspace.git(&["add", "sub/a.txt"]) {
            return;
        }
        let hostile = [
            ("color.grep", "always"),
            ("color.ui", "always"),
            ("grep.column", "true"),
            ("grep.fullName", "true"),
            ("grep.lineNumber", "true"),
            ("grep.patternType", "perl"),
            ("grep.extendedRegexp", "true"),
            ("grep.fallbackToNoIndex", "true"),
            ("core.quotePath", "true"),
        ];
        for (key, value) in hostile {
            assert!(workspace.git(&["config", key, value]), "{key}");
        }

        let query = query(&workspace.root, None);
        for root in [workspace.root.clone(), workspace.root.join("sub")] {
            let result = collect_directory_matches(&query, &root);
            assert_eq!(lines(&result), ["needle tracked"], "{}", root.display());
            assert_eq!(
                result.matches[0].absolute_path,
                workspace.root.join("sub/a.txt")
            );
            assert_eq!(result.matches[0].line_number, 2);
            let count = count_directory_matches(&query, &root);
            assert_eq!((count.matching_lines, count.matching_files), (1, 1));
        }
    }

    #[test]
    fn grep_search_never_runs_programs_from_repository_config() {
        let workspace = Workspace::new();
        workspace.write("tracked.txt", "needle tracked\n");
        if !workspace.git(&["init", "--quiet"]) || !workspace.git(&["add", "tracked.txt"]) {
            return;
        }
        let marker = arm_hostile_git_config(&workspace.root);
        workspace.write("untracked.txt", "needle untracked\n");

        let query = query(&workspace.root, None);
        let result = collect_directory_matches(&query, &workspace.root);
        let count = count_directory_matches(&query, &workspace.root);

        assert!(
            !marker.exists(),
            "{}",
            fs::read_to_string(&marker).unwrap_or_default()
        );
        assert_eq!(lines(&result), ["needle tracked", "needle untracked"]);
        assert_eq!(count.matching_lines, 2);
    }

    #[test]
    fn grep_search_never_fetches_missing_objects_from_a_promisor_remote() {
        let server = Workspace::new();
        let client = Workspace::new();
        server.write("lazy.txt", "needle lazy\n");
        let marker = client.root.join("fetched");
        let upload_pack = client.write(
            "upload-pack.sh",
            format!("#!/bin/sh\necho ran >> '{}'\nexit 1\n", marker.display()),
        );
        fs::set_permissions(&upload_pack, fs::Permissions::from_mode(0o755)).unwrap();
        let checkout = client.root.join("checkout");
        let server_url = format!("file://{}", server.root.display());
        let commit = ["-c", "user.name=t", "-c", "user.email=t@t"];
        let ready = server.git(&["init", "--quiet"])
            && server.git(&["add", "lazy.txt"])
            && server.git(&[&commit[..], &["commit", "--quiet", "-m", "init"]].concat())
            && server.git(&["config", "uploadpack.allowFilter", "true"])
            && client.git(&[
                "clone",
                "--quiet",
                "--filter=blob:none",
                "--no-checkout",
                &server_url,
                "checkout",
            ])
            && run_git(&checkout, &["read-tree", "HEAD"])
            && run_git(
                &checkout,
                &["update-index", "--assume-unchanged", "lazy.txt"],
            )
            && run_git(
                &checkout,
                &[
                    "config",
                    "remote.origin.uploadpack",
                    upload_pack.to_str().unwrap(),
                ],
            );
        if !ready {
            return;
        }

        let query = query(&checkout, None);
        let result = collect_directory_matches(&query, &checkout);
        let count = count_directory_matches(&query, &checkout);

        assert!(!marker.exists());
        assert!(result.matches.is_empty());
        assert_eq!(count.matching_lines, 0);
    }

    #[test]
    fn grep_search_skips_tracked_files_reached_through_a_symlinked_parent_directory() {
        let workspace = Workspace::new();
        let outside = Workspace::new();
        outside.write("secret.txt", "needle outside\n");
        workspace.write("dir/secret.txt", "needle inside\n");
        workspace.write("kept.txt", "needle kept\n");
        if !workspace.git(&["init", "--quiet"])
            || !workspace.git(&["add", "dir/secret.txt", "kept.txt"])
        {
            return;
        }
        fs::remove_dir_all(workspace.root.join("dir")).unwrap();
        symlink(&outside.root, workspace.root.join("dir")).unwrap();

        let query = query(&workspace.root, None);
        let result = collect_directory_matches(&query, &workspace.root);
        let count = count_directory_matches(&query, &workspace.root);

        assert_eq!(lines(&result), ["needle kept"]);
        assert_eq!((count.matching_lines, count.matching_files), (1, 1));
    }

    #[test]
    fn grep_search_reads_regular_files_under_an_external_search_root() {
        let workspace = Workspace::new();
        let external = Workspace::new();
        let file = external.write("nested/file.txt", "needle external\n");

        let query = query(&workspace.root, None);
        let result = collect_directory_matches(&query, &external.root);

        assert_eq!(lines(&result), ["needle external"]);
        assert_eq!(result.matches[0].absolute_path, file);
        assert_eq!(result.matches[0].read_path, file);
    }

    #[test]
    fn grep_search_skips_symlinks_to_fifos_without_blocking() {
        let plain = Workspace::new();
        let repository = Workspace::new();
        if !repository.git(&["init", "--quiet"]) {
            return;
        }
        for workspace in [&plain, &repository] {
            workspace.write("a.txt", "needle a\n");
            let fifo = workspace.root.join("pipe");
            assert!(
                Command::new("mkfifo")
                    .arg(&fifo)
                    .status()
                    .unwrap()
                    .success()
            );
            symlink("pipe", workspace.root.join("link.txt")).unwrap();
        }
        assert!(repository.git(&["add", "a.txt"]));

        for root in [plain.root.clone(), repository.root.clone()] {
            let (sender, receiver) = mpsc::channel();
            thread::spawn(move || {
                let query = query(&root, None);
                let result = collect_directory_matches(&query, &root);
                let count = count_directory_matches(&query, &root);
                let lines: Vec<String> =
                    result.matches.into_iter().map(|found| found.line).collect();
                let _ = sender.send((lines, count.matching_lines));
            });
            let (lines, matching_lines) = receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("grep returns instead of blocking on the fifo");
            assert_eq!(lines, ["needle a"]);
            assert_eq!(matching_lines, 1);
        }
    }

    #[test]
    fn grep_search_reads_symlinked_matches_through_their_resolved_target() {
        let workspace = Workspace::new();
        let target = workspace.write("target/internal.txt", "needle internal\n");
        symlink("target/internal.txt", workspace.root.join("link.txt")).unwrap();

        let include = Pattern::compile(b"link.txt").unwrap();
        let query = query(&workspace.root, Some(&include));
        let result = collect_directory_matches(&query, &workspace.root);

        assert_eq!(lines(&result), ["needle internal"]);
        assert_eq!(
            result.matches[0].absolute_path,
            workspace.root.join("link.txt")
        );
        assert_eq!(result.matches[0].read_path, target);
    }

    #[test]
    fn grep_search_counts_matching_lines_and_ascii_case_insensitively() {
        let workspace = Workspace::new();
        workspace.write("a.txt", "Needle one\nneedle two\nother\n");
        workspace.write("b.txt", "NEEDLE\n");
        workspace.write("c.txt", "none\n");

        let query = GrepQuery {
            case_insensitive: true,
            ..query(&workspace.root, None)
        };
        let count = count_directory_matches(&query, &workspace.root);

        assert_eq!(count.matching_lines, 3);
        assert_eq!(count.matching_files, 2);
    }
}
