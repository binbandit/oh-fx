mod assistant_presentation;
mod presentation;
mod styled;
#[cfg(test)]
mod test_support;

pub use assistant_presentation::{
    Completions, Event, MarkdownProcessor, parse_table_payload, render_code_block_payload,
    render_table_payload,
};
pub use presentation::block_render::table_header_cell;
pub use presentation::payload::{CodeBlockPayload, TableColumnAlign, TablePayload, TableRow};
pub use styled::{Attr, Hang, Hyperlink, Line, Slot, Span, Style};
