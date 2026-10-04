use super::*;

fn work(id: &str, fingerprint: u8) -> ActiveWork {
    ActiveWork {
        id: id.to_owned(),
        request_fingerprint: [fingerprint; 32],
        message: "do the work".to_owned(),
        root_user_requests: Arc::default(),
        root_user_context: String::new(),
        permission_mode: PermissionMode::Auto,
        created_at_ms: 1,
    }
}

#[test]
fn the_registry_is_written_as_upstream_writes_children_json() {
    let mut registry = Registry::default();
    registry
        .append_one_off("child-1", work("work-1", 1))
        .unwrap();
    registry
        .finish(
            "child-1",
            "work-1",
            Outcome::Failed,
            Some(ModelFailureDiagnostic::new("agent_turn_failed: Boom")),
        )
        .unwrap();
    let mut active = work("work-2", 0xab);
    active.message = "review \"this\"\n".to_owned();
    active.root_user_context = "current_request: review it\n".to_owned();
    active.permission_mode = PermissionMode::Yolo;
    active.created_at_ms = 1_700_000_000_000;
    registry
        .append_persistent("child-2", "reviewer", "Be terse.", active)
        .unwrap();
    let one = "01".repeat(32);
    let ab = "ab".repeat(32);
    assert_eq!(
        String::from_utf8(registry.render("parent")).unwrap(),
        format!(
            "{{\"schema_version\":2,\"parent_id\":\"parent\",\"generation\":3,\"children\":[\
            {{\"id\":\"child-1\",\"kind\":\"one_off\",\"persistent\":null,\"phase\":\"finished\",\"work_generation\":1,\"active\":null,\"last_work_id\":\"work-1\",\"last_request_fingerprint\":\"{one}\",\"last_outcome\":\"failed\",\"last_failure\":\"agent_turn_failed: Boom\"}},\
            {{\"id\":\"child-2\",\"kind\":\"persistent\",\"persistent\":{{\"agent\":\"reviewer\",\"instructions\":\"Be terse.\"}},\"phase\":\"running\",\"work_generation\":1,\"active\":{{\"id\":\"work-2\",\"request_fingerprint\":\"{ab}\",\"message\":\"review \\\"this\\\"\\n\",\"root_user_intent_context\":\"current_request: review it\\n\",\"root_user_messages\":[],\"root_user_evidence_complete\":false,\"permission_mode\":\"yolo\",\"created_at_ms\":1700000000000}},\"last_work_id\":null,\"last_request_fingerprint\":null,\"last_outcome\":null,\"last_failure\":null}}\
            ]}}"
        )
    );
}

#[test]
fn every_change_advances_the_generation_and_new_work_its_childs() {
    let mut registry = Registry::default();
    registry
        .append_persistent("child", "reviewer", "", work("work-1", 1))
        .unwrap();
    registry
        .finish("child", "work-1", Outcome::Completed, None)
        .unwrap();
    registry
        .start_persistent_work("reviewer", Some("Be terse."), work("work-2", 2))
        .unwrap();
    let rendered = String::from_utf8(registry.render("parent")).unwrap();
    assert!(rendered.contains("\"generation\":3,"), "{rendered}");
    assert!(rendered.contains("\"work_generation\":2,"), "{rendered}");
    assert!(
        rendered.contains("\"persistent\":{\"agent\":\"reviewer\",\"instructions\":\"Be terse.\"}"),
        "{rendered}"
    );
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

fn sample() -> Registry {
    let mut registry = Registry::default();
    registry
        .append_one_off("child-1", work("work-1", 1))
        .unwrap();
    registry
        .finish(
            "child-1",
            "work-1",
            Outcome::Failed,
            Some(ModelFailureDiagnostic::new("agent_turn_failed: Boom")),
        )
        .unwrap();
    let mut active = work("work-2", 0xab);
    active.root_user_context = "current_request: review it\n".to_owned();
    registry
        .append_persistent("child-2", "reviewer", "Be terse.", active)
        .unwrap();
    registry
}

#[test]
fn a_saved_registry_reads_back_as_it_was_written() {
    let registry = sample();
    let saved = registry.render("parent");
    assert_eq!(Registry::parse(&saved, "parent"), Ok(registry));
    assert_eq!(
        Registry::parse(&saved, "other"),
        Err(RegistryError::InvalidParentId)
    );
}

#[test]
fn a_schema_version_1_registry_reads_without_its_failure() {
    let legacy = concat!(
        "{\"schema_version\":1,\"parent_id\":\"01J00000000000000000000000\",\"generation\":1,\"children\":[",
        "{\"id\":\"01J00000000000000000000001\",\"kind\":\"one_off\",\"persistent\":null,\"phase\":\"finished\",\"work_generation\":1,\"active\":null,\"last_work_id\":\"work-1\",\"last_request_fingerprint\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"last_outcome\":\"failed\"}",
        "]}"
    );
    let registry = Registry::parse(legacy.as_bytes(), "01J00000000000000000000000").unwrap();
    let child = registry.find_by_id("01J00000000000000000000001").unwrap();
    assert_eq!(child.last_failure, None);
    assert_eq!(child.last_outcome, Some(Outcome::Failed));
    assert!(
        String::from_utf8(registry.render("01J00000000000000000000000"))
            .unwrap()
            .contains("\"last_outcome\":\"failed\",\"last_failure\":null}")
    );
}

#[test]
fn registries_upstream_rejects_are_rejected() {
    let saved = String::from_utf8(sample().render("parent")).unwrap();
    let invalid = [
        saved.replace("\"generation\":3,", "\"generation\":3,\"extra\":1,"),
        saved.replace("\"generation\":3,", ""),
        saved.replace("\"generation\":3,", "\"generation\":-1,"),
        saved.replace("\"phase\":\"running\"", "\"phase\":\"idle\""),
        saved.replace("\"phase\":\"finished\"", "\"phase\":\"done\""),
        saved.replace("\"reviewer\"", "\"Reviewer\""),
        saved.replace("\"child-2\"", "\"child-1\""),
        saved.replace("\"child-2\"", "\"..\""),
        saved.replace(
            "\"last_outcome\":\"failed\"",
            "\"last_outcome\":\"completed\"",
        ),
        saved.replace("agent_turn_failed: Boom", "unsafe\\u001b[31m"),
        saved.replace(&"ab".repeat(32), &"ab".repeat(31)),
        saved.replace(
            "\"permission_mode\":\"auto\"",
            "\"permission_mode\":\"full\"",
        ),
        saved.replace("\"created_at_ms\":1", "\"created_at_ms\":1.5"),
        saved.replace(
            "\"kind\":\"one_off\",\"persistent\":null",
            "\"kind\":\"one_off\",\"persistent\":{\"agent\":\"a\",\"instructions\":\"\"}",
        ),
    ];
    for document in invalid {
        assert_eq!(
            Registry::parse(document.as_bytes(), "parent"),
            Err(RegistryError::InvalidState),
            "{document}"
        );
    }
    assert_eq!(
        Registry::parse(
            saved
                .replace("\"schema_version\":2", "\"schema_version\":99")
                .as_bytes(),
            "parent"
        ),
        Err(RegistryError::UnsupportedSchema)
    );
    let crowded = format!(
        "{{\"schema_version\":2,\"parent_id\":\"parent\",\"generation\":0,\"children\":[{}]}}",
        vec!["{}"; 257].join(",")
    );
    assert_eq!(
        Registry::parse(crowded.as_bytes(), "parent"),
        Err(RegistryError::InvalidState)
    );
}

#[test]
fn work_left_running_by_an_earlier_process_is_interrupted() {
    let mut registry = sample();
    registry.interrupt_active();
    let child = registry.find_by_id("child-2").unwrap();
    assert_eq!(child.phase, ChildPhase::Interrupted);
    assert_eq!(child.active, None);
    assert_eq!(child.last_work_id.as_deref(), Some("work-2"));
    assert_eq!(child.last_request_fingerprint, Some([0xab; 32]));
    assert_eq!(child.last_outcome, Some(Outcome::Interrupted));
    assert!(
        String::from_utf8(registry.render("parent"))
            .unwrap()
            .contains("\"generation\":4,")
    );
    let mut settled = registry.clone();
    settled.interrupt_active();
    assert_eq!(settled, registry);
}
