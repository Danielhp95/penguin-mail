//! A DAV client that signs in with a bearer token.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use mailrs_dav::{DavApi, DavClient, Kind, Login, TokenSource};
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Counting(AtomicU32, std::sync::Mutex<Vec<Option<String>>>);

impl Counting {
    fn new() -> Counting {
        Counting(AtomicU32::new(0), std::sync::Mutex::new(Vec::new()))
    }
}

impl TokenSource for Counting {
    fn token<'a>(&'a self, refused: Option<&'a str>) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        let n = self.0.fetch_add(1, Ordering::Relaxed);
        self.1.lock().unwrap().push(refused.map(str::to_string));
        Box::pin(async move { Ok(if refused.is_some() { "fresh".to_string() } else { format!("stale{n}") }) })
    }
}

#[tokio::test]
async fn the_calendar_takes_the_oauth_token() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(header("Authorization", "Bearer fresh"))
        .respond_with(ResponseTemplate::new(207).set_body_string(
            r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:"><d:response><d:href>/</d:href><d:propstat><d:prop><d:current-user-principal><d:href>/p/</d:href></d:current-user-principal></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("PROPFIND")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
    let source = Arc::new(Counting::new());
    let client = DavClient::over(url::Url::parse(&server.uri()).unwrap(), Kind::Calendar, Login::new("me", ""))
        .unwrap()
        .with_bearer(source);
    let _ = client.homes().await;
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().any(|r| r.headers.get("Authorization").is_some_and(|v| v == "Bearer fresh")));
    assert!(requests.iter().all(|r| !r.headers.get("Authorization").is_some_and(|v| v.to_str().unwrap_or_default().starts_with("Basic"))));
}

#[tokio::test]
async fn a_second_refusal_is_unauthorized_after_one_fresh_token() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
    let source = Arc::new(Counting::new());
    let client = DavClient::over(url::Url::parse(&server.uri()).unwrap(), Kind::Calendar, Login::new("me", ""))
        .unwrap()
        .with_bearer(source.clone());
    assert!(client.homes().await.is_err());
    let asked_for_fresh = server.received_requests().await.unwrap().iter().filter(|r| r.headers.get("Authorization").is_some_and(|v| v == "Bearer fresh")).count();
    assert_eq!(asked_for_fresh, 1);
}

#[tokio::test]
async fn a_refused_token_goes_back_to_its_source() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
    let source = Arc::new(Counting::new());
    let client = DavClient::over(url::Url::parse(&server.uri()).unwrap(), Kind::Calendar, Login::new("me", ""))
        .unwrap()
        .with_bearer(source.clone());
    let _ = client.homes().await;
    let asked = source.1.lock().unwrap().clone();
    assert_eq!(asked[..2], [None, Some("stale0".to_string())]);
}

#[test]
fn only_addresses_on_one_site_share_it() {
    use mailrs_dav::shares_site;

    assert!(shares_site("https://api.fastmail.com/jmap/session", "https://caldav.fastmail.com/dav/"));
    assert!(!shares_site("https://api.fastmail.com/jmap/session", "https://dav.example.org/cal/"));
    assert!(!shares_site("https://api.fastmail.com/jmap/session", "https://fastmail.com@dav.example.org/"));
    assert!(!shares_site("https://api.fastmail.com/jmap/session", "not a url"));
}
