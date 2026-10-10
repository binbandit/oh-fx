use std::ffi::{OsStr, OsString};

use ofx_config::is_valid_provider_order_list;
use ofx_contract::ReasoningEffort;

use super::arg_stream::{ArgStream, MissingValue, ValueForm, merge_toggle, non_blank};
use super::launch_modifiers::GlobalLaunchError;

pub const ULTRAFAST_ARG: &str = "--ultrafast";
pub const NO_ULTRAFAST_ARG: &str = "--no-ultrafast";

pub(crate) enum ModelOverride {
    Model(OsString),
    Setting,
}

#[derive(Debug, Default)]
pub(crate) struct ModelOverrides {
    pub(crate) effort: Option<ReasoningEffort>,
    pub(crate) fast: Option<bool>,
    pub(crate) ultrafast: Option<bool>,
    pub(crate) routes: bool,
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
            let effort = value.to_str().and_then(ReasoningEffort::parse);
            self.effort = Some(effort.ok_or(GlobalLaunchError::InvalidEffortValue)?);
        } else if let Some(enabled) = args.take_toggle("--fast", "--no-fast") {
            let fast = merge_toggle(self.fast, enabled, GlobalLaunchError::ConflictingFastFlags)?;
            self.fast = Some(fast);
            if enabled {
                self.ultrafast = Some(false);
            }
        } else if let Some(enabled) = args.take_toggle(ULTRAFAST_ARG, NO_ULTRAFAST_ARG) {
            let ultrafast = merge_toggle(
                self.ultrafast,
                enabled,
                GlobalLaunchError::ConflictingUltrafastFlags,
            )?;
            self.ultrafast = Some(ultrafast);
            if enabled {
                self.fast = Some(false);
            }
        } else if let Some(value) = args.take_option("provider-order", ValueForm::SeparateOrJoined)
        {
            let value =
                value.map_err(|MissingValue| GlobalLaunchError::MissingProviderOrderValue)?;
            if !is_text(&value, is_valid_provider_order_list) {
                return Err(GlobalLaunchError::InvalidProviderOrderValue);
            }
            self.routes = true;
        } else if let Some(strict) = args.take_toggle("--provider-strict", "--no-provider-strict") {
            let strict = merge_toggle(
                self.provider_strict,
                strict,
                GlobalLaunchError::ConflictingProviderStrictFlags,
            )?;
            self.provider_strict = Some(strict);
            self.routes = true;
        } else {
            return Ok(None);
        }
        Ok(Some(ModelOverride::Setting))
    }
}

fn is_text(value: &OsStr, valid: impl FnOnce(&str) -> bool) -> bool {
    value.to_str().is_some_and(valid)
}
