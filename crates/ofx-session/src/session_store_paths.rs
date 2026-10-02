#[cfg(target_vendor = "apple")]
pub(crate) const MAX_PATH_BYTES: usize = 1024;
#[cfg(not(target_vendor = "apple"))]
pub(crate) const MAX_PATH_BYTES: usize = 4096;

pub(crate) fn normalize_workspace_root(workspace_root: &str) -> &str {
    let mut end = workspace_root.len();
    while end > 1 && workspace_root.as_bytes()[end - 1] == b'/' {
        end -= 1;
    }
    &workspace_root[..end]
}

pub(crate) fn is_valid_workspace_root(workspace_root: &str) -> bool {
    !workspace_root.is_empty()
        && workspace_root.len() <= MAX_PATH_BYTES
        && workspace_root.starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_roots_lose_trailing_slashes_but_keep_the_root() {
        assert_eq!(normalize_workspace_root("/work/space//"), "/work/space");
        assert_eq!(normalize_workspace_root("///"), "/");
        assert_eq!(normalize_workspace_root("/"), "/");
        assert_eq!(normalize_workspace_root(""), "");
    }

    #[test]
    fn workspace_roots_must_be_absolute_and_bounded() {
        assert!(is_valid_workspace_root("/work"));
        assert!(!is_valid_workspace_root(""));
        assert!(!is_valid_workspace_root("work"));
        assert!(is_valid_workspace_root(&format!(
            "/{}",
            "a".repeat(MAX_PATH_BYTES - 1)
        )));
        assert!(!is_valid_workspace_root(&format!(
            "/{}",
            "a".repeat(MAX_PATH_BYTES)
        )));
    }
}
