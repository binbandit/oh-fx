use std::env;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, openat, readlinkat, statat};
use rustix::io::Errno;

use super::{
    ExternalPathInput, MAX_PATH_BYTES, PATH_ENTRY_WHITESPACE, SEPARATOR,
    classify_external_path_input, inside, is_absolute,
};
use crate::path_error::PathError;
use crate::regular_file::DIRECTORY_FLAGS;

pub(crate) const MAX_FILE_TARGET_COMPONENTS: usize = 256;
const MAX_SYMBOLIC_LINK_EXPANSIONS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetMode {
    Existing,
    Create,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMutationTarget {
    pub anchor: PathBuf,
    pub anchor_is_external: bool,
    pub components: Vec<OsString>,
}

impl FileMutationTarget {
    pub fn path(&self) -> PathBuf {
        let mut path = self.anchor.clone();
        path.extend(&self.components);
        path
    }
}

pub fn resolve_file_mutation_target(
    workspace_root: &Path,
    input_path: &str,
    mode: TargetMode,
) -> Result<FileMutationTarget, PathError> {
    let home = env::var_os("HOME");
    resolve_with_home(workspace_root, input_path, mode, home.as_deref())
}

fn resolve_with_home(
    workspace_root: &Path,
    input_path: &str,
    mode: TargetMode,
    home: Option<&OsStr>,
) -> Result<FileMutationTarget, PathError> {
    let root = workspace_root.as_os_str().as_bytes();
    let cleaned = input_path.trim_matches(PATH_ENTRY_WHITESPACE).as_bytes();
    let (mut pending, external_intent) = match classify_external_path_input(cleaned)? {
        ExternalPathInput::Absolute(absolute) => (normalize(&[absolute])?, true),
        ExternalPathInput::HomeRelative(relative) => {
            let home = home.ok_or(PathError::HomeNotSet)?.as_bytes();
            if home.is_empty() || !is_absolute(home) {
                return Err(PathError::InvalidPath);
            }
            (normalize(&[home, relative])?, true)
        }
        ExternalPathInput::WorkspaceRelative(relative) => {
            if root.is_empty() || !is_absolute(root) {
                return Err(PathError::WorkspaceUnavailable);
            }
            let absolute = normalize(&[root, relative])?;
            let external_intent = !inside(root, &absolute);
            (absolute, external_intent)
        }
    };
    let mut expansions = 0;
    loop {
        let step = traverse(root, &pending, mode, &mut expansions)?;
        let reached = match &step {
            Step::Restart(path) => path,
            Step::Complete(walk) => &walk.path,
        };
        if !external_intent && !inside(root, reached) {
            return Err(PathError::PathOutsideWorkspace);
        }
        match step {
            Step::Restart(path) => pending = path,
            Step::Complete(walk) => return Ok(walk.into_target(root)),
        }
    }
}

enum Step {
    Restart(Vec<u8>),
    Complete(Walk),
}

struct Walk {
    path: Vec<u8>,
    anchor_end: usize,
    components: Vec<OsString>,
}

impl Walk {
    fn into_target(mut self, root: &[u8]) -> FileMutationTarget {
        self.path.truncate(self.anchor_end);
        FileMutationTarget {
            anchor_is_external: !inside(root, &self.path),
            anchor: PathBuf::from(OsString::from_vec(self.path)),
            components: self.components,
        }
    }
}

enum Child {
    Directory(OwnedFd),
    Missing,
    Symlink,
}

fn traverse(
    root: &[u8],
    absolute: &[u8],
    mode: TargetMode,
    expansions: &mut usize,
) -> Result<Step, PathError> {
    let names: Vec<&[u8]> = absolute
        .split(|byte| *byte == SEPARATOR)
        .filter(|name| !name.is_empty())
        .collect();
    let workspace_target = inside(root, absolute);
    let mut workspace_anchor_end = (workspace_target && root == b"/").then_some(1);
    let mut walk = Walk {
        path: vec![SEPARATOR],
        anchor_end: 1,
        components: Vec::new(),
    };
    let Some(last) = names.len().checked_sub(1) else {
        return Ok(Step::Complete(walk));
    };
    let mut current = rustix::fs::open("/", DIRECTORY_FLAGS, Mode::empty()).map_err(path_error)?;
    for (index, name) in names.iter().enumerate() {
        let parent_end = walk.path.len();
        append(&mut walk.path, name)?;
        let anchor_end = |workspace_anchor_end: Option<usize>| {
            if workspace_target {
                workspace_anchor_end.ok_or(PathError::PathOutsideWorkspace)
            } else {
                Ok(parent_end)
            }
        };
        if index == last {
            match statat(&current, *name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(_) if workspace_target && walk.path == root => {
                    walk.anchor_end = walk.path.len();
                    return Ok(Step::Complete(walk));
                }
                Ok(_) => {}
                Err(Errno::NOENT) if mode == TargetMode::Create => {}
                Err(errno) => return Err(path_error(errno)),
            }
            push_component(&mut walk.components, name)?;
            walk.anchor_end = anchor_end(workspace_anchor_end)?;
            return Ok(Step::Complete(walk));
        }
        match open_child(&current, name)? {
            Child::Directory(directory) => {
                current = directory;
                if !workspace_target {
                    continue;
                }
                match workspace_anchor_end {
                    Some(_) => push_component(&mut walk.components, name)?,
                    None if walk.path == root => workspace_anchor_end = Some(walk.path.len()),
                    None => {}
                }
            }
            Child::Missing if mode == TargetMode::Create => {
                push_component(&mut walk.components, name)?;
                for missing in &names[index + 1..] {
                    append(&mut walk.path, missing)?;
                    push_component(&mut walk.components, missing)?;
                }
                walk.anchor_end = anchor_end(workspace_anchor_end)?;
                return Ok(Step::Complete(walk));
            }
            Child::Missing => return Err(PathError::FileNotFound),
            Child::Symlink => {
                let parent = &walk.path[..parent_end];
                let suffix = names[index + 1..].join(&SEPARATOR);
                return expand_symlink(&current, name, parent, &suffix, expansions)
                    .map(Step::Restart);
            }
        }
    }
    Err(PathError::InvalidPath)
}

fn open_child(directory: &OwnedFd, name: &[u8]) -> Result<Child, PathError> {
    match openat(directory, name, DIRECTORY_FLAGS, Mode::empty()) {
        Ok(child) => Ok(Child::Directory(child)),
        Err(Errno::NOENT) => Ok(Child::Missing),
        Err(errno @ (Errno::NOTDIR | Errno::LOOP)) => {
            let entry = statat(directory, name, AtFlags::SYMLINK_NOFOLLOW).map_err(path_error)?;
            if FileType::from_raw_mode(entry.st_mode) == FileType::Symlink {
                Ok(Child::Symlink)
            } else {
                Err(path_error(errno))
            }
        }
        Err(errno) => Err(path_error(errno)),
    }
}

fn expand_symlink(
    directory: &OwnedFd,
    name: &[u8],
    parent: &[u8],
    suffix: &[u8],
    expansions: &mut usize,
) -> Result<Vec<u8>, PathError> {
    if *expansions >= MAX_SYMBOLIC_LINK_EXPANSIONS {
        return Err(PathError::SymLinkLoop);
    }
    *expansions += 1;
    let link = readlinkat(directory, name, Vec::new())
        .map_err(path_error)?
        .into_bytes();
    if link.is_empty() {
        return Err(PathError::InvalidPath);
    }
    let base: &[u8] = if is_absolute(&link) { b"/" } else { parent };
    normalize(&[base, &link, suffix])
}

fn normalize(parts: &[&[u8]]) -> Result<Vec<u8>, PathError> {
    let mut path = vec![SEPARATOR];
    for part in parts {
        for component in part.split(|byte| *byte == SEPARATOR) {
            match component {
                b"" | b"." => {}
                b".." => pop(&mut path),
                name if name.contains(&0) => return Err(PathError::InvalidPath),
                name => append(&mut path, name)?,
            }
        }
    }
    Ok(path)
}

fn pop(path: &mut Vec<u8>) {
    let end = path
        .iter()
        .rposition(|byte| *byte == SEPARATOR)
        .unwrap_or_default();
    path.truncate(end.max(1));
}

fn append(path: &mut Vec<u8>, name: &[u8]) -> Result<(), PathError> {
    let separator = usize::from(path.len() > 1);
    if path.len() + separator + name.len() > MAX_PATH_BYTES {
        return Err(PathError::InvalidPath);
    }
    if separator == 1 {
        path.push(SEPARATOR);
    }
    path.extend_from_slice(name);
    Ok(())
}

fn push_component(components: &mut Vec<OsString>, name: &[u8]) -> Result<(), PathError> {
    if components.len() >= MAX_FILE_TARGET_COMPONENTS {
        return Err(PathError::TooManyPathComponents);
    }
    components.push(OsStr::from_bytes(name).to_owned());
    Ok(())
}

fn path_error(errno: Errno) -> PathError {
    io::Error::from(errno).into()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::*;

    struct Fixture {
        _temp: TempDir,
        root: PathBuf,
        workspace: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let root = fs::canonicalize(temp.path()).unwrap();
            let workspace = root.join("workspace");
            fs::create_dir(&workspace).unwrap();
            Self {
                _temp: temp,
                root,
                workspace,
            }
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "content").unwrap();
            path
        }

        fn link(&self, target: impl AsRef<Path>, relative: &str) {
            symlink(target, self.root.join(relative)).unwrap();
        }

        fn resolve(&self, input: &str, mode: TargetMode) -> Result<FileMutationTarget, PathError> {
            resolve_with_home(&self.workspace, input, mode, Some(self.root.as_os_str()))
        }
    }

    fn components(target: &FileMutationTarget) -> Vec<&str> {
        target
            .components
            .iter()
            .map(|name| name.to_str().unwrap())
            .collect()
    }

    #[test]
    fn workspace_targets_are_anchored_at_the_workspace_root() {
        let fixture = Fixture::new();
        fixture.file("workspace/src/file.txt");

        let target = fixture
            .resolve(" src/file.txt ", TargetMode::Existing)
            .unwrap();

        assert_eq!(target.anchor, fixture.workspace);
        assert!(!target.anchor_is_external);
        assert_eq!(components(&target), ["src", "file.txt"]);
        assert_eq!(target.path(), fixture.workspace.join("src/file.txt"));
    }

    #[test]
    fn explicit_external_targets_are_anchored_at_their_parent() {
        let fixture = Fixture::new();
        let external = fixture.file("external/file.txt");

        let target = fixture
            .resolve(external.to_str().unwrap(), TargetMode::Existing)
            .unwrap();

        assert_eq!(target.anchor, fixture.root.join("external"));
        assert!(target.anchor_is_external);
        assert_eq!(components(&target), ["file.txt"]);

        let home = fixture
            .resolve("~/external/new.txt", TargetMode::Create)
            .unwrap();
        assert_eq!(home.path(), fixture.root.join("external/new.txt"));
        assert!(home.anchor_is_external);

        let escape = fixture
            .resolve("../external/file.txt", TargetMode::Existing)
            .unwrap();
        assert_eq!(escape.path(), external);
        assert!(escape.anchor_is_external);
    }

    #[test]
    fn missing_targets_resolve_from_the_nearest_existing_parent_without_creating_anything() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.root.join("external")).unwrap();
        let external = fixture.root.join("external/missing/parents/new.txt");

        let target = fixture
            .resolve(external.to_str().unwrap(), TargetMode::Create)
            .unwrap();
        assert_eq!(target.anchor, fixture.root.join("external"));
        assert_eq!(components(&target), ["missing", "parents", "new.txt"]);

        let inside = fixture
            .resolve("missing/parents/new.txt", TargetMode::Create)
            .unwrap();
        assert_eq!(inside.anchor, fixture.workspace);
        assert_eq!(components(&inside), ["missing", "parents", "new.txt"]);

        assert!(!fixture.root.join("external/missing").exists());
        assert!(!fixture.workspace.join("missing").exists());
        assert_eq!(
            fixture.resolve("missing/new.txt", TargetMode::Existing),
            Err(PathError::FileNotFound)
        );
        assert_eq!(
            fixture.resolve("new.txt", TargetMode::Existing),
            Err(PathError::FileNotFound)
        );
    }

    #[test]
    fn contained_intermediate_symlinks_resolve_and_final_symlinks_stay_entries() {
        let fixture = Fixture::new();
        fixture.file("workspace/real/target.txt");
        let external = fixture.file("external/target.txt");
        fixture.link("real", "workspace/contained-dir-link");
        fixture.link("real/missing-parent", "workspace/missing-dir-link");
        fixture.link(external.parent().unwrap(), "workspace/external-dir-link");
        fixture.link(&external, "workspace/final-link.txt");

        let contained = fixture
            .resolve("contained-dir-link/target.txt", TargetMode::Existing)
            .unwrap();
        assert_eq!(contained.anchor, fixture.workspace);
        assert_eq!(components(&contained), ["real", "target.txt"]);

        let missing = fixture
            .resolve("missing-dir-link/target.txt", TargetMode::Create)
            .unwrap();
        assert_eq!(
            components(&missing),
            ["real", "missing-parent", "target.txt"]
        );
        assert!(!fixture.workspace.join("real/missing-parent").exists());

        assert_eq!(
            fixture.resolve("external-dir-link/target.txt", TargetMode::Existing),
            Err(PathError::PathOutsideWorkspace)
        );
        assert_eq!(
            fixture.resolve("external-dir-link/new.txt", TargetMode::Create),
            Err(PathError::PathOutsideWorkspace)
        );

        let final_link = fixture
            .resolve("final-link.txt", TargetMode::Existing)
            .unwrap();
        assert_eq!(final_link.path(), fixture.workspace.join("final-link.txt"));
        assert!(!final_link.anchor_is_external);
    }

    #[test]
    fn absolute_paths_through_an_escaping_symlink_become_external_targets() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.root.join("outside")).unwrap();
        fixture.link("../outside", "workspace/link");
        let misleading = fixture.workspace.join("link/new.txt");

        let target = fixture
            .resolve(misleading.to_str().unwrap(), TargetMode::Create)
            .unwrap();

        assert!(target.anchor_is_external);
        assert_eq!(target.path(), fixture.root.join("outside/new.txt"));
    }

    #[test]
    fn parent_references_are_normalized_before_symlinks_are_followed() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.root.join("outside")).unwrap();
        fixture.link(fixture.root.join("outside"), "workspace/link");

        let target = fixture
            .resolve("link/../note.txt", TargetMode::Create)
            .unwrap();

        assert_eq!(target.path(), fixture.workspace.join("note.txt"));
        assert!(!target.anchor_is_external);
    }

    #[test]
    fn symlink_loops_and_invalid_inputs_fail() {
        let fixture = Fixture::new();
        fixture.link("loop-b", "workspace/loop-a");
        fixture.link("loop-a", "workspace/loop-b");
        fixture.file("workspace/file.txt");

        assert_eq!(
            fixture.resolve("loop-a/target.txt", TargetMode::Existing),
            Err(PathError::SymLinkLoop)
        );
        for input in ["", "  ", "~other/x", "a\0b"] {
            assert_eq!(
                fixture.resolve(input, TargetMode::Create),
                Err(PathError::InvalidPath),
                "{input:?}"
            );
        }
        assert_eq!(
            fixture.resolve("file.txt/child.txt", TargetMode::Create),
            Err(PathError::NotDir)
        );
        assert_eq!(
            resolve_with_home(&fixture.workspace, "~/x", TargetMode::Create, None),
            Err(PathError::HomeNotSet)
        );
        assert_eq!(
            resolve_with_home(Path::new(""), "x", TargetMode::Create, None),
            Err(PathError::WorkspaceUnavailable)
        );
    }

    #[test]
    fn the_workspace_root_itself_resolves_without_components() {
        let fixture = Fixture::new();
        let target = fixture.resolve(".", TargetMode::Create).unwrap();
        assert_eq!(target.anchor, fixture.workspace);
        assert!(target.components.is_empty());
    }

    #[test]
    fn component_and_length_limits_fail_without_creating_anything() {
        let fixture = Fixture::new();
        let deep = vec!["d"; MAX_FILE_TARGET_COMPONENTS + 1].join("/");
        assert_eq!(
            fixture.resolve(&deep, TargetMode::Create),
            Err(PathError::TooManyPathComponents)
        );
        assert!(!fixture.workspace.join("d").exists());

        let long = "n".repeat(MAX_PATH_BYTES);
        assert_eq!(
            fixture.resolve(&long, TargetMode::Create),
            Err(PathError::InvalidPath)
        );
    }
}
