use super::*;

fn calibration(request: RequestCost, exact_input_tokens: usize) -> Calibration {
    Calibration {
        model: "fixture/model".to_owned(),
        request,
        exact_input_tokens,
    }
}

fn cost(bytes: usize, text_tokens: usize) -> RequestCost {
    RequestCost {
        bytes,
        text_tokens,
        image_identity: None,
        estimated_tokens: text_tokens,
    }
}

fn user_images(parts: &str) -> String {
    format!(r#"{{"input":[{{"role":"user","content":[{parts}]}}]}}"#)
}

#[test]
fn provider_request_measurement_includes_serialized_structure() {
    let compact = RequestCost::measure(r#"{"prompt":[{"role":"user","content":"same"}]}"#, false);
    let fragmented = RequestCost::measure(
        r#"{"prompt":[{"role":"user","content":"s"},{"role":"user","content":"a"},{"role":"user","content":"m"},{"role":"user","content":"e"}]}"#,
        false,
    );
    assert!(fragmented.bytes > compact.bytes);
    assert!(fragmented.estimated_tokens > compact.estimated_tokens);
}

#[test]
fn provider_request_image_accounting_excludes_encoded_payload_length() {
    let prefix = r#"{"input":[{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,"#;
    let suffix = r#""}]}]}"#;
    let small = RequestCost::measure(&format!("{prefix}AAAA{suffix}"), true);
    let large = RequestCost::measure(&format!("{prefix}{}{suffix}", "AAAA".repeat(1000)), true);
    assert!(large.bytes > small.bytes);
    assert_eq!(small.text_tokens, large.text_tokens);
    assert_eq!(small.estimated_tokens, large.estimated_tokens);
    assert!(small.image_identity.is_some());
    assert_ne!(small.image_identity, large.image_identity);
}

#[test]
fn provider_request_image_accounting_preserves_text_and_tool_payloads() {
    let body = r#"{"input":[{"role":"user","content":[{"type":"input_text","text":"data:image/png;base64,AAAA"},{"image_url":"data:image/png;base64,BBBB","type":"input_image"}]},{"type":"function_call","arguments":"{\"image_url\":\"AAAA\"}"},{"type":"function_call_output","output":"data:image/png;base64,AAAA"}]}"#;
    let without_image_payload = r#"{"input":[{"role":"user","content":[{"type":"input_text","text":"data:image/png;base64,AAAA"},{"image_url":"","type":"input_image"}]},{"type":"function_call","arguments":"{\"image_url\":\"AAAA\"}"},{"type":"function_call_output","output":"data:image/png;base64,AAAA"}]}"#;

    let measured = RequestCost::measure(body, true);

    assert_eq!(measured.text_tokens, text_tokens(without_image_payload));
    assert!(measured.image_identity.is_some());
}

#[test]
fn provider_request_image_accounting_finds_a_payload_its_text_repeats_first() {
    let body = user_images(
        r#"{"type":"input_text","text":"\"image_url\":\"data:image/png;base64,BBBB\""},{"type":"input_image","image_url":"data:image/png;base64,BBBB"}"#,
    );
    let without_image_payload = user_images(
        r#"{"type":"input_text","text":"\"image_url\":\"data:image/png;base64,BBBB\""},{"type":"input_image","image_url":""}"#,
    );

    assert_eq!(
        RequestCost::measure(&body, true).text_tokens,
        text_tokens(&without_image_payload)
    );
}

#[test]
fn provider_request_measurement_excludes_responses_protocol_tool_image_payloads() {
    let body = r#"{"input":[{"type":"function_call_output","call_id":"call_1","output":[{"type":"input_text","text":"capture"},{"type":"input_image","image_url":"data:image/png;base64,AAAABBBBCCCCDDDD"}]},{"role":"user","content":[{"type":"input_text","text":"next"}]}]}"#;
    let without_image_payload = r#"{"input":[{"type":"function_call_output","call_id":"call_1","output":[{"type":"input_text","text":"capture"},{"type":"input_image","image_url":""}]},{"role":"user","content":[{"type":"input_text","text":"next"}]}]}"#;

    let measured = RequestCost::measure(body, true);

    assert!(measured.image_identity.is_some());
    assert_eq!(measured.text_tokens, text_tokens(without_image_payload));
}

#[test]
fn provider_request_measurement_degrades_to_text_estimate_on_unknown_envelopes() {
    let body = r#"{"model":"fixture/model","contents":[{"role":"user","parts":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]}]}"#;

    let measured = RequestCost::measure(body, true);

    assert_eq!(measured.estimated_tokens, text_tokens(body));
    assert_eq!(measured.image_identity, None);
}

#[test]
fn chat_completions_image_payloads_stay_out_of_the_text_estimate() {
    let body = r#"{"model":"fixture/model","messages":[{"role":"system","content":"rules"},{"role":"user","content":[{"type":"text","text":"\"url\":\"data:image/png;base64,AAAA\""},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]},{"role":"tool","tool_call_id":"call_1","content":"{\"url\":\"data:image/png;base64,BBBB\"}"},{"role":"user","content":[{"type":"text","text":"The tool \"read_file\" returned 1 image(s)."},{"type":"image_url","image_url":{"url":"data:image/png;base64,BBBB"}}]}]}"#;
    let without_image_payloads = r#"{"model":"fixture/model","messages":[{"role":"system","content":"rules"},{"role":"user","content":[{"type":"text","text":"\"url\":\"data:image/png;base64,AAAA\""},{"type":"image_url","image_url":{"url":""}}]},{"role":"tool","tool_call_id":"call_1","content":"{\"url\":\"data:image/png;base64,BBBB\"}"},{"role":"user","content":[{"type":"text","text":"The tool \"read_file\" returned 1 image(s)."},{"type":"image_url","image_url":{"url":""}}]}]}"#;

    let measured = RequestCost::measure(body, true);

    assert_eq!(measured.text_tokens, text_tokens(without_image_payloads));
    assert!(measured.image_identity.is_some());
    let other_image = body.replace("AAAA\"}}]}", "CCCC\"}}]}");
    assert_ne!(
        RequestCost::measure(&other_image, true).image_identity,
        measured.image_identity
    );
}

#[test]
fn chat_completions_image_accounting_handles_a_large_payload() {
    let payload = "A".repeat(4 * 1024 * 1024);
    let body = format!(
        r#"{{"messages":[{{"role":"user","content":[{{"type":"text","text":"what is this"}},{{"type":"image_url","image_url":{{"url":"data:image/png;base64,{payload}"}}}}]}}]}}"#
    );

    assert!(RequestCost::measure(&body, true).text_tokens < 100);
}

#[test]
fn provider_request_measurement_without_images_never_parses_the_body() {
    let body = user_images(r#"{"type":"input_image","image_url":"data:image/png;base64,AAAA"}"#);

    let measured = RequestCost::measure(&body, false);

    assert_eq!(measured, cost(body.len(), text_tokens(&body)));
}

#[test]
fn provider_request_image_identity_includes_ordering_media_type_and_detail() {
    let first =
        r#"{"type":"input_image","image_url":"data:image/png;base64,AAAA","detail":"auto"}"#;
    let second = r#"{"type":"input_image","image_url":"data:image/png;base64,BBBB"}"#;
    let reference = RequestCost::measure(&user_images(&format!("{first},{second}")), true);
    for parts in [
        format!("{second},{first}"),
        first.to_owned(),
        format!(
            r#"{{"type":"input_image","image_url":"data:image/png;base64,AAAA","detail":"high"}},{second}"#
        ),
        format!(
            r#"{{"type":"input_image","image_url":"data:image/jpeg;base64,AAAA","detail":"auto"}},{second}"#
        ),
    ] {
        let changed = RequestCost::measure(&user_images(&parts), true);
        assert_ne!(reference.image_identity, changed.image_identity, "{parts}");
    }
    let empty = RequestCost::measure(r#"{"input":[]}"#, true);
    assert_eq!(empty.text_tokens, text_tokens(r#"{"input":[]}"#));
    assert_eq!(empty.image_identity, None);
}

#[test]
fn provider_request_image_accounting_handles_a_large_payload() {
    let payload = "A".repeat(4 * 1024 * 1024);
    let body = user_images(&format!(
        r#"{{"type":"input_image","image_url":"data:image/png;base64,{payload}"}}"#
    ));

    assert!(RequestCost::measure(&body, true).text_tokens < 100);
}

#[test]
fn provider_request_measurement_learns_the_prior_exact_token_density() {
    let current = cost(1_456_988, 365_113);
    let calibrated = current.calibrated(&calibration(cost(767_736, 192_000), 398_710));
    assert!(calibrated.estimated_tokens > 695_142);
    assert!(calibrated.estimated_tokens >= current.estimated_tokens);
}

#[test]
fn provider_usage_corrects_a_serialized_estimate_downward() {
    let cost = cost(34_210, 8_892);
    let calibrated = cost.calibrated(&calibration(cost, 6_030));
    assert_eq!(calibrated.estimated_tokens, 6_030);
    assert_eq!(calibrated.text_tokens, cost.text_tokens);
}

#[test]
fn a_calibration_without_bytes_or_exact_tokens_changes_nothing() {
    let measured = RequestCost::measure("one two three", false);
    assert_eq!(measured.calibrated(&calibration(cost(0, 0), 10)), measured);
    assert_eq!(measured.calibrated(&calibration(measured, 0)), measured);
    assert_eq!(
        RequestCost::measure("abcd", false).calibrated(&calibration(cost(1_000_000, 1), 1)),
        cost(4, 1)
    );
}

#[test]
fn provider_request_image_calibration_uses_exact_usage_plus_text_growth_without_compounding() {
    let first = RequestCost {
        image_identity: Some([1; 32]),
        ..cost(4_000_000, 100)
    };
    let mut next = RequestCost {
        bytes: first.bytes + 200,
        ..RequestCost {
            image_identity: first.image_identity,
            ..cost(0, 150)
        }
    };
    let calibrated = next.calibrated(&calibration(first, 1000));
    assert_eq!(calibrated.estimated_tokens, 1050);
    assert_eq!(calibrated.text_tokens, 150);
    let third = RequestCost {
        text_tokens: 200,
        estimated_tokens: 200,
        ..next
    };
    assert_eq!(
        third
            .calibrated(&calibration(calibrated, 1050))
            .estimated_tokens,
        1100
    );
    assert_eq!(
        first
            .calibrated(&calibration(calibrated, 1050))
            .estimated_tokens,
        1000
    );

    next.image_identity = Some([2; 32]);
    assert_eq!(next.calibrated(&calibration(first, 1000)), next);
    next.image_identity = None;
    assert_eq!(next.calibrated(&calibration(first, 1000)), next);
    assert_eq!(first.calibrated(&calibration(next, 1000)), first);
    assert_eq!(first.calibrated(&calibration(first, 0)), first);
    assert_eq!(
        calibrated
            .calibrated(&calibration(third, 1))
            .estimated_tokens,
        150
    );
    assert_eq!(
        calibrated
            .calibrated(&calibration(first, usize::MAX))
            .estimated_tokens,
        usize::MAX
    );
}
