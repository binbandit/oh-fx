use rustix::io::Errno;

pub(crate) const MCP: &str = "mcp";
pub(crate) const TIMEOUT: &str = "Timeout";
pub(crate) const NONE: &str = "none";
const NAME_PREVIEW_BYTES: usize = 256;
const VARIABLE_PREVIEW_BYTES: usize = 160;

pub(crate) fn server_name_preview(name: &str) -> String {
    ofx_trace::terminal_preview(name, NAME_PREVIEW_BYTES)
}

pub(crate) fn variable_preview(name: &str) -> String {
    ofx_trace::terminal_preview(name, VARIABLE_PREVIEW_BYTES)
}

pub(crate) fn kill_error_name(errno: Errno) -> &'static str {
    match errno {
        Errno::PERM => "PermissionDenied",
        Errno::SRCH => "ProcessNotFound",
        _ => "Unexpected",
    }
}
