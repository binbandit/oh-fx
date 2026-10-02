use std::collections::VecDeque;
use std::sync::Mutex;

use ofx_contract::{
    BoxFuture, ChatMessage, Completion, FinishReason, ModelProvider, ModelRequest, ProviderError,
    ProviderErrorKind, StreamEvent, StreamSink, ToolCall, ToolChoice, Usage,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeenRequest {
    pub(crate) model: String,
    pub(crate) instructions: Vec<String>,
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) tools: Vec<String>,
    pub(crate) tool_choice: ToolChoice,
    pub(crate) max_output_tokens: Option<u32>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) fast: bool,
}

pub(crate) struct ScriptedProvider {
    replies: Mutex<VecDeque<Result<Completion, ProviderError>>>,
    seen: Mutex<Vec<SeenRequest>>,
}

impl ScriptedProvider {
    pub(crate) fn new(replies: Vec<Result<Completion, ProviderError>>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            seen: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn seen(&self) -> Vec<SeenRequest> {
        self.seen.lock().unwrap().clone()
    }
}

pub(crate) fn text(content: &str) -> Completion {
    Completion {
        content: Some(content.to_owned()),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
        provider_replay: None,
    }
}

pub(crate) fn calling(call: ToolCall) -> Completion {
    Completion {
        content: None,
        tool_calls: vec![call],
        finish_reason: FinishReason::ToolCalls,
        usage: Usage::default(),
        provider_replay: None,
    }
}

pub(crate) fn failure(kind: ProviderErrorKind, code: &str) -> ProviderError {
    ProviderError::new(kind, code)
}

impl ModelProvider for ScriptedProvider {
    fn stream<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        sink: &'a mut dyn StreamSink,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        self.seen.lock().unwrap().push(SeenRequest {
            model: request.model.to_owned(),
            instructions: request
                .instructions
                .iter()
                .map(|text| (*text).to_owned())
                .collect(),
            messages: request.messages.to_vec(),
            tools: request.tools.iter().map(|tool| tool.name.clone()).collect(),
            tool_choice: request.tool_choice,
            max_output_tokens: request.max_output_tokens,
            reasoning_effort: request.provider_options.reasoning_effort.map(str::to_owned),
            fast: request.provider_options.fast,
        });
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(failure(ProviderErrorKind::Protocol, "NoScript")));
        Box::pin(async move {
            if let Ok(Completion {
                content: Some(text),
                ..
            }) = &reply
            {
                sink.emit(StreamEvent::TextDelta { text: text.clone() });
            }
            reply
        })
    }
}
