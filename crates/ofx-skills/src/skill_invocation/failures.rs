use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ofx_config::ContextLimit;
use ofx_contract::prepare_model_output;
use ofx_text::sanitize_model_text_owned;

use crate::encoded_scalar::{bounded_encoded_prefix, encoded_bytes, encoded_scalar};
use crate::skill_contract::{ExecuteOutput, MAX_NAME_BYTES, Skill};

pub(crate) const DISCOVERY_MODEL_NOTICE: &str =
    "<skill_discovery_warning details=\"context_notice\" />\n";
const IDENTITY_PREFIX: &str = "Skill \"";
const CHUNK_OVERRIDE: &str = "--context-limit skill_chunk_bytes=BYTES|off";
const FILE_OVERRIDE: &str = "--context-limit skill_file_bytes=BYTES|off";
const ELLIPSIS: &str = "...";

pub(crate) fn execute_primary_budget(max_tool_result_bytes: usize, include_notice: bool) -> usize {
    if include_notice {
        max_tool_result_bytes - DISCOVERY_MODEL_NOTICE.len().min(max_tool_result_bytes)
    } else {
        max_tool_result_bytes
    }
}

pub(crate) fn attach_discovery_notice(
    mut output: ExecuteOutput,
    notice: Option<String>,
    max_tool_result_bytes: Option<usize>,
) -> ExecuteOutput {
    let Some(notice) = notice else {
        return output;
    };
    output.model_output = match max_tool_result_bytes {
        Some(limit) => {
            let primary_budget = execute_primary_budget(limit, true);
            let prepared = prepare_model_output("skill", output.model_output, primary_budget);
            let primary = &prepared[..prepared.floor_char_boundary(primary_budget)];
            let marker = &DISCOVERY_MODEL_NOTICE[..DISCOVERY_MODEL_NOTICE.len().min(limit)];
            format!("{marker}{primary}")
        }
        None => format!("{DISCOVERY_MODEL_NOTICE}{}", output.model_output),
    };
    output.diagnostic_notice = Some(notice);
    output
}

pub(crate) fn bounded_skill_error(text: String, max_bytes: usize) -> String {
    prepare_model_output("skill", text, max_bytes)
}

pub(crate) fn format_missing_skill(name: &str, max_bytes: usize) -> String {
    const SUFFIX: &str =
        "\" not found. Refresh available skills and retry with an advertised name.";
    const FALLBACK: &str =
        "Skill not found. Refresh available skills and retry with an advertised name.";
    let fixed_len = IDENTITY_PREFIX.len() + SUFFIX.len();
    if fixed_len.saturating_add(ELLIPSIS.len()) >= max_bytes {
        return bounded_skill_error(FALLBACK.to_owned(), max_bytes);
    }
    let encoded = encoded_scalar(name);
    let ellipsis_len = if encoded.len() > max_bytes - fixed_len {
        ELLIPSIS.len()
    } else {
        0
    };
    let shown = bounded_encoded_prefix(encoded.as_bytes(), max_bytes - fixed_len - ellipsis_len);
    let ellipsis = if shown.len() < encoded.len() {
        ELLIPSIS
    } else {
        ""
    };
    bounded_skill_error(
        format!("{IDENTITY_PREFIX}{shown}{ellipsis}{SUFFIX}"),
        max_bytes,
    )
}

pub(crate) fn format_exact_skill_not_found(
    name: &str,
    location: &Path,
    max_bytes: usize,
) -> String {
    format_identity_pair(
        &IdentityPair {
            middle: "\" was not found at advertised location \"",
            suffix: "\". Refresh available skills and retry with an advertised name and location.",
            fallback: "Skill was not found at the advertised location. Refresh available skills and retry with an advertised name and location.",
        },
        name,
        location,
        max_bytes,
    )
}

pub(crate) fn format_skill_location_mismatch(
    name: &str,
    location: &Path,
    max_bytes: usize,
) -> String {
    format_identity_pair(
        &IdentityPair {
            middle: "\" does not match the skill advertised at location \"",
            suffix: "\". Refresh available skills and retry with the advertised name and location.",
            fallback: "Skill name and location do not match one advertised skill. Refresh available skills and retry with the advertised name and location.",
        },
        name,
        location,
        max_bytes,
    )
}

struct IdentityPair {
    middle: &'static str,
    suffix: &'static str,
    fallback: &'static str,
}

fn format_identity_pair(
    pair: &IdentityPair,
    name: &str,
    location: &Path,
    max_bytes: usize,
) -> String {
    let fixed_len = IDENTITY_PREFIX.len() + pair.middle.len() + pair.suffix.len();
    if fixed_len.saturating_add(2 * ELLIPSIS.len()) >= max_bytes {
        return bounded_skill_error(pair.fallback.to_owned(), max_bytes);
    }
    let encoded_name = encoded_scalar(name);
    let encoded_location = encoded_bytes(location.as_os_str().as_bytes());
    let available = max_bytes - fixed_len - 2 * ELLIPSIS.len();
    let name_budget = encoded_name.len().min(available.min(MAX_NAME_BYTES));
    let shown_name = bounded_encoded_prefix(encoded_name.as_bytes(), name_budget);
    let shown_location = bounded_encoded_prefix(&encoded_location, available - name_budget);
    let ellipsis = |shown: &str, encoded_len: usize| {
        if shown.len() < encoded_len {
            ELLIPSIS
        } else {
            ""
        }
    };
    bounded_skill_error(
        format!(
            "{IDENTITY_PREFIX}{shown_name}{}{}{shown_location}{}{}",
            ellipsis(shown_name, encoded_name.len()),
            pair.middle,
            ellipsis(shown_location, encoded_location.len()),
            pair.suffix
        ),
        max_bytes,
    )
}

pub(crate) fn format_ambiguous_skill<'s>(
    candidates: impl Iterator<Item = &'s Skill> + Clone,
    name: &str,
    max_bytes: usize,
) -> String {
    let match_count = candidates.clone().count();
    let names_differ = candidates.clone().any(|skill| skill.name != name);
    let retry = if names_differ {
        "Retry with one advertised name and location"
    } else {
        "Retry with the name and one advertised location"
    };
    let mut out = format!(
        "{IDENTITY_PREFIX}{}\" is ambiguous. {retry}: ",
        encoded_scalar(name)
    )
    .into_bytes();
    let mut shown_count = 0;
    for skill in candidates {
        let mut choice = Vec::new();
        if names_differ {
            choice.push(b'"');
            choice.extend_from_slice(encoded_scalar(&skill.name).as_bytes());
            choice.extend_from_slice(b"\" at ");
        }
        choice.push(b'"');
        choice.extend(encoded_bytes(skill.path.as_os_str().as_bytes()));
        choice.push(b'"');
        let separator = if shown_count > 0 { ", " } else { "" };
        let suffix = ambiguous_suffix(match_count - (shown_count + 1), max_bytes);
        let prospective_len = out
            .len()
            .saturating_add(separator.len())
            .saturating_add(choice.len())
            .saturating_add(suffix.len());
        if prospective_len > max_bytes {
            break;
        }
        out.extend_from_slice(separator.as_bytes());
        out.extend_from_slice(&choice);
        shown_count += 1;
    }
    if shown_count == 0 {
        return bounded_skill_error(
            format!(
                "Requested skill name is ambiguous; all {match_count} advertised locations were omitted by the {max_bytes}-byte tool-result limit. Refresh available skills and retry with an advertised name and location."
            ),
            max_bytes,
        );
    }
    out.extend_from_slice(ambiguous_suffix(match_count - shown_count, max_bytes).as_bytes());
    bounded_skill_error(sanitize_model_text_owned(out), max_bytes)
}

fn ambiguous_suffix(omitted_count: usize, max_bytes: usize) -> String {
    if omitted_count == 0 {
        return ".".to_owned();
    }
    let plural = if omitted_count == 1 { "" } else { "s" };
    format!(
        "; {omitted_count} additional advertised location{plural} omitted by the {max_bytes}-byte tool-result limit. Refresh available skills and retry with an advertised name and location."
    )
}

pub(crate) fn skill_chunk_blocked_marker(
    skill_name: &str,
    resource: &str,
    observed_bytes: usize,
    limit: ContextLimit,
    offset: usize,
) -> String {
    format!(
        "<context_limit name=\"skill_chunk_bytes\" action=\"blocked\" skill=\"{}\" resource=\"{}\" observed_bytes=\"{observed_bytes}\" effective_bytes=\"{}\" source=\"{}\" offset=\"{offset}\" override=\"{CHUNK_OVERRIDE}\" />",
        encoded_scalar(skill_name),
        encoded_scalar(resource),
        limit.effective_bytes(),
        limit.source.label()
    )
}

pub(crate) fn skill_chunk_truncated_marker(
    observed_bytes: usize,
    limit: ContextLimit,
    next_offset: usize,
) -> String {
    format!(
        "<context_limit name=\"skill_chunk_bytes\" action=\"truncated\" observed_bytes=\"{observed_bytes}\" effective_bytes=\"{}\" source=\"{}\" next_offset=\"{next_offset}\" override=\"{CHUNK_OVERRIDE}\" />",
        limit.effective_bytes(),
        limit.source.label()
    )
}

pub(crate) fn skill_file_blocked_marker(
    resource: &str,
    observed_bytes: usize,
    limit: ContextLimit,
) -> String {
    format!(
        "<context_limit name=\"skill_file_bytes\" action=\"blocked_remainder\" observed_bytes=\"{observed_bytes}\" effective_bytes=\"{}\" source=\"{}\" resource=\"{}\" override=\"{FILE_OVERRIDE}\" />",
        limit.effective_bytes(),
        limit.source.label(),
        encoded_scalar(resource)
    )
}

pub(crate) fn skill_chunk_notice(
    skill_name: &str,
    resource: &str,
    observed_bytes: usize,
    limit: ContextLimit,
    next_offset: usize,
) -> String {
    format!(
        "{}\" truncated: observed={observed_bytes} bytes effective={} bytes source={}; continue with offset={next_offset} or override with {CHUNK_OVERRIDE}",
        resource_notice_prefix(skill_name, resource),
        limit.effective_bytes(),
        limit.source.label()
    )
}

pub(crate) fn skill_file_blocked_notice(
    skill_name: &str,
    resource: &str,
    observed_bytes: usize,
    limit: ContextLimit,
) -> String {
    format!(
        "{}\" remainder blocked: observed={observed_bytes} bytes effective={} bytes source={}; override with {FILE_OVERRIDE}",
        resource_notice_prefix(skill_name, resource),
        limit.effective_bytes(),
        limit.source.label()
    )
}

fn resource_notice_prefix(skill_name: &str, resource: &str) -> String {
    format!(
        "[context] skill resource \"{}/{}",
        encoded_scalar(skill_name),
        encoded_scalar(resource)
    )
}
