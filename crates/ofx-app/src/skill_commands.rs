use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use ofx_contract::{
    NoticeTone, SkillMenuFocus, SkillMenuGroup, SkillMenuItem, SkillMenuSource, UiEvent,
};
use ofx_skills::{Skill, SkillDiscovery, SkillSource, diagnostic_summary, install_local};
use rustix::fs::{Mode, OFlags, mkdirat, openat};
use rustix::io::Errno;

use crate::app_agent_runtime::ControllerState;

const TOPIC: &str = "skills";
const TRIMMED: [char; 2] = [' ', '\t'];
const USAGE: &str = "usage: /skills [list|add|install|show|create|remove|path] [name|url|path]";
const INVALID_NAME: &str = "Invalid skill name. Use a single directory name without '/' or '\\'.";
const MAX_NAME_BYTES: usize = 256;
const SKILL_FILE: &str = "SKILL.md";
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);
const NEW_FILE_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command<'a> {
    List,
    Show(&'a str),
    Install {
        source: &'a str,
        filter: Option<&'a str>,
    },
    Create(&'a str),
    Remove(&'a str),
    Path,
    Usage,
}

fn parse(rest: &str) -> Command<'_> {
    let trimmed = rest.trim_matches(TRIMMED);
    if trimmed.is_empty() || trimmed == "list" {
        return Command::List;
    }
    if let Some(name) = trimmed.strip_prefix("show ") {
        return Command::Show(name.trim_matches(TRIMMED));
    }
    if let Some(source) = trimmed
        .strip_prefix("add ")
        .or_else(|| trimmed.strip_prefix("install "))
    {
        return parse_install(source.trim_matches(TRIMMED));
    }
    if let Some(name) = trimmed.strip_prefix("create ") {
        return Command::Create(name.trim_matches(TRIMMED));
    }
    if let Some(name) = trimmed.strip_prefix("remove ") {
        return Command::Remove(name.trim_matches(TRIMMED));
    }
    if trimmed == "path" {
        return Command::Path;
    }
    Command::Usage
}

fn parse_install(arguments: &str) -> Command<'_> {
    ["--skill ", "--skill="]
        .iter()
        .find_map(|flag| arguments.find(flag))
        .map_or(
            Command::Install {
                source: arguments,
                filter: None,
            },
            |index| Command::Install {
                source: arguments[..index].trim_matches(TRIMMED),
                filter: Some(arguments[index + 8..].trim_matches(TRIMMED)),
            },
        )
}

pub(crate) fn handle_skills(state: &ControllerState, rest: &str) {
    let command = parse(rest);
    let skills = state.skills();
    if matches!(
        command,
        Command::List | Command::Show(_) | Command::Remove(_)
    ) {
        skills.refresh();
    }
    let found = skills.current();
    if let Some(summary) = diagnostic_summary(&found.diagnostics)
        && state.claim_context_notice(&summary)
    {
        state.notice(NoticeTone::Warning, TOPIC, &summary);
    }
    match command {
        Command::List => open_menu(state, &found, SkillMenuFocus::Start),
        Command::Show(name) => show(state, &found, name),
        Command::Install { source, filter } => install(state, source, filter),
        Command::Create(name) => create(state, name),
        Command::Remove(name) => remove(state, &found, name),
        Command::Path => state.notice(
            NoticeTone::Neutral,
            TOPIC,
            &path_notice(skills.managed_root()),
        ),
        Command::Usage => state.notice(NoticeTone::Neutral, "", USAGE),
    }
}

fn install(state: &ControllerState, source: &str, filter: Option<&str>) {
    state.notice(
        NoticeTone::Neutral,
        TOPIC,
        &format!("Installing from {source}..."),
    );
    let skills = state.skills();
    let notice = match install_local(
        skills.managed_root(),
        Path::new(source.trim_matches([' ', '\t', '\r', '\n'])),
        filter,
    ) {
        Ok(result) if result.installed.is_empty() => filter.map_or_else(
            || "No skills found (no SKILL.md files).".to_owned(),
            |filter| format!("Skill '{filter}' not found in the repository."),
        ),
        Ok(result) => {
            skills.refresh();
            result
                .installed
                .iter()
                .map(|name| format!("Installed: {name}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
        Err(_) => "Failed to install. Check the source path or URL and try again.".to_owned(),
    };
    state.notice(NoticeTone::Neutral, TOPIC, &notice);
}

fn open_menu(state: &ControllerState, found: &SkillDiscovery, focus: SkillMenuFocus) {
    state.emit(UiEvent::SkillsMenu {
        items: found.skills.iter().map(menu_item).collect(),
        focus,
    });
}

fn show(state: &ControllerState, found: &SkillDiscovery, name: &str) {
    let mut named = found
        .skills
        .iter()
        .enumerate()
        .filter(|(_, skill)| skill.name == name)
        .map(|(index, _)| index);
    match (named.next(), named.next()) {
        (Some(index), None) => open_menu(state, found, SkillMenuFocus::Item(index)),
        (Some(_), Some(_)) => open_menu(state, found, SkillMenuFocus::Query(name.to_owned())),
        (None, _) => not_found(state, name),
    }
}

fn not_found(state: &ControllerState, name: &str) {
    state.notice(
        NoticeTone::Neutral,
        TOPIC,
        &format!("Skill '{name}' not found."),
    );
}

fn create(state: &ControllerState, name: &str) {
    if !valid_managed_name(name) {
        state.notice(NoticeTone::Neutral, TOPIC, INVALID_NAME);
        return;
    }
    let skills = state.skills();
    match create_template(skills.managed_root(), name) {
        Ok(path) => {
            skills.refresh();
            state.notice(
                NoticeTone::Neutral,
                TOPIC,
                &format!("Created {}", path.display()),
            );
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => state.notice(
            NoticeTone::Error,
            TOPIC,
            &format!(
                "Failed to create skill '{name}': {} already exists",
                skills.managed_root().join(name).join(SKILL_FILE).display()
            ),
        ),
        Err(error) => state.notice(
            NoticeTone::Error,
            TOPIC,
            &format!("Failed to create skill '{name}': {error}"),
        ),
    }
}

fn create_template(managed_root: &Path, name: &str) -> io::Result<PathBuf> {
    let (Some(parent), Some(root_name), true) = (
        managed_root.parent(),
        managed_root.file_name(),
        managed_root.is_absolute(),
    ) else {
        return Err(io::ErrorKind::NotFound.into());
    };
    fs::create_dir_all(parent)?;
    let parent = rustix::fs::open(parent, DIRECTORY_FLAGS, Mode::empty())?;
    let root = child_directory(&parent, root_name)?;
    let directory = child_directory(&root, OsStr::new(name))?;
    let file = openat(
        &directory,
        SKILL_FILE,
        NEW_FILE_FLAGS,
        Mode::from_raw_mode(0o666),
    )?;
    File::from(file).write_all(
        format!(
            "---\nname: {name}\ndescription: Describe when this skill should activate\n---\n\n# {name}\n\nInstructions for this skill...\n"
        )
        .as_bytes(),
    )?;
    Ok(managed_root.join(name).join(SKILL_FILE))
}

fn child_directory(parent: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    match mkdirat(parent, name, Mode::from_raw_mode(0o755)) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(openat(
        parent,
        name,
        DIRECTORY_FLAGS.union(OFlags::NOFOLLOW),
        Mode::empty(),
    )?)
}

fn remove(state: &ControllerState, found: &SkillDiscovery, name: &str) {
    let named = || found.skills.iter().filter(|skill| skill.name == name);
    let Some(skill) = named()
        .find(|skill| skill.source == SkillSource::GlobalOhFx)
        .or_else(|| named().next())
    else {
        not_found(state, name);
        return;
    };
    if skill.source != SkillSource::GlobalOhFx {
        state.notice(
            NoticeTone::Neutral,
            TOPIC,
            &format!(
                "Skill '{name}' comes from {}, not the oh-fx managed install root. Remove it from {}.",
                source_label(skill.source),
                skill.path.display()
            ),
        );
        return;
    }
    let skills = state.skills();
    if remove_managed(skills.managed_root(), skill).is_err() {
        state.notice(
            NoticeTone::Neutral,
            TOPIC,
            &format!("Failed to remove skill '{name}'."),
        );
        return;
    }
    skills.refresh();
    state.notice(
        NoticeTone::Neutral,
        TOPIC,
        &format!("Removed skill '{name}'."),
    );
}

fn remove_managed(managed_root: &Path, skill: &Skill) -> io::Result<()> {
    let leaf = skill
        .path
        .file_name()
        .and_then(|leaf| leaf.to_str())
        .filter(|leaf| valid_managed_name(leaf))
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    fs::remove_dir_all(managed_directory(managed_root, leaf)?)
}

fn managed_directory(managed_root: &Path, name: &str) -> io::Result<PathBuf> {
    if managed_root.is_absolute() {
        Ok(managed_root.join(name))
    } else {
        Err(io::ErrorKind::NotFound.into())
    }
}

fn valid_managed_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
}

fn path_notice(managed_root: &Path) -> String {
    format!(
        "oh-fx workspace roots are auto-discovered from .oh-fx/skills and skills/.\noh-fx managed install root: {}\ncompatibility roots are auto-discovered from workspace and home (.opencode/.codex/.claude/.agents/.claw).",
        managed_root.display()
    )
}

fn menu_item(skill: &Skill) -> SkillMenuItem {
    SkillMenuItem {
        name: skill.name.clone(),
        description: skill.description.clone(),
        path: skill.path.clone(),
        source: menu_source(skill.source),
        group: menu_group(skill.source),
        scope: scope_label(skill.source).to_owned(),
        source_label: short_source_label(skill.source).to_owned(),
    }
}

fn menu_source(source: SkillSource) -> SkillMenuSource {
    match source {
        SkillSource::GlobalOhFx | SkillSource::WorkspaceOhFx => SkillMenuSource::OhFx,
        SkillSource::WorkspaceShared => SkillMenuSource::Workspace,
        SkillSource::WorkspaceOpencode | SkillSource::GlobalOpencode => SkillMenuSource::OpenCode,
        SkillSource::WorkspaceCodex | SkillSource::GlobalCodex => SkillMenuSource::Codex,
        SkillSource::WorkspaceClaude | SkillSource::GlobalClaude => SkillMenuSource::Claude,
        SkillSource::WorkspaceAgents | SkillSource::GlobalAgents => SkillMenuSource::Agents,
        SkillSource::WorkspaceClaw | SkillSource::GlobalClaw => SkillMenuSource::Claw,
    }
}

fn menu_group(source: SkillSource) -> SkillMenuGroup {
    match source {
        SkillSource::GlobalOhFx => SkillMenuGroup::Managed,
        SkillSource::WorkspaceOhFx | SkillSource::WorkspaceShared => SkillMenuGroup::Workspace,
        _ => SkillMenuGroup::Compatibility,
    }
}

fn scope_label(source: SkillSource) -> &'static str {
    match source {
        SkillSource::GlobalOhFx => "oh-fx · Global",
        SkillSource::WorkspaceOhFx | SkillSource::WorkspaceShared => "oh-fx · Workspace",
        SkillSource::WorkspaceOpencode => "OpenCode · Workspace",
        SkillSource::GlobalOpencode => "OpenCode · Global",
        SkillSource::WorkspaceCodex => "Codex · Workspace",
        SkillSource::GlobalCodex => "Codex · Global",
        SkillSource::WorkspaceClaude => "Claude · Workspace",
        SkillSource::GlobalClaude => "Claude · Global",
        SkillSource::WorkspaceAgents => "Agents · Workspace",
        SkillSource::GlobalAgents => "Agents · Global",
        SkillSource::WorkspaceClaw => "Claw · Workspace",
        SkillSource::GlobalClaw => "Claw · Global",
    }
}

fn source_label(source: SkillSource) -> &'static str {
    match source {
        SkillSource::WorkspaceOhFx => "workspace .oh-fx/skills",
        SkillSource::WorkspaceShared => "workspace skills/",
        SkillSource::WorkspaceOpencode => "workspace .opencode/skills",
        SkillSource::WorkspaceCodex => "workspace .codex/skills",
        SkillSource::WorkspaceClaude => "workspace .claude/skills",
        SkillSource::WorkspaceAgents => "workspace .agents/skills",
        SkillSource::WorkspaceClaw => "workspace .claw/skills",
        SkillSource::GlobalOhFx => "global ~/.config/oh-fx/skills",
        SkillSource::GlobalOpencode => "global ~/.config/opencode/skills",
        SkillSource::GlobalCodex => "global ~/.codex/skills",
        SkillSource::GlobalClaude => "global ~/.claude/skills",
        SkillSource::GlobalAgents => "global ~/.agents/skills",
        SkillSource::GlobalClaw => "global ~/.claw/skills",
    }
}

fn short_source_label(source: SkillSource) -> &'static str {
    match source {
        SkillSource::WorkspaceOhFx => "workspace .oh-fx",
        SkillSource::WorkspaceShared => "workspace skills/",
        SkillSource::WorkspaceOpencode => "workspace .opencode",
        SkillSource::WorkspaceCodex => "workspace .codex",
        SkillSource::WorkspaceClaude => "workspace .claude",
        SkillSource::WorkspaceAgents => "workspace .agents",
        SkillSource::WorkspaceClaw => "workspace .claw",
        SkillSource::GlobalOhFx => "global oh-fx",
        SkillSource::GlobalOpencode => "global opencode",
        SkillSource::GlobalCodex => "global .codex",
        SkillSource::GlobalClaude => "global .claude",
        SkillSource::GlobalAgents => "global .agents",
        SkillSource::GlobalClaw => "global .claw",
    }
}

#[cfg(test)]
mod tests;
