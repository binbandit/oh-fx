use std::env;
use std::fmt::Write as _;
use std::path::Path;

use ofx_config::{
    AllowlistResetScope, CommitOutcome, ConfigSource, PermissionPatch, PermissionSources,
    ProfilePaths, Settings, save_permission_patch, save_startup_scrollback, save_statusline_item,
};
use ofx_contract::{
    Notice, NoticeTone, PermissionAction, PermissionMode, PermissionRule, StatuslineItem,
    StatuslineToggles, UiEvent,
};
use ofx_permissions::{
    WEB_FETCH_PERMISSION, WEB_SEARCH_PERMISSION, canonical_web_fetch_domain_pattern,
    is_canonical_web_fetch_domain_pattern, permission_name_for_tool, web_fetch_rule_warning_count,
};

use crate::user_settings::{Unsaved, append_legacy_cleanup, not_saved_notice, unsaved_notice};

const ALLOWLIST_TOPIC: &str = "allowlist";
const SETTINGS_TOPIC: &str = "settings";
const HOME_NOT_SET: &str = "HomeNotSet";
const ALLOWLIST_USAGE: &str =
    "usage: /allowlist [view [effective|local|user]|[local|user] add|remove|reset ...]";
const ADD_USAGE: &str = "usage: /allowlist add [command|tool|url|web-fetch-domain] <pattern>";
const REMOVE_USAGE: &str = "usage: /allowlist remove [command|tool|url|web-fetch-domain] <pattern>";
const RESET_USAGE: &str = "usage: /allowlist reset [commands|tools|urls|web-fetch-domains|all]";
const RELOAD_FAILED: &str = "saved but effective source unknown and runtime reload failed";
const KNOWN_PERMISSION_CATEGORIES: [&str; 6] = [
    "edit",
    "read",
    "glob",
    "grep",
    "skill",
    WEB_SEARCH_PERMISSION,
];
const WORKSPACE_PATH_PERMISSIONS: [&str; 4] = ["edit", "read", "glob", "grep"];
const SEPARATORS: [char; 2] = [' ', '\t'];
const SETTINGS_USAGE: &str = "usage: /settings [startup-scrollback [on|off]]";
const STARTUP_SCROLLBACK_LABEL: &str = "startup-scrollback";
const SAVED_TO_USER_SETTINGS: &str = "saved to user settings (scope=user)";
const STATUSLINE_TOPIC: &str = "statusline";
const STATUSLINE_USAGE: &str = "usage: /statusline [context|session|workspace]";

pub(crate) struct SettingsAccess<'a> {
    pub(crate) paths: Option<&'a ProfilePaths>,
    pub(crate) workspace_root: &'a Path,
    pub(crate) tool_names: Vec<String>,
}

pub(crate) struct SessionFacts<'a> {
    pub(crate) model: &'a str,
    pub(crate) permission_mode: PermissionMode,
    pub(crate) step_limit: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionScope {
    User,
    Local,
}

impl PermissionScope {
    const fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Local => "local",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AllowlistView {
    Effective,
    Local,
    User,
}

impl AllowlistView {
    const fn label(self) -> &'static str {
        match self {
            Self::Effective => "effective",
            Self::Local => "local",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AllowlistKind {
    Command,
    Tool,
    Url,
    WebFetchDomain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AllowlistTarget {
    kind: AllowlistKind,
    category: String,
    pattern: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WordSplit<'a> {
    word: &'a str,
    rest: &'a str,
}

pub(crate) fn handle_allowlist(access: &SettingsAccess<'_>, rest: &str) -> Notice {
    let Some(first) = split_first_word(rest) else {
        return allowlist_status(access, AllowlistView::Effective);
    };
    if first.word.eq_ignore_ascii_case("view") {
        return match parse_allowlist_view(first.rest) {
            Some(view) => allowlist_status(access, view),
            None => usage(ALLOWLIST_USAGE),
        };
    }
    let mut scope = PermissionScope::Local;
    let mut action = first;
    if first.word.eq_ignore_ascii_case("user") || first.word.eq_ignore_ascii_case("local") {
        if first.word.eq_ignore_ascii_case("user") {
            scope = PermissionScope::User;
        }
        let Some(next) = split_first_word(first.rest) else {
            return usage(ALLOWLIST_USAGE);
        };
        action = next;
    }
    if action.word.eq_ignore_ascii_case("add") {
        return allowlist_add(access, scope, action.rest);
    }
    if action.word.eq_ignore_ascii_case("remove") {
        return allowlist_remove(access, scope, action.rest);
    }
    if action.word.eq_ignore_ascii_case("reset") {
        return allowlist_reset(access, scope, action.rest);
    }
    usage(ALLOWLIST_USAGE)
}

pub(crate) fn handle_settings(
    access: &SettingsAccess<'_>,
    facts: &SessionFacts<'_>,
    rest: &str,
) -> Vec<Notice> {
    let Some(first) = split_first_word(rest) else {
        return vec![settings_status(access, facts)];
    };
    if !first.word.eq_ignore_ascii_case("startup-scrollback") {
        return vec![usage(SETTINGS_USAGE)];
    }
    let enabled = match split_first_word(first.rest) {
        Some(value) if !value.rest.is_empty() => return vec![usage(SETTINGS_USAGE)],
        Some(value) if value.word.eq_ignore_ascii_case("on") => true,
        Some(value) if value.word.eq_ignore_ascii_case("off") => false,
        Some(_) => return vec![usage(SETTINGS_USAGE)],
        None => match load(access) {
            Ok(settings) => !settings.startup_scrollback(),
            Err(error) => return vec![settings_load_error(&error)],
        },
    };
    save_startup_scrollback_setting(access, enabled)
}

fn settings_status(access: &SettingsAccess<'_>, facts: &SessionFacts<'_>) -> Notice {
    let settings = match load(access) {
        Ok(settings) => settings,
        Err(error) => return settings_load_error(&error),
    };
    let lookup = |name: &str| env::var(name).ok();
    Notice::new(
        NoticeTone::Neutral,
        SETTINGS_TOPIC,
        format!(
            "model: {}\nmodel_config_source: {}\npermission_mode: {}\nworkspace: {}\nstep_limit: {}\nstartup_scrollback: {}",
            facts.model,
            settings.gateway_model_source(&lookup).label(),
            facts.permission_mode.display_label(),
            access.workspace_root.to_string_lossy(),
            facts.step_limit,
            on_off(settings.startup_scrollback()),
        ),
    )
}

fn save_startup_scrollback_setting(access: &SettingsAccess<'_>, enabled: bool) -> Vec<Notice> {
    let saved = match access.paths {
        Some(paths) => save_startup_scrollback(paths, enabled).map_err(Unsaved::Failed),
        None => Err(Unsaved::HomeNotSet),
    };
    let outcome = match saved {
        Ok(outcome) => outcome,
        Err(unsaved) => return vec![not_saved_notice(STARTUP_SCROLLBACK_LABEL, &unsaved)],
    };
    let mut notices = Vec::new();
    let settings = report_user_settings_commit(
        access,
        STARTUP_SCROLLBACK_LABEL,
        &outcome,
        |settings| Some(("startup_scrollback", settings.startup_scrollback_source())),
        false,
        &mut notices,
    );
    let detail = match settings.map(|settings| settings.startup_scrollback_source()) {
        Some(ConfigSource::UserGlobal) => "(applies on next launch)".to_owned(),
        Some(source) => format!("(saved user default; current source={})", source.label()),
        None => "(saved user default; next-launch source unknown)".to_owned(),
    };
    notices.push(Notice::new(
        NoticeTone::Neutral,
        SETTINGS_TOPIC,
        format!("startup_scrollback: {} {detail}", on_off(enabled)),
    ));
    notices
}

pub(crate) fn handle_statusline(
    access: &SettingsAccess<'_>,
    toggles: &mut StatuslineToggles,
    rest: &str,
) -> Vec<UiEvent> {
    let requested = rest.trim_matches(SEPARATORS);
    if requested.is_empty() {
        return vec![notice_event(statusline_status(*toggles))];
    }
    let Some(item) = StatuslineItem::ALL
        .into_iter()
        .find(|item| item.label() == requested)
    else {
        return vec![notice_event(usage(STATUSLINE_USAGE))];
    };
    let enabled = !toggles.enabled(item);
    toggles.set(item, enabled);
    let mut events = vec![UiEvent::StatuslineChanged { item, enabled }];
    let mut notices = Vec::new();
    let saved = match access.paths {
        Some(paths) => save_statusline_item(paths, item, enabled).map_err(Unsaved::Failed),
        None => Err(Unsaved::HomeNotSet),
    };
    match saved {
        Ok(outcome) => {
            report_user_settings_commit(
                access,
                STATUSLINE_TOPIC,
                &outcome,
                |settings| {
                    settings
                        .statusline_source(item)
                        .map(|source| (statusline_field(item), source))
                },
                true,
                &mut notices,
            );
        }
        Err(unsaved) => notices.push(unsaved_notice(STATUSLINE_TOPIC, &unsaved)),
    }
    notices.push(Notice::new(
        NoticeTone::Neutral,
        STATUSLINE_TOPIC,
        format!("{}: {}", item.label(), on_off(enabled)),
    ));
    events.extend(notices.into_iter().map(notice_event));
    events
}

fn statusline_status(toggles: StatuslineToggles) -> Notice {
    let body = StatuslineItem::ALL
        .map(|item| format!("{}: {}", item.label(), on_off(toggles.enabled(item))))
        .join("\n");
    Notice::new(NoticeTone::Neutral, STATUSLINE_TOPIC, body)
}

const fn statusline_field(item: StatuslineItem) -> &'static str {
    match item {
        StatuslineItem::Context => "statusLine.context",
        StatuslineItem::Session => "statusLine.session",
        StatuslineItem::Workspace => "statusLine.workspace",
    }
}

fn notice_event(notice: Notice) -> UiEvent {
    UiEvent::Notice { notice }
}

fn report_user_settings_commit(
    access: &SettingsAccess<'_>,
    label: &str,
    outcome: &CommitOutcome,
    shadow: impl Fn(&Settings) -> Option<(&'static str, ConfigSource)>,
    announce: bool,
    notices: &mut Vec<Notice>,
) -> Option<Settings> {
    let settings = match reload(access) {
        Ok(settings) => settings,
        Err(error) => {
            let mut body =
                format!("{SAVED_TO_USER_SETTINGS}; next-startup source unknown ({error})");
            append_commit_cleanup(&mut body, outcome);
            notices.push(Notice::new(NoticeTone::Warning, label, body));
            return None;
        }
    };
    let mut body = SAVED_TO_USER_SETTINGS.to_owned();
    if let Some((field, source)) = shadow(&settings)
        && source != ConfigSource::UserGlobal
    {
        let _ = write!(
            body,
            "; fresh sessions here use higher-precedence {field}={}",
            source.label()
        );
    }
    append_commit_cleanup(&mut body, outcome);
    if announce || body != SAVED_TO_USER_SETTINGS {
        notices.push(Notice::new(NoticeTone::Neutral, label, body));
    }
    Some(settings)
}

fn append_commit_cleanup(body: &mut String, outcome: &CommitOutcome) {
    if let CommitOutcome::Committed { cleanup, .. } = outcome {
        append_legacy_cleanup(body, cleanup);
    }
}

const fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

fn allowlist_add(access: &SettingsAccess<'_>, scope: PermissionScope, raw: &str) -> Notice {
    let Some(target) = parse_allowlist_target(&access.tool_names, raw) else {
        return usage(ADD_USAGE);
    };
    let patch = PermissionPatch::Add {
        category: &target.category,
        pattern: &target.pattern,
        action: PermissionAction::Allow,
    };
    match save(access, scope, patch) {
        Ok(_) => finish_allowlist_mutation(access, scope, "added", &target),
        Err(error) => allowlist_error(format!(
            "failed to add rule to settings (scope={}, error={error})",
            scope.label()
        )),
    }
}

fn allowlist_remove(access: &SettingsAccess<'_>, scope: PermissionScope, raw: &str) -> Notice {
    let Some(target) = parse_allowlist_target(&access.tool_names, raw) else {
        return usage(REMOVE_USAGE);
    };
    let patch = PermissionPatch::Remove {
        category: &target.category,
        pattern: &target.pattern,
    };
    match save(access, scope, patch) {
        Ok(CommitOutcome::Unchanged) => Notice::new(
            NoticeTone::Neutral,
            ALLOWLIST_TOPIC,
            format_allowlist_change("no matching rule for", &target, scope, ""),
        ),
        Ok(CommitOutcome::Committed { .. }) => {
            finish_allowlist_mutation(access, scope, "removed", &target)
        }
        Err(error) => allowlist_error(format!(
            "failed to remove rule from settings (scope={}, error={error})",
            scope.label()
        )),
    }
}

fn allowlist_reset(access: &SettingsAccess<'_>, scope: PermissionScope, raw: &str) -> Notice {
    let Some(reset_scope) = parse_allowlist_reset_scope(raw) else {
        return usage(RESET_USAGE);
    };
    let removed = match save(access, scope, PermissionPatch::Reset(reset_scope)) {
        Ok(CommitOutcome::Unchanged) => 0,
        Ok(CommitOutcome::Committed {
            permission_rules_removed,
            ..
        }) => permission_rules_removed,
        Err(error) => {
            return allowlist_error(format!(
                "failed to reset rules in settings (scope={}, error={error})",
                scope.label()
            ));
        }
    };
    let summary = format!(
        "reset {}: removed {removed} rule{} (scope={})",
        reset_scope_label(reset_scope),
        plural(removed),
        scope.label()
    );
    match reload(access) {
        Ok(settings) => {
            let shadow = permission_shadow_label(settings.permission_sources(), scope);
            let shadowed = if shadow.is_empty() {
                String::new()
            } else {
                format!("; user rules shadowed by {shadow}")
            };
            Notice::new(
                NoticeTone::Neutral,
                ALLOWLIST_TOPIC,
                format!("{summary}{shadowed}"),
            )
        }
        Err(error) => Notice::new(
            NoticeTone::Warning,
            ALLOWLIST_TOPIC,
            format!("{summary}; {RELOAD_FAILED} ({error})"),
        ),
    }
}

fn finish_allowlist_mutation(
    access: &SettingsAccess<'_>,
    scope: PermissionScope,
    verb: &str,
    target: &AllowlistTarget,
) -> Notice {
    match reload(access) {
        Ok(settings) => {
            let shadow = permission_shadow_label(settings.permission_sources(), scope);
            let detail = if shadow.is_empty() {
                String::new()
            } else {
                format!("user rules shadowed by {shadow}")
            };
            Notice::new(
                NoticeTone::Neutral,
                ALLOWLIST_TOPIC,
                format_allowlist_change(verb, target, scope, &detail),
            )
        }
        Err(error) => Notice::new(
            NoticeTone::Warning,
            ALLOWLIST_TOPIC,
            format_allowlist_change(verb, target, scope, &format!("{RELOAD_FAILED} ({error})")),
        ),
    }
}

fn allowlist_status(access: &SettingsAccess<'_>, view: AllowlistView) -> Notice {
    let settings = match load(access) {
        Ok(settings) => settings,
        Err(error) => return settings_load_error(&error),
    };
    if let Some(error) = settings.unsafe_path_failure() {
        return settings_load_error(error);
    }
    let sources = settings.permission_sources();
    let rules = match view {
        AllowlistView::Effective => settings.effective_permission_rules(),
        AllowlistView::Local => sources.local,
        AllowlistView::User => sources.user,
    };
    let mut body = String::new();
    if rules.iter().any(allowlist_rule_displayable) {
        let _ = writeln!(body, "{} persistent allow rules:", view.label());
        write_allowlist_rule_groups(&mut body, rules);
    } else {
        let _ = writeln!(body, "{} persistent allow rules: (none)", view.label());
    }
    let shadow = permission_shadow_label(sources, PermissionScope::User);
    if matches!(view, AllowlistView::Effective | AllowlistView::User) && !shadow.is_empty() {
        let _ = writeln!(body, "user rules are shadowed by {shadow}");
    }
    let warnings = web_fetch_rule_warning_count(rules);
    if warnings > 0 {
        let _ = writeln!(
            body,
            "ignored {warnings} malformed web_fetch rule{}; expected domain:<canonical-hostname>",
            plural(warnings)
        );
    }
    let tone = if warnings == 0 {
        NoticeTone::Neutral
    } else {
        NoticeTone::Warning
    };
    Notice::new(tone, ALLOWLIST_TOPIC, body.trim_end_matches('\n'))
}

fn save(
    access: &SettingsAccess<'_>,
    scope: PermissionScope,
    patch: PermissionPatch<'_>,
) -> Result<CommitOutcome, String> {
    let paths = access.paths.ok_or_else(|| HOME_NOT_SET.to_owned())?;
    let workspace_root = match scope {
        PermissionScope::User => None,
        PermissionScope::Local => Some(access.workspace_root),
    };
    save_permission_patch(paths, workspace_root, patch).map_err(|failure| failure.to_string())
}

fn load(access: &SettingsAccess<'_>) -> Result<Settings, String> {
    match access.paths {
        Some(paths) => {
            Settings::load(paths, access.workspace_root).map_err(|error| error.to_string())
        }
        None => Ok(Settings::default()),
    }
}

fn reload(access: &SettingsAccess<'_>) -> Result<Settings, String> {
    let settings = load(access)?;
    match settings.user_layer_failure() {
        Some(error) => Err(error.to_owned()),
        None => Ok(settings),
    }
}

fn usage(body: &str) -> Notice {
    Notice::new(NoticeTone::Error, "", body)
}

fn allowlist_error(body: String) -> Notice {
    Notice::new(NoticeTone::Error, ALLOWLIST_TOPIC, body)
}

fn settings_load_error(error: &str) -> Notice {
    Notice::new(
        NoticeTone::Error,
        SETTINGS_TOPIC,
        format!("Failed to load settings: {error}"),
    )
}

fn split_first_word(text: &str) -> Option<WordSplit<'_>> {
    let trimmed = text.trim_matches(SEPARATORS);
    if trimmed.is_empty() {
        return None;
    }
    Some(match trimmed.split_once(SEPARATORS) {
        Some((word, rest)) => WordSplit {
            word,
            rest: rest.trim_start_matches(SEPARATORS),
        },
        None => WordSplit {
            word: trimmed,
            rest: "",
        },
    })
}

fn parse_quoted_or_rest(raw: &str) -> &str {
    let trimmed = raw.trim_matches(SEPARATORS);
    match trimmed.strip_prefix('"') {
        Some(quoted) => quoted.split_once('"').map_or(quoted, |(inner, _)| inner),
        None => trimmed,
    }
}

fn parse_allowlist_target(tool_names: &[String], raw: &str) -> Option<AllowlistTarget> {
    let split = split_first_word(raw)?;
    let pattern = parse_quoted_or_rest(split.rest);
    if pattern.is_empty() {
        return None;
    }
    let target = |kind, category: &str, pattern: &str| AllowlistTarget {
        kind,
        category: category.to_owned(),
        pattern: pattern.to_owned(),
    };
    let kind = split.word;
    if kind.eq_ignore_ascii_case("web-fetch-domain") {
        let canonical = canonical_web_fetch_domain_pattern(pattern)?;
        return Some(target(
            AllowlistKind::WebFetchDomain,
            WEB_FETCH_PERMISSION,
            &canonical,
        ));
    }
    if kind.eq_ignore_ascii_case("command") {
        return Some(target(AllowlistKind::Command, "bash", pattern));
    }
    if kind.eq_ignore_ascii_case("url") {
        return Some(target(AllowlistKind::Url, "url", pattern));
    }
    if kind.eq_ignore_ascii_case("tool")
        && pattern != WEB_FETCH_PERMISSION
        && is_known_allowlist_tool(tool_names, pattern)
    {
        return Some(target(
            AllowlistKind::Tool,
            permission_name_for_tool(pattern),
            "*",
        ));
    }
    None
}

fn is_known_allowlist_tool(tool_names: &[String], name: &str) -> bool {
    tool_names.iter().any(|tool| tool == name) || KNOWN_PERMISSION_CATEGORIES.contains(&name)
}

fn parse_allowlist_reset_scope(raw: &str) -> Option<AllowlistResetScope> {
    let trimmed = raw.trim_matches(SEPARATORS);
    let is = |spellings: &[&str]| {
        spellings
            .iter()
            .any(|spelling| trimmed.eq_ignore_ascii_case(spelling))
    };
    if is(&["all"]) {
        Some(AllowlistResetScope::All)
    } else if is(&["command", "commands"]) {
        Some(AllowlistResetScope::Commands)
    } else if is(&["tool", "tools"]) {
        Some(AllowlistResetScope::Tools)
    } else if is(&["url", "urls"]) {
        Some(AllowlistResetScope::Urls)
    } else if is(&["web-fetch-domain", "web-fetch-domains"]) {
        Some(AllowlistResetScope::WebFetchDomains)
    } else {
        None
    }
}

fn parse_allowlist_view(raw: &str) -> Option<AllowlistView> {
    let trimmed = raw.trim_matches(SEPARATORS);
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("effective") {
        Some(AllowlistView::Effective)
    } else if trimmed.eq_ignore_ascii_case("local") {
        Some(AllowlistView::Local)
    } else if trimmed.eq_ignore_ascii_case("user") {
        Some(AllowlistView::User)
    } else {
        None
    }
}

fn permission_shadow_label(sources: PermissionSources<'_>, scope: PermissionScope) -> &'static str {
    if scope == PermissionScope::User && !sources.user.is_empty() && sources.user_shadowed_by_local
    {
        "local settings"
    } else {
        ""
    }
}

const fn reset_scope_label(scope: AllowlistResetScope) -> &'static str {
    match scope {
        AllowlistResetScope::All => "all",
        AllowlistResetScope::Commands => "commands",
        AllowlistResetScope::Tools => "tools",
        AllowlistResetScope::Urls => "urls",
        AllowlistResetScope::WebFetchDomains => "web-fetch-domains",
    }
}

fn format_allowlist_change(
    verb: &str,
    target: &AllowlistTarget,
    scope: PermissionScope,
    detail: &str,
) -> String {
    let mut out = format!("{verb} ");
    let pattern = &target.pattern;
    let _ = match target.kind {
        AllowlistKind::Command => write!(out, "command: \"{pattern}\""),
        AllowlistKind::Url => write!(out, "url: \"{pattern}\""),
        AllowlistKind::Tool => write!(out, "tool {}: \"{pattern}\"", target.category),
        AllowlistKind::WebFetchDomain => write!(out, "web-fetch-domain: \"{pattern}\""),
    };
    let _ = write!(out, " (scope={})", scope.label());
    if !detail.is_empty() {
        let _ = write!(out, "; {detail}");
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayKind {
    Tool,
    Command,
    Url,
    WebFetchDomain,
}

fn allowlist_rule_group(rule: &PermissionRule) -> (DisplayKind, &str) {
    match rule.permission.as_str() {
        "bash" => (DisplayKind::Command, "command"),
        "url" => (DisplayKind::Url, "url"),
        WEB_FETCH_PERMISSION => (DisplayKind::WebFetchDomain, "web-fetch-domain"),
        permission => (DisplayKind::Tool, permission),
    }
}

fn allowlist_rule_displayable(rule: &PermissionRule) -> bool {
    rule.action == PermissionAction::Allow
        && (rule.permission != WEB_FETCH_PERMISSION
            || is_canonical_web_fetch_domain_pattern(&rule.pattern))
}

fn write_allowlist_rule_groups(out: &mut String, rules: &[PermissionRule]) {
    let displayable: Vec<&PermissionRule> = rules
        .iter()
        .filter(|rule| allowlist_rule_displayable(rule))
        .collect();
    for (kind, heading) in [
        (DisplayKind::Tool, "tools"),
        (DisplayKind::Command, "commands"),
        (DisplayKind::Url, "urls"),
        (DisplayKind::WebFetchDomain, "web-fetch domains"),
    ] {
        let mut groups: Vec<&str> = Vec::new();
        for rule in &displayable {
            let (rule_kind, name) = allowlist_rule_group(rule);
            if rule_kind == kind && !groups.contains(&name) {
                groups.push(name);
            }
        }
        if groups.is_empty() {
            continue;
        }
        let _ = writeln!(out, "  {heading}:");
        for name in groups {
            out.push_str("    ");
            if kind == DisplayKind::Tool {
                let _ = write!(out, "{name}: ");
            }
            let patterns: Vec<&str> = displayable
                .iter()
                .filter(|rule| allowlist_rule_group(rule) == (kind, name))
                .map(|rule| display_pattern(kind, name, &rule.pattern))
                .collect();
            out.push_str(&patterns.join(", "));
            out.push('\n');
        }
    }
}

fn display_pattern<'a>(kind: DisplayKind, name: &str, pattern: &'a str) -> &'a str {
    if kind == DisplayKind::Tool && pattern == "*" && WORKSPACE_PATH_PERMISSIONS.contains(&name) {
        "workspace"
    } else {
        pattern
    }
}

const fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests;
