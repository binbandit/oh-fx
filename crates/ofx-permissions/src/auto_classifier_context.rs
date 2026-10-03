use ofx_contract::RootUserRequests;
use ofx_text::{HeadRounding, encode_terminal_safe, is_terminal_safe, write_head_tail_bounded};

const MAX_ROOT_USER_BYTES: usize = 1024;
const CURRENT_LABEL: &str = "current_request: ";
const FIRST_ROOT_USER_LABEL: &str = "first_root_user_request: ";
const RECENT_ROOT_USER_LABEL: &str = "recent_root_user_request: ";
const OMITTED_ROOT_USER_LABEL: &str = "omitted_proven_root_user_turns: ";
const TRUSTED_PERMISSION_FEEDBACK_LABEL: &str = "trusted_user_permission_feedback: ";
const OMITTED_PERMISSION_FEEDBACK_LABEL: &str = "omitted_trusted_user_permission_feedback: ";
const CONTENT_OMISSION_MARKER: &str = " [... omitted ...] ";
const COUNT_MARKER_RESERVE: usize = 64;

pub(crate) fn root_user_request_context(context: &str) -> Option<&str> {
    is_canonical_root_user_context(context).then(|| &context[..canonical_feedback_start(context)])
}

pub fn canonical_root_user_context(requests: &RootUserRequests) -> String {
    let earlier: Vec<&str> = requests.earlier.iter().map(String::as_str).collect();
    build_canonical_root_user_context(&requests.current, &earlier, requests.compacted_turns)
}

pub(crate) fn build_canonical_root_user_context(
    current_request: &str,
    earlier_requests: &[&str],
    compacted_turns: Option<usize>,
) -> String {
    let mut turns = earlier_requests.to_vec();
    turns.push(current_request);
    build_root_user_context_bounded(
        &turns,
        MAX_ROOT_USER_BYTES,
        compacted_turns.map_or(0, |removed| removed.max(1)),
        compacted_turns.is_none(),
    )
}

fn is_canonical_root_user_context(context: &str) -> bool {
    let Some(body) = context
        .strip_suffix('\n')
        .filter(|_| context.len() <= MAX_ROOT_USER_BYTES)
    else {
        return false;
    };
    let mut lines = body.split('\n');
    if !lines
        .next()
        .is_some_and(|current| canonical_text_line(current, CURRENT_LABEL))
    {
        return false;
    }
    let mut saw_first = false;
    let mut saw_recent = false;
    let mut saw_omitted_root = false;
    let mut feedback_started = false;
    let mut saw_omitted_feedback = false;
    for line in lines {
        if line.starts_with(FIRST_ROOT_USER_LABEL) {
            if feedback_started
                || saw_first
                || saw_recent
                || saw_omitted_root
                || !canonical_text_line(line, FIRST_ROOT_USER_LABEL)
            {
                return false;
            }
            saw_first = true;
        } else if line.starts_with(RECENT_ROOT_USER_LABEL) {
            if feedback_started
                || saw_omitted_root
                || !canonical_text_line(line, RECENT_ROOT_USER_LABEL)
            {
                return false;
            }
            saw_recent = true;
        } else if line.starts_with(OMITTED_ROOT_USER_LABEL) {
            if feedback_started
                || saw_omitted_root
                || !canonical_count_line(line, OMITTED_ROOT_USER_LABEL)
            {
                return false;
            }
            saw_omitted_root = true;
        } else if line.starts_with(TRUSTED_PERMISSION_FEEDBACK_LABEL) {
            if saw_omitted_feedback || !canonical_text_line(line, TRUSTED_PERMISSION_FEEDBACK_LABEL)
            {
                return false;
            }
            feedback_started = true;
        } else if line.starts_with(OMITTED_PERMISSION_FEEDBACK_LABEL) {
            if saw_omitted_feedback
                || !canonical_count_line(line, OMITTED_PERMISSION_FEEDBACK_LABEL)
            {
                return false;
            }
            feedback_started = true;
            saw_omitted_feedback = true;
        } else {
            return false;
        }
    }
    true
}

fn canonical_text_line(line: &str, label: &str) -> bool {
    line.strip_prefix(label)
        .is_some_and(|value| !value.is_empty() && is_terminal_safe(value.as_bytes()))
}

fn canonical_count_line(line: &str, label: &str) -> bool {
    line.strip_prefix(label).is_some_and(|value| {
        !value.is_empty()
            && !value.starts_with('0')
            && value.bytes().all(|byte| byte.is_ascii_digit())
            && value.parse::<usize>().is_ok_and(|count| count > 0)
    })
}

fn canonical_feedback_start(context: &str) -> usize {
    [
        TRUSTED_PERMISSION_FEEDBACK_LABEL,
        OMITTED_PERMISSION_FEEDBACK_LABEL,
    ]
    .iter()
    .filter_map(|label| {
        context
            .match_indices('\n')
            .find(|(index, _)| context[index + 1..].starts_with(label))
            .map(|(index, _)| index + 1)
    })
    .min()
    .unwrap_or(context.len())
}

fn required_anchor_budgets(lengths: [Option<usize>; 3], capacity: usize) -> [usize; 3] {
    let mut budgets = [0; 3];
    let present = lengths.iter().flatten().count();
    let initial_share = capacity / present;
    let mut used = 0;
    for (budget, length) in budgets.iter_mut().zip(lengths) {
        if let Some(length) = length {
            *budget = length.min(initial_share);
            used += *budget;
        }
    }
    let mut remaining = capacity - used;
    while remaining > 0 {
        let unfinished = budgets
            .iter()
            .zip(lengths)
            .filter(|(budget, length)| length.is_some_and(|length| **budget < length))
            .count();
        if unfinished == 0 {
            break;
        }
        let share = (remaining / unfinished).max(1);
        for (budget, length) in budgets.iter_mut().zip(lengths) {
            let Some(length) = length else {
                continue;
            };
            let added = (length - *budget).min(share.min(remaining));
            *budget += added;
            remaining -= added;
            if remaining == 0 {
                break;
            }
        }
    }
    budgets
}

fn build_root_user_context_bounded(
    turns: &[&str],
    max_bytes: usize,
    prior_omitted_root_turns: usize,
    first_root_user_is_proven: bool,
) -> String {
    let latest = encode_text(turns.last().copied().unwrap_or_default());
    let first = (first_root_user_is_proven && turns.len() > 1).then(|| encode_text(turns[0]));
    let first_turn_count = usize::from(first.is_some());
    let recent_total = turns.len().saturating_sub(1 + first_turn_count);
    let newest_recent = (recent_total > 0).then(|| encode_text(turns[turns.len() - 2]));
    let omitted_after_required =
        recent_total - usize::from(newest_recent.is_some()) + prior_omitted_root_turns;
    let anchor_overhead = CURRENT_LABEL.len()
        + 1
        + first
            .as_ref()
            .map_or(0, |_| FIRST_ROOT_USER_LABEL.len() + 1)
        + newest_recent
            .as_ref()
            .map_or(0, |_| RECENT_ROOT_USER_LABEL.len() + 1);
    let recent_marker_reserve = if omitted_after_required > 0 {
        COUNT_MARKER_RESERVE
    } else {
        0
    };
    let [current_budget, first_budget, recent_budget] = required_anchor_budgets(
        [
            Some(latest.len()),
            first.as_ref().map(String::len),
            newest_recent.as_ref().map(String::len),
        ],
        max_bytes - anchor_overhead - recent_marker_reserve,
    );
    let mut out = String::new();
    write_line(&mut out, CURRENT_LABEL, &latest, current_budget);
    if let Some(first) = &first {
        write_line(&mut out, FIRST_ROOT_USER_LABEL, first, first_budget);
    }
    if let Some(recent) = &newest_recent {
        write_line(&mut out, RECENT_ROOT_USER_LABEL, recent, recent_budget);
    }
    let mut selected_recent = usize::from(newest_recent.is_some());
    let mut history_index = turns.len().saturating_sub(2);
    while history_index > first_turn_count {
        history_index -= 1;
        let older_turns_remaining = history_index - first_turn_count;
        let omission_reserve = if older_turns_remaining > 0 || prior_omitted_root_turns > 0 {
            COUNT_MARKER_RESERVE
        } else {
            0
        };
        let used = out.len();
        let entry_overhead = RECENT_ROOT_USER_LABEL.len() + 1;
        if used + entry_overhead + CONTENT_OMISSION_MARKER.len() + omission_reserve > max_bytes {
            break;
        }
        let encoded = encode_text(turns[history_index]);
        let content_budget = max_bytes - used - entry_overhead - omission_reserve;
        write_line(&mut out, RECENT_ROOT_USER_LABEL, &encoded, content_budget);
        selected_recent += 1;
    }
    let omitted_recent = recent_total - selected_recent + prior_omitted_root_turns;
    if omitted_recent > 0 {
        out.push_str(OMITTED_ROOT_USER_LABEL);
        out.push_str(&omitted_recent.to_string());
        out.push('\n');
    }
    out
}

fn write_line(out: &mut String, label: &str, text: &str, max_content_bytes: usize) {
    out.push_str(label);
    let bounded = write_head_tail_bounded(
        text.as_bytes(),
        max_content_bytes,
        CONTENT_OMISSION_MARKER,
        HeadRounding::Up,
    );
    out.push_str(&String::from_utf8_lossy(&bounded));
    out.push('\n');
}

fn encode_text(raw: &str) -> String {
    encode_terminal_safe(raw.as_bytes(), usize::MAX).text
}

#[cfg(test)]
mod tests;
