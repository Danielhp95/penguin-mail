//! The services beside mail, as wiremock receives their calls.

mod common;

use mailrs_jmap::{CardWrite, VacationResponse};
use serde_json::{Map, Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{client, session_on};

const CONTACTS: &str = "urn:ietf:params:jmap:contacts";
const VACATION: &str = "urn:ietf:params:jmap:vacationresponse";
const SIEVE: &str = "urn:ietf:params:jmap:sieve";
const MASKED: &str = "https://www.fastmail.com/dev/maskedemail";

/// A session that offers every service, with contacts in a second account
/// as Fastmail keeps them.
async fn serving(server: &MockServer, answer: Value) {
    let mut session = session_on(&server.uri(), json!({}));
    for capability in [VACATION, SIEVE, MASKED] {
        session["capabilities"][capability] = json!({});
        session["accounts"]["A1"]["accountCapabilities"][capability] = json!({});
        session["primaryAccounts"][capability] = json!("A1");
    }
    session["accounts"]["A1"]["accountCapabilities"][SIEVE] = json!({ "sieveExtensions": ["fileinto", "vacation", "include"] });
    session["capabilities"][CONTACTS] = json!({});
    session["accounts"]["C1"] = json!({ "name": "me", "isPersonal": true, "isReadOnly": false, "accountCapabilities": { CONTACTS: {} } });
    session["primaryAccounts"][CONTACTS] = json!("C1");
    Mock::given(method("GET")).and(path("/jmap/session")).respond_with(ResponseTemplate::new(200).set_body_json(session)).mount(server).await;
    Mock::given(method("POST")).and(path("/api/")).respond_with(ResponseTemplate::new(200).set_body_json(answer)).mount(server).await;
}

async fn sent(server: &MockServer) -> Value {
    let posted = server.received_requests().await.unwrap().into_iter().rev().find(|r| r.method.as_str() == "POST" && r.url.path() == "/api/").unwrap();
    serde_json::from_slice(&posted.body).unwrap()
}

#[tokio::test]
async fn the_automatic_reply_is_the_singleton() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [["VacationResponse/get", {"accountId": "A1", "state": "v", "notFound": [], "list": [
        {"id": "singleton", "isEnabled": true, "fromDate": null, "toDate": "2026-11-01T00:00:00Z", "subject": null, "textBody": "Away", "htmlBody": null}
    ]}, "c0"]]})).await;
    let reply = client(&server).vacation().await.unwrap();
    assert!(reply.is_enabled);
    assert_eq!(reply.text_body.as_deref(), Some("Away"));
    assert_eq!(sent(&server).await["methodCalls"][0][1]["ids"], json!(["singleton"]));
}

#[tokio::test]
async fn a_reply_change_updates_the_singleton() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [["VacationResponse/set", {"accountId": "A1", "updated": {"singleton": null}}, "c0"]]})).await;
    let reply = VacationResponse { is_enabled: true, subject: Some("Out".into()), text_body: Some("Back Monday".into()), ..VacationResponse::default() };
    client(&server).set_vacation(&reply).await.unwrap();
    let update = &sent(&server).await["methodCalls"][0][1]["update"]["singleton"];
    assert_eq!(update["isEnabled"], true);
    assert_eq!(update["subject"], "Out");
}

#[tokio::test]
async fn contacts_go_to_their_own_account() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["ContactCard/query", {"accountId": "C1", "queryState": "q", "ids": ["K1"], "position": 0, "total": 1}, "c0"],
        ["ContactCard/get", {"accountId": "C1", "state": "k7", "notFound": [], "list": [{"id": "K1", "name": {"full": "Ada"}}]}, "c1"]
    ]})).await;
    let page = client(&server).cards_page(0, 100).await.unwrap();
    assert_eq!(page.state, "k7");
    assert_eq!(page.cards[0]["name"]["full"], "Ada");
    let body = sent(&server).await;
    assert_eq!(body["methodCalls"][0][1]["accountId"], "C1");
    assert!(body["using"].as_array().unwrap().contains(&json!(CONTACTS)));
}

#[tokio::test]
async fn a_new_card_is_read_back_by_its_creation_id() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["ContactCard/set", {"accountId": "C1", "created": {"k": {"id": "K9"}}}, "c0"],
        ["ContactCard/get", {"accountId": "C1", "state": "k8", "notFound": [], "list": [{"id": "K9", "name": {"full": "Grace"}}]}, "c1"]
    ]})).await;
    let mut card = Map::new();
    card.insert("name".into(), json!({"full": "Grace"}));
    let made = client(&server).set_card(&CardWrite::Create(card)).await.unwrap();
    assert_eq!(made["id"], "K9");
    assert_eq!(sent(&server).await["methodCalls"][1][1]["ids"], json!(["#k"]));
}

#[tokio::test]
async fn a_sieve_script_is_uploaded_then_set_and_activated() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/upload/A1/")).respond_with(ResponseTemplate::new(201).set_body_json(json!({
        "accountId": "A1", "blobId": "SB1", "type": "application/sieve", "size": 10
    }))).mount(&server).await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [["SieveScript/set", {"accountId": "A1", "created": {"s": {"id": "S1"}}}, "c0"]]})).await;
    let client = client(&server);
    let id = client.put_sieve("penguin-mail", None, "require \"fileinto\";").await.unwrap();
    assert_eq!(id, "S1");
    let set = &sent(&server).await["methodCalls"][0][1];
    assert_eq!(set["create"]["s"], json!({"name": "penguin-mail", "blobId": "SB1"}));
    assert_eq!(client.sieve_extensions().await.unwrap(), ["fileinto", "vacation", "include"]);
}

#[tokio::test]
async fn a_masked_address_is_made_enabled_for_its_site() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [["MaskedEmail/set", {"accountId": "A1", "created": {"m": {
        "id": "ME1", "email": "kite.fern@fastmail.com", "state": "enabled", "forDomain": "https://shop.example", "description": "Shop"
    }}}, "c0"]]})).await;
    let made = client(&server).create_masked("https://shop.example", "Shop").await.unwrap();
    assert_eq!(made.email, "kite.fern@fastmail.com");
    let created = &sent(&server).await["methodCalls"][0][1]["create"]["m"];
    assert_eq!(created["state"], "enabled");
    assert_eq!(created["forDomain"], "https://shop.example");
}
