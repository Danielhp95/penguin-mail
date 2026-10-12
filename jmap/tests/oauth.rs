//! OAuth's production rules without a server that speaks https: what
//! the client refuses to send over http, the redirect's checks, the
//! authorize URL, Debug output and the loopback. The exchanges against
//! wiremock live in `oauth.rs`'s unit tests, which alone may use http.

use std::sync::Arc;
use std::time::Duration;

use mailrs_jmap::oauth::{self, Loopback, Metadata, OAuthClient, OAuthSession, Pkce, Tokens, parse_redirect};
use mailrs_jmap::{Auth, JmapError};
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
        authorization_response_iss_parameter_supported: false,
    }
}

/// `https_metadata`, promising RFC 9207's `iss` on the redirect or not.
fn promising_iss(promised: bool) -> Metadata {
    Metadata { authorization_response_iss_parameter_supported: promised, ..https_metadata() }
}

fn redirect(query: &str) -> String {
    format!("GET /?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
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
    let metadata = https_metadata();
    // Not the consent this sign-in opened: ignored, so the wait goes on.
    assert_eq!(parse_redirect(&redirect("code=abc&state=other"), "mine", &metadata), None);
    assert_eq!(parse_redirect(&redirect("code=abc"), "mine", &metadata), None);
    assert_eq!(parse_redirect(&redirect("code=abc&state=mine"), "mine", &metadata), Some(Ok("abc".into())));
    assert_eq!(parse_redirect(&redirect("error=access_denied&state=mine"), "mine", &metadata), Some(Err(JmapError::Declined)));
    assert_eq!(parse_redirect("GET /favicon.ico HTTP/1.1\r\n\r\n", "mine", &metadata), None);
}

#[test]
fn a_refusal_with_another_state_is_ignored() {
    assert_eq!(parse_redirect(&redirect("error=access_denied&state=other"), "mine", &https_metadata()), None);
}

#[test]
fn a_redirect_from_the_expected_issuer_is_taken() {
    let request = redirect("code=abc&state=mine&iss=https%3A%2F%2Fauth.example.com");
    assert_eq!(parse_redirect(&request, "mine", &promising_iss(true)), Some(Ok("abc".into())));
    assert_eq!(parse_redirect(&request, "mine", &promising_iss(false)), Some(Ok("abc".into())));
}

#[test]
fn a_redirect_from_another_issuer_is_refused() {
    for metadata in [promising_iss(true), promising_iss(false)] {
        for query in [
            "code=abc&state=mine&iss=https%3A%2F%2Fevil.example",
            "code=abc&state=mine&iss=https%3A%2F%2Fauth.example.com%2F",
            "error=access_denied&state=mine&iss=https%3A%2F%2Fevil.example",
        ] {
            let outcome = parse_redirect(&redirect(query), "mine", &metadata);
            assert_eq!(outcome, Some(Err(JmapError::IssuerMismatch)), "{query}");
        }
    }
}

#[test]
fn a_missing_iss_is_refused_when_the_server_promised_it() {
    let outcome = parse_redirect(&redirect("code=abc&state=mine"), "mine", &promising_iss(true));
    assert_eq!(outcome, Some(Err(JmapError::IssuerMismatch)));
}

#[test]
fn a_missing_iss_is_taken_when_the_server_never_promised_it() {
    let outcome = parse_redirect(&redirect("code=abc&state=mine"), "mine", &promising_iss(false));
    assert_eq!(outcome, Some(Ok("abc".into())));
}

#[test]
fn a_redirect_error_keeps_no_server_text() {
    let query = "error=server_error&error_description=token+tok-SECRET-123&state=mine";
    let err = parse_redirect(&redirect(query), "mine", &https_metadata()).unwrap().unwrap_err();
    assert_eq!(err, JmapError::Forbidden("server_error".into()));
    for oddity in ["bad%22quote", "back%5Cslash", "line%0Abreak", &"x".repeat(65)] {
        let err = parse_redirect(&redirect(&format!("error={oddity}&state=mine")), "mine", &https_metadata())
            .unwrap()
            .unwrap_err();
        assert_eq!(err, JmapError::Forbidden("unknown".into()), "{oddity}");
    }
    let said = format!("{err} {err:?}");
    assert!(!said.contains("SECRET"), "{said}");
}

#[test]
fn a_redirect_naming_an_unknown_client_says_so() {
    for error in ["invalid_client", "unauthorized_client"] {
        let err = parse_redirect(&redirect(&format!("error={error}&state=mine")), "mine", &https_metadata()).unwrap().unwrap_err();
        assert_eq!(err, JmapError::UnknownClient, "{error}");
    }
}

#[tokio::test]
async fn a_session_url_with_a_password_is_refused_without_echo() {
    let err = oauth::metadata("https://me:hunter2@api.example.com/jmap/session").await.unwrap_err();
    assert!(matches!(err, JmapError::BadUrl(_)), "{err:?}");
    let said = format!("{err} {err:?}");
    assert!(!said.contains("hunter2") && !said.contains("api.example.com"), "{said}");
}

#[tokio::test]
async fn an_unreadable_session_url_is_not_echoed() {
    let err = oauth::metadata("not a url hunter2").await.unwrap_err();
    assert!(matches!(err, JmapError::BadUrl(_)), "{err:?}");
    assert!(!format!("{err} {err:?}").contains("hunter2"), "{err:?}");
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
        authorization_response_iss_parameter_supported: false,
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

/// The loopback waiting for state `mine` from `https_metadata`'s issuer.
async fn waiting_loopback() -> (u16, tokio::task::JoinHandle<Result<String, JmapError>>) {
    let loopback = Loopback::bind().await.unwrap();
    assert!(loopback.redirect_uri.starts_with("http://127.0.0.1:"), "{}", loopback.redirect_uri);
    let port = port_of(&loopback);
    let waiting = tokio::spawn(async move { loopback.wait_for_code("mine", &https_metadata()).await });
    (port, waiting)
}

/// What the loopback hands back, or a failure when it takes longer than a
/// moment. Ten seconds is the per-connection wait, so a result inside two
/// shows that nothing waited it out.
async fn promptly(waiting: tokio::task::JoinHandle<Result<String, JmapError>>) -> Result<String, JmapError> {
    tokio::time::timeout(Duration::from_secs(2), waiting).await.expect("the loopback held up the redirect").unwrap()
}

#[tokio::test]
async fn the_loopback_hands_back_the_code_and_closes() {
    let (port, waiting) = waiting_loopback().await;
    let favicon = send(port, "GET /favicon.ico HTTP/1.1\r\n\r\n").await;
    assert!(favicon.starts_with("HTTP/1.1 404"), "{favicon}");
    let page = send(port, &redirect("code=abc&state=mine")).await;
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert_eq!(promptly(waiting).await, Ok("abc".into()));
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
}

#[tokio::test]
async fn a_connection_that_sends_nothing_does_not_hold_up_the_redirect() {
    let (port, waiting) = waiting_loopback().await;
    let _idle = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let page = send(port, &redirect("code=abc&state=mine")).await;
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert_eq!(promptly(waiting).await, Ok("abc".into()));
}

#[tokio::test]
async fn a_redirect_with_another_state_leaves_the_loopback_waiting() {
    let (port, waiting) = waiting_loopback().await;
    let forged = send(port, &redirect("error=access_denied&state=other")).await;
    assert!(forged.starts_with("HTTP/1.1 404"), "{forged}");
    assert!(!waiting.is_finished());
    send(port, &redirect("code=abc&state=mine")).await;
    assert_eq!(promptly(waiting).await, Ok("abc".into()));
}

#[tokio::test]
async fn a_request_that_arrives_in_pieces_is_read_whole() {
    let (port, waiting) = waiting_loopback().await;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let request = redirect("code=abc&state=mine");
    let (first, rest) = request.split_at(12);
    stream.write_all(first.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    stream.write_all(rest.as_bytes()).await.unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await.unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert_eq!(promptly(waiting).await, Ok("abc".into()));
}

#[tokio::test]
async fn a_session_refreshes_over_https_only() {
    let server = untouchable().await;
    let client = OAuthClient { client_id: "c1".into(), token_endpoint: format!("{}/token", server.uri()), resource: "x".into() };
    let err = OAuthSession::new(client, "r1".into()).bearer(false).await.unwrap_err();
    assert_eq!(err, JmapError::NotHttps);
    says_no_address(&err, &server.uri());
}

#[test]
fn the_session_keeps_its_tokens_out_of_debug() {
    let client = || OAuthClient {
        client_id: "client-SECRET".into(),
        token_endpoint: "https://auth.example.com/token".into(),
        resource: "https://api.example.com/jmap/session".into(),
    };
    let tokens = Tokens { access_token: "access-SECRET".into(), expires_in: 3600, refresh_token: Some("refresh-SECRET".into()) };
    let signed_in = OAuthSession::signed_in(client(), tokens);
    let kept = Auth::OAuth(Arc::new(OAuthSession::new(client(), "kept-SECRET".into())));
    let said = format!("{signed_in:?} {kept:?}");
    assert!(!said.contains("SECRET"), "{said}");
}
