use std::sync::atomic::{AtomicBool, Ordering};

use ofx_trace::{TraceContext, trace_event};
use reqwest::header::HeaderMap;

use crate::secret_mask::mask_configured_secrets;

const HEADERS: [&str; 5] = [
    "x-vercel-ai-gateway-model",
    "x-vercel-ai-gateway-model-id",
    "x-ai-gateway-model",
    "ai-gateway-model",
    "ai-language-model-id",
];
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];

static TRACED: AtomicBool = AtomicBool::new(false);

pub(crate) fn from_headers(headers: &HeaderMap, requested_model: &str, secrets: &[String]) -> bool {
    let Some((name, value)) = headers
        .iter()
        .find(|(name, _)| HEADERS.contains(&name.as_str()))
    else {
        return false;
    };
    let value = String::from_utf8_lossy(value.as_bytes());
    let resolved = mask_configured_secrets(value.trim_matches(TRIMMED).to_owned(), secrets);
    traced(requested_model, name.as_str(), &resolved);
    true
}

pub(crate) fn traced(requested_model: &str, source: &str, resolved_model: &str) {
    if TRACED.swap(true, Ordering::AcqRel) {
        return;
    }
    trace_event!(
        "gateway",
        "resolved_model",
        TraceContext::default(),
        "requested_model={requested_model} source={source} resolved_model={resolved_model}"
    );
}

pub(crate) fn missing(requested_model: &str) {
    if TRACED.swap(true, Ordering::AcqRel) {
        return;
    }
    trace_event!(
        "gateway",
        "resolved_model_missing",
        TraceContext::default(),
        "requested_model={requested_model}"
    );
}
