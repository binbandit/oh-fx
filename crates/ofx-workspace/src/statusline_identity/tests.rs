use std::fs;
use std::path::Path;

use ofx_contract::WorkspaceIdentitySource;

use super::*;

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn temp_root() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(directory.path()).unwrap();
    (directory, root)
}

fn rewrite_head(path: &Path, content: &str) {
    let replacement = path.with_extension("next");
    fs::write(&replacement, content).unwrap();
    fs::rename(replacement, path).unwrap();
}

#[test]
fn the_identity_refreshes_the_branch_and_a_detached_head() {
    let (_directory, root) = temp_root();
    write(&root, "workspace/.git/HEAD", "ref: refs/heads/main\n");
    let workspace = root.join("workspace");
    let mut identity = StatuslineIdentity::new(&workspace);
    let snapshot = identity.refresh();
    assert_eq!(snapshot.label, workspace.to_str().unwrap());
    assert_eq!(snapshot.branch.as_deref(), Some("main"));
    let head = workspace.join(".git/HEAD");
    rewrite_head(&head, "ref: refs/heads/refreshed-branch\n");
    assert_eq!(
        identity.refresh().branch.as_deref(),
        Some("refreshed-branch")
    );
    rewrite_head(&head, "0123456789abcdef0123456789abcdef01234567\n");
    assert_eq!(
        identity.refresh().branch.as_deref(),
        Some("detached:0123456789ab")
    );
}

#[test]
fn an_unchanged_head_is_not_read_again() {
    let (_directory, root) = temp_root();
    write(&root, "workspace/.git/HEAD", "ref: refs/heads/main\n");
    let mut identity = StatuslineIdentity::new(root.join("workspace"));
    assert_eq!(identity.refresh().branch.as_deref(), Some("main"));
    let signature = identity.head_signature;
    assert!(signature.is_some());
    assert_eq!(identity.refresh().branch.as_deref(), Some("main"));
    assert_eq!(identity.head_signature, signature);
}

#[test]
fn the_identity_resolves_worktree_gitdir_files() {
    let (_directory, root) = temp_root();
    write(
        &root,
        "workspace/.git",
        "gitdir: ../git-data/worktrees/active\n",
    );
    write(
        &root,
        "git-data/worktrees/active/HEAD",
        "ref: refs/heads/worktree-branch\n",
    );
    let snapshot = StatuslineIdentity::new(root.join("workspace")).refresh();
    assert_eq!(snapshot.branch.as_deref(), Some("worktree-branch"));
}

#[test]
fn the_identity_finds_a_repository_above_the_working_directory() {
    let (_directory, root) = temp_root();
    write(&root, "repo/.git/HEAD", "ref: refs/heads/parent-repo\n");
    let workspace = root.join("repo/packages/app");
    fs::create_dir_all(&workspace).unwrap();
    let snapshot = StatuslineIdentity::new(&workspace).refresh();
    assert_eq!(snapshot.label, workspace.to_str().unwrap());
    assert_eq!(snapshot.branch.as_deref(), Some("parent-repo"));
}

#[test]
fn the_identity_handles_non_git_and_hostile_paths() {
    let (_directory, root) = temp_root();
    let workspace = root.join("unsafe-\x1b[31m-workspace");
    fs::create_dir_all(&workspace).unwrap();
    write(&workspace, ".git", "not a Git directory\n");
    let snapshot = StatuslineIdentity::new(&workspace).refresh();
    assert_eq!(snapshot.branch, None);
    assert!(!snapshot.label.contains('\x1b'));
    assert!(snapshot.label.contains("\\x1b"));
}

#[test]
fn the_identity_rejects_a_malformed_detached_head_and_escapes_branch_names() {
    let (_directory, root) = temp_root();
    write(&root, "workspace/.git/HEAD", "not-a-commit\x1b[31m\n");
    let workspace = root.join("workspace");
    assert_eq!(StatuslineIdentity::new(&workspace).refresh().branch, None);
    rewrite_head(
        &workspace.join(".git/HEAD"),
        "ref: refs/heads/red\x1b[31m\n",
    );
    assert_eq!(
        StatuslineIdentity::new(&workspace)
            .refresh()
            .branch
            .as_deref(),
        Some("red\\x1b[31m")
    );
    rewrite_head(&workspace.join(".git/HEAD"), "abc12\n");
    assert_eq!(StatuslineIdentity::new(&workspace).refresh().branch, None);
}

#[test]
fn the_identity_represents_the_filesystem_root() {
    assert_eq!(StatuslineIdentity::new("/").refresh().label, "/");
    assert_eq!(
        StatuslineIdentity::new("").refresh(),
        WorkspaceIdentity::default()
    );
}

#[test]
fn heads_parse_branches_and_detached_commits() {
    for (head, parsed) in [
        (
            &b"ref: refs/heads/feature/x\n"[..],
            Some(ParsedHead::Branch(b"feature/x")),
        ),
        (
            b" ref: refs/remotes/origin/main ",
            Some(ParsedHead::Branch(b"refs/remotes/origin/main")),
        ),
        (b"ref: refs/heads/", None),
        (b"ref:", None),
        (b"", None),
        (b"0123456", Some(ParsedHead::Detached(b"0123456"))),
        (b"012345", None),
        (b"0123456z", None),
    ] {
        assert_eq!(parse_head(head), parsed, "{head:?}");
    }
}
