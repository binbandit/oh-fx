use std::path::PathBuf;

use ofx_contract::{
    ModelCapabilities, ModelCatalog, ModelCatalogSource, ModelOption, SkillMenuFocus,
    SkillMenuGroup, SkillMenuItem, SkillMenuSource, UiCommand, UiEvent,
};

use crate::shell::SkillCatalogSource;
use crate::shell::test_shell::TestShell;

const HEADER: &str = "Commands 4  [All]  General  Model";
const ROWS: &str = "  /help     show available slash commands\n  /clear    clear the screen\n  /quit     exit the interactive shell\n  /model    choose a model";
const HINT: &str = "↑↓ navigate     tab category     enter open     esc close";

fn open() -> TestShell {
    let mut test = TestShell::start();
    test.submit("/help");
    assert_eq!(
        test.sent(),
        [UiCommand::RunCommand {
            text: "/help".to_owned()
        }]
    );
    test.deliver(UiEvent::HelpRequested);
    test
}

fn press(test: &mut TestShell, bytes: &[u8]) -> String {
    test.type_bytes(bytes);
    test.step();
    test.screen()
}

fn sent_after_help(test: &TestShell) -> Vec<UiCommand> {
    test.sent()[1..].to_vec()
}

#[test]
fn help_opens_a_command_menu_under_the_composer_in_category_order() {
    let mut test = open();
    let screen = test.screen();
    assert!(
        screen.contains(&format!("┃ \n\n{HEADER}\n\n{ROWS}\n\n{HINT}")),
        "{screen}"
    );
    assert!(!screen.contains("auto · model-a"), "{screen}");
    assert!(test.shell.composer.is_empty());
}

#[test]
fn help_closes_an_open_skills_menu() {
    let mut test = TestShell::start();
    test.deliver(UiEvent::SkillsMenu {
        items: Vec::new(),
        focus: SkillMenuFocus::Start,
    });
    assert!(test.shell.skills_menu.is_some());
    test.deliver(UiEvent::HelpRequested);
    assert!(test.shell.skills_menu.is_none());
    let screen = test.screen();
    assert!(screen.contains(HEADER), "{screen}");
    assert!(!screen.contains("Skills"), "{screen}");
}

#[test]
fn typing_filters_tab_switches_the_category_and_enter_runs_the_selection() {
    let mut test = open();
    let screen = press(&mut test, b"shell");
    assert!(
        screen.contains("Commands 1  [All]  General  Model"),
        "{screen}"
    );
    assert!(
        screen.contains("  /quit    exit the interactive shell"),
        "{screen}"
    );
    assert!(!screen.contains("show available"), "{screen}");
    let screen = press(&mut test, b"\x7f\x7f\x7f\x7f\x7f\t");
    assert!(
        screen.contains("Commands 3  All  [General]  Model"),
        "{screen}"
    );
    assert!(!screen.contains("/model"), "{screen}");
    let screen = press(&mut test, b"\t");
    assert!(
        screen.contains("Commands 1  All  General  [Model]"),
        "{screen}"
    );
    let screen = press(&mut test, b"\x1b[Z\x1b[Z");
    assert!(screen.contains(HEADER), "{screen}");
    press(&mut test, b"\x1b[B\x1b[B\x1b[A\x0e\x0b\x0e");
    assert_eq!(test.shell.help_menu.unwrap().selected(), 2);
    assert!(sent_after_help(&test).is_empty());
    let screen = press(&mut test, b"\r");
    assert!(test.shell.help_menu.is_none());
    assert_eq!(
        sent_after_help(&test),
        [UiCommand::RunCommand {
            text: "/quit".to_owned()
        }]
    );
    assert!(screen.contains("auto · model-a"), "{screen}");
    assert!(!screen.contains("Commands"), "{screen}");
}

#[test]
fn a_command_that_takes_arguments_is_left_in_the_composer() {
    let mut test = open();
    let screen = press(&mut test, b"id-or");
    assert!(screen.contains("Commands 1  [All]"), "{screen}");
    let screen = press(&mut test, b"\r");
    assert!(test.shell.help_menu.is_none());
    assert_eq!(test.shell.composer.text(), "/model ");
    assert!(screen.contains("┃ /model"), "{screen}");
    assert_eq!(sent_after_help(&test), [UiCommand::ListModels]);
}

#[test]
fn an_exact_command_closes_the_menu_and_runs_as_typed() {
    for typed in ["/clear", "/exit "] {
        let mut test = open();
        press(&mut test, typed.as_bytes());
        press(&mut test, b"\r");
        assert!(test.shell.help_menu.is_none(), "{typed:?}");
        assert_eq!(
            sent_after_help(&test),
            [UiCommand::RunCommand {
                text: typed.to_owned()
            }],
            "{typed:?}"
        );
    }
}

#[test]
fn without_matches_enter_keeps_the_menu_and_arrows_pass_through() {
    let mut test = open();
    let screen = press(&mut test, b"zzz\r");
    assert!(screen.contains("Commands 0  [All]"), "{screen}");
    assert!(screen.contains("No commands found."), "{screen}");
    assert!(test.shell.help_menu.is_some());
    assert_eq!(test.shell.composer.text(), "zzz");
    assert!(sent_after_help(&test).is_empty());
    assert!(!test.shell.move_help_menu(-1));
    press(&mut test, b"\x7f\x7f\x7f");
    assert!(test.shell.move_help_menu(-1));
    assert_eq!(test.shell.help_menu.unwrap().selected(), 3);
}

#[test]
fn escape_closes_the_menu_and_clears_its_query() {
    let mut test = open();
    press(&mut test, b"cl\x1b");
    test.advance(40);
    test.draining(|shell| shell.flush_pending_input().unwrap());
    assert!(test.shell.help_menu.is_none());
    assert!(test.shell.composer.is_empty());
    assert!(!test.shell.gestures.escape_clear_armed());
    let screen = test.screen();
    assert!(!screen.contains("Commands"), "{screen}");
    assert!(screen.contains("auto · model-a"), "{screen}");
}

struct ReviewSkill;

impl SkillCatalogSource for ReviewSkill {
    fn menu_items(&self) -> Vec<SkillMenuItem> {
        vec![SkillMenuItem {
            name: "review".to_owned(),
            description: "review workflow".to_owned(),
            path: PathBuf::from("/skills/review"),
            source: SkillMenuSource::OhFx,
            group: SkillMenuGroup::Workspace,
            scope: "oh-fx · Workspace".to_owned(),
            source_label: String::new(),
        }]
    }
}

#[test]
fn skill_mentions_stay_search_text_while_the_menu_is_open() {
    let mut test = TestShell::start_with(|options| {
        options.skill_catalog = Some(Box::new(ReviewSkill));
    });
    test.deliver(UiEvent::HelpRequested);
    let screen = press(&mut test, b"\x1b[200~line1\n$rev\x1b[201~");
    assert!(test.shell.skills_menu.is_none());
    assert!(screen.contains("No commands found."), "{screen}");
    assert!(!screen.contains("Skills"), "{screen}");
    assert_eq!(test.shell.composer.text(), "line1\n$rev");
    press(&mut test, b"\x1b[A");
    assert!(test.shell.composer.cursor() <= "line1".len());
    press(&mut test, b"\x1b[B $");
    assert!(test.shell.skills_menu.is_none());
    assert!(test.shell.help_menu.is_some());
}

#[test]
fn ctrl_p_waits_for_the_menu_and_an_exact_model_command_opens_the_model_menu() {
    let mut test = open();
    press(&mut test, b"\x10");
    assert!(test.shell.model_menu.is_none());
    assert!(test.shell.help_menu.is_some());
    let screen = press(&mut test, b"/model\r");
    assert!(test.shell.help_menu.is_none());
    assert!(test.shell.model_menu.is_some());
    assert!(screen.contains("Models 0  [All]"), "{screen}");
    assert!(!screen.contains("Commands"), "{screen}");
}

#[test]
fn a_late_help_reply_leaves_an_open_model_menu_and_its_lent_draft() {
    let mut test = TestShell::start();
    press(&mut test, b"draft\x10");
    assert!(test.shell.model_menu.is_some());
    test.deliver(UiEvent::HelpRequested);
    assert!(test.shell.help_menu.is_none());
    assert!(test.shell.model_menu.is_some());
    let screen = test.screen();
    assert!(screen.contains("Models 0  [All]"), "{screen}");
    assert!(!screen.contains("Commands"), "{screen}");
}

#[test]
fn model_shaped_search_text_leaves_the_arrows_to_the_composer() {
    let mut test = open();
    let screen = press(&mut test, b"\x1b[200~/model zzz\nsecond\x1b[201~");
    assert!(screen.contains("No commands found."), "{screen}");
    assert!(test.shell.model_query().is_none());
    press(&mut test, b"\x1b[A");
    assert!(test.shell.composer.cursor() <= "/model zzz".len());
    assert_eq!(test.shell.composer.text(), "/model zzz\nsecond");
}

#[test]
fn a_late_help_reply_over_an_effort_column_keeps_left_for_the_cursor() {
    let mut test = TestShell::start();
    press(&mut test, b"/model\r");
    test.deliver(UiEvent::ModelCatalog {
        catalog: ModelCatalog::Listed {
            models: vec![ModelOption {
                id: "vendor/model".to_owned(),
                capabilities: ModelCapabilities {
                    reasoning_efforts: vec!["low".to_owned(), "high".to_owned()],
                    supports_fast_mode: false,
                    context_window: None,
                },
                max_output_tokens: None,
            }],
            source: ModelCatalogSource::ProfileSettings,
        },
    });
    press(&mut test, b"\r");
    press(&mut test, b"high");
    assert_eq!(test.shell.composer.text(), "/model vendor/model high");
    test.deliver(UiEvent::HelpRequested);
    assert!(test.shell.help_menu.is_some());
    press(&mut test, b"\x1b[D");
    assert_eq!(test.shell.composer.text(), "/model vendor/model high");
    assert_eq!(
        test.shell.composer.cursor(),
        "/model vendor/model high".len() - 1
    );
}
