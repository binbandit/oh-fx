use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Read};

use ofx_contract::{PermissionMode, ReasoningEffort};
use ofx_text::parse_unsigned;

use crate::cli_surface::{
    ArgStream, ModelOverride, ModelOverrides, Report, ValueForm, non_blank, requests_json,
};
use crate::command_specs::{PRODUCT_NAME, TopLevelKind};

const ASK_STDIN_PROMPT_LIMIT_BYTES: usize = 8 * 1024 * 1024;

const MILLISECONDS_PER_SECOND: u64 = 1000;

#[derive(Debug)]
enum AskPrompt {
    Text(String),
    Stdin,
}

#[derive(Debug)]
pub enum StdinPrompt {
    Terminal,
    Bytes(Vec<u8>),
    ReadFailed,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AskOutput {
    pub json: bool,
    pub quiet: bool,
    pub no_color: bool,
}

#[derive(Debug, Default)]
pub struct AskPermissions {
    pub mode: Option<PermissionMode>,
    pub prompt: bool,
}

#[derive(Debug, Default)]
pub struct AskSession {
    pub resume_flag: Option<&'static str>,
    pub continue_recovery: bool,
    pub no_save: bool,
    pub sessions_v2: bool,
}

#[derive(Debug)]
pub struct AskArgs {
    prompt: AskPrompt,
    pub permissions: AskPermissions,
    pub model: Option<OsString>,
    pub effort: Option<ReasoningEffort>,
    pub fast: Option<bool>,
    pub system_prompt: Option<String>,
    pub output: AskOutput,
    pub session: AskSession,
    pub images: bool,
    pub timeout: bool,
    json_errors: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AskErrorKind {
    #[error("missing prompt")]
    MissingPrompt,
    #[error("--no-save cannot be used with --resume or --resume-id")]
    NoSaveResumeConflict,
    #[error("prompt exceeds the local input safety limit")]
    PromptResourceLimitExceeded,
    #[error("failed to read prompt from stdin")]
    PromptInputReadFailed,
    #[error("invalid arguments")]
    InvalidAskArgs,
    #[error("prompt must be valid UTF-8 and contain no NUL bytes")]
    InvalidPromptText,
}

impl AskErrorKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::MissingPrompt => "MissingPrompt",
            Self::NoSaveResumeConflict | Self::InvalidAskArgs => "InvalidAskArgs",
            Self::PromptResourceLimitExceeded => "PromptResourceLimitExceeded",
            Self::PromptInputReadFailed => "PromptInputReadFailed",
            Self::InvalidPromptText => "InvalidPromptText",
        }
    }

    fn shows_usage(self) -> bool {
        matches!(
            self,
            Self::MissingPrompt | Self::NoSaveResumeConflict | Self::InvalidAskArgs
        )
    }
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("{kind}")]
pub struct AskError {
    pub kind: AskErrorKind,
    pub json: bool,
}

impl AskError {
    pub fn report(&self) -> Report {
        let mut stderr = String::new();
        if self.kind != AskErrorKind::InvalidAskArgs {
            stderr = format!("{PRODUCT_NAME} ask: {}\n", self.kind);
        }
        if self.kind.shows_usage() {
            let _ = writeln!(
                stderr,
                "usage: {PRODUCT_NAME} {}",
                TopLevelKind::Ask.spec().usage
            );
        }
        Report::stderr(stderr)
    }
}

impl AskArgs {
    pub fn resolve_prompt(
        &self,
        read_stdin: impl FnOnce() -> StdinPrompt,
    ) -> Result<String, AskError> {
        let input = match &self.prompt {
            AskPrompt::Text(text) => return Ok(text.clone()),
            AskPrompt::Stdin => read_stdin(),
        };
        let bytes = match input {
            StdinPrompt::Terminal => return Err(self.error(AskErrorKind::MissingPrompt)),
            StdinPrompt::ReadFailed => return Err(self.error(AskErrorKind::PromptInputReadFailed)),
            StdinPrompt::Bytes(bytes) if bytes.len() > ASK_STDIN_PROMPT_LIMIT_BYTES => {
                return Err(self.error(AskErrorKind::PromptResourceLimitExceeded));
            }
            StdinPrompt::Bytes(bytes) => bytes,
        };
        let trimmed = trim_prompt_bytes(&bytes);
        if trimmed.is_empty() {
            return Err(self.error(AskErrorKind::MissingPrompt));
        }
        let text =
            model_safe_text(trimmed).ok_or_else(|| self.error(AskErrorKind::InvalidPromptText))?;
        self.check_no_save_conflict()?;
        Ok(text)
    }

    fn error(&self, kind: AskErrorKind) -> AskError {
        AskError {
            kind,
            json: self.json_errors,
        }
    }

    fn check_no_save_conflict(&self) -> Result<(), AskError> {
        if self.session.no_save && self.session.resume_flag.is_some() {
            Err(self.error(AskErrorKind::NoSaveResumeConflict))
        } else {
            Ok(())
        }
    }
}

pub fn read_stdin_prompt() -> StdinPrompt {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return StdinPrompt::Terminal;
    }
    let mut bytes = Vec::new();
    let detection_limit = ASK_STDIN_PROMPT_LIMIT_BYTES.saturating_add(1);
    let read = stdin
        .lock()
        .take(u64::try_from(detection_limit).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes);
    match read {
        Ok(_) => StdinPrompt::Bytes(bytes),
        Err(_) => StdinPrompt::ReadFailed,
    }
}

fn trim_prompt_bytes(bytes: &[u8]) -> &[u8] {
    let is_trimmed = |byte: &u8| matches!(byte, b' ' | b'\t' | b'\r' | b'\n');
    let start = bytes
        .iter()
        .position(|byte| !is_trimmed(byte))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !is_trimmed(byte))
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

fn model_safe_text(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    (!text.contains('\0')).then(|| text.to_owned())
}

pub(crate) fn parse_ask(args: Vec<OsString>) -> Result<AskArgs, AskError> {
    let json_errors = args
        .split(|arg| arg == "--")
        .next()
        .is_some_and(requests_json);
    let mut parser = AskParser {
        args: AskArgs {
            prompt: AskPrompt::Stdin,
            permissions: AskPermissions::default(),
            model: None,
            effort: None,
            fast: None,
            system_prompt: None,
            output: AskOutput::default(),
            session: AskSession::default(),
            images: false,
            timeout: false,
            json_errors,
        },
        model_overrides: ModelOverrides::default(),
        prompt_parts: Vec::new(),
        stream: ArgStream::new(args),
    };
    parser.parse_options()?;
    parser.finish()
}

struct AskParser {
    args: AskArgs,
    model_overrides: ModelOverrides,
    prompt_parts: Vec<OsString>,
    stream: ArgStream,
}

impl AskParser {
    fn error(&self, kind: AskErrorKind) -> AskError {
        self.args.error(kind)
    }

    fn parse_options(&mut self) -> Result<(), AskError> {
        loop {
            if self.stream.take_flag("--") {
                self.prompt_parts.extend(self.stream.by_ref());
            } else if !self.take_option()? {
                let Some(arg) = self.stream.next() else {
                    return Ok(());
                };
                if arg.len() > 1 && arg.as_encoded_bytes().starts_with(b"-") {
                    return Err(self.error(AskErrorKind::InvalidAskArgs));
                }
                self.prompt_parts.push(arg);
            }
        }
    }

    fn take_option(&mut self) -> Result<bool, AskError> {
        let invalid = self.error(AskErrorKind::InvalidAskArgs);
        let missing = self.error(AskErrorKind::MissingPrompt);
        match self
            .model_overrides
            .take(&mut self.stream, ValueForm::Separate)
        {
            Ok(Some(ModelOverride::Model(model))) => {
                self.args.model = Some(model);
                return Ok(true);
            }
            Ok(Some(ModelOverride::Setting)) => return Ok(true),
            Ok(None) => {}
            Err(_) => return Err(invalid),
        }
        if let Some(permission) = self.take_permission() {
            if self.args.permissions.mode.replace(permission).is_some() {
                return Err(invalid);
            }
        } else if let Some((flag, value)) = self.take_resume() {
            if self.args.session.resume_flag.replace(flag).is_some() {
                return Err(invalid);
            }
            value.as_deref().and_then(non_blank).ok_or(invalid)?;
        } else if let Some(value) = self.stream.take_option("image", ValueForm::Separate) {
            value.map_err(|_| missing)?;
            self.args.images = true;
        } else if let Some(value) = self.stream.take_option("system", ValueForm::Separate) {
            let value = value.map_err(|_| missing)?;
            self.args.system_prompt = Some(value.into_string().map_err(|_| invalid)?);
        } else if let Some(value) = self.stream.take_option("timeout", ValueForm::Separate) {
            self.args.timeout = is_valid_timeout(&value.map_err(|_| missing)?);
        } else if self.stream.take_flag("--continue-recovery") {
            if self.args.session.continue_recovery {
                return Err(invalid);
            }
            self.args.session.continue_recovery = true;
        } else {
            return Ok(self.take_switch());
        }
        Ok(true)
    }

    fn take_permission(&mut self) -> Option<PermissionMode> {
        if self.stream.take_flag("--auto") {
            Some(PermissionMode::Auto)
        } else if self.stream.take_flag("--full-access") || self.stream.take_flag("--yolo") {
            Some(PermissionMode::Yolo)
        } else {
            None
        }
    }

    fn take_resume(&mut self) -> Option<(&'static str, Option<OsString>)> {
        ["--resume", "--resume-id"].into_iter().find_map(|flag| {
            self.stream
                .take_option(&flag[2..], ValueForm::Separate)
                .map(|value| (flag, value.ok()))
        })
    }

    fn take_switch(&mut self) -> bool {
        let (stream, args) = (&mut self.stream, &mut self.args);
        let switch = if stream.take_flag("--json") {
            &mut args.output.json
        } else if stream.take_flag("--prompt-permissions") {
            &mut args.permissions.prompt
        } else if stream.take_flag("--quiet") {
            &mut args.output.quiet
        } else if stream.take_flag("--no-save") {
            &mut args.session.no_save
        } else if stream.take_flag("--sessions-v2") {
            &mut args.session.sessions_v2
        } else if stream.take_flag("--no-color") {
            &mut args.output.no_color
        } else {
            return stream.take_flag("--verbose");
        };
        *switch = true;
        true
    }

    fn finish(mut self) -> Result<AskArgs, AskError> {
        self.args.effort = self.model_overrides.effort.take();
        self.args.fast = self.model_overrides.fast;
        if self.args.session.continue_recovery {
            let session = &self.args.session;
            if session.resume_flag.is_none()
                || session.no_save
                || !self.prompt_parts.is_empty()
                || self.args.images
            {
                return Err(self.error(AskErrorKind::InvalidAskArgs));
            }
            self.args.prompt = AskPrompt::Text(String::new());
        } else if !self.prompt_parts.is_empty() {
            let text = join_prompt(&self.prompt_parts)
                .ok_or_else(|| self.error(AskErrorKind::InvalidPromptText))?;
            self.args.prompt = AskPrompt::Text(text);
        } else {
            return Ok(self.args);
        }
        self.args.check_no_save_conflict()?;
        Ok(self.args)
    }
}

fn join_prompt(parts: &[OsString]) -> Option<String> {
    let words = parts
        .iter()
        .map(|part| part.to_str())
        .collect::<Option<Vec<&str>>>()?;
    let text = words.join(" ");
    (!text.contains('\0')).then_some(text)
}

fn is_valid_timeout(raw: &OsStr) -> bool {
    let Some(raw) = raw.to_str() else {
        return false;
    };
    let seconds: Option<u64> = match raw.strip_prefix('-') {
        Some(digits) => parse_unsigned(digits).filter(|value| *value == 0),
        None => parse_unsigned(raw.strip_prefix('+').unwrap_or(raw)),
    };
    seconds.is_some_and(|seconds| seconds.checked_mul(MILLISECONDS_PER_SECOND).is_some())
}

#[cfg(test)]
mod tests;
