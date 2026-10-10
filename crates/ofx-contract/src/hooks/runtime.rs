use std::sync::Arc;

use super::definitions::{
    AttentionRequiredInput, HANDLER_NAME_BYTES, HookRegistrationError, PostTurnEndInput,
};

type SideEffect<Input> = Box<dyn Fn(&Input) + Send + Sync>;

struct Registered<Handler> {
    name: Box<str>,
    run: Handler,
}

#[derive(Default)]
pub struct HookRuntime {
    post_turn_end: Vec<Registered<SideEffect<PostTurnEndInput>>>,
    attention_required: Vec<Registered<SideEffect<AttentionRequiredInput>>>,
}

impl HookRuntime {
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
        let empty = self.post_turn_end.is_empty() && self.attention_required.is_empty();
        HookView((!empty).then(|| Arc::new(self)))
    }
}

#[derive(Clone, Default)]
pub struct HookView(Option<Arc<HookRuntime>>);

impl HookView {
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
mod tests;
