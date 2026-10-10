use super::*;

const NOTICE: &str = "[context] omitted 3 MCP servers from the model catalog because the fixed 4096-byte budget was reached";

struct Catalog {
    sections: Mutex<VecDeque<McpServersSection>>,
    calls: AtomicUsize,
}

impl McpServersCatalog for Catalog {
    fn section(&self) -> McpServersSection {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.sections
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default()
    }
}

fn notices(events: &[UiEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ContextNotice { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_servers_section_follows_the_stable_context_and_is_rendered_once_per_turn() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call_1", "{}")]),
        text_reply("first"),
        text_reply("second"),
        text_reply("third"),
    ]);
    let catalog = Arc::new(Catalog {
        sections: Mutex::new(VecDeque::from([
            McpServersSection {
                text: "SECTION ONE".to_owned(),
                change_notice: Some("CHANGED".to_owned()),
                notice: Some(NOTICE.to_owned()),
            },
            McpServersSection {
                text: "SECTION TWO".to_owned(),
                change_notice: None,
                notice: None,
            },
            McpServersSection::default(),
        ])),
        calls: AtomicUsize::new(0),
    });
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()])
        .with_mcp_servers(Arc::clone(&catalog) as Arc<dyn McpServersCatalog>);
    let (_, first) = run(&mut agent, "one").await;
    let (_, second) = run(&mut agent, "two").await;
    let (_, third) = run(&mut agent, "three").await;
    assert_eq!(catalog.calls.load(Ordering::SeqCst), 3);
    assert_eq!(notices(&first), [NOTICE]);
    assert!(notices(&second).is_empty());
    assert!(notices(&third).is_empty());
    let instructions: Vec<Vec<String>> = provider
        .requests()
        .into_iter()
        .map(|request| request.instructions)
        .collect();
    let with = |middle: &[&str]| -> Vec<String> {
        [SYSTEM_PROMPT]
            .iter()
            .chain(middle)
            .chain(&[TURN_CONTEXT, RESPONSE_LANGUAGE_CONTROL])
            .map(|text| (*text).to_owned())
            .collect()
    };
    assert_eq!(
        instructions,
        [
            with(&["SECTION ONE", "CHANGED"]),
            with(&["SECTION ONE", "CHANGED"]),
            with(&["SECTION TWO"]),
            with(&[]),
        ]
    );
}
