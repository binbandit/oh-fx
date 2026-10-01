mod session_adapter;
mod session_layout;

pub use session_adapter::{SESSIONS_V2_VARIABLE, sessions_v2_variable_is_on};
pub use session_layout::is_valid_session_id;
