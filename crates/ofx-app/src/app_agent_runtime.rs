use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use ofx_agent::{Agent, TurnFailure};
use ofx_contract::{Notice, NoticeTone, ProviderError, UiCommand, UiEvent};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use crate::app_bootstrap_runtime::{AgentSetup, CredentialSource};
use crate::app_commands::{CommandEffect, handle_command};

pub(crate) type Emit = Arc<dyn Fn(UiEvent) + Send + Sync>;

pub(crate) struct ControllerState {
    setup: AgentSetup,
    model: String,
    model_pending: bool,
    pending_clear: Option<u64>,
    received_prompts: u64,
    queue: VecDeque<String>,
    emit: Emit,
}

impl ControllerState {
    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn models(&self) -> &[String] {
        self.setup.models()
    }

    pub(crate) fn emit(&self, event: UiEvent) {
        (self.emit)(event);
    }

    pub(crate) fn notice(&self, tone: NoticeTone, topic: &str, body: &str) {
        self.emit(UiEvent::Notice {
            notice: Notice::new(tone, topic, body),
        });
    }

    fn select_model(&mut self, model: String) {
        self.model = model;
        self.emit(UiEvent::ModelSelected {
            model: self.model.clone(),
        });
    }

    fn receive_prompt(&mut self, prompt: String) {
        self.received_prompts += 1;
        self.queue.push_back(prompt);
    }
}

pub(crate) struct Controller {
    agent: Agent,
    state: ControllerState,
}

impl Controller {
    pub(crate) fn new(setup: AgentSetup, emit: Emit) -> Self {
        let state = ControllerState {
            model: setup.model().to_owned(),
            setup,
            model_pending: false,
            pending_clear: None,
            received_prompts: 0,
            queue: VecDeque::new(),
            emit,
        };
        Self {
            agent: state.setup.agent(),
            state,
        }
    }

    pub(crate) async fn run(mut self, mut commands: UnboundedReceiver<UiCommand>) {
        loop {
            if let Some(prompt) = self.state.queue.pop_front() {
                if !self.run_turn(&prompt, &mut commands).await {
                    return;
                }
                continue;
            }
            let Some(command) = commands.recv().await else {
                return;
            };
            match command {
                UiCommand::Submit { prompt } => self.state.receive_prompt(prompt),
                UiCommand::RunCommand { text } => self.run_idle_command(&text),
                UiCommand::Cancel { .. } | UiCommand::Approval { .. } => {}
            }
        }
    }

    fn run_idle_command(&mut self, text: &str) {
        match handle_command(&self.state, text, false) {
            CommandEffect::None => {}
            CommandEffect::SwitchModel(model) => {
                self.state.select_model(model);
                self.reconfigure();
            }
            CommandEffect::Clear => self.clear(self.state.received_prompts),
        }
    }

    fn reconfigure(&mut self) {
        self.agent
            .set_config(self.state.setup.config(&self.state.model));
    }

    fn clear(&mut self, first_kept_prompt: u64) {
        self.agent.clear_history();
        self.state
            .emit(UiEvent::ConversationCleared { first_kept_prompt });
    }

    async fn run_turn(
        &mut self,
        prompt: &str,
        commands: &mut UnboundedReceiver<UiCommand>,
    ) -> bool {
        let cancel = CancellationToken::new();
        let emit = Arc::clone(&self.state.emit);
        let running = Arc::new(Mutex::new(None));
        let started = Arc::clone(&running);
        let mut sink = move |event: UiEvent| match &event {
            UiEvent::TurnFinished { .. } => {}
            UiEvent::TurnStarted { turn_id } => {
                *started.lock().unwrap_or_else(PoisonError::into_inner) = Some(*turn_id);
                emit(event);
            }
            _ => emit(event),
        };
        let running_turn = || *running.lock().unwrap_or_else(PoisonError::into_inner);
        let state = &mut self.state;
        let mut open = true;
        let report = {
            let turn = self.agent.run_turn(prompt, &mut sink, &cancel);
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    report = &mut turn => break report,
                    command = commands.recv(), if open => match command {
                        None => {
                            open = false;
                            cancel.cancel();
                        }
                        Some(UiCommand::Submit { prompt }) => state.receive_prompt(prompt),
                        Some(UiCommand::Cancel { turn_id }) => {
                            if running_turn() == Some(turn_id) {
                                cancel.cancel();
                            }
                        }
                        Some(UiCommand::Approval { request_id, decision }) => {
                            if let Some(approvals) = state.setup.approvals() {
                                approvals.resolve(request_id, decision);
                            }
                        }
                        Some(UiCommand::RunCommand { text }) => {
                            match handle_command(state, &text, true) {
                                CommandEffect::None => {}
                                CommandEffect::SwitchModel(model) => {
                                    state.model_pending = true;
                                    state.select_model(model);
                                }
                                CommandEffect::Clear => {
                                    state.pending_clear = Some(state.received_prompts);
                                    state.queue.clear();
                                    cancel.cancel();
                                }
                            }
                        }
                    },
                }
            }
        };
        if let Some(turn_id) = running_turn() {
            let source = self.state.setup.source();
            let status = report
                .failure
                .as_ref()
                .and_then(|failure| failure_status(failure, source));
            if let Some(text) = status {
                self.state.emit(UiEvent::ApiStatus { turn_id, text });
            }
            self.state.emit(UiEvent::TurnFinished {
                turn_id,
                outcome: report.outcome,
            });
        }
        if std::mem::take(&mut self.state.model_pending) {
            self.reconfigure();
        }
        if let Some(first_kept_prompt) = self.state.pending_clear.take() {
            self.clear(first_kept_prompt);
        }
        open
    }
}

fn failure_status(failure: &TurnFailure, source: CredentialSource) -> Option<String> {
    match failure {
        TurnFailure::Provider(error) => Some(provider_status(error, source)),
        TurnFailure::InvalidCompletion
        | TurnFailure::PermissionRequired(_)
        | TurnFailure::ProjectContext => Some(format!("⚠ {}", failure.code())),
        TurnFailure::StepLimitReached | TurnFailure::RepeatedMalformedArguments => None,
    }
}

fn provider_status(error: &ProviderError, source: CredentialSource) -> String {
    match (
        ofx_gateway::http_failure(error, source.label()),
        &error.detail,
    ) {
        (Some(failure), _) if failure.unauthorized => {
            format!("⚠ {} · {}", failure.message, source.repair())
        }
        (Some(failure), _) => format!("⚠ {}", failure.message),
        (None, Some(detail)) => format!("⚠ request failed: {} · {detail}", error.code),
        (None, None) => format!("⚠ request failed: {}", error.code),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use ofx_config::{ProfilePaths, Settings};
    use ofx_contract::{
        ApprovalDecision, PermissionMode, ProviderErrorKind, ToolResultStatus, TurnId, TurnOutcome,
    };
    use ofx_exec::{ManagedExecutions, SessionSupervisor};
    use ofx_testkit::{FakeServer, Reply, chat_text_events, chat_tool_call_events};
    use serde_json::{Value, json};
    use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
    use tokio::time::timeout;

    use super::*;
    use crate::app_bootstrap_runtime::{Launch, Profile};
    use crate::codex_provider::SubscriptionEndpoints;

    struct Harness {
        home: tempfile::TempDir,
        commands: UnboundedSender<UiCommand>,
        events: UnboundedReceiver<UiEvent>,
        seen: Vec<UiEvent>,
    }

    async fn agent_setup(home: &tempfile::TempDir, server: &FakeServer) -> AgentSetup {
        let config = home.path().join("config");
        let workspace = home.path().join("workspace");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let settings = json!({
            "provider": "local",
            "providers": {
                "local": {
                    "protocol": "openai-chat-completions",
                    "base_url": server.base_url(),
                    "auth": {"type": "none"},
                    "models": ["model-a", "vendor/model-b"]
                }
            }
        });
        fs::write(config.join("settings.json"), settings.to_string()).unwrap();
        let paths = ProfilePaths {
            config,
            data: home.path().join("data"),
            state: home.path().join("state"),
            cache: home.path().join("cache"),
        };
        let settings = Settings::load(&paths, &workspace).unwrap();
        let executions = ManagedExecutions::new(SessionSupervisor::new("/nonexistent"));
        Profile::new(workspace, Some(paths), settings)
            .unwrap()
            .connect_interactive(
                Launch {
                    model: None,
                    permission_mode: PermissionMode::Auto,
                    system_prompt: None,
                    reasoning_effort: None,
                    fast_mode: false,
                    context_limits: &[],
                    command_timeout: None,
                    executions: &executions,
                    endpoints: SubscriptionEndpoints::default(),
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap()
    }

    impl Harness {
        async fn start(server: &FakeServer) -> Self {
            let home = tempfile::tempdir().unwrap();
            let setup = agent_setup(&home, server).await;
            let (events_sender, events) = unbounded_channel();
            let emit: Emit = Arc::new(move |event| {
                let _ = events_sender.send(event);
            });
            let (commands, receiver) = unbounded_channel();
            tokio::spawn(Controller::new(setup, emit).run(receiver));
            Self {
                home,
                commands,
                events,
                seen: Vec::new(),
            }
        }

        fn send(&self, command: UiCommand) {
            self.commands.send(command).unwrap();
        }

        fn command(&self, text: &str) {
            self.send(UiCommand::RunCommand {
                text: text.to_owned(),
            });
        }

        fn submit(&self, prompt: &str) {
            self.send(UiCommand::Submit {
                prompt: prompt.to_owned(),
            });
        }

        fn running_turn(&self) -> TurnId {
            self.seen
                .iter()
                .rev()
                .find_map(|event| match event {
                    UiEvent::TurnStarted { turn_id } => Some(*turn_id),
                    _ => None,
                })
                .unwrap()
        }

        async fn until(&mut self, done: impl Fn(&UiEvent) -> bool) -> &[UiEvent] {
            let start = self.seen.len();
            while let Some(event) = self.events.recv().await {
                let finished = done(&event);
                self.seen.push(event);
                if finished {
                    break;
                }
            }
            &self.seen[start..]
        }
    }

    fn finished(outcome: TurnOutcome) -> impl Fn(&UiEvent) -> bool {
        move |event| matches!(event, UiEvent::TurnFinished { outcome: seen, .. } if *seen == outcome)
    }

    fn notice_body(events: &[UiEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                UiEvent::Notice { notice } => Some(format!("{}|{}", notice.topic, notice.body)),
                _ => None,
            })
            .collect()
    }

    fn user_messages(body: &Value) -> usize {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .count()
    }

    #[tokio::test]
    async fn prompts_submitted_during_a_turn_run_next_in_order() {
        let server = FakeServer::start([
            Reply::sse(&chat_text_events(&["one"])),
            Reply::sse(&chat_text_events(&["two"])),
        ]);
        let mut harness = Harness::start(&server).await;
        harness.submit("first");
        harness.submit("second");
        harness.until(finished(TurnOutcome::Completed)).await;
        let second = harness.until(finished(TurnOutcome::Completed)).await;
        assert!(matches!(second[0], UiEvent::TurnStarted { .. }));
        assert!(
            second
                .iter()
                .any(|event| matches!(event, UiEvent::AssistantText { text, .. } if text == "two"))
        );
        let requests = server.requests();
        assert_eq!(user_messages(&requests[1].json()), 2);
    }

    #[tokio::test]
    async fn model_commands_show_and_switch_the_connection_models() {
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["ok"]))]);
        let mut harness = Harness::start(&server).await;
        harness.command("/model");
        let shown = harness
            .until(|event| matches!(event, UiEvent::Notice { .. }))
            .await;
        assert_eq!(
            notice_body(shown),
            ["model|model-a\navailable: model-a, vendor/model-b"]
        );
        harness.command("/model model-b");
        let switched = harness
            .until(|event| matches!(event, UiEvent::ModelSelected { .. }))
            .await;
        assert_eq!(
            switched.last(),
            Some(&UiEvent::ModelSelected {
                model: "vendor/model-b".to_owned()
            })
        );
        assert_eq!(notice_body(switched), ["|Switched to vendor/model-b"]);
        harness.submit("go");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(server.requests()[0].json()["model"], "vendor/model-b");
    }

    #[tokio::test]
    async fn models_switched_mid_turn_apply_to_the_next_turn() {
        let held = Reply::held_sse(&chat_text_events(&["partial\n"])[..2]);
        let server = FakeServer::start([held, Reply::sse(&chat_text_events(&["ok"]))]);
        let mut harness = Harness::start(&server).await;
        harness.submit("slow");
        harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        harness.command("/model model-b");
        let notice = harness
            .until(|event| matches!(event, UiEvent::Notice { .. }))
            .await;
        assert_eq!(notice_body(notice), ["|Next turn will use vendor/model-b"]);
        let turn_id = harness.running_turn();
        harness.send(UiCommand::Cancel { turn_id });
        harness.until(finished(TurnOutcome::Interrupted)).await;
        harness.submit("next");
        harness.until(finished(TurnOutcome::Completed)).await;
        let requests = server.requests();
        assert_eq!(requests[0].json()["model"], "model-a");
        assert_eq!(requests[1].json()["model"], "vendor/model-b");
    }

    #[tokio::test]
    async fn clear_starts_a_fresh_conversation() {
        let server = FakeServer::start([
            Reply::sse(&chat_text_events(&["one"])),
            Reply::sse(&chat_text_events(&["two"])),
        ]);
        let mut harness = Harness::start(&server).await;
        harness.submit("first");
        harness.until(finished(TurnOutcome::Completed)).await;
        harness.command("/clear");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        harness.submit("second");
        harness.until(finished(TurnOutcome::Completed)).await;
        assert_eq!(user_messages(&server.requests()[1].json()), 1);
    }

    #[tokio::test]
    async fn prompts_submitted_after_a_mid_turn_clear_run_in_the_fresh_conversation() {
        let held = Reply::held_sse(&chat_text_events(&["partial\n"])[..2]);
        let server = FakeServer::start([held, Reply::sse(&chat_text_events(&["after"]))]);
        let mut harness = Harness::start(&server).await;
        harness.submit("slow");
        harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        harness.submit("dropped");
        harness.command("/clear");
        harness.submit("kept");
        let cleared = harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        assert_eq!(
            cleared.last(),
            Some(&UiEvent::ConversationCleared {
                first_kept_prompt: 2
            })
        );
        let next = timeout(
            Duration::from_secs(10),
            harness.until(finished(TurnOutcome::Completed)),
        )
        .await
        .expect("the prompt sent after clear runs");
        assert!(
            next.iter().any(
                |event| matches!(event, UiEvent::AssistantText { text, .. } if text == "after")
            )
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(user_messages(&requests[1].json()), 1);
    }

    #[tokio::test]
    async fn cancels_naming_a_finished_turn_leave_the_running_turn_alone() {
        let held = Reply::held_sse(&chat_text_events(&["partial\n"])[..2]);
        let server = FakeServer::start([Reply::sse(&chat_text_events(&["one"])), held]);
        let mut harness = Harness::start(&server).await;
        harness.submit("first");
        harness.until(finished(TurnOutcome::Completed)).await;
        let stale = harness.running_turn();
        harness.submit("second");
        harness
            .until(|event| matches!(event, UiEvent::AssistantText { .. }))
            .await;
        let running = harness.running_turn();
        assert_ne!(stale, running);
        harness.send(UiCommand::Cancel { turn_id: stale });
        let early = timeout(
            Duration::from_millis(300),
            harness.until(finished(TurnOutcome::Interrupted)),
        )
        .await;
        assert!(
            early.is_err(),
            "a stale cancel interrupted the running turn"
        );
        harness.send(UiCommand::Cancel { turn_id: running });
        let interrupted = harness.until(finished(TurnOutcome::Interrupted)).await;
        assert!(matches!(
            interrupted.last(),
            Some(UiEvent::TurnFinished { turn_id, .. }) if *turn_id == running
        ));
    }

    #[tokio::test]
    async fn help_exit_and_unknown_commands_produce_ui_events() {
        let server = FakeServer::start([]);
        let mut harness = Harness::start(&server).await;
        harness.command("/help");
        harness.command("/bogus");
        harness.command("/exit");
        let events = harness
            .until(|event| matches!(event, UiEvent::ExitRequested))
            .await
            .to_vec();
        assert_eq!(events[0], UiEvent::HelpRequested);
        assert_eq!(
            notice_body(&events),
            ["command|Unknown command. Try /help."]
        );
    }

    #[tokio::test]
    async fn provider_failures_are_reported_for_their_turn_before_it_finishes() {
        let server = FakeServer::start([Reply::status(401, r#"{"error":{"message":"bad key"}}"#)]);
        let mut harness = Harness::start(&server).await;
        harness.submit("hi");
        let events = harness.until(finished(TurnOutcome::Failed)).await.to_vec();
        let turn_id = harness.running_turn();
        assert_eq!(
            events[events.len() - 2..],
            [
                UiEvent::ApiStatus {
                    turn_id,
                    text: "⚠ configured provider authentication failed · HTTP 401 · Check the configured provider auth environment variable.".to_owned()
                },
                UiEvent::TurnFinished {
                    turn_id,
                    outcome: TurnOutcome::Failed
                }
            ]
        );
    }

    #[tokio::test]
    async fn approvals_from_the_shell_let_gated_reads_run_and_denials_reach_the_model() {
        for (decision, status) in [
            (ApprovalDecision::Once, ToolResultStatus::Success),
            (ApprovalDecision::Deny, ToolResultStatus::Failure),
        ] {
            let server = FakeServer::start([
                Reply::sse(&chat_tool_call_events(
                    "call-1",
                    "read_file",
                    r#"{"path":"../outside.txt"}"#,
                )),
                Reply::sse(&chat_text_events(&["done"])),
            ]);
            let mut harness = Harness::start(&server).await;
            fs::write(harness.home.path().join("outside.txt"), "secret notes\n").unwrap();
            harness.submit("read it");
            let requested = harness
                .until(|event| matches!(event, UiEvent::ApprovalRequested { .. }))
                .await;
            let Some(UiEvent::ApprovalRequested { request, .. }) = requested.last().cloned() else {
                unreachable!()
            };
            assert_eq!(request.tool_name, "read_file");
            assert_eq!(request.title, "Reading ../outside.txt");
            harness.send(UiCommand::Approval {
                request_id: request.id,
                decision,
            });
            let events = harness.until(finished(TurnOutcome::Completed)).await;
            assert!(events.iter().any(|event| matches!(
                event,
                UiEvent::ToolFinished { status: seen, .. } if *seen == status
            )));
            let body = server.requests()[1].json();
            let tool_result = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["role"] == "tool")
                .and_then(|message| message["content"].as_str())
                .unwrap();
            assert_eq!(
                tool_result.contains("secret notes"),
                decision == ApprovalDecision::Once,
                "{tool_result}"
            );
            assert_eq!(
                tool_result.contains("tool_permission_denied"),
                decision == ApprovalDecision::Deny,
                "{tool_result}"
            );
        }
    }

    #[tokio::test]
    async fn clear_forgets_the_approvals_remembered_for_the_session() {
        let read = || {
            Reply::sse(&chat_tool_call_events(
                "call-1",
                "read_file",
                r#"{"path":"../outside.txt"}"#,
            ))
        };
        let server = FakeServer::start([
            read(),
            Reply::sse(&chat_text_events(&["done"])),
            read(),
            Reply::sse(&chat_text_events(&["done"])),
            read(),
            Reply::sse(&chat_text_events(&["done"])),
        ]);
        let mut harness = Harness::start(&server).await;
        fs::write(harness.home.path().join("outside.txt"), "secret notes\n").unwrap();
        let requested = |event: &UiEvent| matches!(event, UiEvent::ApprovalRequested { .. });
        harness.submit("read it");
        let Some(UiEvent::ApprovalRequested { request, .. }) =
            harness.until(requested).await.last().cloned()
        else {
            unreachable!()
        };
        assert!(request.scope.always.is_some());
        harness.send(UiCommand::Approval {
            request_id: request.id,
            decision: ApprovalDecision::Always,
        });
        harness.until(finished(TurnOutcome::Completed)).await;
        harness.submit("read it again");
        let remembered = harness.until(finished(TurnOutcome::Completed)).await;
        assert!(!remembered.iter().any(requested));
        harness.command("/clear");
        harness
            .until(|event| matches!(event, UiEvent::ConversationCleared { .. }))
            .await;
        harness.submit("read it after clearing");
        let asked = harness
            .until(|event| requested(event) || matches!(event, UiEvent::TurnFinished { .. }))
            .await;
        assert!(asked.last().is_some_and(requested), "{asked:?}");
    }

    #[test]
    fn failure_status_names_provider_and_model_errors_but_not_step_limits() {
        let mut error = ProviderError::new(ProviderErrorKind::ConnectionFailed, "ConnectionFailed");
        error.detail = Some("refused".to_owned());
        assert_eq!(
            failure_status(&TurnFailure::Provider(error), CredentialSource::Configured).unwrap(),
            "⚠ request failed: ConnectionFailed · refused"
        );
        let mut unauthorized = ProviderError::new(ProviderErrorKind::Unauthorized, "HttpError");
        unauthorized.status = Some(401);
        assert_eq!(
            failure_status(
                &TurnFailure::Provider(unauthorized),
                CredentialSource::Codex
            )
            .unwrap(),
            "⚠ Codex subscription authentication failed · HTTP 401 · Run oh-fx login codex to sign in again."
        );
        assert_eq!(
            failure_status(&TurnFailure::InvalidCompletion, CredentialSource::Codex).unwrap(),
            "⚠ ModelError"
        );
        for silent in [
            TurnFailure::StepLimitReached,
            TurnFailure::RepeatedMalformedArguments,
        ] {
            assert_eq!(failure_status(&silent, CredentialSource::Configured), None);
        }
    }
}
