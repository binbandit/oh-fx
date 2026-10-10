use ofx_config::ProviderId;
use ofx_contract::{
    CommandProcessPresentation, ToolArgumentIntegrity, ToolExecutionProvenance, ToolResultStatus,
    TurnSummary, TurnTokenProgress,
};

use super::{
    ArtifactCompleteness, AssistantEvent, CONVERSATION_SCHEMA_VERSION, ContextCheckpointEvent,
    ConversationEnvelope, ConversationEvent, FileEvidence, FileEvidenceAction, InterruptedEvent,
    SavedReplay, SavedReplaySource, SteeringEvent, ToolCallEvent, ToolResultEvent,
    TurnCompletedEvent, UserEvent, WireTag, validate_event_shape,
};
use crate::fixed_field::{False, NoItems, Null, TurnOrigin, ValidIdentity};
use crate::session_codec::SavedProvider;

const PROVIDER_ID_BYTES: usize = 64;
const BINDING_BYTES: usize = 32;
const CONFIGURED_PROVIDER_TAG: u8 = 3;
const BUILT_IN_PROVIDERS: [ProviderId; 3] =
    [ProviderId::Gateway, ProviderId::Codex, ProviderId::Grok];

pub(crate) fn encode_history_envelope(
    out: &mut Vec<u8>,
    envelope: &ConversationEnvelope,
) -> Option<()> {
    let mut encoder = Encoder { out };
    encoder.int(u64::from(envelope.schema_version));
    encoder.int(envelope.seq);
    encoder.signed(envelope.timestamp_ms);
    encoder.event(&envelope.event)
}

pub(crate) fn decode_history_envelope(bytes: &[u8]) -> Option<ConversationEnvelope> {
    let mut decoder = Decoder { bytes };
    let envelope = ConversationEnvelope {
        schema_version: u8::try_from(decoder.int()?).ok()?,
        seq: decoder.int()?,
        timestamp_ms: decoder.signed()?,
        event: decoder.event()?,
    };
    let valid = decoder.bytes.is_empty()
        && envelope.schema_version == CONVERSATION_SCHEMA_VERSION
        && envelope.seq != 0
        && envelope.timestamp_ms >= 0
        && validate_event_shape(&envelope.event).is_ok();
    valid.then_some(envelope)
}

struct Encoder<'a> {
    out: &'a mut Vec<u8>,
}

impl Encoder<'_> {
    fn byte(&mut self, value: u8) {
        self.out.push(value);
    }

    fn flag(&mut self, value: bool) {
        self.byte(u8::from(value));
    }

    fn int(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    fn signed(&mut self, value: i64) {
        self.int(value.cast_unsigned());
    }

    fn tag<T: WireTag + PartialEq>(&mut self, value: T) -> Option<()> {
        let index = T::ALL.iter().position(|candidate| *candidate == value)?;
        self.byte(u8::try_from(index).ok()?);
        Some(())
    }

    fn bytes(&mut self, value: &[u8]) -> Option<()> {
        self.int(u64::from(u32::try_from(value.len()).ok()?));
        self.out.extend_from_slice(value);
        Some(())
    }

    fn text(&mut self, value: &str) -> Option<()> {
        self.bytes(value.as_bytes())
    }

    fn optional_text(&mut self, value: Option<&str>) -> Option<()> {
        self.flag(value.is_some());
        value.map_or(Some(()), |text| self.text(text))
    }

    fn optional_int(&mut self, value: Option<u64>) {
        self.flag(value.is_some());
        if let Some(value) = value {
            self.int(value);
        }
    }

    fn absent(&mut self) {
        self.byte(0);
    }

    fn summary(&mut self, summary: Option<TurnSummary>) {
        self.flag(summary.is_some());
        if let Some(summary) = summary {
            self.signed(summary.started_at_ms);
            self.signed(summary.completed_at_ms);
            self.int(summary.thinking_duration_ms);
            self.int(summary.turn_duration_ms);
            self.int(summary.token_progress.input_tokens);
            self.int(summary.token_progress.output_tokens);
            self.flag(summary.token_progress.input_exact);
            self.flag(summary.token_progress.output_exact);
        }
    }

    fn empty(&mut self) {
        self.int(0);
    }

    fn event(&mut self, event: &ConversationEvent) -> Option<()> {
        match event {
            ConversationEvent::User(user) => {
                self.byte(0);
                self.text(&user.text)?;
                self.empty();
                self.optional_text(user.work_id.as_deref())
            }
            ConversationEvent::Assistant(assistant) => {
                self.byte(1);
                self.text(&assistant.text)?;
                self.flag(assistant.provider_replay.is_some());
                if let Some(replay) = &assistant.provider_replay {
                    self.provider(&replay.source.provider)?;
                    self.text(&replay.source.model)?;
                    self.text(&replay.parts_json)?;
                }
                self.flag(assistant.standalone_response);
                Some(())
            }
            ConversationEvent::ToolCall(call) => self.tool_call(call),
            ConversationEvent::ToolResult(result) => self.tool_result(result),
            ConversationEvent::Steering(steering) => {
                self.byte(4);
                self.text(&steering.text)
            }
            ConversationEvent::TurnCompleted(completed) => {
                self.byte(5);
                self.files(&completed.files)?;
                self.summary(completed.turn_summary);
                Some(())
            }
            ConversationEvent::Interrupted(interrupted) => {
                self.byte(6);
                self.tag(interrupted.reason)?;
                self.optional_text(interrupted.partial_text.as_deref())?;
                self.optional_text(interrupted.command_replay_ref.as_deref())?;
                self.optional_int(interrupted.command_replay_bytes);
                self.optional_text(interrupted.command_artifact_ref.as_deref())?;
                self.files(&interrupted.files)?;
                self.summary(interrupted.turn_summary);
                self.byte(0);
                Some(())
            }
            ConversationEvent::ContextCheckpoint(checkpoint) => {
                self.byte(7);
                self.int(checkpoint.covers_through_seq);
                self.text(&checkpoint.summary)
            }
        }
    }

    fn provider(&mut self, provider: &SavedProvider) -> Option<()> {
        let id = provider.id();
        let ProviderId::Configured(name) = id else {
            let index = BUILT_IN_PROVIDERS.iter().position(|known| known == id)?;
            self.byte(u8::try_from(index).ok()?);
            return Some(());
        };
        let mut padded = [0_u8; PROVIDER_ID_BYTES];
        padded
            .get_mut(..name.len())?
            .copy_from_slice(name.as_bytes());
        self.byte(CONFIGURED_PROVIDER_TAG);
        self.out.extend_from_slice(&padded);
        self.int(u64::try_from(name.len()).ok()?);
        self.flag(provider.binding().is_some());
        if let Some(binding) = provider.binding() {
            self.out.extend_from_slice(&binding);
        }
        Some(())
    }

    fn tool_call(&mut self, call: &ToolCallEvent) -> Option<()> {
        self.byte(2);
        self.text(&call.call_id)?;
        self.text(&call.tool_name)?;
        self.text(&call.arguments_json)?;
        self.tag(call.argument_integrity)?;
        self.absent();
        self.optional_text(call.provider_result.as_deref())?;
        self.byte(0);
        self.tag(call.provenance)
    }

    fn tool_result(&mut self, result: &ToolResultEvent) -> Option<()> {
        self.byte(3);
        self.text(&result.call_id)?;
        self.text(&result.tool_name)?;
        self.tag(result.status)?;
        self.text(&result.artifact_ref)?;
        self.absent();
        self.optional_int(result.output_bytes);
        self.int(result.stored_bytes);
        self.tag(result.completeness)?;
        self.optional_text(result.preview.as_deref())?;
        self.flag(result.provider_native);
        self.flag(false);
        self.signed(result.created_at_ms);
        self.texts(&result.permission_feedback)?;
        self.absent();
        self.optional_text(result.command_replay_ref.as_deref())?;
        self.optional_int(result.command_replay_bytes);
        self.process(result.command_process_presentation);
        self.absent();
        Some(())
    }

    fn process(&mut self, presentation: Option<CommandProcessPresentation>) {
        self.flag(presentation.is_some());
        match presentation {
            None => {}
            Some(CommandProcessPresentation::ExitCode(code)) => {
                self.byte(0);
                self.signed(code);
            }
            Some(CommandProcessPresentation::Signal(signal)) => {
                self.byte(1);
                self.int(u64::from(signal));
            }
            Some(CommandProcessPresentation::TimedOut) => self.byte(2),
            Some(CommandProcessPresentation::OutputCaptureFailed) => self.byte(3),
        }
    }

    fn texts(&mut self, values: &[String]) -> Option<()> {
        self.int(u64::from(u32::try_from(values.len()).ok()?));
        values.iter().try_for_each(|value| self.text(value))
    }

    fn files(&mut self, files: &[FileEvidence]) -> Option<()> {
        self.int(u64::from(u32::try_from(files.len()).ok()?));
        for file in files {
            self.text(&file.path)?;
            self.optional_text(file.new_path.as_deref())?;
            self.text(&file.tool_call_id)?;
            self.text(&file.tool_name)?;
            self.tag(file.action)?;
            self.tag(file.status)?;
            self.flag(file.model_view_covers_full_file);
            self.flag(file.stale);
        }
        Some(())
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
}

struct Corrupt;

impl<'a> Decoder<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.bytes.split_at_checked(count)?;
        self.bytes = rest;
        Some(head)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    fn byte(&mut self) -> Option<u8> {
        let [value] = self.array()?;
        Some(value)
    }

    fn flag(&mut self) -> Option<bool> {
        match self.byte()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn int(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.array()?))
    }

    fn signed(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.array()?))
    }

    fn length(&mut self) -> Option<usize> {
        usize::try_from(u32::try_from(self.int()?).ok()?).ok()
    }

    fn tag<T: WireTag>(&mut self) -> Option<T> {
        T::ALL.get(usize::from(self.byte()?)).copied()
    }

    fn text(&mut self) -> Option<String> {
        let length = self.length()?;
        let bytes = self.take(length)?;
        Some(std::str::from_utf8(bytes).ok()?.to_owned())
    }

    fn optional<T>(
        &mut self,
        read: impl FnOnce(&mut Self) -> Option<T>,
    ) -> Result<Option<T>, Corrupt> {
        match self.flag() {
            Some(false) => Ok(None),
            Some(true) => read(self).map(Some).ok_or(Corrupt),
            None => Err(Corrupt),
        }
    }

    fn optional_text(&mut self) -> Result<Option<String>, Corrupt> {
        self.optional(Self::text)
    }

    fn process(&mut self) -> Option<CommandProcessPresentation> {
        Some(match self.byte()? {
            0 => CommandProcessPresentation::ExitCode(self.signed()?),
            1 => CommandProcessPresentation::Signal(u32::try_from(self.int()?).ok()?),
            2 => CommandProcessPresentation::TimedOut,
            3 => CommandProcessPresentation::OutputCaptureFailed,
            _ => return None,
        })
    }

    fn summary(&mut self) -> Option<TurnSummary> {
        Some(TurnSummary {
            started_at_ms: self.signed()?,
            completed_at_ms: self.signed()?,
            thinking_duration_ms: self.int()?,
            turn_duration_ms: self.int()?,
            token_progress: TurnTokenProgress {
                input_tokens: self.int()?,
                output_tokens: self.int()?,
                input_exact: self.flag()?,
                output_exact: self.flag()?,
            },
        })
    }

    fn fixed<T: Default>(&mut self) -> Option<T> {
        (self.byte()? == 0).then(T::default)
    }

    fn no_items(&mut self) -> Option<NoItems> {
        (self.int()? == 0).then_some(NoItems)
    }

    fn event(&mut self) -> Option<ConversationEvent> {
        Some(match self.byte()? {
            0 => ConversationEvent::User(UserEvent {
                text: self.text()?,
                images: self.no_items()?,
                work_id: self.optional_text().ok()?,
            }),
            1 => ConversationEvent::Assistant(self.assistant()?),
            2 => ConversationEvent::ToolCall(self.tool_call()?),
            3 => ConversationEvent::ToolResult(self.tool_result()?),
            4 => ConversationEvent::Steering(SteeringEvent { text: self.text()? }),
            5 => ConversationEvent::TurnCompleted(TurnCompletedEvent {
                files: self.files()?,
                turn_summary: self.optional(Self::summary).ok()?,
            }),
            6 => ConversationEvent::Interrupted(self.interrupted()?),
            7 => ConversationEvent::ContextCheckpoint(ContextCheckpointEvent {
                covers_through_seq: self.int()?,
                summary: self.text()?,
            }),
            _ => return None,
        })
    }

    fn assistant(&mut self) -> Option<AssistantEvent> {
        let text = self.text()?;
        let provider_replay = if self.flag()? {
            Some(SavedReplay {
                source: SavedReplaySource {
                    provider: self.provider()?,
                    model: self.text()?,
                },
                parts_json: self.text()?,
            })
        } else {
            None
        };
        Some(AssistantEvent {
            text,
            provider_replay,
            standalone_response: self.flag()?,
        })
    }

    fn provider(&mut self) -> Option<SavedProvider> {
        let tag = self.byte()?;
        if tag != CONFIGURED_PROVIDER_TAG {
            let id = BUILT_IN_PROVIDERS.get(usize::from(tag))?.clone();
            return SavedProvider::new(id, None);
        }
        let padded: [u8; PROVIDER_ID_BYTES] = self.array()?;
        let length = usize::try_from(self.int()?).ok()?;
        let binding = if self.flag()? {
            Some(self.array::<BINDING_BYTES>()?)
        } else {
            None
        };
        let name = std::str::from_utf8(padded.get(..length)?).ok()?;
        let id = ProviderId::parse(name)?;
        if !matches!(&id, ProviderId::Configured(parsed) if parsed == name) {
            return None;
        }
        SavedProvider::new(id, binding)
    }

    fn tool_call(&mut self) -> Option<ToolCallEvent> {
        Some(ToolCallEvent {
            call_id: self.text()?,
            tool_name: self.text()?,
            arguments_json: self.text()?,
            argument_integrity: self.tag::<ToolArgumentIntegrity>()?,
            provisional_id: self.fixed::<Null>()?,
            provider_result: self.optional_text().ok()?,
            final_identity: self.fixed::<ValidIdentity>()?,
            provenance: self.tag::<ToolExecutionProvenance>()?,
        })
    }

    fn tool_result(&mut self) -> Option<ToolResultEvent> {
        Some(ToolResultEvent {
            call_id: self.text()?,
            tool_name: self.text()?,
            status: self.tag::<ToolResultStatus>()?,
            artifact_ref: self.text()?,
            tool_image_handle: self.fixed::<Null>()?,
            output_bytes: self.optional(Self::int).ok()?,
            stored_bytes: self.int()?,
            completeness: self.tag::<ArtifactCompleteness>()?,
            preview: self.optional_text().ok()?,
            provider_native: self.flag()?,
            review_feedback: self.fixed::<False>()?,
            created_at_ms: self.signed()?,
            permission_feedback: self.texts()?,
            committed_file_presentation: self.fixed::<Null>()?,
            command_replay_ref: self.optional_text().ok()?,
            command_replay_bytes: self.optional(Self::int).ok()?,
            command_process_presentation: self.optional(Self::process).ok()?,
            terminal_action_presentation: self.fixed::<Null>()?,
        })
    }

    fn interrupted(&mut self) -> Option<InterruptedEvent> {
        Some(InterruptedEvent {
            reason: self.tag()?,
            partial_text: self.optional_text().ok()?,
            command_replay_ref: self.optional_text().ok()?,
            command_replay_bytes: self.optional(Self::int).ok()?,
            command_artifact_ref: self.optional_text().ok()?,
            files: self.files()?,
            turn_summary: self.optional(Self::summary).ok()?,
            cancellation_origin: self.fixed::<TurnOrigin>()?,
        })
    }

    fn texts(&mut self) -> Option<Vec<String>> {
        let count = self.length()?;
        (0..count).map(|_| self.text()).collect()
    }

    fn files(&mut self) -> Option<Vec<FileEvidence>> {
        let count = self.length()?;
        let mut files = Vec::with_capacity(count.min(self.bytes.len()));
        for _ in 0..count {
            files.push(FileEvidence {
                path: self.text()?,
                new_path: self.optional_text().ok()?,
                tool_call_id: self.text()?,
                tool_name: self.text()?,
                action: self.tag::<FileEvidenceAction>()?,
                status: self.tag::<ToolResultStatus>()?,
                model_view_covers_full_file: self.flag()?,
                stale: self.flag()?,
            });
        }
        Some(files)
    }
}

#[cfg(test)]
mod tests;
