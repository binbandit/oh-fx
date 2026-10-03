use ofx_contract::{SkillMenuFocus, SkillMenuItem};

use super::Shell;
use super::skills_menu::SkillsMenu;
use crate::composer::InsertResult;
use crate::footer::skills_menu_presentation::{MAX_MENU_ROWS, visible_item_rows};
use crate::input::COMPOSER_INPUT_LIMIT_BYTES;

impl Shell<'_> {
    pub(super) fn open_skills_menu(&mut self, items: Vec<SkillMenuItem>, focus: &SkillMenuFocus) {
        let Some(menu) = SkillsMenu::open(items, focus) else {
            return;
        };
        self.close_model_menu_for_skills();
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

    pub(super) fn move_skills_menu(&mut self, delta: isize) -> bool {
        let budget = self.menu_budget(MAX_MENU_ROWS);
        let Some(menu) = &mut self.skills_menu else {
            return false;
        };
        let rows = visible_item_rows(menu, budget);
        menu.move_selection(delta, rows);
        true
    }

    pub(super) fn cycle_skills_menu_source(&mut self, delta: isize) -> bool {
        let budget = self.menu_budget(MAX_MENU_ROWS);
        let Some(menu) = &mut self.skills_menu else {
            return false;
        };
        let rows = visible_item_rows(menu, budget);
        menu.cycle_filter(delta, rows);
        true
    }

    pub(super) fn cancel_skills_menu(&mut self) -> bool {
        if self.skills_menu.take().is_none() {
            return false;
        }
        self.composer.clear();
        true
    }

    pub(super) fn submit_skills_menu_selection(&mut self) -> bool {
        let Some(menu) = &self.skills_menu else {
            return false;
        };
        let Some(item) = menu.selected_item() else {
            return true;
        };
        let (name, path) = (item.name.clone(), item.path.clone());
        self.skills_menu = None;
        let end = self.composer.text().len();
        let inserted = self.composer.skill_token_inserted_len(end, &name);
        if !self
            .composer
            .can_replace_range(0, end, inserted, COMPOSER_INPUT_LIMIT_BYTES)
        {
            self.report_limit();
            return true;
        }
        self.composer.bind_skill_token(0, end, &name, &path);
        true
    }

    pub(super) fn sync_skills_menu(&mut self) {
        if self.skills_menu.is_none() {
            return;
        }
        let budget = self.menu_budget(MAX_MENU_ROWS);
        let query = self.composer.text().to_owned();
        if let Some(menu) = &mut self.skills_menu {
            let rows = visible_item_rows(menu, budget);
            menu.set_query(&query, rows);
        }
    }
}

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
