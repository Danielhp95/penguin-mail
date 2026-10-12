//! OAuth's production rules without a server that speaks https: what
//! the client refuses to send over http, the redirect's checks, the
//! authorize URL, Debug output and the loopback. The exchanges against
//! wiremock live in `oauth.rs`'s unit tests, which alone may use http.

use mailrs_jmap::JmapError;
use mailrs_jmap::oauth::{self, Loopback, Metadata, OAuthClient, Pkce, Tokens, parse_redirect};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A mock server that fails the test if any request reaches it.
async fn untouchable() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any()).respond_with(ResponseTemplate::new(500)).expect(0).mount(&server).await;
    server
}

fn says_no_address(err: &JmapError, address: &str) {
    let said = err.to_string();
    assert!(!said.contains(address) && !said.contains("127.0.0.1"), "{said}");
}

fn https_metadata() -> Metadata {
    Metadata {
        issuer: "https://auth.example.com".into(),
        authorization_endpoint: "https://auth.example.com/authorize".into(),
        token_endpoint: "https://auth.example.com/token".into(),
        registration_endpoint: Some("https://auth.example.com/register".into()),
        scopes_supported: Vec::new(),
        code_challenge_methods_supported: vec!["S256".into()],
    }
}

#[tokio::test]
async fn metadata_over_http_is_refused_before_any_request() {
    let server = untouchable().await;
    let err = oauth::metadata(&format!("{}/jmap/session", server.uri())).await.unwrap_err();
    assert_eq!(err, JmapError::NotHttps);
    says_no_address(&err, &server.uri());
}

#[tokio::test]
async fn a_token_endpoint_over_http_gets_neither_code_nor_refresh_token() {
    let server = untouchable().await;
    let client = OAuthClient {
        client_id: "c1".into(),
        token_endpoint: format!("{}/token", server.uri()),
        resource: "https://api.example.com/jmap/session".into(),
    };
    let err = client.exchange("code", "http://127.0.0.1:5555/", &Pkce::generate()).await.unwrap_err();
    assert_eq!(err, JmapError::NotHttps);
    says_no_address(&err, &server.uri());
    assert_eq!(client.refresh("r1").await.unwrap_err(), JmapError::NotHttps);
}

#[tokio::test]
async fn a_registration_endpoint_over_http_is_refused() {
    let server = untouchable().await;
    let metadata = Metadata { registration_endpoint: Some(format!("{}/register", server.uri())), ..https_metadata() };
    let err = oauth::register(&metadata, "http://127.0.0.1:5555/", &[]).await.unwrap_err();
    assert_eq!(err, JmapError::NotHttps);
    says_no_address(&err, &server.uri());
}

#[tokio::test]
async fn registration_names_only_the_apps_own_loopback() {
    for redirect in ["http://evil.example/", "https://127.0.0.1/", "http://localhost:5555/", "http://127.0.0.1@evil.example/"] {
        let err = oauth::register(&https_metadata(), redirect, &[]).await.unwrap_err();
        assert_eq!(err, JmapError::BadRedirect, "{redirect}");
        assert!(!err.to_string().contains("evil.example"), "{err}");
    }
}

#[test]
fn an_authorization_endpoint_over_http_is_refused() {
    let metadata = Metadata { authorization_endpoint: "http://auth.example.com/authorize".into(), ..https_metadata() };
    let err = oauth::authorize_url(&metadata, "c1", "http://127.0.0.1:5555/", &[], &Pkce::generate(), "s1", "https://api.example.com/jmap/session", None)
        .unwrap_err();
    assert_eq!(err, JmapError::NotHttps);
    says_no_address(&err, "auth.example.com");
}

#[test]
fn the_authorize_url_hands_back_only_to_the_apps_own_loopback() {
    let err = oauth::authorize_url(&https_metadata(), "c1", "http://evil.example/", &[], &Pkce::generate(), "s1", "https://api.example.com/jmap/session", None)
        .unwrap_err();
    assert_eq!(err, JmapError::BadRedirect);
}

#[test]
fn a_redirect_with_another_state_is_refused() {
    let request = "GET /?code=abc&state=other HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
    assert!(matches!(parse_redirect(request, "mine"), Some(Err(_))));
    let good = "GET /?code=abc&state=mine HTTP/1.1\r\n\r\n";
    assert_eq!(parse_redirect(good, "mine"), Some(Ok("abc".into())));
    let denied = "GET /?error=access_denied&state=mine HTTP/1.1\r\n\r\n";
    assert_eq!(parse_redirect(denied, "mine"), Some(Err(JmapError::Declined)));
    assert_eq!(parse_redirect("GET /favicon.ico HTTP/1.1\r\n\r\n", "mine"), None);
}

#[test]
fn a_refusal_with_another_state_is_not_taken_for_the_persons_answer() {
    let forged = "GET /?error=access_denied&state=other HTTP/1.1\r\n\r\n";
    let outcome = parse_redirect(forged, "mine");
    assert!(matches!(outcome, Some(Err(ref e)) if *e != JmapError::Declined), "{outcome:?}");
}

#[test]
fn the_authorize_url_names_pkce_the_scopes_and_the_resource() {
    let metadata = oauth::Metadata {
        issuer: "https://auth.example.com".into(),
        authorization_endpoint: "https://auth.example.com/authorize".into(),
        token_endpoint: "https://auth.example.com/token".into(),
        registration_endpoint: None,
        scopes_supported: Vec::new(),
        code_challenge_methods_supported: vec!["S256".into()],
    };
    let pkce = Pkce::generate();
    let url = oauth::authorize_url(
        &metadata,
        "c1",
        "http://127.0.0.1:5555/",
        &["urn:ietf:params:oauth:scope:mail".into()],
        &pkce,
        "s1",
        "https://api.example.com/jmap/session",
        Some("me@example.com"),
    )
    .unwrap();
    let parsed = url::Url::parse(&url).unwrap();
    let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
    assert_eq!(pairs["code_challenge_method"], "S256");
    assert_eq!(pairs["code_challenge"], pkce.challenge);
    assert_eq!(pairs["resource"], "https://api.example.com/jmap/session");
    assert_eq!(pairs["login_hint"], "me@example.com");
    assert_eq!(pairs["scope"], "urn:ietf:params:oauth:scope:mail");
}

#[test]
fn secrets_stay_out_of_debug_output() {
    let pkce = Pkce::generate();
    let tokens = Tokens { access_token: "access-secret".into(), expires_in: 60, refresh_token: Some("refresh-secret".into()) };
    let client = OAuthClient {
        client_id: "client-id-secret".into(),
        token_endpoint: "https://auth.example.com/token".into(),
        resource: "https://api.example.com/jmap/session".into(),
    };
    let said = format!("{pkce:?} {tokens:?} {client:?}");
    for secret in [pkce.verifier.as_str(), "access-secret", "refresh-secret", "client-id-secret"] {
        assert!(!said.contains(secret), "{said}");
    }
}

async fn send(port: u16, request: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await.unwrap();
    reply
}

fn port_of(loopback: &Loopback) -> u16 {
    url::Url::parse(&loopback.redirect_uri).unwrap().port().unwrap()
}

#[tokio::test]
async fn the_loopback_hands_back_the_code_and_closes() {
    let loopback = Loopback::bind().await.unwrap();
    assert!(loopback.redirect_uri.starts_with("http://127.0.0.1:"), "{}", loopback.redirect_uri);
    let port = port_of(&loopback);
    let waiting = tokio::spawn(loopback.wait_for_code("mine"));
    let favicon = send(port, "GET /favicon.ico HTTP/1.1\r\n\r\n").await;
    assert!(favicon.starts_with("HTTP/1.1 404"), "{favicon}");
    let page = send(port, "GET /?code=abc&state=mine HTTP/1.1\r\n\r\n").await;
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert_eq!(waiting.await.unwrap(), Ok("abc".into()));
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn a_connection_that_sends_nothing_does_not_hold_up_the_redirect() {
    let loopback = Loopback::bind().await.unwrap();
    let port = port_of(&loopback);
    let waiting = tokio::spawn(loopback.wait_for_code("mine"));
    let _silent = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let page = send(port, "GET /?code=abc&state=mine HTTP/1.1\r\n\r\n").await;
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert_eq!(waiting.await.unwrap(), Ok("abc".into()));
}
