use ofx_contract::{StatuslineItem, TurnId, TurnOutcome, UiEvent, Usage};

use super::super::test_shell::TestShell;

fn context_shell() -> TestShell {
    TestShell::start_with(|options| options.statusline.set(StatuslineItem::Context, true))
}

fn started(turn: u64) -> UiEvent {
    UiEvent::TurnStarted {
        turn_id: TurnId::new(turn),
    }
}

fn usage(turn: u64, input_tokens: u64, context_window: Option<u32>) -> UiEvent {
    UiEvent::UsageReported {
        turn_id: TurnId::new(turn),
        usage: Usage {
            input_tokens: Some(input_tokens),
            output_tokens: Some(1),
        },
        context_window,
    }
}

fn finished(turn: u64) -> UiEvent {
    UiEvent::TurnFinished {
        turn_id: TurnId::new(turn),
        outcome: TurnOutcome::Completed,
    }
}

#[test]
fn a_resumed_session_starts_its_context_usage_over() {
    let mut test = context_shell();
    test.submit("go");
    test.deliver(started(1));
    test.deliver(usage(1, 12_000, Some(128_000)));
    test.deliver(finished(1));
    let screen = test.screen();
    assert!(screen.contains("auto · model-a · 12k/128k 9%"), "{screen}");
    test.deliver(UiEvent::SessionResumed {
        history: Vec::new(),
    });
    let screen = test.screen();
    assert!(screen.contains("auto · model-a"), "{screen}");
    assert!(!screen.contains("12k"), "{screen}");
}

#[test]
fn a_model_picked_during_a_turn_ignores_the_old_models_window() {
    let mut test = context_shell();
    test.submit("go");
    test.deliver(started(1));
    test.deliver(usage(1, 1_000, Some(128_000)));
    assert!(test.screen().contains("model-a · 1k/128k 0%"));
    test.deliver(UiEvent::ModelSelected {
        model: "model-b".to_owned(),
    });
    test.deliver(usage(1, 2_000, Some(128_000)));
    let screen = test.screen();
    assert!(screen.contains("model-b · 2k"), "{screen}");
    assert!(!screen.contains("/128k"), "{screen}");
    test.deliver(finished(1));
    test.submit("next");
    test.deliver(started(2));
    test.deliver(usage(2, 3_000, Some(32_000)));
    let screen = test.screen();
    assert!(screen.contains("model-b · 3k/32k 9%"), "{screen}");
}
