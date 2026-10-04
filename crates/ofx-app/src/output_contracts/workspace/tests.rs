use std::path::PathBuf;

use ofx_workspace::DirectorySource;

use super::*;

fn directory(path: &str, saved: bool, command_line: bool, available: bool) -> AdditionalDirectory {
    AdditionalDirectory {
        path: PathBuf::from(path),
        source: DirectorySource {
            saved,
            command_line,
        },
        available,
        active: available,
    }
}

fn mutation(path: &str, launch_flag_can_restore: bool) -> Mutation {
    Mutation {
        action: "remove",
        path: Some(path.to_owned()),
        saved_changed: !launch_flag_can_restore,
        runtime_changed: true,
        launch_flag_can_restore,
    }
}

#[test]
fn a_mutation_line_precedes_the_listing_and_untrusted_paths_are_encoded() {
    let entries = [
        directory("/shared\u{1b}[2J", true, false, true),
        directory("/gone", true, false, false),
    ];
    let mutation = Mutation {
        action: "add",
        path: Some("../shared\u{1b}[2J".to_owned()),
        saved_changed: true,
        runtime_changed: false,
        launch_flag_can_restore: false,
    };
    let snapshot = WorkspaceSnapshot {
        primary_directory: Path::new("/work"),
        saved_suppressed: false,
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
fn the_snapshot_renders_each_source_and_warns_when_a_launch_flag_can_restore_access() {
    let entries = [
        directory("/tmp/shared", true, false, true),
        directory("/tmp/run-only", false, true, true),
    ];
    let restorable = mutation("/tmp/removed", true);
    let mut snapshot = WorkspaceSnapshot {
        primary_directory: Path::new("/tmp/project"),
        saved_suppressed: true,
        additional_directories: &entries,
        mutation: Some(&restorable),
    };
    assert_eq!(
        snapshot.render(OutputFormat::Text),
        "[workspace] primary=/tmp/project\n[workspace] saved_suppressed=true limit=16\n[workspace] remove /tmp/removed saved_changed=false runtime_changed=true launch_flag_can_restore=true\n[workspace] warning: repeating --add-dir can restore removed access on the next launch\n[workspace] additional directories:\n - /tmp/shared saved=true command_line=false available=true active=true\n - /tmp/run-only saved=false command_line=true available=true active=true\n"
    );
    assert_eq!(
        snapshot.render(OutputFormat::Json),
        "{\"kind\":\"workspace\",\"action\":\"remove\",\"changed\":true,\"primary_directory\":\"/tmp/project\",\"saved_suppressed\":true,\"limit\":16,\"path\":\"/tmp/removed\",\"saved_changed\":false,\"runtime_changed\":true,\"launch_flag_can_restore\":true,\"additional_directories\":[{\"path\":\"/tmp/shared\",\"saved\":true,\"command_line\":false,\"available\":true,\"active\":true},{\"path\":\"/tmp/run-only\",\"saved\":false,\"command_line\":true,\"available\":true,\"active\":true}]}\n"
    );
    let saved_only = mutation("/tmp/shared", false);
    snapshot.mutation = Some(&saved_only);
    let text = snapshot.render(OutputFormat::Text);
    assert!(text.contains("launch_flag_can_restore=false\n"), "{text}");
    assert!(!text.contains("warning: repeating --add-dir"), "{text}");
    assert!(
        snapshot
            .render(OutputFormat::Json)
            .contains("\"launch_flag_can_restore\":false")
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
