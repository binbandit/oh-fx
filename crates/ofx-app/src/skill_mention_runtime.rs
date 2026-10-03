use ofx_contract::SkillMenuItem;
use ofx_tui::SkillCatalogSource;

use crate::skill_commands::menu_item;
use crate::skills::HostSkills;

pub(crate) struct SkillMentions(HostSkills);

impl SkillMentions {
    pub(crate) fn new(skills: HostSkills) -> Self {
        Self(skills)
    }
}

impl SkillCatalogSource for SkillMentions {
    fn menu_items(&self) -> Vec<SkillMenuItem> {
        self.0.current().skills.iter().map(menu_item).collect()
    }
}
