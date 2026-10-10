use ofx_cli::SLASH_REGISTRY;

#[test]
fn trace_keeps_upstreams_position_category_and_argument_policy() {
    let commands = SLASH_REGISTRY.commands();
    let index = commands
        .iter()
        .position(|spec| spec.command == "/trace")
        .unwrap();
    assert_eq!(commands[index - 1].command, "/feedback");
    assert_eq!(commands[index + 1].command, "/compact");
    assert_eq!(commands[index].presentation_category.label(), "Product");
    assert_eq!(
        commands[index].completion_description,
        "copy a private diagnostic trace"
    );
    assert_eq!(commands[index].help_entry, "/trace");
    assert!(SLASH_REGISTRY.parse_command("/trace").is_some());
    assert!(SLASH_REGISTRY.parse_command("/trace extra").is_none());
}
