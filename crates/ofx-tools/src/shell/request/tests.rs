use serde_json::Value;

use super::*;

fn correction(arguments: &str) -> Value {
    let failure = decode(arguments).expect_err(arguments);
    let parsed: Value = serde_json::from_str(&failure).unwrap();
    let detail = &parsed["error"];
    assert_eq!(detail["code"], "invalid_shell_request", "{arguments}");
    assert_eq!(detail["executed"], false, "{arguments}");
    assert!(
        !detail["problems"].as_array().unwrap().is_empty(),
        "{arguments}"
    );
    parsed
}

fn decoded(arguments: &str) -> ShellRequest {
    decode(arguments).unwrap_or_else(|failure| panic!("{arguments}: {failure}"))
}

#[test]
fn shell_request_correction_suggests_only_unambiguous_repairs_without_executing() {
    let cases: [(&str, Option<&str>); 19] = [
        (
            r#"{"command":"sleep 30","timeout_ms":"40000","yield_time_ms":"30000"}"#,
            Some(
                r#"{"action":"run","command":"sleep 30","yield_time_ms":30000,"timeout_ms":40000}"#,
            ),
        ),
        (
            r#"{"request":{"command":"sleep 30"},"yield_time_ms":"30000"}"#,
            Some(r#"{"action":"run","command":"sleep 30","yield_time_ms":30000}"#),
        ),
        (
            r#"{"action":"run","command":"true","background":null}"#,
            Some(r#"{"action":"run","command":"true"}"#),
        ),
        (
            r#"{"request":"{\"action\":\"run\",\"command\":\"true\"}"}"#,
            Some(r#"{"action":"run","command":"true"}"#),
        ),
        (
            r#"{"action":"interact","session_id":"shell-3","yield_time_ms":"1000","unused":null}"#,
            Some(r#"{"action":"interact","yield_time_ms":1000,"session_id":"shell-3"}"#),
        ),
        (r#"{"command":"true","tty":true}"#, None),
        ("{}", None),
        (r#"{"action":null,"command":"true"}"#, None),
        (r#"{"session_id":"shell-3"}"#, None),
        (r#"{"command":"true","session_id":"shell-3"}"#, None),
        (
            r#"{"action":"run","command":"true","background":true}"#,
            None,
        ),
        (
            r#"{"action":"run","command":"true","profile":"clean","shell":{"kind":"executable","path":"/bin/bash"}}"#,
            None,
        ),
        (
            r#"{"action":"run","command":"true","yield_time_ms":30001}"#,
            None,
        ),
        (
            r#"{"request":{"action":"run","command":"true","yield_time_ms":"1000","timeout_ms":0}}"#,
            None,
        ),
        (
            r#"{"action":"run","command":"true","yield_time_ms":4294967296}"#,
            None,
        ),
        (
            r#"{"action":"stop","session_id":"shell-3","force":"true"}"#,
            None,
        ),
        (
            r#"{"request":{"action":"run","command":"true"},"command":"false"}"#,
            None,
        ),
        ("{", None),
        ("[]", None),
    ];
    for (arguments, retry) in cases {
        let parsed = correction(arguments);
        let detail = &parsed["error"];
        if let Some(expected) = retry {
            assert_eq!(
                detail["instruction"], "Call shell once using retry_with exactly.",
                "{arguments}"
            );
            let request = detail["retry_with"]["request"].to_string();
            assert_eq!(request, expected, "{arguments}");
            assert!(decode(&request).is_ok(), "{request}");
        } else {
            assert!(detail.get("retry_with").is_none(), "{arguments}");
            assert!(detail.get("instruction").is_none(), "{arguments}");
        }
    }
}

#[test]
fn shell_request_corrections_explain_each_problem() {
    let problems = |arguments: &str| correction(arguments)["error"]["problems"].clone();
    assert_eq!(
        problems(r#"{"request":{"command":"sleep 30"},"yield_time_ms":"30000"}"#),
        serde_json::json!([
            "Only request is allowed at the top level; put action fields inside request.",
            "request.action is required.",
            "request.yield_time_ms must be an integer."
        ])
    );
    assert_eq!(
        problems(r#"{"action":"interact","command":"true","zeta":1,"alpha":2}"#),
        serde_json::json!([
            "request.alpha is not accepted for interact.",
            "request.command is not accepted for interact.",
            "request.zeta is not accepted for interact.",
            "request.session_id is required."
        ])
    );
    assert_eq!(
        problems(r#"{"command":"true","tty":true}"#),
        serde_json::json!([
            "request.action is required.",
            "Interactive Shell fields require a saved session."
        ])
    );
    assert_eq!(
        problems("{"),
        serde_json::json!(["Shell arguments must be a JSON object."])
    );
    assert_eq!(
        problems("[]"),
        serde_json::json!(["Shell arguments must be one bounded request object."])
    );
    assert_eq!(
        problems(r#"{"request":7}"#),
        serde_json::json!(["request must be one object containing the intended action."])
    );
    assert_eq!(
        problems(r#"{"action":"wait","session_id":"shell-session"}"#),
        serde_json::json!(["request.action must be run, interact, or stop."])
    );
    assert_eq!(
        problems(
            r#"{"action":"run","command":"true","profile":"clean","shell":{"kind":"executable","path":"/bin/bash"}}"#
        ),
        serde_json::json!([
            "Choose either request.profile or request.shell.",
            "Choose either request.profile or request.shell.",
            "Interactive Shell fields require a saved session."
        ])
    );
}

#[test]
fn shell_timeout_minimum_is_enforced_before_correction_and_execution() {
    let parsed = correction(r#"{"action":"run","command":"true","timeout_ms":0}"#);
    assert!(parsed["error"].get("retry_with").is_none());
    for arguments in [
        r#"{"action":"run","command":"true"}"#,
        r#"{"action":"run","command":"true","timeout_ms":1}"#,
    ] {
        decoded(arguments);
    }
}

#[test]
fn shell_request_correction_canonicalizes_nested_shell_members() {
    let first = decode(r#"{"command":"true","tty":true,"shell":{"kind":"executable","path":"/bin/bash","clean_start":false}}"#).unwrap_err();
    let reordered = decode(r#"{"shell":{"clean_start":false,"path":"/bin/bash","kind":"executable"},"tty":true,"command":"true"}"#).unwrap_err();
    assert_eq!(first, reordered);
}

#[test]
fn shell_request_correction_bounds_feedback_and_preserves_input_bytes() {
    let command = "printf '\u{1f308}\\n'; echo \"$VALUE\"";
    let source = serde_json::json!({"command": command, "x": null}).to_string();
    let parsed = correction(&source);
    assert_eq!(parsed["error"]["retry_with"]["request"]["command"], command);

    let key = format!("{}\u{1f308}", "x".repeat(63));
    let bounded = decode(&format!(r#"{{"command":"true","{key}":null}}"#)).unwrap_err();
    assert!(bounded.contains(&format!("request.{} is not accepted", "x".repeat(63))));
    let too_large = " ".repeat(16 * 1024 + 1);
    let failure = decode(&too_large).unwrap_err();
    assert!(failure.len() < 512);
    assert!(!failure.contains("retry_with"));
}

#[test]
fn shell_decoder_preserves_null_omission_and_rejects_cross_action_fields() {
    let request = decoded(
        r#"{"action":"run","command":"true","cwd":null,"profile":null,"tty":false,"yield_time_ms":0,"timeout_ms":null}"#,
    );
    assert_eq!(request.action, Action::Run);
    assert!(!request.tty);
    assert_eq!(request.yield_time_ms, 0);
    assert_eq!(request.cwd, None);
    let placeholders =
        decoded(r#"{"action":"run","command":"true","cwd":" Null ","profile":"null"}"#);
    assert_eq!(placeholders.cwd, None);
    assert_eq!(placeholders.profile, None);
    assert!(
        decode(r#"{"action":"interact","session_id":"shell-session","command":"true"}"#)
            .unwrap_err()
            .contains("invalid_shell_request")
    );
}

#[test]
fn shell_decoder_applies_action_specific_observation_defaults() {
    assert_eq!(
        decoded(r#"{"action":"run","command":"true"}"#).yield_time_ms,
        30_000
    );
    assert_eq!(
        decoded(r#"{"action":"interact","session_id":"shell-session"}"#).yield_time_ms,
        5_000
    );
    assert_eq!(
        decoded(r#"{"action":"stop","session_id":"shell-session"}"#).yield_time_ms,
        0
    );
}

#[test]
fn shell_decoder_coerces_values_the_way_upstream_parses_them() {
    let request = decoded(
        r#"{"action":"run","command":"true","yield_time_ms":"1_000","timeout_ms":2e3,"profile":0}"#,
    );
    assert_eq!(request.yield_time_ms, 1_000);
    assert_eq!(request.timeout_ms, Some(2_000));
    assert_eq!(request.profile, Some(Profile::Clean));
    assert_eq!(
        decoded(r#"{"action":"run","command":"true","yield_time_ms":"+2","profile":"1"}"#).profile,
        Some(Profile::User)
    );
    assert_eq!(
        decoded(r#"{"action":"run","command":"true","yield_time_ms":-0.0}"#).yield_time_ms,
        0
    );
    for arguments in [
        r#"{"action":"run","command":"true","yield_time_ms":1.5}"#,
        r#"{"action":"run","command":"true","yield_time_ms":-1}"#,
        r#"{"action":"run","command":"true","yield_time_ms":"nope"}"#,
        r#"{"action":"run","command":"true","profile":"strict"}"#,
        r#"{"action":"run","command":"true","profile":2}"#,
        r#"{"action":"run","command":"true","tty":"false"}"#,
        r#"{"action":"run","command":7}"#,
        r#"{"action":"run","command":""}"#,
        r#"{"action":"run","command":"true","command":"false"}"#,
    ] {
        assert!(decode(arguments).is_err(), "{arguments}");
    }
}

#[test]
fn shell_decoder_reads_a_shell_object_encoded_as_a_string() {
    let request = decoded(
        r#"{"action":"run","command":"true","tty":true,"shell":"{\"kind\":\"executable\",\"path\":\"/bin/bash\"}"}"#,
    );
    assert!(request.has_shell);
    assert!(request.tty);
    for shell in [
        r#""{\"kind\":\"script\",\"path\":\"/bin/bash\"}""#,
        r#""[]""#,
        r#"{"kind":"executable"}"#,
        r#"{"kind":"executable","path":"/bin/bash","extra":1}"#,
        r#"{"kind":"executable","path":"/bin/bash","clean_start":"yes"}"#,
    ] {
        let arguments =
            format!(r#"{{"action":"run","command":"true","tty":true,"shell":{shell}}}"#);
        assert!(decode(&arguments).is_err(), "{arguments}");
    }
}

#[test]
fn shell_interaction_requests_bound_their_waits_and_input() {
    assert!(
        decode(r#"{"action":"interact","session_id":"shell-1","yield_time_ms":300001}"#)
            .unwrap_err()
            .contains("between 0 and 300000")
    );
    assert!(
        decode(r#"{"action":"interact","session_id":"shell-1","yield_time_ms":4294967296}"#)
            .is_err()
    );
    let oversized = serde_json::json!({
        "action": "interact",
        "session_id": "shell-1",
        "chars": "x".repeat(MAX_WRITE_BYTES + 1),
    })
    .to_string();
    assert!(decode(&oversized).is_err());
    assert!(!decoded(r#"{"action":"interact","session_id":"shell-1"}"#).has_input());
    assert!(!decoded(r#"{"action":"interact","session_id":"shell-1","chars":""}"#).has_input());
    assert!(
        decoded(r#"{"action":"interact","session_id":"shell-1","chars":"hello\n"}"#).has_input()
    );
}

#[test]
fn shell_decoder_rejects_removed_handoff_and_legacy_actions() {
    assert!(
        decode(r#"{"action":"run","command":"sleep 30","yield_time_ms":0,"handoff":"next_turn"}"#)
            .is_err()
    );
    assert!(decode(r#"{"action":"wait","session_id":"shell-session"}"#).is_err());
}

#[test]
fn single_request_wrappers_unwrap_and_everything_else_stays_as_sent() {
    assert_eq!(
        unwrap_request(r#"{"request":{"action":"run","command":"ls"}}"#),
        r#"{"action":"run","command":"ls"}"#
    );
    for arguments in [
        r#"{"request":{"action":"run"},"extra":1}"#,
        r#"{"request":"{}"}"#,
        r#"{"request":{"a":1},"request":{"b":2}}"#,
        r#"{"action":"run","command":"ls"}"#,
        "[]",
        "{",
    ] {
        assert_eq!(unwrap_request(arguments), arguments);
    }
}
