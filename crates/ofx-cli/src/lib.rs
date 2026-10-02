mod cli_ask;
mod cli_replay;
mod cli_surface;
mod command_router;
mod command_specs;
mod commands;
mod registry;

pub use cli_ask::{AskArgs, AskError, AskOutput, read_stdin_prompt};
pub use cli_surface::{
    CliError, Command, CommandLaunch, HelpLayout, Invocation, LaunchModifiers, OutputFormat,
    command_failure_json, parse_args,
};
pub use command_router::SlashCommand;
pub use command_specs::{
    HelpStyle, SlashKind, SlashPresentationCategory, SlashSpec, TOP_LEVEL_HELP_DEFAULT_WIDTH,
    TopLevelKind, parse_column_count, render_command_help, render_top_level_help,
};
pub use commands::SLASH_REGISTRY;
pub use registry::SlashRegistry;
