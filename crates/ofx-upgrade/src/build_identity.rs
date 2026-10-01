use std::env::consts::{ARCH, OS};

use semver::Version;

const RELEASE: Option<&str> = option_env!("OH_FX_RELEASE_VERSION");

pub const VERSION: &str = match RELEASE {
    Some(version) => version,
    None => concat!(env!("CARGO_PKG_VERSION"), "-local"),
};

pub(crate) fn release_version() -> Option<Version> {
    RELEASE.and_then(|version| Version::parse(version).ok())
}

pub(crate) fn platform() -> String {
    format!("{OS}-{ARCH}")
}
