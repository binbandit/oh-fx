use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ofx_testkit::{FakeServer, OTHER_CA_PEM, Reply, TEST_CA_PEM, TEST_SERVER_CERTIFICATE_PEM};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tempfile::TempDir;

use super::*;

const DEBIAN_BUNDLE: &str = "etc/ssl/certs/ca-certificates.crt";
const FEDORA_BUNDLE: &str = "etc/pki/tls/certs/ca-bundle.crt";
const RHEL_BUNDLE: &str = "etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem";
const ALPINE_BUNDLE: &str = "etc/ssl/cert.pem";

struct Filesystem {
    root: TempDir,
}

impl Filesystem {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    fn distribution(&self) -> Arc<SystemRoots> {
        system(None, None, self.root.path())
    }
}

fn system(file: Option<&Path>, directories: Option<&str>, root: &Path) -> Arc<SystemRoots> {
    Arc::new(SystemRoots::new(Sources::from_variables(
        file.map(|path| path.as_os_str().to_owned()),
        directories.map(OsString::from),
        root,
    )))
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn store(pem: &str) -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.add(certificate(pem)).unwrap();
    roots
}

fn certificate(pem: &str) -> CertificateDer<'static> {
    CertificateDer::from_pem_slice(pem.as_bytes()).unwrap()
}

fn verify(system: &Arc<SystemRoots>, extra: RootCertStore) -> Result<(), Error> {
    let verifier = BundleVerifier::new(Arc::clone(system), extra, provider());
    verifier
        .verify_server_cert(
            &certificate(TEST_SERVER_CERTIFICATE_PEM),
            &[],
            &ServerName::try_from("127.0.0.1").unwrap(),
            &[],
            UnixTime::now(),
        )
        .map(|_| ())
}

fn failure_message(error: &Error) -> String {
    load_failure(error).unwrap_or_else(|| panic!("not a bundle load failure: {error}"))
}

fn loaded_anchors(system: &SystemRoots) -> Vec<rustls::pki_types::TrustAnchor<'static>> {
    system.load().roots.as_ref().unwrap().roots.clone()
}

fn client(system: &Arc<SystemRoots>) -> reqwest::Client {
    let config = configured(Arc::clone(system), RootCertStore::empty(), provider()).unwrap();
    reqwest::Client::builder()
        .tls_backend_preconfigured(config)
        .build()
        .unwrap()
}

#[test]
fn environment_variables_follow_rustls_native_certs() {
    let root = Path::new("/sysroot");
    let unset = Sources::from_variables(None, None, root);
    let Sources::Distribution {
        bundles,
        directories,
    } = &unset
    else {
        panic!("expected the distribution bundles");
    };
    let expected_bundles = [
        DEBIAN_BUNDLE,
        FEDORA_BUNDLE,
        "etc/ssl/ca-bundle.pem",
        "etc/pki/tls/cacert.pem",
        RHEL_BUNDLE,
        ALPINE_BUNDLE,
        "opt/etc/ssl/certs/ca-certificates.crt",
        "etc/ssl/certs/cacert.pem",
    ];
    assert_eq!(bundles, &rooted(root, &expected_bundles));
    let expected_directories = [
        "etc/ssl/certs",
        "etc/pki/tls/certs",
        "etc/security/certificates",
    ];
    assert_eq!(directories, &rooted(root, &expected_directories));

    let empty_directory_list = Sources::from_variables(None, Some(OsString::new()), root);
    assert!(matches!(empty_directory_list, Sources::Distribution { .. }));

    let file_only = Sources::from_variables(Some("/certs/ca.pem".into()), None, root);
    assert!(matches!(
        &file_only,
        Sources::Environment { file: Some(file), directories } if file == Path::new("/certs/ca.pem") && directories.is_empty()
    ));

    let directories_only = Sources::from_variables(None, Some("/a::/b".into()), root);
    assert!(matches!(
        &directories_only,
        Sources::Environment { file: None, directories } if directories == &[PathBuf::from("/a"), PathBuf::from("/b")]
    ));

    let empty_file = Sources::from_variables(Some(OsString::new()), None, root);
    assert!(matches!(
        &empty_file,
        Sources::Environment { file: Some(file), .. } if file.as_os_str().is_empty()
    ));
}

#[test]
fn the_debian_bundle_verifies_without_scanning_the_certificate_directory() {
    let filesystem = Filesystem::new();
    filesystem.write(DEBIAN_BUNDLE, TEST_CA_PEM);
    filesystem.write("etc/ssl/certs/5e17d00d.0", OTHER_CA_PEM);
    let system = filesystem.distribution();
    verify(&system, RootCertStore::empty()).unwrap();
    assert!(system.load().from_bundle);
    assert_eq!(loaded_anchors(&system), store(TEST_CA_PEM).roots);
    assert!(system.widened.get().is_none());
}

#[test]
fn fedora_rhel_and_alpine_bundles_are_found() {
    for bundle in [FEDORA_BUNDLE, RHEL_BUNDLE, ALPINE_BUNDLE] {
        let filesystem = Filesystem::new();
        filesystem.write(bundle, TEST_CA_PEM);
        let system = filesystem.distribution();
        verify(&system, RootCertStore::empty()).unwrap_or_else(|error| panic!("{bundle}: {error}"));
        assert!(system.load().from_bundle, "{bundle}");
        assert!(system.widened.get().is_none(), "{bundle}");
    }
}

#[test]
fn the_first_bundle_in_upstream_order_is_the_only_one_read() {
    let filesystem = Filesystem::new();
    filesystem.write(DEBIAN_BUNDLE, OTHER_CA_PEM);
    filesystem.write(ALPINE_BUNDLE, TEST_CA_PEM);
    let system = filesystem.distribution();
    assert_eq!(loaded_anchors(&system), store(OTHER_CA_PEM).roots);
    let error = verify(&system, RootCertStore::empty()).unwrap_err();
    assert_eq!(
        error,
        Error::InvalidCertificate(CertificateError::UnknownIssuer)
    );
}

#[test]
fn a_ca_only_in_the_certificate_directory_is_found_through_the_fallback() {
    let filesystem = Filesystem::new();
    filesystem.write(DEBIAN_BUNDLE, OTHER_CA_PEM);
    filesystem.write("etc/ssl/certs/5e17d00d.0", TEST_CA_PEM);
    filesystem.write("etc/ssl/certs/README", "not a certificate");
    let system = filesystem.distribution();
    verify(&system, RootCertStore::empty()).unwrap();
    assert!(system.load().from_bundle);
    let widened = system.widened.get().unwrap().as_ref().unwrap();
    assert_eq!(widened.len(), 2);
    fs::remove_file(filesystem.path("etc/ssl/certs/5e17d00d.0")).unwrap();
    verify(&system, RootCertStore::empty()).unwrap();
}

#[test]
fn an_unknown_issuer_stays_unknown_when_the_directory_adds_nothing() {
    let filesystem = Filesystem::new();
    filesystem.write(DEBIAN_BUNDLE, OTHER_CA_PEM);
    let system = filesystem.distribution();
    let error = verify(&system, RootCertStore::empty()).unwrap_err();
    assert_eq!(
        error,
        Error::InvalidCertificate(CertificateError::UnknownIssuer)
    );
    assert!(system.widened.get().unwrap().is_none());
}

#[test]
fn without_a_bundle_the_certificate_directories_are_read_up_front() {
    let filesystem = Filesystem::new();
    filesystem.write("etc/pki/tls/certs/5e17d00d.0", TEST_CA_PEM);
    let system = filesystem.distribution();
    verify(&system, RootCertStore::empty()).unwrap();
    assert!(!system.load().from_bundle);
}

#[test]
fn a_missing_bundle_is_a_named_failure_that_is_not_retried() {
    let filesystem = Filesystem::new();
    let system = filesystem.distribution();
    let error = verify(&system, RootCertStore::empty()).unwrap_err();
    let bundle = filesystem.path(DEBIAN_BUNDLE);
    assert_eq!(
        failure_message(&error),
        format!(
            "no CA certificate bundle at {} or the other standard locations",
            bundle.display()
        )
    );
    filesystem.write(DEBIAN_BUNDLE, TEST_CA_PEM);
    let again = verify(&system, RootCertStore::empty()).unwrap_err();
    assert_eq!(failure_message(&again), failure_message(&error));
}

#[test]
fn a_bundle_without_certificates_names_the_bundle() {
    let filesystem = Filesystem::new();
    let bundle = filesystem.write(ALPINE_BUNDLE, "not a certificate");
    let system = filesystem.distribution();
    let error = verify(&system, RootCertStore::empty()).unwrap_err();
    assert_eq!(
        failure_message(&error),
        format!("no usable CA certificates in {}", bundle.display())
    );
}

#[test]
fn ssl_cert_file_and_dir_replace_the_distribution_bundle() {
    let filesystem = Filesystem::new();
    filesystem.write(DEBIAN_BUNDLE, TEST_CA_PEM);
    let other = filesystem.write("env/other.pem", OTHER_CA_PEM);
    let test = filesystem.write("env/dir/test.pem", TEST_CA_PEM);

    let file_only = system(Some(&other), None, filesystem.root.path());
    let error = verify(&file_only, RootCertStore::empty()).unwrap_err();
    assert_eq!(
        error,
        Error::InvalidCertificate(CertificateError::UnknownIssuer)
    );
    assert!(file_only.widened.get().unwrap().is_none());

    let directory = test.parent().unwrap().to_str().unwrap();
    let with_directory = system(Some(&other), Some(directory), filesystem.root.path());
    verify(&with_directory, RootCertStore::empty()).unwrap();

    let missing = filesystem.path("env/missing.pem");
    let missing_file = system(Some(&missing), None, filesystem.root.path());
    let error = verify(&missing_file, RootCertStore::empty()).unwrap_err();
    assert_eq!(
        failure_message(&error),
        format!("no CA certificates found at {}", missing.display())
    );
}

#[test]
fn ca_file_roots_are_merged_with_the_system_roots() {
    let filesystem = Filesystem::new();
    filesystem.write(DEBIAN_BUNDLE, OTHER_CA_PEM);
    let system = filesystem.distribution();
    verify(&system, store(TEST_CA_PEM)).unwrap();
    assert_eq!(loaded_anchors(&system), store(OTHER_CA_PEM).roots);

    let without_roots = Filesystem::new().distribution();
    verify(&without_roots, store(TEST_CA_PEM)).unwrap();
    let error = verify(&without_roots, store(OTHER_CA_PEM)).unwrap_err();
    assert!(failure_message(&error).starts_with("no CA certificate bundle at "));
}

#[tokio::test]
async fn plain_http_requests_never_load_the_roots() {
    let server = FakeServer::start([Reply::status(200, "{}")]);
    let system = Filesystem::new().distribution();
    let response = client(&system)
        .get(format!("{}/models", server.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(system.loaded.get().is_none());
}

#[tokio::test]
async fn https_requests_read_one_bundle_once_and_offer_only_http_1_1() {
    let server = FakeServer::start_tls([Reply::status(200, "{}"), Reply::status(200, "{}")]);
    let filesystem = Filesystem::new();
    let bundle = filesystem.write(DEBIAN_BUNDLE, TEST_CA_PEM);
    let system = filesystem.distribution();
    let url = format!("{}/models", server.base_url());
    let response = client(&system).get(&url).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert!(system.load().from_bundle);
    assert!(system.widened.get().is_none());
    fs::remove_file(bundle).unwrap();
    let again = client(&system).get(&url).send().await.unwrap();
    assert_eq!(again.status(), 200);
    assert_eq!(
        server.offered_protocols(),
        [vec!["http/1.1".to_owned()], vec!["http/1.1".to_owned()]]
    );
}

#[tokio::test]
async fn https_load_failures_surface_as_named_errors() {
    let server = FakeServer::start_tls([Reply::status(200, "{}")]);
    let filesystem = Filesystem::new();
    let system = filesystem.distribution();
    let error = client(&system)
        .get(format!("{}/models", server.base_url()))
        .send()
        .await
        .unwrap_err();
    let message = load_failure(&error).unwrap();
    assert!(
        message.contains(&filesystem.path(DEBIAN_BUNDLE).display().to_string()),
        "{message}"
    );
    assert_eq!(server.offered_protocols().len(), 1);
    assert!(server.requests().is_empty());
}
