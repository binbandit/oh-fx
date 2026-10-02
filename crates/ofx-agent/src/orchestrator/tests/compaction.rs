use super::*;
use crate::compactor::CompactionError;

async fn chat(agent: &mut Agent, turns: usize) {
    for turn in 1..=turns {
        let (report, _) = run(agent, &format!("question {turn}")).await;
        assert_eq!(report.outcome, TurnOutcome::Completed);
    }
}

fn chat_replies(turns: usize) -> Vec<Script> {
    (1..=turns)
        .map(|turn| text_reply(&format!("answer {turn}")))
        .collect()
}

fn user_text(message: &ChatMessage) -> &str {
    match message {
        ChatMessage::User { content } => content,
        _ => "",
    }
}

#[tokio::test]
async fn manual_compaction_keeps_the_newest_turns_and_replaces_the_rest_with_a_checkpoint() {
    let mut scripts = chat_replies(6);
    scripts.push(text_reply("answer 7"));
    let provider = FakeProvider::new(scripts);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    chat(&mut agent, 6).await;

    assert_eq!(
        agent.compact(&CancellationToken::new()).await,
        Ok(Compaction::Compacted)
    );
    assert_eq!(provider.requests().len(), 6);

    run(&mut agent, "question 7").await;
    let requests = provider.requests();
    let messages = &requests[6].messages;
    let checkpoint = user_text(&messages[0]);
    assert!(checkpoint.starts_with("<compacted_conversation>\n"));
    assert!(
        checkpoint.contains("Turn 1\nUser 1:\nquestion 1\n\nAssistant 1, final reply:\nanswer 1\n")
    );
    assert!(checkpoint.contains("Turn 2\nUser 2:\nquestion 2\n"));
    assert!(!checkpoint.contains("question 3"));
    assert_eq!(user_text(&messages[1]), "question 3");
    assert_eq!(messages.len(), 1 + 4 * 2 + 1);
    assert_eq!(user_text(&messages[9]), "question 7");
}

#[tokio::test]
async fn manual_compaction_asks_the_conversations_model_for_notes_on_tool_work() {
    let mut scripts = vec![
        tool_reply(&[("call-1", r#"{"value":"notes.md"}"#)]),
        text_reply("Read the notes."),
    ];
    scripts.extend(chat_replies(4));
    scripts.push(text_reply(
        "Turn 1\nIn between: Echoed the notes.\nT1: echoed notes.md",
    ));
    scripts.push(text_reply("done"));
    let provider = FakeProvider::new(scripts);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    run(&mut agent, "read the notes").await;
    chat(&mut agent, 4).await;

    assert_eq!(
        agent.compact(&CancellationToken::new()).await,
        Ok(Compaction::Compacted)
    );
    let requests = provider.requests();
    let summary = &requests[6];
    assert_eq!(summary.model, "test-model");
    assert_eq!(summary.instructions.len(), 1);
    assert!(summary.instructions[0].starts_with("You write compaction notes"));
    assert!(summary.tools.is_empty());
    assert_eq!(summary.max_output_tokens, Some(64));
    assert_eq!(summary.reasoning_effort, None);
    assert_eq!(summary.messages.len(), 1);
    let asked = user_text(&summary.messages[0]);
    assert!(asked.starts_with("[Turn 1]\n[User]\nread the notes\n"));
    assert!(asked.contains("[Tool call T1: echo]\n{\"value\":\"notes.md\"}\n"));

    run(&mut agent, "and now?").await;
    let requests = provider.requests();
    let checkpoint = user_text(&requests[7].messages[0]);
    assert!(checkpoint.contains("Assistant 1, in between:\nEchoed the notes.\n"));
    assert!(checkpoint.contains("Tools:\n  T1 echo notes.md (25 bytes): echoed notes.md\n"));
    assert_eq!(user_text(&requests[7].messages[1]), "question 1");
}

#[tokio::test]
async fn a_conversation_that_fits_is_left_alone() {
    let provider = FakeProvider::new(chat_replies(2));
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    chat(&mut agent, 1).await;
    assert_eq!(
        agent.compact(&CancellationToken::new()).await,
        Ok(Compaction::Unchanged)
    );
    run(&mut agent, "question 2").await;
    assert_eq!(provider.requests()[1].messages.len(), 3);
}

#[tokio::test]
async fn a_failed_summary_keeps_the_whole_history() {
    let mut scripts = vec![
        tool_reply(&[("call-1", r#"{"value":"notes.md"}"#)]),
        text_reply("Read the notes."),
    ];
    scripts.extend(chat_replies(4));
    scripts.push(Script::Fail(
        Vec::new(),
        failure(ProviderErrorKind::InvalidRequest, "BadRequest"),
    ));
    scripts.push(text_reply("done"));
    let provider = FakeProvider::new(scripts);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    run(&mut agent, "read the notes").await;
    chat(&mut agent, 4).await;

    assert_eq!(
        agent.compact(&CancellationToken::new()).await,
        Err(CompactionError::ModelFailed)
    );
    run(&mut agent, "and now?").await;
    let requests = provider.requests();
    assert_eq!(user_text(&requests[7].messages[0]), "read the notes");
    assert_eq!(requests[7].messages.len(), 4 + 4 * 2 + 1);
}

#[tokio::test]
async fn a_cancelled_compaction_sends_nothing_and_keeps_the_history() {
    let provider = FakeProvider::new(chat_replies(6));
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    chat(&mut agent, 6).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        agent.compact(&cancel).await,
        Err(CompactionError::Cancelled)
    );
    assert_eq!(provider.requests().len(), 6);
}

#[tokio::test]
async fn clearing_the_history_forgets_the_checkpoint() {
    let mut scripts = chat_replies(6);
    scripts.push(text_reply("fresh"));
    let provider = FakeProvider::new(scripts);
    let mut agent = new_agent(Arc::clone(&provider), Vec::new());
    chat(&mut agent, 6).await;
    assert_eq!(
        agent.compact(&CancellationToken::new()).await,
        Ok(Compaction::Compacted)
    );
    agent.clear_history();
    run(&mut agent, "start over").await;
    let requests = provider.requests();
    assert_eq!(requests[6].messages, [ChatMessage::user("start over")]);
}
