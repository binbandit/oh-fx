use std::time::Instant;

use ofx_trace::{TraceContext, trace_event};

use crate::hooks::definitions::HookInvocation;

const HOOKS: &str = "hooks";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Lifecycle {
    PreToolUse,
    Stop,
    PostTurnEnd,
    AttentionRequired,
}

impl Lifecycle {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::Stop => "Stop",
            Self::PostTurnEnd => "PostTurnEnd",
            Self::AttentionRequired => "AttentionRequired",
        }
    }
}

pub(super) struct HandlerTrace<'a> {
    event: Lifecycle,
    handler: &'a str,
    invocation: HookInvocation,
    call_id: Option<&'a str>,
    started: Instant,
}

impl<'a> HandlerTrace<'a> {
    pub(super) fn start(
        event: Lifecycle,
        handler: &'a str,
        invocation: HookInvocation,
        call_id: Option<&'a str>,
    ) -> Self {
        let trace = Self {
            event,
            handler,
            invocation,
            call_id,
            started: Instant::now(),
        };
        let (event, scope) = (event.name(), invocation.scope.name());
        match call_id {
            Some(id) => trace_event!(
                HOOKS,
                "handler_start",
                trace.context(),
                "lifecycle_event={event} handler={handler} scope={scope} call_id={id}"
            ),
            None => trace_event!(
                HOOKS,
                "handler_start",
                trace.context(),
                "lifecycle_event={event} handler={handler} scope={scope}"
            ),
        }
        trace
    }

    pub(super) fn finish(self, outcome: &str) {
        self.finished(format_args!("outcome={outcome}"));
    }

    pub(super) fn fail(self, error: &str) {
        self.finished(format_args!("outcome=error error={error}"));
    }

    fn finished(&self, outcome: std::fmt::Arguments<'_>) {
        let (event, handler) = (self.event.name(), self.handler);
        let scope = self.invocation.scope.name();
        let duration_ms = self.started.elapsed().as_millis();
        match self.call_id {
            Some(id) => trace_event!(
                HOOKS,
                "handler_finish",
                self.context(),
                "lifecycle_event={event} handler={handler} scope={scope} call_id={id} duration_ms={duration_ms} {outcome}"
            ),
            None => trace_event!(
                HOOKS,
                "handler_finish",
                self.context(),
                "lifecycle_event={event} handler={handler} scope={scope} duration_ms={duration_ms} {outcome}"
            ),
        }
    }

    fn context(&self) -> TraceContext {
        TraceContext {
            step_id: 0,
            ..self.invocation.trace
        }
    }
}
