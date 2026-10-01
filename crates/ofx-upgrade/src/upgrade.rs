use std::env;
use std::path::PathBuf;
use std::time::Duration;

use semver::Version;

use crate::archive;
use crate::build_identity;
use crate::error::UpgradeError;
use crate::lock::UpgradeLock;
use crate::release_source::{self, ReleaseSource};

const POINTER_LIMIT: usize = 128;
const CHECKSUM_LIMIT: usize = 4096;
const SMALL_FILE_TIMEOUT: Duration = Duration::from_secs(30);
const ARCHIVE_TIMEOUT: Duration = Duration::from_mins(10);
const DELETED_SUFFIX: &str = " (deleted)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeOutcome {
    UpToDate {
        current: Version,
        latest: Version,
    },
    Upgraded {
        current: Version,
        latest: Version,
        notes_url: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeProgress<'a> {
    Found {
        current: &'a Version,
        latest: &'a Version,
    },
    Downloading {
        received: u64,
        total: Option<u64>,
    },
    Installing,
}

struct Installation {
    version: Version,
    platform: String,
    executable: PathBuf,
}

pub async fn upgrade(
    client: &reqwest::Client,
    _held: &UpgradeLock,
    progress: impl FnMut(UpgradeProgress<'_>),
) -> Result<UpgradeOutcome, UpgradeError> {
    let installation = Installation {
        version: build_identity::release_version().ok_or(UpgradeError::LocalBuild)?,
        platform: build_identity::platform(),
        executable: running_executable()?,
    };
    upgrade_installation(
        client,
        &ReleaseSource::from_environment(),
        &installation,
        progress,
    )
    .await
}

async fn upgrade_installation(
    client: &reqwest::Client,
    source: &ReleaseSource,
    installation: &Installation,
    mut progress: impl FnMut(UpgradeProgress<'_>),
) -> Result<UpgradeOutcome, UpgradeError> {
    archive::ensure_replaceable(&installation.executable)?;
    let current = installation.version.clone();
    let latest = fetch_text(client, &source.latest_pointer_url(), POINTER_LIMIT)
        .await
        .and_then(|pointer| release_source::parse_latest_pointer(&pointer))
        .ok_or(UpgradeError::FetchFailed)?;
    if latest <= current {
        return Ok(UpgradeOutcome::UpToDate { current, latest });
    }
    progress(UpgradeProgress::Found {
        current: &current,
        latest: &latest,
    });
    let archive_url = source.archive_url(&latest, &installation.platform);
    let checksum_file = fetch_text(client, &format!("{archive_url}.sha256"), CHECKSUM_LIMIT)
        .await
        .ok_or(UpgradeError::ChecksumFetchFailed)?;
    let download = Download {
        url: &archive_url,
        limit: usize::MAX,
        timeout: ARCHIVE_TIMEOUT,
    };
    let archive = fetch(client, &download, |received, total| {
        progress(UpgradeProgress::Downloading { received, total });
    })
    .await
    .ok_or(UpgradeError::DownloadFailed)?;
    archive::verify_checksum(&archive, &checksum_file)?;
    progress(UpgradeProgress::Installing);
    let binary = archive::extract_binary(&archive)?;
    archive::install_executable(&installation.executable, &binary, &latest.to_string())?;
    Ok(UpgradeOutcome::Upgraded {
        notes_url: release_source::release_notes_url(&latest.to_string()),
        current,
        latest,
    })
}

struct Download<'a> {
    url: &'a str,
    limit: usize,
    timeout: Duration,
}

async fn fetch_text(client: &reqwest::Client, url: &str, limit: usize) -> Option<String> {
    let download = Download {
        url,
        limit,
        timeout: SMALL_FILE_TIMEOUT,
    };
    let body = fetch(client, &download, |_, _| {}).await?;
    String::from_utf8(body).ok()
}

async fn fetch(
    client: &reqwest::Client,
    download: &Download<'_>,
    mut on_chunk: impl FnMut(u64, Option<u64>),
) -> Option<Vec<u8>> {
    let mut response = client
        .get(download.url)
        .timeout(download.timeout)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let total = response.content_length();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        body.extend_from_slice(&chunk);
        if body.len() > download.limit {
            return None;
        }
        on_chunk(body.len() as u64, total);
    }
    Some(body)
}

fn running_executable() -> Result<PathBuf, UpgradeError> {
    let path = env::current_exe().map_err(|_| UpgradeError::SelfExeNotFound)?;
    let path = match path
        .to_str()
        .and_then(|text| text.strip_suffix(DELETED_SUFFIX))
    {
        Some(live_path) => PathBuf::from(live_path),
        None => path,
    };
    path.canonicalize()
        .map_err(|_| UpgradeError::SelfExeNotFound)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, Permissions};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::thread;

    use super::*;

    const ARCHIVE_PATH: &str = "/download/v0.1.0-dev.9/oh-fx-linux-x86_64.tar.gz";

    fn serve(routes: Vec<(String, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut request = [0; 4096];
                let length = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..length]);
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let (status, body) = routes
                    .iter()
                    .find(|(route, _)| route == path)
                    .map_or(("404 Not Found", Vec::new()), |(_, body)| {
                        ("200 OK", body.clone())
                    });
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        format!("http://{address}")
    }

    fn release_archive() -> Vec<u8> {
        archive::archive_with(&[("oh-fx", &archive::version_script("0.1.0-dev.9"))])
    }

    fn release_routes(checksum_archive: &[u8]) -> Vec<(String, Vec<u8>)> {
        let archive = release_archive();
        vec![
            (
                "/latest/download/latest.txt".to_owned(),
                b"v0.1.0-dev.9".to_vec(),
            ),
            (
                format!("{ARCHIVE_PATH}.sha256"),
                archive::sha256_line(checksum_archive, "a").into_bytes(),
            ),
            (ARCHIVE_PATH.to_owned(), archive),
        ]
    }

    fn installed(version: &str, directory: &tempfile::TempDir) -> Installation {
        let executable = directory.path().join("oh-fx");
        fs::write(&executable, archive::version_script(version)).unwrap();
        fs::set_permissions(&executable, Permissions::from_mode(0o755)).unwrap();
        Installation {
            version: Version::parse(version).unwrap(),
            platform: "linux-x86_64".to_owned(),
            executable,
        }
    }

    async fn run(
        routes: Vec<(String, Vec<u8>)>,
        installation: &Installation,
    ) -> Result<UpgradeOutcome, UpgradeError> {
        let client = ofx_http::build_client("oh-fx/test").unwrap();
        let source = ReleaseSource::at(serve(routes));
        upgrade_installation(&client, &source, installation, |_| {}).await
    }

    #[tokio::test]
    async fn installs_a_newer_release_over_the_executable() {
        let directory = tempfile::tempdir().unwrap();
        let installation = installed("0.1.0-dev.8", &directory);
        let outcome = run(release_routes(&release_archive()), &installation)
            .await
            .unwrap();
        assert!(matches!(outcome, UpgradeOutcome::Upgraded { .. }));
        assert_eq!(
            fs::read(&installation.executable).unwrap(),
            archive::version_script("0.1.0-dev.9")
        );
    }

    #[tokio::test]
    async fn reports_up_to_date_without_downloading() {
        let directory = tempfile::tempdir().unwrap();
        let installation = installed("0.1.0-dev.9", &directory);
        let outcome = run(release_routes(b"unused"), &installation).await.unwrap();
        assert!(matches!(outcome, UpgradeOutcome::UpToDate { .. }));
    }

    #[tokio::test]
    async fn leaves_the_executable_untouched_on_checksum_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let installation = installed("0.1.0-dev.8", &directory);
        let outcome = run(release_routes(b"other archive"), &installation).await;
        assert!(matches!(outcome, Err(UpgradeError::ChecksumMismatch)));
        assert_eq!(
            fs::read(&installation.executable).unwrap(),
            archive::version_script("0.1.0-dev.8")
        );
    }

    #[tokio::test]
    async fn checks_writability_before_any_download() {
        let directory = tempfile::tempdir().unwrap();
        let installation = installed("0.1.0-dev.8", &directory);
        fs::set_permissions(&installation.executable, Permissions::from_mode(0o555)).unwrap();
        let outcome = run(Vec::new(), &installation).await;
        assert!(matches!(outcome, Err(UpgradeError::ReplaceFailed)));
    }

    #[tokio::test]
    async fn fails_to_fetch_when_the_pointer_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let installation = installed("0.1.0-dev.8", &directory);
        let outcome = run(Vec::new(), &installation).await;
        assert!(matches!(outcome, Err(UpgradeError::FetchFailed)));
    }
}
