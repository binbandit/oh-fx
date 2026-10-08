use std::ffi::OsString;

use crate::cli_surface::{Report, command_failure_json, requests_json};
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplayArgs {
    pub path: OsString,
    pub frames: bool,
    pub json: bool,
    pub golden: Option<OsString>,
    pub frames_dir: Option<OsString>,
}

pub(crate) fn parse_replay(args: Vec<OsString>) -> Result<ReplayArgs, ReplayError> {
    let json_requested = requests_json(&args);
    let error = |kind| ReplayError {
        kind,
        json: json_requested,
    };
    let mut parsed = ReplayArgs::default();
    let mut path = None;
    let mut rest = args.into_iter();
    while let Some(arg) = rest.next() {
        if arg == "--frames" {
            parsed.frames = true;
        } else if arg == "--json" {
            parsed.json = true;
        } else if arg == "--golden" {
            parsed.golden = Some(
                rest.next()
                    .ok_or_else(|| error(ReplayArgsError::MissingGoldenPath))?,
            );
        } else if arg == "--frames-dir" {
            parsed.frames_dir = Some(
                rest.next()
                    .ok_or_else(|| error(ReplayArgsError::MissingFramesDirPath))?,
            );
        } else if arg.as_encoded_bytes().starts_with(b"--") {
            return Err(error(ReplayArgsError::UnknownFlag));
        } else if path.replace(arg).is_some() {
            return Err(error(ReplayArgsError::TooManyArgs));
        }
    }
    parsed.path = path.ok_or_else(|| error(ReplayArgsError::MissingTapePath))?;
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<ReplayArgs, ReplayError> {
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
        let parsed = parse(&["tape.fxtape"]).unwrap();
        assert_eq!(parsed.path, "tape.fxtape");
        assert!(!parsed.frames && !parsed.json);
        assert_eq!((parsed.golden, parsed.frames_dir), (None, None));
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
        assert_eq!(
            parsed.unwrap(),
            ReplayArgs {
                path: "t.fxtape".into(),
                frames: true,
                json: true,
                golden: Some("out.txt".into()),
                frames_dir: Some("frames-out".into()),
            }
        );
        let golden_json = parse(&["t.fxtape", "--golden", "--json"]).unwrap();
        assert!(!golden_json.json);
        assert_eq!(golden_json.golden, Some("--json".into()));
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
