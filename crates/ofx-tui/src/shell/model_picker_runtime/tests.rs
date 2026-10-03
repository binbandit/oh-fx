use std::path::PathBuf;

use ofx_contract::{
    CatalogRetry, ModelCatalogSource, ModelOption, SkillMenuFocus, SkillMenuGroup, SkillMenuItem,
    SkillMenuSource, TurnId, UiEvent,
};

use super::*;
use crate::shell::test_shell::TestShell;

const ESC: &[u8] = b"\x1b[27u";
const DOWN: &[u8] = b"\x1b[B";
const UP: &[u8] = b"\x1b[A";
const LEFT: &[u8] = b"\x1b[D";
const SHIFT_TAB: &[u8] = b"\x1b[Z";
const CTRL_P: &[u8] = b"\x10";
const MENU_HINT: &str = "↑↓ navigate     tab provider     enter use     esc close";

fn press(test: &mut TestShell, keys: &[u8]) {
    test.type_bytes(keys);
    test.step();
}

fn option(id: &str, efforts: &[&str], fast: bool) -> ModelOption {
    ModelOption {
        id: id.to_owned(),
        capabilities: ModelCapabilities {
            reasoning_efforts: efforts.iter().map(|effort| (*effort).to_owned()).collect(),
            supports_fast_mode: fast,
            context_window: Some(200_000),
        },
        max_output_tokens: None,
    }
}

fn catalog() -> ModelCatalog {
    ModelCatalog::Listed {
        models: vec![
            option("anthropic/claude-x", &["low", "high"], true),
            option("openai/gpt-y", &[], false),
            option("openai/gpt-z", &["medium"], false),
            option("deepseek/v3", &[], true),
        ],
        source: ModelCatalogSource::ProfileSettings,
    }
}

fn listed(test: &mut TestShell, catalog: ModelCatalog) {
    test.deliver(UiEvent::ModelCatalog {
        provider: "local".to_owned(),
        catalog,
    });
}

fn open_menu(test: &mut TestShell) {
    press(test, b"/model\r");
    listed(test, catalog());
}

fn picks(test: &TestShell) -> Vec<UiCommand> {
    test.sent()
        .into_iter()
        .filter(|command| matches!(command, UiCommand::SelectModel { .. }))
        .collect()
}

fn pick(model: &str, effort: ReasoningEffort, fast_mode: Option<bool>) -> UiCommand {
    UiCommand::SelectModel {
        model: model.to_owned(),
        effort,
        fast_mode,
    }
}

fn named(effort: &str) -> ReasoningEffort {
    ReasoningEffort::Named(effort.to_owned())
}

fn draft(test: &mut TestShell, content: &str, cursor: usize) {
    test.shell.composer.replace_text(content);
    test.shell
        .composer
        .move_cursor(crate::input::MoveIntent::new(
            crate::input::MoveKind::LineStart,
        ));
    for _ in 0..cursor {
        test.shell
            .composer
            .move_cursor(crate::input::MoveIntent::new(
                crate::input::MoveKind::CharacterRight,
            ));
    }
}

fn skills_menu() -> UiEvent {
    UiEvent::SkillsMenu {
        items: vec![SkillMenuItem {
            name: "review".to_owned(),
            description: String::new(),
            path: PathBuf::from("/skills/review"),
            source: SkillMenuSource::OhFx,
            group: SkillMenuGroup::Workspace,
            scope: "oh-fx · Workspace".to_owned(),
            source_label: String::new(),
        }],
        focus: SkillMenuFocus::Start,
    }
}

#[test]
fn bare_model_opens_the_catalog_menu_in_the_footer_while_it_loads() {
    let mut test = TestShell::start();
    press(&mut test, b"/model\r");
    assert_eq!(test.sent(), [UiCommand::ListModels]);
    assert_eq!(test.shell.composer.text(), "");
    let screen = test.screen();
    assert!(screen.contains("Models 0  [All]"), "{screen}");
    assert!(screen.contains("Loading models…"), "{screen}");
    assert!(screen.contains(MENU_HINT), "{screen}");
    assert!(!screen.contains("auto · model-a"), "{screen}");
    listed(&mut test, catalog());
    let screen = test.screen();
    assert!(
        screen.contains("Models 4  [All]  Anthropic  OpenAI  Others"),
        "{screen}"
    );
    assert!(
        screen.contains("  anthropic/claude-x  200K context · Fast"),
        "{screen}"
    );
    assert!(
        screen.contains("  openai/gpt-y        200K context"),
        "{screen}"
    );
    assert!(
        screen.contains("Models from profile settings; explicit model IDs do not require"),
        "{screen}"
    );
    press(&mut test, ESC);
    assert!(test.shell.model_menu.is_none());
    let screen = test.screen();
    assert!(!screen.contains("Models 4"), "{screen}");
    assert!(screen.contains("auto · model-a"), "{screen}");
    press(&mut test, b"/model\r");
    assert_eq!(test.sent(), [UiCommand::ListModels]);
    assert!(test.screen().contains("Models 4  [All]"));
}

#[test]
fn a_partial_command_resolving_to_model_opens_the_menu_too() {
    let mut test = TestShell::start();
    test.submit("/mod");
    assert!(test.shell.model_menu.is_some());
    assert_eq!(test.sent(), [UiCommand::ListModels]);
    let mut test = TestShell::start();
    draft(&mut test, "/model", 3);
    test.shell.submit();
    assert!(test.shell.model_menu.is_none());
    assert_eq!(
        test.sent(),
        [UiCommand::RunCommand {
            text: "/model".to_owned()
        }]
    );
}

#[test]
fn typing_filters_the_menu_and_tab_switches_the_vendor() {
    let mut test = TestShell::start();
    open_menu(&mut test);
    press(&mut test, b"gpt");
    let screen = test.screen();
    assert!(screen.contains("Models 2  [All]"), "{screen}");
    assert!(!screen.contains("claude-x"), "{screen}");
    assert!(screen.contains("┃ gpt"), "{screen}");
    press(&mut test, b"\x7f\x7f\x7f");
    assert!(test.screen().contains("Models 4"));
    press(&mut test, b"\t");
    let screen = test.screen();
    assert!(
        screen.contains("Models 1  All  [Anthropic]  OpenAI  Others"),
        "{screen}"
    );
    press(&mut test, SHIFT_TAB);
    press(&mut test, SHIFT_TAB);
    let screen = test.screen();
    assert!(
        screen.contains("Models 1  All  Anthropic  OpenAI  [Others]"),
        "{screen}"
    );
    assert!(screen.contains("deepseek/v3"), "{screen}");
    assert!(!test.sent().contains(&UiCommand::TogglePermissionMode));
}

#[test]
fn arrows_wrap_and_enter_applies_a_model_without_efforts_or_fast_mode_at_once() {
    let mut test = TestShell::start();
    open_menu(&mut test);
    press(&mut test, UP);
    assert_eq!(test.shell.model_menu.as_ref().unwrap().selected, 3);
    press(&mut test, DOWN);
    press(&mut test, DOWN);
    assert_eq!(test.shell.model_menu.as_ref().unwrap().selected, 1);
    press(&mut test, b"\r");
    assert!(test.shell.model_menu.is_none());
    assert_eq!(test.shell.composer.text(), "");
    assert_eq!(
        picks(&test),
        [pick("openai/gpt-y", ReasoningEffort::Auto, None)]
    );
}

#[test]
fn a_model_with_efforts_continues_into_the_effort_and_mode_columns() {
    let mut test = TestShell::start();
    open_menu(&mut test);
    press(&mut test, b"\r");
    assert_eq!(test.shell.composer.text(), "/model anthropic/claude-x ");
    let screen = test.screen();
    assert!(
        screen.contains("┃ /model anthropic/claude-x \n─"),
        "{screen}"
    );
    let column = "┃ /model anthropic/claude-x ".chars().count();
    for label in ["default", "low", "high"] {
        assert!(
            screen.contains(&format!("\n{}{label}", " ".repeat(column))),
            "{label}: {screen}"
        );
    }
    press(&mut test, DOWN);
    press(&mut test, b"\r");
    assert_eq!(test.shell.composer.text(), "/model anthropic/claude-x low ");
    assert!(test.screen().contains("normal"));
    assert_eq!(test.shell.model_flow.fast.index, 1);
    assert!(picks(&test).is_empty());
    press(&mut test, b"\r");
    assert_eq!(test.shell.composer.text(), "");
    assert_eq!(
        picks(&test),
        [pick("anthropic/claude-x", named("low"), Some(true))]
    );
}

#[test]
fn typed_efforts_and_modes_filter_their_columns_and_space_advances_only_on_an_exact_value() {
    let mut test = TestShell::start();
    open_menu(&mut test);
    press(&mut test, b"\r");
    press(&mut test, b"hi");
    let screen = test.screen();
    assert!(screen.contains("/model anthropic/claude-x hi"), "{screen}");
    assert!(!screen.contains("default"), "{screen}");
    press(&mut test, b"gh ");
    assert_eq!(
        test.shell.composer.text(),
        "/model anthropic/claude-x high "
    );
    assert_eq!(test.shell.model_flow.stage, ModelStage::Fast);
    press(&mut test, b"norm ");
    assert_eq!(
        test.shell.composer.text(),
        "/model anthropic/claude-x high norm"
    );
    press(&mut test, b"\r");
    assert_eq!(
        picks(&test),
        [pick("anthropic/claude-x", named("high"), Some(false))]
    );
    let mut test = TestShell::start();
    open_menu(&mut test);
    press(&mut test, b"\r");
    press(&mut test, b"zzz");
    assert!(test.screen().contains("no matching effort"));
    press(&mut test, b"\r");
    assert!(picks(&test).is_empty());
    press(&mut test, b"\x7f\x7f\x7fauto\r");
    assert_eq!(test.shell.model_flow.stage, ModelStage::Fast);
    assert_eq!(
        test.shell.composer.text(),
        "/model anthropic/claude-x auto "
    );
}

#[test]
fn no_space_that_cannot_advance_the_picker_reaches_its_query() {
    let mut test = TestShell::start();
    open_menu(&mut test);
    press(&mut test, ESC);
    press(&mut test, b"/model ");
    assert_eq!(test.shell.composer.text(), "/model ");
    press(&mut test, b" gpt- ");
    assert_eq!(test.shell.composer.text(), "/model gpt-");
    press(&mut test, b"z ");
    assert_eq!(test.shell.composer.text(), "/model gpt-z");
    press(&mut test, b"\x7f\x7f\x7f\x7f\x7fopenai/gpt-z ");
    assert_eq!(test.shell.composer.text(), "/model openai/gpt-z ");
    assert_eq!(test.shell.model_flow.stage, ModelStage::Effort);
    press(&mut test, b"medium ");
    assert_eq!(test.shell.composer.text(), "/model openai/gpt-z medium");
    press(&mut test, b"\r");
    assert_eq!(picks(&test), [pick("openai/gpt-z", named("medium"), None)]);
    press(&mut test, b"/model deepseek/v3 ");
    assert_eq!(test.shell.composer.text(), "/model deepseek/v3 auto ");
    press(&mut test, b"fast ");
    assert_eq!(test.shell.composer.text(), "/model deepseek/v3 auto fast");
}

#[test]
fn a_model_with_only_a_fast_tier_skips_to_the_mode_column() {
    let mut test = TestShell::start();
    open_menu(&mut test);
    press(&mut test, b"deep\r");
    assert_eq!(test.shell.composer.text(), "/model deepseek/v3 auto ");
    assert_eq!(test.shell.model_flow.stage, ModelStage::Fast);
    press(&mut test, DOWN);
    press(&mut test, b"\r");
    assert_eq!(
        picks(&test),
        [pick("deepseek/v3", ReasoningEffort::Auto, Some(false))]
    );
}

#[test]
fn tab_writes_the_highlighted_option_and_left_steps_back() {
    let mut test = TestShell::start();
    open_menu(&mut test);
    press(&mut test, b"\r");
    press(&mut test, DOWN);
    press(&mut test, DOWN);
    press(&mut test, b"\t");
    assert_eq!(test.shell.composer.text(), "/model anthropic/claude-x high");
    assert_eq!(test.shell.model_flow.stage, ModelStage::Effort);
    press(&mut test, b" ");
    assert_eq!(test.shell.model_flow.stage, ModelStage::Fast);
    press(&mut test, LEFT);
    assert_eq!(test.shell.composer.text(), "/model anthropic/claude-x ");
    assert_eq!(test.shell.model_flow.stage, ModelStage::Effort);
    assert_eq!(test.shell.model_flow.effort.index, 2);
    press(&mut test, LEFT);
    assert_eq!(test.shell.composer.text(), "/model anthropic/claude-x");
    assert_eq!(test.shell.model_flow.stage, ModelStage::Model);
    press(&mut test, b"\r");
    assert_eq!(test.shell.composer.text(), "/model anthropic/claude-x ");
    assert_eq!(test.shell.model_flow.stage, ModelStage::Effort);
}

#[test]
fn escape_dismisses_the_column_until_the_text_stops_naming_a_model() {
    let mut test = TestShell::start();
    open_menu(&mut test);
    press(&mut test, b"\r");
    press(&mut test, ESC);
    assert_eq!(test.shell.composer.text(), "/model anthropic/claude-x ");
    assert!(test.shell.model_query().is_none());
    assert!(!test.screen().contains("default"));
    press(&mut test, b"\x15");
    assert_eq!(test.shell.composer.text(), "");
    assert!(!test.shell.model_flow.dismissed);
    press(&mut test, b"/model ");
    assert!(test.screen().contains("anthropic/claude-x"));
}

#[test]
fn tab_on_bare_model_lists_the_catalog_from_the_current_model() {
    let mut test = TestShell::start_with(|options| "openai/gpt-z".clone_into(&mut options.model));
    press(&mut test, b"/model\t");
    assert_eq!(test.shell.composer.text(), "/model ");
    assert_eq!(test.sent(), [UiCommand::ListModels]);
    assert!(test.screen().contains("loading models..."));
    listed(&mut test, catalog());
    let screen = test.screen();
    for id in ["anthropic/claude-x", "openai/gpt-y", "openai/gpt-z"] {
        assert!(screen.contains(&format!("       {id}")), "{id}: {screen}");
    }
    press(&mut test, b"\r");
    assert_eq!(test.shell.composer.text(), "/model openai/gpt-z ");
    press(&mut test, b"\r");
    assert_eq!(
        picks(&test),
        [pick("openai/gpt-z", ReasoningEffort::Auto, None)]
    );
    press(&mut test, b"/model nothing-matches\r");
    assert_eq!(
        test.sent().last(),
        Some(&UiCommand::RunCommand {
            text: "/model nothing-matches".to_owned()
        })
    );
}

#[test]
fn typed_model_effort_and_mode_choices_are_sent_and_malformed_ones_explain_the_usage() {
    let mut test = TestShell::start();
    test.shell.composer.replace_text("/model gpt-5 high fast");
    test.draining(Shell::submit_explicit_model);
    assert_eq!(picks(&test), [pick("gpt-5", named("high"), Some(true))]);
    assert_eq!(test.shell.composer.text(), "");
    test.shell.composer.replace_text("/model gpt-5 high quick");
    press(&mut test, b"\r");
    assert_eq!(picks(&test).len(), 1);
    assert_eq!(test.shell.composer.text(), "/model gpt-5 high quick");
    assert!(
        test.screen()
            .contains("usage: /model <id> <effort> [normal|fast]")
    );
}

#[test]
fn an_unreachable_catalog_says_so_and_a_later_open_asks_again() {
    let mut test = TestShell::start();
    press(&mut test, b"/model\r");
    listed(
        &mut test,
        ModelCatalog::Failed {
            retry: Some(CatalogRetry::Unreachable),
        },
    );
    let screen = test.screen();
    assert!(
        screen.contains("Could not reach AI Gateway; retry /model."),
        "{screen}"
    );
    press(&mut test, b"\r");
    assert!(test.shell.model_menu.is_some());
    press(&mut test, ESC);
    press(&mut test, b"/model\r");
    assert_eq!(test.sent(), [UiCommand::ListModels, UiCommand::ListModels]);
    listed(&mut test, ModelCatalog::Failed { retry: None });
    assert!(test.screen().contains("Unable to load models."));
}

#[test]
fn hostile_model_names_never_reach_the_terminal_raw() {
    let mut test = TestShell::start();
    press(&mut test, b"/model\r");
    let hostile = "evil\u{1b}]52;c;aGk=\u{7}\u{1b}[2J/\u{202e}model";
    listed(
        &mut test,
        ModelCatalog::Listed {
            models: vec![option(hostile, &["low\u{1b}[31m"], true)],
            source: ModelCatalogSource::Subscription,
        },
    );
    let written = test.written();
    assert!(!written.contains("\u{1b}]52"), "{written:?}");
    assert!(!written.contains("\u{1b}[2J/"), "{written:?}");
    assert!(!written.contains('\u{202e}'), "{written:?}");
    assert!(
        test.screen()
            .contains("Codex catalog: authenticated with a subscription.")
    );
    press(&mut test, b"\r");
    let written = test.written();
    assert!(!written.contains("\u{1b}[31m"), "{written:?}");
    assert!(!written.contains("\u{1b}]52"), "{written:?}");
}

#[test]
fn the_menu_opens_and_picks_during_a_turn() {
    let mut test = TestShell::start();
    test.submit("slow");
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(1),
    });
    open_menu(&mut test);
    press(&mut test, DOWN);
    press(&mut test, b"\r");
    assert_eq!(
        picks(&test),
        [pick("openai/gpt-y", ReasoningEffort::Auto, None)]
    );
    assert!(test.shell.turn.is_some());
}

#[test]
fn the_model_menu_and_the_skills_menu_are_never_open_together() {
    let mut test = TestShell::start();
    test.deliver(skills_menu());
    press(&mut test, CTRL_P);
    assert!(test.shell.model_menu.is_none());
    assert!(test.shell.skills_menu.is_some());
    press(&mut test, ESC);
    draft(&mut test, "keep", 4);
    press(&mut test, CTRL_P);
    listed(&mut test, catalog());
    assert!(test.shell.model_menu.is_some());
    test.deliver(skills_menu());
    assert!(test.shell.model_menu.is_none());
    assert!(test.shell.skills_menu.is_some());
    assert_eq!(test.shell.composer.text(), "keep");
    let screen = test.screen();
    assert!(screen.contains("Skills 1"), "{screen}");
    assert!(!screen.contains("Models"), "{screen}");
    press(&mut test, ESC);
    open_menu(&mut test);
    test.deliver(skills_menu());
    assert!(test.shell.model_menu.is_none());
}

#[test]
fn ctrl_p_lends_the_composer_to_the_menu_and_gives_the_draft_back_on_every_exit() {
    let exits: [&[u8]; 5] = [ESC, CTRL_P, b"\x03", b"\x04", b"\r"];
    for exit in exits {
        let mut test = TestShell::start();
        draft(&mut test, "keep this draft", 4);
        press(&mut test, CTRL_P);
        assert!(test.shell.model_menu.is_some());
        assert_eq!(test.shell.composer.text(), "");
        listed(&mut test, catalog());
        press(&mut test, b"gpt-y");
        press(&mut test, exit);
        assert!(test.shell.model_menu.is_none(), "{exit:?}");
        assert_eq!(test.shell.composer.text(), "keep this draft", "{exit:?}");
        assert_eq!(test.shell.composer.cursor(), 4, "{exit:?}");
        assert!(test.shell.model_draft.is_none(), "{exit:?}");
        assert!(!test.shell.should_exit, "{exit:?}");
        assert!(!test.shell.gestures.ctrl_c_exit_armed(), "{exit:?}");
        let picked = if exit == b"\r" {
            vec![pick("openai/gpt-y", ReasoningEffort::Auto, None)]
        } else {
            Vec::new()
        };
        assert_eq!(picks(&test), picked, "{exit:?}");
    }
}

#[test]
fn ctrl_p_columns_back_out_to_the_draft_without_choosing_a_model() {
    let exits: [&[u8]; 5] = [ESC, CTRL_P, b"\x03", b"\x15", b"\x01\x1b[200~x\x1b[201~"];
    for exit in exits {
        let mut test = TestShell::start();
        draft(&mut test, "keep me", 2);
        press(&mut test, CTRL_P);
        listed(&mut test, catalog());
        press(&mut test, b"\r");
        assert_eq!(test.shell.model_flow.stage, ModelStage::Effort);
        assert!(test.shell.model_draft.is_some());
        test.type_bytes(exit);
        test.step();
        test.advance(40);
        test.draining(|shell| shell.flush_pending_input().unwrap());
        assert_eq!(test.shell.composer.text(), "keep me", "{exit:?}");
        assert_eq!(test.shell.composer.cursor(), 2, "{exit:?}");
        assert!(test.shell.model_draft.is_none(), "{exit:?}");
        assert_eq!(test.shell.model_flow.stage, ModelStage::Model, "{exit:?}");
        assert!(picks(&test).is_empty(), "{exit:?}");
    }
}

#[test]
fn ctrl_p_choices_finish_their_columns_before_the_draft_returns() {
    let mut test = TestShell::start();
    draft(&mut test, "draft survives", 3);
    press(&mut test, CTRL_P);
    listed(&mut test, catalog());
    press(&mut test, b"\r");
    press(&mut test, b"high\r");
    assert_eq!(test.shell.model_flow.stage, ModelStage::Fast);
    assert!(test.shell.model_draft.is_some());
    press(&mut test, b"normal\r");
    assert_eq!(
        picks(&test),
        [pick("anthropic/claude-x", named("high"), Some(false))]
    );
    assert_eq!(test.shell.composer.text(), "draft survives");
    assert_eq!(test.shell.composer.cursor(), 3);
    assert!(test.shell.model_draft.is_none());
}

#[test]
fn a_borrowed_composer_is_never_submitted_except_as_a_typed_model_choice() {
    let mut test = TestShell::start();
    draft(&mut test, "draft", 5);
    press(&mut test, CTRL_P);
    listed(&mut test, catalog());
    press(&mut test, b"\r");
    press(&mut test, LEFT);
    press(&mut test, b"zz\r");
    assert_eq!(test.shell.composer.text(), "/model anthropic/claude-xzz");
    assert!(test.shell.model_draft.is_some());
    assert_eq!(test.sent(), [UiCommand::ListModels]);
    test.shell
        .composer
        .replace_text("/model openai/gpt-y default");
    press(&mut test, b"\r");
    assert_eq!(
        picks(&test),
        [pick("openai/gpt-y", ReasoningEffort::Auto, None)]
    );
    assert_eq!(test.shell.composer.text(), "draft");
    assert!(test.shell.model_draft.is_none());
}

#[test]
fn ctrl_p_keeps_the_prompt_history_position_of_the_draft() {
    let mut test = TestShell::start();
    test.shell
        .composer
        .install_history(vec!["older".to_owned(), "history entry".to_owned()]);
    draft(&mut test, "draft", 5);
    press(&mut test, UP);
    press(&mut test, UP);
    assert_eq!(test.shell.composer.text(), "history entry");
    press(&mut test, CTRL_P);
    assert_eq!(test.shell.composer.text(), "");
    press(&mut test, ESC);
    test.advance(40);
    test.draining(|shell| shell.flush_pending_input().unwrap());
    assert_eq!(test.shell.composer.text(), "history entry");
    press(&mut test, DOWN);
    assert_eq!(test.shell.composer.text(), "draft");
}

#[test]
fn reopening_the_menu_starts_without_a_leftover_effort_or_mode_column() {
    let mut test = TestShell::start();
    open_menu(&mut test);
    press(&mut test, b"\r");
    assert_eq!(test.shell.model_flow.stage, ModelStage::Effort);
    press(&mut test, ESC);
    press(&mut test, CTRL_P);
    assert_eq!(test.shell.model_flow, ModelFlow::default());
    press(&mut test, DOWN);
    press(&mut test, b"\r");
    assert_eq!(
        picks(&test),
        [pick("openai/gpt-y", ReasoningEffort::Auto, None)]
    );
    assert_eq!(test.shell.composer.text(), "/model anthropic/claude-x ");
    assert_eq!(test.shell.model_flow.stage, ModelStage::Model);
    let screen = test.screen();
    assert!(!screen.contains("default"), "{screen}");
    assert!(screen.contains("anthropic/claude-x\n"), "{screen}");
}

fn paste(test: &mut TestShell, content: &str) {
    for chunk in format!("\x1b[200~{content}\x1b[201~")
        .as_bytes()
        .chunks(256)
    {
        press(test, chunk);
    }
    test.advance(40);
    test.draining(|shell| shell.flush_pending_input().unwrap());
}

#[test]
fn pasted_blocks_stay_with_the_text_that_holds_them() {
    let mut test = TestShell::start();
    let kept = "x".repeat(1200);
    paste(&mut test, &kept);
    let placeholder = test.shell.composer.text().to_owned();
    assert!(placeholder.starts_with("[Pasted text #1"), "{placeholder}");
    press(&mut test, CTRL_P);
    assert_eq!(test.shell.composer.expanded_text(), "");
    listed(&mut test, catalog());
    paste(&mut test, &"y".repeat(1200));
    assert!(test.shell.composer.text().starts_with("[Pasted text #1"));
    press(&mut test, ESC);
    test.advance(40);
    test.draining(|shell| shell.flush_pending_input().unwrap());
    assert_eq!(test.shell.composer.text(), placeholder);
    assert_eq!(test.shell.composer.expanded_text(), kept);
    press(&mut test, CTRL_P);
    paste(&mut test, &"y".repeat(1200));
    press(&mut test, b"\x15claude\r");
    assert_eq!(
        test.shell.composer.expanded_text(),
        "/model anthropic/claude-x "
    );
    press(&mut test, ESC);
    test.advance(40);
    test.draining(|shell| shell.flush_pending_input().unwrap());
    assert_eq!(test.shell.composer.expanded_text(), kept);
}
