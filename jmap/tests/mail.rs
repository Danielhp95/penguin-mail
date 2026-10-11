//! The mail calls' requests, as wiremock receives them, and their answers.

mod common;

use mailrs_jmap::{EmailQuery, MailboxWrite, Submission};
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

async fn sent(server: &MockServer) -> Value {
    let posted = server.received_requests().await.unwrap().into_iter().find(|r| r.method.as_str() == "POST").unwrap();
    serde_json::from_slice(&posted.body).unwrap()
}

#[tokio::test]
async fn the_change_feed_fetches_changed_places_in_the_same_request() {
    let server = MockServer::start().await;
    answering(&server, json!({"sessionState": "s1", "methodResponses": [
        ["Email/changes", {"accountId": "A1", "oldState": "e1", "newState": "e2", "hasMoreChanges": false,
            "created": ["M2"], "updated": ["M1"], "destroyed": ["M0"]}, "c0"],
        ["Email/get", {"accountId": "A1", "state": "e2", "notFound": [], "list": [
            {"id": "M2", "threadId": "T2", "mailboxIds": {"inbox": true}, "keywords": {}}]}, "c1"],
        ["Email/get", {"accountId": "A1", "state": "e2", "notFound": [], "list": [
            {"id": "M1", "threadId": "T1", "mailboxIds": {"archive": true}, "keywords": {"$seen": true}}]}, "c2"],
        ["Mailbox/changes", {"accountId": "A1", "oldState": "m1", "newState": "m1", "hasMoreChanges": false,
            "created": [], "updated": [], "destroyed": []}, "c3"]
    ]})).await;
    let changes = client(&server).mail_changes("e1", "m1", 500).await.unwrap();
    assert_eq!(changes.email.destroyed, ["M0"]);
    assert_eq!(changes.changed.len(), 2);
    let body = sent(&server).await;
    assert_eq!(body["methodCalls"][1][1]["#ids"], json!({"resultOf": "c0", "name": "Email/changes", "path": "/created"}));
    assert_eq!(body["methodCalls"][2][1]["#ids"]["path"], "/updated");
    assert_eq!(body["methodCalls"][0][1]["maxChanges"], 500);
}

#[tokio::test]
async fn a_forgotten_state_answers_cannot_calculate_changes() {
    let server = MockServer::start().await;
    answering(&server, json!({"sessionState": "s1", "methodResponses": [
        ["error", {"type": "cannotCalculateChanges"}, "c0"],
        ["error", {"type": "invalidResultReference"}, "c1"],
        ["error", {"type": "invalidResultReference"}, "c2"],
        ["error", {"type": "cannotCalculateChanges"}, "c3"]
    ]})).await;
    let refused = client(&server).mail_changes("old", "old", 500).await.unwrap_err();
    assert_eq!(refused, mailrs_jmap::JmapError::CannotCalculateChanges);
}

#[tokio::test]
async fn a_query_sorts_newest_first_and_fetches_threads_by_back_reference() {
    let server = MockServer::start().await;
    answering(&server, json!({"sessionState": "s1", "methodResponses": [
        ["Email/query", {"accountId": "A1", "queryState": "q", "canCalculateChanges": false, "position": 0, "ids": ["M2", "M1"], "total": 2}, "c0"],
        ["Email/get", {"accountId": "A1", "state": "e", "notFound": [], "list": [
            {"id": "M1", "threadId": "T1"}, {"id": "M2", "threadId": "T2"}]}, "c1"]
    ]})).await;
    let query = EmailQuery { filter: Some(json!({"inMailbox": "inbox"})), position: 0, limit: 50, total: true };
    let found = client(&server).query(&query).await.unwrap();
    let ids: Vec<(&str, &str)> = found.hits.iter().map(|h| (h.id.as_str(), h.thread_id.as_str())).collect();
    assert_eq!(ids, [("M2", "T2"), ("M1", "T1")], "the query's order, each with its thread");
    assert_eq!(found.total, Some(2));
    let body = sent(&server).await;
    assert_eq!(body["methodCalls"][0][1]["sort"], json!([{"property": "receivedAt", "isAscending": false}]));
    assert_eq!(body["methodCalls"][0][1]["calculateTotal"], true);
}

#[tokio::test]
async fn sending_imports_submits_and_files_the_copy_in_one_request() {
    let server = MockServer::start().await;
    answering(&server, json!({"sessionState": "s1", "methodResponses": [
        ["Email/import", {"accountId": "A1", "created": {"m": {"id": "M9", "blobId": "G1", "threadId": "T9", "size": 10}}}, "c0"],
        ["EmailSubmission/set", {"accountId": "A1", "created": {"s": {"id": "S1"}}}, "c1"],
        ["Email/set", {"accountId": "A1", "updated": {"M9": null}}, "c1"]
    ]})).await;
    let submission = Submission {
        blob_id: "G1".into(),
        identity_id: "I1".into(),
        mail_from: "me@example.org".into(),
        rcpt_to: vec!["ada@example.org".into(), "secret@example.org".into()],
        drafts: Some("drafts".into()),
        sent: Some("sent".into()),
    };
    let imported = client(&server).submit(&submission).await.unwrap();
    assert_eq!((imported.id.as_str(), imported.thread_id.as_str()), ("M9", "T9"));
    let body = sent(&server).await;
    let set = &body["methodCalls"][1][1];
    assert_eq!(set["create"]["s"]["emailId"], "#m");
    assert_eq!(set["create"]["s"]["envelope"]["rcptTo"][1]["email"], "secret@example.org");
    assert_eq!(set["onSuccessUpdateEmail"]["#s"]["mailboxIds/sent"], true);
    assert_eq!(set["onSuccessUpdateEmail"]["#s"]["mailboxIds/drafts"], Value::Null);
    assert_eq!(set["onSuccessUpdateEmail"]["#s"]["keywords/$draft"], Value::Null);
    assert_eq!(body["methodCalls"][0][1]["emails"]["m"]["keywords"]["$draft"], true);
}

#[tokio::test]
async fn a_refused_submission_says_why() {
    let server = MockServer::start().await;
    answering(&server, json!({"sessionState": "s1", "methodResponses": [
        ["Email/import", {"accountId": "A1", "created": {"m": {"id": "M9", "blobId": "G1", "threadId": "T9", "size": 10}}}, "c0"],
        ["EmailSubmission/set", {"accountId": "A1", "notCreated": {"s": {"type": "forbiddenFrom", "description": "not yours"}}}, "c1"]
    ]})).await;
    let submission = Submission {
        blob_id: "G1".into(), identity_id: "I1".into(), mail_from: "x@example.org".into(),
        rcpt_to: vec!["a@example.org".into()], drafts: Some("drafts".into()), sent: Some("sent".into()),
    };
    let refused = client(&server).submit(&submission).await.unwrap_err();
    assert_eq!(refused, mailrs_jmap::JmapError::Method { kind: "forbiddenFrom".into(), description: "not yours".into() });
}

#[tokio::test]
async fn a_mailbox_rename_sends_its_new_parent() {
    let server = MockServer::start().await;
    answering(&server, json!({"sessionState": "s1", "methodResponses": [
        ["Mailbox/set", {"accountId": "A1", "updated": {"B1": null}}, "c0"]
    ]})).await;
    let id = client(&server)
        .set_mailbox(&MailboxWrite::Update { id: "B1".into(), name: "Trips".into(), parent_id: Some("B0".into()) })
        .await
        .unwrap();
    assert_eq!(id, "B1");
    let body = sent(&server).await;
    assert_eq!(body["methodCalls"][0][1]["update"]["B1"], json!({"name": "Trips", "parentId": "B0"}));
}

#[test]
fn a_patch_path_escapes_slashes_and_tildes() {
    assert_eq!(mailrs_jmap::pointer("a/b~c"), "a~1b~0c");
}
