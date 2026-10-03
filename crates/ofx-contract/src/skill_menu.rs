use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillMenuSource {
    OhFx,
    Workspace,
    OpenCode,
    Codex,
    Claude,
    Agents,
    Claw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SkillMenuGroup {
    Managed,
    Workspace,
    Compatibility,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMenuItem {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub source: SkillMenuSource,
    pub group: SkillMenuGroup,
    pub scope: String,
    pub source_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillMenuFocus {
    Start,
    Query(String),
    Item(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillBinding {
    pub name: String,
    pub path: PathBuf,
}
