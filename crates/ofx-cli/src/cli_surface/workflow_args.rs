use std::ffi::OsString;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowArgs {
    pub auto: bool,
    pub create: bool,
    pub context: OsString,
}

pub(crate) fn parse_workflow_args(args: Vec<OsString>) -> WorkflowArgs {
    let mut parsed = WorkflowArgs::default();
    let mut args = args.into_iter().peekable();
    while let Some(flag) = args.next_if(|arg| arg == "--auto" || arg == "--create") {
        if flag == "--auto" {
            parsed.auto = true;
        } else {
            parsed.create = true;
        }
    }
    for (index, arg) in args.enumerate() {
        if index > 0 {
            parsed.context.push(" ");
        }
        parsed.context.push(arg);
    }
    parsed
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    fn parse(args: &[&str]) -> WorkflowArgs {
        parse_workflow_args(args.iter().map(OsString::from).collect())
    }

    #[test]
    fn leading_flags_are_consumed_and_the_rest_is_joined_into_the_context() {
        assert_eq!(
            parse(&["--auto", "--create", "ready", "for", "review"]),
            WorkflowArgs {
                auto: true,
                create: true,
                context: OsString::from("ready for review"),
            }
        );
        assert_eq!(
            parse(&["context", "--auto"]),
            WorkflowArgs {
                auto: false,
                create: false,
                context: OsString::from("context --auto"),
            }
        );
        assert_eq!(parse(&[]), WorkflowArgs::default());
    }

    #[test]
    fn flags_repeat_in_any_order_and_anything_else_is_context() {
        assert_eq!(
            parse(&["--create", "--auto", "--create", "--json", "  spaced  ", ""]),
            WorkflowArgs {
                auto: true,
                create: true,
                context: OsString::from("--json   spaced   "),
            }
        );
        let raw = parse_workflow_args(vec![
            OsString::from("--auto"),
            OsString::from_vec(b"bad\xffbyte".to_vec()),
        ]);
        assert!(raw.auto);
        assert_eq!(raw.context, OsString::from_vec(b"bad\xffbyte".to_vec()));
    }
}
