use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Certificate, Proxy};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CERTIFICATE_FILE_VARIABLE: &str = "SSL_CERT_FILE";

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
    #[error("could not build the HTTP client: {0}")]
    Build(#[source] reqwest::Error),
}

pub fn build_connection_client(
    options: &ConnectionOptions,
) -> Result<reqwest::Client, ClientError> {
    install_crypto_provider();
    let mut builder = reqwest::Client::builder()
        .user_agent(&options.user_agent)
        .connect_timeout(CONNECT_TIMEOUT)
        .default_headers(header_map(&options.default_headers)?);
    if !options.follow_redirects {
        builder = builder.redirect(Policy::none());
    }
    let mut roots = Vec::new();
    if let Some(path) = platform_ignored_certificate_file() {
        roots.extend(read_certificates(&path).unwrap_or_default());
    }
    if let Some(path) = &options.ca_file {
        roots.extend(read_certificates(path)?);
    }
    if !roots.is_empty() {
        builder = builder.tls_certs_merge(roots);
    }
    if let Some(proxy) = &options.proxy {
        let proxy = Proxy::all(proxy).map_err(|_| ClientError::InvalidProxy)?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(ClientError::Build)
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

fn read_certificates(path: &Path) -> Result<Vec<Certificate>, ClientError> {
    let pem = fs::read(path).map_err(|source| ClientError::CaFileUnreadable {
        path: path.to_owned(),
        source,
    })?;
    let certificates =
        Certificate::from_pem_bundle(&pem).map_err(|_| ClientError::CaFileInvalid {
            path: path.to_owned(),
        })?;
    if certificates.is_empty() {
        return Err(ClientError::CaFileInvalid {
            path: path.to_owned(),
        });
    }
    Ok(certificates)
}

fn platform_ignored_certificate_file() -> Option<PathBuf> {
    if !platform_verifier_ignores_certificate_file() {
        return None;
    }
    env::var_os(CERTIFICATE_FILE_VARIABLE)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

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
    use super::*;

    const TEST_CERTIFICATE: &str = "-----BEGIN CERTIFICATE-----
MIIBhjCCAS2gAwIBAgIUd6LE8F704/6Dls/V4KmszcLpJTUwCgYIKoZIzj0EAwIw
GDEWMBQGA1UEAwwNb2gtZnggdGVzdCBDQTAgFw0yNjEwMDEwMTM2MjlaGA8yMTI2
MDkwNzAxMzYyOVowGDEWMBQGA1UEAwwNb2gtZnggdGVzdCBDQTBZMBMGByqGSM49
AgEGCCqGSM49AwEHA0IABLsmF0hSYztpOb0c4nHIzJ44f3HXDxdN1oS596H10ZK5
VVX3il6MdeGvoAQESSdyj74RbM8LBbnvcRLH1/BXXR+jUzBRMB0GA1UdDgQWBBTn
sFJdBZIspogugFBs5l08jRVMUTAfBgNVHSMEGDAWgBTnsFJdBZIspogugFBs5l08
jRVMUTAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0cAMEQCIG3FUzkWvT65
+lOtIHbTn1B9cEj7SE/STu5agJ/SM2I+AiAyr91F7s7uuHy5jPBBrsWIiOJB0Cbk
ktS94Hd1UuEWlg==
-----END CERTIFICATE-----
";

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
        fs::write(&path, TEST_CERTIFICATE).unwrap();
        assert_eq!(read_certificates(&path).unwrap().len(), 1);
    }
}
