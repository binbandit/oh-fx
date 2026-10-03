use super::*;

fn work(id: &str, fingerprint: u8) -> ActiveWork {
    ActiveWork {
        id: id.to_owned(),
        request_fingerprint: [fingerprint; 32],
        message: "do the work".to_owned(),
        permission_mode: PermissionMode::Auto,
    }
}

#[test]
fn subagent_failure_state_is_replaced_only_by_matching_work() {
    let mut registry = Registry::default();
    registry
        .append_persistent("child", "reviewer", "", work("work-1", 1))
        .unwrap();
    let failure = ModelFailureDiagnostic::new("agent_turn_failed: SessionCommitFailed");
    assert_eq!(
        registry
            .finish("child", "other", Outcome::Failed, Some(failure.clone()))
            .err(),
        Some(RegistryError::StaleWork)
    );
    registry
        .finish("child", "work-1", Outcome::Failed, Some(failure.clone()))
        .unwrap();
    assert_eq!(
        registry.find_by_id("child").unwrap().last_failure,
        Some(failure.clone())
    );

    registry
        .start_persistent_work("reviewer", None, work("work-2", 2))
        .unwrap();
    assert_eq!(
        registry
            .finish("child", "work-1", Outcome::Failed, Some(failure.clone()))
            .err(),
        Some(RegistryError::StaleWork)
    );
    assert_eq!(
        registry
            .finish("child", "work-2", Outcome::Completed, Some(failure))
            .err(),
        Some(RegistryError::InvalidState)
    );
    let finished = registry
        .finish("child", "work-2", Outcome::Completed, None)
        .unwrap();
    assert_eq!(finished.last_failure, None);
    assert_eq!(finished.last_work_id.as_deref(), Some("work-2"));
    assert_eq!(finished.last_request_fingerprint, Some([2; 32]));
}

#[test]
fn persistent_state_derives_create_continue_busy_and_terminal_transitions() {
    let mut registry = Registry::default();
    registry
        .append_persistent("child", "reviewer", "Review carefully.", work("work-1", 1))
        .unwrap();
    assert_eq!(
        registry
            .start_persistent_work(
                "reviewer",
                Some("Must not replace while busy."),
                work("work-1b", 1)
            )
            .err(),
        Some(RegistryError::ChildBusy)
    );
    assert_eq!(
        registry.find_by_id("child").unwrap().instructions(),
        "Review carefully."
    );
    registry
        .finish("child", "work-1", Outcome::Completed, None)
        .unwrap();
    let continued = registry
        .start_persistent_work("reviewer", None, work("work-2", 2))
        .unwrap();
    assert_eq!(continued.phase, ChildPhase::Running);
    assert_eq!(continued.instructions(), "Review carefully.");
    registry
        .finish("child", "work-2", Outcome::Completed, None)
        .unwrap();
    let replaced = registry
        .start_persistent_work("reviewer", Some("Audit security only."), work("work-3", 3))
        .unwrap();
    assert_eq!(replaced.instructions(), "Audit security only.");
    assert_eq!(
        registry
            .start_persistent_work("writer", None, work("work-4", 4))
            .err(),
        Some(RegistryError::ChildNotFound)
    );
}

#[test]
fn one_off_children_finish_and_operations_resolve_to_their_child() {
    let mut registry = Registry::default();
    registry.append_one_off("one", work("work-1", 7)).unwrap();
    let running = registry.find_by_operation("work-1").unwrap();
    assert_eq!(running.id, "one");
    assert_eq!(running.operation_fingerprint("work-1"), Some([7; 32]));
    assert_eq!(running.operation_fingerprint("work-2"), None);
    let finished = registry
        .finish("one", "work-1", Outcome::Cancelled, None)
        .unwrap();
    assert_eq!(finished.phase, ChildPhase::Finished);
    assert_eq!(finished.last_outcome, Some(Outcome::Cancelled));
    assert_eq!(
        registry
            .find_by_operation("work-1")
            .unwrap()
            .operation_fingerprint("work-1"),
        Some([7; 32])
    );
    assert!(registry.find_by_operation("work-2").is_none());
    assert!(registry.find_persistent("one").is_none());
}

#[test]
fn appends_reject_duplicates_invalid_identities_and_a_full_registry() {
    let mut registry = Registry::default();
    registry.append_one_off("child", work("work-1", 1)).unwrap();
    assert_eq!(
        registry.append_one_off("child", work("work-2", 2)),
        Err(RegistryError::ChildAlreadyExists)
    );
    registry
        .append_persistent("named", "reviewer", "", work("work-3", 3))
        .unwrap();
    assert_eq!(
        registry.append_persistent("other", "reviewer", "", work("work-4", 4)),
        Err(RegistryError::AgentAlreadyExists)
    );
    assert_eq!(
        registry.append_persistent("bad", "Reviewer", "", work("work-5", 5)),
        Err(RegistryError::InvalidState)
    );
    assert_eq!(
        registry
            .start_persistent_work("reviewer", Some(""), work("work-6", 6))
            .err(),
        Some(RegistryError::InvalidState)
    );
    let mut full = Registry::default();
    for index in 0..MAX_CHILDREN {
        full.append_one_off(&index.to_string(), work(&format!("w{index}"), 0))
            .unwrap();
    }
    assert_eq!(
        full.append_one_off("overflow", work("overflow", 0)),
        Err(RegistryError::CapacityExceeded)
    );
}
