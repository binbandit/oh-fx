use ofx_contract::ProviderRouting;
use ofx_trace::trace_log;
use serde_json::{Map, Value};

use crate::config_runtime::LayerError;
use crate::settings_store::{MAX_PROVIDER_ORDER_ENTRIES, validate_provider_slug};

const CONFIG: &str = "config";
const ORDER_VARIABLE: &str = "OH_FX_PROVIDER_ORDER";
const STRICT_VARIABLE: &str = "OH_FX_PROVIDER_STRICT";
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RoutingLayer {
    order: Option<Vec<String>>,
    strict: Option<bool>,
}

impl RoutingLayer {
    pub(crate) fn parse(object: &Map<String, Value>) -> Result<Self, LayerError> {
        let order = object.get("provider_order").map(parse_order).transpose()?;
        let strict = object
            .get("provider_strict")
            .map(|value| value.as_bool().ok_or(LayerError::InvalidProviderStrictType))
            .transpose()?;
        Ok(Self { order, strict })
    }
}

fn parse_order(value: &Value) -> Result<Vec<String>, LayerError> {
    let Value::Array(items) = value else {
        return Err(LayerError::InvalidProviderOrderType);
    };
    if items.len() > MAX_PROVIDER_ORDER_ENTRIES {
        return Err(LayerError::InvalidProviderOrderValue);
    }
    let mut order: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Value::String(slug) if validate_provider_slug(slug) && !order.contains(slug) => {
                order.push(slug.clone());
            }
            _ => return Err(LayerError::InvalidProviderOrderValue),
        }
    }
    Ok(order)
}

pub(crate) fn resolve(
    layers: [&RoutingLayer; 3],
    lookup: &dyn Fn(&str) -> Option<String>,
) -> ProviderRouting {
    let mut order = layers.iter().find_map(|layer| layer.order.clone());
    let mut strict = layers.iter().find_map(|layer| layer.strict);
    if let Some(raw) = lookup(ORDER_VARIABLE) {
        match parse_provider_order_list(&raw) {
            Ok(Some(listed)) => order = Some(listed),
            Ok(None) => {}
            Err(InvalidOrder) => trace_log!(CONFIG, "ignoring invalid {ORDER_VARIABLE} value"),
        }
    }
    if let Some(raw) = lookup(STRICT_VARIABLE) {
        let trimmed = raw.trim_matches(TRIMMED);
        match parse_env_switch(trimmed) {
            Some(value) => strict = Some(value),
            None if !trimmed.is_empty() => {
                trace_log!(CONFIG, "ignoring invalid {STRICT_VARIABLE} value");
            }
            None => {}
        }
    }
    ProviderRouting {
        order: order.unwrap_or_default(),
        strict: strict.unwrap_or(false),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidOrder;

pub(crate) fn parse_provider_order_list(raw: &str) -> Result<Option<Vec<String>>, InvalidOrder> {
    let mut order: Vec<String> = Vec::new();
    for slug in raw.split(',').map(|token| token.trim_matches(TRIMMED)) {
        if slug.is_empty() {
            continue;
        }
        if !validate_provider_slug(slug)
            || order.len() >= MAX_PROVIDER_ORDER_ENTRIES
            || order.iter().any(|listed| listed == slug)
        {
            return Err(InvalidOrder);
        }
        order.push(slug.to_owned());
    }
    Ok(Some(order).filter(|order| !order.is_empty()))
}

pub fn provider_order_list(raw: &str) -> Option<Vec<String>> {
    parse_provider_order_list(raw).ok().flatten()
}

pub(crate) fn parse_env_switch(value: &str) -> Option<bool> {
    const ON: [&str; 4] = ["1", "true", "on", "yes"];
    const OFF: [&str; 4] = ["0", "false", "off", "no"];
    if ON.iter().any(|word| value.eq_ignore_ascii_case(word)) {
        Some(true)
    } else if OFF.iter().any(|word| value.eq_ignore_ascii_case(word)) {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
