use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ofx_config::{DurableError, PrivateDir};
use ofx_trace::trace_log;
use zeroize::Zeroizing;

pub const DISABLE_KEYCHAIN_VARIABLE: &str = "OH_FX_DISABLE_KEYCHAIN";
pub const STORED_KEY_BACKEND: &str = if cfg!(target_os = "macos") {
    KEYCHAIN_LABEL
} else {
    PROFILE_FILE_LABEL
};

const KEYCHAIN_LABEL: &str = "macOS Keychain";
const PROFILE_FILE_LABEL: &str = "profile file";
const KEYCHAIN_SERVICE: &str = "OH_FX_AI_GATEWAY_API_KEY";
const SECURITY_PROGRAM: &str = "/usr/bin/security";
const API_KEY_FILE: &str = "api-key";
const MAX_KEY_BYTES: usize = 8 * 1024;
const MAX_ACCOUNT_BYTES: usize = 256;
const ITEM_NOT_FOUND: &str = "could not be found";
const KEY_LINE_ENDS: [char; 2] = ['\r', '\n'];
const KEYCHAIN: &str = "keychain";
const STORED_KEY: &str = "stored_key";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StoredKeyError {
    #[error("StoredKeyUnreadable")]
    Unreadable,
    #[error("StoredKeyInsecure")]
    Insecure,
    #[error("StoredKeyWriteFailed")]
    WriteFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Backend {
    Keychain {
        program: PathBuf,
        account: Option<String>,
    },
    ProfileFile {
        directory: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredKeyStore {
    backend: Backend,
    disabled: bool,
}

impl StoredKeyStore {
    pub fn from_environment(
        data_directory: Option<&Path>,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        let backend = if cfg!(target_os = "macos") {
            Backend::Keychain {
                program: PathBuf::from(SECURITY_PROGRAM),
                account: keychain_account(lookup),
            }
        } else {
            Backend::ProfileFile {
                directory: data_directory.map(Path::to_path_buf),
            }
        };
        Self {
            backend,
            disabled: keychain_disabled(lookup),
        }
    }

    pub fn in_profile(data_directory: Option<&Path>) -> Self {
        Self {
            backend: Backend::ProfileFile {
                directory: data_directory.map(Path::to_path_buf),
            },
            disabled: false,
        }
    }

    pub fn label(&self) -> &'static str {
        match self.backend {
            Backend::Keychain { .. } => KEYCHAIN_LABEL,
            Backend::ProfileFile { .. } => PROFILE_FILE_LABEL,
        }
    }

    pub fn reads_the_key_itself(&self) -> bool {
        matches!(self.backend, Backend::Keychain { .. })
    }

    pub fn disabled(&self) -> bool {
        self.disabled
    }

    pub fn load(&self) -> Result<Option<Zeroizing<String>>, StoredKeyError> {
        if self.disabled {
            return Ok(None);
        }
        match &self.backend {
            Backend::Keychain { program, account } => {
                load_from_keychain(program, account.as_deref())
            }
            Backend::ProfileFile { directory } => load_from_profile(directory.as_deref()),
        }
    }

    pub fn save(
        &self,
        read_key: impl FnOnce() -> Option<Zeroizing<String>>,
    ) -> Result<(), StoredKeyError> {
        if self.disabled {
            return Err(StoredKeyError::WriteFailed);
        }
        match &self.backend {
            Backend::Keychain { program, account } => save_to_keychain(program, account.as_deref()),
            Backend::ProfileFile { directory } => {
                let key = read_key().ok_or(StoredKeyError::WriteFailed)?;
                save_to_profile(directory.as_deref(), &key)
            }
        }
    }
}

fn keychain_disabled(lookup: &dyn Fn(&str) -> Option<String>) -> bool {
    lookup(DISABLE_KEYCHAIN_VARIABLE)
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

fn keychain_account(lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    ["USER", "LOGNAME"]
        .into_iter()
        .filter_map(lookup)
        .find(|name| !name.is_empty() && name.len() <= MAX_ACCOUNT_BYTES)
}

fn load_from_keychain(
    program: &Path,
    account: Option<&str>,
) -> Result<Option<Zeroizing<String>>, StoredKeyError> {
    let Some(account) = account else {
        trace_log!(KEYCHAIN, "load skipped step=account reason=UserNotSet");
        return Ok(None);
    };
    let output = Command::new(program)
        .args([
            "find-generic-password",
            "-a",
            account,
            "-s",
            KEYCHAIN_SERVICE,
            "-w",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            trace_log!(KEYCHAIN, "load failed step=spawn err={:?}", error.kind());
            StoredKeyError::Unreadable
        })?;
    let stdout = Zeroizing::new(output.stdout);
    if !output.status.success() {
        if String::from_utf8_lossy(&output.stderr).contains(ITEM_NOT_FOUND) {
            trace_log!(KEYCHAIN, "load failed step=lookup err=KeychainItemNotFound");
            return Ok(None);
        }
        trace_log!(KEYCHAIN, "load failed step=lookup err=KeychainReadFailed");
        return Err(StoredKeyError::Unreadable);
    }
    key_text(&stdout).inspect_err(|error| {
        trace_log!(KEYCHAIN, "load failed step=decode err={error}");
    })
}

fn load_from_profile(
    directory: Option<&Path>,
) -> Result<Option<Zeroizing<String>>, StoredKeyError> {
    let Some(directory) = directory else {
        trace_log!(STORED_KEY, "load failed step=home err=HomeNotSet");
        return Err(StoredKeyError::Unreadable);
    };
    let profile = match PrivateDir::open_existing(directory) {
        Ok(Some(profile)) => profile,
        Ok(None) => return Ok(None),
        Err(error) => {
            trace_log!(STORED_KEY, "load failed step=open_profile err={error}");
            return Err(StoredKeyError::Unreadable);
        }
    };
    match profile.read_private(API_KEY_FILE, MAX_KEY_BYTES) {
        Ok(Some(bytes)) => key_text(&bytes),
        Ok(None) => Ok(None),
        Err(DurableError::InsecureFile) => {
            trace_log!(
                STORED_KEY,
                "load failed step=permissions err=StoredKeyInsecure"
            );
            Err(StoredKeyError::Insecure)
        }
        Err(error) => {
            trace_log!(STORED_KEY, "load failed step=read err={error}");
            Err(StoredKeyError::Unreadable)
        }
    }
}

fn key_text(bytes: &[u8]) -> Result<Option<Zeroizing<String>>, StoredKeyError> {
    let text = std::str::from_utf8(bytes).map_err(|_| StoredKeyError::Unreadable)?;
    let key = text.trim_matches(KEY_LINE_ENDS);
    Ok((!key.is_empty()).then(|| Zeroizing::new(key.to_owned())))
}

fn save_to_keychain(program: &Path, account: Option<&str>) -> Result<(), StoredKeyError> {
    let Some(account) = account else {
        trace_log!(KEYCHAIN, "store failed step=account err=UserNotSet");
        return Err(StoredKeyError::WriteFailed);
    };
    let status = Command::new(program)
        .args([
            "add-generic-password",
            "-a",
            account,
            "-s",
            KEYCHAIN_SERVICE,
            "-U",
            "-w",
        ])
        .status()
        .map_err(|error| {
            trace_log!(
                KEYCHAIN,
                "store failed step=interactive_spawn err={:?}",
                error.kind()
            );
            StoredKeyError::WriteFailed
        })?;
    if status.success() {
        return Ok(());
    }
    trace_log!(KEYCHAIN, "store failed step=interactive_exit term={status}");
    Err(StoredKeyError::WriteFailed)
}

fn save_to_profile(directory: Option<&Path>, key: &str) -> Result<(), StoredKeyError> {
    let directory = directory.ok_or(StoredKeyError::WriteFailed)?;
    if key.is_empty() {
        return Err(StoredKeyError::WriteFailed);
    }
    PrivateDir::open_or_create(directory)
        .and_then(|profile| {
            profile.ensure_private()?;
            profile.replace(API_KEY_FILE, key.as_bytes())
        })
        .map_err(|error| {
            trace_log!(STORED_KEY, "store failed step=write err={error}");
            StoredKeyError::WriteFailed
        })
}

#[cfg(test)]
mod tests;
