//! Quotas (RFC 9425) and read receipts (RFC 9007), as wiremock receives them.

mod common;

use mailrs_jmap::{MDN, MdnOut, QUOTA, Quota, mail_storage};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{client, session_on};

/// Serves a session offering mail, submission and `extra`, and `answer`
/// to every call.
async fn answering(server: &MockServer, extra: &[&str], answer: Value) {
    let mut session = session_on(&server.uri(), json!({"maxCallsInRequest": 8}));
    for capability in extra {
        session["capabilities"][*capability] = json!({});
        session["accounts"]["A1"]["accountCapabilities"][*capability] = json!({});
        session["primaryAccounts"][*capability] = json!("A1");
    }
    Mock::given(method("GET"))
        .and(path("/jmap/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session))
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

fn quota(id: &str, scope: &str, resource: &str, used: u64, limit: u64, types: &[&str]) -> Quota {
    Quota {
        id: id.into(),
        resource_type: resource.into(),
        scope: scope.into(),
        used,
        hard_limit: limit,
        warn_limit: None,
        types: types.iter().map(|t| t.to_string()).collect(),
    }
}

#[tokio::test]
async fn quotas_come_from_quota_get() {
    let server = MockServer::start().await;
    answering(&server, &[QUOTA], json!({"sessionState": "s1", "methodResponses": [
        ["Quota/get", {"accountId": "A1", "state": "q1", "notFound": [], "list": [
            {"id": "Q1", "resourceType": "octets", "scope": "account", "used": 0, "hardLimit": 1073741824,
             "warnLimit": 966367641, "types": ["Mail"], "name": "me"}]}, "c0"]
    ]})).await;
    let quotas = client(&server).quotas().await.unwrap();
    assert_eq!(quotas[0].hard_limit, 1_073_741_824);
    assert_eq!(quotas[0].warn_limit, Some(966_367_641));
    let body = sent(&server).await;
    assert_eq!(body["using"], json!(["urn:ietf:params:jmap:core", QUOTA]));
}

#[tokio::test]
async fn a_session_without_quotas_is_refused_before_a_call() {
    let server = MockServer::start().await;
    answering(&server, &[], json!({"sessionState": "s1", "methodResponses": []})).await;
    assert!(matches!(client(&server).quotas().await, Err(mailrs_jmap::JmapError::UnknownCapability(_))));
}

#[test]
fn mail_storage_is_the_tightest_byte_quota_on_the_account() {
    let quotas = [
        quota("count", "account", "count", 10, 100, &["Mail"]),
        quota("domain", "domain", "octets", 10, 1000, &["Mail"]),
        quota("files", "account", "octets", 10, 50, &["FileNode"]),
        quota("roomy", "account", "octets", 10, 5000, &[]),
        quota("tight", "account", "octets", 900, 1000, &["Mail"]),
    ];
    assert_eq!(mail_storage(&quotas).map(|q| q.id.as_str()), Some("tight"));
    assert_eq!(mail_storage(&quotas[..3]), None);
}

#[tokio::test]
async fn a_receipt_names_the_message_and_marks_it_sent() {
    let server = MockServer::start().await;
    answering(&server, &[MDN], json!({"sessionState": "s1", "methodResponses": [
        ["MDN/send", {"accountId": "A1", "sent": {"k": {"finalRecipient": "rfc822; me@example.org"}}}, "c0"],
        ["Email/set", {"accountId": "A1", "updated": {"M1": null}}, "c0"]
    ]})).await;
    let mdn = MdnOut {
        for_email_id: "M1".into(),
        identity_id: "I1".into(),
        subject: "Read: Rota".into(),
        text_body: "Shown on 11 Oct.".into(),
    };
    client(&server).send_mdn(&mdn).await.unwrap();
    let body = sent(&server).await;
    let call = &body["methodCalls"][0][1];
    assert_eq!(call["send"]["k"]["forEmailId"], "M1");
    assert_eq!(call["send"]["k"]["subject"], "Read: Rota", "the app's own words, never the server's English");
    assert_eq!(call["send"]["k"]["disposition"]["type"], "displayed");
    assert_eq!(call["send"]["k"]["disposition"]["sendingMode"], "mdn-sent-manually");
    assert_eq!(call["onSuccessUpdateEmail"]["#k"]["keywords/$mdnsent"], true);
    assert!(body["using"].as_array().unwrap().contains(&json!("urn:ietf:params:jmap:mail")));
}

#[tokio::test]
async fn a_receipt_the_server_would_not_send_says_why() {
    let server = MockServer::start().await;
    answering(&server, &[MDN], json!({"sessionState": "s1", "methodResponses": [
        ["MDN/send", {"accountId": "A1", "notSent": {"k": {"type": "mdnAlreadySent"}}}, "c0"]
    ]})).await;
    let mdn = MdnOut { for_email_id: "M1".into(), identity_id: "I1".into(), subject: "s".into(), text_body: "t".into() };
    let refused = client(&server).send_mdn(&mdn).await.unwrap_err();
    assert!(matches!(refused, mailrs_jmap::JmapError::Method { ref kind, .. } if kind == "mdnAlreadySent"));
}
