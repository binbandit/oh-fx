use super::*;

const SAMPLE_NOTES: &str = "Turn 1
In between: Ran the build and found the missing semicolon.
T1: ran the build; it stopped at src/a.zig:4

Facts:
F1 (T1): the build fails on a missing semicolon at src/a.zig:4";

const SAMPLE_FACT: &str = "F1 (T1): the build fails on a missing semicolon at src/a.zig:4";

#[derive(Default)]
struct FakeModel {
    reply: Option<&'static str>,
    replies: Vec<&'static str>,
    fail: Option<CompactionError>,
    fail_after_conversation: bool,
    calls: usize,
    after_conversation_calls: usize,
    largest: usize,
    seen_system: String,
    seen_user: String,
}

impl FakeModel {
    fn replying(reply: &'static str) -> Self {
        Self {
            reply: Some(reply),
            ..Self::default()
        }
    }

    fn scripted(replies: &[&'static str]) -> Self {
        Self {
            replies: replies.to_vec(),
            ..Self::default()
        }
    }

    fn failing(error: CompactionError) -> Self {
        Self {
            fail: Some(error),
            ..Self::default()
        }
    }
}

impl SummaryModel for FakeModel {
    fn summarize<'a>(
        &'a mut self,
        prompt: Prompt<'a>,
    ) -> BoxFuture<'a, Result<String, CompactionError>> {
        Box::pin(async move {
            self.calls += 1;
            self.largest = self.largest.max(tokens(&[prompt.system, prompt.user]));
            prompt.system.clone_into(&mut self.seen_system);
            prompt.user.clone_into(&mut self.seen_user);
            if let Some(error) = self.fail {
                return Err(error);
            }
            if prompt.after_conversation {
                self.after_conversation_calls += 1;
                if self.fail_after_conversation {
                    return Err(CompactionError::ModelFailed);
                }
            }
            if !self.replies.is_empty() {
                let index = self.calls.min(self.replies.len()) - 1;
                return Ok(self.replies[index].to_owned());
            }
            Ok(self.reply.unwrap_or(SAMPLE_NOTES).to_owned())
        })
    }
}

fn request<'a>(turns: &'a [Turn<'a>]) -> Request<'a> {
    Request {
        earlier: None,
        turns,
        last_turn_open: false,
        kept: &[],
        max_prompt_tokens: usize::MAX,
        conversation_room: None,
        max_text_tokens: usize::MAX,
    }
}

fn after<'a>(earlier: &'a Payload, turns: &'a [Turn<'a>]) -> Request<'a> {
    Request {
        earlier: Some(earlier),
        ..request(turns)
    }
}

fn run(request: Request<'_>, model: &mut FakeModel) -> Result<Summary, CompactionError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(compact(request, model))
}

fn call<'a>(id: &'a str, name: &'a str, arguments: &'a str) -> Item<'a> {
    Item::ToolCall(ToolCall {
        id,
        name,
        arguments,
    })
}

fn result<'a>(call_id: &'a str, name: &'a str, output: &'a str) -> Item<'a> {
    Item::ToolResult(ToolResult {
        call_id,
        name,
        output,
        failed: false,
    })
}

fn sample() -> Vec<Turn<'static>> {
    vec![
        Turn {
            user: "Fix the build.\nIt fails on main.",
            items: vec![
                Item::Assistant("I'll run the build first."),
                call("call-1", "shell", "{\"command\":\"zig build\"}"),
                result("call-1", "shell", "error: missing semicolon at src/a.zig:4"),
                Item::Assistant("Found it: a missing semicolon at src/a.zig:4."),
            ],
        },
        Turn {
            user: "thanks",
            items: vec![Item::Assistant("You're welcome.")],
        },
    ]
}

fn worked_turn<'a>(user: &'a str, call_id: &'a str, output: &'a str) -> Turn<'a> {
    Turn {
        user,
        items: vec![
            Item::Assistant("Checking."),
            call(call_id, "shell", "{\"command\":\"make\"}"),
            result(call_id, "shell", output),
            Item::Assistant("Done."),
        ],
    }
}

fn entry_ids(payload: &Payload) -> Vec<&str> {
    payload
        .entries
        .iter()
        .map(|entry| entry.id.as_str())
        .collect()
}

#[test]
fn user_messages_and_final_replies_stay_exact_beside_notes_and_a_line_per_tool_call() {
    let turns = sample();
    let mut model = FakeModel::default();
    let summary = run(request(&turns), &mut model).unwrap();

    let shown = &summary.compacted.turns;
    assert_eq!(shown.len(), 2);
    assert_eq!(shown[0].users[0], "Fix the build.\nIt fails on main.");
    assert_eq!(
        shown[0].final_reply,
        "Found it: a missing semicolon at src/a.zig:4."
    );
    assert_eq!(
        shown[0].work,
        "Ran the build and found the missing semicolon."
    );
    assert_eq!(shown[0].tools.len(), 1);
    assert_eq!(shown[0].tools[0].line, "shell zig build (39 bytes)");
    assert_eq!(
        shown[0].tools[0].why,
        "ran the build; it stopped at src/a.zig:4"
    );
    assert_eq!(shown[1].final_reply, "You're welcome.");
    assert_eq!(summary.compacted.turn_count, 2);
    assert_eq!(summary.compacted.tool_count, 1);
    assert_eq!(summary.compacted.entries.len(), 1);
    assert_eq!(summary.compacted.entries[0].text, SAMPLE_FACT);

    let expected = format!(
        "Turn 1
User 1:
Fix the build.
It fails on main.

Assistant 1, in between:
Ran the build and found the missing semicolon.

Tools:
  T1 shell zig build (39 bytes): ran the build; it stopped at src/a.zig:4

Assistant 1, final reply:
Found it: a missing semicolon at src/a.zig:4.

Turn 2
User 2:
thanks

Assistant 2, final reply:
You're welcome.

Facts of the session:
{SAMPLE_FACT}

"
    );
    assert!(summary.text.contains(&expected), "{}", summary.text);
    assert!(!summary.text.contains("I'll run the build first."));
    assert!(!summary.text.contains("error: missing semicolon"));
}

#[test]
fn a_reply_that_skips_turns_with_work_is_asked_once_more_for_just_those() {
    let turns = [
        worked_turn("first", "call-1", "one"),
        worked_turn("second", "call-2", "two"),
        worked_turn("third", "call-3", "three"),
        Turn {
            user: "thanks",
            items: vec![Item::Assistant("Sure.")],
        },
    ];
    let mut model = FakeModel::scripted(&[
        "Turn 3\nIn between: Ran make a third time.\nT3: third build\n\nFacts:\nF1 (T3): the third build printed three",
        "**Turn 1 (T1)**\nIn-between: Ran make.\n\nTurn 2: the second build\nIn between: none\n\nFacts:\nF2 (T1): the first build printed one",
    ]);
    let summary = run(request(&turns), &mut model).unwrap();

    assert_eq!(model.calls, 2);
    let follow_up = &model.seen_user;
    assert!(follow_up.starts_with("[Turn 1]\n[User]\nfirst\n"));
    assert!(follow_up.ends_with(
        "Your notes on the turns above left some out. Write the notes for only these turns now, each heading followed by its notes:\n\nTurn 1 (T1)\nTurn 2 (T2)\n\nUnder each heading, In between: with what the assistant did before its final reply, then a line for every tool call, starting with its ID, on why it was used and what it showed.\n\nThen any new entries from those turns under the same sections, each starting with its ID and the turn or tool call it comes from. The highest IDs so far: F1. Number new entries after them. Write only these notes."
    ), "{follow_up}");
    assert!(!follow_up.contains("Write the compaction notes for the new turns above"));
    let shown = &summary.compacted.turns;
    assert_eq!(shown[0].work, "Ran make.");
    assert_eq!(shown[1].work, "");
    assert_eq!(shown[2].work, "Ran make a third time.");
    assert_eq!(shown[2].tools[0].why, "third build");
    assert_eq!(summary.compacted.entries.len(), 2);
    assert_eq!(summary.compacted.entries[1].id, "F2");

    let mut facts = FakeModel::scripted(&[
        "Facts:\nF1 (T1): the first build printed one",
        "Turn 1\nIn between: Ran make.\n\nTurn 2\nIn between: Ran make again.\n\nTurn 3\nIn between: Ran make a third time.",
    ]);
    let only_facts = run(request(&turns), &mut facts).unwrap();
    assert_eq!(facts.calls, 2);
    assert!(facts.seen_user.ends_with(
        "\n\nTurn 1 (T1)\nTurn 2 (T2)\nTurn 3 (T3)\n\nUnder each heading, In between: with what the assistant did before its final reply, then a line for every tool call, starting with its ID, on why it was used and what it showed.\n\nThen any new entries from those turns under the same sections, each starting with its ID and the turn or tool call it comes from. The highest IDs so far: F1. Number new entries after them. Write only these notes."
    ), "{}", facts.seen_user);
    assert_eq!(only_facts.compacted.turns[1].work, "Ran make again.");
    assert_eq!(only_facts.compacted.entries[0].id, "F1");

    let mut prose = FakeModel::replying("The builds ran.");
    let other = run(request(&turns), &mut prose).unwrap();
    assert_eq!(prose.calls, 2);
    assert_eq!(other.compacted.turns[3].work, "The builds ran.");
    assert_eq!(other.compacted.turns[2].work, "The builds ran.");

    let single = [worked_turn("first", "call-1", "one")];
    let mut once = FakeModel::replying("The build ran.");
    let kept = run(request(&single), &mut once).unwrap();
    assert_eq!(once.calls, 1);
    assert_eq!(kept.compacted.turns[0].work, "The build ran.");
}

#[test]
fn after_the_conversation_the_model_reads_only_the_request_with_every_turn_findable() {
    let turns = sample();
    let mut model = FakeModel::default();
    let summary = run(
        Request {
            conversation_room: Some(100_000),
            ..request(&turns)
        },
        &mut model,
    )
    .unwrap();

    assert_eq!(model.calls, 1);
    assert_eq!(model.after_conversation_calls, 1);
    assert_eq!(model.seen_system, "");
    let seen = &model.seen_user;
    assert!(seen.starts_with(&format!(
        "{SYSTEM_PROMPT}\n\nWrite the compaction notes for the turns of the conversation above that are listed below;"
    )));
    assert!(!seen.contains("[Turn 1]"));
    assert!(!seen.contains("missing semicolon"));
    assert!(seen.contains("Answer with text only and call no tools."));
    assert!(seen.contains("This request comes from oh-fx, not from the user"));
    assert!(seen.contains(
        "each followed by its notes:\n\nTurn 1 (T1), which begins \u{201c}Fix the build. It fails on main.\u{201d}\n  T1 shell: zig build\nTurn 2 (no tool calls), which begins \u{201c}thanks\u{201d}\n\nUnder each heading above are the turn's tool calls in order"
    ));
    assert!(summary.text.contains(SAMPLE_FACT));
    assert_eq!(
        summary.compacted.turns[0].work,
        "Ran the build and found the missing semicolon."
    );
}

#[test]
fn a_request_after_the_conversation_that_fails_or_does_not_fit_writes_the_turns_out() {
    let turns = sample();
    let mut failing = FakeModel {
        fail_after_conversation: true,
        ..FakeModel::default()
    };
    let summary = run(
        Request {
            conversation_room: Some(100_000),
            ..request(&turns)
        },
        &mut failing,
    )
    .unwrap();
    assert_eq!(failing.calls, 2);
    assert_eq!(failing.after_conversation_calls, 1);
    assert_eq!(failing.seen_system, SYSTEM_PROMPT);
    assert!(failing.seen_user.starts_with("[Turn 1]\n"));
    assert!(summary.text.contains(SAMPLE_FACT));

    let mut cramped = FakeModel::default();
    run(
        Request {
            conversation_room: Some(50),
            ..request(&turns)
        },
        &mut cramped,
    )
    .unwrap();
    assert_eq!(cramped.calls, 1);
    assert_eq!(cramped.after_conversation_calls, 0);
    assert!(cramped.seen_user.starts_with("[Turn 1]\n"));
}

#[test]
fn a_follow_up_after_the_conversation_lists_only_the_skipped_turns_findable() {
    let turns = [
        worked_turn("first", "call-1", "one"),
        worked_turn("second", "call-2", "two"),
    ];
    let mut model = FakeModel::scripted(&[
        "Turn 2 (T2)\nIn between: Ran make again.\nT2: second build",
        "Turn 1 (T1)\nIn between: Ran make.\nT1: first build",
    ]);
    let summary = run(
        Request {
            conversation_room: Some(100_000),
            ..request(&turns)
        },
        &mut model,
    )
    .unwrap();
    assert_eq!(model.after_conversation_calls, 2);
    let follow_up = &model.seen_user;
    assert!(follow_up.starts_with(&format!(
        "{SYSTEM_PROMPT}\n\nYour notes on the turns above left some out."
    )));
    assert!(follow_up.contains(
        "each heading followed by its notes:\n\nTurn 1 (T1), which begins \u{201c}first\u{201d}\n  T1 shell: make\n\n"
    ));
    assert!(!follow_up.contains("Turn 2"));
    assert_eq!(summary.compacted.turns[0].work, "Ran make.");
    assert_eq!(summary.compacted.turns[0].tools[0].why, "first build");
}

#[test]
fn the_conversation_serves_only_a_request_for_every_turn() {
    let output = "x".repeat(4000);
    let turns = [
        worked_turn("first", "call-1", &output),
        worked_turn("second", "call-2", &output),
    ];
    let one = turn_tokens(&turns[0]);
    let room = REQUEST_OVERHEAD_TOKENS + tokens(&[SYSTEM_PROMPT]) + one + one / 2;
    let mut model = FakeModel::scripted(&[
        "Turn 1\nIn between: Ran the first build.",
        "Turn 2\nIn between: Ran the second build.",
    ]);
    let summary = run(
        Request {
            max_prompt_tokens: room,
            conversation_room: Some(100_000),
            ..request(&turns)
        },
        &mut model,
    )
    .unwrap();
    assert_eq!(model.calls, 2);
    assert_eq!(model.after_conversation_calls, 0);
    assert_eq!(summary.compacted.turns[1].work, "Ran the second build.");
}

#[test]
fn the_models_notes_are_checked_against_the_turns_and_tool_calls_they_name() {
    let turns = [Turn {
        user: "run the tests",
        items: vec![
            call("c1", "shell", "{\"command\":\"zig build test\"}"),
            result(
                "c1",
                "shell",
                "{\"exit_code\":1,\"output\":\"3 of 120 tests failed in src/lexer.zig\"}",
            ),
            Item::Assistant("Three tests fail in the lexer."),
        ],
    }];
    let mut model = FakeModel::replying(
        "Turn 1\nIn between: Ran the suite.\nT1: ran the tests; all 120 pass\n\nFacts:\nF1 (T1): 3 of 120 tests fail in src/lexer.zig\nF2 (T1): the failures are in src/parser.zig\nF3 (T7): the build is slow",
    );
    let summary = run(request(&turns), &mut model).unwrap();
    assert_eq!(
        summary.compacted.turns[0].tools[0].why,
        "ran the tests; all 120 pass [check: T1 failed]"
    );
    let entries = &summary.compacted.entries;
    assert_eq!(
        entries[0].text,
        "F1 (T1): 3 of 120 tests fail in src/lexer.zig"
    );
    assert_eq!(
        entries[1].text,
        "F2 (T1): the failures are in src/parser.zig [check: not in the saved turns or tool calls: src/parser.zig]"
    );
    assert_eq!(
        entries[2].text,
        "F3 (T7): the build is slow [check: T7 does not exist]"
    );
    assert!(summary.text.contains("A note or entry marked [check: ...]"));
}

#[test]
fn entries_with_a_bold_or_checked_id_are_saved_from_the_id_on() {
    let turns = [Turn {
        user: "run the tests",
        items: vec![
            call("c1", "shell", "{\"command\":\"zig build test\"}"),
            result(
                "c1",
                "shell",
                "{\"exit_code\":1,\"output\":\"3 of 120 tests failed in src/lexer.zig\"}",
            ),
            Item::Assistant("Three tests fail in the lexer."),
        ],
    }];
    let mut model = FakeModel::replying(
        "Turn 1\nIn between: Ran the suite.\nT1: ran the tests\n\nFacts:\n- **F1** (T1): 3 of 120 tests fail in src/lexer.zig\n\nStatus:\n- [x] S1 (T1): fixing the lexer",
    );
    let summary = run(request(&turns), &mut model).unwrap();
    let entries = &summary.compacted.entries;
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0].text,
        "F1 (T1): 3 of 120 tests fail in src/lexer.zig"
    );
    assert_eq!(entries[1].text, "S1 (T1): fixing the lexer");
}

fn pending<'a>(
    name: &'a str,
    call: Option<ToolCall<'a>>,
    result: Option<ToolResult<'a>>,
) -> PendingTool<'a> {
    PendingTool {
        number: 1,
        name,
        call,
        result,
    }
}

#[test]
fn the_tool_line_says_what_code_knows_the_call_how_it_ended_and_its_size() {
    let shell = ToolCall {
        id: "c",
        name: "shell",
        arguments: "{\"command\":\"zig build test\"}",
    };
    let output = "{\"state\":\"completed\",\"exit_code\":1,\"output\":\"2 failed\"}";
    assert_eq!(
        code_line(&pending(
            "shell",
            Some(shell),
            Some(ToolResult {
                call_id: "c",
                name: "shell",
                output,
                failed: true
            })
        )),
        format!(
            "shell zig build test (failed, exit 1, {} bytes)",
            output.len()
        )
    );
    assert_eq!(
        code_line(&pending(
            "read_file",
            Some(ToolCall {
                id: "r",
                name: "read_file",
                arguments: "{\"path\":\"src/a.zig\"}"
            }),
            Some(ToolResult {
                call_id: "r",
                name: "read_file",
                output: "a\nb\nc",
                failed: false
            })
        )),
        "read_file src/a.zig (3 lines)"
    );
    assert_eq!(
        code_line(&pending("shell", Some(shell), None)),
        "shell zig build test (no result)"
    );
    let arguments = format!("{{\"content\":\"{}\"}}", "\u{e9}".repeat(200));
    let long = code_line(&pending(
        "write_file",
        Some(ToolCall {
            id: "w",
            name: "write_file",
            arguments: &arguments,
        }),
        None,
    ));
    assert!(long.ends_with("\u{2026} (no result)"));
    assert_eq!(exit_code("{\"exit_code\":-1}"), Some(-1));
    assert_eq!(exit_code("exit_code: 3"), None);
}

#[test]
fn the_model_reads_the_new_turns_as_they_are_and_is_asked_only_for_their_notes() {
    let turns = sample();
    let mut model = FakeModel::default();
    run(request(&turns), &mut model).unwrap();
    assert_eq!(model.calls, 1);
    assert_eq!(model.seen_system, SYSTEM_PROMPT);
    let seen = &model.seen_user;
    for part in [
        "[Turn 1]\n[User]\nFix the build.\nIt fails on main.\n",
        "[Assistant]\nI'll run the build first.\n",
        "[Tool call T1: shell]\n{\"command\":\"zig build\"}\n",
        "[Tool result T1: shell]\nerror: missing semicolon at src/a.zig:4\n",
        "[Turn 2]\n[User]\nthanks\n",
        "Write the compaction notes for the new turns above. ",
        "each followed by its notes:\n\nTurn 1 (T1)\n\nUnder each heading:\n",
        "The tool calls will not be available later",
        " Number each kind from 1.",
    ] {
        assert!(seen.contains(part), "{part}");
    }
    assert!(seen.starts_with("[Turn 1]\n"));
    assert!(!seen.contains("turn in progress"));
    assert!(seen.ends_with("Write only these notes."));
}

#[test]
fn the_skills_and_mcp_tools_used_are_listed_from_the_tool_calls() {
    let turns = [Turn {
        user: "check the issue",
        items: vec![
            call("a", "skill", "{\"location\":\"skill:ab:4/fx-conventions\"}"),
            result("a", "skill", "Conventions."),
            call("b", "mcp_linear_get_issue", "{\"id\":\"FX-1\"}"),
            result("b", "mcp_linear_get_issue", "FX-1: crash"),
            Item::Assistant("FX-1 is a crash."),
        ],
    }];
    let mut model = FakeModel::default();
    let summary = run(request(&turns), &mut model).unwrap();
    assert_eq!(summary.compacted.used.len(), 2);
    assert!(summary.text.contains("Skills and MCP tools used:\n- skill skill:ab:4/fx-conventions: 1 call, T1\n- MCP tool mcp_linear_get_issue: 1 call, T2\n"));
}

#[test]
fn turns_with_nothing_to_summarize_need_no_model_call() {
    let turns = [
        Turn {
            user: "what is 2+2?",
            items: vec![Item::Assistant("4")],
        },
        Turn {
            user: "yes",
            items: vec![Item::Assistant("ok")],
        },
        Turn {
            user: "yes",
            items: vec![Item::Assistant("ok")],
        },
    ];
    let mut model = FakeModel::default();
    let summary = run(request(&turns), &mut model).unwrap();
    assert_eq!(model.calls, 0);
    assert_eq!(summary.compacted.turns.len(), 3);
    assert_eq!(summary.compacted.turns[2].users[0], "yes");
    assert_eq!(summary.compacted.turns[2].final_reply, "ok");
}

fn tests_turn() -> Vec<Turn<'static>> {
    vec![Turn {
        user: "now run the tests",
        items: vec![
            call("call-2", "shell", "{\"command\":\"zig build test\"}"),
            result("call-2", "shell", "All 12 tests passed."),
            Item::Assistant("All 12 tests pass."),
        ],
    }]
}

#[test]
fn a_rule_may_quote_the_users_answer_to_a_question_but_not_the_question() {
    let asked = [Turn {
        user: "plan the next blocks",
        items: vec![
            call("q", "ask_user_question", "{}"),
            result(
                "q",
                "ask_user_question",
                "[{\"question\":\"Which block comes first?\",\"answer\":\"Block 6 before Block 5\"}]",
            ),
            Item::Assistant("Block 6 goes first."),
        ],
    }];
    let mut model = FakeModel::replying(
        "Turn 1\nIn between: asked which block comes first.\nT1: the user put block 6 first\n\nRules:\n- R1 (T1): \"Block 6 before Block 5\"\n- R2 (T1): \"Which block comes first?\"",
    );
    let summary = run(request(&asked), &mut model).unwrap();
    let entries = &summary.compacted.entries;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].text, "R1 (T1): \"Block 6 before Block 5\"");
    assert_eq!(
        entries[1].text,
        "R2 (T1): \"Which block comes first?\" [check: not the user's exact words]"
    );
}

#[test]
fn a_questions_result_gives_the_users_answers_not_the_questions() {
    assert_eq!(
        question_answers(
            "[{\"question\":\"Which block first?\",\"answer\":\"Block 6 before Block 5\"},{\"question\":\"Push it?\",\"answer\":\"Legitimate, push\"}]"
        ),
        ["Block 6 before Block 5", "Legitimate, push"]
    );
    assert!(question_answers("The user dismissed the question.").is_empty());
    assert!(question_answers("[{\"question\":\"Push it?\"}]").is_empty());

    let output = "[{\"question\":\"Push it?\",\"answer\":\"Legitimate, push\"}]";
    let turns = [Turn {
        user: "ship it",
        items: vec![
            result("q", "ask_user_question", output),
            result("s", "shell", output),
        ],
    }];
    assert_eq!(
        user_messages(&request(&turns)),
        ["ship it", "Legitimate, push"]
    );
}

#[test]
fn a_value_read_in_the_conversation_that_stays_after_the_cut_is_not_marked() {
    let kept = [Turn {
        user: "",
        items: vec![
            call(
                "k",
                "shell",
                "{\"command\":\"rg -n subagentStatusLine src\"}",
            ),
            result(
                "k",
                "shell",
                "src/ui/status_line.zig:40:fn subagentStatusLine(",
            ),
        ],
    }];
    let fact = "F2 (T1): the status line comes from `subagentStatusLine` in src/ui/status_line.zig";
    let turns = sample();

    let mut model = FakeModel::replying(
        "Turn 1\nIn between: Ran the build and found the missing semicolon.\nT1: ran the build; it stopped at src/a.zig:4\n\nFacts:\nF1 (T1): the build fails on a missing semicolon at src/a.zig:4\nF2 (T1): the status line comes from `subagentStatusLine` in src/ui/status_line.zig",
    );
    let with_kept = run(
        Request {
            kept: &kept,
            ..request(&turns)
        },
        &mut model,
    )
    .unwrap();
    assert_eq!(with_kept.compacted.entries[1].text, fact);

    let without = run(request(&turns), &mut model).unwrap();
    assert_eq!(
        without.compacted.entries[1].text,
        format!(
            "{fact} [check: not in the saved turns or tool calls: subagentStatusLine, src/ui/status_line.zig]"
        )
    );
}

#[test]
fn a_status_that_finishes_an_open_entry_marks_it_replaced() {
    let first_turns = sample();
    let mut first_model = FakeModel::replying(
        "Turn 1\nIn between: Ran the build and found the missing semicolon.\nT1: ran the build; it stopped at src/a.zig:4\n\nFacts:\nF1 (T1): the build fails on a missing semicolon at src/a.zig:4\n\nOpen:\nO1 (M1): run the tests after the fix",
    );
    let first = run(request(&first_turns), &mut first_model).unwrap();

    let second_turns = tests_turn();
    let mut second_model = FakeModel::replying(
        "Turn 3\nIn between: ran the tests.\nT2: ran the tests; all 12 pass\n\nStatus:\nS1 (T2): all 12 tests pass; replaces O1",
    );
    let second = run(after(&first.compacted, &second_turns), &mut second_model).unwrap();
    assert!(
        second
            .text
            .contains("O1 (M1): run the tests after the fix (replaced by S1)\n"),
        "{}",
        second.text
    );
    assert!(
        second
            .text
            .contains("S1 (T2): all 12 tests pass; replaces O1\n")
    );
}

#[test]
fn compacting_again_keeps_the_earlier_turns_and_entries_and_numbers_on() {
    let first_turns = sample();
    let mut first_model = FakeModel::default();
    let first = run(request(&first_turns), &mut first_model).unwrap();

    let notes = "## Turn 3
In between: none
T2: ran the tests; all 12 pass
T1: a rewrite of an old note

Turn 1
In between: a rewrite of an old turn

Rules:
- R1 (turn 1): \"It fails on main\"
- R2 (turn 3): \"never skip the tests\"

Facts:
F1 (T2): a rewrite of an old fact
F2 (T2): the suite has 12 tests

Status:
S1 (T2): the build and the tests pass";
    let second_turns = tests_turn();
    let mut second_model = FakeModel::replying(notes);
    let second = run(after(&first.compacted, &second_turns), &mut second_model).unwrap();

    let seen = &second_model.seen_user;
    assert!(seen.starts_with(&format!(
        "[Rules, facts, decisions and status so far]\n{SAMPLE_FACT}\n\n[Turn 3]\n[User]\nnow run the tests\n"
    )));
    assert!(seen.contains("each followed by its notes:\n\nTurn 3 (T2)\n\n"));
    assert!(seen.contains(" The highest IDs so far: F1. Number new entries after them."));

    let compacted = &second.compacted;
    assert_eq!(compacted.turns.len(), 3);
    assert_eq!(compacted.turns[..2], first.compacted.turns[..]);
    assert_eq!(compacted.turns[2].number, 3);
    assert_eq!(compacted.turns[2].first_tool, 2);
    assert_eq!(compacted.turns[2].work, "");
    assert_eq!(
        compacted.turns[2].tools[0].why,
        "ran the tests; all 12 pass"
    );
    assert_eq!(compacted.turn_count, 3);
    assert_eq!(compacted.tool_count, 2);
    assert_eq!(entry_ids(compacted), ["F1", "R1", "R2", "F3", "F2", "S1"]);
    assert_eq!(
        compacted.entries[1].text,
        "R1 (turn 1): \"It fails on main\""
    );
    assert_eq!(
        compacted.entries[2].text,
        "R2 (turn 3): \"never skip the tests\" [check: not the user's exact words]"
    );
    assert_eq!(
        compacted.entries[3].text,
        "F3 (T2): a rewrite of an old fact"
    );
    assert!(second.text.contains("User 1:\nFix the build."));
    assert!(second.text.contains(SAMPLE_FACT));
}

#[test]
fn without_a_store_nothing_is_folded_so_plain_turns_need_no_model_call() {
    let first_turns = sample();
    let mut first_model = FakeModel::default();
    let first = run(request(&first_turns), &mut first_model).unwrap();
    let chat = [Turn {
        user: "great",
        items: vec![Item::Assistant("Glad it works.")],
    }];
    let mut unused = FakeModel::default();
    let unsaved = run(after(&first.compacted, &chat), &mut unused).unwrap();
    assert_eq!(unused.calls, 0);
    assert_eq!(unsaved.compacted.entries.len(), 1);
    assert_eq!(unsaved.compacted.turns.len(), 3);
}

#[test]
fn every_turn_stays_each_long_user_message_and_final_reply_whole() {
    let pasted = format!("PASTE_START {}PASTE_END", "log line ".repeat(2000));
    let plan = "PLAN ".repeat(400);
    let turns: Vec<Turn<'_>> = (0..12)
        .map(|_| Turn {
            user: &pasted,
            items: vec![
                call("c", "shell", "{\"command\":\"make\"}"),
                result("c", "shell", "ok"),
                Item::Assistant(&plan),
            ],
        })
        .collect();
    let mut model = FakeModel::default();
    let summary = run(
        Request {
            max_prompt_tokens: 2000,
            ..request(&turns)
        },
        &mut model,
    )
    .unwrap();
    let shown = &summary.compacted.turns;
    assert_eq!(shown.len(), turns.len());
    for (turn, number) in shown.iter().zip(1..) {
        assert_eq!(turn.number, number);
        assert_eq!(turn.users[0], pasted);
        assert_eq!(turn.final_reply, plan);
    }
    assert!(model.seen_user.contains(" bytes left out here]"));
    assert!(!summary.text.contains("left out"));
    assert!(summary.text.contains(&format!("User 1:\n{pasted}\n")));
    assert!(
        summary
            .text
            .contains(&format!("Assistant 12, final reply:\n{plan}\n"))
    );
}

#[test]
fn only_a_text_over_its_room_clips_its_longest_messages() {
    let long_reply = format!("REPLY_START {}REPLY_END", "detail ".repeat(3000));
    let turns = [
        Turn {
            user: "write the plan",
            items: vec![Item::Assistant(&long_reply)],
        },
        Turn {
            user: "thanks",
            items: vec![Item::Assistant("Sure.")],
        },
    ];
    let limit = 1000;
    let mut model = FakeModel::default();
    let summary = run(
        Request {
            max_text_tokens: limit,
            ..request(&turns)
        },
        &mut model,
    )
    .unwrap();
    assert!(tokens(&[&summary.text]) <= limit);
    let final_reply = &summary.compacted.turns[0].final_reply;
    assert!(final_reply.starts_with("REPLY_START "));
    assert!(final_reply.ends_with(" REPLY_END"));
    assert!(final_reply.contains(" bytes left out here]"));
    assert_eq!(summary.compacted.turns[0].users[0], "write the plan");
    assert_eq!(summary.compacted.turns[1].final_reply, "Sure.");

    let roomy = run(request(&turns), &mut model).unwrap();
    assert_eq!(roomy.compacted.turns[0].final_reply, long_reply);
}

#[test]
fn the_request_overhead_estimate_covers_the_longest_request() {
    let earlier = Payload {
        entries: ["R99999", "F99999", "D99999", "S99999", "O99999"]
            .iter()
            .map(|id| Entry {
                id: (*id).to_owned(),
                text: String::new(),
            })
            .collect(),
        ..Payload::default()
    };
    let source = Turn {
        user: "",
        items: Vec::new(),
    };
    let prepared = |number| Prepared {
        source: &source,
        users: Vec::new(),
        number,
        tool_numbers: Vec::new(),
        tools: vec![
            PendingTool {
                number: 99_990,
                name: "shell",
                call: None,
                result: None,
            },
            PendingTool {
                number: 99_999,
                name: "shell",
                call: None,
                result: None,
            },
        ],
        first_tool: 0,
        last_tool: 0,
        final_reply: "",
        has_work: true,
        continued: None,
        text: String::new(),
    };
    let turns = [prepared(99_998), prepared(99_999), prepared(0)];
    let plan = Plan {
        earlier: &earlier,
        turns: &turns,
        complete_end: 2,
        candidates: Vec::new(),
    };
    let mut text = String::new();
    write_request(&mut text, &plan, false);
    assert!(text.contains("\n\nTurn 99998 (T99990\u{2013}T99999)\nTurn 99999 (T99990\u{2013}T99999)\nTurn in progress (T99990\u{2013}T99999)\n\n"));
    assert!(text.contains("For the turn in progress, give its notes so far."));
    assert!(text.contains("O99999. Number new entries after them."));
    assert!(tokens(&[&text]) <= REQUEST_OVERHEAD_TOKENS + turns.len() * ITEM_LABEL_TOKENS);
}

#[test]
fn a_turn_in_progress_carries_over_and_its_text_is_complete_once_it_ends() {
    let running = [Turn {
        user: "migrate the db",
        items: vec![
            Item::Assistant("Starting with a dry run."),
            call("c1", "shell", "{\"command\":\"migrate --dry-run\"}"),
            result("c1", "shell", "3 tables to change"),
        ],
    }];
    let mut first_model = FakeModel::replying(
        "Turn in progress\nIn between: Ran a dry run of the migration.\nT1: dry run; 3 tables to change\n\nStatus:\nS1: the migration is in progress (T1)",
    );
    let first = run(
        Request {
            last_turn_open: true,
            ..request(&running)
        },
        &mut first_model,
    )
    .unwrap();

    let open = first.compacted.open.as_ref().unwrap();
    assert_eq!(open.work, "Ran a dry run of the migration.");
    assert_eq!(open.tools[0].why, "dry run; 3 tables to change");
    assert_eq!(
        first.compacted.entries[0].text,
        "S1: the migration is in progress (T1)"
    );
    assert_eq!(open.first_tool, 1);
    assert!(first.compacted.turns.is_empty());
    assert_eq!(first.compacted.turn_count, 0);
    let first_seen = &first_model.seen_user;
    assert!(first_seen.contains("[Turn in progress]\n[User, this message stays in the conversation after the summary]\nmigrate the db\n"));
    assert!(first_seen.contains("each followed by its notes:\n\nTurn in progress (T1)\n\n"));
    assert!(!first.text.contains("migrate the db"));
    assert!(first.text.contains(
        "Its tools so far:\n  T1 shell migrate --dry-run (18 bytes): dry run; 3 tables to change\n"
    ));

    let finished = [Turn {
        user: "migrate the db",
        items: vec![
            call("c2", "shell", "{\"command\":\"migrate --target staging\"}"),
            result("c2", "shell", "migrated"),
            Item::Assistant("Migrated staging."),
        ],
    }];
    let mut second_model = FakeModel::replying(
        "Turn 1\nIn between: Migrated staging.\nT2: migrated staging\n\nStatus:\nS2: the migration is done on staging (T2), replaces S1",
    );
    let second = run(after(&first.compacted, &finished), &mut second_model).unwrap();

    let second_seen = &second_model.seen_user;
    assert!(second_seen.starts_with(
        "[Rules, facts, decisions and status so far]\nS1: the migration is in progress (T1)\n\n[Turn 1]\n[User]\nmigrate the db\n\n[Earlier part of this turn, summarized]\nRan a dry run of the migration.\n\n[Its tools so far: T1 to T1]\n"
    ));
    let turn = &second.compacted.turns[0];
    assert_eq!(turn.number, 1);
    assert_eq!(turn.final_reply, "Migrated staging.");
    assert_eq!(turn.first_tool, 1);
    assert_eq!(turn.last_tool, 2);
    assert_eq!(
        turn.work,
        "Ran a dry run of the migration.\nMigrated staging."
    );
    assert_eq!(turn.tools.len(), 2);
    assert_eq!(
        turn.tools[1].line,
        "shell migrate --target staging (8 bytes)"
    );
    assert_eq!(turn.tools[1].why, "migrated staging");
    assert_eq!(second.compacted.entries.len(), 2);
    assert!(second.compacted.open.is_none());
    let turn_text = turn_file(&prepare(
        &finished[0],
        first.compacted.open.as_ref(),
        false,
        &mut 2,
    ));
    assert!(turn_text.ends_with(
        "Assistant:\nStarting with a dry run.\n\n[T1 shell: migrate --dry-run]\n\n[T2 shell: migrate --target staging]\n\nAssistant, final reply:\nMigrated staging.\n\n"
    ), "{turn_text}");
}

#[test]
fn a_turn_in_progress_compacted_twice_keeps_its_whole_text_so_far() {
    let part = [Turn {
        user: "long task",
        items: vec![
            call("a", "shell", "{\"command\":\"step one\"}"),
            result("a", "shell", "ok"),
        ],
    }];
    let mut model =
        FakeModel::replying("Turn in progress\nIn between: Did step one.\nT1: step one worked");
    let first = run(
        Request {
            last_turn_open: true,
            ..request(&part)
        },
        &mut model,
    )
    .unwrap();
    let more = [Turn {
        user: "long task",
        items: vec![
            call("b", "shell", "{\"command\":\"step two\"}"),
            result("b", "shell", "ok"),
        ],
    }];
    let mut again =
        FakeModel::replying("Turn in progress\nIn between: Did step two.\nT2: step two worked");
    let second = run(
        Request {
            last_turn_open: true,
            ..after(&first.compacted, &more)
        },
        &mut again,
    )
    .unwrap();
    let open = second.compacted.open.as_ref().unwrap();
    assert_eq!(open.work, "Did step one.\nDid step two.");
    assert_eq!(open.tools.len(), 2);
    assert_eq!(open.tools[1].why, "step two worked");
    assert_eq!(open.first_tool, 1);
    assert_eq!(open.last_tool, 2);
    assert_eq!(
        open.text,
        "[T1 shell: step one]\n\n[T2 shell: step two]\n\n"
    );
    assert_eq!(second.compacted.turn_count, 0);
}

#[test]
fn the_index_line_is_built_the_same_way_for_any_tool() {
    assert_eq!(
        index_line(
            "{\"request\":{\"action\":\"run\",\"command\":\"cd /repo &&\\n  wc -l src/app.zig\",\"yield_time_ms\":300000}}"
        ),
        "run cd /repo && wc -l src/app.zig"
    );
    assert_eq!(
        index_line(
            "{\"pattern\":\"resumeForWrite\",\"path\":\"src/core\",\"include\":[\"*.zig\"],\"case_insensitive\":true}"
        ),
        "resumeForWrite src/core *.zig"
    );
    assert_eq!(index_line("{}"), "");
    assert_eq!(index_line("not   json\nat all"), "not json at all");
    let long = index_line(&format!("{{\"content\":\"{}\"}}", "\u{e9}".repeat(300)));
    assert!(long.len() <= MAX_INDEX_BYTES);
}

#[test]
fn a_call_without_a_result_and_a_result_without_a_call_are_both_numbered() {
    let turns = [Turn {
        user: "go",
        items: vec![
            result("orphan", "read_file", "file text"),
            call("cut-off", "shell", "{}"),
        ],
    }];
    let mut model = FakeModel::default();
    let summary = run(request(&turns), &mut model).unwrap();
    assert_eq!(summary.compacted.tool_count, 2);
    assert_eq!(summary.compacted.turns[0].final_reply, "");
    let prepared = prepare(&turns[0], None, false, &mut 1);
    assert!(
        tool_file(&prepared.tools[0])
            .contains("T1 read_file\nCall ID: orphan\n\nArguments: (not recorded)")
    );
    assert!(tool_file(&prepared.tools[1]).contains("Result: (not recorded)"));
    assert!(
        prepared
            .text
            .contains("[T1 read_file, result only]\n\n[T2 shell]\n")
    );
}

#[test]
fn input_errors_are_reported_before_any_work() {
    let mut model = FakeModel::default();
    assert_eq!(
        run(request(&[]), &mut model),
        Err(CompactionError::NothingToCompact)
    );
    assert_eq!(model.calls, 0);
}

#[test]
fn model_failures_return_errors() {
    let turns = sample();
    let mut empty = FakeModel::replying(" \n\t ");
    assert_eq!(
        run(request(&turns), &mut empty),
        Err(CompactionError::EmptySummary)
    );
    let mut failing = FakeModel::failing(CompactionError::SummaryIncomplete);
    assert_eq!(
        run(request(&turns), &mut failing),
        Err(CompactionError::SummaryIncomplete)
    );
    let mut cancelled = FakeModel::failing(CompactionError::Cancelled);
    assert_eq!(
        run(
            Request {
                conversation_room: Some(100_000),
                ..request(&turns)
            },
            &mut cancelled
        ),
        Err(CompactionError::Cancelled)
    );
    assert_eq!(cancelled.calls, 1);
}

#[test]
fn without_saved_records_the_notes_keep_tool_details() {
    let turns = sample();
    let mut model = FakeModel::default();
    let summary = run(request(&turns), &mut model).unwrap();
    assert!(model.seen_user.contains("The tool calls will not be available later, so keep in your notes the details from them that the work still needs."));
    assert!(!summary.text.contains("read_tool_result"));
    assert!(summary.text.contains("User 1:\nFix the build."));
}

#[test]
fn turns_too_large_for_one_request_go_oldest_first_each_part_adding_to_the_one_before() {
    let output = "build output line ".repeat(400);
    let turns = [
        worked_turn("first", "call-1", &output),
        worked_turn("second", "call-2", &output),
        worked_turn("third", "call-3", &output),
    ];
    let one = turn_tokens(&turns[0]);
    let room = REQUEST_OVERHEAD_TOKENS + tokens(&[SYSTEM_PROMPT]) + one + one / 2;
    let mut model = FakeModel::replying(
        "Turn 1\nIn between: Ran make.\n\nTurn 2\nIn between: Ran make.\n\nTurn 3\nIn between: Ran make.\n\nStatus:\nS1 (T1): make runs",
    );
    let summary = run(
        Request {
            max_prompt_tokens: room,
            ..request(&turns)
        },
        &mut model,
    )
    .unwrap();

    assert_eq!(model.calls, 3);
    assert!(model.largest <= room);
    let seen = &model.seen_user;
    assert!(seen.starts_with(
        "[Rules, facts, decisions and status so far]\nS1 (T1): make runs\n\n[Turn 3]\n"
    ));
    assert!(seen.contains(&format!("[Tool result T3: shell]\n{output}\n")));
    assert!(!seen.contains("[Turn 1]"));
    assert!(!seen.contains("[Turn 2]"));
    let shown = &summary.compacted.turns;
    assert_eq!(shown.len(), 3);
    for ((turn, user), number) in shown.iter().zip(["first", "second", "third"]).zip(1..) {
        assert_eq!(turn.number, number);
        assert_eq!(turn.users[0], user);
        assert_eq!(turn.final_reply, "Done.");
        assert_eq!(turn.first_tool, number);
        assert_eq!(turn.work, "Ran make.");
    }
    assert_eq!(summary.compacted.turn_count, 3);
    assert_eq!(summary.compacted.tool_count, 3);
    assert_eq!(summary.compacted.entries.len(), 1);
}

#[test]
fn a_turn_too_large_for_one_request_keeps_the_start_and_end_of_its_long_texts() {
    let output = format!("START_OF_OUTPUT {}END_OF_OUTPUT", "filler ".repeat(20_000));
    let turns = [worked_turn("Run the build.", "call-1", &output)];
    let room = 4000;
    let mut model = FakeModel::default();
    run(
        Request {
            max_prompt_tokens: room,
            ..request(&turns)
        },
        &mut model,
    )
    .unwrap();
    assert_eq!(model.calls, 1);
    let seen = &model.seen_user;
    assert!(tokens(&[SYSTEM_PROMPT, seen]) <= room);
    assert!(seen.contains("[Tool result T1: shell]\nSTART_OF_OUTPUT "));
    assert!(seen.contains(" bytes left out here]\n"));
    assert!(seen.contains(" END_OF_OUTPUT\n"));
    assert!(seen.contains("[User]\nRun the build.\n"));
}

#[test]
fn a_request_fits_even_when_a_turn_has_many_texts_too_short_to_clip() {
    let output = "one short line of build output ".repeat(48);
    let ids: Vec<String> = (0..300).map(|index| format!("call-{index}")).collect();
    let mut items = Vec::new();
    for id in &ids {
        items.push(call(id, "shell", "{\"command\":\"make\"}"));
        items.push(result(id, "shell", &output));
    }
    items.push(Item::Assistant("Done."));
    let turns = [Turn {
        user: "Build every target.",
        items,
    }];
    let room = 20_000;
    let mut model = FakeModel::default();
    run(
        Request {
            max_prompt_tokens: room,
            ..request(&turns)
        },
        &mut model,
    )
    .unwrap();
    let seen = &model.seen_user;
    assert!(tokens(&[SYSTEM_PROMPT, seen]) <= room);
    assert!(seen.contains("[Tool result T300: shell]\n\n[1488 bytes left out here]\n"));
    assert!(seen.contains("[Tool call T300: shell]\n{\"command\":\"make\"}\n"));
}

#[test]
fn the_earlier_entries_are_clipped_only_when_the_turns_alone_cannot_make_the_request_fit() {
    let long_fact = format!(
        "F1 (T1): EARLIER_START {}EARLIER_END",
        "older work ".repeat(8000)
    );
    let earlier = Payload {
        entries: vec![Entry {
            id: "F1".to_owned(),
            text: long_fact.clone(),
        }],
        turn_count: 2,
        tool_count: 1,
        ..Payload::default()
    };
    let turns = [worked_turn("Run it again.", "call-1", "ok")];
    let room = 8000;
    let mut model = FakeModel::default();
    let summary = run(
        Request {
            max_prompt_tokens: room,
            ..after(&earlier, &turns)
        },
        &mut model,
    )
    .unwrap();
    let seen = &model.seen_user;
    assert!(tokens(&[SYSTEM_PROMPT, seen]) <= room);
    assert!(
        seen.starts_with("[Rules, facts, decisions and status so far]\nF1 (T1): EARLIER_START ")
    );
    assert!(seen.contains(" bytes left out here]\n"));
    assert!(seen.contains("EARLIER_END\n"));
    assert!(seen.contains("[Tool result T2: shell]\nok\n"));
    assert_eq!(summary.compacted.entries[0].text, long_fact);
}

#[test]
fn user_messages_added_to_a_turn_in_progress_carry_over_until_it_ends() {
    let running = [Turn {
        user: "migrate the db",
        items: vec![
            call("c1", "shell", "{\"command\":\"migrate --dry-run\"}"),
            result("c1", "shell", "3 tables to change"),
            Item::User("use staging, not production"),
        ],
    }];
    let mut first_model = FakeModel::replying(
        "Turn in progress\nIn between: Ran a dry run.\nT1: dry run\n\nStatus:\nS1: dry run done (T1)",
    );
    let first = run(
        Request {
            last_turn_open: true,
            ..request(&running)
        },
        &mut first_model,
    )
    .unwrap();
    assert_eq!(
        first.compacted.open.as_ref().unwrap().users,
        ["use staging, not production"]
    );
    assert!(
        first_model
            .seen_user
            .contains("[User, added while the assistant worked]\nuse staging, not production\n\n")
    );
    assert!(first.text.contains(
        "Turn in progress, whose first user message follows this:\nUser, added while the assistant worked:\nuse staging, not production\n\n"
    ));

    let finished = [Turn {
        user: "migrate the db",
        items: vec![
            Item::User("and back it up first"),
            call("c2", "shell", "{\"command\":\"migrate --target staging\"}"),
            result("c2", "shell", "migrated"),
            Item::Assistant("Migrated staging."),
        ],
    }];
    let mut second_model = FakeModel::replying(
        "Turn 1\nIn between: Backed up and migrated staging.\nT2: migrated staging",
    );
    let second = run(after(&first.compacted, &finished), &mut second_model).unwrap();
    assert!(second_model.seen_user.contains(
        "[Earlier part of this turn, summarized]\nRan a dry run.\n\n[User, added while the assistant worked]\nuse staging, not production\n\n"
    ));
    assert_eq!(
        second.compacted.turns[0].users,
        [
            "migrate the db",
            "use staging, not production",
            "and back it up first"
        ]
    );
    assert!(second.compacted.open.is_none());
}
