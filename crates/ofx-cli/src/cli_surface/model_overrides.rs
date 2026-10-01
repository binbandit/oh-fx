use std::ffi::{OsStr, OsString};

use ofx_config::is_valid_provider_order_list;
use ofx_contract::is_valid_reasoning_effort;

use super::arg_stream::{ArgStream, MissingValue, ValueForm, merge_toggle, non_blank};
use super::launch_modifiers::GlobalLaunchError;

pub(crate) enum ModelOverride {
    Model(OsString),
    Setting,
}

#[derive(Debug, Default)]
pub(crate) struct ModelOverrides {
    fast: Option<bool>,
    provider_strict: Option<bool>,
}

impl ModelOverrides {
    pub(crate) fn take(
        &mut self,
        args: &mut ArgStream,
        form: ValueForm,
    ) -> Result<Option<ModelOverride>, GlobalLaunchError> {
        if let Some(value) = args.take_option("model", form) {
            let value = value.map_err(|MissingValue| GlobalLaunchError::MissingModelValue)?;
            let model = non_blank(&value).ok_or(GlobalLaunchError::MissingModelValue)?;
            return Ok(Some(ModelOverride::Model(model.to_os_string())));
        }
        if let Some(value) = args.take_option("effort", form) {
            let value = value.map_err(|MissingValue| GlobalLaunchError::MissingEffortValue)?;
            if !is_text(&value, is_valid_reasoning_effort) {
                return Err(GlobalLaunchError::InvalidEffortValue);
            }
        } else if let Some(enabled) = args.take_toggle("--fast", "--no-fast") {
            let fast = merge_toggle(self.fast, enabled, GlobalLaunchError::ConflictingFastFlags)?;
            self.fast = Some(fast);
        } else if let Some(value) = args.take_option("provider-order", ValueForm::SeparateOrJoined)
        {
            let value =
                value.map_err(|MissingValue| GlobalLaunchError::MissingProviderOrderValue)?;
            if !is_text(&value, is_valid_provider_order_list) {
                return Err(GlobalLaunchError::InvalidProviderOrderValue);
            }
        } else if let Some(strict) = args.take_toggle("--provider-strict", "--no-provider-strict") {
            let strict = merge_toggle(
                self.provider_strict,
                strict,
                GlobalLaunchError::ConflictingProviderStrictFlags,
            )?;
            self.provider_strict = Some(strict);
        } else {
            return Ok(None);
        }
        Ok(Some(ModelOverride::Setting))
    }
}

fn is_text(value: &OsStr, valid: impl FnOnce(&str) -> bool) -> bool {
    value.to_str().is_some_and(valid)
}
