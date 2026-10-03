use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use super::*;
use crate::config_runtime::Settings;

const MODEL: &str = "gpt-6.1-sol";

struct Fixture {
    _directory: tempfile::TempDir,
    paths: ProfilePaths,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let paths = ProfilePaths {
            config: root.join("config/oh-fx"),
            data: root.join("data/oh-fx"),
            state: root.join("state/oh-fx"),
            cache: root.join("cache/oh-fx"),
        };
        Self {
            _directory: directory,
            paths,
        }
    }

    fn with_settings(text: &str) -> Self {
        let fixture = Self::new();
        fs::create_dir_all(&fixture.paths.config).unwrap();
        fs::write(fixture.settings(), text).unwrap();
        fixture
    }

    fn settings(&self) -> PathBuf {
        self.paths.config.join(SETTINGS_FILE)
    }

    fn read(&self) -> String {
        fs::read_to_string(self.settings()).unwrap()
    }

    fn save(&self, model: &str) -> Result<(), SettingsWriteError> {
        save_codex_model(&self.paths, model)
    }

    fn copies(&self, kind: &str) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.paths.config.join(BACKUPS_DIRECTORY)) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with(&format!("settings.json.{kind}.")))
            .collect();
        names.sort();
        names
    }
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn missing_settings_are_created_privately_with_the_provider_and_its_model() {
    let fixture = Fixture::new();
    fixture.save(MODEL).unwrap();
    assert_eq!(
        fixture.read(),
        "{\"models\":{\"codex\":\"gpt-6.1-sol\"},\"provider\":\"codex\"}\n"
    );
    assert_eq!(mode(&fixture.paths.config), 0o700);
    assert_eq!(mode(&fixture.settings()), 0o600);
    assert!(fixture.copies("backup").is_empty());
}

#[test]
fn provider_patch_writes_one_provider_model_and_drops_the_legacy_key() {
    let fixture = Fixture::with_settings(
        "{\"model\":\"gateway/model\",\"codex_model\":\"gpt-5.6-terra\",\"provider\":\"gateway\"}",
    );
    fixture.save("gpt-5.6-luna").unwrap();
    assert_eq!(
        fixture.read(),
        "{\"model\":\"gateway/model\",\"provider\":\"codex\",\"models\":{\"codex\":\"gpt-5.6-luna\"}}\n"
    );
}

#[test]
fn unknown_keys_order_and_values_survive_a_compact_rewrite() {
    let fixture = Fixture::with_settings(
        "\u{feff}{\n  \"theme\": \"dark\",\n  \"models\": {\"local\": \"m\"},\n  \"nested\": {\"b\": [1, 2.5, 1.0, -3, 18446744073709551615, true, null], \"a\": \"\\u0041\\n\\u00e9\\\"\"},\n  \"provider\": \"local\",\n  \"providers\": {\"local\": {\"protocol\": \"openai-chat-completions\"}}\n}\n",
    );
    fixture.save(MODEL).unwrap();
    assert_eq!(
        fixture.read(),
        "{\"theme\":\"dark\",\"models\":{\"local\":\"m\",\"codex\":\"gpt-6.1-sol\"},\"nested\":{\"b\":[1,2.5,1,-3,18446744073709551615,true,null],\"a\":\"A\\n\u{e9}\\\"\"},\"provider\":\"codex\",\"providers\":{\"local\":{\"protocol\":\"openai-chat-completions\"}}}\n"
    );
}

#[test]
fn an_unchanged_selection_writes_nothing() {
    let original = "{\n  \"provider\": \"codex\",\n  \"models\": {\"codex\": \"gpt-6.1-sol\"}\n}\n";
    let fixture = Fixture::with_settings(original);
    fixture.save(MODEL).unwrap();
    assert_eq!(fixture.read(), original);
    assert!(fixture.copies("backup").is_empty());
}

#[test]
fn retired_settings_and_fast_mode_bindings_are_removed_with_a_model_change() {
    let fixture = Fixture::with_settings(
        r#"{"input_appearance":"x","maxxing_mode":true,"fast_mode":true,"fast_mode_model_bound":true,"workspaces":{"/a":{"fast_mode_model_bound":true},"/b":{"fast_mode_model_bound":true,"maxxing_mode":1,"permission_mode":"ask"},"/c":"legacy","/d":{"input_appearance":"y"}}}"#,
    );
    fixture.save(MODEL).unwrap();
    assert_eq!(
        fixture.read(),
        "{\"fast_mode\":true,\"workspaces\":{\"/b\":{\"permission_mode\":\"ask\"},\"/c\":\"legacy\",\"/d\":{}},\"models\":{\"codex\":\"gpt-6.1-sol\"},\"provider\":\"codex\"}\n"
    );
}

#[test]
fn a_workspace_binding_alone_does_not_force_a_rewrite() {
    let original = r#"{"provider":"codex","models":{"codex":"gpt-6.1-sol"},"workspaces":{"/a":{"fast_mode_model_bound":true}}}"#;
    let fixture = Fixture::with_settings(original);
    fixture.save(MODEL).unwrap();
    assert_eq!(fixture.read(), original);
}

#[test]
fn invalid_settings_are_kept_and_copied_once_for_recovery() {
    for (original, expected) in [
        ("{\"provider\":", SettingsWriteError::InvalidFormat),
        ("[]", SettingsWriteError::InvalidFormat),
        (
            "{\"provider\":\"codex\",\"provider\":\"gateway\"}",
            SettingsWriteError::InvalidFormat,
        ),
    ] {
        let fixture = Fixture::with_settings(original);
        assert_eq!(fixture.save(MODEL), Err(expected), "{original}");
        assert_eq!(fixture.save(MODEL), Err(expected), "{original}");
        assert_eq!(fixture.read(), original);
        let copies = fixture.copies("corrupt");
        assert_eq!(copies.len(), 1, "{original}");
        let copy = fixture
            .paths
            .config
            .join(BACKUPS_DIRECTORY)
            .join(&copies[0]);
        assert_eq!(fs::read_to_string(&copy).unwrap(), original);
        assert_eq!(mode(&copy), 0o600);
        assert_eq!(mode(&fixture.paths.config.join(BACKUPS_DIRECTORY)), 0o700);
    }
}

#[test]
fn settings_that_cannot_hold_the_selection_are_refused_unchanged() {
    for original in [
        r#"{"models":"gpt-6.1-sol"}"#,
        r#"{"workspaces":[]}"#,
        r#"{"models":{"CODEX":"gpt-5.6-terra"}}"#,
        r#"{"permission_mode":"sometimes"}"#,
        r#"{"max_agent_steps":-1}"#,
        r#"{"auto_compact_percent":90}"#,
    ] {
        let fixture = Fixture::with_settings(original);
        assert_eq!(
            fixture.save(MODEL),
            Err(SettingsWriteError::InvalidFormat),
            "{original}"
        );
        assert_eq!(fixture.read(), original);
        assert!(fixture.copies("corrupt").is_empty());
    }
    let fixture = Fixture::new();
    for model in ["", " gpt", "gpt\n", "gpt\u{7f}"] {
        assert_eq!(fixture.save(model), Err(SettingsWriteError::InvalidField));
    }
    assert!(!fixture.settings().exists());
}

#[test]
fn settings_accept_exactly_64_kib_and_refuse_larger_files_and_candidates() {
    let padding = |total: usize| {
        let prefix = "{\"theme\":\"";
        let suffix = "\"}";
        format!(
            "{prefix}{}{suffix}",
            "x".repeat(total - prefix.len() - suffix.len())
        )
    };
    let largest = padding(MAX_SETTINGS_BYTES);
    let fixture = Fixture::with_settings(&largest);
    assert_eq!(fixture.save(MODEL), Err(SettingsWriteError::TooLarge));
    assert_eq!(fixture.read(), largest);

    let too_large = padding(MAX_SETTINGS_BYTES + 1);
    let fixture = Fixture::with_settings(&too_large);
    assert_eq!(
        fixture.save(MODEL),
        Err(SettingsWriteError::PrimaryTooLarge)
    );
    assert_eq!(fixture.read(), too_large);

    let fits = padding(MAX_SETTINGS_BYTES - 100);
    let fixture = Fixture::with_settings(&fits);
    fixture.save(MODEL).unwrap();
    assert!(fixture.read().len() <= MAX_SETTINGS_BYTES);
}

#[test]
fn every_commit_backs_up_the_previous_settings_and_keeps_five() {
    let fixture = Fixture::with_settings("{\"theme\":\"dark\"}");
    let models = [
        "gpt-6.1-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "m4",
        "m5",
        "m6",
        "m7",
    ];
    for model in models {
        fixture.save(model).unwrap();
    }
    let backups = fixture.copies("backup");
    assert_eq!(backups.len(), BACKUP_KEEP_COUNT);
    let sequences: Vec<u64> = backups
        .iter()
        .map(|name| parse_sequence(name).unwrap())
        .collect();
    assert_eq!(sequences, [3, 4, 5, 6, 7]);
    let newest = fixture
        .paths
        .config
        .join(BACKUPS_DIRECTORY)
        .join(&backups[4]);
    assert_eq!(
        fs::read_to_string(newest).unwrap(),
        "{\"theme\":\"dark\",\"models\":{\"codex\":\"m6\"},\"provider\":\"codex\"}\n"
    );
    let entries = fs::read_dir(fixture.paths.config.join(BACKUPS_DIRECTORY))
        .unwrap()
        .count();
    assert_eq!(entries, BACKUP_KEEP_COUNT);
}

#[test]
fn backup_names_order_by_sequence_then_timestamp_then_name() {
    assert!(backup_name_newer_than(
        "settings.json.backup.100-0000000000000002-aa",
        "settings.json.backup.100-0000000000000001-bb"
    ));
    assert!(!backup_name_newer_than(
        "settings.json.backup.999-0000000000000001-aa",
        "settings.json.backup.100-0000000000000002-bb"
    ));
    assert!(backup_name_newer_than(
        "settings.json.backup.1-0000000000000001-aa",
        "settings.json.backup.999"
    ));
    assert!(backup_name_newer_than(
        "settings.json.backup.200",
        "settings.json.backup.100"
    ));
    assert_eq!(
        parse_sequence("settings.json.corrupt.5-00000000000000ff-cafe"),
        Some(255)
    );
    assert_eq!(parse_sequence("settings.json.backup.5-ff-cafe"), None);
    assert_eq!(parse_sequence("other.5-00000000000000ff-cafe"), None);
    assert_eq!(parse_backup_timestamp("settings.json.backup.-x"), None);
    assert_eq!(parse_backup_timestamp("settings.json.backup.42"), Some(42));
}

#[test]
fn concurrent_edits_are_merged_and_persistent_conflicts_give_up() {
    let fixture = Fixture::with_settings("{\"theme\":\"dark\"}");
    let settings = fixture.settings();
    let mut edits = 0;
    commit(&fixture.paths, Patch::CodexModel(MODEL), &mut || {
        if edits == 0 {
            fs::write(&settings, "{\"theme\":\"light\"}").unwrap();
        }
        edits += 1;
    })
    .unwrap();
    assert_eq!(edits, 2);
    assert_eq!(
        fixture.read(),
        "{\"theme\":\"light\",\"models\":{\"codex\":\"gpt-6.1-sol\"},\"provider\":\"codex\"}\n"
    );

    let fixture = Fixture::with_settings("{\"theme\":\"dark\"}");
    let settings = fixture.settings();
    let mut edits = 0;
    let outcome = commit(&fixture.paths, Patch::CodexModel(MODEL), &mut || {
        edits += 1;
        fs::write(&settings, format!("{{\"edit\":{edits}}}")).unwrap();
    });
    assert_eq!(
        outcome,
        Err(SettingsWriteError::ConcurrentModification.into())
    );
    assert_eq!(edits, COMMIT_ATTEMPTS);
    assert_eq!(fixture.read(), "{\"edit\":3}");
}

#[test]
fn linked_settings_and_profile_directories_are_refused_without_touching_targets() {
    let fixture = Fixture::new();
    fs::create_dir_all(&fixture.paths.config).unwrap();
    let outside = fixture.paths.config.parent().unwrap().join("outside.json");
    fs::write(&outside, "{}").unwrap();
    symlink(&outside, fixture.settings()).unwrap();
    assert_eq!(
        fixture.save(MODEL),
        Err(SettingsWriteError::DurablePathUnsafe)
    );
    fs::remove_file(fixture.settings()).unwrap();
    fs::hard_link(&outside, fixture.settings()).unwrap();
    assert_eq!(
        fixture.save(MODEL),
        Err(SettingsWriteError::DurablePathUnsafe)
    );
    assert_eq!(fs::read_to_string(&outside).unwrap(), "{}");

    let fixture = Fixture::new();
    let elsewhere = fixture.paths.config.parent().unwrap().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    symlink(&elsewhere, &fixture.paths.config).unwrap();
    assert_eq!(
        fixture.save(MODEL),
        Err(SettingsWriteError::DurablePathUnsafe)
    );
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

#[test]
fn shared_settings_are_made_private_before_they_are_rewritten() {
    let fixture = Fixture::with_settings("{}");
    fs::set_permissions(&fixture.paths.config, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(fixture.settings(), fs::Permissions::from_mode(0o664)).unwrap();
    fixture.save(MODEL).unwrap();
    assert_eq!(mode(&fixture.paths.config), 0o700);
    assert_eq!(mode(&fixture.settings()), 0o600);
}

#[test]
fn a_held_settings_lock_fails_as_busy() {
    let fixture = Fixture::with_settings("{}");
    let holder = PrivateDir::open_existing(&fixture.paths.config)
        .unwrap()
        .unwrap();
    let held = holder.try_lock(LOCK_FILE).unwrap().unwrap();
    let started = Instant::now();
    assert_eq!(fixture.save(MODEL), Err(SettingsWriteError::LockBusy));
    assert!(started.elapsed() >= LOCK_WAIT);
    assert_eq!(fixture.read(), "{}");
    drop(held);
    fixture.save(MODEL).unwrap();
}

#[test]
fn numbers_whose_value_would_change_are_never_rewritten() {
    for original in [
        r#"{"limit":123456789012345678901234567890}"#,
        r#"{"nested":[{"x":-9223372036854775809}]}"#,
        r#"{"limit":9007199254740993.0}"#,
        r#"{"tiny":1e-400}"#,
        r#"{"wide":18446744073709551616}"#,
        r#"{"close":0.1000000000000000055511151231257827}"#,
    ] {
        let fixture = Fixture::with_settings(original);
        assert_eq!(
            fixture.save(MODEL),
            Err(SettingsWriteError::NumberNotPreserved),
            "{original}"
        );
        assert_eq!(fixture.read(), original);
        assert!(fixture.copies("backup").is_empty());
    }
    let fixture = Fixture::with_settings(
        r#"{"bounds":[-9223372036854775808,18446744073709551615,1e300,0.30000000000000004],"short":[0.1,1e2,1.50,2.5E-3,-7.0,0e99999999999999999999],"note":"123456789012345678901234567890 \" 99999999999999999999"}"#,
    );
    fixture.save(MODEL).unwrap();
    assert_eq!(
        fixture.read(),
        format!(
            "{{\"bounds\":[-9223372036854775808,18446744073709551615,1{},0.30000000000000004],\"short\":[0.1,100,1.5,0.0025,-7,0],\"note\":\"123456789012345678901234567890 \\\" 99999999999999999999\",\"models\":{{\"codex\":\"gpt-6.1-sol\"}},\"provider\":\"codex\"}}\n",
            "0".repeat(300)
        )
    );
}

#[test]
fn numbers_are_only_checked_when_the_save_changes_the_file() {
    for original in [
        r#"{"provider":"codex","models":{"codex":"gpt-6.1-sol"},"limit":9007199254740993.0}"#,
        r#"{"models":{"codex":"gpt-6.1-sol"},"provider":"codex","big":123456789012345678901234567890}"#,
        r#"{"provider":"codex","models":{"codex":"gpt-6.1-sol"},"workspaces":{"/a":{"fast_mode_model_bound":true}},"limit":9007199254740993.0}"#,
    ] {
        let fixture = Fixture::with_settings(original);
        fixture.save(MODEL).unwrap();
        assert_eq!(fixture.read(), original);
        assert!(fixture.copies("backup").is_empty(), "{original}");
    }
    for original in [
        r#"{"provider":"codex","models":{"codex":"gpt-6.1-sol"},"codex_model":"gpt-6.1-sol","limit":9007199254740993.0}"#,
        r#"{"provider":"codex","models":{"codex":"gpt-6.1-sol"},"fast_mode_model_bound":true,"limit":9007199254740993.0}"#,
        r#"{"provider":"codex","models":{"codex":"gpt-5.6-terra"},"limit":9007199254740993.0}"#,
    ] {
        let fixture = Fixture::with_settings(original);
        assert_eq!(
            fixture.save(MODEL),
            Err(SettingsWriteError::NumberNotPreserved),
            "{original}"
        );
        assert_eq!(fixture.read(), original);
        assert!(fixture.copies("backup").is_empty(), "{original}");
    }
}

#[test]
fn an_exhausted_backup_sequence_never_prunes_the_new_backup() {
    let original = "{\"theme\":\"must-survive\"}";
    let fixture = Fixture::with_settings(original);
    let backups = fixture.paths.config.join(BACKUPS_DIRECTORY);
    fs::create_dir_all(&backups).unwrap();
    for suffix in ["a1", "b2", "c3", "d4", "e5"] {
        let name = format!("settings.json.backup.9223372036854775807-ffffffffffffffff-{suffix}");
        fs::write(backups.join(name), "{}").unwrap();
    }
    fixture.save(MODEL).unwrap();
    let copies = fixture.copies("backup");
    assert_eq!(copies.len(), BACKUP_KEEP_COUNT);
    let kept: Vec<String> = copies
        .iter()
        .map(|name| fs::read_to_string(backups.join(name)).unwrap())
        .collect();
    assert_eq!(
        kept.iter().filter(|text| text.as_str() == original).count(),
        1
    );
    assert!(fixture.read().contains("\"provider\":\"codex\""));
}

#[test]
fn a_backup_that_cannot_be_written_stops_the_save() {
    let original = "{\"theme\":\"dark\"}";
    let elsewhere = tempfile::tempdir().unwrap();
    for blocked in ["file", "symlink"] {
        let fixture = Fixture::with_settings(original);
        let backups = fixture.paths.config.join(BACKUPS_DIRECTORY);
        if blocked == "file" {
            fs::write(&backups, "not a folder").unwrap();
        } else {
            symlink(elsewhere.path(), &backups).unwrap();
        }
        assert_eq!(
            fixture.save(MODEL),
            Err(SettingsWriteError::BackupFailed),
            "{blocked}"
        );
        assert_eq!(fixture.read(), original, "{blocked}");
        fs::write(fixture.settings(), "{").unwrap();
        assert_eq!(
            fixture.save(MODEL),
            Err(SettingsWriteError::InvalidFormat),
            "{blocked}"
        );
        fs::remove_file(fixture.settings()).unwrap();
        fixture.save(MODEL).unwrap();
        assert!(fixture.read().contains("\"provider\":\"codex\""));
    }
    assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);
}

#[test]
fn permission_mode_patches_write_the_upstream_label_at_top_level() {
    let fixture = Fixture::with_settings("{\"future\":{\"nested\":7},\"permission_mode\":\"ask\"}");
    for (mode, label) in [
        (PermissionMode::Auto, "auto"),
        (PermissionMode::Yolo, "yolo"),
        (PermissionMode::Ask, "ask"),
    ] {
        save_permission_mode(&fixture.paths, mode).unwrap();
        assert_eq!(
            fixture.read(),
            format!("{{\"future\":{{\"nested\":7}},\"permission_mode\":\"{label}\"}}\n")
        );
    }
    let backups = fixture.copies("backup").len();
    save_permission_mode(&fixture.paths, PermissionMode::Ask).unwrap();
    assert_eq!(fixture.copies("backup").len(), backups);
    let fresh = Fixture::new();
    save_permission_mode(&fresh.paths, PermissionMode::Yolo).unwrap();
    assert_eq!(fresh.read(), "{\"permission_mode\":\"yolo\"}\n");
}

#[test]
fn a_permission_mode_patch_snapshots_and_removes_legacy_workspace_copies() {
    let original = concat!(
        "{\"future\":7,\"workspaces\":{",
        "\"/workspace/a\":{\"permission_mode\":\"ask\",\"input_appearance\":\"lines\",\"sandbox\":\"none\"},",
        "\"/workspace/b\":{\"permission_mode\":\"yolo\"},",
        "\"/workspace/c\":{\"effort\":\"low\"},",
        "\"legacy-string\":\"preserve-me\"}}\n"
    );
    let fixture = Fixture::with_settings(original);
    save_permission_mode(&fixture.paths, PermissionMode::Auto).unwrap();
    assert_eq!(
        fixture.read(),
        "{\"future\":7,\"workspaces\":{\"/workspace/a\":{\"sandbox\":\"none\"},\"/workspace/c\":{\"effort\":\"low\"},\"legacy-string\":\"preserve-me\"},\"permission_mode\":\"auto\"}\n"
    );
    let snapshot = fixture
        .paths
        .config
        .join(BACKUPS_DIRECTORY)
        .join(PERMISSION_MODE_MIGRATION.snapshot);
    assert_eq!(fs::read_to_string(&snapshot).unwrap(), original);
    assert_eq!(mode(&snapshot), 0o600);
    let settings = Settings::load(&fixture.paths, Path::new("/workspace/b")).unwrap();
    assert_eq!(settings.permission_mode(&|_| None), PermissionMode::Auto);
}

#[test]
fn a_missing_migration_snapshot_stops_the_permission_mode_save() {
    let original = r#"{"workspaces":{"/workspace":{"permission_mode":"ask"}}}"#;
    let fixture = Fixture::with_settings(original);
    fs::write(fixture.paths.config.join(BACKUPS_DIRECTORY), "not a folder").unwrap();
    assert_eq!(
        save_permission_mode(&fixture.paths, PermissionMode::Yolo),
        Err(SettingsWriteError::MigrationSnapshotFailed.into())
    );
    assert_eq!(fixture.read(), original);
}

#[test]
fn a_model_preference_saves_the_provider_its_model_and_the_bound_fast_choice() {
    let fixture = Fixture::with_settings("{\"future\":true,\"provider\":\"codex\"}");
    let local = ProviderId::Configured("local".to_owned());
    save_model_preference(&fixture.paths, &local, "m-1", true).unwrap();
    assert_eq!(
        fixture.read(),
        "{\"future\":true,\"provider\":\"local\",\"models\":{\"local\":\"m-1\"},\"fast_mode\":true,\"fast_mode_model_bound\":true}\n"
    );
    save_model_preference(&fixture.paths, &local, "m-2", false).unwrap();
    assert_eq!(
        fixture.read(),
        "{\"future\":true,\"provider\":\"local\",\"models\":{\"local\":\"m-2\"},\"fast_mode\":false,\"fast_mode_model_bound\":true}\n"
    );
    let backups = fixture.copies("backup").len();
    save_model_preference(&fixture.paths, &local, "m-2", false).unwrap();
    assert_eq!(fixture.copies("backup").len(), backups);
    let fresh =
        Fixture::with_settings("{\"codex_model\":\"gpt-5.6-terra\",\"model\":\"gateway/model\"}");
    save_model_preference(&fresh.paths, &ProviderId::Codex, MODEL, true).unwrap();
    assert_eq!(
        fresh.read(),
        "{\"model\":\"gateway/model\",\"models\":{\"codex\":\"gpt-6.1-sol\"},\"provider\":\"codex\",\"fast_mode\":true,\"fast_mode_model_bound\":true}\n"
    );
}

#[test]
fn a_model_preference_snapshots_and_removes_workspace_fast_choices() {
    let original = concat!(
        "{\"workspaces\":{",
        "\"/workspace/a\":{\"fast_mode\":false,\"fast_mode_model_bound\":true,\"effort\":\"low\"},",
        "\"/workspace/b\":{\"fast_mode\":false},",
        "\"/workspace/c\":{\"permission_mode\":\"ask\"}}}\n"
    );
    let fixture = Fixture::with_settings(original);
    save_model_preference(&fixture.paths, &ProviderId::Codex, MODEL, true).unwrap();
    assert_eq!(
        fixture.read(),
        "{\"workspaces\":{\"/workspace/a\":{\"effort\":\"low\"},\"/workspace/c\":{\"permission_mode\":\"ask\"}},\"models\":{\"codex\":\"gpt-6.1-sol\"},\"provider\":\"codex\",\"fast_mode\":true,\"fast_mode_model_bound\":true}\n"
    );
    let snapshot = fixture
        .paths
        .config
        .join(BACKUPS_DIRECTORY)
        .join(FAST_MODE_MIGRATION.snapshot);
    assert_eq!(fs::read_to_string(&snapshot).unwrap(), original);
    let settings = Settings::load(&fixture.paths, Path::new("/workspace/b")).unwrap();
    assert!(settings.fast_mode_for(&ProviderId::Codex, MODEL));
    let bound_only = Fixture::with_settings(
        r#"{"workspaces":{"/workspace":{"fast_mode_model_bound":true,"effort":"low"}}}"#,
    );
    save_model_preference(&bound_only.paths, &ProviderId::Codex, MODEL, false).unwrap();
    assert_eq!(
        bound_only.read(),
        "{\"workspaces\":{\"/workspace\":{\"effort\":\"low\"}},\"models\":{\"codex\":\"gpt-6.1-sol\"},\"provider\":\"codex\",\"fast_mode\":false,\"fast_mode_model_bound\":true}\n"
    );
    assert!(
        !bound_only
            .paths
            .config
            .join(BACKUPS_DIRECTORY)
            .join(FAST_MODE_MIGRATION.snapshot)
            .exists()
    );
}

#[test]
fn a_model_preference_refuses_a_model_settings_cannot_hold() {
    let original = "{\"provider\":\"codex\"}";
    let fixture = Fixture::with_settings(original);
    for model in [" padded", ""] {
        assert_eq!(
            save_model_preference(&fixture.paths, &ProviderId::Codex, model, true),
            Err(SettingsWriteError::InvalidField.into())
        );
    }
    assert_eq!(fixture.read(), original);
}

#[test]
fn the_full_access_acknowledgment_keeps_every_mode_spelling() {
    for spelling in ["full-access", "Full Access", "yolo"] {
        let fixture = Fixture::with_settings(&format!("{{\"permission_mode\":\"{spelling}\"}}\n"));
        save_yolo_acknowledged(&fixture.paths).unwrap();
        assert_eq!(
            fixture.read(),
            format!("{{\"permission_mode\":\"{spelling}\",\"yolo_acknowledged\":true}}\n")
        );
        let backups = fixture.copies("backup");
        save_yolo_acknowledged(&fixture.paths).unwrap();
        assert_eq!(fixture.copies("backup"), backups);
    }
    let fixture = Fixture::with_settings("{\"yolo_acknowledged\":false}");
    save_yolo_acknowledged(&fixture.paths).unwrap();
    assert_eq!(fixture.read(), "{\"yolo_acknowledged\":true}\n");
}

#[test]
fn user_preferences_refuse_workspaces_that_are_not_an_object() {
    let original = r#"{"workspaces":"legacy"}"#;
    let fixture = Fixture::with_settings(original);
    assert_eq!(
        save_permission_mode(&fixture.paths, PermissionMode::Ask),
        Err(SettingsWriteError::InvalidFormat.into())
    );
    assert_eq!(
        save_yolo_acknowledged(&fixture.paths),
        Err(SettingsWriteError::InvalidFormat.into())
    );
    assert_eq!(fixture.save(MODEL), Err(SettingsWriteError::InvalidFormat));
    assert_eq!(fixture.read(), original);
}

fn add_rule(category: &'static str, pattern: &'static str) -> PermissionPatch<'static> {
    PermissionPatch::Add {
        category,
        pattern,
        action: PermissionAction::Allow,
    }
}

#[test]
fn user_permission_patches_write_top_level_and_keep_local_rules() {
    let fixture = Fixture::with_settings(
        "{\"permission\":{\"bash\":{\"global *\":\"allow\"}},\"workspaces\":{\"/work\":{\"permission\":{\"bash\":{\"local *\":\"allow\"}}}}}\n",
    );
    assert_eq!(
        save_permission_patch(&fixture.paths, None, add_rule("bash", "user *")),
        Ok(CommitOutcome::Committed {
            permission_rules_removed: 0
        })
    );
    assert_eq!(
        fixture.read(),
        "{\"permission\":{\"bash\":{\"global *\":\"allow\",\"user *\":\"allow\"}},\"workspaces\":{\"/work\":{\"permission\":{\"bash\":{\"local *\":\"allow\"}}}}}\n"
    );
    assert_eq!(
        save_permission_patch(&fixture.paths, None, add_rule("bash", "user *")),
        Ok(CommitOutcome::Unchanged)
    );
}

#[test]
fn local_permission_patches_create_the_workspace_entry_and_keep_its_order() {
    let fixture = Fixture::new();
    let workspace = Some(Path::new("/work"));
    for (category, pattern) in [("bash", "git *"), ("read", "*"), ("bash", "echo")] {
        save_permission_patch(&fixture.paths, workspace, add_rule(category, pattern)).unwrap();
    }
    assert_eq!(
        fixture.read(),
        "{\"workspaces\":{\"/work\":{\"permission\":{\"bash\":{\"git *\":\"allow\",\"echo\":\"allow\"},\"read\":{\"*\":\"allow\"}}}}}\n"
    );
    let settings = Settings::load(&fixture.paths, Path::new("/work")).unwrap();
    let rules: Vec<(&str, &str)> = settings
        .effective_permission_rules()
        .iter()
        .map(|rule| (rule.permission.as_str(), rule.pattern.as_str()))
        .collect();
    assert_eq!(rules, [("bash", "git *"), ("bash", "echo"), ("read", "*")]);
}

#[test]
fn permission_patches_need_an_absolute_workspace_and_isolate_remove_and_reset() {
    let fixture = Fixture::with_settings(
        "{\"permission\":{\"bash\":{\"user *\":\"allow\"}},\"workspaces\":{\"/work\":{\"permission\":{\"bash\":{\"local *\":\"allow\",\"deny *\":\"deny\"},\"read\":{\"*\":\"allow\"}}}}}\n",
    );
    let workspace = Some(Path::new("/work"));
    assert_eq!(
        save_permission_patch(
            &fixture.paths,
            Some(Path::new("relative")),
            PermissionPatch::Reset(AllowlistResetScope::All)
        )
        .map_err(|failure| failure.error),
        Err(SettingsWriteError::InvalidField)
    );
    assert_eq!(
        save_permission_patch(
            &fixture.paths,
            workspace,
            PermissionPatch::Remove {
                category: "bash",
                pattern: "local *"
            }
        ),
        Ok(CommitOutcome::Committed {
            permission_rules_removed: 1
        })
    );
    assert_eq!(
        save_permission_patch(
            &fixture.paths,
            workspace,
            PermissionPatch::Reset(AllowlistResetScope::All)
        ),
        Ok(CommitOutcome::Committed {
            permission_rules_removed: 1
        })
    );
    assert_eq!(
        fixture.read(),
        "{\"permission\":{\"bash\":{\"user *\":\"allow\"}},\"workspaces\":{\"/work\":{\"permission\":{\"bash\":{\"deny *\":\"deny\"}}}}}\n"
    );
}

#[test]
fn permission_patches_match_rules_stored_with_padded_keys() {
    let fixture = Fixture::with_settings(
        "{\"permission\":{\" bash \":{\" git status * \":\"allow\",\"keep *\":\"deny\"}}}\n",
    );
    assert_eq!(
        save_permission_patch(
            &fixture.paths,
            None,
            PermissionPatch::Remove {
                category: "bash",
                pattern: "git status *"
            }
        ),
        Ok(CommitOutcome::Committed {
            permission_rules_removed: 1
        })
    );
    assert_eq!(
        save_permission_patch(
            &fixture.paths,
            None,
            PermissionPatch::Reset(AllowlistResetScope::Commands)
        ),
        Ok(CommitOutcome::Unchanged)
    );
    assert_eq!(
        fixture.read(),
        "{\"permission\":{\" bash \":{\"keep *\":\"deny\"}}}\n"
    );
    let fixture = Fixture::with_settings(
        "{\"permission\":{\" bash \":{\" one * \":\"allow\",\"two *\":\"allow\"}}}\n",
    );
    assert_eq!(
        save_permission_patch(
            &fixture.paths,
            None,
            PermissionPatch::Reset(AllowlistResetScope::Commands)
        ),
        Ok(CommitOutcome::Committed {
            permission_rules_removed: 2
        })
    );
    assert_eq!(fixture.read(), "{}\n");
}

#[test]
fn permission_patches_refuse_settings_the_loader_would_reject() {
    let local = Some(Path::new("/work"));
    for (text, workspaces) in [
        (
            "{\"permission\":{\"bash\":5},\"workspaces\":{\"/work\":{\"permission\":[]}}}\n",
            &[None, local][..],
        ),
        (
            "{\"workspaces\":{\"/work\":{\"permission\":{\"edit\":5}}}}\n",
            &[local][..],
        ),
    ] {
        let fixture = Fixture::with_settings(text);
        for &workspace in workspaces {
            assert_eq!(
                save_permission_patch(&fixture.paths, workspace, add_rule("bash", "ls"))
                    .map_err(|failure| failure.error),
                Err(SettingsWriteError::InvalidFormat),
                "{text} {workspace:?}"
            );
        }
        assert_eq!(fixture.read(), text);
    }
}
