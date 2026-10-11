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
            account: account
                .map(str::to_owned)
                .ok_or(KeychainUnavailable::UserNotSet),
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

fn home_with_keychains() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join(KEYCHAINS_FOLDER)).unwrap();
    home
}

fn environment<'a>(variables: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name: &str| {
        variables
            .iter()
            .find(|(variable, _)| *variable == name)
            .map(|(_, value)| (*value).to_owned())
    }
}

#[test]
fn the_keychain_account_is_user_then_logname() {
    let home = home_with_keychains();
    let home = home.path().to_str().unwrap();
    assert_eq!(
        keychain_account(&environment(&[
            ("USER", "user-name"),
            ("LOGNAME", "log-name"),
            ("HOME", home)
        ])),
        Ok("user-name".to_owned())
    );
    assert_eq!(
        keychain_account(&environment(&[
            ("USER", ""),
            ("LOGNAME", "log-name"),
            ("HOME", home)
        ])),
        Ok("log-name".to_owned())
    );
    assert_eq!(
        keychain_account(&environment(&[("HOME", home)])),
        Err(KeychainUnavailable::UserNotSet)
    );
    let long = "u".repeat(MAX_ACCOUNT_BYTES + 1);
    assert_eq!(
        keychain_account(&environment(&[("USER", &long), ("HOME", home)])),
        Err(KeychainUnavailable::UserNotSet)
    );
}

#[test]
fn a_home_without_a_keychains_folder_never_reaches_security_even_with_a_user() {
    let scratch = tempfile::tempdir().unwrap();
    let calls = scratch.path().join("calls");
    let recording = fake_security(
        scratch.path(),
        &format!(
            "echo \"$@\" >> '{}'\nprintf 'vck-real-keychain\\n'",
            calls.display()
        ),
    );
    let bare_home = tempfile::tempdir().unwrap();
    let bare = bare_home.path().to_str().unwrap().to_owned();
    for variables in [
        vec![("USER", "someone"), ("HOME", bare.as_str())],
        vec![("USER", "someone"), ("LOGNAME", "someone")],
        vec![("USER", "someone"), ("HOME", "relative/home")],
        vec![("USER", "someone"), ("HOME", "")],
    ] {
        let lookup = environment(&variables);
        let account = keychain_account(&lookup);
        assert_eq!(
            account,
            Err(KeychainUnavailable::NoKeychainsFolder),
            "{variables:?}"
        );
        let store = StoredKeyStore {
            backend: Backend::Keychain {
                program: recording.clone(),
                account,
            },
            disabled: false,
        };
        assert_eq!(loaded(&store), Ok(None), "{variables:?}");
        assert_eq!(
            store.save(|| panic!("the keychain prompts for the key itself")),
            Err(StoredKeyError::WriteFailed),
            "{variables:?}"
        );
        if cfg!(target_os = "macos") {
            let real = StoredKeyStore::from_environment(None, &lookup);
            assert_eq!(
                real.backend,
                Backend::Keychain {
                    program: PathBuf::from(SECURITY_PROGRAM),
                    account: Err(KeychainUnavailable::NoKeychainsFolder),
                },
                "{variables:?}"
            );
            assert_eq!(loaded(&real), Ok(None), "{variables:?}");
        }
    }
    assert!(
        !calls.exists(),
        "security ran: {:?}",
        fs::read_to_string(&calls)
    );

    let home = home_with_keychains();
    let home = home.path().to_str().unwrap();
    let store = StoredKeyStore {
        backend: Backend::Keychain {
            program: recording,
            account: keychain_account(&environment(&[("USER", "someone"), ("HOME", home)])),
        },
        disabled: false,
    };
    assert_eq!(loaded(&store), Ok(Some("vck-real-keychain".to_owned())));
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "find-generic-password -a someone -s OH_FX_AI_GATEWAY_API_KEY -w\n"
    );
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
