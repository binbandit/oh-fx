use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{ClientBuilder, Proxy};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct ConnectionOptions {
    pub user_agent: String,
    pub default_headers: Vec<(String, String)>,
    pub ca_file: Option<PathBuf>,
    pub proxy: Option<String>,
    pub follow_redirects: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("could not read CA file {path}: {source}")]
    CaFileUnreadable { path: PathBuf, source: io::Error },
    #[error("CA file {path} does not contain a valid PEM certificate")]
    CaFileInvalid { path: PathBuf },
    #[error("invalid proxy URL")]
    InvalidProxy,
    #[error("invalid value for header {name}")]
    InvalidHeader { name: String },
    #[error("could not configure TLS: {0}")]
    Tls(#[source] rustls::Error),
    #[error("could not build the HTTP client: {0}")]
    Build(#[source] reqwest::Error),
}

pub fn build_connection_client(
    options: &ConnectionOptions,
) -> Result<reqwest::Client, ClientError> {
    connection_client_builder(options)?
        .build()
        .map_err(ClientError::Build)
}

pub fn connection_client_builder(
    options: &ConnectionOptions,
) -> Result<ClientBuilder, ClientError> {
    install_crypto_provider();
    let mut builder = reqwest::Client::builder()
        .user_agent(&options.user_agent)
        .connect_timeout(CONNECT_TIMEOUT)
        .default_headers(header_map(&options.default_headers)?);
    if !options.follow_redirects {
        builder = builder.redirect(Policy::none());
    }
    builder = trusted_roots(builder, options.ca_file.as_deref())?;
    if let Some(proxy) = &options.proxy {
        let proxy = Proxy::all(proxy).map_err(|_| ClientError::InvalidProxy)?;
        builder = builder.proxy(proxy);
    }
    Ok(builder)
}

#[cfg(target_os = "linux")]
pub fn warm_tls_roots() {
    crate::ca_bundle::warm();
}

#[cfg(not(target_os = "linux"))]
pub fn warm_tls_roots() {}

#[cfg(target_os = "linux")]
pub fn certificate_bundle_load_failure(error: &reqwest::Error) -> Option<String> {
    crate::ca_bundle::load_failure(error)
}

#[cfg(not(target_os = "linux"))]
pub fn certificate_bundle_load_failure(_: &reqwest::Error) -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn trusted_roots(
    builder: ClientBuilder,
    ca_file: Option<&Path>,
) -> Result<ClientBuilder, ClientError> {
    let mut extra = rustls::RootCertStore::empty();
    if let Some(path) = ca_file {
        for certificate in read_certificates(path)? {
            extra
                .add(certificate)
                .map_err(|_| ClientError::CaFileInvalid {
                    path: path.to_owned(),
                })?;
        }
    }
    let config = crate::ca_bundle::client_config(extra).map_err(ClientError::Tls)?;
    Ok(builder.tls_backend_preconfigured(config))
}

#[cfg(not(target_os = "linux"))]
fn trusted_roots(
    builder: ClientBuilder,
    ca_file: Option<&Path>,
) -> Result<ClientBuilder, ClientError> {
    let mut roots = Vec::new();
    if let Some(path) = platform_ignored_certificate_file() {
        roots.extend(read_certificates(&path).unwrap_or_default());
    }
    if let Some(path) = ca_file {
        roots.extend(read_certificates(path)?);
    }
    if roots.is_empty() {
        return Ok(builder);
    }
    let roots = roots
        .iter()
        .map(|der| reqwest::Certificate::from_der(der))
        .collect::<Result<Vec<_>, _>>()
        .map_err(ClientError::Build)?;
    Ok(builder.tls_certs_merge(roots))
}

fn header_map(headers: &[(String, String)]) -> Result<HeaderMap, ClientError> {
    let mut map = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let invalid = || ClientError::InvalidHeader { name: name.clone() };
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?;
        let mut header_value = HeaderValue::from_str(value).map_err(|_| invalid())?;
        header_value.set_sensitive(true);
        map.append(header_name, header_value);
    }
    Ok(map)
}

fn read_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, ClientError> {
    let pem = fs::read(path).map_err(|source| ClientError::CaFileUnreadable {
        path: path.to_owned(),
        source,
    })?;
    let invalid = || ClientError::CaFileInvalid {
        path: path.to_owned(),
    };
    let certificates = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid())?;
    if certificates.is_empty() {
        return Err(invalid());
    }
    Ok(certificates)
}

#[cfg(not(target_os = "linux"))]
fn platform_ignored_certificate_file() -> Option<PathBuf> {
    if !platform_verifier_ignores_certificate_file() {
        return None;
    }
    std::env::var_os("SSL_CERT_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(not(target_os = "linux"))]
const fn platform_verifier_ignores_certificate_file() -> bool {
    cfg!(any(target_vendor = "apple", windows))
}

fn install_crypto_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use ofx_testkit::TEST_CA_PEM;

    use super::*;

    #[test]
    fn connection_clients_accept_headers_and_proxies() {
        let options = ConnectionOptions {
            user_agent: "oh-fx/test".to_owned(),
            default_headers: vec![("x-portkey-api-key".to_owned(), "secret".to_owned())],
            ca_file: None,
            proxy: Some("http://127.0.0.1:3128".to_owned()),
            follow_redirects: false,
        };
        assert!(build_connection_client(&options).is_ok());
    }

    #[test]
    fn invalid_headers_name_the_header_without_the_value() {
        let options = ConnectionOptions {
            default_headers: vec![("x-key".to_owned(), "line\nbreak".to_owned())],
            ..ConnectionOptions::default()
        };
        let error = build_connection_client(&options).unwrap_err();
        assert_eq!(error.to_string(), "invalid value for header x-key");
    }

    #[test]
    fn invalid_proxies_are_rejected_without_echoing_credentials() {
        let options = ConnectionOptions {
            proxy: Some("http://user:hunter2@::not a url::".to_owned()),
            ..ConnectionOptions::default()
        };
        let error = build_connection_client(&options).unwrap_err();
        assert!(matches!(error, ClientError::InvalidProxy));
        assert_eq!(error.to_string(), "invalid proxy URL");
    }

    #[test]
    fn missing_and_empty_ca_files_are_reported() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.pem");
        let options = ConnectionOptions {
            ca_file: Some(missing),
            ..ConnectionOptions::default()
        };
        assert!(matches!(
            build_connection_client(&options),
            Err(ClientError::CaFileUnreadable { .. })
        ));
        let empty = directory.path().join("empty.pem");
        fs::write(&empty, "not a certificate").unwrap();
        let options = ConnectionOptions {
            ca_file: Some(empty),
            ..ConnectionOptions::default()
        };
        assert!(matches!(
            build_connection_client(&options),
            Err(ClientError::CaFileInvalid { .. })
        ));
    }

    #[test]
    fn pem_bundles_parse_into_certificates() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ca.pem");
        fs::write(&path, TEST_CA_PEM).unwrap();
        assert_eq!(read_certificates(&path).unwrap().len(), 1);
    }

    #[test]
    fn ca_files_with_a_malformed_certificate_fail_when_the_client_is_built() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ca.pem");
        fs::write(
            &path,
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let options = ConnectionOptions {
            ca_file: Some(path),
            ..ConnectionOptions::default()
        };
        assert!(build_connection_client(&options).is_err());
    }

    #[test]
    fn ca_files_build_clients() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ca.pem");
        fs::write(&path, TEST_CA_PEM).unwrap();
        let options = ConnectionOptions {
            ca_file: Some(path),
            ..ConnectionOptions::default()
        };
        assert!(build_connection_client(&options).is_ok());
    }
}
