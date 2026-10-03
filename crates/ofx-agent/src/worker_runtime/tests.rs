use std::path::PathBuf;

use super::*;

fn prompt(id: u64, text: &str) -> QueuedPrompt {
    QueuedPrompt::new(id, text.to_owned(), Vec::new())
}

fn rich(id: u64, text: &str) -> QueuedPrompt {
    QueuedPrompt::new(
        id,
        text.to_owned(),
        vec![SkillBinding {
            name: "review".to_owned(),
            path: PathBuf::from("/skills/review/SKILL.md"),
        }],
    )
}

fn running(texts: &[&str]) -> WorkerRuntime {
    let runtime = WorkerRuntime::default();
    runtime.admit(prompt(0, "turn"));
    runtime.take_next().unwrap();
    for (index, text) in texts.iter().enumerate() {
        runtime.admit(prompt(index as u64 + 1, text));
    }
    runtime
}

fn continued(boundary: Boundary) -> Vec<String> {
    match boundary {
        Boundary::Continue(steering) => steering.into_iter().map(|entry| entry.text).collect(),
        other => panic!("expected steering, got {other:?}"),
    }
}

fn queued(runtime: &WorkerRuntime) -> Vec<(String, bool)> {
    runtime
        .lock()
        .queue
        .iter()
        .map(|prompt| (prompt.text.clone(), prompt.is_continuation()))
        .collect()
}

#[test]
fn a_prompt_submitted_while_a_turn_runs_steers_it() {
    let runtime = running(&["steer"]);
    assert!(runtime.interrupt_requested());
    assert_eq!(
        continued(runtime.take_boundary(BoundaryKind::Cancelled)),
        ["steer"]
    );
    assert!(!runtime.interrupt_requested());
    assert!(queued(&runtime).is_empty());
}

#[test]
fn a_prompt_submitted_while_idle_waits_in_the_ordinary_queue() {
    let runtime = WorkerRuntime::default();
    runtime.admit(prompt(1, "next"));
    assert!(!runtime.interrupt_requested());
    assert_eq!(queued(&runtime), [("next".to_owned(), false)]);
    let next = runtime.take_next().unwrap();
    assert!(!next.is_continuation());
    assert!(!runtime.continues_steering());
}

#[test]
fn a_prompt_during_a_manual_compaction_waits_and_runs_next_as_a_continuation() {
    let runtime = WorkerRuntime::default();
    runtime.begin_compaction();
    runtime.admit(prompt(1, "steer"));
    assert!(!runtime.interrupt_requested());
    runtime.finish_processing();
    let next = runtime.take_next().unwrap();
    assert!(next.is_continuation());
    assert_eq!(next.text, "steer");
    assert!(runtime.continues_steering());
}

#[test]
fn a_prompt_during_an_in_turn_compaction_waits_for_the_next_model_step() {
    let runtime = running(&[]);
    runtime.set_compacting(true);
    runtime.admit(prompt(1, "steer"));
    assert!(!runtime.interrupt_requested());
    assert_eq!(
        continued(runtime.take_boundary(BoundaryKind::Model)),
        ["steer"]
    );
    runtime.set_compacting(false);
    runtime.admit(prompt(2, "now"));
    assert!(runtime.interrupt_requested());
    assert_eq!(
        continued(runtime.take_boundary(BoundaryKind::Cancelled)),
        ["now"]
    );
}

#[test]
fn steering_drains_in_the_order_it_was_typed() {
    let runtime = running(&["first", "second"]);
    let Boundary::Continue(steering) = runtime.take_boundary(BoundaryKind::Cancelled) else {
        panic!("expected steering");
    };
    assert_eq!(
        steering,
        [
            Steering {
                id: 1,
                text: "first".to_owned()
            },
            Steering {
                id: 2,
                text: "second".to_owned()
            }
        ]
    );
}

#[test]
fn an_immediate_steer_cancels_only_the_model_step() {
    let runtime = running(&[]);
    let turn = CancellationToken::new();
    let step = runtime.model_step(&turn);
    runtime.admit(prompt(1, "steer"));
    assert!(step.is_cancelled());
    assert!(!turn.is_cancelled());
    assert_eq!(
        continued(runtime.take_boundary(BoundaryKind::Cancelled)),
        ["steer"]
    );
    let next = runtime.model_step(&turn);
    assert!(!next.is_cancelled());
}

#[test]
fn a_model_step_begun_after_an_immediate_steer_starts_cancelled() {
    let runtime = running(&["steer"]);
    let step = runtime.model_step(&CancellationToken::new());
    assert!(step.is_cancelled());
}

#[test]
fn an_explicit_cancel_overrides_immediate_steering_and_keeps_it_queued() {
    let runtime = running(&["steer"]);
    runtime.request_cancel();
    assert_eq!(
        runtime.take_boundary(BoundaryKind::Cancelled),
        Boundary::Interrupt
    );
    assert_eq!(queued(&runtime), [("steer".to_owned(), false)]);
    runtime.finish_processing();
    let next = runtime.take_next().unwrap();
    assert!(next.is_continuation());
    assert_eq!(next.text, "steer");
}

#[test]
fn a_prompt_after_an_explicit_cancel_starts_a_new_turn() {
    let runtime = running(&[]);
    runtime.request_cancel();
    runtime.admit(prompt(1, "ok"));
    assert_eq!(
        runtime.take_boundary(BoundaryKind::Cancelled),
        Boundary::Interrupt
    );
    runtime.finish_processing();
    let next = runtime.take_next().unwrap();
    assert!(!next.is_continuation());
    assert!(!runtime.interrupt_requested());
}

#[test]
fn a_running_tool_holds_steering_until_the_next_model_step() {
    let runtime = running(&[]);
    runtime.enter_tool_phase();
    runtime.admit(prompt(1, "wait for pending tools"));
    assert!(!runtime.interrupt_requested());
    assert_eq!(
        runtime.take_boundary(BoundaryKind::Cancelled),
        Boundary::None
    );
    assert_eq!(
        continued(runtime.take_boundary(BoundaryKind::Model)),
        ["wait for pending tools"]
    );
    let step = runtime.model_step(&CancellationToken::new());
    runtime.admit(prompt(2, "interrupt next model step"));
    assert!(runtime.interrupt_requested());
    assert!(step.is_cancelled());
}

#[test]
fn rich_steering_hands_off_at_the_tool_boundary() {
    let runtime = running(&[]);
    runtime.enter_tool_phase();
    runtime.admit(rich(1, "review this"));
    assert!(!runtime.interrupt_requested());
    assert_eq!(
        runtime.take_boundary(BoundaryKind::Model),
        Boundary::Handoff
    );
    assert_eq!(
        runtime.take_boundary(BoundaryKind::Finalizing),
        Boundary::Handoff
    );
    assert_eq!(queued(&runtime), [("review this".to_owned(), false)]);
}

#[test]
fn rich_steering_turns_an_immediate_steer_into_an_interrupt() {
    let runtime = running(&["plain steer"]);
    runtime.admit(rich(2, "rich steer"));
    assert_eq!(
        runtime.take_boundary(BoundaryKind::Cancelled),
        Boundary::Interrupt
    );
}

#[test]
fn plain_input_after_a_rich_interrupt_queues_as_a_new_turn() {
    let runtime = running(&[]);
    runtime.admit(rich(1, "rich steer"));
    runtime.admit(prompt(2, "plain next turn"));
    assert_eq!(
        runtime.take_boundary(BoundaryKind::Cancelled),
        Boundary::Interrupt
    );
    runtime.finish_processing();
    assert_eq!(
        queued(&runtime),
        [
            ("rich steer".to_owned(), true),
            ("plain next turn".to_owned(), false)
        ]
    );
}

#[test]
fn a_steer_waiting_at_a_tool_boundary_pops_back_for_editing() {
    let runtime = running(&[]);
    runtime.enter_tool_phase();
    runtime.admit(prompt(1, "first steer"));
    runtime.admit(prompt(2, "second steer"));
    let popped = runtime.pop_queued_steer_for_edit().unwrap();
    assert_eq!((popped.id, popped.text.as_str()), (2, "second steer"));
    assert_eq!(
        continued(runtime.take_boundary(BoundaryKind::Model)),
        ["first steer"]
    );
    assert!(runtime.pop_queued_steer_for_edit().is_none());
}

#[test]
fn an_immediate_steer_already_committed_to_an_interrupt_is_not_retractable() {
    let runtime = running(&["steer"]);
    assert!(runtime.pop_queued_steer_for_edit().is_none());
    assert_eq!(
        continued(runtime.take_boundary(BoundaryKind::Cancelled)),
        ["steer"]
    );
}

#[test]
fn retraction_skips_rich_prompts_and_takes_the_newest_text_steer() {
    let runtime = running(&[]);
    runtime.enter_tool_phase();
    runtime.admit(rich(1, "review this"));
    runtime.admit(prompt(2, "plain text"));
    assert_eq!(
        runtime.pop_queued_steer_for_edit().unwrap().text,
        "plain text"
    );
    assert!(runtime.pop_queued_steer_for_edit().is_none());
    assert_eq!(
        runtime.take_boundary(BoundaryKind::Model),
        Boundary::Handoff
    );
}

#[test]
fn retraction_requires_a_running_turn() {
    let runtime = WorkerRuntime::default();
    runtime.admit(prompt(1, "queued"));
    assert!(runtime.pop_queued_steer_for_edit().is_none());
    assert_eq!(queued(&runtime).len(), 1);
}

#[test]
fn late_steering_keeps_its_admission_order_when_the_turn_ends() {
    let runtime = running(&["steer first"]);
    runtime.request_cancel();
    runtime.admit(prompt(2, "queue second"));
    runtime.finish_processing();
    assert_eq!(
        queued(&runtime),
        [
            ("steer first".to_owned(), true),
            ("queue second".to_owned(), false)
        ]
    );
}

#[test]
fn a_continuation_turn_takes_the_plain_steering_queued_behind_it() {
    let runtime = running(&[]);
    runtime.enter_tool_phase();
    runtime.admit(prompt(1, "first"));
    runtime.admit(prompt(2, "second"));
    runtime.finish_processing();
    let promoted = runtime.take_next().unwrap();
    assert!(promoted.is_continuation());
    assert_eq!(promoted.text, "first");
    assert_eq!(
        continued(runtime.take_boundary(BoundaryKind::Model)),
        ["second"]
    );
}

#[test]
fn a_continuation_turn_leaves_rich_steering_for_its_own_turn() {
    let runtime = running(&[]);
    runtime.enter_tool_phase();
    runtime.admit(prompt(1, "first"));
    runtime.admit(rich(2, "second with a skill"));
    runtime.finish_processing();
    runtime.take_next().unwrap();
    assert_eq!(runtime.take_boundary(BoundaryKind::Model), Boundary::None);
    runtime.finish_processing();
    let trailing = runtime.take_next().unwrap();
    assert!(trailing.is_continuation());
    assert_eq!(trailing.text, "second with a skill");
}

#[test]
fn clearing_the_queue_also_clears_steering() {
    let runtime = running(&[]);
    runtime.enter_tool_phase();
    runtime.admit(prompt(1, "steer"));
    runtime.clear();
    assert_eq!(runtime.take_boundary(BoundaryKind::Model), Boundary::None);
    assert!(runtime.take_next().is_none());
}

#[test]
fn boundaries_outside_a_turn_only_report_a_requested_cancel() {
    let runtime = WorkerRuntime::default();
    assert_eq!(runtime.take_boundary(BoundaryKind::Model), Boundary::None);
    assert_eq!(
        runtime.take_boundary(BoundaryKind::Cancelled),
        Boundary::None
    );
    runtime.request_cancel();
    assert_eq!(
        runtime.take_boundary(BoundaryKind::Cancelled),
        Boundary::Interrupt
    );
}
