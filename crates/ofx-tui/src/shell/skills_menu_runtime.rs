use ofx_contract::{SkillMenuFocus, SkillMenuItem};

use super::Shell;
use super::skills_menu::{MentionAnchor, SkillsMenu};
use crate::composer::InsertResult;
use crate::composer::file_picker_path::contains_position;
use crate::footer::input_presentation::input_row_limit;
use crate::footer::skills_menu_presentation::{menu_row_budget, visible_item_rows};
use crate::input::COMPOSER_INPUT_LIMIT_BYTES;

pub trait SkillCatalogSource {
    fn menu_items(&self) -> Vec<SkillMenuItem>;
}

impl Shell<'_> {
    pub(super) fn open_skills_menu(&mut self, items: Vec<SkillMenuItem>, focus: &SkillMenuFocus) {
        let Some(menu) = SkillsMenu::open(items, focus) else {
            return;
        };
        if let SkillMenuFocus::Query(query) = focus {
            self.composer.clear();
            if self.composer.insert_text(query, COMPOSER_INPUT_LIMIT_BYTES)
                == InsertResult::LimitExceeded
            {
                self.report_limit();
            }
        }
        self.skills_menu = Some(menu);
        self.invalidate();
    }

    pub(super) fn skills_menu_budget(&self) -> usize {
        let composer_rows = self
            .composer
            .visual_layout(self.layout.cols)
            .summary(None)
            .total_rows
            .min(input_row_limit(usize::from(self.layout.content_bottom)));
        menu_row_budget(
            usize::from(self.layout.rows),
            composer_rows.saturating_sub(1),
            self.banner_rows().len(),
        )
    }

    pub(super) fn move_skills_menu(&mut self, delta: isize) -> bool {
        let budget = self.skills_menu_budget();
        let Some(menu) = self.visible_skills_menu_mut() else {
            return false;
        };
        let rows = visible_item_rows(menu, budget);
        let moved = menu.move_selection(delta, rows);
        moved || menu.mention_anchor().is_none()
    }

    pub(super) fn cycle_skills_menu_source(&mut self, delta: isize) -> bool {
        let budget = self.skills_menu_budget();
        let Some(menu) = self.visible_skills_menu_mut() else {
            return false;
        };
        let rows = visible_item_rows(menu, budget);
        menu.cycle_filter(delta, rows);
        true
    }

    pub(super) fn cancel_skills_menu(&mut self) -> bool {
        let Some(menu) = &self.skills_menu else {
            return false;
        };
        if menu.mention_anchor().is_some() {
            if !menu.is_visible() {
                return false;
            }
            self.skills_menu = None;
            return true;
        }
        self.skills_menu = None;
        self.composer.clear();
        true
    }

    pub(super) fn submit_skills_menu_selection(&mut self) -> bool {
        let Some(menu) = &self.skills_menu else {
            return false;
        };
        let anchor = menu.mention_anchor();
        let Some(item) = menu.selected_item() else {
            if anchor.is_some() {
                self.skills_menu = None;
                return false;
            }
            return true;
        };
        let (name, path) = (item.name.clone(), item.path.clone());
        self.skills_menu = None;
        let (start, end) = anchor.map_or((0, self.composer.text().len()), |anchor| {
            (anchor.start, anchor.end)
        });
        let inserted = self.composer.skill_token_inserted_len(end, &name);
        if !self
            .composer
            .can_replace_range(start, end, inserted, COMPOSER_INPUT_LIMIT_BYTES)
        {
            self.report_limit();
            return true;
        }
        self.composer.bind_skill_token(start, end, &name, &path);
        true
    }

    pub(super) fn sync_skills_menu(&mut self) {
        let Some(menu) = &self.skills_menu else {
            return;
        };
        let budget = self.skills_menu_budget();
        let text = self.composer.text();
        let query = match menu.mention_anchor() {
            None => text.to_owned(),
            Some(anchor) if text.as_bytes().get(anchor.start) == Some(&b'$') => {
                let end = skill_token_end(text, anchor.start + 1);
                let query = text[anchor.start + 1..end].to_owned();
                if let Some(menu) = &mut self.skills_menu {
                    menu.set_mention_end(end);
                }
                query
            }
            Some(_) => {
                self.skills_menu = None;
                return;
            }
        };
        if let Some(menu) = &mut self.skills_menu {
            let rows = visible_item_rows(menu, budget);
            menu.set_query(&query, rows);
        }
    }

    pub(super) fn skills_menu_visible(&self) -> bool {
        self.skills_menu
            .as_ref()
            .is_some_and(SkillsMenu::is_visible)
    }

    pub(super) fn command_skills_menu_open(&self) -> bool {
        self.skills_menu
            .as_ref()
            .is_some_and(|menu| menu.mention_anchor().is_none())
    }

    pub(super) fn insert_dollar(&mut self) {
        let start = self
            .composer
            .selection()
            .map_or(self.composer.cursor(), |selection| selection.start);
        let mentions = self.skill_catalog.is_some()
            && !self.command_skills_menu_open()
            && !self.picker_active()
            && !contains_position(self.composer.text(), start);
        match self.composer.insert_text("$", COMPOSER_INPUT_LIMIT_BYTES) {
            InsertResult::LimitExceeded => self.report_limit(),
            InsertResult::Inserted
                if mentions && self.composer.text().as_bytes().get(start) == Some(&b'$') =>
            {
                self.open_skill_mention(start);
            }
            _ => {}
        }
    }

    pub(super) fn close_skill_mention(&mut self) {
        if self
            .skills_menu
            .as_ref()
            .is_some_and(|menu| menu.mention_anchor().is_some())
        {
            self.skills_menu = None;
        }
    }

    pub(super) fn open_pasted_skill_mention(&mut self, start: usize, pasted: &str) {
        if self.skills_menu.is_some() || self.skill_catalog.is_none() {
            return;
        }
        let text = self.composer.text();
        if text.get(start..start + pasted.len()) != Some(pasted) {
            return;
        }
        let Some(source) = &self.skill_catalog else {
            return;
        };
        let items = source.menu_items();
        let found = (start..start + pasted.len())
            .filter(|&index| text.as_bytes()[index] == b'$' && !contains_position(text, index))
            .find_map(|index| {
                let end = skill_token_end(text, index + 1);
                let menu = SkillsMenu::mention(
                    items.clone(),
                    MentionAnchor { start: index, end },
                    &text[index + 1..end],
                );
                (end > index + 1 && menu.is_visible()).then_some(menu)
            });
        if let Some(menu) = found {
            self.skills_menu = Some(menu);
            self.invalidate();
        }
    }

    fn open_skill_mention(&mut self, start: usize) {
        let Some(source) = &self.skill_catalog else {
            return;
        };
        let anchor = MentionAnchor {
            start,
            end: start + 1,
        };
        let menu = SkillsMenu::mention(source.menu_items(), anchor, "");
        self.skills_menu = Some(menu);
        self.invalidate();
    }

    fn visible_skills_menu_mut(&mut self) -> Option<&mut SkillsMenu> {
        self.skills_menu.as_mut().filter(|menu| menu.is_visible())
    }
}

fn skill_token_end(text: &str, start: usize) -> usize {
    text.as_bytes()
        .get(start..)
        .and_then(|rest| {
            rest.iter().position(|byte| {
                !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
            })
        })
        .map_or(text.len(), |offset| start + offset)
}

#[cfg(test)]
mod mention_tests;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ofx_contract::{
        SkillBinding, SkillMenuFocus, SkillMenuGroup, SkillMenuItem, SkillMenuSource, UiCommand,
        UiEvent,
    };

    use crate::shell::test_shell::TestShell;

    const HEADER: &str = "Skills 2  [All]  oh-fx  Workspace  Claude  Codex  Agents  OpenCode  Claw";
    const HINT: &str = "↑↓ navigate     tab source     enter use     esc close";

    fn item(
        name: &str,
        source: SkillMenuSource,
        group: SkillMenuGroup,
        scope: &str,
    ) -> SkillMenuItem {
        SkillMenuItem {
            name: name.to_owned(),
            description: format!("{name} workflow"),
            path: PathBuf::from(format!("/skills/{name}")),
            source,
            group,
            scope: scope.to_owned(),
            source_label: String::new(),
        }
    }

    fn open(test: &mut TestShell, focus: SkillMenuFocus) {
        test.deliver(UiEvent::SkillsMenu {
            items: vec![
                item(
                    "review",
                    SkillMenuSource::OhFx,
                    SkillMenuGroup::Workspace,
                    "oh-fx · Workspace",
                ),
                item(
                    "deploy",
                    SkillMenuSource::Claude,
                    SkillMenuGroup::Compatibility,
                    "Claude · Global",
                ),
            ],
            focus,
        });
    }

    fn press(test: &mut TestShell, bytes: &[u8]) -> String {
        test.type_bytes(bytes);
        test.step();
        test.screen()
    }

    #[test]
    fn the_menu_sits_under_the_composer_and_enter_binds_the_chosen_skill() {
        let mut test = TestShell::start();
        open(&mut test, SkillMenuFocus::Start);
        let screen = test.screen();
        let composer = screen.find("┃ ").unwrap();
        let header = screen.find(HEADER).unwrap();
        let review = screen.find("  review    oh-fx · Workspace").unwrap();
        let deploy = screen.find("  deploy    Claude · Global").unwrap();
        let hint = screen.find(HINT).unwrap();
        assert!(composer < header && header < review && review < deploy && deploy < hint);
        assert!(screen.contains(&format!("┃ \n\n{HEADER}\n\n")), "{screen}");
        assert!(!screen.contains("auto · model-a"), "{screen}");
        press(&mut test, b"\x1b[B\x1b[B\x1b[A\x0e\x0b\n\x1b[5~\x1b[6~");
        assert_eq!(test.shell.skills_menu.as_ref().unwrap().selected(), 1);
        assert!(test.shell.composer.is_empty());
        let screen = press(&mut test, b"\r");
        assert!(test.shell.skills_menu.is_none());
        assert_eq!(test.shell.composer.text(), "$deploy ");
        assert!(screen.contains("┃ deploy "), "{screen}");
        assert!(!screen.contains("$deploy"), "{screen}");
        assert!(screen.contains("auto · model-a"), "{screen}");
        press(&mut test, b"ship it\r");
        assert_eq!(
            test.sent(),
            [UiCommand::Submit {
                prompt: "$deploy ship it".to_owned(),
                skills: vec![SkillBinding {
                    name: "deploy".to_owned(),
                    path: PathBuf::from("/skills/deploy"),
                }],
            }]
        );
    }

    #[test]
    fn typing_filters_the_menu_and_tab_switches_the_source() {
        let mut test = TestShell::start();
        open(&mut test, SkillMenuFocus::Start);
        let screen = press(&mut test, b"dep");
        assert!(screen.contains("Skills 1  [All]"), "{screen}");
        assert!(!screen.contains("  review"), "{screen}");
        let screen = press(&mut test, b"\x7f\x7f\x7f\t");
        assert!(screen.contains("Skills 1  All  [oh-fx]"), "{screen}");
        assert!(screen.contains("  review"), "{screen}");
        let screen = press(&mut test, b"\x1b[Z");
        assert!(screen.contains(HEADER), "{screen}");
        assert!(test.sent().is_empty());
        let screen = press(&mut test, b"zzz\r");
        assert!(screen.contains("No skills found."), "{screen}");
        assert!(test.shell.skills_menu.is_some());
        assert_eq!(test.shell.composer.text(), "zzz");
        assert!(test.sent().is_empty());
    }

    #[test]
    fn escape_closes_the_menu_and_clears_its_query() {
        let mut test = TestShell::start();
        open(&mut test, SkillMenuFocus::Start);
        press(&mut test, b"rev\x1b");
        test.advance(40);
        test.draining(|shell| shell.flush_pending_input().unwrap());
        assert!(test.shell.skills_menu.is_none());
        assert!(test.shell.composer.is_empty());
        assert!(!test.shell.gestures.escape_clear_armed());
        let screen = test.screen();
        assert!(!screen.contains("Skills"), "{screen}");
        assert!(screen.contains("auto · model-a"), "{screen}");
    }

    #[test]
    fn super_r_leaves_an_open_skills_menu_alone() {
        let mut test = TestShell::start();
        open(&mut test, SkillMenuFocus::Start);
        let screen = press(&mut test, b"\x1b[114;9u");
        assert!(test.sent().is_empty());
        assert!(test.shell.skills_menu.is_some());
        assert!(screen.contains(HEADER), "{screen}");
        let screen = press(&mut test, b"rev\x1b[114;9u");
        assert!(test.sent().is_empty());
        assert!(!screen.contains("switching sessions"), "{screen}");
    }

    #[test]
    fn show_focuses_a_skill_on_its_source_tab_or_filters_an_ambiguous_name() {
        let mut test = TestShell::start();
        open(&mut test, SkillMenuFocus::Item(1));
        let screen = test.screen();
        assert!(
            screen.contains("Skills 1  All  oh-fx  Workspace  [Claude]"),
            "{screen}"
        );
        press(&mut test, b"\r");
        assert_eq!(test.shell.composer.text(), "$deploy ");
        let mut test = TestShell::start();
        press(&mut test, b"draft");
        open(&mut test, SkillMenuFocus::Query("review".to_owned()));
        assert_eq!(test.shell.composer.text(), "review");
        let screen = test.screen();
        assert!(screen.contains("┃ review"), "{screen}");
        assert!(screen.contains("Skills 1  [All]"), "{screen}");
    }
}
