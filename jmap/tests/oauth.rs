//! OAuth against wiremock: metadata, registration, the code exchange and
//! refresh, and the redirect's checks.

use mailrs_jmap::JmapError;
use mailrs_jmap::oauth::{self, Loopback, OAuthClient, Pkce, Tokens, parse_redirect};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn serving_metadata(server: &MockServer, registration: bool) {
    let mut metadata = json!({
        "issuer": server.uri(),
        "authorization_endpoint": format!("{}/authorize", server.uri()),
        "token_endpoint": format!("{}/token", server.uri()),
        "code_challenge_methods_supported": ["S256"],
        "scopes_supported": ["urn:ietf:params:oauth:scope:mail", "urn:ietf:params:oauth:scope:contacts", "openid", "offline_access"]
    });
    if registration {
        metadata["registration_endpoint"] = json!(format!("{}/register", server.uri()));
    }
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata))
        .mount(server)
        .await;
}

#[tokio::test]
async fn metadata_comes_from_beside_the_session() {
    let server = MockServer::start().await;
    serving_metadata(&server, true).await;
    let found = oauth::metadata(&format!("{}/jmap/session", server.uri())).await.unwrap();
    assert_eq!(found.token_endpoint, format!("{}/token", server.uri()));
    assert_eq!(
        oauth::oauth_scopes(&found),
        ["urn:ietf:params:oauth:scope:mail", "urn:ietf:params:oauth:scope:contacts", "offline_access"]
    );
}

#[tokio::test]
async fn metadata_without_s256_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": server.uri(),
            "authorization_endpoint": format!("{}/authorize", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "code_challenge_methods_supported": ["plain"]
        })))
        .mount(&server)
        .await;
    assert!(oauth::metadata(&format!("{}/jmap/session", server.uri())).await.is_err());
}

#[tokio::test]
async fn a_server_without_registration_offers_no_browser_sign_in() {
    let server = MockServer::start().await;
    serving_metadata(&server, false).await;
    let found = oauth::metadata(&format!("{}/jmap/session", server.uri())).await.unwrap();
    assert!(found.registration_endpoint.is_none());
    assert!(oauth::register(&found, "http://127.0.0.1/", &[]).await.is_err());
}

#[tokio::test]
async fn registration_asks_for_a_public_client_on_the_loopback() {
    let server = MockServer::start().await;
    serving_metadata(&server, true).await;
    Mock::given(method("POST"))
        .and(path("/register"))
        .and(body_string_contains("\"token_endpoint_auth_method\":\"none\""))
        .and(body_string_contains("http://127.0.0.1/"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"client_id": "stateless-123"})))
        .mount(&server)
        .await;
    let found = oauth::metadata(&format!("{}/jmap/session", server.uri())).await.unwrap();
    assert_eq!(
        oauth::register(&found, "http://127.0.0.1/", &oauth::oauth_scopes(&found)).await.unwrap(),
        "stateless-123"
    );
}

#[tokio::test]
async fn the_code_and_the_refresh_carry_pkce_and_the_resource() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code_verifier="))
        .and(body_string_contains("resource=https%3A%2F%2Fapi.example.com%2Fjmap%2Fsession"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "a1", "expires_in": 3600, "refresh_token": "r1"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant", "temporary": true})))
        .mount(&server)
        .await;
    let client = OAuthClient {
        client_id: "c1".into(),
        token_endpoint: format!("{}/token", server.uri()),
        resource: "https://api.example.com/jmap/session".into(),
    };
    let tokens = client.exchange("code", "http://127.0.0.1:5555/", &Pkce::generate()).await.unwrap();
    assert_eq!((tokens.access_token.as_str(), tokens.refresh_token.as_deref()), ("a1", Some("r1")));
    assert_eq!(client.refresh("r1").await.unwrap_err(), JmapError::SignInLocked);
    assert!(JmapError::SignInLocked.is_transient());
}

#[tokio::test]
async fn a_revoked_grant_asks_to_sign_in_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})))
        .mount(&server)
        .await;
    let client = OAuthClient {
        client_id: "c1".into(),
        token_endpoint: format!("{}/token", server.uri()),
        resource: "https://api.example.com/jmap/session".into(),
    };
    assert_eq!(client.refresh("r1").await.unwrap_err(), JmapError::Unauthorized);
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
