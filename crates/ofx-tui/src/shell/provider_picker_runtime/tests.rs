use std::path::PathBuf;

use ofx_contract::{
    ModelCatalog, ModelCatalogSource, SessionScope, SkillMenuFocus, SkillMenuGroup, SkillMenuItem,
    SkillMenuSource, TurnId, UiEvent,
};

use super::*;
use crate::shell::test_shell::TestShell;

const ESC: &[u8] = b"\x1b[27u";
const DOWN: &[u8] = b"\x1b[B";
const RIGHT: &[u8] = b"\x1b[C";
const LEFT: &[u8] = b"\x1b[D";

fn press(test: &mut TestShell, keys: &[u8]) {
    test.type_bytes(keys);
    test.step();
}

fn chosen(test: &TestShell) -> Vec<String> {
    test.sent()
        .into_iter()
        .filter_map(|command| match command {
            UiCommand::SelectProvider { provider } => Some(provider),
            _ => None,
        })
        .collect()
}

fn column_rows(screen: &str) -> Vec<&str> {
    screen
        .lines()
        .skip_while(|line| !line.starts_with('─'))
        .skip(1)
        .take_while(|line| !line.starts_with('─'))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect()
}

fn start_turn(test: &mut TestShell) {
    test.submit("slow");
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(1),
    });
}

#[test]
fn typing_the_command_opens_the_provider_column_under_its_argument() {
    let mut test = TestShell::start();
    press(&mut test, b"/provider ");
    let screen = test.screen();
    assert_eq!(
        column_rows(&screen),
        ["codex", "local · current", "portkey"],
        "{screen}"
    );
    let column = "┃ /provider ".chars().count();
    assert!(
        screen.contains(&format!("\n{}codex", " ".repeat(column))),
        "{screen}"
    );
    press(&mut test, b"po");
    assert_eq!(column_rows(&test.screen()), ["portkey"]);
    press(&mut test, b"\r");
    assert_eq!(chosen(&test), ["portkey"]);
    assert_eq!(test.shell.composer.text(), "");
    press(&mut test, b"/login zzz");
    assert_eq!(column_rows(&test.screen()), ["no matching providers"]);
    press(&mut test, b"\r");
    assert_eq!(chosen(&test), ["portkey"]);
    assert!(matches!(
        test.sent().last(),
        Some(UiCommand::Submit { prompt, .. }) if prompt == "/login zzz"
    ));
}

#[test]
fn tab_completes_the_highlighted_provider_and_keeps_the_typed_command() {
    let mut test = TestShell::start();
    press(&mut test, b" /SETUP ");
    press(&mut test, DOWN);
    press(&mut test, DOWN);
    press(&mut test, b"\t");
    assert_eq!(test.shell.composer.text(), "/SETUP portkey");
    assert_eq!(column_rows(&test.screen()), ["portkey"]);
    press(&mut test, b"\r");
    assert_eq!(chosen(&test), ["portkey"]);
}

#[test]
fn right_arrow_at_the_end_of_the_text_chooses_the_provider() {
    let mut test = TestShell::start();
    press(&mut test, b"/provider cod");
    press(&mut test, LEFT);
    press(&mut test, RIGHT);
    assert!(chosen(&test).is_empty());
    assert_eq!(
        test.shell.composer.cursor(),
        test.shell.composer.text().len()
    );
    press(&mut test, RIGHT);
    assert_eq!(chosen(&test), ["codex"]);
    assert_eq!(test.shell.composer.text(), "");
    press(&mut test, b"plain");
    press(&mut test, LEFT);
    press(&mut test, RIGHT);
    assert_eq!(chosen(&test), ["codex"]);
}

#[test]
fn escape_hides_the_column_until_the_text_stops_naming_the_command() {
    let mut test = TestShell::start();
    press(&mut test, b"/provider lo");
    press(&mut test, ESC);
    test.advance(40);
    test.draining(|shell| shell.flush_pending_input().unwrap());
    assert_eq!(test.shell.composer.text(), "/provider lo");
    assert!(column_rows(&test.screen()).is_empty());
    press(&mut test, b"c");
    assert!(column_rows(&test.screen()).is_empty());
    press(&mut test, b"\x15/provider ");
    assert_eq!(column_rows(&test.screen()).len(), 3);
}

#[test]
fn keys_are_limited_while_a_turn_runs() {
    let mut test = TestShell::start();
    start_turn(&mut test);
    press(&mut test, b"/provider");
    press(&mut test, b" ");
    assert_eq!(test.shell.composer.text(), "/provider ");
    assert!(column_rows(&test.screen()).is_empty());
    press(&mut test, DOWN);
    assert_eq!(test.shell.provider_column.cursor, Cursor::default());
    assert_eq!(test.shell.composer.text(), "/provider ");
    press(&mut test, b"\t");
    assert_eq!(test.shell.composer.text(), "/provider ");
    press(&mut test, b"codex");
    press(&mut test, LEFT);
    press(&mut test, RIGHT);
    press(&mut test, RIGHT);
    assert!(chosen(&test).is_empty());
    press(&mut test, b"\r");
    assert_eq!(chosen(&test), ["codex"]);
    assert_eq!(test.shell.composer.text(), "/provider codex");
    press(&mut test, ESC);
    test.advance(40);
    test.draining(|shell| shell.flush_pending_input().unwrap());
    assert!(!test.shell.provider_column.dismissed);
    test.deliver(UiEvent::TurnFinished {
        turn_id: TurnId::new(1),
        outcome: ofx_contract::TurnOutcome::Completed,
    });
    assert_eq!(column_rows(&test.screen()), ["codex"]);
}

#[test]
fn the_command_reply_seeds_the_composer_and_refreshes_the_providers() {
    let mut test = TestShell::start();
    press(&mut test, b"draft");
    test.deliver(UiEvent::ProviderPicker {
        prefix: "/login ".to_owned(),
        providers: vec!["codex".to_owned(), "fresh".to_owned()],
    });
    assert_eq!(test.shell.composer.text(), "/login ");
    assert_eq!(column_rows(&test.screen()), ["codex", "fresh"]);
    press(&mut test, b"\x15kept draft\x10");
    test.deliver(UiEvent::ProviderPicker {
        prefix: "/provider ".to_owned(),
        providers: vec!["codex".to_owned()],
    });
    assert!(test.shell.model_menu.is_some());
    press(&mut test, ESC);
    test.advance(40);
    test.draining(|shell| shell.flush_pending_input().unwrap());
    assert_eq!(test.shell.composer.text(), "kept draft");
}

#[test]
fn a_switch_marks_the_new_provider_and_drops_a_catalog_from_the_old_one() {
    let mut test = TestShell::start();
    press(&mut test, b"/model\r");
    assert_eq!(test.sent(), [UiCommand::ListModels]);
    test.deliver(UiEvent::ProviderSelected {
        provider: "codex".to_owned(),
    });
    assert_eq!(test.sent(), [UiCommand::ListModels, UiCommand::ListModels]);
    let listed = |provider: &str, id: &str| UiEvent::ModelCatalog {
        provider: provider.to_owned(),
        catalog: ModelCatalog::Listed {
            models: vec![crate::shell::model_menu::tests::option(id)],
            source: ModelCatalogSource::Subscription,
        },
    };
    test.deliver(listed("local", "old-model"));
    let screen = test.screen();
    assert!(screen.contains("Loading models…"), "{screen}");
    assert!(!screen.contains("old-model"), "{screen}");
    test.deliver(listed("codex", "new-model"));
    assert!(test.screen().contains("new-model"));
    press(&mut test, ESC);
    press(&mut test, b"/provider ");
    assert_eq!(
        column_rows(&test.screen()),
        ["codex · current", "local", "portkey"]
    );
}

#[test]
fn menus_take_the_footer_before_the_provider_column() {
    let mut test = TestShell::start();
    test.deliver(UiEvent::SkillsMenu {
        items: vec![SkillMenuItem {
            name: "review".to_owned(),
            description: String::new(),
            path: PathBuf::from("/skills/review"),
            source: SkillMenuSource::OhFx,
            group: SkillMenuGroup::Workspace,
            scope: "oh-fx · Workspace".to_owned(),
            source_label: String::new(),
        }],
        focus: SkillMenuFocus::Query("/provider ".to_owned()),
    });
    let screen = test.screen();
    assert!(screen.contains("Skills 0"), "{screen}");
    assert!(!screen.contains("portkey"), "{screen}");
    press(&mut test, b"\r");
    assert!(chosen(&test).is_empty());
}

#[test]
fn the_session_picker_keeps_the_footer_from_the_provider_column() {
    let mut test = TestShell::start();
    test.deliver(UiEvent::SessionPickerOpened {
        scope: SessionScope::CurrentWorkspace,
    });
    press(&mut test, b"/provider ");
    test.deliver(UiEvent::ProviderPicker {
        prefix: "/login ".to_owned(),
        providers: vec!["codex".to_owned()],
    });
    assert!(test.shell.picker.is_some());
    assert_eq!(test.shell.composer.text(), "/provider ");
    let screen = test.screen();
    assert!(!screen.contains("portkey"), "{screen}");
    assert!(!screen.contains("current"), "{screen}");
    press(&mut test, RIGHT);
    assert!(chosen(&test).is_empty());
}
