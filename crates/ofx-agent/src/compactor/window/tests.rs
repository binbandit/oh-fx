use ofx_contract::{ChatMessage, ReplaySource, ToolCall, ToolCallId, ToolResultStatus};

use super::*;
use crate::execution_memory::history_turns;

const MODEL: &str = "fixture/model";

fn replay(model: &str) -> ProviderReplay {
    ProviderReplay {
        source: ReplaySource {
            provider: "codex".to_owned(),
            model: model.to_owned(),
        },
        parts_json: format!(
            "[{{\"type\":\"reasoning\",\"text\":\"\",\"encrypted_content\":\"{}\"}}]",
            "r".repeat(80_000)
        ),
    }
}

fn assistant(content: &str, calls: Vec<ToolCall>, replay: Option<ProviderReplay>) -> ChatMessage {
    ChatMessage::Assistant {
        content: Some(content.to_owned()),
        tool_calls: calls,
        provider_replay: replay,
    }
}

fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(id),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

fn result(id: &str, name: &str, output: &str) -> ChatMessage {
    ChatMessage::Tool {
        call_id: ToolCallId::new(id),
        tool_name: name.to_owned(),
        content: output.to_owned(),
        status: ToolResultStatus::Success,
    }
}

struct Conversation {
    history: Vec<ChatMessage>,
    starts: Vec<usize>,
}

impl Conversation {
    fn new() -> Self {
        Self {
            history: Vec::new(),
            starts: Vec::new(),
        }
    }

    fn turn(mut self, user: &str, rest: Vec<ChatMessage>) -> Self {
        self.starts.push(self.history.len());
        self.history.push(ChatMessage::user(user));
        self.history.extend(rest);
        self
    }

    fn chat(self, count: usize) -> Self {
        (0..count).fold(self, |conversation, _| {
            conversation.turn("question", vec![assistant("answer", Vec::new(), None)])
        })
    }

    fn turns(&self) -> Vec<HistoryTurn<'_>> {
        history_turns(&self.history, &self.starts)
    }
}

fn percent(value: u64) -> AutoCompactPercent {
    AutoCompactPercent::new(value).unwrap()
}

#[test]
fn compactor_input_budget_follows_normal_model_capacity() {
    assert_eq!(
        usable_input_tokens(Some(500_000), Some(203_184)),
        Some(296_816)
    );
    assert_eq!(usable_input_tokens(Some(500_000), None), Some(500_000));
    assert_eq!(usable_input_tokens(None, Some(32_000)), None);
}

#[test]
fn retained_context_budgets_provider_replay_on_completed_exchanges() {
    let conversation = Conversation::new().turn(
        "continue",
        vec![
            assistant("one", Vec::new(), Some(replay(MODEL))),
            assistant("two", Vec::new(), Some(replay(MODEL))),
            assistant("three", Vec::new(), Some(replay(MODEL))),
            ChatMessage::user("Summarize what you just did."),
            assistant("", Vec::new(), None),
        ],
    );
    let turns = conversation.turns();
    assert_eq!(turns[0].steps.len(), 3);
    assert_eq!(
        select_recent_context(&turns, 5_990, Some(119_808), MODEL, MAX_KEPT_TURNS).cut,
        Cut {
            turns: 1,
            tool_steps: 0
        }
    );
    let kept = select_recent_context(
        &turns,
        5_990,
        Some(119_808),
        "fixture/other",
        MAX_KEPT_TURNS,
    );
    assert_eq!(kept.cut.tool_steps, 1);
    assert!(kept.tokens > 0 && kept.tokens <= 5_990);
}

#[test]
fn retained_context_budgets_replay_on_standalone_assistant_replies() {
    let conversation = (0..3).fold(Conversation::new(), |conversation, _| {
        conversation.turn(
            "continue",
            vec![assistant("small reply", Vec::new(), Some(replay(MODEL)))],
        )
    });
    assert_eq!(
        select_recent_context(
            &conversation.turns(),
            5_990,
            Some(119_808),
            MODEL,
            MAX_KEPT_TURNS
        )
        .cut
        .turns,
        2
    );
}

#[test]
fn retained_context_keeps_or_compacts_a_parallel_tool_exchange_whole_without_shortening_results() {
    let body = "large output ".repeat(2000);
    let conversation = Conversation::new()
        .turn(
            "old request",
            vec![assistant("old answer", Vec::new(), None)],
        )
        .turn(
            "current request",
            vec![
                assistant("earlier work", Vec::new(), None),
                ChatMessage::user("Summarize what you just did."),
                assistant(
                    "",
                    vec![
                        call("one", "read_file", "{}"),
                        call("two", "read_file", "{}"),
                    ],
                    None,
                ),
                result("one", "read_file", &body),
                result("two", "read_file", &body),
                assistant("", Vec::new(), None),
            ],
        );
    let turns = conversation.turns();
    assert_eq!(
        select_recent_context(&turns, 100_000, None, MODEL, MAX_KEPT_TURNS).cut,
        Cut {
            turns: 1,
            tool_steps: 0
        }
    );
    let everything = Recent {
        cut: Cut {
            turns: 2,
            tool_steps: 0,
        },
        tokens: 0,
    };
    assert_eq!(
        select_recent_context(&turns, 5000, None, MODEL, MAX_KEPT_TURNS),
        everything
    );
    assert_eq!(
        select_recent_context(&turns, 100_000, Some(10_000), MODEL, MAX_KEPT_TURNS),
        everything
    );
    assert_eq!(turns[1].steps[1].results[0].output, body);
    assert_eq!(turns[1].steps[1].results[1].output, body);
}

#[test]
fn retained_context_keeps_at_most_the_requested_number_of_turns() {
    let conversation = Conversation::new().chat(6);
    let turns = conversation.turns();
    assert_eq!(
        select_recent_context(&turns, 100_000, None, MODEL, 6)
            .cut
            .turns,
        1
    );
    assert_eq!(
        select_recent_context(&turns, 100_000, None, MODEL, 4)
            .cut
            .turns,
        2
    );
}

#[test]
fn automatic_compaction_starts_at_the_configured_share_of_usable_input() {
    let size = |percent| Size::of(Some(1_000_000), Some(128_000), percent);
    assert_eq!(size(percent(80)).compact_at_tokens, Some(697_600));
    assert_eq!(size(percent(10)).compact_at_tokens, Some(87_200));
    assert_eq!(
        Size::of(None, Some(128_000), percent(80)).compact_at_tokens,
        None
    );
}

#[test]
fn the_compacted_text_may_use_half_the_compaction_point_less_the_fixed_part_and_the_kept_turns() {
    let size = Size {
        compact_at_tokens: Some(100_000),
        usable_tokens: Some(125_000),
        fixed_tokens: Some(10_000),
        ..Size::default()
    };
    assert_eq!(size.compacted_tokens(5_000), 35_000);
    let crowded = Size {
        fixed_tokens: Some(60_000),
        ..size
    };
    assert_eq!(crowded.compacted_tokens(0), 12_500);
    assert_eq!(Size::default().compacted_tokens(0), usize::MAX);
}

#[test]
fn the_kept_turns_get_a_share_of_a_fifth_of_the_usable_input_less_the_fixed_part() {
    let large = Size {
        compact_at_tokens: Some(800_000),
        usable_tokens: Some(1_000_000),
        fixed_tokens: Some(12_000),
        ..Size::default()
    };
    assert_eq!(large.after_tokens(), 200_000);
    assert_eq!(large.conversation_tokens(), 188_000);
    let low_point = Size {
        compact_at_tokens: Some(100_000),
        usable_tokens: Some(1_000_000),
        ..Size::default()
    };
    assert_eq!(low_point.after_tokens(), 50_000);
    let unmeasured = Size {
        fixed_tokens: None,
        ..large
    };
    assert_eq!(unmeasured.conversation_tokens(), 100_000);
    let crowded = Size {
        fixed_tokens: Some(190_000),
        ..large
    };
    assert_eq!(crowded.conversation_tokens(), 50_000);
    assert_eq!(Size::default().conversation_tokens(), usize::MAX);
    let counted_more = Size {
        correction: Some(Correction {
            estimated: 30_000,
            measured: 40_000,
        }),
        ..large
    };
    assert_eq!(counted_more.conversation_tokens(), 141_000);
    assert_eq!(counted_more.summary_request_tokens(), 750_000);

    let conversation = Conversation::new().chat(6);
    let window = choose(&conversation.turns(), false, large, MODEL)
        .unwrap()
        .unwrap();
    assert_eq!(
        percent_of(large.conversation_tokens(), KEPT_PERCENT),
        75_200
    );
    assert!(window.kept_used > 0 && window.kept_used <= 75_200);
}

#[test]
fn the_window_keeps_the_newest_turns_and_compacts_the_rest() {
    let conversation = Conversation::new().chat(6);
    let turns = conversation.turns();
    let window = split(&turns, false, 100_000, None, MODEL);
    assert_eq!(
        window.cut,
        Cut {
            turns: 2,
            tool_steps: 0
        }
    );
    let everything = split(&turns, false, 0, None, MODEL);
    assert_eq!(
        everything.cut,
        Cut {
            turns: 6,
            tool_steps: 0
        }
    );
}

#[test]
fn the_window_ends_an_unfinished_turn_at_its_completed_exchange() {
    let arguments = "x".repeat(32_000);
    let running = Conversation::new().turn(
        "write once",
        vec![
            assistant(
                "",
                vec![call("large-write", "write_file", &arguments)],
                None,
            ),
            result("large-write", "write_file", "written"),
        ],
    );
    let active = split(&running.turns(), true, 800, Some(4_000), MODEL);
    assert_eq!(
        active.cut,
        Cut {
            turns: 0,
            tool_steps: 1
        }
    );
    assert!(active.has_older() && active.splits_last_turn());
    let saved = split(&running.turns(), false, 800, Some(4_000), MODEL);
    assert_eq!(
        saved.cut,
        Cut {
            turns: 1,
            tool_steps: 0
        }
    );
}

#[test]
fn nothing_is_compacted_unless_it_is_due_or_required() {
    let conversation = Conversation::new().turn("hi", vec![assistant("hello", Vec::new(), None)]);
    let turns = conversation.turns();
    let size = Size {
        compact_at_tokens: Some(100_000),
        usable_tokens: Some(120_000),
        ..Size::default()
    };
    assert_eq!(choose(&turns, false, size, MODEL), Ok(None));
    let forced = choose(
        &turns,
        false,
        Size {
            request_tokens: Some(130_000),
            ..size
        },
        MODEL,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        forced.cut,
        Cut {
            turns: 1,
            tool_steps: 0
        }
    );
    assert_eq!(forced.kept_used, 0);
    assert_eq!(
        choose(
            &[],
            false,
            Size {
                overflow: true,
                request_tokens: Some(130_000),
                ..size
            },
            MODEL
        ),
        Err(CompactionError::ContextCapacityExceeded)
    );
    let fresh = Conversation::new().turn("only the prompt", Vec::new());
    assert_eq!(
        choose(
            &fresh.turns(),
            true,
            Size {
                request_tokens: Some(130_000),
                ..size
            },
            MODEL
        ),
        Err(CompactionError::ContextCapacityExceeded)
    );
}

#[test]
fn a_summary_request_may_use_the_usable_input_less_after_an_overflow() {
    assert_eq!(Size::default().summary_request_tokens(), usize::MAX);
    let size = Size {
        compact_at_tokens: Some(96_000),
        usable_tokens: Some(120_000),
        request_tokens: Some(110_000),
        ..Size::default()
    };
    assert_eq!(size.summary_request_tokens(), 120_000);
    let rejected = Size {
        request_tokens: Some(100_000),
        overflow: true,
        ..size
    };
    assert_eq!(rejected.summary_request_tokens(), 75_000);
}

#[test]
fn a_request_after_the_conversation_gets_what_the_conversation_leaves() {
    let size = Size {
        compact_at_tokens: Some(96_000),
        usable_tokens: Some(120_000),
        request_tokens: Some(110_000),
        ..Size::default()
    };
    assert_eq!(size.room_after_conversation(), Some(10_000));
    let counted_more = Size {
        correction: Some(Correction {
            estimated: 30_000,
            measured: 40_000,
        }),
        ..size
    };
    assert_eq!(counted_more.room_after_conversation(), Some(7_500));
    let unmeasured = Size {
        request_tokens: None,
        ..size
    };
    assert_eq!(unmeasured.room_after_conversation(), None);
    let full = Size {
        request_tokens: Some(120_000),
        ..size
    };
    assert_eq!(full.room_after_conversation(), None);
    let rejected = Size {
        request_tokens: Some(100_000),
        overflow: true,
        ..size
    };
    assert_eq!(rejected.room_after_conversation(), None);
}

#[test]
fn a_request_is_due_once_it_reaches_the_compaction_point() {
    let size = Size {
        compact_at_tokens: Some(1_000),
        request_tokens: Some(999),
        ..Size::default()
    };
    assert!(!size.due());
    assert!(
        Size {
            request_tokens: Some(1_000),
            ..size
        }
        .due()
    );
    assert!(
        !Size {
            compact_at_tokens: None,
            ..size
        }
        .due()
    );
    assert!(
        !Size {
            request_tokens: None,
            ..size
        }
        .due()
    );
}
