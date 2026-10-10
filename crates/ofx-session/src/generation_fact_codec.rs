use std::fmt::Write as _;

use ofx_contract::{GenerationFact, Json};

use crate::json_fields::push_string;

const FIELDS: usize = 9;
const FIELDS_WITH_SEARCH_CALLS: usize = 10;

pub(crate) fn parse(value: &Json<'_>) -> Option<GenerationFact> {
    let object = value.as_object()?;
    if !matches!(object.len(), FIELDS | FIELDS_WITH_SEARCH_CALLS) {
        return None;
    }
    let reasoning = object.get("reasoning_tokens")?;
    let fact = GenerationFact {
        id: object.get("id")?.as_str()?.to_owned(),
        created_at_ms: non_negative(object.get("created_at_ms")?)?,
        model: object.get("model")?.as_str()?.to_owned(),
        input_tokens: object.get("input_tokens")?.as_u64()?,
        output_tokens: object.get("output_tokens")?.as_u64()?,
        cache_read_tokens: object.get("cache_read_tokens")?.as_u64()?,
        cache_write_tokens: object.get("cache_write_tokens")?.as_u64()?,
        reasoning_tokens: match reasoning {
            Json::Null => None,
            value => Some(value.as_u64()?),
        },
        billable_web_search_calls: match object.get("billable_web_search_calls") {
            Some(value) => value.as_u64()?,
            None => 0,
        },
        total_cost: cost(object.get("total_cost")?)?,
    };
    fact.is_valid().then_some(fact)
}

pub(crate) fn write(out: &mut String, fact: &GenerationFact) {
    out.push_str("{\"id\":");
    push_string(out, &fact.id);
    let _ = write!(out, ",\"created_at_ms\":{},\"model\":", fact.created_at_ms);
    push_string(out, &fact.model);
    let _ = write!(
        out,
        ",\"input_tokens\":{},\"output_tokens\":{},\"cache_read_tokens\":{},\"cache_write_tokens\":{},\"reasoning_tokens\":",
        fact.input_tokens, fact.output_tokens, fact.cache_read_tokens, fact.cache_write_tokens,
    );
    match fact.reasoning_tokens {
        Some(reasoning) => {
            let _ = write!(out, "{reasoning}");
        }
        None => out.push_str("null"),
    }
    let _ = write!(
        out,
        ",\"billable_web_search_calls\":{},\"total_cost\":{}}}",
        fact.billable_web_search_calls, fact.total_cost
    );
}

pub(crate) fn non_negative(value: &Json<'_>) -> Option<i64> {
    i64::try_from(value.as_u64()?).ok()
}

pub(crate) fn cost(value: &Json<'_>) -> Option<f64> {
    let Json::Number(number) = value else {
        return None;
    };
    number
        .as_f64()
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}

#[cfg(test)]
mod tests {
    use crate::json_fields::parse_json;

    use super::*;

    const FX_FACT: &str = "{\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"created_at_ms\":42,\"model\":\"provider/model\",\"input_tokens\":8,\"output_tokens\":3,\"cache_read_tokens\":2,\"cache_write_tokens\":1,\"reasoning_tokens\":null,\"billable_web_search_calls\":4,\"total_cost\":0.25}";

    fn parsed(text: &str) -> Option<GenerationFact> {
        parse(&parse_json(text.as_bytes()).ok()?)
    }

    fn expected() -> GenerationFact {
        GenerationFact {
            id: "gen_01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
            created_at_ms: 42,
            model: "provider/model".to_owned(),
            input_tokens: 8,
            output_tokens: 3,
            cache_read_tokens: 2,
            cache_write_tokens: 1,
            reasoning_tokens: None,
            billable_web_search_calls: 4,
            total_cost: 0.25,
        }
    }

    #[test]
    fn facts_parse_the_shared_shape_fx_writes() {
        assert_eq!(parsed(FX_FACT), Some(expected()));
        let without_search_calls = FX_FACT.replace(",\"billable_web_search_calls\":4", "");
        assert_eq!(
            parsed(&without_search_calls),
            Some(GenerationFact {
                billable_web_search_calls: 0,
                ..expected()
            })
        );
        let reasoned = FX_FACT.replace("\"reasoning_tokens\":null", "\"reasoning_tokens\":3");
        assert_eq!(
            parsed(&reasoned),
            Some(GenerationFact {
                reasoning_tokens: Some(3),
                ..expected()
            })
        );
        for (from, to, total_cost) in [
            ("\"total_cost\":0.25", "\"total_cost\":2", 2.0),
            ("\"total_cost\":0.25", "\"total_cost\":1e-7", 1e-7),
            (
                "\"total_cost\":0.25",
                "\"total_cost\":18446744073709551616",
                1.844_674_407_370_955_2e19,
            ),
        ] {
            assert_eq!(
                parsed(&FX_FACT.replace(from, to)),
                Some(GenerationFact {
                    total_cost,
                    ..expected()
                }),
                "{to}"
            );
        }
        let unknown_tenth = FX_FACT.replace("\"billable_web_search_calls\":4", "\"future\":4");
        assert_eq!(
            parsed(&unknown_tenth),
            Some(GenerationFact {
                billable_web_search_calls: 0,
                ..expected()
            })
        );
    }

    #[test]
    fn facts_write_the_bytes_fx_writes_and_read_back() {
        let mut written = String::new();
        write(&mut written, &expected());
        assert_eq!(written, FX_FACT);
        let mut reasoned = String::new();
        let fact = GenerationFact {
            reasoning_tokens: Some(2),
            total_cost: 1e-7,
            model: "a\"b".to_owned(),
            ..expected()
        };
        write(&mut reasoned, &fact);
        assert!(reasoned.contains("\"model\":\"a\\\"b\""), "{reasoned}");
        assert!(reasoned.contains("\"reasoning_tokens\":2,"), "{reasoned}");
        assert!(
            reasoned.ends_with(",\"total_cost\":0.0000001}"),
            "{reasoned}"
        );
    }

    #[test]
    fn facts_reject_other_shapes_and_invalid_values() {
        for (from, to) in [
            ("\"reasoning_tokens\":null,", ""),
            (
                "\"total_cost\":0.25}",
                "\"total_cost\":0.25,\"a\":1,\"b\":2}",
            ),
            (
                "\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV\"",
                "\"id\":\"resp_1\"",
            ),
            ("\"id\":\"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV\"", "\"id\":7"),
            ("\"created_at_ms\":42", "\"created_at_ms\":-1"),
            (
                "\"created_at_ms\":42",
                "\"created_at_ms\":9223372036854775808",
            ),
            ("\"created_at_ms\":42", "\"created_at_ms\":42.0"),
            ("\"model\":\"provider/model\"", "\"model\":\"two words\""),
            ("\"model\":\"provider/model\"", "\"model\":null"),
            ("\"input_tokens\":8", "\"input_tokens\":\"8\""),
            ("\"output_tokens\":3", "\"output_tokens\":-3"),
            ("\"cache_read_tokens\":2", "\"cache_read_tokens\":9"),
            ("\"cache_write_tokens\":1", "\"cache_write_tokens\":9"),
            ("\"reasoning_tokens\":null", "\"reasoning_tokens\":4"),
            ("\"reasoning_tokens\":null", "\"reasoning_tokens\":\"1\""),
            (
                "\"billable_web_search_calls\":4",
                "\"billable_web_search_calls\":null",
            ),
            ("\"total_cost\":0.25", "\"total_cost\":-0.25"),
            ("\"total_cost\":0.25", "\"total_cost\":\"0.25\""),
        ] {
            assert_eq!(parsed(&FX_FACT.replace(from, to)), None, "{to}");
        }
        assert_eq!(parsed("[]"), None);
    }
}
