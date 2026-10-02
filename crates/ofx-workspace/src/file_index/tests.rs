use std::fs;
use std::os::unix::fs::symlink;
use std::time::{Duration, Instant};

use super::matcher::MatchScore;
use super::*;
use crate::workspace_files::tests::run_git;

const WAIT: Duration = Duration::from_secs(20);

fn file(path: &str) -> Candidate {
    Candidate {
        path: path.to_owned(),
        kind: CandidateKind::File,
    }
}

fn directory(path: &str) -> Candidate {
    Candidate {
        path: path.to_owned(),
        kind: CandidateKind::Directory,
    }
}

fn ready(candidates: &[Candidate]) -> FileIndex {
    let mut index = FileIndex::new(None);
    index.active = Generation::build(1, 0, candidates, None);
    index.generation = 1;
    index
}

fn search(index: &FileIndex, query: &str) -> Vec<SearchResult> {
    index
        .search_at_revision(index.readable_revision(), query, 32)
        .unwrap()
}

fn pairs(spans: &[Range<usize>]) -> Vec<(usize, usize)> {
    spans.iter().map(|span| (span.start, span.end)).collect()
}

fn paths(results: &[SearchResult]) -> Vec<&str> {
    results.iter().map(|result| result.path.as_str()).collect()
}

fn first(candidates: &[&str], query: &str) -> String {
    let candidates: Vec<Candidate> = candidates.iter().map(|path| file(path)).collect();
    search(&ready(&candidates), query)
        .first()
        .map(|result| result.path.clone())
        .unwrap_or_default()
}

fn wait_until_settled(index: &mut FileIndex) {
    let deadline = Instant::now() + WAIT;
    while index.is_loading() {
        index.join_if_done();
        assert!(Instant::now() < deadline, "the index never settled");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn best_matches_come_first() {
    assert_eq!(
        first(
            &[
                "src/wasm_term_main.zig",
                "src/main.zig",
                "README.md",
                "src/core/shared/io.zig",
                "benchmarks/startup.sh",
            ],
            "main"
        ),
        "src/main.zig"
    );
    let index = ready(&[file("src/wasm_term_main.zig"), file("src/main.zig")]);
    assert_eq!(
        paths(&search(&index, "main")),
        ["src/main.zig", "src/wasm_term_main.zig"]
    );
    assert_eq!(
        first(&["src/main.zig", "foo/bar/baz-main.zig"], "main"),
        "src/main.zig"
    );
    assert_eq!(first(&["src/main.zig"], "xyz"), "");
    assert_eq!(first(&["src/main.zig"], "src/main"), "src/main.zig");
}

#[test]
fn ranking_signals_follow_their_descending_influence() {
    for (candidates, query, expected) in [
        (
            &["other/src/core/main.zig", "src/core/main.zig"][..],
            "src/core/main.zig",
            "src/core/main.zig",
        ),
        (
            &["src/main.zig.backup", "src/main.zig"],
            "main.zig",
            "src/main.zig",
        ),
        (
            &["core/guide.txt", "docs/core-guide.txt"],
            "core",
            "docs/core-guide.txt",
        ),
        (
            &["misc/scattered/memo.zig", "src/core/main.zig"],
            "scm",
            "src/core/main.zig",
        ),
        (
            &["src/xwidget.zig", "src/widgetx.zig"],
            "wid",
            "src/widgetx.zig",
        ),
        (&["x/a/b", "a/x/b"], "ab", "a/x/b"),
        (
            &["src/xxidxx.zig", "src/xidxxx.zig"],
            "id",
            "src/xidxxx.zig",
        ),
        (&["src/axbyc.zig", "src/abczz.zig"], "abc", "src/abczz.zig"),
        (
            &["src/axxxbxc.zig", "src/axbxczz.zig"],
            "abc",
            "src/axbxczz.zig",
        ),
        (
            &["src/abc-long-name.txt", "src/abc.txt"],
            "abc",
            "src/abc.txt",
        ),
        (&["src/zeta.txt", "src/beta.txt"], "ta", "src/beta.txt"),
        (&["src/beta.txt", "src/zeta.txt"], "ta", "src/beta.txt"),
    ] {
        assert_eq!(first(candidates, query), expected, "{query}");
    }
}

#[test]
fn abbreviations_and_punctuation_match_as_case_insensitive_subsequences() {
    let index = ready(&[
        directory("src/core/workspace"),
        file("src/core/workspace/file_index.zig"),
        file("src/core/workspace/workspace_files.zig"),
    ]);
    let results = search(&index, "FIIDX");
    assert_eq!(paths(&results), ["src/core/workspace/file_index.zig"]);
    let matched: String = results[0]
        .matched_spans
        .iter()
        .map(|span| &results[0].path[span.clone()])
        .collect();
    assert_eq!(matched, "fiidx");
    let punctuation = ready(&[file("src/API-route.ts"), file("src/api_handler.ts")]);
    assert_eq!(paths(&search(&punctuation, "api-rt")), ["src/API-route.ts"]);
    let repeated = ready(&[file("src/axaya.zig"), file("src/alpha.zig")]);
    assert_eq!(paths(&search(&repeated, "aaa")), ["src/axaya.zig"]);
    let cased = ready(&[file("README.md"), file("src/Main.zig")]);
    assert_eq!(paths(&search(&cased, "readme"))[0], "README.md");
    assert_eq!(paths(&search(&cased, "main"))[0], "src/Main.zig");
}

#[test]
fn name_queries_rank_and_highlight_exactly_as_the_index_does() {
    let names = [
        "Desktop",
        "desktop-tools",
        "myDesktop",
        "Desk top",
        "dusk-top",
        "\u{c4}rger-file.txt",
        "\u{212a}elvin",
        "cafe\u{301}.txt",
    ];
    let index = ready(&names.map(file));
    for query in ["ktop", "dsktp", "desk", "\u{e4}rf", "klv", "ce", "nomatch"] {
        let matcher = NameQuery::new(query).unwrap();
        let mut ranked: Vec<(&str, MatchScore)> = names
            .iter()
            .filter_map(|name| matcher.score(name).map(|score| (*name, score)))
            .collect();
        ranked.sort_by(|left, right| {
            if matcher.better(&left.1, left.0, &right.1, right.0) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        });
        let results = search(&index, query);
        assert_eq!(results.len(), ranked.len(), "{query}");
        for ((name, _), result) in ranked.iter().zip(&results) {
            assert_eq!(result.path, *name, "{query}");
            assert_eq!(
                Some(result.matched_spans.clone()),
                matcher.match_spans(name)
            );
        }
    }
}

#[test]
fn files_and_directories_rank_without_kind_priority() {
    let index = ready(&[
        file("docs-guide.md"),
        directory("docs"),
        file("src/docs.rs"),
    ]);
    let results = search(&index, "docs");
    assert_eq!(paths(&results), ["docs", "src/docs.rs", "docs-guide.md"]);
    assert_eq!(results[0].kind, CandidateKind::Directory);
    assert_eq!(pairs(&results[0].matched_spans), [(0, 4)]);
    assert_eq!(pairs(&results[1].matched_spans), [(4, 8)]);
}

#[test]
fn search_is_case_insensitive_and_keeps_the_raw_spelling() {
    let index = ready(&[file("Src/Main.ZIG"), file("lib/other.zig")]);
    let results = search(&index, "MAIN");
    assert_eq!(paths(&results), ["Src/Main.ZIG"]);
    assert_eq!(pairs(&results[0].matched_spans), [(4, 8)]);
}

#[test]
fn unicode_search_uses_simple_folding_and_keeps_the_raw_spelling() {
    let index = ready(&[
        file("docs/\u{c4}rger-file.txt"),
        file("docs/\u{3a3}igma.txt"),
        file("docs/\u{3c2}igma-final.txt"),
        file("docs/\u{212a}elvin.txt"),
        file("docs/kettle.txt"),
    ]);
    assert_eq!(
        paths(&search(&index, "\u{e4}rger")),
        ["docs/\u{c4}rger-file.txt"]
    );
    let sigma = search(&index, "\u{3c3}igma");
    assert_eq!(sigma.len(), 2);
    assert_eq!(sigma[0].path, "docs/\u{3a3}igma.txt");
    assert_eq!(search(&index, "kelvin")[0].path, "docs/\u{212a}elvin.txt");
    assert!(paths(&search(&index, "k")).contains(&"docs/\u{212a}elvin.txt"));
    assert!(paths(&search(&index, "\u{212a}")).contains(&"docs/kettle.txt"));
    let unnormalized = ready(&[file("\u{c4}rger.txt"), file("Fu\u{df}.txt")]);
    assert!(search(&unnormalized, "A\u{308}rger").is_empty());
    assert!(search(&unnormalized, "Fuss").is_empty());
}

#[test]
fn spans_keep_combining_sequences_whole() {
    let index = ready(&[file("docs/Cafe\u{301}-note.txt")]);
    let results = search(&index, "e\u{301}n");
    assert_eq!(results.len(), 1);
    let first_span = results[0].matched_spans[0].clone();
    assert_eq!(&results[0].path[first_span], "e\u{301}");
    let combining = search(&index, "\u{301}");
    assert_eq!(combining[0].matched_spans.len(), 1);
    let span = combining[0].matched_spans[0].clone();
    assert_eq!(&combining[0].path[span], "e\u{301}");
}

#[test]
fn an_empty_query_lists_the_index_in_order_without_spans() {
    let index = ready(&[file("b.txt"), directory("a"), file("a/c.txt")]);
    let results = search(&index, "");
    assert_eq!(paths(&results), ["b.txt", "a", "a/c.txt"]);
    assert!(results.iter().all(|result| result.matched_spans.is_empty()));
}

#[test]
fn results_are_capped_and_overlong_queries_match_nothing() {
    let candidates: Vec<Candidate> = (0..200)
        .map(|index| file(&format!("file-{index:03}.txt")))
        .collect();
    let index = ready(&candidates);
    let revision = index.readable_revision();
    assert_eq!(
        index
            .search_at_revision(revision, "file", 500)
            .unwrap()
            .len(),
        MAX_SEARCH_RESULTS
    );
    assert_eq!(
        index.search_at_revision(revision, "", 500).unwrap().len(),
        200
    );
    let long = "f".repeat(MAX_PATH_LEN + 1);
    assert!(
        index
            .search_at_revision(revision, &long, 32)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_revision_from_another_generation_is_rejected() {
    let mut index = ready(&[file("a.txt")]);
    let old = index.readable_revision();
    assert_eq!(old.state, IndexState::Ready);
    assert_eq!(old.count, 1);
    index.active = Generation::build(2, 0, &[file("a.txt")], None);
    assert_eq!(
        index.search_at_revision(old, "a", 32),
        Err(InvalidIndexData)
    );
    let idle = FileIndex::new(None);
    assert_eq!(idle.current_state(), IndexState::Idle);
    assert!(
        idle.search_at_revision(idle.readable_revision(), "a", 32)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn unsafe_overlong_and_slash_terminated_directories_are_left_out() {
    let index = ready(&[
        file("ok.txt"),
        file("bad\u{1b}[2J.txt"),
        file("line\nbreak.txt"),
        file(&"x".repeat(MAX_PATH_LEN + 1)),
        directory("dir/"),
        file(""),
    ]);
    assert_eq!(paths(&search(&index, "")), ["ok.txt"]);
}

fn workspace() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    (temp, root)
}

fn write(root: &Path, relative: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "x").unwrap();
}

fn discovered(root: &Path) -> Vec<(String, CandidateKind)> {
    discovery::discover_scope(root, &AtomicBool::new(false))
        .unwrap()
        .into_iter()
        .map(|candidate| (candidate.path, candidate.kind))
        .collect()
}

#[test]
fn git_workspaces_list_tracked_untracked_and_hidden_files_with_their_directories() {
    let (_temp, root) = workspace();
    assert!(run_git(&root, &["init", "-q"]));
    write(&root, ".gitignore");
    fs::write(root.join(".gitignore"), "ignored/\n*.log\n").unwrap();
    write(&root, "src/main.rs");
    write(&root, ".config/settings.json");
    write(&root, "ignored/skip.txt");
    write(&root, "debug.log");
    write(&root, "node_modules/pkg/index.js");
    assert!(run_git(&root, &["add", "src/main.rs"]));
    let listed = discovered(&root);
    let names: Vec<&str> = listed.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(
        names,
        [
            ".config",
            ".config/settings.json",
            ".gitignore",
            "node_modules",
            "node_modules/pkg",
            "node_modules/pkg/index.js",
            "src",
            "src/main.rs",
        ]
    );
    let kinds: Vec<CandidateKind> = listed.iter().map(|(_, kind)| *kind).collect();
    assert_eq!(kinds[0], CandidateKind::Directory);
    assert_eq!(kinds[1], CandidateKind::File);
}

#[test]
fn plain_directories_are_walked_without_git_metadata_or_ignored_names() {
    let (_temp, root) = workspace();
    write(&root, "README.md");
    write(&root, "node_modules/pkg/index.js");
    write(&root, ".hidden/notes.txt");
    write(&root, "src/lib.rs");
    symlink(root.join("README.md"), root.join("linked.md")).unwrap();
    let names: Vec<String> = discovered(&root)
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert_eq!(
        names,
        [
            ".hidden",
            ".hidden/notes.txt",
            "README.md",
            "linked.md",
            "src",
            "src/lib.rs"
        ]
    );
    assert_eq!(
        discovery::discover_scope(&root.join("missing"), &AtomicBool::new(false)),
        Err(discovery::DiscoveryError::Failed)
    );
    assert_eq!(
        discovery::discover_scope(&root, &AtomicBool::new(true)),
        Err(discovery::DiscoveryError::Canceled)
    );
}

#[test]
fn the_index_loads_in_the_background_and_refreshes_from_disk() {
    let (_temp, root) = workspace();
    write(&root, "first.txt");
    let mut index = FileIndex::new(None);
    index.ensure_scope(&root);
    assert_eq!(index.current_state(), IndexState::Loading);
    wait_until_settled(&mut index);
    assert_eq!(index.current_state(), IndexState::Ready);
    assert_eq!(paths(&search(&index, "")), ["first.txt"]);
    write(&root, "second.txt");
    index.refresh();
    index.refresh();
    wait_until_settled(&mut index);
    assert_eq!(paths(&search(&index, "")), ["first.txt", "second.txt"]);
    assert!(index.is_current_candidate_kind("first.txt", CandidateKind::File));
    assert!(!index.is_current_candidate_kind("first.txt", CandidateKind::Directory));
    assert!(!index.is_current_candidate_kind("gone.txt", CandidateKind::File));
    assert!(!index.is_current_candidate_kind("/elsewhere/first.txt", CandidateKind::File));
}

#[test]
fn a_cached_index_paints_first_and_a_real_scan_replaces_it() {
    let (_temp, root) = workspace();
    let cache = tempfile::tempdir().unwrap();
    write(&root, "real.txt");
    file_index_cache::save(cache.path(), &[root.as_path()], &[file("cached.txt")]).unwrap();
    let mut index = FileIndex::new(Some(cache.path().to_owned()));
    index.ensure_scope(&root);
    let deadline = Instant::now() + WAIT;
    while !index.join_if_done() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(paths(&search(&index, "")), ["cached.txt"]);
    assert!(index.is_loading());
    wait_until_settled(&mut index);
    assert_eq!(paths(&search(&index, "")), ["real.txt"]);
    assert_eq!(
        file_index_cache::load(cache.path(), &[root.as_path()]),
        Some(vec![file("real.txt")])
    );
}

#[test]
fn a_missing_root_fails_the_first_load() {
    let (_temp, root) = workspace();
    let mut index = FileIndex::new(None);
    index.ensure_scope(&root.join("missing"));
    wait_until_settled(&mut index);
    assert_eq!(index.current_state(), IndexState::Failed);
    assert_eq!(index.readable_revision().state, IndexState::Failed);
}
