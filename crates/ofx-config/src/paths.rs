use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const APPLICATION_DIRECTORY: &str = "oh-fx";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilePaths {
    pub config: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
}

impl ProfilePaths {
    pub fn from_environment() -> Option<Self> {
        let home = env::home_dir()?;
        Some(Self::resolve(&home, |name| env::var_os(name)))
    }

    fn resolve(home: &Path, variable: impl Fn(&str) -> Option<OsString>) -> Self {
        let base_directory = |name: &str, default: &str| {
            variable(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| home.join(default))
                .join(APPLICATION_DIRECTORY)
        };
        Self {
            config: base_directory("XDG_CONFIG_HOME", ".config"),
            data: base_directory("XDG_DATA_HOME", ".local/share"),
            state: base_directory("XDG_STATE_HOME", ".local/state"),
            cache: base_directory("XDG_CACHE_HOME", ".cache"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_xdg_locations_under_home() {
        let paths = ProfilePaths::resolve(Path::new("/home/ada"), |_| None);
        assert_eq!(paths.config, Path::new("/home/ada/.config/oh-fx"));
        assert_eq!(paths.data, Path::new("/home/ada/.local/share/oh-fx"));
        assert_eq!(paths.state, Path::new("/home/ada/.local/state/oh-fx"));
        assert_eq!(paths.cache, Path::new("/home/ada/.cache/oh-fx"));
    }

    #[test]
    fn honours_absolute_xdg_overrides_and_ignores_relative_ones() {
        let paths = ProfilePaths::resolve(Path::new("/home/ada"), |name| match name {
            "XDG_CONFIG_HOME" => Some("/etc/ada".into()),
            "XDG_CACHE_HOME" => Some("relative/cache".into()),
            _ => None,
        });
        assert_eq!(paths.config, Path::new("/etc/ada/oh-fx"));
        assert_eq!(paths.cache, Path::new("/home/ada/.cache/oh-fx"));
    }
}
