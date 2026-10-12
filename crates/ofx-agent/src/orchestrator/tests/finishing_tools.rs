use super::*;

#[tokio::test]
async fn a_tool_that_finishes_the_turn_ends_it_without_another_request_or_its_later_calls() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"text":"a","serial":1}"#),
            ("call-2", r#"{"needs_input":1,"serial":1}"#),
            ("call-3", r#"{"text":"c","serial":1}"#),
        ]),
        text_reply("must not be requested"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "connect").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.final_text, "");
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(
        finished(&events),
        [
            ("call-1", ToolResultStatus::Success),
            ("call-2", ToolResultStatus::Failure)
        ]
    );
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::ToolFinished {
            status_detail: Some(ofx_contract::ToolStatusDetail::McpInputRequired),
            ..
        }
    )));
    let ChatMessage::Assistant { tool_calls, .. } = &agent.history[1] else {
        panic!("the step's calls stay in history");
    };
    let kept: Vec<&str> = tool_calls.iter().map(|call| call.id.as_str()).collect();
    assert_eq!(kept, ["call-1", "call-2"]);
    assert_eq!(
        agent.history[3],
        tool_message("call-2", "echo needs input", ToolResultStatus::Failure)
    );
    assert_eq!(agent.history.len(), 4);
}

#[tokio::test]
async fn the_next_turn_after_a_finished_turn_continues_from_its_results() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", r#"{"needs_input":1}"#)]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    run(&mut agent, "connect").await;
    let (report, _) = run(&mut agent, "try again").await;
    assert_eq!(report.final_text, "done");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].messages[2],
        tool_message("call-1", "echo needs input", ToolResultStatus::Failure)
    );
}
