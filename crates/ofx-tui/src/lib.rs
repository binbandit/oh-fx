#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the inline shell that drives these modules lands later in this stack"
    )
)]

mod input;
mod terminal;

pub use terminal::TerminalError;
