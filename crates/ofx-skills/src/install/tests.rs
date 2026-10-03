use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use super::*;

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let source = base.join("pack");
    let root = base.join("managed/skills");
    fs::create_dir_all(&source).unwrap();
    (temp, source, root)
}

fn skill(source: &Path, directory: &str, name: &str) {
    let path = source.join(directory);
    fs::create_dir_all(path.join("assets")).unwrap();
    fs::write(
        path.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: review\n---\n\nbody\n"),
    )
    .unwrap();
    fs::write(path.join("assets/data"), b"asset\0bytes").unwrap();
}

#[test]
fn local_install_copies_complete_assets_and_uses_metadata_names() {
    let (_temp, source, root) = fixture();
    skill(&source, "review", "review-name");
    let result = install_local(&root, &source, None).unwrap();
    assert_eq!(result.installed, ["review-name"]);
    assert_eq!(
        fs::read(root.join("review/assets/data")).unwrap(),
        b"asset\0bytes"
    );
    assert_eq!(
        fs::read(root.join("review/SKILL.md")).unwrap(),
        fs::read(source.join("review/SKILL.md")).unwrap()
    );
}

#[test]
fn root_first_filter_and_transactional_replacement_match_source() {
    let (_temp, source, root) = fixture();
    fs::write(source.join("SKILL.md"), "root body\n").unwrap();
    skill(&source, "nested", "selected");
    assert_eq!(
        install_local(&root, &source, Some("pack"))
            .unwrap()
            .installed,
        ["pack"]
    );
    assert_eq!(
        install_local(&root, &source, Some("selected"))
            .unwrap()
            .installed,
        ["selected"]
    );
    fs::write(root.join("nested/obsolete"), "old").unwrap();
    fs::write(source.join("nested/assets/data"), "new").unwrap();
    install_local(&root, &source, Some("nested")).unwrap();
    assert!(!root.join("nested/obsolete").exists());
    assert_eq!(fs::read(root.join("nested/assets/data")).unwrap(), b"new");
    assert!(
        install_local(&root, &source, Some("absent"))
            .unwrap()
            .installed
            .is_empty()
    );
}

#[test]
fn source_and_destination_links_never_redirect_install_writes() {
    let (temp, source, root) = fixture();
    skill(&source, "review", "review");
    let outside = temp.path().canonicalize().unwrap().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), "untouched").unwrap();
    fs::create_dir_all(&root).unwrap();
    symlink(&outside, root.join("review")).unwrap();
    assert!(install_local(&root, &source, None).is_err());
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"untouched");
    assert!(root.join("review").is_symlink());
}

#[test]
fn empty_sources_and_source_links_are_rejected_without_installing() {
    let (temp, source, root) = fixture();
    skill(&source, "review", "review");
    assert_eq!(
        path_directory(Path::new(""), false).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    let linked = temp.path().canonicalize().unwrap().join("linked");
    symlink(&source, &linked).unwrap();
    assert!(install_local(&root, &linked, None).is_err());
    assert!(!root.join("review").exists());
}

#[test]
fn malformed_metadata_and_nonregular_assets_are_skipped() {
    let (temp, source, root) = fixture();
    skill(&source, "valid", "valid");
    fs::create_dir(source.join("bad")).unwrap();
    fs::write(
        source.join("bad/SKILL.md"),
        "---\nname: bad\ndescription: first\n  continued: malformed\n---\n",
    )
    .unwrap();
    let outside = temp.path().canonicalize().unwrap().join("secret");
    fs::write(&outside, "private").unwrap();
    symlink(&outside, source.join("valid/linked")).unwrap();
    fs::write(source.join("valid/.gitignore"), "ignored").unwrap();
    assert_eq!(
        install_local(&root, &source, None).unwrap().installed,
        ["valid"]
    );
    assert!(!root.join("valid/linked").exists());
    assert!(!root.join("valid/.gitignore").exists());
    assert!(!root.join("bad").exists());
}

#[test]
fn managed_root_inside_source_is_rejected_before_discovery() {
    let (_temp, source, _root) = fixture();
    skill(&source, "valid", "valid");
    let nested = source.join("managed/skills");
    assert!(install_local(&nested, &source, None).is_err());
    assert!(!nested.exists());
    assert!(!source.join("managed").exists());
}

#[test]
fn empty_filter_is_unfiltered_and_hidden_parents_remain_skipped() {
    let (_temp, source, root) = fixture();
    skill(&source, "valid", "valid");
    skill(&source, ".hidden", "hidden");
    assert_eq!(
        install_local(&root, &source, Some("")).unwrap().installed,
        ["valid"]
    );
}

#[test]
fn hardlinked_destination_lock_leaves_never_authorize_replacement() {
    let (_temp, source, root) = fixture();
    skill(&source, "review", "review");
    fs::create_dir_all(root.parent().unwrap().join(".skill-install-locks")).unwrap();
    let locks = root.parent().unwrap().join(".skill-install-locks");
    fs::set_permissions(&locks, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(locks.join("original"), "private").unwrap();
    fs::set_permissions(locks.join("original"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(locks.join("original"), locks.join("review")).unwrap();
    assert!(install_local(&root, &source, None).is_err());
    assert!(!root.join("review").exists());
    assert_eq!(fs::read(locks.join("original")).unwrap(), b"private");
}

#[test]
fn executable_and_private_assets_retain_source_permissions() {
    let (_temp, source, root) = fixture();
    skill(&source, "review", "review");
    for (name, mode) in [("script", 0o755), ("private", 0o600)] {
        let path = source.join("review/assets").join(name);
        fs::write(&path, "asset").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    }
    install_local(&root, &source, None).unwrap();
    for (name, mode) in [("script", 0o755), ("private", 0o600)] {
        assert_eq!(
            fs::metadata(root.join("review/assets").join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            mode
        );
    }
}

#[test]
fn copied_assets_never_retain_setuid_or_setgid_bits() {
    let (_temp, source, root) = fixture();
    skill(&source, "review", "review");
    let path = source.join("review/assets/data");
    fs::set_permissions(path, fs::Permissions::from_mode(0o6755)).unwrap();
    install_local(&root, &source, None).unwrap();
    assert_eq!(
        fs::metadata(root.join("review/assets/data"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o755
    );
}

#[test]
fn candidate_order_follows_skill_file_entries_in_native_walk_order() {
    let (_temp, source, root) = fixture();
    skill(&source, "parent/0child", "child");
    fs::write(
        source.join("parent/SKILL.md"),
        "---\nname: parent\ndescription: parent\n---\n",
    )
    .unwrap();
    let parent = path_directory(&source.join("parent"), false).unwrap();
    let expected: Vec<_> = entries(&parent)
        .unwrap()
        .into_iter()
        .filter_map(|(name, _)| {
            if name == "0child" {
                Some("child")
            } else if name == "SKILL.md" {
                Some("parent")
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        install_local(&root, &source, None).unwrap().installed,
        expected
    );
}
