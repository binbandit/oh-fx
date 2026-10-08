use ofx_cli::SLASH_REGISTRY;

#[test]
fn feedback_keeps_upstreams_position_category_and_argument_policy() {
    let commands = SLASH_REGISTRY.commands();
    let index = commands
        .iter()
        .position(|spec| spec.command == "/feedback")
        .unwrap();
    assert_eq!(commands[index - 1].command, "/copy");
    assert_eq!(commands[index + 1].command, "/compact");
    assert_eq!(commands[index].presentation_category.label(), "Product");
    assert_eq!(
        commands[index].completion_description,
        "open the oh-fx issue form"
    );
    assert!(SLASH_REGISTRY.parse_command("/feedback").is_some());
    assert!(SLASH_REGISTRY.parse_command("/feedback extra").is_none());
}
