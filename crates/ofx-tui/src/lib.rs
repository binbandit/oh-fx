#![cfg_attr(
    not(test),
    expect(
        dead_code,
        unused_imports,
        reason = "the inline shell that drives these modules lands later in this stack"
    )
)]

mod composer;
mod input;
mod terminal;

pub use terminal::TerminalError;
