use super::*;

fn entries() -> Vec<Entry> {
    vec![
        Entry {
            path: "/shared\u{1b}[2J".to_owned(),
            available: true,
        },
        Entry {
            path: "/gone".to_owned(),
            available: false,
        },
    ]
}

#[test]
fn a_mutation_line_precedes_the_listing_and_untrusted_paths_are_encoded() {
    let entries = entries();
    let mutation = Mutation {
        action: "add",
        path: Some("../shared\u{1b}[2J".to_owned()),
        saved_changed: true,
        runtime_changed: false,
    };
    let snapshot = WorkspaceSnapshot {
        primary_directory: Path::new("/work"),
        additional_directories: &entries,
        mutation: Some(&mutation),
    };
    assert_eq!(
        snapshot.render(OutputFormat::Text),
        "[workspace] primary=/work\n[workspace] saved_suppressed=false limit=16\n[workspace] add ../shared\\x1b[2J saved_changed=true runtime_changed=false launch_flag_can_restore=false\n[workspace] additional directories:\n - /shared\\x1b[2J saved=true command_line=false available=true active=true\n - /gone saved=true command_line=false available=false active=false\n"
    );
    assert_eq!(
        snapshot.render(OutputFormat::Json),
        "{\"kind\":\"workspace\",\"action\":\"add\",\"changed\":true,\"primary_directory\":\"/work\",\"saved_suppressed\":false,\"limit\":16,\"path\":\"../shared\\u001b[2J\",\"saved_changed\":true,\"runtime_changed\":false,\"launch_flag_can_restore\":false,\"additional_directories\":[{\"path\":\"/shared\\u001b[2J\",\"saved\":true,\"command_line\":false,\"available\":true,\"active\":true},{\"path\":\"/gone\",\"saved\":true,\"command_line\":false,\"available\":false,\"active\":false}]}\n"
    );
}

#[test]
fn error_codes_map_to_upstreams_messages() {
    assert_eq!(
        workspace_error_message("TooManyDirectories"),
        "additional directory limit reached"
    );
    assert_eq!(
        workspace_error_message("PrivateStatePermissionsUnsupported"),
        "settings path is unsafe"
    );
    assert_eq!(
        workspace_error_message("SettingsLockBusy"),
        "workspace update failed"
    );
}
