#![cfg_attr(
    not(test),
    expect(
        dead_code,
        unused_imports,
        reason = "the inline shell that drives these modules lands later in this stack"
    )
)]

mod assistant;
mod composer;
mod input;
mod output;
mod render;
mod render_engine;
mod row_text;
mod terminal;
mod theme;
mod transcript;

pub use terminal::TerminalError;
