use ofx_contract::{ProviderErrorKind, ToolCallId};

use super::*;

fn provider(code: &str, status: Option<u16>) -> Result<String, Stop> {
    Err(Stop::failed(TurnFailure::Provider(ProviderError {
        status,
        ..ProviderError::new(ProviderErrorKind::ProviderError, code)
    })))
}

fn trail(completed: &[&str], active: Option<&str>) -> ToolTrail {
    ToolTrail {
        completed: completed.iter().map(|name| (*name).to_owned()).collect(),
        active: active.map(|name| (ToolCallId::new("call_1"), name.to_owned())),
        ..ToolTrail::default()
    }
}

#[test]
fn each_turn_ending_has_upstreams_outcome_kind() {
    let quiet = ToolTrail::default();
    let kind = |result: &Result<String, Stop>| outcome_kind(result, &quiet, false);
    assert_eq!(kind(&Ok("done".to_owned())), "assistant");
    assert_eq!(kind(&Err(Stop::interrupted())), "interrupted");
    assert_eq!(
        kind(&Err(Stop::failed(TurnFailure::StepLimitReached))),
        "step_limit"
    );
    assert_eq!(
        kind(&Err(Stop::failed(TurnFailure::InvalidCompletion))),
        "invalid_tool_finish"
    );
    assert_eq!(
        kind(&Err(Stop::failed(TurnFailure::MalformedProviderArguments))),
        "malformed_provider_tool_arguments"
    );
    assert_eq!(kind(&provider("OutputTruncated", None)), "provider_length");
    assert_eq!(kind(&provider("ContentFiltered", None)), "content_filter");
    assert_eq!(kind(&provider("ProviderError", None)), "provider_error");
    assert_eq!(kind(&provider("ProviderError", Some(418))), "http_error");
    assert_eq!(
        kind(&provider("IncompleteStream", None)),
        "stream_interrupted"
    );
    assert_eq!(
        kind(&provider("StreamInterrupted", None)),
        "stream_interrupted"
    );
    assert_eq!(kind(&provider("BadRequest", Some(400))), "http_error");
    let persistence = Err(Stop::failed(TurnFailure::Persistence(
        ofx_contract::LogFailure {
            code: "Io".to_owned(),
        },
    )));
    assert_eq!(kind(&persistence), "http_error");
    assert_eq!(outcome_kind(&persistence, &quiet, true), "cancelled");
    let after_tools = ToolTrail {
        ran_tools: true,
        ..ToolTrail::default()
    };
    assert_eq!(outcome_kind(&persistence, &after_tools, true), "error");
    assert_eq!(
        outcome_kind(&provider("BadRequest", Some(400)), &after_tools, false),
        "error"
    );
    assert_eq!(
        outcome_kind(&provider("OutputTruncated", None), &after_tools, false),
        "provider_length"
    );
    let stalled = ToolTrail {
        finish: Some("recovery_stalled"),
        ..ToolTrail::default()
    };
    assert_eq!(
        outcome_kind(&provider("Timeout", None), &stalled, false),
        "recovery_stalled"
    );
    let handed_off = ToolTrail {
        finish: Some("steering_handoff"),
        ..ToolTrail::default()
    };
    assert_eq!(
        outcome_kind(&Err(Stop::interrupted()), &handed_off, false),
        "steering_handoff"
    );
}

#[test]
fn an_interruption_names_what_it_interrupted() {
    let reason = |trail: &ToolTrail, partial: &str| {
        interrupt_reason(&Interrupted {
            prompt: "p",
            partial_bytes: partial.len(),
            trail,
        })
    };
    assert_eq!(
        reason(&trail(&["read_file"], Some("shell")), "text"),
        "active_tool_call_present"
    );
    assert_eq!(
        reason(&trail(&["read_file"], None), "text"),
        "completed_tools_present"
    );
    assert_eq!(reason(&trail(&[], None), "text"), "partial_assistant_only");
    assert_eq!(reason(&trail(&[], None), ""), "no_assistant_output");
}

#[test]
fn a_step_is_entered_once_however_often_it_is_attempted() {
    let mut trail = ToolTrail::default();
    assert!(trail.enter_step(0));
    assert!(!trail.enter_step(0));
    assert!(trail.enter_step(1));
}

#[test]
fn completed_tool_names_are_joined_or_none() {
    assert_eq!(trail(&[], None).completed_names(), "none");
    assert_eq!(
        trail(&["read_file", "glob_files"], None).completed_names(),
        "read_file,glob_files"
    );
}

#[test]
fn finish_reasons_use_upstreams_labels() {
    assert_eq!(finish_reason_label(FinishReason::Stop), "stop");
    assert_eq!(finish_reason_label(FinishReason::ToolCalls), "tool-calls");
}

#[test]
fn a_turn_context_drops_the_step() {
    let context = TraceContext {
        turn_id: 4,
        step_id: 9,
        subagent_id: 2,
    };
    assert_eq!(
        turn_context(context),
        TraceContext {
            turn_id: 4,
            step_id: 0,
            subagent_id: 2,
        }
    );
}
