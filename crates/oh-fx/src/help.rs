use std::env;
use std::io::{self, IsTerminal};

use ofx_cli::{
    HelpLayout, HelpStyle, TOP_LEVEL_HELP_DEFAULT_WIDTH, parse_column_count, render_top_level_help,
};

pub(crate) fn top_level(layout: HelpLayout) -> String {
    if matches!(layout, HelpLayout::Plain) {
        return render_top_level_help(
            TOP_LEVEL_HELP_DEFAULT_WIDTH,
            ofx_upgrade::VERSION,
            HelpStyle::Plain,
        );
    }
    let stdout = io::stdout();
    let columns = rustix::termios::tcgetwinsize(&stdout)
        .ok()
        .map(|size| usize::from(size.ws_col))
        .filter(|columns| *columns != 0)
        .or_else(|| {
            env::var("COLUMNS")
                .ok()
                .and_then(|value| parse_column_count(&value))
        })
        .unwrap_or(TOP_LEVEL_HELP_DEFAULT_WIDTH);
    let style = HelpStyle::for_terminal(
        stdout.is_terminal(),
        env::var_os("NO_COLOR").is_some(),
        env::var_os("TERM").is_some_and(|term| term == "dumb"),
    );
    render_top_level_help(columns, ofx_upgrade::VERSION, style)
}
