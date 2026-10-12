use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::crc32c::crc32c;

pub(crate) const FIXTURES: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sessions_v2");

pub(crate) fn framed(payload: &str) -> Vec<u8> {
    format!(
        "{payload},\"crc\":\"{:08x}\"}}\n",
        crc32c(payload.as_bytes())
    )
    .into_bytes()
}

pub(crate) fn line(seq: u64, kind: &str, fields: &str) -> Vec<u8> {
    framed(&format!(
        "{{\"v\":1,\"seq\":{seq},\"ts\":1000,\"kind\":\"{kind}\"{fields}"
    ))
}

pub(crate) fn fixture_log(scenario: &str, id: &str) -> Vec<u8> {
    fs::read(
        Path::new(FIXTURES)
            .join(scenario)
            .join(id)
            .join("log.jsonl"),
    )
    .unwrap()
}

pub(crate) fn install(scenario: &str, root: &Path) {
    copy_private(&Path::new(FIXTURES).join(scenario), root);
}

fn copy_private(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    fs::set_permissions(to, fs::Permissions::from_mode(0o700)).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target: PathBuf = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_private(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
            let blob = from.file_name().is_some_and(|name| name == "blobs");
            let mode = if blob { 0o400 } else { 0o600 };
            fs::set_permissions(&target, fs::Permissions::from_mode(mode)).unwrap();
        }
    }
}
