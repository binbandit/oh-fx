use std::env;
use std::path::PathBuf;
use std::time::Duration;

use semver::Version;

use crate::archive;
use crate::build_identity;
use crate::control::UpgradeControl;
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
    control: &UpgradeControl,
    progress: impl FnMut(UpgradeProgress<'_>),
) -> Result<UpgradeOutcome, UpgradeError> {
    let installation = Installation {
        version: build_identity::release_version().ok_or(UpgradeError::LocalBuild)?,
        platform: build_identity::platform(),
        executable: installed_executable()?,
    };
    upgrade_installation(
        client,
        &ReleaseSource::from_environment(),
        &installation,
        control,
        progress,
    )
    .await
}

async fn upgrade_installation(
    client: &reqwest::Client,
    source: &ReleaseSource,
    installation: &Installation,
    control: &UpgradeControl,
    mut progress: impl FnMut(UpgradeProgress<'_>),
) -> Result<UpgradeOutcome, UpgradeError> {
    control.check()?;
    archive::ensure_replaceable(&installation.executable)?;
    let current = installation.version.clone();
    let latest = fetch_text(client, control, &source.latest_pointer_url(), POINTER_LIMIT)
        .await
        .map_err(|failure| failure.or(UpgradeError::FetchFailed))?
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
    let checksum_file = fetch_text(
        client,
        control,
        &format!("{archive_url}.sha256"),
        CHECKSUM_LIMIT,
    )
    .await
    .map_err(|failure| failure.or(UpgradeError::ChecksumFetchFailed))?
    .ok_or(UpgradeError::ChecksumFetchFailed)?;
    let download = Download {
        url: &archive_url,
        limit: usize::MAX,
        timeout: ARCHIVE_TIMEOUT,
    };
    let archive = fetch(client, control, &download, |received, total| {
        progress(UpgradeProgress::Downloading { received, total });
    })
    .await
    .map_err(|failure| failure.or(UpgradeError::DownloadFailed))?;
    control.check()?;
    archive::verify_checksum(&archive, &checksum_file)?;
    progress(UpgradeProgress::Installing);
    let binary = archive::extract_binary(&archive)?;
    control.install_unless_stopped(|| {
        archive::install_executable(&installation.executable, &binary, &latest.to_string())
    })?;
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

enum Failure {
    Unavailable,
    Cancelled,
}

impl Failure {
    fn or(self, unavailable: UpgradeError) -> UpgradeError {
        match self {
            Self::Unavailable => unavailable,
            Self::Cancelled => UpgradeError::Cancelled,
        }
    }
}

impl From<UpgradeError> for Failure {
    fn from(_: UpgradeError) -> Self {
        Self::Cancelled
    }
}

async fn fetch_text(
    client: &reqwest::Client,
    control: &UpgradeControl,
    url: &str,
    limit: usize,
) -> Result<Option<String>, Failure> {
    let download = Download {
        url,
        limit,
        timeout: SMALL_FILE_TIMEOUT,
    };
    let body = fetch(client, control, &download, |_, _| {}).await?;
    Ok(String::from_utf8(body).ok())
}

async fn fetch(
    client: &reqwest::Client,
    control: &UpgradeControl,
    download: &Download<'_>,
    mut on_chunk: impl FnMut(u64, Option<u64>),
) -> Result<Vec<u8>, Failure> {
    control.check()?;
    let sent = control
        .unless_stopped(client.get(download.url).timeout(download.timeout).send())
        .await?;
    let mut response = sent
        .and_then(reqwest::Response::error_for_status)
        .map_err(|_| Failure::Unavailable)?;
    let total = response.content_length();
    let mut body = Vec::new();
    while let Some(chunk) = control
        .unless_stopped(response.chunk())
        .await?
        .map_err(|_| Failure::Unavailable)?
    {
        body.extend_from_slice(&chunk);
        if body.len() > download.limit {
            return Err(Failure::Unavailable);
        }
        on_chunk(body.len() as u64, total);
    }
    Ok(body)
}

pub fn installed_executable() -> Result<PathBuf, UpgradeError> {
    let path = env::current_exe().map_err(|_| UpgradeError::SelfExeNotFound)?;
    on_disk_path(path)
        .canonicalize()
        .map_err(|_| UpgradeError::SelfExeNotFound)
}

fn on_disk_path(path: PathBuf) -> PathBuf {
    if !cfg!(target_os = "linux") {
        return path;
    }
    match path
        .to_str()
        .and_then(|text| text.strip_suffix(DELETED_SUFFIX))
    {
        Some(on_disk) => PathBuf::from(on_disk),
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, Permissions};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::Instant;

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

    fn client() -> reqwest::Client {
        ofx_http::build_connection_client(&ofx_http::ConnectionOptions {
            user_agent: "oh-fx/test".to_owned(),
            follow_redirects: true,
            ..ofx_http::ConnectionOptions::default()
        })
        .unwrap()
    }

    async fn run(
        routes: Vec<(String, Vec<u8>)>,
        installation: &Installation,
    ) -> Result<UpgradeOutcome, UpgradeError> {
        run_with(routes, installation, &UpgradeControl::new()).await
    }

    async fn run_with(
        routes: Vec<(String, Vec<u8>)>,
        installation: &Installation,
        control: &UpgradeControl,
    ) -> Result<UpgradeOutcome, UpgradeError> {
        let source = ReleaseSource::at(serve(routes));
        upgrade_installation(&client(), &source, installation, control, |_| {}).await
    }

    fn serve_stalled_archive(routes: Vec<(String, Vec<u8>)>) -> (String, mpsc::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (stalled, stalls) = mpsc::channel();
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut request = [0; 4096];
                let length = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..length]);
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                if path == ARCHIVE_PATH {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\npartial",
                    );
                    let _ = stalled.send(());
                    thread::sleep(Duration::from_secs(30));
                    continue;
                }
                let body = routes
                    .iter()
                    .find(|(route, _)| *route == path)
                    .map(|(_, body)| body.clone())
                    .unwrap_or_default();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        (format!("http://{address}"), stalls)
    }

    #[tokio::test]
    async fn a_stop_wakes_a_stalled_download_and_installs_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let installation = installed("0.1.0-dev.8", &directory);
        let (base_url, stalls) = serve_stalled_archive(release_routes(&release_archive()));
        let control = Arc::new(UpgradeControl::new());
        let stopper = Arc::clone(&control);
        thread::spawn(move || {
            if stalls.recv_timeout(Duration::from_secs(10)).is_ok() {
                stopper.request_stop();
            }
        });
        let started = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            upgrade_installation(
                &client(),
                &ReleaseSource::at(base_url),
                &installation,
                &control,
                |_| {},
            ),
        )
        .await
        .expect("the stop wakes the transfer");
        assert!(
            matches!(outcome, Err(UpgradeError::Cancelled)),
            "{outcome:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(
            fs::read(&installation.executable).unwrap(),
            archive::version_script("0.1.0-dev.8")
        );
    }

    #[tokio::test]
    async fn a_stop_before_the_check_fetches_and_installs_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let installation = installed("0.1.0-dev.8", &directory);
        let control = UpgradeControl::new();
        control.request_stop();
        let outcome = run_with(release_routes(&release_archive()), &installation, &control).await;
        assert!(
            matches!(outcome, Err(UpgradeError::Cancelled)),
            "{outcome:?}"
        );
        assert_eq!(
            fs::read(&installation.executable).unwrap(),
            archive::version_script("0.1.0-dev.8")
        );
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

    #[test]
    fn a_replaced_binary_resolves_to_its_on_disk_path_on_linux() {
        let replaced = PathBuf::from("/opt/oh-fx/bin/oh-fx (deleted)");
        let expected = if cfg!(target_os = "linux") {
            PathBuf::from("/opt/oh-fx/bin/oh-fx")
        } else {
            replaced.clone()
        };
        assert_eq!(on_disk_path(replaced), expected);
        assert_eq!(
            on_disk_path(PathBuf::from("/opt/oh-fx/bin/oh-fx")),
            PathBuf::from("/opt/oh-fx/bin/oh-fx")
        );
    }

    #[test]
    fn the_installed_executable_is_the_running_binary_on_disk() {
        assert_eq!(
            installed_executable().unwrap(),
            env::current_exe().unwrap().canonicalize().unwrap()
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
