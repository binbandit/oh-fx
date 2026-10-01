use std::error::Error as StdError;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, OnceLock};
use std::{env, fmt, fs};

use rustix::fs::OFlags;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error, OtherError, RootCertStore,
    SignatureScheme,
};

const CERTIFICATE_FILE_VARIABLE: &str = "SSL_CERT_FILE";
const CERTIFICATE_DIRECTORY_VARIABLE: &str = "SSL_CERT_DIR";
const DISTRIBUTION_BUNDLES: [&str; 8] = [
    "etc/ssl/certs/ca-certificates.crt",
    "etc/pki/tls/certs/ca-bundle.crt",
    "etc/ssl/ca-bundle.pem",
    "etc/pki/tls/cacert.pem",
    "etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "etc/ssl/cert.pem",
    "opt/etc/ssl/certs/ca-certificates.crt",
    "etc/ssl/certs/cacert.pem",
];
const DISTRIBUTION_DIRECTORIES: [&str; 3] = [
    "etc/ssl/certs",
    "etc/pki/tls/certs",
    "etc/security/certificates",
];
const HTTP_1_1: &[u8] = b"http/1.1";

static SYSTEM_ROOTS: LazyLock<Arc<SystemRoots>> =
    LazyLock::new(|| Arc::new(SystemRoots::new(Sources::from_environment())));

pub(crate) fn client_config(extra: RootCertStore) -> Result<ClientConfig, Error> {
    let provider = CryptoProvider::get_default().map_or_else(
        || Arc::new(rustls::crypto::ring::default_provider()),
        Arc::clone,
    );
    configured(Arc::clone(&SYSTEM_ROOTS), extra, provider)
}

pub(crate) fn load_failure(error: &(dyn StdError + 'static)) -> Option<String> {
    let mut next = Some(error);
    while let Some(cause) = next {
        if let Some(failure) = cause.downcast_ref::<CertificateBundleLoadFailure>() {
            return Some(failure.to_string());
        }
        next = wrapped(cause);
    }
    None
}

fn wrapped<'a>(cause: &'a (dyn StdError + 'static)) -> Option<&'a (dyn StdError + 'static)> {
    if let Some(error) = cause.downcast_ref::<io::Error>() {
        return error
            .get_ref()
            .map(|inner| inner as &(dyn StdError + 'static));
    }
    match cause.downcast_ref::<Error>() {
        Some(Error::InvalidCertificate(CertificateError::Other(other)) | Error::Other(other)) => {
            Some(other.0.as_ref())
        }
        _ => cause.source(),
    }
}

fn configured(
    system: Arc<SystemRoots>,
    extra: RootCertStore,
    provider: Arc<CryptoProvider>,
) -> Result<ClientConfig, Error> {
    let verifier = BundleVerifier::new(system, extra, Arc::clone(&provider));
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::ALL_VERSIONS)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![HTTP_1_1.to_vec()];
    Ok(config)
}

enum Sources {
    Environment {
        file: Option<PathBuf>,
        directories: Vec<PathBuf>,
    },
    Distribution {
        bundles: Vec<PathBuf>,
        directories: Vec<PathBuf>,
    },
}

impl Sources {
    fn from_environment() -> Self {
        Self::from_variables(
            env::var_os(CERTIFICATE_FILE_VARIABLE),
            env::var_os(CERTIFICATE_DIRECTORY_VARIABLE),
            Path::new("/"),
        )
    }

    fn from_variables(
        file: Option<OsString>,
        directories: Option<OsString>,
        filesystem_root: &Path,
    ) -> Self {
        let file = file.map(PathBuf::from);
        let directories: Vec<PathBuf> = directories
            .map(|value| {
                env::split_paths(&value)
                    .filter(|path| !path.as_os_str().is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if file.is_none() && directories.is_empty() {
            return Self::Distribution {
                bundles: rooted(filesystem_root, &DISTRIBUTION_BUNDLES),
                directories: rooted(filesystem_root, &DISTRIBUTION_DIRECTORIES),
            };
        }
        Self::Environment { file, directories }
    }
}

fn rooted(root: &Path, paths: &[&str]) -> Vec<PathBuf> {
    paths.iter().map(|path| root.join(path)).collect()
}

struct SystemRoots {
    sources: Sources,
    loaded: OnceLock<Loaded>,
    widened: OnceLock<Option<Arc<RootCertStore>>>,
}

struct Loaded {
    roots: Result<Arc<RootCertStore>, Arc<CertificateBundleLoadFailure>>,
    from_bundle: bool,
}

impl SystemRoots {
    const fn new(sources: Sources) -> Self {
        Self {
            sources,
            loaded: OnceLock::new(),
            widened: OnceLock::new(),
        }
    }

    fn load(&self) -> &Loaded {
        self.loaded.get_or_init(|| match &self.sources {
            Sources::Environment { file, directories } => {
                let mut found = Found::new();
                if let Some(file) = file {
                    found.read_named_file(file);
                }
                for directory in directories {
                    found.read_directory(directory);
                }
                let path = file
                    .as_ref()
                    .or_else(|| directories.first())
                    .cloned()
                    .unwrap_or_default();
                Loaded {
                    roots: found.into_roots(path),
                    from_bundle: false,
                }
            }
            Sources::Distribution {
                bundles,
                directories,
            } => load_distribution(bundles, directories),
        })
    }

    fn widened(&self) -> Option<Arc<RootCertStore>> {
        self.widened
            .get_or_init(|| {
                let Loaded {
                    roots: Ok(bundle),
                    from_bundle: true,
                } = self.load()
                else {
                    return None;
                };
                let Sources::Distribution { directories, .. } = &self.sources else {
                    return None;
                };
                let mut found = Found::new();
                for directory in directories {
                    found.read_standard_directory(directory);
                }
                let mut widened = RootCertStore::clone(bundle);
                for anchor in found.roots.roots {
                    if !widened.roots.contains(&anchor) {
                        widened.roots.push(anchor);
                    }
                }
                (widened.len() > bundle.len()).then(|| Arc::new(widened))
            })
            .clone()
    }
}

fn load_distribution(bundles: &[PathBuf], directories: &[PathBuf]) -> Loaded {
    let mut found = Found::new();
    let bundle = bundles
        .iter()
        .find_map(|path| first_bundle(path).map(|read| (path, read)));
    let path = if let Some((path, read)) = bundle {
        match read {
            Ok(pem) => found.parse(&pem),
            Err(error) => found.note(error),
        }
        if !found.roots.is_empty() {
            return Loaded {
                roots: Ok(Arc::new(found.roots)),
                from_bundle: true,
            };
        }
        path.clone()
    } else {
        found.missing_bundle = true;
        bundles.first().cloned().unwrap_or_default()
    };
    for directory in directories {
        found.read_standard_directory(directory);
    }
    Loaded {
        roots: found.into_roots(path),
        from_bundle: false,
    }
}

fn first_bundle(path: &Path) -> Option<io::Result<Vec<u8>>> {
    match read_regular_file(path) {
        Ok(Some(pem)) => Some(Ok(pem)),
        Ok(None) => None,
        Err(error) if is_missing(&error) => None,
        Err(error) => Some(Err(error)),
    }
}

fn read_regular_file(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(OFlags::NONBLOCK.bits().cast_signed())
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    let mut pem = Vec::new();
    file.read_to_end(&mut pem)?;
    Ok(Some(pem))
}

fn is_missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

struct Found {
    roots: RootCertStore,
    first_error: Option<io::Error>,
    missing_bundle: bool,
}

impl Found {
    fn new() -> Self {
        Self {
            roots: RootCertStore::empty(),
            first_error: None,
            missing_bundle: false,
        }
    }

    fn read_named_file(&mut self, path: &Path) {
        match fs::read(path) {
            Ok(pem) => self.parse(&pem),
            Err(error) => self.note(error),
        }
    }

    fn read_directory(&mut self, path: &Path) {
        if let Err(error) = self.scan_directory(path) {
            self.note(error);
        }
    }

    fn read_standard_directory(&mut self, path: &Path) {
        if let Err(error) = self.scan_directory(path)
            && !is_missing(&error)
        {
            self.note(error);
        }
    }

    fn scan_directory(&mut self, path: &Path) -> io::Result<()> {
        for entry in fs::read_dir(path)? {
            match entry.map(|entry| read_regular_file(&entry.path())) {
                Ok(Ok(Some(pem))) => self.parse(&pem),
                Ok(Ok(None)) => {}
                Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(Err(error)) | Err(error) => self.note(error),
            }
        }
        Ok(())
    }

    fn parse(&mut self, pem: &[u8]) {
        self.roots
            .add_parsable_certificates(CertificateDer::pem_slice_iter(pem).filter_map(Result::ok));
    }

    fn note(&mut self, error: io::Error) {
        self.first_error.get_or_insert(error);
    }

    fn into_roots(
        self,
        path: PathBuf,
    ) -> Result<Arc<RootCertStore>, Arc<CertificateBundleLoadFailure>> {
        if !self.roots.is_empty() {
            return Ok(Arc::new(self.roots));
        }
        let cause = if self.missing_bundle {
            Cause::NoBundle
        } else {
            match self.first_error {
                Some(error) if is_missing(&error) => Cause::NotFound,
                Some(error) => Cause::Unreadable(error),
                None => Cause::NoCertificates,
            }
        };
        Err(Arc::new(CertificateBundleLoadFailure { path, cause }))
    }
}

struct CertificateBundleLoadFailure {
    path: PathBuf,
    cause: Cause,
}

enum Cause {
    NoBundle,
    NotFound,
    Unreadable(io::Error),
    NoCertificates,
}

impl fmt::Display for CertificateBundleLoadFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match &self.cause {
            Cause::NoBundle => write!(
                formatter,
                "no CA certificate bundle at {path} or the other standard locations"
            ),
            Cause::NotFound => write!(formatter, "no CA certificates found at {path}"),
            Cause::Unreadable(error) => {
                write!(
                    formatter,
                    "could not read CA certificates from {path}: {error}"
                )
            }
            Cause::NoCertificates => {
                write!(formatter, "no usable CA certificates in {path}")
            }
        }
    }
}

impl fmt::Debug for CertificateBundleLoadFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl StdError for CertificateBundleLoadFailure {}

struct BundleVerifier {
    system: Arc<SystemRoots>,
    extra: RootCertStore,
    provider: Arc<CryptoProvider>,
    primary: OnceLock<Result<Arc<WebPkiServerVerifier>, Error>>,
    widened: OnceLock<Option<Arc<WebPkiServerVerifier>>>,
}

impl BundleVerifier {
    const fn new(
        system: Arc<SystemRoots>,
        extra: RootCertStore,
        provider: Arc<CryptoProvider>,
    ) -> Self {
        Self {
            system,
            extra,
            provider,
            primary: OnceLock::new(),
            widened: OnceLock::new(),
        }
    }

    fn primary(&self) -> Result<&Arc<WebPkiServerVerifier>, Error> {
        self.primary
            .get_or_init(|| match &self.system.load().roots {
                Ok(system) => self.verifier(self.with_extra(system)),
                Err(_) if !self.extra.is_empty() => self.verifier(Arc::new(self.extra.clone())),
                Err(failure) => Err(load_error(failure)),
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    fn widened(&self) -> Option<&Arc<WebPkiServerVerifier>> {
        self.widened
            .get_or_init(|| {
                let system = self.system.widened()?;
                self.verifier(self.with_extra(&system)).ok()
            })
            .as_ref()
    }

    fn with_extra(&self, system: &Arc<RootCertStore>) -> Arc<RootCertStore> {
        if self.extra.is_empty() {
            return Arc::clone(system);
        }
        let mut roots = RootCertStore::clone(system);
        roots.roots.extend(self.extra.roots.iter().cloned());
        Arc::new(roots)
    }

    fn verifier(&self, roots: Arc<RootCertStore>) -> Result<Arc<WebPkiServerVerifier>, Error> {
        WebPkiServerVerifier::builder_with_provider(roots, Arc::clone(&self.provider))
            .build()
            .map_err(|error| Error::General(error.to_string()))
    }
}

impl fmt::Debug for BundleVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BundleVerifier")
            .finish_non_exhaustive()
    }
}

fn load_error(failure: &Arc<CertificateBundleLoadFailure>) -> Error {
    Error::InvalidCertificate(CertificateError::Other(OtherError(failure.clone())))
}

impl ServerCertVerifier for BundleVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let verified = self.primary()?.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        );
        if !matches!(
            verified,
            Err(Error::InvalidCertificate(CertificateError::UnknownIssuer))
        ) {
            return verified;
        }
        if let Some(widened) = self.widened() {
            return widened.verify_server_cert(
                end_entity,
                intermediates,
                server_name,
                ocsp_response,
                now,
            );
        }
        match &self.system.load().roots {
            Err(failure) => Err(load_error(failure)),
            Ok(_) => verified,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests;
