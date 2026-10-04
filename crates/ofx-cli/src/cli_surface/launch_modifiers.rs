use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;

use ofx_config::{
    ContextLimitError, ContextLimitOverride, is_valid_provider_id, parse_context_limit_override,
};
use ofx_contract::ReasoningEffort;

use super::arg_stream::{ArgStream, MissingValue, ValueForm};
use super::model_overrides::{ModelOverride, ModelOverrides};

#[derive(Debug, Default)]
pub struct LaunchModifiers {
    context_limits: Vec<ContextLimitOverride>,
    workspace: WorkspaceModifiers,
    model: ModelModifiers,
    sessions_v2: bool,
}

#[derive(Debug, Default)]
struct ModelModifiers {
    overridden: bool,
    provider: bool,
    model: Option<OsString>,
    effort: Option<ReasoningEffort>,
    fast: Option<bool>,
    ultrafast: Option<bool>,
    routes: bool,
}

#[derive(Debug, Default)]
struct WorkspaceModifiers {
    additional_directories: bool,
    saved_directories_suppressed: bool,
}

impl LaunchModifiers {
    pub fn context_limit_overrides(&self) -> &[ContextLimitOverride] {
        &self.context_limits
    }

    pub fn adds_directories(&self) -> bool {
        self.workspace.additional_directories
    }

    pub fn selects_sessions_v2(&self) -> bool {
        self.sessions_v2
    }

    pub fn overrides_provider(&self) -> bool {
        self.model.provider
    }

    pub fn model(&self) -> Option<&OsStr> {
        self.model.model.as_deref()
    }

    pub fn reasoning_effort(&self) -> Option<&ReasoningEffort> {
        self.model.effort.as_ref()
    }

    pub fn fast_mode(&self) -> Option<bool> {
        self.model.fast
    }

    pub(crate) fn has_workspace_modifiers(&self) -> bool {
        self.workspace.additional_directories || self.workspace.saved_directories_suppressed
    }

    pub(crate) fn has_model_overrides(&self) -> bool {
        self.model.overridden
    }

    pub(crate) fn has_only_ultrafast_override(&self) -> bool {
        let model = &self.model;
        model.ultrafast.is_some()
            && !model.provider
            && model.model.is_none()
            && model.effort.is_none()
            && model.fast != Some(true)
            && !model.routes
    }
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum GlobalLaunchError {
    #[error("invalid global launch option: MissingContextLimitValue")]
    MissingContextLimitValue,
    #[error("invalid global launch option: {0}")]
    ContextLimit(#[from] ContextLimitError),
    #[error("--add-dir requires a directory path")]
    MissingAddDirectoryValue,
    #[error("--no-additional-dirs may only be specified once")]
    DuplicateAdditionalDirectorySuppression,
    #[error("--provider requires a provider name")]
    MissingProviderValue,
    #[error("--provider accepts gateway, codex, grok, or a configured provider name")]
    InvalidProviderValue,
    #[error("--model requires a model id")]
    MissingModelValue,
    #[error("--effort requires a value")]
    MissingEffortValue,
    #[error("--effort value is not a valid reasoning effort")]
    InvalidEffortValue,
    #[error("--fast and --no-fast cannot be used together")]
    ConflictingFastFlags,
    #[error("--ultrafast and --no-ultrafast cannot be used together")]
    ConflictingUltrafastFlags,
    #[error("--provider-order requires a comma-separated provider list")]
    MissingProviderOrderValue,
    #[error("--provider-order accepts comma-separated provider slugs (letters, digits, '-')")]
    InvalidProviderOrderValue,
    #[error("--provider-strict and --no-provider-strict cannot be used together")]
    ConflictingProviderStrictFlags,
}

pub(crate) fn parse_launch_modifiers(
    args: &mut ArgStream,
) -> Result<LaunchModifiers, GlobalLaunchError> {
    let mut modifiers = LaunchModifiers::default();
    let mut model_overrides = ModelOverrides::default();
    while modifiers.take_next(args, &mut model_overrides)? {}
    modifiers.model.effort = model_overrides.effort;
    modifiers.model.fast = model_overrides.fast;
    modifiers.model.ultrafast = model_overrides.ultrafast;
    modifiers.model.routes = model_overrides.routes;
    Ok(modifiers)
}

impl LaunchModifiers {
    fn take_next(
        &mut self,
        args: &mut ArgStream,
        model_overrides: &mut ModelOverrides,
    ) -> Result<bool, GlobalLaunchError> {
        let joined = ValueForm::SeparateOrJoined;
        if args.take_flag("--sessions-v2") {
            self.sessions_v2 = true;
        } else if let Some(value) = args.take_option("context-limit", joined) {
            let value =
                value.map_err(|MissingValue| GlobalLaunchError::MissingContextLimitValue)?;
            self.context_limits
                .push(parse_context_limit_override(value.as_bytes())?);
        } else if let Some(value) = args.take_option("add-dir", joined) {
            let value =
                value.map_err(|MissingValue| GlobalLaunchError::MissingAddDirectoryValue)?;
            if value.is_empty() {
                return Err(GlobalLaunchError::MissingAddDirectoryValue);
            }
            self.workspace.additional_directories = true;
        } else if args.take_flag("--no-additional-dirs") {
            if self.workspace.saved_directories_suppressed {
                return Err(GlobalLaunchError::DuplicateAdditionalDirectorySuppression);
            }
            self.workspace.saved_directories_suppressed = true;
        } else if let Some(value) = args.take_option("provider", joined) {
            let value = value.map_err(|MissingValue| GlobalLaunchError::MissingProviderValue)?;
            if !value.to_str().is_some_and(is_valid_provider_id) {
                return Err(GlobalLaunchError::InvalidProviderValue);
            }
            self.model.overridden = true;
            self.model.provider = true;
        } else if let Some(model_override) = model_overrides.take(args, joined)? {
            if let ModelOverride::Model(model) = model_override {
                self.model.model = Some(model);
            }
            self.model.overridden = true;
        } else {
            return Ok(false);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    use ofx_config::ContextLimitName;

    use super::*;

    fn parse_raw(
        args: Vec<OsString>,
    ) -> Result<(LaunchModifiers, Vec<OsString>), GlobalLaunchError> {
        let mut stream = ArgStream::new(args);
        let modifiers = parse_launch_modifiers(&mut stream)?;
        Ok((modifiers, stream.collect()))
    }

    fn parse(args: &[&str]) -> Result<(LaunchModifiers, Vec<OsString>), GlobalLaunchError> {
        parse_raw(args.iter().map(OsString::from).collect())
    }

    fn error(args: &[&str]) -> String {
        parse(args).unwrap_err().to_string()
    }

    #[test]
    fn global_launch_modifiers_preserve_repeatable_context_limits_before_the_command() {
        let (modifiers, remaining) = parse(&[
            "--context-limit",
            "skill_chunk_bytes=4096",
            "--context-limit=mcp_description_bytes=off",
            "ask",
            "hello",
        ])
        .unwrap();
        assert_eq!(
            modifiers
                .context_limit_overrides()
                .iter()
                .map(|limit| limit.name)
                .collect::<Vec<_>>(),
            [
                ContextLimitName::SkillChunkBytes,
                ContextLimitName::McpDescriptionBytes
            ]
        );
        assert!(!modifiers.has_workspace_modifiers());
        assert_eq!(
            remaining,
            vec![OsString::from("ask"), OsString::from("hello")]
        );
    }

    #[test]
    fn global_context_limits_reject_missing_values_and_stop_at_the_command() {
        assert_eq!(
            error(&["--context-limit"]),
            GlobalLaunchError::MissingContextLimitValue.to_string()
        );
        let (modifiers, remaining) =
            parse(&["ask", "--context-limit", "skill_chunk_bytes=1"]).unwrap();
        assert!(modifiers.context_limit_overrides().is_empty());
        assert_eq!(remaining.len(), 3);
    }

    #[test]
    fn global_launch_modifiers_own_repeatable_additional_directories_and_suppression() {
        let (modifiers, remaining) = parse(&[
            "--add-dir",
            "/tmp/shared one",
            "--context-limit=skill_chunk_bytes=2048",
            "--add-dir=/tmp/shared-two",
            "--no-additional-dirs",
            "ask",
            "inspect",
        ])
        .unwrap();
        assert!(modifiers.adds_directories());
        assert!(modifiers.workspace.saved_directories_suppressed);
        assert!(modifiers.has_workspace_modifiers());
        assert_eq!(remaining[0], "ask");

        let (suppressed, _) = parse(&["--no-additional-dirs"]).unwrap();
        assert!(!suppressed.adds_directories());
        assert!(suppressed.has_workspace_modifiers());
    }

    #[test]
    fn global_launch_modifiers_own_provider_model_effort_and_fast_overrides_before_the_command() {
        let (modifiers, remaining) = parse(&[
            "--provider",
            "grok",
            "--model",
            "provider/launch-model",
            "--effort=high",
            "--fast",
            "--add-dir",
            "/tmp/shared",
        ])
        .unwrap();
        assert!(modifiers.has_model_overrides());
        assert!(modifiers.overrides_provider());
        assert_eq!(modifiers.model(), Some(OsStr::new("provider/launch-model")));
        assert!(modifiers.adds_directories());
        assert!(remaining.is_empty());

        for args in [
            &["--provider-order", "azure, anthropic", "--provider-strict"][..],
            &["--model= provider/spaced ", "--effort", "low", "--no-fast"],
            &["--provider=my-llm"],
            &["--effort", "auto"],
            &["--no-provider-strict"],
        ] {
            let (modifiers, remaining) = parse(args).unwrap();
            assert!(modifiers.has_model_overrides(), "{args:?}");
            assert!(remaining.is_empty(), "{args:?}");
        }

        let (untouched, remaining) = parse(&["ask", "--fast"]).unwrap();
        assert!(!untouched.has_model_overrides());
        assert_eq!(remaining.len(), 2);
    }

    #[test]
    fn global_launch_modifiers_keep_the_requested_model_for_interactive_launches() {
        let (modifiers, _) = parse(&["--fast", "--model=vendor/model-b"]).unwrap();
        assert_eq!(modifiers.model(), Some(OsStr::new("vendor/model-b")));
        assert!(!modifiers.overrides_provider());
        assert_eq!(modifiers.fast_mode(), Some(true));
        assert_eq!(modifiers.reasoning_effort(), None);
        let (settings_only, _) = parse(&["--effort", "low", "--no-fast"]).unwrap();
        assert!(settings_only.has_model_overrides());
        assert_eq!(settings_only.model(), None);
        assert_eq!(
            settings_only.reasoning_effort(),
            Some(&ReasoningEffort::Named("low".to_owned()))
        );
        assert_eq!(settings_only.fast_mode(), Some(false));
        let (raw, _) = parse_raw(vec![OsString::from_vec(b"--model=m\xff".to_vec())]).unwrap();
        assert_eq!(raw.model().map(OsStrExt::as_bytes), Some(&b"m\xff"[..]));
    }

    #[test]
    fn global_model_overrides_keep_non_utf8_models() {
        let (modifiers, remaining) = parse_raw(vec![
            OsString::from("--model"),
            OsString::from_vec(b" m\xff ".to_vec()),
            OsString::from_vec(b"--model=\xfe".to_vec()),
        ])
        .unwrap();
        assert!(modifiers.has_model_overrides());
        assert!(remaining.is_empty());
        assert_eq!(
            parse_raw(vec![OsString::from_vec(b"--model= \t".to_vec())])
                .unwrap_err()
                .to_string(),
            GlobalLaunchError::MissingModelValue.to_string()
        );
    }

    #[test]
    fn global_model_overrides_fail_closed_when_malformed() {
        for (args, expected) in [
            (&["--provider"][..], GlobalLaunchError::MissingProviderValue),
            (
                &["--provider", "bogus name"],
                GlobalLaunchError::InvalidProviderValue,
            ),
            (&["--model"], GlobalLaunchError::MissingModelValue),
            (&["--model="], GlobalLaunchError::MissingModelValue),
            (&["--effort"], GlobalLaunchError::MissingEffortValue),
            (
                &["--effort=not an effort"],
                GlobalLaunchError::InvalidEffortValue,
            ),
            (
                &["--fast", "--no-fast"],
                GlobalLaunchError::ConflictingFastFlags,
            ),
            (
                &["--no-fast", "--fast"],
                GlobalLaunchError::ConflictingFastFlags,
            ),
            (
                &["--provider-order"],
                GlobalLaunchError::MissingProviderOrderValue,
            ),
            (
                &["--provider-order=Bad Slug"],
                GlobalLaunchError::InvalidProviderOrderValue,
            ),
            (
                &["--provider-order=azure,azure"],
                GlobalLaunchError::InvalidProviderOrderValue,
            ),
            (
                &["--provider-order="],
                GlobalLaunchError::InvalidProviderOrderValue,
            ),
            (
                &["--provider-strict", "--no-provider-strict"],
                GlobalLaunchError::ConflictingProviderStrictFlags,
            ),
            (
                &["--ultrafast", "--no-ultrafast"],
                GlobalLaunchError::ConflictingUltrafastFlags,
            ),
            (
                &["--fast", "--ultrafast"],
                GlobalLaunchError::ConflictingUltrafastFlags,
            ),
            (
                &["--ultrafast", "--fast"],
                GlobalLaunchError::ConflictingFastFlags,
            ),
        ] {
            assert_eq!(error(args), expected.to_string(), "{args:?}");
        }
        assert!(parse(&["--fast", "--fast"]).is_ok());
        assert!(parse(&["--ultrafast", "--ultrafast"]).is_ok());
        assert_eq!(
            GlobalLaunchError::ConflictingUltrafastFlags.to_string(),
            "--ultrafast and --no-ultrafast cannot be used together"
        );
    }

    #[test]
    fn requesting_ultra_mode_turns_fast_mode_off_and_only_it_reaches_acp() {
        let (ultra, remaining) = parse(&["--ultrafast", "acp"]).unwrap();
        assert!(ultra.has_model_overrides());
        assert!(ultra.has_only_ultrafast_override());
        assert_eq!(ultra.fast_mode(), Some(false));
        assert_eq!(remaining, vec![OsString::from("acp")]);

        let (fast_then_off, _) = parse(&["--fast", "--no-ultrafast"]).unwrap();
        assert_eq!(fast_then_off.fast_mode(), Some(true));
        assert!(!fast_then_off.has_only_ultrafast_override());

        for args in [&["--no-ultrafast"][..], &["--no-fast", "--ultrafast"]] {
            let (only, _) = parse(args).unwrap();
            assert!(only.has_only_ultrafast_override(), "{args:?}");
        }
        for args in [
            &["--no-fast"][..],
            &["--ultrafast", "--model", "m"],
            &["--ultrafast", "--provider", "codex"],
            &["--ultrafast", "--effort", "low"],
            &["--ultrafast", "--provider-order", "azure"],
            &["--ultrafast", "--no-provider-strict"],
        ] {
            let (mixed, _) = parse(args).unwrap();
            assert!(!mixed.has_only_ultrafast_override(), "{args:?}");
        }
    }

    #[test]
    fn additional_directory_flags_fail_closed_when_malformed() {
        let missing = GlobalLaunchError::MissingAddDirectoryValue.to_string();
        assert_eq!(error(&["--add-dir"]), missing);
        assert_eq!(error(&["--add-dir="]), missing);
        assert_eq!(error(&["--add-dir", ""]), missing);
        assert_eq!(
            error(&["--no-additional-dirs", "--no-additional-dirs"]),
            GlobalLaunchError::DuplicateAdditionalDirectorySuppression.to_string()
        );
    }

    #[test]
    fn global_launch_errors_use_user_facing_copy() {
        assert_eq!(
            GlobalLaunchError::MissingAddDirectoryValue.to_string(),
            "--add-dir requires a directory path"
        );
        assert_eq!(
            GlobalLaunchError::ContextLimit(ContextLimitError::UnknownContextLimit).to_string(),
            "invalid global launch option: UnknownContextLimit"
        );
        assert_eq!(
            error(&["--context-limit=wat=1"]),
            "invalid global launch option: UnknownContextLimit"
        );
    }

    #[test]
    fn sessions_v2_is_a_repeatable_launch_modifier_outside_the_workspace_and_model_groups() {
        let (modifiers, remaining) = parse(&[
            "--sessions-v2",
            "--add-dir",
            "/tmp",
            "--sessions-v2",
            "ask",
            "hi",
        ])
        .unwrap();
        assert!(modifiers.selects_sessions_v2());
        assert!(modifiers.adds_directories());
        assert!(!modifiers.has_model_overrides());
        assert_eq!(remaining, vec![OsString::from("ask"), OsString::from("hi")]);

        let (only, _) = parse(&["--sessions-v2"]).unwrap();
        assert!(only.selects_sessions_v2());
        assert!(!only.has_workspace_modifiers());
        assert!(!only.has_model_overrides());

        let (after, remaining) = parse(&["ask", "--sessions-v2"]).unwrap();
        assert!(!after.selects_sessions_v2());
        assert_eq!(remaining.len(), 2);

        let (joined, remaining) = parse(&["--sessions-v2=1", "ask"]).unwrap();
        assert!(!joined.selects_sessions_v2());
        assert_eq!(remaining[0], "--sessions-v2=1");
    }

    #[test]
    fn modifier_flags_with_unexpected_values_end_the_modifier_list() {
        let (modifiers, remaining) = parse(&["--no-additional-dirs=yes", "ask"]).unwrap();
        assert!(!modifiers.workspace.saved_directories_suppressed);
        assert_eq!(remaining[0], "--no-additional-dirs=yes");
    }
}
