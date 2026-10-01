use std::env;

use semver::Version;

const RELEASES_URL: &str = "https://github.com/binbandit/oh-fx/releases";
const TEST_BASE_URL_VARIABLE: &str = "OH_FX_E2E_UPGRADE_BASE_URL";
const LOOPBACK_PREFIX: &str = "http://127.0.0.1:";

pub(crate) struct ReleaseSource {
    base_url: String,
}

impl ReleaseSource {
    pub(crate) fn from_environment() -> Self {
        let base_url = env::var(TEST_BASE_URL_VARIABLE)
            .ok()
            .filter(|url| is_loopback_base_url(url))
            .unwrap_or_else(|| RELEASES_URL.to_owned());
        Self { base_url }
    }

    #[cfg(test)]
    pub(crate) fn at(base_url: String) -> Self {
        Self { base_url }
    }

    pub(crate) fn latest_pointer_url(&self) -> String {
        format!("{}/latest/download/latest.txt", self.base_url)
    }

    pub(crate) fn archive_url(&self, version: &Version, platform: &str) -> String {
        format!(
            "{}/download/v{version}/oh-fx-{platform}.tar.gz",
            self.base_url
        )
    }
}

pub fn release_notes_url(version: &str) -> String {
    format!("{RELEASES_URL}/tag/v{version}")
}

pub(crate) fn parse_latest_pointer(contents: &str) -> Option<Version> {
    let tag = contents.trim();
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

fn is_loopback_base_url(url: &str) -> bool {
    url.strip_prefix(LOOPBACK_PREFIX)
        .is_some_and(|port| !port.is_empty() && port.parse::<u16>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tagged_and_bare_versions() {
        assert_eq!(
            parse_latest_pointer("v0.1.0-dev.42\n"),
            Some(Version::parse("0.1.0-dev.42").unwrap())
        );
        assert_eq!(parse_latest_pointer(" 1.2.3 "), Some(Version::new(1, 2, 3)));
        assert_eq!(parse_latest_pointer("latest"), None);
    }

    #[test]
    fn dev_numbers_order_numerically() {
        let older = parse_latest_pointer("v0.1.0-dev.9").unwrap();
        let newer = parse_latest_pointer("v0.1.0-dev.10").unwrap();
        let stable = parse_latest_pointer("v0.1.0").unwrap();
        assert!(older < newer && newer < stable);
    }

    #[test]
    fn accepts_only_plain_loopback_test_overrides() {
        assert!(is_loopback_base_url("http://127.0.0.1:8080"));
        assert!(!is_loopback_base_url("http://127.0.0.1:"));
        assert!(!is_loopback_base_url("http://localhost:8080"));
        assert!(!is_loopback_base_url("https://127.0.0.1:8080"));
        assert!(!is_loopback_base_url("http://127.0.0.1:8080/path"));
    }

    #[test]
    fn builds_github_release_urls() {
        let source = ReleaseSource::at(RELEASES_URL.to_owned());
        let version = Version::parse("0.1.0-dev.3").unwrap();
        assert_eq!(
            source.archive_url(&version, "linux-x86_64"),
            "https://github.com/binbandit/oh-fx/releases/download/v0.1.0-dev.3/oh-fx-linux-x86_64.tar.gz"
        );
        assert_eq!(
            source.latest_pointer_url(),
            "https://github.com/binbandit/oh-fx/releases/latest/download/latest.txt"
        );
    }
}
