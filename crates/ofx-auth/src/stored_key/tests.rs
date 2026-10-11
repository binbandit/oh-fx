use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::*;

fn profile(directory: &Path) -> StoredKeyStore {
    StoredKeyStore {
        backend: Backend::ProfileFile {
            directory: Some(directory.to_path_buf()),
        },
        disabled: false,
    }
}

fn keychain(program: &Path, account: Option<&str>) -> StoredKeyStore {
    StoredKeyStore {
        backend: Backend::Keychain {
            program: program.to_path_buf(),
            account: account.map(str::to_owned),
        },
        disabled: false,
    }
}

fn loaded(store: &StoredKeyStore) -> Result<Option<String>, StoredKeyError> {
    store
        .load()
        .map(|key| key.map(|key| key.as_str().to_owned()))
}

fn fake_security(directory: &Path, script: &str) -> PathBuf {
    let program = directory.join("security");
    fs::write(&program, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    program
}

#[test]
fn the_profile_file_holds_one_trimmed_key_with_private_permissions() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data/oh-fx");
    let store = profile(&data);
    assert_eq!(loaded(&store), Ok(None));

    store
        .save(|| Some(Zeroizing::new("vck-saved-key".to_owned())))
        .unwrap();
    let file = data.join(API_KEY_FILE);
    assert_eq!(fs::read_to_string(&file).unwrap(), "vck-saved-key");
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&data).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(loaded(&store), Ok(Some("vck-saved-key".to_owned())));

    fs::write(&file, "vck-line-ends\r\n").unwrap();
    assert_eq!(loaded(&store), Ok(Some("vck-line-ends".to_owned())));
    fs::write(&file, "\r\n").unwrap();
    assert_eq!(loaded(&store), Ok(None));
}

#[test]
fn an_unsafe_or_unreadable_profile_file_is_an_error_not_an_absent_key() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    let store = profile(&data);
    store
        .save(|| Some(Zeroizing::new("vck-key".to_owned())))
        .unwrap();
    let file = data.join(API_KEY_FILE);

    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(loaded(&store), Err(StoredKeyError::Insecure));
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&file, "k".repeat(MAX_KEY_BYTES + 1)).unwrap();
    assert_eq!(loaded(&store), Err(StoredKeyError::Unreadable));
    fs::write(&file, b"\xff\xfe").unwrap();
    assert_eq!(loaded(&store), Err(StoredKeyError::Unreadable));

    let homeless = StoredKeyStore {
        backend: Backend::ProfileFile { directory: None },
        disabled: false,
    };
    assert_eq!(loaded(&homeless), Err(StoredKeyError::Unreadable));
    assert_eq!(
        homeless.save(|| Some(Zeroizing::new("vck-key".to_owned()))),
        Err(StoredKeyError::WriteFailed)
    );
}

#[test]
fn a_cancelled_or_empty_paste_saves_nothing() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    let store = profile(&data);
    assert_eq!(store.save(|| None), Err(StoredKeyError::WriteFailed));
    assert_eq!(
        store.save(|| Some(Zeroizing::new(String::new()))),
        Err(StoredKeyError::WriteFailed)
    );
    assert!(!data.join(API_KEY_FILE).exists());
}

#[test]
fn the_disable_switch_hides_and_refuses_the_store() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("oh-fx");
    profile(&data)
        .save(|| Some(Zeroizing::new("vck-key".to_owned())))
        .unwrap();
    for value in ["1", "true", "TRUE"] {
        let lookup = |name: &str| (name == DISABLE_KEYCHAIN_VARIABLE).then(|| value.to_owned());
        assert!(keychain_disabled(&lookup), "{value}");
    }
    for value in ["0", "yes", "", " 1"] {
        let lookup = |name: &str| (name == DISABLE_KEYCHAIN_VARIABLE).then(|| value.to_owned());
        assert!(!keychain_disabled(&lookup), "{value}");
    }
    let disabled = StoredKeyStore {
        disabled: true,
        ..profile(&data)
    };
    assert!(disabled.disabled());
    assert_eq!(loaded(&disabled), Ok(None));
    assert_eq!(
        disabled.save(|| Some(Zeroizing::new("vck-other".to_owned()))),
        Err(StoredKeyError::WriteFailed)
    );
    assert_eq!(
        fs::read_to_string(data.join(API_KEY_FILE)).unwrap(),
        "vck-key"
    );
}

#[test]
fn the_keychain_answers_through_security_for_the_user_account() {
    let root = tempfile::tempdir().unwrap();
    let calls = root.path().join("calls");
    let program = fake_security(
        root.path(),
        &format!(
            "echo \"$@\" >> '{}'\nprintf 'vck-keychain-key\\n'",
            calls.display()
        ),
    );
    assert_eq!(
        loaded(&keychain(&program, Some("someone"))),
        Ok(Some("vck-keychain-key".to_owned()))
    );
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "find-generic-password -a someone -s OH_FX_AI_GATEWAY_API_KEY -w\n"
    );
}

#[test]
fn a_missing_keychain_item_is_absent_and_other_failures_are_unreadable() {
    let root = tempfile::tempdir().unwrap();
    let missing = fake_security(
        root.path(),
        "echo 'security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.' >&2\nexit 44",
    );
    assert_eq!(loaded(&keychain(&missing, Some("someone"))), Ok(None));
    let failing = fake_security(root.path(), "echo 'security: denied' >&2\nexit 1");
    assert_eq!(
        loaded(&keychain(&failing, Some("someone"))),
        Err(StoredKeyError::Unreadable)
    );
    let absent = root.path().join("no-such-security");
    assert_eq!(
        loaded(&keychain(&absent, Some("someone"))),
        Err(StoredKeyError::Unreadable)
    );
    assert_eq!(loaded(&keychain(&absent, None)), Ok(None));
}

#[test]
fn saving_to_the_keychain_lets_security_prompt_for_the_key() {
    let root = tempfile::tempdir().unwrap();
    let calls = root.path().join("calls");
    let program = fake_security(
        root.path(),
        &format!("echo \"$@\" >> '{}'", calls.display()),
    );
    let store = keychain(&program, Some("someone"));
    store
        .save(|| panic!("the keychain prompts for the key itself"))
        .unwrap();
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "add-generic-password -a someone -s OH_FX_AI_GATEWAY_API_KEY -U -w\n"
    );
    let failing = fake_security(root.path(), "exit 1");
    assert_eq!(
        keychain(&failing, Some("someone")).save(|| None),
        Err(StoredKeyError::WriteFailed)
    );
    assert_eq!(
        keychain(&program, None).save(|| None),
        Err(StoredKeyError::WriteFailed)
    );
}

#[test]
fn the_keychain_account_is_user_then_logname() {
    let both = |name: &str| match name {
        "USER" => Some("user-name".to_owned()),
        "LOGNAME" => Some("log-name".to_owned()),
        _ => None,
    };
    assert_eq!(keychain_account(&both), Some("user-name".to_owned()));
    let logname = |name: &str| match name {
        "USER" => Some(String::new()),
        "LOGNAME" => Some("log-name".to_owned()),
        _ => None,
    };
    assert_eq!(keychain_account(&logname), Some("log-name".to_owned()));
    assert_eq!(keychain_account(&|_| None), None);
    let long = |name: &str| (name == "USER").then(|| "u".repeat(MAX_ACCOUNT_BYTES + 1));
    assert_eq!(keychain_account(&long), None);
}

#[test]
fn the_platform_picks_the_backend_and_its_label() {
    let store = StoredKeyStore::from_environment(Some(Path::new("/data/oh-fx")), &|_| None);
    if cfg!(target_os = "macos") {
        assert_eq!(store.label(), "macOS Keychain");
    } else {
        assert_eq!(store.label(), "profile file");
    }
    assert_eq!(store.label(), STORED_KEY_BACKEND);
    assert_eq!(store.reads_the_key_itself(), cfg!(target_os = "macos"));
    assert!(!store.disabled());
}
