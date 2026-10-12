use ofx_contract::{Json, Object, ProviderBilling, valid_gateway_generation_id};
use ofx_trace::trace_log;

use super::ToolStream;

const MAX_MODEL_ID_BYTES: usize = 128;
const MAX_CANONICAL_SLUG_BYTES: usize = 1024;
const MILLIS_PER_SECOND: i64 = 1000;
const SECONDS_PER_DAY: i64 = 86_400;

#[derive(Debug, Default)]
pub(super) struct Generation {
    id: Option<String>,
    invalid: bool,
    created_at_ms: Option<i64>,
    timestamp_invalid: bool,
    model_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InvalidBilling;

impl Generation {
    pub(super) fn observe(&mut self, fields: &Object<'_>) {
        let Some(metadata) = fields.get("providerMetadata") else {
            return;
        };
        let Some(gateway) = metadata.as_object().map(|metadata| metadata.get("gateway")) else {
            self.invalid = true;
            return;
        };
        let Some(gateway) = gateway else {
            return;
        };
        let Some(id) = gateway
            .as_object()
            .map(|gateway| gateway.get("generationId"))
        else {
            self.invalid = true;
            return;
        };
        let Some(id) = id else {
            return;
        };
        match (
            id.as_str().filter(|id| valid_gateway_generation_id(id)),
            &self.id,
        ) {
            (None, _) => self.invalid = true,
            (Some(id), Some(seen)) => self.invalid |= id != seen,
            (Some(id), None) => self.id = Some(id.to_owned()),
        }
    }

    pub(super) fn response_metadata(&mut self, fields: &Object<'_>) {
        if let Some(timestamp) = fields.get("timestamp") {
            match timestamp.as_str().and_then(parse_gateway_timestamp) {
                Some(ms) if self.created_at_ms.is_none_or(|seen| seen == ms) => {
                    if !self.timestamp_invalid {
                        self.created_at_ms = Some(ms);
                    }
                }
                _ => {
                    self.timestamp_invalid = true;
                    self.created_at_ms = None;
                }
            }
        }
        if let Some(model) = fields.get("modelId") {
            match model
                .as_str()
                .filter(|model| !model.is_empty() && model.len() <= MAX_MODEL_ID_BYTES)
            {
                Some(model) => match &self.model_id {
                    Some(seen) => self.invalid |= seen != model,
                    None => self.model_id = Some(model.to_owned()),
                },
                None => self.invalid = true,
            }
        }
    }

    pub(super) fn billing(
        &self,
        fields: &Object<'_>,
        tools: &ToolStream,
    ) -> Option<ProviderBilling> {
        let billed = self
            .created_at_ms
            .ok_or(InvalidBilling)
            .and_then(|created_at_ms| {
                terminal_billing(fields, created_at_ms, tools.billable_searches())
            });
        let Ok(billing) = billed else {
            trace_log!(
                "sse",
                "stream billing ignored reason=invalid_terminal_metadata"
            );
            return None;
        };
        let generation_id = self.id.clone().filter(|_| !self.invalid)?;
        Some(ProviderBilling {
            generation_id,
            ..billing
        })
    }
}

fn terminal_billing(
    fields: &Object<'_>,
    created_at_ms: i64,
    billable_web_search_calls: u64,
) -> Result<ProviderBilling, InvalidBilling> {
    let usage = fields
        .get("usage")
        .and_then(Json::as_object)
        .ok_or(InvalidBilling)?;
    let input = usage
        .get("inputTokens")
        .and_then(Json::as_object)
        .ok_or(InvalidBilling)?;
    let output = usage
        .get("outputTokens")
        .and_then(Json::as_object)
        .ok_or(InvalidBilling)?;
    let gateway = fields
        .get("providerMetadata")
        .and_then(Json::as_object)
        .and_then(|metadata| metadata.get("gateway"))
        .and_then(Json::as_object)
        .ok_or(InvalidBilling)?;
    let model = gateway
        .get("routing")
        .and_then(Json::as_object)
        .and_then(|routing| routing.get("canonicalSlug"))
        .and_then(Json::as_str)
        .filter(|slug| valid_canonical_slug(slug))
        .ok_or(InvalidBilling)?;
    let total_cost = gateway
        .get("cost")
        .and_then(Json::as_str)
        .and_then(|cost| cost.parse::<f64>().ok())
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
        .ok_or(InvalidBilling)?;
    let input_tokens = count(input.get("total"))?;
    let output_tokens = count(output.get("total"))?;
    let cache_read_tokens = input
        .get("cacheRead")
        .map_or(Ok(0), |value| count(Some(value)))?;
    let cache_write_tokens = input
        .get("cacheWrite")
        .map_or(Ok(0), |value| count(Some(value)))?;
    let reasoning_tokens = match output.get("reasoning") {
        None | Some(Json::Null) => None,
        Some(value) => Some(count(Some(value))?),
    };
    if cache_read_tokens > input_tokens
        || cache_write_tokens > input_tokens
        || reasoning_tokens.is_some_and(|reasoning| reasoning > output_tokens)
    {
        return Err(InvalidBilling);
    }
    Ok(ProviderBilling {
        generation_id: String::new(),
        created_at_ms,
        model: model.to_owned(),
        total_cost,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens,
        billable_web_search_calls,
    })
}

fn count(value: Option<&Json<'_>>) -> Result<u64, InvalidBilling> {
    value
        .and_then(Json::as_i64)
        .and_then(|count| u64::try_from(count).ok())
        .ok_or(InvalidBilling)
}

fn valid_canonical_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= MAX_CANONICAL_SLUG_BYTES
        && slug.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

fn parse_gateway_timestamp(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let field = |range: std::ops::Range<usize>| -> Option<i64> {
        let digits = text.get(range)?;
        digits
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| digits.parse().ok())
            .flatten()
    };
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
    if year < 1970
        || !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let rest = &text[19..];
    let (fraction_ms, zone) = match rest.strip_prefix('.') {
        Some(fraction) => {
            let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 || digits > 9 {
                return None;
            }
            let kept = &fraction[..digits.min(3)];
            let scale = [100, 10, 1][kept.len() - 1];
            (kept.parse::<i64>().ok()? * scale, &fraction[digits..])
        }
        None => (0, rest),
    };
    if zone != "Z" {
        return None;
    }
    let seconds = days_from_civil(year, month, day)
        .checked_mul(SECONDS_PER_DAY)?
        .checked_add(hour * 3600 + minute * 60 + second)?;
    seconds
        .checked_mul(MILLIS_PER_SECOND)?
        .checked_add(fraction_ms)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests;
