use ofx_contract::{Notice, NoticeTone};

use super::*;

const CATALOG: &str =
    "Skills provide task instructions.\n<available_skills>\n</available_skills>\n";
const EXPLICIT: &str = "Explicitly invoked skill content for this query:\n";
const PROJECT: &str = "<project-rules>\n</project-rules>";

struct FakeSkills {
    uses_window: bool,
    prepared: Option<SkillContext>,
    calls: Mutex<Vec<(String, Option<u32>)>>,
}

impl FakeSkills {
    fn new(uses_window: bool, prepared: Option<SkillContext>) -> Arc<Self> {
        Arc::new(Self {
            uses_window,
            prepared,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<(String, Option<u32>)> {
        self.calls.lock().unwrap().clone()
    }
}

impl SkillContextProvider for FakeSkills {
    fn uses_context_window(&self) -> bool {
        self.uses_window
    }

    fn prepare<'a>(
        &'a self,
        prompt: &'a str,
        context_window: Option<u32>,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Option<SkillContext>> {
        self.calls
            .lock()
            .unwrap()
            .push((prompt.to_owned(), context_window));
        let prepared = self.prepared.clone();
        Box::pin(async move { prepared })
    }
}

struct WindowResolver {
    lookups: AtomicUsize,
}

impl CapabilityResolver for WindowResolver {
    fn resolve<'a>(
        &'a self,
        _model: &'a str,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, CapabilityLookup> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            CapabilityLookup::Resolved(ModelCapabilities {
                context_window: Some(400_000),
                ..ModelCapabilities::default()
            })
        })
    }
}

struct NoProject;

impl ProjectContextProvider for NoProject {
    fn select(&self, _targets: &[ApplicableTarget], _delivery: &DeliveryState) -> ProjectContext {
        ProjectContext::default()
    }
}

fn prepared() -> SkillContext {
    SkillContext {
        catalog: CATALOG.to_owned(),
        explicit: EXPLICIT.to_owned(),
        context_notices: vec!["skill discovery warning: skipped".to_owned()],
        load_notice: Some(Notice::new(
            NoticeTone::Neutral,
            "",
            "1 requested skill loaded\n\u{2514} Loaded skill review",
        )),
    }
}

fn skilled_agent(
    provider: &Arc<FakeProvider>,
    skills: &Arc<FakeSkills>,
    resolver: &Arc<WindowResolver>,
) -> Agent {
    let shared: Arc<FakeProvider> = Arc::clone(provider);
    Agent::new(
        shared,
        vec![echo_tool()],
        Arc::new(FixedContext),
        Arc::new(ArgumentGate),
        config(),
    )
    .with_capability_resolver(Arc::clone(resolver) as Arc<dyn CapabilityResolver>)
    .with_project_context(
        Arc::new(NoProject),
        ProjectContext {
            content: Some(PROJECT.to_owned()),
            ..ProjectContext::default()
        },
    )
    .with_skills(Arc::clone(skills) as Arc<dyn SkillContextProvider>)
}

fn resolver() -> Arc<WindowResolver> {
    Arc::new(WindowResolver {
        lookups: AtomicUsize::new(0),
    })
}

fn context_notices(events: &[UiEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ContextNotice { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_catalog_follows_the_system_prompt_and_explicit_skills_precede_the_runtime_context() {
    let provider = FakeProvider::new(vec![tool_reply(&[("call-1", "{}")]), text_reply("done")]);
    let skills = FakeSkills::new(true, Some(prepared()));
    let resolver = resolver();
    let mut agent = skilled_agent(&provider, &skills, &resolver);
    let (report, events) = run(&mut agent, "use $review").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(skills.calls(), [("use $review".to_owned(), Some(400_000))]);
    assert_eq!(resolver.lookups.load(Ordering::SeqCst), 1);
    let expected = [
        SYSTEM_PROMPT,
        CATALOG,
        PROJECT,
        EXPLICIT,
        TURN_CONTEXT,
        RESPONSE_LANGUAGE_CONTROL,
    ];
    for request in provider.requests() {
        assert_eq!(request.instructions, expected);
    }
    assert_eq!(
        context_notices(&events),
        ["skill discovery warning: skipped"]
    );
    let notices: Vec<&Notice> = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::Notice { notice } => Some(notice),
            _ => None,
        })
        .collect();
    assert_eq!(notices, [prepared().load_notice.as_ref().unwrap()]);
    let started = events
        .iter()
        .position(|event| matches!(event, UiEvent::TurnStarted { .. }))
        .unwrap();
    let noticed = events
        .iter()
        .position(|event| matches!(event, UiEvent::Notice { .. }))
        .unwrap();
    assert!(started < noticed);
    run(&mut agent, "again").await;
    assert_eq!(skills.calls()[1], ("again".to_owned(), Some(400_000)));
    assert_eq!(resolver.lookups.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_catalog_that_needs_no_window_leaves_the_capabilities_unresolved() {
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let skills = FakeSkills::new(false, Some(SkillContext::default()));
    let resolver = resolver();
    let mut agent = skilled_agent(&provider, &skills, &resolver);
    let (report, events) = run(&mut agent, "hello").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(skills.calls(), [("hello".to_owned(), None)]);
    assert_eq!(resolver.lookups.load(Ordering::SeqCst), 0);
    assert_eq!(
        provider.requests()[0].instructions,
        [
            SYSTEM_PROMPT,
            PROJECT,
            TURN_CONTEXT,
            RESPONSE_LANGUAGE_CONTROL
        ]
    );
    assert!(context_notices(&events).is_empty());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, UiEvent::Notice { .. }))
    );
}

#[tokio::test]
async fn a_cancelled_skill_load_interrupts_the_turn_before_any_request() {
    let provider = FakeProvider::new(vec![text_reply("never")]);
    let skills = FakeSkills::new(false, None);
    let resolver = resolver();
    let mut agent = skilled_agent(&provider, &skills, &resolver);
    let (report, _) = run(&mut agent, "$review").await;
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn context_notices_a_call_reports_reach_the_host_before_it_finishes() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[
            ("call-1", r#"{"noticed":true}"#),
            ("call-2", r#"{"refused":"noticed"}"#),
        ]),
        text_reply("done"),
    ]);
    let mut agent = new_agent(Arc::clone(&provider), vec![echo_tool()]);
    let (report, events) = run(&mut agent, "go").await;
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let order: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ContextNotice { text, .. } => Some(format!("notice {text}")),
            UiEvent::ToolFinished { call_id, .. } => Some(format!("finished {}", call_id.as_str())),
            UiEvent::ToolRejected { call_id, .. } => Some(format!("rejected {}", call_id.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        order,
        [
            "notice echo notice",
            "finished call-1",
            "rejected call-2",
            "notice refusal notice",
        ]
    );
}
