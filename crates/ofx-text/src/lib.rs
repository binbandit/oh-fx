mod capability_retrieval;
mod display_width;
mod fmt;
mod language_script;
mod lexical_relevance;
mod model_context_encoding;
mod sorted_lines;
mod text_utils;
mod token_estimate;
mod unicode_display_data;
mod url_display;
mod utf8_validator;

pub use display_width::{
    DisplayUnit, display_unit_at, escape_ambiguous_width, next_tab_stop_column, prefix_by_width,
    should_wrap_at, starts_display_unit, status_prefix_end, suffix_by_width, trim_break_whitespace,
    visible_width, wrap_cut_ignoring_ansi,
};
pub use fmt::{lowercase_hex, parse_unsigned, shell_word};
pub use language_script::{Profile, ProseProfiles, Script, dominant_script, prose_profiles};
pub use model_context_encoding::write_scalar;
pub use text_utils::{
    EncodedText, HeadRounding, contains_ignore_case, encode_terminal_safe,
    encode_terminal_safe_inline, encode_terminal_safe_path_tail, escape_terminal_controls,
    is_model_safe_text, is_posix_space, is_terminal_control, is_terminal_safe,
    is_terminal_safe_char, mask_secrets, normalize_line_endings_in_place, sanitize_assistant_text,
    sanitize_model_text_owned, write_head_tail_bounded,
};
pub use token_estimate::StreamingEstimator;
pub use url_display::{clipped_label, redact_url_for_display};
pub use utf8_validator::{InvalidUtf8, Utf8Validator};

pub use capability_retrieval::{Document, Domain, Page, Request, retrieve};
pub use lexical_relevance::{PreparedQuery, QueryTooLong};
