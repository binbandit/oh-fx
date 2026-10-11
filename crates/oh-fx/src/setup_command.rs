use std::cell::Cell;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::process::ExitCode;

use ofx_auth::{
    DISABLE_KEYCHAIN_VARIABLE, HOST_MANAGED_AUTH_MESSAGE, RawInput, StoredKeyStore,
    host_managed_auth,
};
use ofx_config::ProfilePaths;
use zeroize::Zeroizing;

const PROMPT: &str = "Paste AI Gateway API key (input hidden): ";
const NOT_SAVED: &str = "oh-fx setup: API key was not saved";
const NO_TERMINAL: &str = "oh-fx setup: an interactive terminal is required to paste an API key";
const MAX_KEY_BYTES: usize = 8 * 1024;
const MASK: &str = "\u{2022}";
const ERASE: &str = "\x08 \x08";

pub(crate) fn run() -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    if host_managed_auth() {
        return crate::print(
            format!("{HOST_MANAGED_AUTH_MESSAGE}\n").as_bytes(),
            crate::command_write_failure(ofx_cli::TopLevelKind::Setup),
        );
    }
    let lookup = |name: &str| env::var(name).ok();
    let paths = ProfilePaths::from_environment();
    if let Ok(workspace_root) = env::current_dir().and_then(fs::canonicalize) {
        ofx_trace::configure_from_env(
            &workspace_root,
            paths.as_ref().map(|paths| paths.state.as_path()),
        );
    }
    let store =
        StoredKeyStore::from_environment(paths.as_ref().map(|paths| paths.data.as_path()), &lookup);
    if store.disabled() {
        return refuse(&format!(
            "oh-fx setup: stored API keys are disabled by {DISABLE_KEYCHAIN_VARIABLE}"
        ));
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return refuse(NO_TERMINAL);
    }
    if store.reads_the_key_itself() {
        let _ = write_stderr(PROMPT);
    }
    let line_ended = Cell::new(false);
    let saved = store.save(|| {
        let key = read_masked_key()?;
        let _ = write_stderr("\n");
        line_ended.set(true);
        Some(key)
    });
    match saved {
        Ok(()) => crate::print(
            format!("Saved API key to {}.\n", store.label()).as_bytes(),
            crate::command_write_failure(ofx_cli::TopLevelKind::Setup),
        ),
        Err(_) if line_ended.get() => refuse(NOT_SAVED),
        Err(_) => refuse(&format!("\n{NOT_SAVED}")),
    }
}

fn refuse(message: &str) -> ExitCode {
    let _ = write_stderr(&format!("{message}\n"));
    ExitCode::FAILURE
}

fn write_stderr(text: &str) -> io::Result<()> {
    let mut stderr = io::stderr().lock();
    stderr.write_all(text.as_bytes())?;
    stderr.flush()
}

fn read_masked_key() -> Option<Zeroizing<String>> {
    let raw = RawInput::enable(rustix::stdio::stdin());
    let _ = write_stderr(PROMPT);
    let _raw = raw.ok()?;
    let mut key = Zeroizing::new(String::new());
    let mut stdin = io::stdin().lock();
    while key.len() < MAX_KEY_BYTES {
        let mut byte = [0_u8; 1];
        match stdin.read(&mut byte) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        match byte[0] {
            b'\r' | b'\n' if key.is_empty() => {}
            b'\r' | b'\n' => return Some(key),
            3 | 4 | 0x1b => return None,
            8 | 127 if key.pop().is_some() => {
                let _ = write_stderr(ERASE);
            }
            printable @ 0x20..=0x7e => {
                key.push(char::from(printable));
                let _ = write_stderr(MASK);
            }
            _ => {}
        }
    }
    None
}
