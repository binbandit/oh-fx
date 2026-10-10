use ofx_text::StreamingEstimator;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::compactor::text_tokens;

const RESPONSES_IMAGE_KEY: &str = "\"image_url\":\"";
const CHAT_IMAGE_KEY: &str = "\"url\":\"";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestCost {
    pub(crate) bytes: usize,
    pub(crate) text_tokens: usize,
    pub(crate) image_identity: Option<[u8; 32]>,
    pub(crate) estimated_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Calibration {
    pub(crate) model: String,
    pub(crate) request: RequestCost,
    pub(crate) exact_input_tokens: usize,
}

impl Calibration {
    fn applies(&self, cost: &RequestCost) -> bool {
        self.request.bytes != 0
            && self.exact_input_tokens != 0
            && self.request.image_identity == cost.image_identity
    }
}

impl RequestCost {
    pub(crate) fn measure(body: &str, has_images: bool) -> Self {
        let images = if has_images {
            measure_images(body)
        } else {
            None
        };
        images.unwrap_or_else(|| {
            let tokens = text_tokens(body);
            Self {
                bytes: body.len(),
                text_tokens: tokens,
                image_identity: None,
                estimated_tokens: tokens,
            }
        })
    }

    pub(crate) fn calibrated(self, calibration: &Calibration) -> Self {
        if !calibration.applies(&self) {
            return self;
        }
        let exact = calibration.exact_input_tokens;
        let estimated_tokens = if self.image_identity.is_some() {
            let measured = calibration.request.text_tokens;
            let calibrated = if self.text_tokens >= measured {
                exact.saturating_add(self.text_tokens - measured)
            } else {
                exact.saturating_sub(measured - self.text_tokens)
            };
            calibrated.max(self.text_tokens)
        } else {
            multiply_divide_ceil(self.bytes, exact, calibration.request.bytes).max(1)
        };
        Self {
            estimated_tokens,
            ..self
        }
    }
}

struct ImagePart<'a> {
    kind: &'a str,
    detail: &'a str,
    payload: &'a str,
    key: &'static str,
}

fn measure_images(body: &str) -> Option<RequestCost> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    let parts = match parsed.get("input") {
        Some(input) => responses_image_parts(input.as_array()?)?,
        None => chat_image_parts(parsed.get("messages")?.as_array()?)?,
    };
    let mut estimator = StreamingEstimator::default();
    let mut identity = Sha256::new();
    let mut cursor = 0;
    for part in &parts {
        let offset = payload_offset(body, cursor, part.key, part.payload)?;
        estimator.consume(&body[cursor..offset]);
        cursor = offset + part.payload.len();
        for value in [part.kind, "", part.detail, part.payload] {
            identity.update(value.len().to_ne_bytes());
            identity.update(value);
        }
    }
    estimator.consume(&body[cursor..]);
    let tokens = usize::try_from(estimator.estimate()).unwrap_or(usize::MAX);
    Some(RequestCost {
        bytes: body.len(),
        text_tokens: tokens,
        image_identity: (!parts.is_empty()).then(|| identity.finalize().into()),
        estimated_tokens: tokens,
    })
}

fn responses_image_parts(items: &[Value]) -> Option<Vec<ImagePart<'_>>> {
    let mut parts = Vec::new();
    for item in items {
        let content = if item.get("role").and_then(Value::as_str) == Some("user") {
            item.get("content")
        } else if item.get("type").and_then(Value::as_str) == Some("function_call_output") {
            item.get("output")
        } else {
            continue;
        };
        for part in content.and_then(Value::as_array).into_iter().flatten() {
            if part.get("type").and_then(Value::as_str) != Some("input_image") {
                continue;
            }
            parts.push(ImagePart {
                kind: "input_image",
                detail: part
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                payload: part.get("image_url")?.as_str()?,
                key: RESPONSES_IMAGE_KEY,
            });
        }
    }
    Some(parts)
}

fn chat_image_parts(messages: &[Value]) -> Option<Vec<ImagePart<'_>>> {
    let mut parts = Vec::new();
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        for part in message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if part.get("type").and_then(Value::as_str) != Some("image_url") {
                continue;
            }
            let image = part.get("image_url")?;
            parts.push(ImagePart {
                kind: "image_url",
                detail: image
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                payload: image.get("url")?.as_str()?,
                key: CHAT_IMAGE_KEY,
            });
        }
    }
    Some(parts)
}

fn payload_offset(body: &str, cursor: usize, key: &str, payload: &str) -> Option<usize> {
    let mut search = cursor;
    loop {
        let found = search + body.get(search..)?.find(key)?;
        let start = found + key.len();
        let rest = &body[start..];
        if rest.starts_with(payload) && rest[payload.len()..].starts_with('"') {
            return Some(start);
        }
        search = start;
    }
}

fn multiply_divide_ceil(value: usize, numerator: usize, denominator: usize) -> usize {
    let product =
        u128::try_from(value).unwrap_or(u128::MAX) * u128::try_from(numerator).unwrap_or(u128::MAX);
    let denominator = u128::try_from(denominator).unwrap_or(u128::MAX);
    usize::try_from(product.div_ceil(denominator)).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests;
