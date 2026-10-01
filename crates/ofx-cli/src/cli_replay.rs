use std::ffi::OsString;

use crate::cli_surface::{OutputFormat, Report, command_failure_json, requests_json};
use crate::command_specs::TopLevelKind;

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) enum ReplayArgsError {
    #[error(
        "oh-fx replay: missing tape path\nusage: oh-fx replay <tape> [--frames] [--json] [--golden <path>] [--frames-dir <path>]"
    )]
    MissingTapePath,
    #[error("oh-fx replay: too many positional arguments")]
    TooManyArgs,
    #[error("oh-fx replay: unknown flag")]
    UnknownFlag,
    #[error("oh-fx replay: --golden requires a path")]
    MissingGoldenPath,
    #[error("oh-fx replay: --frames-dir requires a path")]
    MissingFramesDirPath,
}

impl ReplayArgsError {
    fn name(self) -> &'static str {
        match self {
            Self::MissingTapePath => "MissingTapePath",
            Self::TooManyArgs => "TooManyArgs",
            Self::UnknownFlag => "UnknownFlag",
            Self::MissingGoldenPath => "MissingGoldenPath",
            Self::MissingFramesDirPath => "MissingFramesDirPath",
        }
    }
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("{kind}")]
pub struct ReplayError {
    pub(crate) kind: ReplayArgsError,
    pub(crate) json: bool,
}

impl ReplayError {
    pub(crate) fn report(self) -> Report {
        let message = self.kind.to_string();
        if self.json {
            Report::stdout(command_failure_json(
                TopLevelKind::Replay,
                &message,
                self.kind.name(),
            ))
        } else {
            Report::stderr(format!("{message}\n"))
        }
    }
}

pub(crate) fn parse_replay(args: Vec<OsString>) -> Result<OutputFormat, ReplayError> {
    let json_requested = requests_json(&args);
    let error = |kind| ReplayError {
        kind,
        json: json_requested,
    };
    let mut path = false;
    let mut json = false;
    let mut rest = args.into_iter();
    while let Some(arg) = rest.next() {
        if arg == "--frames" {
            continue;
        }
        if arg == "--json" {
            json = true;
        } else if arg == "--golden" {
            rest.next()
                .ok_or_else(|| error(ReplayArgsError::MissingGoldenPath))?;
        } else if arg == "--frames-dir" {
            rest.next()
                .ok_or_else(|| error(ReplayArgsError::MissingFramesDirPath))?;
        } else if arg.as_encoded_bytes().starts_with(b"--") {
            return Err(error(ReplayArgsError::UnknownFlag));
        } else if std::mem::replace(&mut path, true) {
            return Err(error(ReplayArgsError::TooManyArgs));
        }
    }
    if !path {
        return Err(error(ReplayArgsError::MissingTapePath));
    }
    Ok(if json {
        OutputFormat::Json
    } else {
        OutputFormat::Text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<OutputFormat, ReplayError> {
        parse_replay(args.iter().map(OsString::from).collect())
    }

    fn kind(args: &[&str]) -> ReplayArgsError {
        parse(args).unwrap_err().kind
    }

    #[test]
    fn parse_args_requires_a_positional_tape_path() {
        assert_eq!(kind(&[]), ReplayArgsError::MissingTapePath);
        assert_eq!(
            kind(&["--frames", "--json"]),
            ReplayArgsError::MissingTapePath
        );
        assert_eq!(parse(&["tape.fxtape"]).unwrap(), OutputFormat::Text);
    }

    #[test]
    fn parse_args_accepts_supported_flags() {
        let parsed = parse(&[
            "--frames",
            "t.fxtape",
            "--json",
            "--golden",
            "out.txt",
            "--frames-dir",
            "frames-out",
        ]);
        assert_eq!(parsed.unwrap(), OutputFormat::Json);
        assert_eq!(
            parse(&["t.fxtape", "--golden", "--json"]).unwrap(),
            OutputFormat::Text
        );
    }

    #[test]
    fn parse_args_rejects_invalid_forms() {
        assert_eq!(
            kind(&["a.fxtape", "b.fxtape"]),
            ReplayArgsError::TooManyArgs
        );
        assert_eq!(
            kind(&["a.fxtape", "--unknown"]),
            ReplayArgsError::UnknownFlag
        );
        assert_eq!(
            kind(&["a.fxtape", "--golden"]),
            ReplayArgsError::MissingGoldenPath
        );
        assert_eq!(
            kind(&["a.fxtape", "--frames-dir"]),
            ReplayArgsError::MissingFramesDirPath
        );
        assert_eq!(
            kind(&["--golden=out.txt", "a"]),
            ReplayArgsError::UnknownFlag
        );
    }

    #[test]
    fn run_missing_tape_path_returns_current_stderr() {
        let report = parse(&[]).unwrap_err().report();
        assert_eq!(report.stdout, "");
        assert!(
            report
                .stderr
                .starts_with("oh-fx replay: missing tape path\n")
        );
    }

    #[test]
    fn json_failures_use_stdout_for_missing_arguments() {
        let report = parse(&["--json"]).unwrap_err().report();
        assert_eq!(report.stderr, "");
        assert_eq!(
            report.stdout,
            "{\"kind\":\"replay\",\"error\":\"oh-fx replay: missing tape path\\nusage: oh-fx replay <tape> [--frames] [--json] [--golden <path>] [--frames-dir <path>]\",\"code\":\"MissingTapePath\"}\n"
        );
    }
}
