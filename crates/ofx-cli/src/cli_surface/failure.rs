use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;

use ofx_text::encode_terminal_safe;

use crate::command_specs::{
    HelpStyle, PRODUCT_NAME, TOP_LEVEL_HELP_DEFAULT_WIDTH, render_top_level_help,
};

const ECHOED_ARGUMENT_BYTES: usize = 160;

#[derive(Debug, thiserror::Error)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub enum CliError {
    #[error("unknown subcommand: {}", terminal_safe(.0))]
    UnknownSubcommand(OsString),
    #[error("usage: oh-fx --version")]
    VersionUsage,
}

impl CliError {
    pub fn report(&self, version: &str) -> String {
        match self {
            Self::UnknownSubcommand(_) => format!(
                "{PRODUCT_NAME}: {self}\n\n{}",
                render_top_level_help(TOP_LEVEL_HELP_DEFAULT_WIDTH, version, HelpStyle::Plain)
            ),
            Self::VersionUsage => format!("{self}\n"),
        }
    }
}

fn terminal_safe(argument: &OsStr) -> String {
    encode_terminal_safe(argument.as_bytes(), ECHOED_ARGUMENT_BYTES).text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_subcommands_echo_a_terminal_safe_bounded_token() {
        let echoed = |raw: &[u8]| {
            CliError::UnknownSubcommand(OsStr::from_bytes(raw).to_os_string()).to_string()
        };
        assert_eq!(echoed(b"wat"), "unknown subcommand: wat");
        assert_eq!(
            echoed(b"\x1b]0;pwned\x07"),
            "unknown subcommand: \\x1b]0;pwned\\x07"
        );
        assert_eq!(echoed(b"b\xffd"), "unknown subcommand: b\\xffd");
        assert_eq!(
            echoed("a\u{202e}b".as_bytes()),
            "unknown subcommand: a\\u{202e}b"
        );
        let long = echoed(&[b'x'; 200]);
        assert_eq!(
            long,
            format!(
                "unknown subcommand: {}...",
                "x".repeat(ECHOED_ARGUMENT_BYTES - 3)
            )
        );
    }
}
