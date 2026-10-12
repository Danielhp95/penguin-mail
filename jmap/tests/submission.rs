//! Holding a message on the server, as wiremock receives the calls.

mod common;

use mailrs_jmap::{Canceled, HELD, Hold, JmapError, Session, max_delayed_send};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{client, session_on};

async fn answering(server: &MockServer, answer: Value) {
    Mock::given(method("GET"))
        .and(path("/jmap/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session_on(&server.uri(), json!({"maxCallsInRequest": 8}))))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer))
        .mount(server)
        .await;
}

async fn posts(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn hold() -> Hold {
    Hold {
        blob_id: "G1".into(),
        identity_id: "I1".into(),
        mail_from: "me@example.org".into(),
        rcpt_to: vec!["ada@example.org".into()],
        drafts: "drafts".into(),
        hold_for: 3600,
    }
}

#[tokio::test]
async fn the_hold_waits_in_drafts_and_asks_for_holdfor() {
    let server = MockServer::start().await;
    answering(&server, json!({"sessionState": "s1", "methodResponses": [
        ["Email/import", {"accountId": "A1", "created": {"m": {"id": "M9", "blobId": "G1", "threadId": "T9", "size": 10}}}, "c0"],
        ["EmailSubmission/set", {"accountId": "A1", "created": {"s": {"id": "S1", "undoStatus": "pending", "sendAt": "2026-10-12T09:00:00Z"}}}, "c1"]
    ]})).await;
    let held = client(&server).hold(&hold()).await.unwrap();
    assert_eq!((held.id.as_str(), held.email_id.as_str(), held.thread_id.as_str()), ("S1", "M9", "T9"));
    let body = &posts(&server).await[0];
    let keywords = &body["methodCalls"][0][1]["emails"]["m"]["keywords"];
    assert_eq!((keywords["$draft"].clone(), keywords[HELD].clone()), (json!(true), json!(true)));
    assert_eq!(body["methodCalls"][0][1]["emails"]["m"]["mailboxIds"]["drafts"], true);
    let create = &body["methodCalls"][1][1]["create"]["s"];
    assert_eq!(create["envelope"]["mailFrom"]["parameters"]["HOLDFOR"], "3600");
    assert_eq!(create["emailId"], "#m");
    assert!(body["methodCalls"][1][1].get("onSuccessUpdateEmail").is_none(), "the copy stays in Drafts until it goes");
}

#[tokio::test]
async fn a_refused_hold_destroys_the_copy() {
    let server = MockServer::start().await;
    answering(&server, json!({"sessionState": "s1", "methodResponses": [
        ["Email/import", {"accountId": "A1", "created": {"m": {"id": "M9", "threadId": "T9"}}}, "c0"],
        ["EmailSubmission/set", {"accountId": "A1", "notCreated": {"s": {"type": "invalidArguments", "description": "Unsupported parameterName"}}}, "c1"]
    ]})).await;
    let refused = client(&server).hold(&hold()).await.unwrap_err();
    assert_eq!(refused, JmapError::Method { kind: "invalidArguments".into(), description: "Unsupported parameterName".into() });
    let sent = posts(&server).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["methodCalls"][0][1]["destroy"], json!(["M9"]));
}

#[tokio::test]
async fn a_cancel_in_time_reads_as_canceled() {
    let server = MockServer::start().await;
    answering(&server, json!({"sessionState": "s1", "methodResponses": [
        ["EmailSubmission/set", {"accountId": "A1", "updated": {"S1": null}}, "c0"]
    ]})).await;
    assert_eq!(client(&server).cancel_submission("S1").await.unwrap(), Canceled::Canceled);
    let body = &posts(&server).await[0];
    assert_eq!(body["methodCalls"][0][1]["update"]["S1"]["undoStatus"], "canceled");
}

#[tokio::test]
async fn a_late_cancel_reads_as_too_late() {
    let server = MockServer::start().await;
    answering(&server, json!({"sessionState": "s1", "methodResponses": [
        ["EmailSubmission/set", {"accountId": "A1", "notUpdated": {"S1": {"type": "cannotUnsend"}}}, "c0"]
    ]})).await;
    assert_eq!(client(&server).cancel_submission("S1").await.unwrap(), Canceled::TooLate);
}

#[tokio::test]
async fn a_server_without_submission_get_says_so() {
    let server = MockServer::start().await;
    answering(&server, json!({"sessionState": "s1", "methodResponses": [
        ["error", {"type": "unknownMethod"}, "c0"]
    ]})).await;
    assert!(!client(&server).answers_submission_get().await.unwrap());
}

#[tokio::test]
async fn a_submission_reads_its_status_and_hour() {
    let server = MockServer::start().await;
    answering(&server, json!({"sessionState": "s1", "methodResponses": [
        ["EmailSubmission/get", {"accountId": "A1", "state": "x", "notFound": [], "list": [
            {"id": "S1", "emailId": "M9", "undoStatus": "pending", "sendAt": "2026-10-12T09:00:00Z"}]}, "c0"]
    ]})).await;
    let found = client(&server).submissions(&["S1".to_string()]).await.unwrap();
    assert_eq!(found[0].undo_status, "pending");
    assert_eq!(found[0].send_at.as_deref(), Some("2026-10-12T09:00:00Z"));
}

#[tokio::test]
async fn filing_a_sent_hold_moves_it_and_drops_both_keywords() {
    let server = MockServer::start().await;
    answering(&server, json!({"sessionState": "s1", "methodResponses": [
        ["Email/set", {"accountId": "A1", "updated": {"M9": null}}, "c0"]
    ]})).await;
    client(&server).file_held("M9", "drafts", "sent").await.unwrap();
    let patch = &posts(&server).await[0]["methodCalls"][0][1]["update"]["M9"];
    assert_eq!(patch["mailboxIds/drafts"], Value::Null);
    assert_eq!(patch["mailboxIds/sent"], true);
    assert_eq!(patch["keywords/$draft"], Value::Null);
    assert_eq!(patch[format!("keywords/{HELD}")], Value::Null);
}

#[test]
fn max_delayed_send_reads_the_submission_account_and_is_zero_without_it() {
    let mut session = session_on("https://jmap.example.org", json!({}));
    assert_eq!(max_delayed_send(&Session::parse(&serde_json::to_vec(&session).unwrap()).unwrap()), 0);
    session["accounts"]["A1"]["accountCapabilities"]["urn:ietf:params:jmap:submission"] = json!({"maxDelayedSend": 44_236_800});
    assert_eq!(max_delayed_send(&Session::parse(&serde_json::to_vec(&session).unwrap()).unwrap()), 44_236_800);
}
