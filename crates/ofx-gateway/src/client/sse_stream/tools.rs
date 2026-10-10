use ofx_contract::{
    Json, Object, StreamEvent, StreamSink, ToolArgumentDiagnostic, ToolArgumentIntegrity, ToolCall,
    ToolCallId, ToolExecutionProvenance, parse_strict_json_value,
};
use ofx_trace::trace_log;

use crate::client::replay::{MAX_IDENTITY_BYTES, ReplayCall};
use crate::secret_mask::mask_configured_secrets;

const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];
const MALFORMED_RESULT: &str = "MalformedProviderResultIdentity";
const MALFORMED_IDENTITY: &str = "MalformedAuthoritativeToolIdentity";
const MALFORMED_ARGUMENTS: &str = "MalformedProviderToolArguments";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputState {
    Open,
    Ended,
    Finalized,
}

#[derive(Debug)]
struct StreamedInput {
    id: String,
    name: String,
    arguments: String,
    state: InputState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Identity {
    Absent,
    Empty,
    WrongType,
    Valid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultFailure {
    Absent,
    Empty,
    WrongType,
    Unmatched,
    Ambiguous,
    ProvenanceContradiction,
    DuplicateResult,
    MalformedPreliminary,
    MissingResult,
    ConflictingToolName,
    IncompleteStreamedInput,
    InvalidToolInput,
    MalformedProviderExecuted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultState {
    None,
    Preliminary,
    Final,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Anomaly {
    InvalidStart,
    ConflictingStart,
    LateStart,
    UnmatchedOrLateDelta,
    UnmatchedOrDuplicateEnd,
}

impl Anomaly {
    const fn reason(self) -> &'static str {
        match self {
            Self::InvalidStart => "invalid_start",
            Self::ConflictingStart => "conflicting_start",
            Self::LateStart => "late_start",
            Self::UnmatchedOrLateDelta => "unmatched_or_late_delta",
            Self::UnmatchedOrDuplicateEnd => "unmatched_or_duplicate_end",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgumentSource {
    FinalString,
    StreamedFallback,
}

impl ArgumentSource {
    const fn name(self) -> &'static str {
        match self {
            Self::FinalString => "final_string",
            Self::StreamedFallback => "streamed_fallback",
        }
    }
}

#[derive(Debug)]
struct FinalCall {
    id: String,
    name: String,
    arguments: String,
    provisional_id: Option<String>,
    identity: Identity,
    provenance: ToolExecutionProvenance,
    malformed: bool,
    result: Option<String>,
    result_state: ResultState,
}

#[derive(Debug, Default)]
pub(crate) struct ToolStream {
    streamed: Vec<StreamedInput>,
    calls: Vec<FinalCall>,
    failure: Option<ResultFailure>,
}

impl ToolStream {
    pub(crate) fn count(&self) -> usize {
        self.calls.len()
    }

    pub(crate) fn all_provider_executed(&self) -> bool {
        self.calls
            .iter()
            .all(|call| call.provenance == ToolExecutionProvenance::ProviderExecuted)
    }

    pub(crate) fn replay_calls(&self) -> Vec<ReplayCall<'_>> {
        self.calls
            .iter()
            .map(|call| ReplayCall {
                id: &call.id,
                provisional_id: call.provisional_id.as_deref(),
            })
            .collect()
    }

    pub(crate) fn start(&mut self, fields: &Object<'_>, sink: &mut dyn StreamSink) {
        let Some(id) = streamed_id(fields, Anomaly::InvalidStart) else {
            return;
        };
        let name = fields.get("toolName").and_then(Json::as_str).unwrap_or("");
        if let Some(record) = self.streamed.iter().find(|record| record.id == id) {
            if record.state != InputState::Open {
                anomaly(Anomaly::LateStart);
            } else if record.name != name {
                anomaly(Anomaly::ConflictingStart);
            }
            return;
        }
        self.streamed.push(StreamedInput {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: String::new(),
            state: InputState::Open,
        });
        if !name.is_empty() {
            sink.emit(StreamEvent::ToolCallStarted {
                call_id: ToolCallId::new(id),
                tool_name: name.to_owned(),
            });
        }
    }

    pub(crate) fn input(&mut self, fields: &Object<'_>, delta: bool, sink: &mut dyn StreamSink) {
        let kind = if delta {
            Anomaly::UnmatchedOrLateDelta
        } else {
            Anomaly::UnmatchedOrDuplicateEnd
        };
        let Some(id) = streamed_id(fields, kind) else {
            return;
        };
        let Some(record) = self
            .streamed
            .iter_mut()
            .find(|record| record.id == id && record.state == InputState::Open)
        else {
            anomaly(kind);
            return;
        };
        if !delta {
            record.state = InputState::Ended;
            return;
        }
        if let Some(text) = fields.get("delta").and_then(Json::as_str) {
            record.arguments.push_str(text);
            sink.emit(StreamEvent::ToolInputDelta {
                text: text.to_owned(),
            });
        }
    }

    pub(crate) fn call(&mut self, fields: &Object<'_>, secrets: &[String]) {
        let identity = final_identity(fields.get("toolCallId"));
        let id = match identity {
            Identity::Valid => fields
                .get("toolCallId")
                .and_then(Json::as_str)
                .unwrap_or(""),
            _ => "",
        };
        let duplicate = identity == Identity::Valid
            && self
                .calls
                .iter()
                .any(|prior| prior.identity == Identity::Valid && prior.id == id);
        let mut stream_index = (identity == Identity::Valid)
            .then(|| self.streamed.iter().position(|record| record.id == id))
            .flatten();
        let final_name = fields.get("toolName");
        let mut name = final_name.and_then(Json::as_str).unwrap_or("").to_owned();
        if let Some(index) = stream_index.filter(|_| name.is_empty() && final_name.is_none()) {
            name.clone_from(&self.streamed[index].name);
        }
        let mut compatible = true;
        if let (Some(index), Some(value)) = (stream_index, final_name)
            && value.as_str() != Some(self.streamed[index].name.as_str())
        {
            compatible = false;
            self.fail(ResultFailure::ConflictingToolName);
        }
        let shown = Shown {
            id: mask_configured_secrets(id.to_owned(), secrets),
            name: mask_configured_secrets(name.clone(), secrets),
        };
        let mut arguments = String::new();
        let mut malformed = false;
        let input = fields.get("input");
        if let Some(input) = input {
            let (text, valid) = final_input(input, &shown);
            arguments = text;
            malformed = !valid;
        }
        let usable = compatible && identity == Identity::Valid && !duplicate;
        if stream_index.is_none() && input.is_some() && !malformed && usable {
            stream_index = self.equivalent_ended(&name, &arguments);
        }
        if input.is_none() && usable {
            match stream_index.map(|index| (index, self.streamed[index].state)) {
                Some((index, InputState::Ended)) => {
                    arguments.clone_from(&self.streamed[index].arguments);
                    malformed =
                        !serialized_valid(&arguments, &shown, ArgumentSource::StreamedFallback);
                }
                Some((_, InputState::Open)) => self.fail(ResultFailure::IncompleteStreamedInput),
                Some((_, InputState::Finalized)) | None => {
                    self.fail(ResultFailure::InvalidToolInput);
                }
            }
        }
        let provenance = match fields.get("providerExecuted").map(Json::as_bool) {
            Some(Some(true)) => ToolExecutionProvenance::ProviderExecuted,
            Some(None) => {
                self.fail(ResultFailure::MalformedProviderExecuted);
                ToolExecutionProvenance::FxLocal
            }
            Some(Some(false)) | None => ToolExecutionProvenance::FxLocal,
        };
        let provisional_id = stream_index
            .map(|index| &self.streamed[index].id)
            .filter(|streamed| streamed.as_str() != id)
            .cloned();
        self.calls.push(FinalCall {
            id: id.to_owned(),
            name,
            arguments,
            provisional_id,
            identity,
            provenance,
            malformed,
            result: None,
            result_state: ResultState::None,
        });
        if let Some(index) = stream_index {
            self.streamed[index].state = InputState::Finalized;
        }
    }

    pub(crate) fn result(&mut self, fields: &Object<'_>) {
        let identity = final_identity(fields.get("toolCallId"));
        let failure = match identity {
            Identity::Absent => Some(ResultFailure::Absent),
            Identity::Empty => Some(ResultFailure::Empty),
            Identity::WrongType => Some(ResultFailure::WrongType),
            Identity::Valid => None,
        };
        if let Some(failure) = failure {
            self.fail(failure);
            return;
        }
        let id = fields
            .get("toolCallId")
            .and_then(Json::as_str)
            .unwrap_or("");
        let matches: Vec<usize> = self
            .calls
            .iter()
            .enumerate()
            .filter(|(_, call)| call.identity == Identity::Valid && call.id == id)
            .map(|(index, _)| index)
            .collect();
        let [index] = matches[..] else {
            self.fail(if matches.is_empty() {
                ResultFailure::Unmatched
            } else {
                ResultFailure::Ambiguous
            });
            return;
        };
        if self.calls[index].provenance != ToolExecutionProvenance::ProviderExecuted {
            self.fail(ResultFailure::ProvenanceContradiction);
            return;
        }
        if self.calls[index].result_state == ResultState::Final {
            self.fail(ResultFailure::DuplicateResult);
            return;
        }
        let preliminary = match fields.get("preliminary").map(Json::as_bool) {
            Some(Some(preliminary)) => preliminary,
            Some(None) => {
                self.fail(ResultFailure::MalformedPreliminary);
                return;
            }
            None => false,
        };
        let Some(result) = fields
            .get("result")
            .filter(|result| !result.is_null())
            .and_then(|result| serde_json::to_string(result).ok())
        else {
            self.fail(ResultFailure::MissingResult);
            return;
        };
        let call = &mut self.calls[index];
        call.result = Some(result);
        call.result_state = if preliminary {
            ResultState::Preliminary
        } else {
            ResultState::Final
        };
    }

    pub(crate) fn admission(&self) -> Result<(), &'static str> {
        if self.failure.is_some() {
            return Err(MALFORMED_RESULT);
        }
        for call in &self.calls {
            let blank = call.id.trim_matches(TRIMMED).is_empty();
            if call.identity != Identity::Valid || blank {
                return Err(
                    if call.provenance == ToolExecutionProvenance::ProviderExecuted {
                        MALFORMED_RESULT
                    } else {
                        MALFORMED_IDENTITY
                    },
                );
            }
        }
        for (index, call) in self.calls.iter().enumerate() {
            if self.calls[..index].iter().any(|prior| prior.id == call.id) {
                return Err(MALFORMED_IDENTITY);
            }
        }
        let unstorable = |value: &str| value.is_empty() || value.len() > MAX_IDENTITY_BYTES;
        if self.calls.iter().any(|call| {
            unstorable(&call.id)
                || unstorable(&call.name)
                || call.provisional_id.as_deref().is_some_and(unstorable)
        }) {
            return Err(MALFORMED_IDENTITY);
        }
        let provider_calls = || {
            self.calls
                .iter()
                .filter(|call| call.provenance == ToolExecutionProvenance::ProviderExecuted)
        };
        if provider_calls().any(|call| call.malformed) {
            return Err(MALFORMED_ARGUMENTS);
        }
        if provider_calls().any(|call| call.result_state != ResultState::Final) {
            return Err(MALFORMED_RESULT);
        }
        Ok(())
    }

    pub(crate) fn into_calls(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .map(|call| ToolCall {
                provider_result: call
                    .result
                    .filter(|_| call.result_state == ResultState::Final),
                provenance: call.provenance,
                ..ToolCall::new(call.id, call.name, call.arguments)
            })
            .collect()
    }

    fn fail(&mut self, failure: ResultFailure) {
        self.failure.get_or_insert(failure);
    }

    fn equivalent_ended(&self, name: &str, arguments: &str) -> Option<usize> {
        if name.is_empty() {
            return None;
        }
        self.streamed.iter().position(|record| {
            record.state == InputState::Ended
                && record.name == name
                && serialized_equal(&record.arguments, arguments)
        })
    }
}

struct Shown {
    id: String,
    name: String,
}

fn anomaly(kind: Anomaly) {
    trace_log!("sse", "event=stream_state_anomaly reason={}", kind.reason());
}

fn streamed_id<'a>(fields: &'a Object<'_>, kind: Anomaly) -> Option<&'a str> {
    let id = fields
        .get("id")
        .and_then(Json::as_str)
        .filter(|id| !id.is_empty());
    if id.is_none() {
        anomaly(kind);
    }
    id
}

fn final_identity(value: Option<&Json<'_>>) -> Identity {
    match value {
        None => Identity::Absent,
        Some(value) => match value.as_str() {
            None => Identity::WrongType,
            Some("") => Identity::Empty,
            Some(_) => Identity::Valid,
        },
    }
}

fn final_input(input: &Json<'_>, shown: &Shown) -> (String, bool) {
    match input {
        Json::String(text) => {
            let valid = serialized_valid(text, shown, ArgumentSource::FinalString);
            (text.to_string(), valid)
        }
        Json::Object(_) | Json::Array(_) => {
            (serde_json::to_string(input).unwrap_or_default(), true)
        }
        Json::Null | Json::Bool(_) | Json::Number(_) => {
            let kind = match input {
                Json::Null => "null",
                Json::Bool(_) => "bool",
                _ if input.is_i64() || input.is_u64() => "integer",
                _ => "float",
            };
            trace_log!(
                "sse",
                "event=tool_argument_integrity call_id={} tool_name={} source=final_value input_kind={kind} failure=malformed_json",
                shown.id,
                shown.name
            );
            (serde_json::to_string(input).unwrap_or_default(), false)
        }
    }
}

fn serialized_valid(raw: &str, shown: &Shown, source: ArgumentSource) -> bool {
    if ToolArgumentIntegrity::classify_serialized(raw) == ToolArgumentIntegrity::Valid {
        return true;
    }
    if ofx_trace::enabled("sse") {
        let diagnostic = ToolArgumentDiagnostic::diagnose(raw);
        let offset = diagnostic
            .error_offset()
            .map_or_else(|| "null".to_owned(), |offset| offset.to_string());
        trace_log!(
            "sse",
            "event=tool_argument_integrity call_id={} tool_name={} source={} bytes={} failure=malformed_json diagnosis={} error_offset={offset}",
            shown.id,
            shown.name,
            source.name(),
            raw.len(),
            diagnostic.failure_name()
        );
    }
    false
}

fn serialized_equal(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    match (
        parse_strict_json_value(left.as_bytes()),
        parse_strict_json_value(right.as_bytes()),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(test)]
mod tests;
