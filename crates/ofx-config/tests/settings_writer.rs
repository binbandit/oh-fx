use std::fs;
use std::path::PathBuf;

use ofx_config::{ProfilePaths, Settings, SettingsWriteError, save_codex_model};

struct Profile {
    _directory: tempfile::TempDir,
    paths: ProfilePaths,
    workspace: PathBuf,
}

impl Profile {
    fn new(settings: Option<&str>) -> Self {
        let directory = tempfile::tempdir().expect("create a profile");
        let root = directory.path();
        let paths = ProfilePaths {
            config: root.join("config/oh-fx"),
            data: root.join("data/oh-fx"),
            state: root.join("state/oh-fx"),
            cache: root.join("cache/oh-fx"),
        };
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).expect("create the workspace");
        if let Some(settings) = settings {
            fs::create_dir_all(&paths.config).expect("create the config directory");
            fs::write(paths.config.join("settings.json"), settings).expect("write settings");
        }
        Self {
            _directory: directory,
            paths,
            workspace,
        }
    }

    fn load(&self) -> Settings {
        Settings::load(&self.paths, &self.workspace).expect("load settings")
    }
}

fn no_environment(_: &str) -> Option<String> {
    None
}

#[test]
fn a_saved_codex_selection_is_what_the_reader_selects() {
    let profile = Profile::new(Some(r#"{"codex_model":"gpt-5.6-terra","theme":"dark"}"#));
    save_codex_model(&profile.paths, "gpt-6.1-sol").expect("save");
    let settings = profile.load();
    assert!(settings.diagnostics().is_empty());
    assert_eq!(settings.codex_selected(&no_environment), Ok(true));
    assert_eq!(
        settings.selected_codex_model(None, &no_environment),
        Ok("gpt-6.1-sol".to_owned())
    );
}

#[test]
fn workspace_overrides_are_left_to_the_user() {
    let profile = Profile::new(None);
    let workspace = profile.workspace.to_string_lossy().into_owned();
    fs::create_dir_all(&profile.paths.config).expect("create the config directory");
    fs::write(
        profile.paths.config.join("settings.json"),
        format!(r#"{{"workspaces":{{"{workspace}":{{"models":{{"codex":"gpt-5.6-luna"}}}}}}}}"#),
    )
    .expect("write settings");
    save_codex_model(&profile.paths, "gpt-6.1-sol").expect("save");
    assert_eq!(
        profile.load().selected_codex_model(None, &no_environment),
        Ok("gpt-5.6-luna".to_owned())
    );
}

#[test]
fn unreadable_settings_are_never_replaced() {
    let profile = Profile::new(Some("{\"provider\":"));
    assert_eq!(
        save_codex_model(&profile.paths, "gpt-6.1-sol"),
        Err(SettingsWriteError::InvalidFormat)
    );
    assert_eq!(
        fs::read_to_string(profile.paths.config.join("settings.json")).expect("read settings"),
        "{\"provider\":"
    );
}
