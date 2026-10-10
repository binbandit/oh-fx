use super::*;

struct Support(Mutex<VecDeque<CapabilityLookup>>);

impl CapabilityResolver for Support {
    fn resolve<'a>(
        &'a self,
        _model: &'a str,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, CapabilityLookup> {
        let lookup = self.0.lock().unwrap().pop_front().unwrap();
        Box::pin(async move { lookup })
    }
}

fn image(id: u64) -> ImageAttachment {
    ImageAttachment {
        id,
        path: format!("/workspace/shot-{id}.png"),
        media_type: "image/png".to_owned(),
        snapshot_path: Some(format!("/snapshots/image-{id}.bin")),
        snapshot_sha256: Some("0".repeat(64)),
        ..ImageAttachment::default()
    }
}

fn agent_seeing(provider: &Arc<FakeProvider>, lookup: CapabilityLookup) -> Agent {
    let shared: Arc<FakeProvider> = Arc::clone(provider);
    Agent::new(
        shared,
        vec![echo_tool()],
        Arc::new(FixedContext),
        Arc::new(ArgumentGate),
        config(),
    )
    .with_capability_resolver(Arc::new(Support(Mutex::new(VecDeque::from([lookup])))))
}

fn resolved(image_input_support: ImageInputSupport) -> CapabilityLookup {
    CapabilityLookup::Resolved(ModelCapabilities {
        image_input_support,
        ..ModelCapabilities::default()
    })
}

async fn run_with_images(
    agent: &mut Agent,
    prompt: &str,
    images: Vec<ImageAttachment>,
) -> TurnReport {
    agent
        .run_turn_with_images(prompt, images, &mut |_| {}, &CancellationToken::new())
        .await
}

fn user_images(message: &ChatMessage) -> &[ImageAttachment] {
    match message {
        ChatMessage::User { images, .. } => images,
        _ => &[],
    }
}

#[tokio::test]
async fn a_model_with_native_image_input_receives_the_turn_images_on_its_prompt() {
    let provider = FakeProvider::new(vec![tool_reply(&[("call-1", "{}")]), text_reply("seen")]);
    let mut agent = agent_seeing(&provider, resolved(ImageInputSupport::Native));

    let report = run_with_images(&mut agent, "what is this", vec![image(1), image(2)]).await;

    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let prompts: Vec<&ChatMessage> = request
            .messages
            .iter()
            .filter(|message| matches!(message, ChatMessage::User { .. }))
            .collect();
        assert_eq!(user_images(prompts[0]), [image(1), image(2)]);
    }
}

#[tokio::test]
async fn images_fail_the_turn_before_any_request_when_image_input_is_not_confirmed() {
    for (lookup, failure) in [
        (
            resolved(ImageInputSupport::Unknown),
            TurnFailure::ModelImageCapabilityUnavailable,
        ),
        (
            CapabilityLookup::CatalogUnavailable,
            TurnFailure::ModelImageCapabilityUnavailable,
        ),
        (
            resolved(ImageInputSupport::NonNative),
            TurnFailure::SubscriptionNativeImageUnavailable,
        ),
    ] {
        let provider = FakeProvider::new(vec![text_reply("unused")]);
        let mut agent = agent_seeing(&provider, lookup);

        let report = run_with_images(&mut agent, "what is this", vec![image(1)]).await;

        assert_eq!(report.outcome, TurnOutcome::Failed);
        assert_eq!(report.failure, Some(failure));
        assert!(provider.requests().is_empty());
    }
    assert_eq!(
        TurnFailure::ModelImageCapabilityUnavailable.code(),
        "ModelImageCapabilityUnavailable"
    );
    assert_eq!(
        TurnFailure::SubscriptionNativeImageUnavailable.code(),
        "SubscriptionNativeImageUnavailable"
    );
}

#[tokio::test]
async fn text_turns_never_ask_about_image_input() {
    let provider = FakeProvider::new(vec![text_reply("plain")]);
    let mut agent = agent_seeing(&provider, resolved(ImageInputSupport::NonNative));

    let report = run_with_images(&mut agent, "no pictures", Vec::new()).await;

    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn a_later_text_turn_still_needs_image_input_for_the_images_in_its_history() {
    let provider = FakeProvider::new(vec![text_reply("seen"), text_reply("again")]);
    let mut agent = agent_seeing(&provider, resolved(ImageInputSupport::Native));

    run_with_images(&mut agent, "look", vec![image(1)]).await;
    let (report, _) = run(&mut agent, "and now?").await;

    assert_eq!(report.outcome, TurnOutcome::Completed);
    let last = provider.requests().pop().unwrap();
    assert_eq!(user_images(&last.messages[0]), [image(1)]);
    agent.set_config(AgentConfig {
        model: "text-model".to_owned(),
        ..config()
    });
    agent.set_provider(
        Arc::clone(&provider) as Arc<dyn ModelProvider>,
        Some(Arc::new(Support(Mutex::new(VecDeque::from([resolved(
            ImageInputSupport::NonNative,
        )]))))),
    );
    let (report, _) = run(&mut agent, "one more").await;
    assert_eq!(
        report.failure,
        Some(TurnFailure::SubscriptionNativeImageUnavailable)
    );
}

fn tool_images_sent(provider: &FakeProvider) -> Vec<(String, usize)> {
    provider
        .requests()
        .last()
        .unwrap()
        .messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::Tool {
                content, images, ..
            } => Some((content.clone(), images.len())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn tool_images_reach_a_model_with_native_image_input() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", r#"{"picture":true}"#)]),
        text_reply("seen"),
    ]);
    let mut agent = agent_seeing(&provider, resolved(ImageInputSupport::Native));

    let (report, events) = run(&mut agent, "look at shot.png").await;

    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        tool_images_sent(&provider),
        [("<path>shot.png</path>".to_owned(), 1)]
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, UiEvent::Operational { .. }))
    );
}

#[tokio::test]
async fn tool_images_are_retained_but_not_sent_to_a_model_without_image_input() {
    for (lookup, notice) in [
        (
            resolved(ImageInputSupport::NonNative),
            "[Tool images were retained but not sent: this model does not accept inline images and no vision fallback is available. Ask the user to attach the image directly or switch to a vision-capable model.]\n",
        ),
        (
            resolved(ImageInputSupport::Unknown),
            "[Tool images were retained but not sent: oh-fx could not confirm image input support for this model (the model is not listed in the model catalog, or the catalog is unavailable). This can recover later in the session, so a retry may succeed; otherwise ask the user to attach the image directly.]\n",
        ),
    ] {
        let provider = FakeProvider::new(vec![
            tool_reply(&[("call-1", r#"{"picture":true}"#)]),
            text_reply("seen"),
        ]);
        let mut agent = agent_seeing(&provider, lookup);

        let (report, events) = run(&mut agent, "look at shot.png").await;

        assert_eq!(report.outcome, TurnOutcome::Completed);
        assert_eq!(
            tool_images_sent(&provider),
            [(format!("{notice}<path>shot.png</path>"), 0)]
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, UiEvent::Operational { .. }))
        );
        let kept = agent.history.iter().any(
            |message| matches!(message, ChatMessage::Tool { images, .. } if images.len() == 1),
        );
        assert!(kept);
    }
}

#[tokio::test]
async fn an_unavailable_catalog_explains_withheld_tool_images_once_per_turn() {
    let provider = FakeProvider::new(vec![
        tool_reply(&[("call-1", r#"{"picture":true}"#)]),
        tool_reply(&[("call-2", r#"{"picture":true}"#)]),
        text_reply("seen"),
    ]);
    let mut agent = agent_seeing(&provider, CapabilityLookup::CatalogUnavailable);

    let (_, events) = run(&mut agent, "look twice").await;

    let notices: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::Operational { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        notices,
        [
            "Images from tools aren't reaching the model right now because image support couldn't be confirmed (model catalog unavailable). oh-fx will retry automatically as the catalog recovers.\n"
        ]
    );
    assert!(
        tool_images_sent(&provider)
            .iter()
            .all(|(_, images)| *images == 0)
    );
}
