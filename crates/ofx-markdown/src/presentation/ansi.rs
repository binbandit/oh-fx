use crate::styled::{Attr, SpanWriter};

pub(crate) const TABLE_COLUMN_SEP: &str = " │ ";
pub(crate) const TABLE_HORIZ: &str = "─";
pub(crate) const TABLE_JUNCTION: &str = "─┼─";
pub(crate) const VERTICAL_RULE_PREFIX: &str = "│ ";
pub(crate) const BULLET_MARKER: &str = "• ";
pub(crate) const TASK_PENDING_MARKER: &str = "☐";
pub(crate) const TASK_COMPLETED_MARKER: &str = "✓";

pub(crate) const MAX_PIPE_BUFFER_BYTES: usize = 32 * 1024;
pub(crate) const MAX_TABLE_CELLS_PER_SOURCE_BYTE: usize = 16;
pub(crate) const HORIZONTAL_RULE_WIDTH: usize = 60;
pub(crate) const MAX_LINK_URL_BYTES: usize = 2083;

pub(crate) fn write_dim(out: &mut SpanWriter, text: &str) {
    out.open(Attr::Dim);
    out.text(text);
    out.close(Attr::Dim);
}

pub(crate) fn write_horizontal_rule(out: &mut SpanWriter) {
    out.open(Attr::Dim);
    out.repeat(TABLE_HORIZ, HORIZONTAL_RULE_WIDTH);
    out.close(Attr::Dim);
}
