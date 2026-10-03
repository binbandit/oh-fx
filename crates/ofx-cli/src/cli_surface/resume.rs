use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;

use ofx_upgrade::is_valid_revision;

use super::arg_stream::non_blank;
use crate::command_specs::TopLevelKind;

pub const UPGRADE_RELAUNCH_ARG: &str = "--upgrade-relaunch";
pub(crate) const RESUME_ID_ALIAS_PREFIX: &str = "--resume-";

const RESUME_PICKER_ALIAS: &str = "-r";
const LAST_TARGET: &str = "last";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestedResume {
    Remembered,
    Pick,
    Last,
    Id(String),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InvalidResumeArgs;

pub(crate) fn resume_alias_target(
    alias: &OsStr,
    rest: &[OsString],
) -> Result<RequestedResume, InvalidResumeArgs> {
    if alias == "--resume" {
        return match rest {
            [] => Ok(RequestedResume::Last),
            [operand] => operand_target(operand, false),
            _ => Err(InvalidResumeArgs),
        };
    }
    if !rest.is_empty() {
        return Err(InvalidResumeArgs);
    }
    if alias == RESUME_PICKER_ALIAS {
        return Ok(RequestedResume::Pick);
    }
    if alias == "-c" || alias == "--continue" {
        return Ok(RequestedResume::Remembered);
    }
    if alias
        .to_str()
        .is_some_and(|alias| TopLevelKind::Resume.spec().matches(alias))
    {
        return Ok(RequestedResume::Last);
    }
    match alias
        .as_bytes()
        .strip_prefix(RESUME_ID_ALIAS_PREFIX.as_bytes())
    {
        Some(id) if !id.is_empty() => Ok(RequestedResume::Id(
            String::from_utf8_lossy(id).into_owned(),
        )),
        _ => Err(InvalidResumeArgs),
    }
}

pub(crate) fn resume_subcommand_target(
    args: &[OsString],
) -> Result<RequestedResume, InvalidResumeArgs> {
    if args.get(1).is_none_or(|arg| arg != UPGRADE_RELAUNCH_ARG) {
        return subcommand_target(args);
    }
    match &args[2..] {
        [] => {}
        [revision] if revision.to_str().is_some_and(is_valid_revision) => {}
        _ => return Err(InvalidResumeArgs),
    }
    subcommand_target(&args[..1])
}

fn subcommand_target(args: &[OsString]) -> Result<RequestedResume, InvalidResumeArgs> {
    let Some(first) = args.first() else {
        return Ok(RequestedResume::Last);
    };
    if first == "--resume" {
        return match &args[1..] {
            [last] if last == "--last" => Ok(RequestedResume::Last),
            _ => Err(InvalidResumeArgs),
        };
    }
    let exact = first == "--id";
    let operands = if exact { &args[1..] } else { args };
    let [operand] = operands else {
        return Err(InvalidResumeArgs);
    };
    operand_target(operand, exact)
}

fn operand_target(raw: &OsStr, exact: bool) -> Result<RequestedResume, InvalidResumeArgs> {
    let id = non_blank(raw).ok_or(InvalidResumeArgs)?;
    if !exact && id == LAST_TARGET {
        return Ok(RequestedResume::Last);
    }
    Ok(RequestedResume::Id(id.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn subcommand(args: &[&str]) -> bool {
        resume_subcommand_target(&os(args)).is_ok()
    }

    fn alias(args: &[&str]) -> bool {
        let args = os(args);
        resume_alias_target(&args[0], &args[1..]).is_ok()
    }

    fn subcommand_target_of(args: &[&str]) -> RequestedResume {
        resume_subcommand_target(&os(args)).unwrap()
    }

    fn alias_target_of(args: &[&str]) -> RequestedResume {
        let args = os(args);
        resume_alias_target(&args[0], &args[1..]).unwrap()
    }

    fn id(value: &str) -> RequestedResume {
        RequestedResume::Id(value.to_owned())
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
        let replaced = id("\u{fffd}");
        assert_eq!(
            resume_alias_target(OsStr::new("--resume"), std::slice::from_ref(&raw)).unwrap(),
            replaced
        );
        assert_eq!(
            resume_alias_target(&OsString::from_vec(b"--resume-\xff".to_vec()), &[]).unwrap(),
            replaced
        );
        assert_eq!(
            resume_subcommand_target(std::slice::from_ref(&raw)).unwrap(),
            replaced
        );
        assert_eq!(
            resume_subcommand_target(&[OsString::from("--id"), raw]).unwrap(),
            replaced
        );
    }

    #[test]
    fn each_spelling_names_the_session_upstream_resumes() {
        assert_eq!(subcommand_target_of(&[]), RequestedResume::Last);
        assert_eq!(subcommand_target_of(&[" last "]), RequestedResume::Last);
        assert_eq!(subcommand_target_of(&[" session-123 "]), id("session-123"));
        assert_eq!(subcommand_target_of(&["--id", "last"]), id("last"));
        assert_eq!(subcommand_target_of(&["--id", " a "]), id("a"));
        assert_eq!(
            subcommand_target_of(&["--resume", "--last"]),
            RequestedResume::Last
        );
        assert_eq!(
            subcommand_target_of(&["abc", UPGRADE_RELAUNCH_ARG]),
            id("abc")
        );
        assert_eq!(alias_target_of(&["-r"]), RequestedResume::Pick);
        assert_eq!(alias_target_of(&["-c"]), RequestedResume::Remembered);
        assert_eq!(
            alias_target_of(&["--continue"]),
            RequestedResume::Remembered
        );
        assert_eq!(alias_target_of(&["-c "]), RequestedResume::Last);
        assert_eq!(alias_target_of(&["--resume-last"]), RequestedResume::Last);
        assert_eq!(alias_target_of(&["--resume"]), RequestedResume::Last);
        assert_eq!(
            alias_target_of(&["--resume", "last"]),
            RequestedResume::Last
        );
        assert_eq!(alias_target_of(&["--resume", " id-1 "]), id("id-1"));
        assert_eq!(alias_target_of(&["--resume-abc"]), id("abc"));
        assert_eq!(alias_target_of(&["--resume- abc"]), id(" abc"));
    }
}
