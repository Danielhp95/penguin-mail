//! The HTTP core against wiremock: the session, its redirects, the caps,
//! the limits, the errors, and where the token goes.

mod common;

use std::time::Duration;

use mailrs_jmap::{Auth, JmapClient, JmapError, MAIL, Request, SIEVE};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{client, session_on};

async fn serving_session(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/jmap/session"))
        .and(header("Authorization", "Bearer token-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session_on(&server.uri(), json!({}))))
        .mount(server)
        .await;
}

#[tokio::test]
async fn the_session_comes_with_the_token_and_is_kept() {
    let server = MockServer::start().await;
    serving_session(&server).await;
    let client = client(&server);
    let first = client.session().await.unwrap();
    let again = client.session().await.unwrap();
    assert_eq!(first.username, "me@example.org");
    assert!(std::sync::Arc::ptr_eq(&first, &again), "the second read is the cached one");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_redirect_on_the_same_host_is_followed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/jmap"))
        .respond_with(ResponseTemplate::new(307).insert_header("Location", format!("{}/jmap/session", server.uri())))
        .mount(&server)
        .await;
    serving_session(&server).await;
    let client = JmapClient::new(&format!("{}/.well-known/jmap", server.uri()), Auth::Bearer("token-1".into())).unwrap();
    assert_eq!(client.session().await.unwrap().state, "s1");
}

#[tokio::test]
async fn a_redirect_to_another_host_gets_no_credentials() {
    let (server, elsewhere) = (MockServer::start().await, MockServer::start().await);
    Mock::given(method("GET"))
        .and(path("/jmap/session"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", format!("{}/jmap/session", elsewhere.uri())))
        .mount(&server)
        .await;
    let refused = client(&server).session().await;
    assert!(matches!(refused, Err(JmapError::OffHost(_))), "{refused:?}");
    assert!(elsewhere.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_refused_token_says_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_string("No Authorization header"))
        .mount(&server)
        .await;
    assert_eq!(client(&server).session().await.unwrap_err(), JmapError::Unauthorized);
}

#[test]
fn a_session_url_over_plain_http_is_refused_unless_it_is_this_computer() {
    let bearer = || Auth::Bearer("t".into());
    assert!(matches!(JmapClient::new("http://mail.example.org/jmap", bearer()), Err(JmapError::BadUrl(_))));
    assert!(JmapClient::new("http://localhost:8080/jmap/session", bearer()).is_ok());
    assert!(JmapClient::new("http://127.0.0.1:8080/jmap/session", bearer()).is_ok());
    assert!(JmapClient::new("http://[::1]:8080/jmap/session", bearer()).is_ok());
    assert!(JmapClient::new("https://api.fastmail.com/jmap/session", bearer()).is_ok());
    assert!(matches!(JmapClient::new("ftp://example.org/", bearer()), Err(JmapError::BadUrl(_))));
}

#[tokio::test]
async fn a_call_goes_to_the_api_url_and_its_answer_comes_back() {
    let server = MockServer::start().await;
    serving_session(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/"))
        .and(header("Authorization", "Bearer token-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "methodResponses": [["Mailbox/get", {"accountId": "A1", "state": "m1", "list": [], "notFound": []}, "c0"]],
            "sessionState": "s1"
        })))
        .mount(&server)
        .await;
    let mut request = Request::new();
    let id = request.call(MAIL, "Mailbox/get", json!({"accountId": "A1", "ids": null}));
    let mut answer = client(&server).send(&request).await.unwrap();
    let got: serde_json::Value = answer.take(&id, "Mailbox/get").unwrap();
    assert_eq!(got["state"], "m1");
}

#[tokio::test]
async fn a_capability_the_session_lacks_never_leaves_the_computer() {
    let server = MockServer::start().await;
    serving_session(&server).await;
    let mut request = Request::new();
    request.call(SIEVE, "SieveScript/get", json!({"accountId": "A1"}));
    let refused = client(&server).send(&request).await;
    assert_eq!(refused.unwrap_err(), JmapError::UnknownCapability(SIEVE.into()));
    let posts = server.received_requests().await.unwrap().into_iter().filter(|r| r.method.as_str() == "POST").count();
    assert_eq!(posts, 0);
}

#[tokio::test]
async fn a_request_past_max_calls_is_refused_before_it_goes() {
    let server = MockServer::start().await;
    serving_session(&server).await;
    let mut request = Request::new();
    for _ in 0..5 {
        request.call(MAIL, "Mailbox/get", json!({"accountId": "A1"}));
    }
    assert_eq!(client(&server).send(&request).await.unwrap_err(), JmapError::RequestTooLarge);
}

#[tokio::test]
async fn a_problem_answer_becomes_its_kind() {
    let server = MockServer::start().await;
    serving_session(&server).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "type": "urn:ietf:params:jmap:error:unknownCapability", "status": 400, "detail": "urn:ietf:params:jmap:mail"
        })))
        .mount(&server)
        .await;
    let mut request = Request::new();
    request.call(MAIL, "Mailbox/get", json!({"accountId": "A1"}));
    let refused = client(&server).send(&request).await.unwrap_err();
    assert_eq!(refused, JmapError::UnknownCapability("urn:ietf:params:jmap:mail".into()));
}

#[tokio::test]
async fn a_rate_limit_carries_its_retry_after() {
    let server = MockServer::start().await;
    serving_session(&server).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "7"))
        .mount(&server)
        .await;
    let mut request = Request::new();
    request.call(MAIL, "Mailbox/get", json!({"accountId": "A1"}));
    let refused = client(&server).send(&request).await.unwrap_err();
    assert_eq!(refused, JmapError::RateLimited { retry_after: Some(Duration::from_secs(7)) });
}

#[tokio::test]
async fn an_answer_past_the_cap_is_refused() {
    let server = MockServer::start().await;
    serving_session(&server).await;
    let huge = "x".repeat(mailrs_jmap::http::JSON_LIMIT + 1);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(huge))
        .mount(&server)
        .await;
    let mut request = Request::new();
    request.call(MAIL, "Mailbox/get", json!({"accountId": "A1"}));
    let refused = client(&server).send(&request).await.unwrap_err();
    assert!(matches!(refused, JmapError::AnswerTooLarge { .. }), "{refused:?}");
}

#[tokio::test]
async fn a_new_session_state_makes_the_next_call_read_the_session_again() {
    let server = MockServer::start().await;
    serving_session(&server).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"methodResponses": [], "sessionState": "s2"})))
        .mount(&server)
        .await;
    let client = client(&server);
    let mut request = Request::new();
    request.call(MAIL, "Mailbox/get", json!({"accountId": "A1"}));
    client.send(&request).await.unwrap();
    client.send(&request).await.unwrap();
    let gets = server.received_requests().await.unwrap().into_iter().filter(|r| r.method.as_str() == "GET").count();
    assert_eq!(gets, 2, "the session is read once, then again after the state moved");
}

#[tokio::test]
async fn a_session_url_on_another_host_in_the_session_is_refused() {
    let server = MockServer::start().await;
    let mut session = session_on(&server.uri(), json!({}));
    session["apiUrl"] = json!("http://mail.example.org/api/");
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session))
        .mount(&server)
        .await;
    let refused = client(&server).session().await.unwrap_err();
    assert!(matches!(refused, JmapError::BadUrl(_)), "plain HTTP off this computer: {refused:?}");
}

#[tokio::test]
async fn calls_on_a_fresh_client_wait_for_one_session_read() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jmap/session"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(session_on(&server.uri(), json!({})))
                .set_delay(Duration::from_millis(50)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"methodResponses": [], "sessionState": "s1"})))
        .mount(&server)
        .await;
    let client = client(&server);
    let calls: Vec<_> = (0..8)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move {
                let mut request = Request::new();
                request.call(MAIL, "Mailbox/get", json!({"accountId": "A1"}));
                client.send(&request).await
            })
        })
        .collect();
    for call in calls {
        call.await.unwrap().unwrap();
    }
    server.verify().await;
}

#[test]
fn a_session_url_with_a_user_or_password_is_refused_and_not_repeated() {
    for url in ["https://me:hunter2@example.org/jmap", "https://me@example.org/jmap"] {
        let refused = JmapClient::new(url, Auth::Bearer("t".into())).unwrap_err();
        assert!(matches!(refused, JmapError::BadUrl(_)), "{refused:?}");
        let said = refused.to_string();
        assert!(!said.contains("hunter2") && !said.contains("me@"), "{said}");
    }
}

#[tokio::test]
async fn a_session_naming_a_url_with_a_password_is_refused() {
    let server = MockServer::start().await;
    let mut session = session_on(&server.uri(), json!({}));
    session["apiUrl"] = json!("https://me:hunter2@mail.example.org/api/");
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session))
        .mount(&server)
        .await;
    let refused = client(&server).session().await.unwrap_err();
    assert!(matches!(refused, JmapError::BadUrl(_)), "{refused:?}");
    assert!(!refused.to_string().contains("hunter2"), "{refused}");
}
