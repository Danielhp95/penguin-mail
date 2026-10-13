//! The sign-in check: the session, what it offers, and the address.

mod common;

use mailrs_jmap::{JmapError, MAIL, check};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{client, session_on};

#[tokio::test]
async fn a_good_token_answers_the_address_from_the_first_identity() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jmap/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session_on(&server.uri(), json!({}))))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "methodResponses": [["Identity/get", {"accountId": "A1", "state": "i1", "notFound": [], "list": [
                {"id": "I1", "name": "Me", "email": "Me@Example.org", "replyTo": null, "bcc": null,
                 "textSignature": "", "htmlSignature": "", "mayDelete": false}
            ]}, "c0"]],
            "sessionState": "s1"
        })))
        .mount(&server)
        .await;
    let checked = check(&client(&server)).await.unwrap();
    assert_eq!(checked.address.as_deref(), Some("me@example.org"));
    assert!(checked.offered.submission);
    assert!(!checked.offered.vacation && !checked.offered.contacts && !checked.offered.masked_email);
}

#[tokio::test]
async fn a_session_without_mail_is_refused() {
    let server = MockServer::start().await;
    let mut session = session_on(&server.uri(), json!({}));
    session["capabilities"].as_object_mut().unwrap().remove(MAIL);
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session))
        .mount(&server)
        .await;
    assert_eq!(check(&client(&server)).await.unwrap_err(), JmapError::UnknownCapability(MAIL.into()));
}

#[tokio::test]
async fn without_submission_the_address_is_the_user_name() {
    let server = MockServer::start().await;
    let mut session = session_on(&server.uri(), json!({}));
    session["capabilities"].as_object_mut().unwrap().remove("urn:ietf:params:jmap:submission");
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session))
        .mount(&server)
        .await;
    let checked = check(&client(&server)).await.unwrap();
    assert!(!checked.offered.submission, "the account is read-only");
    assert_eq!(checked.address.as_deref(), Some("me@example.org"));
}

/// Cyrus answers one identity whose `email` is empty and a user name that
/// is not an address.
async fn identity_without_an_address(username: &str) -> Option<String> {
    let server = MockServer::start().await;
    let mut session = session_on(&server.uri(), json!({}));
    session["username"] = json!(username);
    Mock::given(method("GET"))
        .and(path("/jmap/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "methodResponses": [["Identity/get", {"accountId": "A1", "state": "i1", "notFound": [], "list": [
                {"id": "user1", "name": "", "email": "", "mayDelete": false}
            ]}, "c0"]],
            "sessionState": "s1"
        })))
        .mount(&server)
        .await;
    check(&client(&server)).await.unwrap().address
}

#[tokio::test]
async fn an_identity_without_an_address_falls_back_to_the_user_name() {
    assert_eq!(identity_without_an_address("Me@Example.org").await.as_deref(), Some("me@example.org"));
}

#[tokio::test]
async fn no_address_anywhere_answers_none() {
    assert_eq!(identity_without_an_address("user1").await, None);
}

#[tokio::test]
async fn an_identity_without_an_email_field_still_parses_and_falls_back() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jmap/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session_on(&server.uri(), json!({}))))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "methodResponses": [["Identity/get", {"accountId": "A1", "state": "i1", "notFound": [], "list": [
                {"id": "user1", "name": "", "mayDelete": false}
            ]}, "c0"]],
            "sessionState": "s1"
        })))
        .mount(&server)
        .await;
    let checked = check(&client(&server)).await.unwrap();
    assert_eq!(checked.address.as_deref(), Some("me@example.org"));
}
