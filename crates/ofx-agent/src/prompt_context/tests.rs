use super::*;

fn calibration(request: RequestCost, exact_input_tokens: usize) -> Calibration {
    Calibration {
        model: "fixture/model".to_owned(),
        request,
        exact_input_tokens,
    }
}

#[test]
fn provider_request_measurement_includes_serialized_structure() {
    let compact = RequestCost::measure(r#"{"prompt":[{"role":"user","content":"same"}]}"#);
    let fragmented = RequestCost::measure(
        r#"{"prompt":[{"role":"user","content":"s"},{"role":"user","content":"a"},{"role":"user","content":"m"},{"role":"user","content":"e"}]}"#,
    );
    assert!(fragmented.bytes > compact.bytes);
    assert!(fragmented.estimated_tokens > compact.estimated_tokens);
}

#[test]
fn provider_request_measurement_learns_the_prior_exact_token_density() {
    let current = RequestCost {
        bytes: 1_456_988,
        text_tokens: 365_113,
        estimated_tokens: 365_113,
    };
    let calibrated = current.calibrated(&calibration(
        RequestCost {
            bytes: 767_736,
            text_tokens: 192_000,
            estimated_tokens: 192_000,
        },
        398_710,
    ));
    assert!(calibrated.estimated_tokens > 695_142);
    assert!(calibrated.estimated_tokens >= current.estimated_tokens);
}

#[test]
fn provider_usage_corrects_a_serialized_estimate_downward() {
    let cost = RequestCost {
        bytes: 34_210,
        text_tokens: 8_892,
        estimated_tokens: 8_892,
    };
    let calibrated = cost.calibrated(&calibration(cost, 6_030));
    assert_eq!(calibrated.estimated_tokens, 6_030);
    assert_eq!(calibrated.text_tokens, cost.text_tokens);
}

#[test]
fn a_calibration_without_bytes_or_exact_tokens_changes_nothing() {
    let cost = RequestCost::measure("one two three");
    let empty = RequestCost {
        bytes: 0,
        text_tokens: 0,
        estimated_tokens: 0,
    };
    assert_eq!(cost.calibrated(&calibration(empty, 10)), cost);
    assert_eq!(cost.calibrated(&calibration(cost, 0)), cost);
    assert_eq!(
        RequestCost::measure("abcd").calibrated(&calibration(
            RequestCost {
                bytes: 1_000_000,
                text_tokens: 1,
                estimated_tokens: 1
            },
            1
        )),
        RequestCost {
            bytes: 4,
            text_tokens: 1,
            estimated_tokens: 1
        }
    );
}
