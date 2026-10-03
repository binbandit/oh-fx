use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;

struct Fixture {
    _home: TempDir,
    paths: ProfilePaths,
    workspace: PathBuf,
    tools: Vec<String>,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(home.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let paths = ProfilePaths {
            config: root.join("config/oh-fx"),
            data: root.join("data/oh-fx"),
            state: root.join("state/oh-fx"),
            cache: root.join("cache/oh-fx"),
        };
        Self {
            _home: home,
            paths,
            workspace,
            tools: vec!["read_file".to_owned()],
        }
    }

    fn access(&self) -> SettingsAccess<'_> {
        SettingsAccess {
            paths: Some(&self.paths),
            workspace_root: &self.workspace,
            tool_names: self.tools.clone(),
        }
    }

    fn run(&self, rest: &str) -> String {
        let notice = handle_allowlist(&self.access(), rest);
        format!("{:?}|{}|{}", notice.tone, notice.topic, notice.body)
    }

    fn settings(&self) -> Settings {
        Settings::load(&self.paths, &self.workspace).unwrap()
    }

    fn effective(&self) -> Vec<(String, String, PermissionAction)> {
        rules(self.settings().effective_permission_rules())
    }

    fn write_settings(&self, settings: &Value) {
        fs::create_dir_all(&self.paths.config).unwrap();
        fs::write(
            self.paths.config.join("settings.json"),
            settings.to_string(),
        )
        .unwrap();
    }

    fn saved(&self) -> Value {
        let bytes = fs::read(self.paths.config.join("settings.json")).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn add_local(&self, category: &str, pattern: &str) {
        save_permission_patch(
            &self.paths,
            Some(&self.workspace),
            PermissionPatch::Add {
                category,
                pattern,
                action: PermissionAction::Allow,
            },
        )
        .unwrap();
    }
}

fn rules(rules: &[PermissionRule]) -> Vec<(String, String, PermissionAction)> {
    rules
        .iter()
        .map(|rule| (rule.permission.clone(), rule.pattern.clone(), rule.action))
        .collect()
}

fn allow(permission: &str, pattern: &str) -> (String, String, PermissionAction) {
    (
        permission.to_owned(),
        pattern.to_owned(),
        PermissionAction::Allow,
    )
}

#[test]
fn allowlist_adds_lists_and_removes_workspace_rules() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.run("add command \"git *\""),
        "Neutral|allowlist|added command: \"git *\" (scope=local)"
    );
    assert_eq!(
        fixture.run("add tool read_file"),
        "Neutral|allowlist|added tool read: \"*\" (scope=local)"
    );
    assert_eq!(
        fixture.run("add url \"https://example.com/*\""),
        "Neutral|allowlist|added url: \"https://example.com/*\" (scope=local)"
    );
    assert_eq!(
        fixture.run("add command echo"),
        "Neutral|allowlist|added command: \"echo\" (scope=local)"
    );
    fixture.add_local("read", "docs/*");
    assert_eq!(
        fixture.effective(),
        [
            allow("bash", "git *"),
            allow("bash", "echo"),
            allow("read", "*"),
            allow("read", "docs/*"),
            allow("url", "https://example.com/*"),
        ]
    );
    let listing = "Neutral|allowlist|effective persistent allow rules:\n  tools:\n    read: workspace, docs/*\n  commands:\n    git *, echo\n  urls:\n    https://example.com/*";
    assert_eq!(fixture.run(""), listing);
    assert_eq!(fixture.run("view"), listing);
    assert_eq!(fixture.run("VIEW Effective"), listing);
    assert_eq!(
        fixture.run("remove command \"git *\""),
        "Neutral|allowlist|removed command: \"git *\" (scope=local)"
    );
    assert_eq!(
        fixture.effective(),
        [
            allow("bash", "echo"),
            allow("read", "*"),
            allow("read", "docs/*"),
            allow("url", "https://example.com/*"),
        ]
    );
    assert_eq!(
        fixture.run("remove command \"git *\""),
        "Neutral|allowlist|no matching rule for command: \"git *\" (scope=local)"
    );
    assert_eq!(
        fixture.run("reset tools"),
        "Neutral|allowlist|reset tools: removed 2 rules (scope=local)"
    );
    assert_eq!(
        fixture.run("reset all"),
        "Neutral|allowlist|reset all: removed 2 rules (scope=local)"
    );
    assert!(fixture.effective().is_empty());
    assert_eq!(
        fixture.run("view"),
        "Neutral|allowlist|effective persistent allow rules: (none)"
    );
    let workspace = fixture.workspace.to_string_lossy().into_owned();
    assert_eq!(fixture.saved(), json!({"workspaces": {workspace: {}}}));
}

#[test]
fn allowlist_scopes_expose_and_mutate_hidden_user_rules_independently() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.run("user add command \"user *\""),
        "Neutral|allowlist|added command: \"user *\" (scope=user)"
    );
    assert_eq!(
        fixture.run("add command \"local *\""),
        "Neutral|allowlist|added command: \"local *\" (scope=local)"
    );
    assert_eq!(
        fixture.run("user add command \"again *\""),
        "Neutral|allowlist|added command: \"again *\" (scope=user); user rules shadowed by local settings"
    );
    assert_eq!(
        fixture.run("view user"),
        "Neutral|allowlist|user persistent allow rules:\n  commands:\n    user *, again *\nuser rules are shadowed by local settings"
    );
    assert_eq!(
        fixture.run("view local"),
        "Neutral|allowlist|local persistent allow rules:\n  commands:\n    local *"
    );
    assert_eq!(
        fixture.run("view effective"),
        "Neutral|allowlist|effective persistent allow rules:\n  commands:\n    local *\nuser rules are shadowed by local settings"
    );
    assert_eq!(
        fixture.run("user remove command \"user *\""),
        "Neutral|allowlist|removed command: \"user *\" (scope=user); user rules shadowed by local settings"
    );
    assert_eq!(
        fixture.run("user reset all"),
        "Neutral|allowlist|reset all: removed 1 rule (scope=user)"
    );
    let sources = rules(fixture.settings().permission_sources().user);
    assert!(sources.is_empty());
    assert_eq!(
        rules(fixture.settings().permission_sources().local),
        [allow("bash", "local *")]
    );
    assert_eq!(
        fixture.run("LOCAL remove command \"local *\""),
        "Neutral|allowlist|removed command: \"local *\" (scope=local)"
    );
    assert_eq!(
        fixture.run("view effective"),
        "Neutral|allowlist|effective persistent allow rules: (none)"
    );
}

#[test]
fn allowlist_view_reports_unsafe_settings_without_failing() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.paths.config.join("settings.json")).unwrap();
    assert_eq!(
        fixture.run("view"),
        "Error|settings|Failed to load settings: DurablePathUnsafe"
    );
}

#[test]
fn web_fetch_rules_persist_exact_canonical_domains() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.run("add web-fetch-domain Example.COM."),
        "Neutral|allowlist|added web-fetch-domain: \"domain:example.com\" (scope=local)"
    );
    assert_eq!(
        fixture.effective(),
        [allow("web_fetch", "domain:example.com")]
    );
    assert_eq!(
        fixture.run("view"),
        "Neutral|allowlist|effective persistent allow rules:\n  web-fetch domains:\n    domain:example.com"
    );
    assert_eq!(
        fixture.run("remove web-fetch-domain EXAMPLE.com"),
        "Neutral|allowlist|removed web-fetch-domain: \"domain:example.com\" (scope=local)"
    );
    fixture.add_local("web_fetch", "domain:example.com");
    fixture.add_local("web_fetch", "domain:example.org");
    assert_eq!(
        fixture.run("reset web-fetch-domains"),
        "Neutral|allowlist|reset web-fetch-domains: removed 2 rules (scope=local)"
    );
}

#[test]
fn web_fetch_rules_reject_wildcards_urls_and_tool_wide_authorization() {
    let fixture = Fixture::new();
    for rest in [
        "add web-fetch-domain *",
        "add web-fetch-domain https://example.com",
        "add tool web_fetch",
    ] {
        assert_eq!(
            fixture.run(rest),
            "Error||usage: /allowlist add [command|tool|url|web-fetch-domain] <pattern>",
            "{rest}"
        );
    }
}

#[test]
fn malformed_hand_edited_web_fetch_rules_render_a_bounded_warning() {
    let fixture = Fixture::new();
    fixture.add_local("web_fetch", "*");
    fixture.add_local("read", "*");
    assert_eq!(
        fixture.run("view"),
        "Warning|allowlist|effective persistent allow rules:\n  tools:\n    read: workspace\nignored 1 malformed web_fetch rule; expected domain:<canonical-hostname>"
    );
}

#[test]
fn allowlist_reports_usage_for_invalid_input() {
    let fixture = Fixture::new();
    let add = "Error||usage: /allowlist add [command|tool|url|web-fetch-domain] <pattern>";
    let remove = "Error||usage: /allowlist remove [command|tool|url|web-fetch-domain] <pattern>";
    let general =
        "Error||usage: /allowlist [view [effective|local|user]|[local|user] add|remove|reset ...]";
    for (rest, expected) in [
        ("add", add),
        ("add command", add),
        ("add command \"\"", add),
        ("remove nope value", remove),
        ("add tool not_a_tool", add),
        ("remove tool not_a_tool", remove),
        (
            "reset",
            "Error||usage: /allowlist reset [commands|tools|urls|web-fetch-domains|all]",
        ),
        ("view everything", general),
        ("user", general),
        ("local list", general),
        ("list", general),
    ] {
        assert_eq!(fixture.run(rest), expected, "{rest}");
    }
    assert!(!fixture.paths.config.join("settings.json").exists());
}

#[test]
fn allowlist_targets_follow_upstream_parsing() {
    let tools = vec!["web_search".to_owned(), "provider_custom".to_owned()];
    let target = |raw: &str| {
        parse_allowlist_target(&tools, raw).map(|target| (target.category, target.pattern))
    };
    assert_eq!(
        target("tool web_search"),
        Some(("web_search".to_owned(), "*".to_owned()))
    );
    assert_eq!(target("tool web_search current news"), None);
    assert_eq!(
        target("tool provider_custom"),
        Some(("provider_custom".to_owned(), "*".to_owned()))
    );
    assert_eq!(
        target("tool read"),
        Some(("read".to_owned(), "*".to_owned()))
    );
    assert_eq!(target("tool memory"), None);
    assert_eq!(
        target("command \"git push\" --force"),
        Some(("bash".to_owned(), "git push".to_owned()))
    );
    assert_eq!(
        target("command \"unterminated *"),
        Some(("bash".to_owned(), "unterminated *".to_owned()))
    );
    assert_eq!(
        target("COMMAND\tnpm test"),
        Some(("bash".to_owned(), "npm test".to_owned()))
    );
    assert_eq!(
        parse_allowlist_target(&[], "tool provider_custom").map(|target| target.category),
        None
    );
}

#[test]
fn reset_scopes_remove_only_their_allow_rules() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_string_lossy().into_owned();
    fixture.write_settings(&json!({"workspaces": {workspace.clone(): {"permission": {
        "bash": {"git *": "allow", "rm *": "deny"},
        "open_url": "allow",
        "*": "allow",
        "edit": {"*": "allow"},
    }}}}));
    assert_eq!(
        fixture.run("reset commands"),
        "Neutral|allowlist|reset commands: removed 1 rule (scope=local)"
    );
    assert_eq!(
        fixture.run("reset urls"),
        "Neutral|allowlist|reset urls: removed 1 rule (scope=local)"
    );
    assert_eq!(
        fixture.run("reset tools"),
        "Neutral|allowlist|reset tools: removed 1 rule (scope=local)"
    );
    assert_eq!(
        fixture.saved(),
        json!({"workspaces": {workspace.clone(): {"permission": {
            "bash": {"rm *": "deny"},
            "*": "allow",
        }}}})
    );
    assert_eq!(
        fixture.run("reset all"),
        "Neutral|allowlist|reset all: removed 1 rule (scope=local)"
    );
    assert_eq!(
        fixture.run("reset all"),
        "Neutral|allowlist|reset all: removed 0 rules (scope=local)"
    );
}

#[test]
fn a_whole_category_rule_becomes_a_pattern_map_when_a_pattern_is_added() {
    let fixture = Fixture::new();
    fixture.write_settings(&json!({"permission": {"bash": "ask"}}));
    assert_eq!(
        fixture.run("user add command ls"),
        "Neutral|allowlist|added command: \"ls\" (scope=user)"
    );
    assert_eq!(
        fixture.saved(),
        json!({"permission": {"bash": {"*": "ask", "ls": "allow"}}})
    );
    assert_eq!(
        fixture.run("view user"),
        "Neutral|allowlist|user persistent allow rules:\n  commands:\n    ls"
    );
}

#[test]
fn failed_saves_keep_the_explicit_scope_and_error() {
    let fixture = Fixture::new();
    fixture.write_settings(&json!({"permission": 5}));
    assert_eq!(
        fixture.run("user add command ls"),
        "Error|allowlist|failed to add rule to settings (scope=user, error=InvalidSettingsFormat)"
    );
    assert_eq!(
        fixture.run("remove command ls"),
        "Neutral|allowlist|no matching rule for command: \"ls\" (scope=local)"
    );
    fixture.write_settings(&json!({"workspaces": 5}));
    assert_eq!(
        fixture.run("reset all"),
        "Error|allowlist|failed to reset rules in settings (scope=local, error=InvalidSettingsFormat)"
    );
    let unsaved = SettingsAccess {
        paths: None,
        workspace_root: &fixture.workspace,
        tool_names: Vec::new(),
    };
    let notice = handle_allowlist(&unsaved, "add command ls");
    assert_eq!(
        notice.body,
        "failed to add rule to settings (scope=local, error=HomeNotSet)"
    );
    assert_eq!(
        handle_allowlist(&unsaved, "view").body,
        "effective persistent allow rules: (none)"
    );
}

#[test]
fn a_save_whose_settings_cannot_be_read_back_is_reported_as_unresolved() {
    let fixture = Fixture::new();
    fixture.write_settings(&json!({"providers": 5}));
    assert_eq!(
        fixture.run("add command ls"),
        "Warning|allowlist|added command: \"ls\" (scope=local); saved but effective source unknown and runtime reload failed (InvalidObject)"
    );
    assert_eq!(
        fixture.run("reset all"),
        "Warning|allowlist|reset all: removed 1 rule (scope=local); saved but effective source unknown and runtime reload failed (InvalidObject)"
    );
}
