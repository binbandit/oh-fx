use std::sync::Arc;

use ofx_contract::{
    ApprovalOrigin, ApprovalScope, CallDescription, Concurrency, PathAccess, ToolActivity,
    ToolCallId, ToolEffect,
};

use super::*;

type ShownRequests = Arc<Mutex<Vec<(TurnId, RequestId, ApprovalOrigin)>>>;

fn request(id: u64, origin: ApprovalOrigin) -> ApprovalRequest {
    ApprovalRequest {
        id: RequestId::new(id),
        call_id: ToolCallId::new(format!("call-{id}")),
        tool_name: "read_file".to_owned(),
        description: CallDescription {
            title: "Reading ../notes.txt".to_owned(),
            label: None,
            activity: ToolActivity::Read,
            effect: ToolEffect::ReadOnly,
            concurrency: Concurrency::Parallel,
        },
        tool_arguments_preview: String::new(),
        tool_arguments_truncated: false,
        scope: ApprovalScope {
            target: None,
            access: PathAccess::WorkspaceOnly,
            always: None,
        },
        command: None,
        file: None,
        change: None,
        origin,
    }
}

fn child(id: u64, child: &str) -> ApprovalRequest {
    request(id, ApprovalOrigin::Subagent(child.to_owned()))
}

fn own(id: u64) -> ApprovalRequest {
    request(id, ApprovalOrigin::ActiveSession)
}

fn queue() -> (ApprovalQueue, ShownRequests) {
    let shown = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&shown);
    let queue = ApprovalQueue::default();
    queue.attach(Arc::new(move |event| {
        if let UiEvent::ApprovalRequested { turn_id, request } = event {
            seen.lock()
                .unwrap()
                .push((turn_id, request.id, request.origin));
        }
    }));
    (queue, shown)
}

fn subagent(child: &str) -> ApprovalOrigin {
    ApprovalOrigin::Subagent(child.to_owned())
}

#[test]
fn child_requests_reach_the_prompt_one_at_a_time_in_arrival_order() {
    let (queue, shown) = queue();
    let turn = TurnId::new(4);
    queue.turn_started(turn);
    queue.child(Some(turn), child(1, "1"));
    queue.child(Some(turn), child(2, "2"));
    queue.child(Some(turn), child(3, "1"));
    assert_eq!(
        *shown.lock().unwrap(),
        [(turn, RequestId::new(1), subagent("1"))]
    );
    queue.resolve(RequestId::new(1), ApprovalDecision::Once);
    queue.resolve(RequestId::new(2), ApprovalDecision::Deny);
    assert_eq!(
        *shown.lock().unwrap(),
        [
            (turn, RequestId::new(1), subagent("1")),
            (turn, RequestId::new(2), subagent("2")),
            (turn, RequestId::new(3), subagent("1")),
        ]
    );
}

#[test]
fn the_parents_own_request_goes_ahead_of_waiting_children() {
    let (queue, shown) = queue();
    let turn = TurnId::new(1);
    queue.turn_started(turn);
    queue.child(Some(turn), child(1, "1"));
    queue.child(Some(turn), child(2, "2"));
    queue.own(turn, own(3));
    queue.resolve(RequestId::new(1), ApprovalDecision::Once);
    queue.resolve(RequestId::new(3), ApprovalDecision::Once);
    assert_eq!(
        *shown.lock().unwrap(),
        [
            (turn, RequestId::new(1), subagent("1")),
            (turn, RequestId::new(3), ApprovalOrigin::ActiveSession),
            (turn, RequestId::new(2), subagent("2")),
        ]
    );
}

#[test]
fn requests_outside_a_running_turn_or_left_waiting_at_its_end_are_never_shown() {
    let (queue, shown) = queue();
    queue.child(None, child(1, "1"));
    let turn = TurnId::new(2);
    queue.turn_started(turn);
    queue.child(Some(turn), child(2, "1"));
    queue.child(Some(turn), child(3, "2"));
    queue.turn_finished();
    queue.resolve(RequestId::new(2), ApprovalDecision::Deny);
    let next = TurnId::new(3);
    queue.turn_started(next);
    queue.own(next, own(4));
    assert_eq!(
        *shown.lock().unwrap(),
        [
            (turn, RequestId::new(2), subagent("1")),
            (next, RequestId::new(4), ApprovalOrigin::ActiveSession),
        ]
    );
}

#[test]
fn an_answer_to_a_waiting_request_drops_it_from_the_queue() {
    let (queue, shown) = queue();
    let turn = TurnId::new(1);
    queue.turn_started(turn);
    queue.child(Some(turn), child(1, "1"));
    queue.child(Some(turn), child(2, "2"));
    queue.child(Some(turn), child(3, "3"));
    queue.resolve(RequestId::new(2), ApprovalDecision::Deny);
    queue.resolve(RequestId::new(1), ApprovalDecision::Once);
    assert_eq!(
        *shown.lock().unwrap(),
        [
            (turn, RequestId::new(1), subagent("1")),
            (turn, RequestId::new(3), subagent("3"))
        ]
    );
}

#[test]
fn a_childs_request_from_an_earlier_turn_is_never_shown_in_a_later_one() {
    let (queue, shown) = queue();
    let earlier = TurnId::new(1);
    queue.turn_started(earlier);
    queue.turn_finished();
    let turn = TurnId::new(2);
    queue.turn_started(turn);
    queue.child(Some(earlier), child(1, "1"));
    queue.child(None, child(2, "1"));
    queue.own(turn, own(3));
    assert_eq!(
        *shown.lock().unwrap(),
        [(turn, RequestId::new(3), ApprovalOrigin::ActiveSession)]
    );
}

#[test]
fn a_withdrawn_request_leaves_the_queue_and_the_next_one_shows() {
    let (queue, shown) = queue();
    let turn = TurnId::new(1);
    queue.turn_started(turn);
    queue.child(Some(turn), child(1, "1"));
    queue.child(Some(turn), child(2, "2"));
    queue.own(turn, own(3));
    queue.withdrawn(RequestId::new(1));
    queue.withdrawn(RequestId::new(2));
    queue.resolve(RequestId::new(3), ApprovalDecision::Once);
    queue.child(Some(turn), child(4, "1"));
    assert_eq!(
        *shown.lock().unwrap(),
        [
            (turn, RequestId::new(1), subagent("1")),
            (turn, RequestId::new(3), ApprovalOrigin::ActiveSession),
            (turn, RequestId::new(4), subagent("1")),
        ]
    );
}

#[test]
fn a_childs_feedback_reaches_the_transcript_only_during_its_turn() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&events);
    let queue = ApprovalQueue::default();
    queue.attach(Arc::new(move |event| seen.lock().unwrap().push(event)));
    queue.child_feedback(Some(TurnId::new(1)), "too early".to_owned());
    queue.turn_started(TurnId::new(2));
    queue.child_feedback(Some(TurnId::new(1)), "stale".to_owned());
    queue.child_feedback(Some(TurnId::new(2)), "then stop".to_owned());
    assert_eq!(
        *events.lock().unwrap(),
        [UiEvent::ApprovalFeedback {
            turn_id: TurnId::new(2),
            text: "then stop".to_owned(),
        }]
    );
}
