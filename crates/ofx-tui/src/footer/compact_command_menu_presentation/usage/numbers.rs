const TWO_TO_THE_32: f64 = 4_294_967_296.0;

pub(super) fn token_label(value: u64, label: &str) -> String {
    format!("{} {label}", compact_unsigned(value))
}

pub(super) fn request_label(requests: Option<u64>) -> String {
    match requests {
        None => "Requests unavailable".to_owned(),
        Some(1) => "1 request".to_owned(),
        Some(requests) => format!("{} requests", grouped_unsigned(requests)),
    }
}

pub(super) fn format_duration(duration_ms: u64) -> String {
    let total_seconds = duration_ms / 1000;
    let hours = total_seconds / 3600;
    let minutes = total_seconds % 3600 / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

pub(super) fn compact_unsigned(value: u64) -> String {
    for (unit, suffix) in [(1_000_000_000, "B"), (1_000_000, "M"), (1_000, "K")] {
        if value < unit {
            continue;
        }
        if value.is_multiple_of(unit) {
            return format!("{}{suffix}", value / unit);
        }
        return format!("{:.1}{suffix}", float(value) / float(unit));
    }
    value.to_string()
}

pub(super) fn grouped_unsigned(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

pub(super) fn money(value: f64) -> String {
    format!("${value:.2}")
}

pub(super) fn usage_share(tokens: u64, total_tokens: u64) -> String {
    let share = if total_tokens == 0 {
        0.0
    } else {
        float(tokens) * 100.0 / float(total_tokens)
    };
    format!("{share:.1}%")
}

pub(super) fn float(value: u64) -> f64 {
    let high = u32::try_from(value >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(value & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    f64::from(high) * TWO_TO_THE_32 + f64::from(low)
}
