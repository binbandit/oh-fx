use std::io::Write;
use std::str::FromStr;

use flate2::Compression;
use flate2::write::{GzEncoder, ZlibEncoder};
use ofx_contract::BoxFuture;
use ofx_testkit::{ConnectProxy, FakeServer, Reply, WEB_CA_PEM};
use reqwest::{NoProxy, Proxy};

use super::*;
use crate::web::url_policy::normalize;

struct Fixture {
    server: FakeServer,
    proxy: ConnectProxy,
    transport: Transport,
    _roots: tempfile::TempDir,
}

impl Fixture {
    fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self::resolving(replies, unresolved)
    }

    fn resolving(replies: impl IntoIterator<Item = Reply>, lookup: Lookup) -> Self {
        Self::configured(replies, lookup, None)
    }

    fn configured(
        replies: impl IntoIterator<Item = Reply>,
        lookup: Lookup,
        no_proxy: Option<&str>,
    ) -> Self {
        let server = FakeServer::start_web_tls(replies);
        let proxy = ConnectProxy::start(server.address());
        let roots = tempfile::tempdir().unwrap();
        let ca_file = roots.path().join("web-ca.pem");
        std::fs::write(&ca_file, WEB_CA_PEM).unwrap();
        let rule = Proxy::all(proxy.url())
            .unwrap()
            .no_proxy(no_proxy.and_then(NoProxy::from_string));
        let transport = Transport::through(rule, ca_file, lookup);
        Self {
            server,
            proxy,
            transport,
            _roots: roots,
        }
    }

    async fn fetch(&self, url: &str) -> Result<Retrieved, TransportError> {
        self.transport
            .fetch(&normalize(url).unwrap(), &CancellationToken::new())
            .await
    }
}

fn response(retrieved: Result<Retrieved, TransportError>) -> Response {
    match retrieved {
        Ok(Retrieved::Response(response)) => response,
        other => panic!("expected a response, got {other:?}"),
    }
}

fn answer(addresses: &[&str]) -> BoxFuture<'static, io::Result<Vec<SocketAddr>>> {
    let addresses = addresses
        .iter()
        .map(|address| SocketAddr::new(address.parse().unwrap(), 0))
        .collect();
    Box::pin(async move { Ok(addresses) })
}

fn unresolved(_: String) -> BoxFuture<'static, io::Result<Vec<SocketAddr>>> {
    Box::pin(async { Err(io::ErrorKind::NotFound.into()) })
}

fn no_answer(_: String) -> BoxFuture<'static, io::Result<Vec<SocketAddr>>> {
    answer(&[])
}

fn public_answer(_: String) -> BoxFuture<'static, io::Result<Vec<SocketAddr>>> {
    answer(&["93.184.216.34", "2606:2800:220:1:248:1893:25c8:1946"])
}

fn private_answer(_: String) -> BoxFuture<'static, io::Result<Vec<SocketAddr>>> {
    answer(&["10.0.0.1"])
}

fn mixed_answers(_: String) -> BoxFuture<'static, io::Result<Vec<SocketAddr>>> {
    answer(&["93.184.216.34", "::ffff:127.0.0.1"])
}

fn private_www(host: String) -> BoxFuture<'static, io::Result<Vec<SocketAddr>>> {
    if host.starts_with("www.") {
        answer(&["169.254.169.254"])
    } else {
        unresolved(host)
    }
}

fn redirect(status: u16, location: &str) -> Reply {
    Reply::status_with_headers(status, &[("Location", location)], "")
}

fn encoded(status: u16, encodings: &[&str], body: Vec<u8>) -> Response {
    Response {
        final_url: "https://example.test/".to_owned(),
        status,
        content_type: None,
        content_encodings: encodings.iter().map(|value| (*value).to_owned()).collect(),
        body,
    }
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn zlib(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn rejects_mixed_public_private_dns_answers_without_dialing() {
    let public: SocketAddr = "93.184.216.34:0".parse().unwrap();
    let private: SocketAddr = "10.0.0.1:0".parse().unwrap();
    assert_eq!(admitted(vec![public]), Ok(vec![public]));
    assert_eq!(
        admitted(vec![public, private]),
        Err(ResolveFailure::NonPublicDnsAnswer)
    );
    assert_eq!(admitted(Vec::new()), Err(ResolveFailure::NoAddressReturned));
}

#[tokio::test]
async fn the_pinned_name_is_answered_without_a_second_lookup() {
    let resolver = PinnedResolver::default();
    let public: SocketAddr = "93.184.216.34:0".parse().unwrap();
    resolver.pin("docs.example.test", Ok(vec![public]));
    let answered: Vec<SocketAddr> = resolver
        .resolve(Name::from_str("Docs.Example.Test").unwrap())
        .await
        .unwrap()
        .collect();
    assert_eq!(answered, [public]);
    resolver.pin("docs.example.test", Err(ResolveFailure::UnknownHostName));
    let error = resolver
        .resolve(Name::from_str("docs.example.test").unwrap())
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.downcast_ref::<ResolveFailure>(),
        Some(&ResolveFailure::UnknownHostName)
    );
    let unpinned = resolver
        .resolve(Name::from_str("localhost").unwrap())
        .await
        .unwrap();
    assert!(unpinned.count() > 0);
}

#[tokio::test]
async fn refuses_names_that_resolve_locally_to_non_public_addresses_before_using_the_proxy() {
    for lookup in [private_answer as Lookup, mixed_answers] {
        let fixture = Fixture::resolving([], lookup);
        assert_eq!(
            fixture.fetch("https://docs.example.test/").await,
            Err(TransportError("NonPublicDnsAnswer"))
        );
        assert!(fixture.proxy.targets().is_empty());
        assert!(fixture.server.requests().is_empty());
    }
}

#[tokio::test]
async fn continues_through_the_proxy_when_the_local_answer_is_public_or_missing() {
    for lookup in [public_answer as Lookup, unresolved, no_answer] {
        let fixture = Fixture::resolving([Reply::status(200, "ok")], lookup);
        assert_eq!(
            response(fixture.fetch("https://docs.example.test/").await).body,
            b"ok"
        );
        assert_eq!(fixture.proxy.targets(), ["docs.example.test:443"]);
    }
}

#[tokio::test]
async fn each_redirect_hop_is_checked_against_its_own_local_answer() {
    let fixture = Fixture::resolving(
        [redirect(302, "https://www.example.test/next")],
        private_www,
    );
    assert_eq!(
        fixture.fetch("https://example.test/start").await,
        Err(TransportError("NonPublicDnsAnswer"))
    );
    assert_eq!(fixture.proxy.targets(), ["example.test:443"]);
    assert_eq!(fixture.server.requests().len(), 1);
}

#[tokio::test]
async fn no_proxy_hosts_take_the_direct_path_pinned_to_the_local_answer() {
    for (lookup, error) in [
        (unresolved as Lookup, "UnknownHostName"),
        (no_answer, "NoAddressReturned"),
    ] {
        let fixture = Fixture::configured(
            [Reply::status(200, "proxied")],
            lookup,
            Some("docs.example.test"),
        );
        assert_eq!(
            fixture.fetch("https://docs.example.test/").await,
            Err(TransportError(error))
        );
        assert!(fixture.proxy.targets().is_empty());
        assert_eq!(
            response(fixture.fetch("https://other.example.test/").await).body,
            b"proxied"
        );
        assert_eq!(fixture.proxy.targets(), ["other.example.test:443"]);
    }
}

#[test]
fn advertises_and_decodes_supported_content_codings() {
    let page = b"<p>hello</p>".to_vec();
    for (encoding, body) in [
        ("gzip", gzip(&page)),
        ("GZIP", gzip(&page)),
        ("deflate", zlib(&page)),
        ("identity", page.clone()),
    ] {
        assert_eq!(
            settle(encoded(200, &[encoding], body)),
            Ok(Settled::Success { body: page.clone() }),
            "{encoding}"
        );
    }
    assert_eq!(ACCEPT_ENCODING, "gzip, deflate");
}

#[test]
fn rejects_repeated_unknown_and_corrupt_content_encodings() {
    let cases = [
        (vec!["gzip", "gzip"], gzip(b"x"), "gzip"),
        (vec!["br"], b"x".to_vec(), "br"),
        (vec!["zstd"], b"x".to_vec(), "zstd"),
        (vec!["gzip, identity"], gzip(b"x"), "gzip, identity"),
        (vec!["gzip"], b"not gzip".to_vec(), "gzip"),
    ];
    for (encodings, body, expected) in cases {
        assert_eq!(
            settle(encoded(200, &encodings, body)),
            Ok(Settled::UnexpectedContentEncoding {
                encoding: expected.to_owned()
            }),
            "{encodings:?}"
        );
    }
}

#[test]
fn encoded_error_statuses_are_unexpected_and_plain_ones_keep_their_body() {
    assert_eq!(
        settle(encoded(404, &["gzip"], gzip(b"missing"))),
        Ok(Settled::UnexpectedContentEncoding {
            encoding: "gzip".to_owned()
        })
    );
    assert_eq!(
        settle(encoded(404, &[], b"missing".to_vec())),
        Ok(Settled::NonSuccessStatus {
            body: b"missing".to_vec()
        })
    );
    assert_eq!(
        settle(encoded(500, &["identity"], b"broken".to_vec())),
        Ok(Settled::NonSuccessStatus {
            body: b"broken".to_vec()
        })
    );
}

#[test]
fn decoded_body_cap_is_inclusive() {
    let exact = vec![b'a'; MAX_BODY_BYTES];
    assert_eq!(
        settle(encoded(200, &["gzip"], gzip(&exact))),
        Ok(Settled::Success { body: exact })
    );
    let over = vec![b'a'; MAX_BODY_BYTES + 1];
    assert_eq!(
        settle(encoded(200, &["gzip"], gzip(&over))),
        Err(TransportError("BodyTooLarge"))
    );
}

#[tokio::test]
async fn fetches_through_the_configured_proxy_with_upstream_request_headers() {
    let fixture = Fixture::new([Reply::status_with_headers(
        200,
        &[("Content-Type", "text/html; charset=utf-8")],
        "<p>hi</p>",
    )]);
    let fetched = response(fixture.fetch("http://Docs.Example.Test/a?b=1#frag").await);
    assert_eq!(fetched.final_url, "https://docs.example.test/a?b=1");
    assert_eq!(fetched.status, 200);
    assert_eq!(
        fetched.content_type.as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(fetched.body, b"<p>hi</p>");
    assert_eq!(fixture.proxy.targets(), ["docs.example.test:443"]);
    let requests = fixture.server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/a?b=1");
    assert_eq!(request.header("host"), Some("docs.example.test"));
    assert_eq!(request.header("user-agent"), Some(USER_AGENT));
    assert_eq!(request.header("accept"), Some(ACCEPT));
    assert_eq!(request.header("accept-encoding"), Some(ACCEPT_ENCODING));
    assert_eq!(request.header("connection"), Some("close"));
}

#[tokio::test]
async fn follows_same_host_redirects_and_see_other_with_a_fresh_hop() {
    let fixture = Fixture::new([
        redirect(301, "/moved"),
        redirect(303, "https://www.example.test/final"),
        Reply::status_with_headers(200, &[("Content-Type", "text/plain")], "done"),
    ]);
    let fetched = response(fixture.fetch("https://example.test/start").await);
    assert_eq!(fetched.final_url, "https://www.example.test/final");
    assert_eq!(fetched.body, b"done");
    let paths: Vec<String> = fixture
        .server
        .requests()
        .into_iter()
        .map(|request| request.path)
        .collect();
    assert_eq!(paths, ["/start", "/moved", "/final"]);
    assert_eq!(
        fixture.proxy.targets(),
        [
            "example.test:443",
            "example.test:443",
            "www.example.test:443"
        ]
    );
}

#[tokio::test]
async fn cross_host_redirect_returns_reinvocation_result_without_following() {
    let fixture = Fixture::new([redirect(302, "https://other.example.test/next")]);
    assert_eq!(
        fixture.fetch("https://docs.example.test/").await,
        Ok(Retrieved::CrossHostRedirect(
            "https://other.example.test/next".to_owned()
        ))
    );
    assert_eq!(fixture.server.requests().len(), 1);
}

#[tokio::test]
async fn blocks_unsafe_redirects_before_fetching_the_redirected_target() {
    let fixture = Fixture::new([
        redirect(302, "http://127.0.0.1:3000/private"),
        redirect(307, ""),
        Reply::status(302, ""),
    ]);
    assert_eq!(
        fixture.fetch("https://docs.example.test/").await,
        Err(TransportError("NonPublicAddress"))
    );
    assert_eq!(
        fixture.fetch("https://docs.example.test/").await,
        Err(TransportError("MalformedLocation"))
    );
    assert_eq!(
        fixture.fetch("https://docs.example.test/").await,
        Err(TransportError("MissingRedirectLocation"))
    );
    assert_eq!(fixture.server.requests().len(), 3);
}

#[tokio::test]
async fn stops_after_ten_redirect_hops() {
    let fixture = Fixture::new((0..11).map(|hop| redirect(302, &format!("/hop{hop}"))));
    assert_eq!(
        fixture.fetch("https://docs.example.test/").await,
        Err(TransportError("RedirectLimitExceeded"))
    );
    assert_eq!(fixture.server.requests().len(), 11);
}

#[tokio::test]
async fn non_redirect_statuses_and_encodings_are_left_for_settlement() {
    let fixture = Fixture::new([
        Reply::status_with_headers(304, &[("Location", "/elsewhere")], ""),
        Reply::Raw(
            b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Encoding: br\r\nContent-Type: text/plain\r\nContent-Type: text/html\r\nConnection: close\r\nContent-Length: 2\r\n\r\nhi"
                .to_vec(),
        ),
    ]);
    let not_modified = response(fixture.fetch("https://docs.example.test/").await);
    assert_eq!(not_modified.status, 304);
    let repeated = response(fixture.fetch("https://docs.example.test/").await);
    assert_eq!(repeated.content_encodings, ["gzip", "br"]);
    assert_eq!(repeated.content_type.as_deref(), Some("text/html"));
}

#[tokio::test]
async fn bodies_over_ten_mebibytes_fail_before_they_are_read() {
    let declared = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        MAX_BODY_BYTES + 1
    );
    let fixture = Fixture::new([Reply::Raw(declared.into_bytes())]);
    assert_eq!(
        fixture.fetch("https://docs.example.test/").await,
        Err(TransportError("BodyTooLarge"))
    );
}

#[tokio::test]
async fn names_certificate_mismatches_and_cancellation() {
    let fixture = Fixture::new([]);
    assert_eq!(
        fixture.fetch("https://example.org/").await,
        Err(TransportError("CertificateHostMismatch"))
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        fixture
            .transport
            .fetch(&normalize("https://docs.example.test/").unwrap(), &cancel)
            .await,
        Err(TransportError("Canceled"))
    );
}
