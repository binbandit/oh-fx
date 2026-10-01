use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;

use ofx_upgrade::is_valid_revision;

use super::arg_stream::non_blank;
use crate::command_specs::TopLevelKind;

pub(crate) const UPGRADE_RELAUNCH_ARG: &str = "--upgrade-relaunch";
pub(crate) const RESUME_ID_ALIAS_PREFIX: &str = "--resume-";

#[derive(Debug, Clone, Copy)]
pub(crate) struct InvalidResumeArgs;

pub(crate) fn validate_resume_alias(
    alias: &OsStr,
    rest: &[OsString],
) -> Result<(), InvalidResumeArgs> {
    if alias == "--resume" {
        return match rest {
            [] => Ok(()),
            [operand] => validate_id(operand),
            _ => Err(InvalidResumeArgs),
        };
    }
    if !rest.is_empty() {
        return Err(InvalidResumeArgs);
    }
    if alias
        .to_str()
        .is_some_and(|alias| TopLevelKind::Resume.spec().matches(alias))
    {
        return Ok(());
    }
    match alias
        .as_bytes()
        .strip_prefix(RESUME_ID_ALIAS_PREFIX.as_bytes())
    {
        Some(id) if !id.is_empty() => Ok(()),
        _ => Err(InvalidResumeArgs),
    }
}

pub(crate) fn validate_resume_subcommand(args: &[OsString]) -> Result<(), InvalidResumeArgs> {
    if args.get(1).is_none_or(|arg| arg != UPGRADE_RELAUNCH_ARG) {
        return validate_subcommand_target(args);
    }
    match &args[2..] {
        [] => {}
        [revision] if revision.to_str().is_some_and(is_valid_revision) => {}
        _ => return Err(InvalidResumeArgs),
    }
    validate_subcommand_target(&args[..1])
}

fn validate_subcommand_target(args: &[OsString]) -> Result<(), InvalidResumeArgs> {
    let Some(first) = args.first() else {
        return Ok(());
    };
    if first == "--resume" {
        return match &args[1..] {
            [last] if last == "--last" => Ok(()),
            _ => Err(InvalidResumeArgs),
        };
    }
    let operands = if first == "--id" { &args[1..] } else { args };
    let [operand] = operands else {
        return Err(InvalidResumeArgs);
    };
    validate_id(operand)
}

fn validate_id(raw: &OsStr) -> Result<(), InvalidResumeArgs> {
    non_blank(raw).map(drop).ok_or(InvalidResumeArgs)
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn subcommand(args: &[&str]) -> bool {
        validate_resume_subcommand(&os(args)).is_ok()
    }

    fn alias(args: &[&str]) -> bool {
        let args = os(args);
        validate_resume_alias(&args[0], &args[1..]).is_ok()
    }

    #[test]
    fn parse_resume_args_defaults_to_last_owns_ids_and_rejects_invalid_input() {
        assert!(subcommand(&[]));
        assert!(subcommand(&["last"]));
        assert!(subcommand(&[" session-123 "]));
        assert!(!subcommand(&["a", "b"]));
        assert!(!subcommand(&["   "]));
    }

    #[test]
    fn parse_resume_args_accepts_explicit_id_flag() {
        assert!(subcommand(&["--id", "release.2026.06"]));
        assert!(subcommand(&["--id", "last"]));
        assert!(!subcommand(&["--id"]));
        assert!(!subcommand(&["--id", " "]));
    }

    #[test]
    fn parse_resume_args_accepts_an_operand_on_the_top_level_resume_flag() {
        assert!(alias(&["--resume", "session-123"]));
        assert!(alias(&["--resume", "last"]));
        assert!(alias(&["--resume"]));
        assert!(!alias(&["--resume", "one", "two"]));
    }

    #[test]
    fn top_level_resume_aliases_return_their_targets() {
        for args in [
            &["-r"][..],
            &["-c"],
            &["--continue"],
            &["--resume-last"],
            &["--resume-abc"],
            &["-c "],
        ] {
            assert!(alias(args), "{args:?}");
        }
    }

    #[test]
    fn malformed_top_level_resume_aliases_are_rejected() {
        for args in [
            &["--resume-"][..],
            &["-c", "x"],
            &["--resume-last", "x"],
            &["--resume", " "],
            &["-r", "session.123"],
        ] {
            assert!(!alias(args), "{args:?}");
        }
    }

    #[test]
    fn legacy_resume_flag_requires_the_last_marker() {
        assert!(subcommand(&["--resume", "--last"]));
        assert!(!subcommand(&["--resume"]));
    }

    #[test]
    fn upgrade_relaunches_accept_only_a_valid_previous_revision() {
        let revision = "abcdef0123456789abcdef0123456789abcdef01";
        assert!(subcommand(&["session-123", UPGRADE_RELAUNCH_ARG]));
        assert!(subcommand(&["session-123", UPGRADE_RELAUNCH_ARG, revision]));
        assert!(!subcommand(&[
            "session-123",
            UPGRADE_RELAUNCH_ARG,
            "not-a-revision"
        ]));
        assert!(!subcommand(&["a", UPGRADE_RELAUNCH_ARG, revision, "extra"]));
        assert!(!subcommand(&[" ", UPGRADE_RELAUNCH_ARG]));
    }

    #[test]
    fn non_utf8_resume_targets_are_accepted_as_ids() {
        let raw = OsString::from_vec(b"\xff".to_vec());
        assert!(validate_resume_alias(OsStr::new("--resume"), std::slice::from_ref(&raw)).is_ok());
        assert!(validate_resume_alias(&OsString::from_vec(b"--resume-\xff".to_vec()), &[]).is_ok());
        assert!(validate_resume_subcommand(std::slice::from_ref(&raw)).is_ok());
        assert!(validate_resume_subcommand(&[OsString::from("--id"), raw]).is_ok());
    }
}
