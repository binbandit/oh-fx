use std::sync::Arc;

use super::definitions::{AttentionRequiredInput, HANDLER_NAME_BYTES, HookRegistrationError};

type AttentionRequiredHandler = Box<dyn Fn(&AttentionRequiredInput) + Send + Sync>;

struct Registered<Handler> {
    name: Box<str>,
    run: Handler,
}

#[derive(Default)]
pub struct HookRuntime {
    attention_required: Vec<Registered<AttentionRequiredHandler>>,
}

impl HookRuntime {
    pub fn register_attention_required(
        &mut self,
        name: &str,
        run: impl Fn(&AttentionRequiredInput) + Send + Sync + 'static,
    ) -> Result<(), HookRegistrationError> {
        validate_registration(name, &self.attention_required)?;
        self.attention_required.push(Registered {
            name: name.into(),
            run: Box::new(run),
        });
        Ok(())
    }

    pub fn freeze(self) -> HookView {
        HookView((!self.attention_required.is_empty()).then(|| Arc::new(self)))
    }
}

#[derive(Clone, Default)]
pub struct HookView(Option<Arc<HookRuntime>>);

impl HookView {
    pub fn has_attention_required(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|runtime| !runtime.attention_required.is_empty())
    }

    pub fn run_attention_required(&self, input: &AttentionRequiredInput) {
        if let Some(runtime) = &self.0 {
            for handler in &runtime.attention_required {
                (handler.run)(input);
            }
        }
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
