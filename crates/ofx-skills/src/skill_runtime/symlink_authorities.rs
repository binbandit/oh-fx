use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ofx_workspace::path_inside;

use crate::byte_trim::trim;

const SYMLINK_AUTHORITIES_VARIABLE: &str = "OH_FX_SKILL_SYMLINK_AUTHORITIES";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SymlinkAuthorities {
    configured: Vec<PathBuf>,
    external: Vec<PathBuf>,
}

impl SymlinkAuthorities {
    pub fn new(configured: &[PathBuf], external_variable: Option<&OsStr>) -> Self {
        Self {
            configured: configured
                .iter()
                .filter(|path| is_absolute_without_parent(path.as_os_str().as_bytes()))
                .map(|path| fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
                .collect(),
            external: external_variable
                .map(external_authorities)
                .unwrap_or_default(),
        }
    }

    pub fn from_environment(configured: &[PathBuf]) -> Self {
        Self::new(
            configured,
            env::var_os(SYMLINK_AUTHORITIES_VARIABLE).as_deref(),
        )
    }

    pub(crate) fn allows(&self, read_authority: &Path, canonical_path: &Path) -> bool {
        path_inside(read_authority, canonical_path)
            || self
                .configured
                .iter()
                .chain(&self.external)
                .any(|authority| path_inside(authority, canonical_path))
    }
}

fn external_authorities(raw: &OsStr) -> Vec<PathBuf> {
    raw.as_bytes()
        .split(|&byte| byte == b':')
        .map(|entry| trim(entry, b" \t"))
        .filter(|entry| !entry.is_empty() && is_absolute_without_parent(entry))
        .map(|entry| PathBuf::from(OsStr::from_bytes(entry)))
        .collect()
}

fn is_absolute_without_parent(path: &[u8]) -> bool {
    path.starts_with(b"/") && !path.split(|&byte| byte == b'/').any(|part| part == b"..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_symlink_authorities_parses_colon_separated_absolute_paths() {
        let authorities = SymlinkAuthorities::new(
            &[],
            Some(OsStr::new(
                "/nix/store:/opt/skills: relative :/bad/../path::\t/tabbed\t",
            )),
        );
        assert_eq!(
            authorities.external,
            [
                PathBuf::from("/nix/store"),
                PathBuf::from("/opt/skills"),
                PathBuf::from("/tabbed")
            ]
        );
    }

    #[test]
    fn external_symlink_authorities_returns_empty_when_unset() {
        assert_eq!(
            SymlinkAuthorities::new(&[], None),
            SymlinkAuthorities::default()
        );
        assert_eq!(
            SymlinkAuthorities::new(&[], Some(OsStr::new(""))),
            SymlinkAuthorities::default()
        );
    }
}
