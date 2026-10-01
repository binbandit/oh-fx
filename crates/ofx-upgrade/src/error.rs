#[derive(Debug, thiserror::Error)]
pub enum UpgradeError {
    #[error("this is a local build; install a release to enable upgrades")]
    LocalBuild,
    #[error("failed to fetch latest version from GitHub")]
    FetchFailed,
    #[error("failed to download release archive")]
    DownloadFailed,
    #[error("failed to fetch checksum from GitHub")]
    ChecksumFetchFailed,
    #[error("downloaded archive failed integrity check")]
    ChecksumMismatch,
    #[error("failed to extract release archive")]
    ExtractionFailed,
    #[error("could not determine path of running binary")]
    SelfExeNotFound,
    #[error("failed to replace binary (permission denied?)")]
    ReplaceFailed,
    #[error("downloaded binary failed to run")]
    BinaryVerificationFailed,
}
