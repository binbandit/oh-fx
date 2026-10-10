use std::sync::Arc;

use super::definitions::{
    ARGUMENTS_JSON_BYTES, AttentionRequiredInput, CONTEXT_BYTES, HANDLER_NAME_BYTES,
    HookDispatchError, HookHandlerError, HookRegistrationError, PostTurnEndInput, PreToolUseAction,
    PreToolUseInput, PreToolUseOutcome, REASON_BYTES, StopAction, StopInput, StopOutcome,
};
use crate::types::ToolArgumentIntegrity;

type SideEffect<Input> = Box<dyn Fn(&Input) + Send + Sync>;
type PreToolUseHandler =
    Box<dyn Fn(&PreToolUseInput<'_>) -> Result<PreToolUseAction, HookHandlerError> + Send + Sync>;
type StopHandler =
    Box<dyn Fn(&StopInput<'_>) -> Result<StopAction, HookHandlerError> + Send + Sync>;

struct Registered<Handler> {
    name: Box<str>,
    run: Handler,
}

#[derive(Default)]
pub struct HookRuntime {
    pre_tool_use: Vec<Registered<PreToolUseHandler>>,
    stop: Vec<Registered<StopHandler>>,
    post_turn_end: Vec<Registered<SideEffect<PostTurnEndInput>>>,
    attention_required: Vec<Registered<SideEffect<AttentionRequiredInput>>>,
}

impl HookRuntime {
    pub fn register_pre_tool_use(
        &mut self,
        name: &str,
        run: impl Fn(&PreToolUseInput<'_>) -> Result<PreToolUseAction, HookHandlerError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), HookRegistrationError> {
        validate_registration(name, &self.pre_tool_use)?;
        self.pre_tool_use.push(Registered {
            name: name.into(),
            run: Box::new(run),
        });
        Ok(())
    }

    pub fn register_stop(
        &mut self,
        name: &str,
        run: impl Fn(&StopInput<'_>) -> Result<StopAction, HookHandlerError> + Send + Sync + 'static,
    ) -> Result<(), HookRegistrationError> {
        validate_registration(name, &self.stop)?;
        self.stop.push(Registered {
            name: name.into(),
            run: Box::new(run),
        });
        Ok(())
    }

    pub fn register_post_turn_end(
        &mut self,
        name: &str,
        run: impl Fn(&PostTurnEndInput) + Send + Sync + 'static,
    ) -> Result<(), HookRegistrationError> {
        register_side_effect(&mut self.post_turn_end, name, run)
    }

    pub fn register_attention_required(
        &mut self,
        name: &str,
        run: impl Fn(&AttentionRequiredInput) + Send + Sync + 'static,
    ) -> Result<(), HookRegistrationError> {
        register_side_effect(&mut self.attention_required, name, run)
    }

    pub fn freeze(self) -> HookView {
        let empty = self.pre_tool_use.is_empty()
            && self.stop.is_empty()
            && self.post_turn_end.is_empty()
            && self.attention_required.is_empty();
        HookView((!empty).then(|| Arc::new(self)))
    }
}

#[derive(Clone, Default)]
pub struct HookView(Option<Arc<HookRuntime>>);

impl HookView {
    pub fn has_pre_tool_use(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|runtime| !runtime.pre_tool_use.is_empty())
    }

    pub fn run_pre_tool_use(
        &self,
        input: &PreToolUseInput<'_>,
    ) -> Result<PreToolUseOutcome, HookDispatchError> {
        let Some(runtime) = &self.0 else {
            return Ok(PreToolUseOutcome::Unchanged);
        };
        let mut rewritten: Option<String> = None;
        for handler in &runtime.pre_tool_use {
            let current = PreToolUseInput {
                arguments_json: rewritten.as_deref().unwrap_or(input.arguments_json),
                ..*input
            };
            match (handler.run)(&current).map_err(|error| match error {
                HookHandlerError::Failed => HookDispatchError::HandlerFailed,
                HookHandlerError::Cancelled => HookDispatchError::Cancelled,
            })? {
                PreToolUseAction::Continue => {}
                PreToolUseAction::RewriteArguments(arguments_json) => {
                    rewritten = Some(validated_rewrite(arguments_json)?);
                }
                PreToolUseAction::Block(reason) => {
                    return validated_block(reason).map(PreToolUseOutcome::Blocked);
                }
            }
        }
        Ok(rewritten.map_or(PreToolUseOutcome::Unchanged, PreToolUseOutcome::Rewritten))
    }

    pub fn has_stop(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|runtime| !runtime.stop.is_empty())
    }

    pub fn run_stop(&self, input: &StopInput<'_>) -> StopOutcome {
        let Some(runtime) = &self.0 else {
            return StopOutcome::Allow;
        };
        for handler in &runtime.stop {
            match (handler.run)(input) {
                Err(_) => return StopOutcome::Allow,
                Ok(StopAction::Allow) => {}
                Ok(StopAction::ContinueOnce(context)) => {
                    return if input.can_continue && context.len() <= CONTEXT_BYTES {
                        StopOutcome::ContinueOnce(context)
                    } else {
                        StopOutcome::Allow
                    };
                }
            }
        }
        StopOutcome::Allow
    }

    pub fn has_post_turn_end(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|runtime| !runtime.post_turn_end.is_empty())
    }

    pub fn run_post_turn_end(&self, input: &PostTurnEndInput) {
        if let Some(runtime) = &self.0 {
            run_side_effects(&runtime.post_turn_end, input);
        }
    }

    pub fn has_attention_required(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|runtime| !runtime.attention_required.is_empty())
    }

    pub fn run_attention_required(&self, input: &AttentionRequiredInput) {
        if let Some(runtime) = &self.0 {
            run_side_effects(&runtime.attention_required, input);
        }
    }
}

fn validated_rewrite(arguments_json: String) -> Result<String, HookDispatchError> {
    if arguments_json.len() > ARGUMENTS_JSON_BYTES {
        return Err(HookDispatchError::HandlerOutputTooLarge);
    }
    match ToolArgumentIntegrity::classify_function_input(&arguments_json) {
        ToolArgumentIntegrity::Valid => Ok(arguments_json),
        ToolArgumentIntegrity::MalformedJson | ToolArgumentIntegrity::NonObjectJson => {
            Err(HookDispatchError::InvalidHandlerOutput)
        }
    }
}

fn validated_block(reason: String) -> Result<String, HookDispatchError> {
    if reason.is_empty() {
        return Err(HookDispatchError::InvalidHandlerOutput);
    }
    if reason.len() > REASON_BYTES {
        return Err(HookDispatchError::HandlerOutputTooLarge);
    }
    Ok(reason)
}

fn register_side_effect<Input>(
    registered: &mut Vec<Registered<SideEffect<Input>>>,
    name: &str,
    run: impl Fn(&Input) + Send + Sync + 'static,
) -> Result<(), HookRegistrationError> {
    validate_registration(name, registered)?;
    registered.push(Registered {
        name: name.into(),
        run: Box::new(run),
    });
    Ok(())
}

fn run_side_effects<Input>(handlers: &[Registered<SideEffect<Input>>], input: &Input) {
    for handler in handlers {
        (handler.run)(input);
    }
}

fn validate_registration<Handler>(
    name: &str,
    registered: &[Registered<Handler>],
) -> Result<(), HookRegistrationError> {
    validate_handler_name(name)?;
    if registered.iter().any(|handler| *handler.name == *name) {
        return Err(HookRegistrationError::DuplicateHandlerName);
    }
    Ok(())
}

fn validate_handler_name(name: &str) -> Result<(), HookRegistrationError> {
    if name.is_empty() {
        return Err(HookRegistrationError::EmptyHandlerName);
    }
    if name.len() > HANDLER_NAME_BYTES {
        return Err(HookRegistrationError::HandlerNameTooLong);
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(HookRegistrationError::InvalidHandlerName);
    }
    Ok(())
}

#[cfg(test)]
mod pre_tool_use_tests;
#[cfg(test)]
mod stop_tests;
#[cfg(test)]
mod tests;
