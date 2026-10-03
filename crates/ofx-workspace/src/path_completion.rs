use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use ofx_text::is_terminal_safe;

use crate::file_index::{CandidateKind, MAX_PATH_LEN, MAX_SEARCH_RESULTS, NameQuery, SearchResult};
use crate::pathing::resolve_workspace_or_external_literal_path;

const DIRECTORY_SHORTCUTS: [&str; 3] = ["~", ".", ".."];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    WorkspaceIndex,
    ExplicitPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PathCompletionError {
    #[error("PathUnavailable")]
    Unavailable,
    #[error("Cancelled")]
    Cancelled,
}

struct ParsedQuery<'a> {
    parent: &'a str,
    display_prefix: String,
    basename_query: &'a str,
}

pub fn query_mode(query: &str) -> QueryMode {
    if parse_explicit_query(query).is_some() {
        QueryMode::ExplicitPath
    } else {
        QueryMode::WorkspaceIndex
    }
}

pub fn complete(
    workspace_root: &Path,
    home: Option<&OsStr>,
    query: &str,
    cancel: &AtomicBool,
    limit: usize,
) -> Result<Vec<SearchResult>, PathCompletionError> {
    checkpoint(cancel)?;
    let limit = limit.min(MAX_SEARCH_RESULTS);
    if limit == 0 {
        return Ok(Vec::new());
    }
    let Some(parsed) = parse_explicit_query(query) else {
        return Ok(Vec::new());
    };
    let Some(matcher) = NameQuery::new(parsed.basename_query) else {
        return Ok(Vec::new());
    };
    let resolved = resolve_workspace_or_external_literal_path(workspace_root, parsed.parent, home)
        .map_err(|_| PathCompletionError::Unavailable)?;
    checkpoint(cancel)?;
    let entries = fs::read_dir(&resolved).map_err(|_| PathCompletionError::Unavailable)?;
    let mut ranked: Vec<(_, SearchResult)> = Vec::with_capacity(limit + 1);
    for entry in entries {
        checkpoint(cancel)?;
        let entry = entry.map_err(|_| PathCompletionError::Unavailable)?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_terminal_safe(name.as_bytes()) {
            continue;
        }
        let Some(score) = matcher.score(&name) else {
            continue;
        };
        checkpoint(cancel)?;
        let Some(kind) = entry_kind(&entry) else {
            continue;
        };
        let mut path = String::with_capacity(parsed.display_prefix.len() + name.len());
        path.push_str(&parsed.display_prefix);
        path.push_str(&name);
        if path.len() > MAX_PATH_LEN || !is_terminal_safe(path.as_bytes()) {
            continue;
        }
        let position = ranked
            .iter()
            .position(|(existing, result)| matcher.better(&score, &path, existing, &result.path))
            .unwrap_or(ranked.len());
        if position >= limit {
            continue;
        }
        ranked.push((
            score,
            SearchResult {
                path,
                kind,
                matched_spans: Vec::new(),
            },
        ));
        let mut slot = ranked.len();
        while slot > position + 1
            && let Some([left, right]) = ranked.get_mut(slot - 2..slot)
        {
            std::mem::swap(left, right);
            slot -= 1;
        }
        ranked.truncate(limit);
    }
    let prefix_len = parsed.display_prefix.len();
    ranked
        .into_iter()
        .map(|(_, mut result)| {
            checkpoint(cancel)?;
            let spans = matcher
                .match_spans(result.path.get(prefix_len..).unwrap_or_default())
                .ok_or(PathCompletionError::Unavailable)?;
            result.matched_spans = spans
                .into_iter()
                .map(|span| span.start + prefix_len..span.end + prefix_len)
                .collect();
            Ok(result)
        })
        .collect()
}

pub fn is_current_candidate_kind(
    workspace_root: &Path,
    path: &str,
    expected: CandidateKind,
) -> bool {
    if !is_terminal_safe(path.as_bytes()) {
        return false;
    }
    let home = env::var_os("HOME");
    let Ok(resolved) =
        resolve_workspace_or_external_literal_path(workspace_root, path, home.as_deref())
    else {
        return false;
    };
    fs::metadata(resolved).is_ok_and(|metadata| match expected {
        CandidateKind::File => metadata.is_file(),
        CandidateKind::Directory => metadata.is_dir(),
    })
}

fn parse_explicit_query(query: &str) -> Option<ParsedQuery<'_>> {
    let (parent, basename_query) = if DIRECTORY_SHORTCUTS.contains(&query) {
        (query, "")
    } else {
        query.rsplit_once('/')?
    };
    let mut display_prefix = String::with_capacity(parent.len() + 1);
    display_prefix.push_str(parent);
    display_prefix.push('/');
    Some(ParsedQuery {
        parent: if parent.is_empty() { "/" } else { parent },
        display_prefix,
        basename_query,
    })
}

fn entry_kind(entry: &fs::DirEntry) -> Option<CandidateKind> {
    let file_type = entry.file_type().ok()?;
    let file_type = if file_type.is_symlink() {
        fs::metadata(entry.path()).ok()?.file_type()
    } else {
        file_type
    };
    if file_type.is_file() {
        Some(CandidateKind::File)
    } else if file_type.is_dir() {
        Some(CandidateKind::Directory)
    } else {
        None
    }
}

fn checkpoint(cancel: &AtomicBool) -> Result<(), PathCompletionError> {
    if cancel.load(Ordering::Acquire) {
        Err(PathCompletionError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn write(root: &Path, relative: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    fn pairs(spans: &[std::ops::Range<usize>]) -> Vec<(usize, usize)> {
        spans.iter().map(|span| (span.start, span.end)).collect()
    }

    fn paths(results: &[SearchResult]) -> Vec<&str> {
        results.iter().map(|result| result.path.as_str()).collect()
    }

    fn complete_in(root: &Path, query: &str) -> Vec<SearchResult> {
        complete(root, None, query, &AtomicBool::new(false), 32).unwrap()
    }

    #[test]
    fn only_path_shaped_queries_are_explicit() {
        for query in [
            "~",
            "~/Dow",
            "/tmp/fi",
            "./src/",
            "../shared/",
            "src/core/",
            ".",
            "..",
        ] {
            assert_eq!(query_mode(query), QueryMode::ExplicitPath, "{query}");
        }
        for query in [
            "",
            ".gitignore",
            "...",
            "..notes",
            "~notes",
            "main",
            "readme.md",
        ] {
            assert_eq!(query_mode(query), QueryMode::WorkspaceIndex, "{query}");
        }
        let parsed = parse_explicit_query("../shared/na").unwrap();
        assert_eq!(parsed.parent, "../shared");
        assert_eq!(parsed.display_prefix, "../shared/");
        assert_eq!(parsed.basename_query, "na");
        let root = parse_explicit_query("/et").unwrap();
        assert_eq!((root.parent, root.basename_query), ("/", "et"));
        for shortcut in DIRECTORY_SHORTCUTS {
            let bare = parse_explicit_query(shortcut).unwrap();
            let slashed = format!("{shortcut}/");
            let with_slash = parse_explicit_query(&slashed).unwrap();
            assert_eq!(bare.parent, with_slash.parent);
            assert_eq!(bare.display_prefix, with_slash.display_prefix);
            assert_eq!(bare.basename_query, "");
        }
    }

    #[test]
    fn bare_current_and_parent_directories_list_their_entries() {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        write(&base, "workspace/local.txt");
        write(&base, "parent.txt");
        let root = base.join("workspace");
        let local = complete_in(&root, ".");
        assert_eq!(paths(&local), ["./local.txt"]);
        assert!(local[0].matched_spans.is_empty());
        assert!(is_current_candidate_kind(
            &root,
            &local[0].path,
            CandidateKind::File
        ));
        assert_eq!(
            paths(&complete_in(&root, "..")),
            ["../parent.txt", "../workspace"]
        );
    }

    #[test]
    fn completion_ranks_fuzzy_name_matches_and_offsets_spans_past_the_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        write(&root, "src/main.rs");
        write(&root, "src/my_ain.rs");
        write(&root, "src/other.rs");
        fs::create_dir_all(root.join("src/mailbox")).unwrap();
        symlink(root.join("src/main.rs"), root.join("src/linked.rs")).unwrap();
        symlink(root.join("missing"), root.join("src/dangling")).unwrap();
        let results = complete_in(&root, "./src/ma");
        assert_eq!(
            paths(&results),
            ["./src/my_ain.rs", "./src/mailbox", "./src/main.rs"]
        );
        assert_eq!(pairs(&results[0].matched_spans), [(6, 7), (9, 10)]);
        assert_eq!(results[1].kind, CandidateKind::Directory);
        assert_eq!(pairs(&results[2].matched_spans), [(6, 8)]);
        let all = complete_in(&root, "src/");
        assert!(paths(&all).contains(&"src/linked.rs"));
        assert!(!paths(&all).contains(&"src/dangling"));
        assert!(!is_current_candidate_kind(
            &root,
            "src/mailbox",
            CandidateKind::File
        ));
        assert!(is_current_candidate_kind(
            &root,
            "src/mailbox",
            CandidateKind::Directory
        ));
    }

    #[test]
    fn missing_directories_are_unavailable_and_cancellation_stops_the_listing() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        assert_eq!(
            complete(&root, None, "missing/x", &AtomicBool::new(false), 32),
            Err(PathCompletionError::Unavailable)
        );
        assert_eq!(
            complete(&root, None, "./", &AtomicBool::new(true), 32),
            Err(PathCompletionError::Cancelled)
        );
        assert!(complete_in(&root, "main").is_empty());
    }

    #[test]
    fn home_relative_queries_list_the_home_directory() {
        let temp = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(temp.path()).unwrap();
        write(&home, "Downloads/report.pdf");
        let results = complete(
            Path::new("/nonexistent-workspace"),
            Some(home.as_os_str()),
            "~/Dow",
            &AtomicBool::new(false),
            32,
        )
        .unwrap();
        assert_eq!(paths(&results), ["~/Downloads"]);
        assert_eq!(pairs(&results[0].matched_spans), [(2, 5)]);
    }
}
