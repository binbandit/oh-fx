use std::path::PathBuf;

use ofx_contract::{SkillBinding, SkillMenuGroup, SkillMenuItem, SkillMenuSource, UiCommand};

use crate::shell::SkillCatalogSource;
use crate::shell::test_shell::TestShell;

const HEADER: &str = "Skills 2  [All]  oh-fx  Workspace  Claude  Codex  Agents  OpenCode  Claw";

struct FixedCatalog(Vec<SkillMenuItem>);

impl SkillCatalogSource for FixedCatalog {
    fn menu_items(&self) -> Vec<SkillMenuItem> {
        self.0.clone()
    }
}

fn item(name: &str, source: SkillMenuSource, scope: &str) -> SkillMenuItem {
    SkillMenuItem {
        name: name.to_owned(),
        description: format!("{name} workflow"),
        path: PathBuf::from(format!("/skills/{}/{name}", scope.replace(' ', ""))),
        source,
        group: SkillMenuGroup::Workspace,
        scope: scope.to_owned(),
        source_label: String::new(),
    }
}

fn catalog() -> Vec<SkillMenuItem> {
    vec![
        item("review", SkillMenuSource::OhFx, "oh-fx · Workspace"),
        item("review", SkillMenuSource::Claude, "Claude · Workspace"),
    ]
}

fn shell() -> TestShell {
    TestShell::start_with(|options| {
        options.skill_catalog = Some(Box::new(FixedCatalog(catalog())));
    })
}

fn press(test: &mut TestShell, bytes: &[u8]) -> String {
    test.type_bytes(bytes);
    test.step();
    test.screen()
}

fn binding(scope: &str) -> SkillBinding {
    SkillBinding {
        name: "review".to_owned(),
        path: PathBuf::from(format!("/skills/{}/review", scope.replace(' ', ""))),
    }
}

#[test]
fn a_dollar_anywhere_opens_the_skills_menu_on_the_token_after_it() {
    for typed in [&b"$"[..], b" $", b"hello $", b"price$"] {
        let mut test = shell();
        let screen = press(&mut test, typed);
        assert!(screen.contains(HEADER), "{typed:?}: {screen}");
        assert!(screen.contains("  review    oh-fx · Workspace"), "{screen}");
        assert!(
            screen.contains("  review    Claude · Workspace"),
            "{screen}"
        );
        assert!(!screen.contains("auto · model-a"), "{screen}");
        assert_eq!(test.shell.composer.text().as_bytes(), typed);
    }
}

#[test]
fn enter_binds_the_chosen_skill_over_the_token_without_submitting() {
    let mut test = shell();
    press(&mut test, b"explain $rev");
    let screen = press(&mut test, b"\x1b[B\r");
    assert!(test.shell.skills_menu.is_none());
    assert_eq!(test.shell.composer.text(), "explain $review ");
    assert!(test.sent().is_empty());
    assert!(screen.contains("┃ explain review "), "{screen}");
    press(&mut test, b"the change\r");
    assert_eq!(
        test.sent(),
        [UiCommand::Submit {
            prompt: "explain $review the change".to_owned(),
            skills: vec![binding("Claude · Workspace")],
        }]
    );
}

#[test]
fn a_token_without_a_match_stays_plain_text() {
    let mut test = shell();
    let screen = press(&mut test, b"echo $HOME");
    assert!(!screen.contains("Skills"), "{screen}");
    assert!(screen.contains("auto · model-a"), "{screen}");
    press(&mut test, b"\x1b[A");
    assert_eq!(test.shell.composer.text(), "echo $HOME");
    press(&mut test, b"\r");
    assert_eq!(
        test.sent(),
        [UiCommand::Submit {
            prompt: "echo $HOME".to_owned(),
            skills: Vec::new(),
        }]
    );
}

#[test]
fn space_ends_the_mention_and_the_text_submits_as_typed() {
    let mut test = shell();
    let screen = press(&mut test, b"$rev ");
    assert!(test.shell.skills_menu.is_none());
    assert!(!screen.contains("Skills"), "{screen}");
    press(&mut test, b"x\r");
    assert_eq!(
        test.sent(),
        [UiCommand::Submit {
            prompt: "$rev x".to_owned(),
            skills: Vec::new(),
        }]
    );
}

#[test]
fn escape_closes_the_mention_and_keeps_the_text() {
    let mut test = shell();
    press(&mut test, b"$rev\x1b");
    test.advance(40);
    test.draining(|shell| shell.flush_pending_input().unwrap());
    assert!(test.shell.skills_menu.is_none());
    assert_eq!(test.shell.composer.text(), "$rev");
    let screen = test.screen();
    assert!(!screen.contains("Skills"), "{screen}");
    assert!(screen.contains("┃ $rev"), "{screen}");
}

#[test]
fn tab_switches_the_source_and_enter_on_an_empty_tab_submits_the_text() {
    let mut test = shell();
    press(&mut test, b"$rev");
    let screen = press(&mut test, b"\t");
    assert!(screen.contains("Skills 1  All  [oh-fx]"), "{screen}");
    let screen = press(&mut test, b"\t");
    assert!(screen.contains("No Workspace skills found."), "{screen}");
    press(&mut test, b"\r");
    assert_eq!(
        test.sent(),
        [UiCommand::Submit {
            prompt: "$rev".to_owned(),
            skills: Vec::new(),
        }]
    );
}

#[test]
fn editing_back_to_a_match_shows_the_menu_again_and_removing_the_dollar_closes_it() {
    let mut test = shell();
    let screen = press(&mut test, b"$revx");
    assert!(!screen.contains("Skills"), "{screen}");
    let screen = press(&mut test, b"\x7f");
    assert!(screen.contains(HEADER), "{screen}");
    press(&mut test, b"\x01\x04");
    assert!(test.shell.skills_menu.is_none());
    assert_eq!(test.shell.composer.text(), "rev");
}

#[test]
fn a_dollar_inside_a_file_mention_is_plain_text() {
    let mut test = shell();
    let screen = press(&mut test, b"@./$");
    assert!(!screen.contains("Skills"), "{screen}");
    assert!(test.shell.skills_menu.is_none());
}

#[test]
fn a_pasted_dollar_token_with_a_match_opens_the_menu_on_it() {
    let mut test = shell();
    let screen = press(&mut test, b"\x1b[200~use $rev now\x1b[201~");
    assert!(screen.contains(HEADER), "{screen}");
    press(&mut test, b"\r");
    assert_eq!(test.shell.composer.text(), "use $review now");
    let mut test = shell();
    let screen = press(&mut test, b"\x1b[200~cost $5 total\x1b[201~");
    assert!(!screen.contains("Skills"), "{screen}");
    assert!(test.shell.skills_menu.is_none());
}

#[test]
fn a_shell_without_a_catalog_keeps_the_dollar_plain() {
    let mut test = TestShell::start();
    let screen = press(&mut test, b"$rev");
    assert!(!screen.contains("Skills"), "{screen}");
    assert_eq!(test.shell.composer.text(), "$rev");
}

#[test]
fn super_r_waits_while_a_mention_menu_shows_matches() {
    let mut test = shell();
    let screen = press(&mut test, b"$rev\x1b[114;9u");
    assert!(screen.contains(HEADER), "{screen}");
    assert!(!screen.contains("switching sessions"), "{screen}");
    let screen = press(&mut test, b"zzz\x1b[114;9u");
    assert!(!screen.contains(HEADER), "{screen}");
    assert!(
        screen.contains("* session: submit or clear the draft before switching sessions"),
        "{screen}"
    );
    assert!(test.sent().is_empty());
}
