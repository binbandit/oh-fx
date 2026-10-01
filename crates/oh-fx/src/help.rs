use std::env;

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
    let window = rustix::termios::tcgetwinsize(rustix::stdio::stdout()).ok();
    let columns = window
        .map(|size| usize::from(size.ws_col))
        .filter(|columns| *columns != 0)
        .or_else(|| {
            env::var("COLUMNS")
                .ok()
                .and_then(|value| parse_column_count(&value))
        })
        .unwrap_or(TOP_LEVEL_HELP_DEFAULT_WIDTH);
    let is_terminal = window.is_some();
    let style = HelpStyle::for_terminal(
        is_terminal,
        is_terminal && env::var_os("NO_COLOR").is_some(),
        is_terminal && env::var_os("TERM").is_some_and(|term| term == "dumb"),
    );
    render_top_level_help(columns, ofx_upgrade::VERSION, style)
}
