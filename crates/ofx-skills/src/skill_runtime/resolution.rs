use std::path::Path;

use crate::skill_contract::Skill;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillResolution<'a> {
    Found(&'a Skill),
    NotFound,
    AmbiguousName,
    NameLocationMismatch,
}

pub(crate) fn resolve_skill<'a>(
    skills: &'a [Skill],
    name: &str,
    location: Option<&Path>,
) -> SkillResolution<'a> {
    if let Some(location) = location {
        return find_skill_at(skills, location).map_or(SkillResolution::NotFound, |skill| {
            if skill.name == name {
                SkillResolution::Found(skill)
            } else {
                SkillResolution::NameLocationMismatch
            }
        });
    }
    let mut matches = skills.iter().filter(|skill| skill.name == name);
    match (matches.next(), matches.next()) {
        (Some(skill), None) => SkillResolution::Found(skill),
        (Some(_), Some(_)) => SkillResolution::AmbiguousName,
        (None, _) => SkillResolution::NotFound,
    }
}

pub(crate) fn find_skill_at<'a>(skills: &'a [Skill], location: &Path) -> Option<&'a Skill> {
    skills
        .iter()
        .find(|skill| skill.path.as_os_str() == location.as_os_str())
}
