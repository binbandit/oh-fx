use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::path_error::PathError;

mod file_identity;
mod file_mutation_target;

pub use file_identity::{
    FileIdentity, FileKind, descriptor_identity, entry_identity, open_child_directory,
    open_directory,
};
pub use file_mutation_target::{FileMutationTarget, TargetMode, resolve_file_mutation_target};

#[cfg(target_os = "macos")]
pub const MAX_PATH_BYTES: usize = 1024;
#[cfg(not(target_os = "macos"))]
pub const MAX_PATH_BYTES: usize = 4096;

pub const PATH_ENTRY_WHITESPACE: &[char] = &[' ', '\t', '\r', '\n'];
const SEPARATOR: u8 = b'/';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathScope {
    InsideOnly,
    External,
}

struct ResolvedInput {
    absolute: Vec<u8>,
    external_intent: bool,
}

enum ExternalPathInput<'a> {
    Absolute(&'a [u8]),
    HomeRelative(&'a [u8]),
    WorkspaceRelative(&'a [u8]),
}

pub fn resolve_workspace_path(
    workspace_root: &Path,
    input_path: &str,
) -> Result<PathBuf, PathError> {
    resolve_path(workspace_root, input_path, None, PathScope::InsideOnly)
}

pub fn resolve_workspace_or_external_path(
    workspace_root: &Path,
    input_path: &str,
) -> Result<PathBuf, PathError> {
    let home = env::var_os("HOME");
    resolve_workspace_or_external_path_with_home(workspace_root, input_path, home.as_deref())
}

pub fn workspace_relative_path(workspace_root: &Path, absolute: &Path) -> PathBuf {
    let root = workspace_root.as_os_str().as_bytes();
    let candidate = absolute.as_os_str().as_bytes();
    if !inside(root, candidate) {
        return absolute.to_path_buf();
    }
    let relative = candidate.get(root.len()..).unwrap_or_default();
    let start = relative
        .iter()
        .position(|byte| *byte != SEPARATOR)
        .unwrap_or(relative.len());
    bytes_path(&relative[start..]).to_path_buf()
}

pub fn path_inside(root: &Path, candidate: &Path) -> bool {
    inside(
        root.as_os_str().as_bytes(),
        candidate.as_os_str().as_bytes(),
    )
}

fn resolve_workspace_or_external_path_with_home(
    workspace_root: &Path,
    input_path: &str,
    home: Option<&OsStr>,
) -> Result<PathBuf, PathError> {
    resolve_path(workspace_root, input_path, home, PathScope::External)
}

pub fn resolve_workspace_or_external_literal_path(
    workspace_root: &Path,
    path: &str,
) -> Result<PathBuf, PathError> {
    let home = env::var_os("HOME");
    resolve_workspace_or_external_literal_path_with_home(workspace_root, path, home.as_deref())
}

pub(crate) fn resolve_workspace_or_external_literal_path_with_home(
    workspace_root: &Path,
    path: &str,
    home: Option<&OsStr>,
) -> Result<PathBuf, PathError> {
    resolve_cleaned_path(workspace_root, path, home, PathScope::External)
}

fn resolve_path(
    workspace_root: &Path,
    input_path: &str,
    home: Option<&OsStr>,
    scope: PathScope,
) -> Result<PathBuf, PathError> {
    resolve_cleaned_path(
        workspace_root,
        input_path.trim_matches(PATH_ENTRY_WHITESPACE),
        home,
        scope,
    )
}

fn resolve_cleaned_path(
    workspace_root: &Path,
    path: &str,
    home: Option<&OsStr>,
    scope: PathScope,
) -> Result<PathBuf, PathError> {
    if path.is_empty() || path.contains('\0') {
        return Err(PathError::InvalidPath);
    }
    let root = workspace_root.as_os_str().as_bytes();
    let input = resolve_input(root, path.as_bytes(), home.map(OsStrExt::as_bytes), scope)?;
    let resolved = realpath(&input.absolute)?;
    if !input.external_intent {
        ensure_inside(root, &resolved)?;
    }
    Ok(PathBuf::from(OsString::from_vec(resolved)))
}

fn resolve_input(
    workspace_root: &[u8],
    cleaned: &[u8],
    home: Option<&[u8]>,
    scope: PathScope,
) -> Result<ResolvedInput, PathError> {
    if scope == PathScope::InsideOnly {
        let absolute = if is_absolute(cleaned) {
            resolve_lexically(&[cleaned])
        } else {
            resolve_lexically(&[workspace_root, cleaned])
        };
        return Ok(ResolvedInput {
            absolute,
            external_intent: false,
        });
    }

    Ok(match classify_external_path_input(cleaned)? {
        ExternalPathInput::Absolute(absolute) => ResolvedInput {
            absolute: resolve_lexically(&[absolute]),
            external_intent: true,
        },
        ExternalPathInput::HomeRelative(relative) => {
            let home = home.ok_or(PathError::HomeNotSet)?;
            if home.is_empty() || !is_absolute(home) {
                return Err(PathError::InvalidPath);
            }
            let expanded = [home, relative].concat();
            ResolvedInput {
                absolute: resolve_lexically(&[&expanded]),
                external_intent: true,
            }
        }
        ExternalPathInput::WorkspaceRelative(relative) => {
            let absolute = resolve_lexically(&[workspace_root, relative]);
            let external_intent = !inside(workspace_root, &absolute);
            ResolvedInput {
                absolute,
                external_intent,
            }
        }
    })
}

fn classify_external_path_input(cleaned: &[u8]) -> Result<ExternalPathInput<'_>, PathError> {
    if cleaned.is_empty() {
        return Err(PathError::InvalidPath);
    }
    if is_absolute(cleaned) {
        return Ok(ExternalPathInput::Absolute(cleaned));
    }
    if cleaned == b"~" {
        return Ok(ExternalPathInput::HomeRelative(&[]));
    }
    if cleaned.starts_with(b"~/") {
        return Ok(ExternalPathInput::HomeRelative(&cleaned[1..]));
    }
    if cleaned[0] == b'~' {
        return Err(PathError::InvalidPath);
    }
    Ok(ExternalPathInput::WorkspaceRelative(cleaned))
}

fn ensure_inside(workspace_root: &[u8], absolute: &[u8]) -> Result<(), PathError> {
    if workspace_root.is_empty() {
        return Err(PathError::WorkspaceUnavailable);
    }
    if !inside(workspace_root, absolute) {
        return Err(PathError::PathOutsideWorkspace);
    }
    Ok(())
}

fn inside(root: &[u8], candidate: &[u8]) -> bool {
    if root == candidate {
        return true;
    }
    if !candidate.starts_with(root) || root.is_empty() {
        return false;
    }
    if root.last() == Some(&SEPARATOR) {
        return true;
    }
    candidate.len() > root.len() && candidate[root.len()] == SEPARATOR
}

fn realpath(path: &[u8]) -> Result<Vec<u8>, PathError> {
    if path.len() >= MAX_PATH_BYTES {
        return Err(PathError::NameTooLong);
    }
    Ok(fs::canonicalize(bytes_path(path))
        .map_err(|error| PathError::from_realpath(&error))?
        .into_os_string()
        .into_vec())
}

fn is_absolute(path: &[u8]) -> bool {
    path.first() == Some(&SEPARATOR)
}

pub(crate) fn resolve_lexically(paths: &[&[u8]]) -> Vec<u8> {
    let mut absolute = false;
    let mut components: Vec<&[u8]> = Vec::new();
    for path in paths {
        if is_absolute(path) {
            absolute = true;
            components.clear();
        }
        for component in path.split(|byte| *byte == SEPARATOR) {
            match component {
                b"" | b"." => {}
                b".." => {
                    if components.last().is_some_and(|last| *last != b"..") {
                        components.pop();
                    } else if !absolute {
                        components.push(component);
                    }
                }
                name => components.push(name),
            }
        }
    }
    let joined = components.join(&SEPARATOR);
    if absolute {
        [&[SEPARATOR][..], &joined].concat()
    } else if joined.is_empty() {
        b".".to_vec()
    } else {
        joined
    }
}

fn bytes_path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::*;

    struct Fixture {
        _temp: TempDir,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let root = fs::canonicalize(temp.path()).unwrap();
            Self { _temp: temp, root }
        }

        fn dir(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(&path).unwrap();
            path
        }

        fn file(&self, relative: &str, content: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            path
        }

        fn link(&self, target: impl AsRef<Path>, relative: &str) {
            symlink(target, self.root.join(relative)).unwrap();
        }
    }

    fn with_home(workspace: &Path, input: &str, home: Option<&Path>) -> Result<PathBuf, PathError> {
        resolve_workspace_or_external_path_with_home(workspace, input, home.map(Path::as_os_str))
    }

    fn ensure_path_inside_workspace(
        workspace_root: &Path,
        absolute: &Path,
    ) -> Result<(), PathError> {
        ensure_inside(
            workspace_root.as_os_str().as_bytes(),
            absolute.as_os_str().as_bytes(),
        )
    }

    #[test]
    fn path_inside_preserves_exact_child_empty_root_and_prefix_semantics() {
        assert!(inside(b"/workspace", b"/workspace"));
        assert!(inside(b"/workspace", b"/workspace/file.txt"));
        assert!(inside(b"/workspace/", b"/workspace/file.txt"));
        assert!(inside(b"", b""));
        assert!(!inside(b"", b"/workspace"));
        assert!(!inside(b"/workspace", b"/workspace-evil/file.txt"));
        assert!(path_inside(
            Path::new("/workspace"),
            Path::new("/workspace/file.txt")
        ));
        assert!(!path_inside(Path::new("/workspace"), Path::new("/other")));
    }

    #[test]
    fn ensure_path_inside_workspace_allows_exact_root_and_child_paths() {
        let root = Path::new("/home/user/project");
        assert_eq!(ensure_path_inside_workspace(root, root), Ok(()));
        assert_eq!(
            ensure_path_inside_workspace(root, Path::new("/home/user/project/src/main.zig")),
            Ok(())
        );
    }

    #[test]
    fn ensure_path_inside_workspace_preserves_root_with_trailing_separator_behavior() {
        let root = Path::new("/home/user/project/");
        assert_eq!(
            ensure_path_inside_workspace(root, Path::new("/home/user/project/src/main.zig")),
            Ok(())
        );
        assert_eq!(
            ensure_path_inside_workspace(root, Path::new("/home/user/project-other/src/main.zig")),
            Err(PathError::PathOutsideWorkspace)
        );
    }

    #[test]
    fn ensure_path_inside_workspace_rejects_outside_paths_and_prefix_collisions() {
        assert_eq!(
            ensure_path_inside_workspace(
                Path::new("/home/user/project"),
                Path::new("/home/user/other/file.txt")
            ),
            Err(PathError::PathOutsideWorkspace)
        );
        assert_eq!(
            ensure_path_inside_workspace(
                Path::new("/home/user/proj"),
                Path::new("/home/user/project/file.txt")
            ),
            Err(PathError::PathOutsideWorkspace)
        );
    }

    #[test]
    fn ensure_path_inside_workspace_rejects_empty_workspace_roots() {
        assert_eq!(
            ensure_path_inside_workspace(Path::new(""), Path::new("/some/path")),
            Err(PathError::WorkspaceUnavailable)
        );
    }

    #[test]
    fn workspace_relative_path_returns_relative_paths_inside_workspace_and_absolute_paths_outside()
    {
        let root = Path::new("/home/user/project");
        assert_eq!(
            workspace_relative_path(root, Path::new("/home/user/project/src/main.zig")),
            Path::new("src/main.zig")
        );
        assert_eq!(
            workspace_relative_path(root, Path::new("/etc/passwd")),
            Path::new("/etc/passwd")
        );
        assert_eq!(workspace_relative_path(root, root), Path::new(""));
    }

    #[test]
    fn resolve_workspace_path_rejects_empty_and_whitespace_only_inputs() {
        let root = Path::new("/tmp/ws");
        assert_eq!(
            resolve_workspace_path(root, ""),
            Err(PathError::InvalidPath)
        );
        assert_eq!(
            resolve_workspace_path(root, "   \t\n  "),
            Err(PathError::InvalidPath)
        );
    }

    #[test]
    fn resolve_workspace_path_rejects_absolute_paths_outside_the_workspace() {
        let fixture = Fixture::new();
        let workspace = fixture.dir("workspace");
        let outside = fixture.file("external/file.txt", "outside");

        assert_eq!(
            resolve_workspace_path(&workspace, outside.to_str().unwrap()),
            Err(PathError::PathOutsideWorkspace)
        );
    }

    #[test]
    fn workspace_only_resolver_keeps_tilde_literal_and_rejects_relative_escapes() {
        let fixture = Fixture::new();
        let workspace = fixture.dir("workspace");
        let literal = fixture.file("workspace/~/literal.txt", "literal");
        fixture.file("external/outside.txt", "outside");

        assert_eq!(
            resolve_workspace_path(&workspace, "~/literal.txt"),
            Ok(literal)
        );
        assert_eq!(
            resolve_workspace_path(&workspace, "../external/outside.txt"),
            Err(PathError::PathOutsideWorkspace)
        );
    }

    #[test]
    fn resolve_workspace_or_external_path_allows_explicit_absolute_existing_paths_outside_the_workspace()
     {
        let fixture = Fixture::new();
        let workspace = fixture.dir("workspace");
        let outside = fixture.file("external/file.txt", "outside");

        assert_eq!(
            with_home(&workspace, outside.to_str().unwrap(), None),
            Ok(outside)
        );
    }

    #[test]
    fn external_resolver_expands_home_and_canonicalizes_relative_and_absolute_aliases() {
        let fixture = Fixture::new();
        let workspace = fixture.dir("workspace");
        let home = fixture.dir("home");
        let home_file = fixture.file("home/fx-path-fixture.txt", "home");
        let external_file = fixture.file("external/fx-path-fixture.txt", "external");

        assert_eq!(
            with_home(&workspace, "~/fx-path-fixture.txt", Some(&home)),
            Ok(home_file)
        );
        assert_eq!(
            with_home(&workspace, "../external/fx-path-fixture.txt", Some(&home)),
            Ok(external_file.clone())
        );
        assert_eq!(
            with_home(&workspace, external_file.to_str().unwrap(), Some(&home)),
            Ok(external_file)
        );
    }

    #[test]
    fn external_resolver_handles_exact_home_root_and_normalized_home_escapes() {
        let fixture = Fixture::new();
        let workspace = fixture.dir("workspace");
        let home = fixture.dir("home");
        let home_file = fixture.file("home/fx-path-fixture.txt", "home");
        let external_file = fixture.file("external/fx-path-fixture.txt", "external");

        assert_eq!(with_home(&workspace, "~", Some(&home)), Ok(home.clone()));
        assert_eq!(with_home(&workspace, "~/", Some(&home)), Ok(home.clone()));
        assert_eq!(
            with_home(&workspace, "~/../external/fx-path-fixture.txt", Some(&home)),
            Ok(external_file)
        );
        assert_eq!(
            with_home(&workspace, "~//fx-path-fixture.txt", Some(&home)),
            Ok(home_file)
        );
        assert_eq!(
            with_home(&workspace, "~", Some(Path::new("/"))),
            Ok(PathBuf::from("/"))
        );
    }

    #[test]
    fn external_resolver_rejects_invalid_home_inputs_and_unsupported_tilde_forms() {
        let workspace = Path::new("/tmp/workspace");
        assert_eq!(with_home(workspace, "~", None), Err(PathError::HomeNotSet));
        assert_eq!(
            with_home(workspace, "~", Some(Path::new(""))),
            Err(PathError::InvalidPath)
        );
        assert_eq!(
            with_home(workspace, "~", Some(Path::new("relative/home"))),
            Err(PathError::InvalidPath)
        );
        assert_eq!(
            with_home(workspace, "~/file.txt", None),
            Err(PathError::HomeNotSet)
        );
        assert_eq!(
            with_home(workspace, "~//file.txt", None),
            Err(PathError::HomeNotSet)
        );

        let invalid_paths = [
            "~other",
            "~other/file.txt",
            "~notes.txt",
            "~+",
            "~-",
            "~~",
            "~\\file.txt",
            "~ user/file.txt",
            "",
            " \t\r\n ",
        ];
        for input_path in invalid_paths {
            assert_eq!(
                with_home(workspace, input_path, Some(Path::new("/tmp/home"))),
                Err(PathError::InvalidPath),
                "{input_path:?}"
            );
        }
    }

    #[test]
    fn external_resolver_follows_home_symlinks_but_rejects_workspace_relative_symlink_escapes() {
        let fixture = Fixture::new();
        let workspace = fixture.dir("workspace");
        let home = fixture.dir("home");
        let external_file = fixture.file("external/fx-path-fixture.txt", "external");
        let external = fixture.root.join("external");
        fixture.link(&external, "home/external-link");
        fixture.link(&external, "workspace/external-link");

        assert_eq!(
            with_home(
                &workspace,
                "~/external-link/fx-path-fixture.txt",
                Some(&home)
            ),
            Ok(external_file)
        );
        assert_eq!(
            with_home(&workspace, "external-link/fx-path-fixture.txt", Some(&home)),
            Err(PathError::PathOutsideWorkspace)
        );
    }

    #[test]
    fn external_resolver_preserves_workspace_intent_for_lexical_re_entry() {
        let fixture = Fixture::new();
        let workspace = fixture.dir("workspace");
        let external = fixture.dir("external");
        let inside_file = fixture.file("workspace/inside.txt", "inside");
        fixture.file("external/escaped.txt", "outside");
        fixture.link(&external, "workspace/outside-link");

        assert_eq!(
            with_home(&workspace, "../workspace/inside.txt", None),
            Ok(inside_file)
        );
        assert_eq!(
            with_home(&workspace, "../workspace/outside-link/escaped.txt", None),
            Err(PathError::PathOutsideWorkspace)
        );
    }

    #[test]
    fn external_resolver_normalizes_aliases_without_shell_expansion() {
        let fixture = Fixture::new();
        let workspace = fixture.dir("workspace");
        let home = fixture.dir("home");
        let inside = fixture.file("workspace/inside.txt", "inside");
        let literal_home = fixture.file("workspace/$HOME/literal.txt", "literal-home");
        let literal_star = fixture.file("workspace/*/literal.txt", "literal-star");
        let home_file = fixture.file("home/home.txt", "home");
        let external_file = fixture.file("external/outside.txt", "outside");
        let home_with_separator = PathBuf::from(format!("{}/", home.display()));

        assert_eq!(
            with_home(&workspace, " ./nested/../inside.txt ", Some(&home)),
            Ok(inside)
        );
        assert_eq!(
            with_home(
                &workspace,
                "../external/./nested/../outside.txt",
                Some(&home)
            ),
            Ok(external_file)
        );
        assert_eq!(
            with_home(&workspace, "~///./home.txt", Some(&home_with_separator)),
            Ok(home_file)
        );
        assert_eq!(
            with_home(&workspace, "$HOME/literal.txt", Some(&home)),
            Ok(literal_home)
        );
        assert_eq!(
            with_home(&workspace, "*/literal.txt", Some(&home)),
            Ok(literal_star)
        );
    }

    #[test]
    fn external_resolver_accepts_deep_lexical_traversal_to_filesystem_root() {
        let input = "../".repeat(128);
        assert_eq!(
            with_home(Path::new("/tmp/fx/deep/workspace"), &input, None),
            Ok(PathBuf::from("/"))
        );
    }

    #[test]
    fn resolvers_reject_nul_bytes_as_invalid_paths_instead_of_cutting_them() {
        let fixture = Fixture::new();
        let workspace = fixture.dir("workspace");
        fixture.file("workspace/a.txt", "inside");
        let absolute = format!("{}\0x", workspace.join("a.txt").display());

        for input in [
            "a.txt\0x",
            "\0",
            " a.txt\0 ",
            absolute.as_str(),
            "~/a.txt\0",
        ] {
            assert_eq!(
                with_home(&workspace, input, Some(&workspace)),
                Err(PathError::InvalidPath),
                "{input:?}"
            );
        }
    }

    #[test]
    fn resolvers_report_name_too_long_once_the_resolved_path_reaches_the_platform_limit() {
        let fixture = Fixture::new();
        let workspace = fixture.dir("workspace");
        let root = fixture.root.to_str().unwrap();
        let path_of_length = |length: usize| {
            let mut path = root.to_owned();
            while length - path.len() > 3 {
                path.push_str("/x");
            }
            path.push('/');
            path.push_str(&"y".repeat(length - path.len()));
            path
        };

        let at_limit = path_of_length(MAX_PATH_BYTES);
        assert_eq!(at_limit.len(), MAX_PATH_BYTES);
        assert_eq!(
            with_home(&workspace, &at_limit, None),
            Err(PathError::NameTooLong)
        );
        assert_eq!(
            with_home(&workspace, &path_of_length(MAX_PATH_BYTES - 1), None),
            Err(PathError::FileNotFound)
        );
        assert_eq!(
            with_home(&workspace, &format!("{at_limit}/.."), None),
            Err(PathError::FileNotFound)
        );
    }

    #[test]
    fn external_resolver_canonicalizes_a_symlinked_home_root() {
        let fixture = Fixture::new();
        let workspace = fixture.dir("workspace");
        let home = fixture.dir("home");
        let home_file = fixture.file("home/file.txt", "home");
        fixture.link(&home, "home-link");

        assert_eq!(
            with_home(
                &workspace,
                "~/file.txt",
                Some(&fixture.root.join("home-link"))
            ),
            Ok(home_file)
        );
    }
}
