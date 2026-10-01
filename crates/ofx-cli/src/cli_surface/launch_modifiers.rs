use std::os::unix::ffi::OsStrExt;

use ofx_config::{ContextLimitError, is_valid_provider_id, validate_context_limit_override};

use super::arg_stream::{ArgStream, MissingValue, ValueForm};
use super::model_overrides::ModelOverrides;

#[derive(Debug, Default)]
pub struct LaunchModifiers {
    context_limits: bool,
    workspace: WorkspaceModifiers,
    model_overrides: bool,
}

#[derive(Debug, Default)]
struct WorkspaceModifiers {
    additional_directories: bool,
    saved_directories_suppressed: bool,
}

impl LaunchModifiers {
    pub fn sets_context_limits(&self) -> bool {
        self.context_limits
    }

    pub fn adds_directories(&self) -> bool {
        self.workspace.additional_directories
    }

    pub(crate) fn has_workspace_modifiers(&self) -> bool {
        self.workspace.additional_directories || self.workspace.saved_directories_suppressed
    }

    pub(crate) fn has_model_overrides(&self) -> bool {
        self.model_overrides
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
    Ok(modifiers)
}

impl LaunchModifiers {
    fn take_next(
        &mut self,
        args: &mut ArgStream,
        model_overrides: &mut ModelOverrides,
    ) -> Result<bool, GlobalLaunchError> {
        let joined = ValueForm::SeparateOrJoined;
        if let Some(value) = args.take_option("context-limit", joined) {
            let value =
                value.map_err(|MissingValue| GlobalLaunchError::MissingContextLimitValue)?;
            validate_context_limit_override(value.as_bytes())?;
            self.context_limits = true;
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
            self.model_overrides = true;
        } else if model_overrides.take(args, joined)?.is_some() {
            self.model_overrides = true;
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
        assert!(modifiers.sets_context_limits());
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
        assert!(!modifiers.sets_context_limits());
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
        ] {
            assert_eq!(error(args), expected.to_string(), "{args:?}");
        }
        assert!(parse(&["--fast", "--fast"]).is_ok());
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
    fn modifier_flags_with_unexpected_values_end_the_modifier_list() {
        let (modifiers, remaining) = parse(&["--no-additional-dirs=yes", "ask"]).unwrap();
        assert!(!modifiers.workspace.saved_directories_suppressed);
        assert_eq!(remaining[0], "--no-additional-dirs=yes");
    }
}
