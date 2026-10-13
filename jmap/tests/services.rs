//! The services beside mail, as wiremock receives their calls.

mod common;

use mailrs_jmap::{CardWrite, JmapError, MaskedState, VacationPatch};
use serde_json::{Map, Value, json};
use wiremock::matchers::{body_string_contains, method, path};
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
    let reply = VacationPatch { is_enabled: Some(true), subject: Some(Some("Out".into())), ..VacationPatch::default() };
    client(&server).set_vacation(&reply).await.unwrap();
    let update = &sent(&server).await["methodCalls"][0][1]["update"]["singleton"];
    assert_eq!(update, &json!({"isEnabled": true, "subject": "Out"}), "only what changed goes out");
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

async fn with_upload(server: &MockServer) {
    Mock::given(method("POST")).and(path("/upload/A1/")).respond_with(ResponseTemplate::new(201).set_body_json(json!({
        "accountId": "A1", "blobId": "SB1", "type": "application/sieve", "size": 10
    }))).mount(server).await;
}

#[tokio::test]
async fn a_sieve_script_is_uploaded_and_set() {
    let server = MockServer::start().await;
    with_upload(&server).await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [["SieveScript/set", {"accountId": "A1", "created": {"s": {"id": "S1"}}}, "c0"]]})).await;
    let client = client(&server);
    let id = client.put_sieve("penguin-mail", None, "require \"fileinto\";").await.unwrap();
    assert_eq!(id, "S1");
    let set = &sent(&server).await["methodCalls"][0][1];
    assert_eq!(set["create"]["s"], json!({"name": "penguin-mail", "blobId": "SB1"}));
    assert!(set.get("onSuccessActivateScript").is_none());
    assert_eq!(client.sieve_extensions().await.unwrap(), ["fileinto", "vacation", "include"]);
}

#[tokio::test]
async fn a_new_script_is_activated_in_the_same_call() {
    let server = MockServer::start().await;
    with_upload(&server).await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["SieveScript/set", {"accountId": "A1", "created": {"s": {"id": "S1"}}}, "c0"],
        ["SieveScript/set", {"accountId": "A1", "updated": {"S1": null}}, "c0"]
    ]})).await;
    let id = client(&server).put_sieve_active("penguin-mail", None, "keep;").await.unwrap();
    assert_eq!(id, "S1");
    assert_eq!(sent(&server).await["methodCalls"][0][1]["onSuccessActivateScript"], "#s");
}

#[tokio::test]
async fn an_existing_script_is_updated_and_activated_by_its_id() {
    let server = MockServer::start().await;
    with_upload(&server).await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["SieveScript/set", {"accountId": "A1", "updated": {"S1": null}}, "c0"],
        ["SieveScript/set", {"accountId": "A1", "updated": {"S1": null}}, "c0"]
    ]})).await;
    let id = client(&server).put_sieve_active("penguin-mail", Some("S1"), "keep;").await.unwrap();
    assert_eq!(id, "S1");
    let set = &sent(&server).await["methodCalls"][0][1];
    assert_eq!(set["onSuccessActivateScript"], "S1");
    assert_eq!(set["update"]["S1"], json!({"blobId": "SB1"}));
}

#[tokio::test]
async fn an_update_the_server_did_not_apply_is_an_error() {
    let server = MockServer::start().await;
    with_upload(&server).await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [["SieveScript/set", {"accountId": "A1", "updated": null}, "c0"]]})).await;
    assert!(client(&server).put_sieve("penguin-mail", Some("S1"), "keep;").await.is_err());
}

#[tokio::test]
async fn activating_a_script_reads_the_answer_to_the_activation() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["SieveScript/set", {"accountId": "A1"}, "c0"],
        ["SieveScript/set", {"accountId": "A1", "updated": {"S1": null}}, "c0"]
    ]})).await;
    client(&server).activate_sieve("S1").await.unwrap();
    assert_eq!(sent(&server).await["methodCalls"][0][1]["onSuccessActivateScript"], "S1");
}

#[tokio::test]
async fn a_refused_activation_is_an_error() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["SieveScript/set", {"accountId": "A1"}, "c0"],
        ["SieveScript/set", {"accountId": "A1", "notUpdated": {"S1": {"type": "notFound"}}}, "c0"]
    ]})).await;
    assert_eq!(client(&server).activate_sieve("S1").await, Err(JmapError::NotFound));
}

#[tokio::test]
async fn an_activation_that_goes_unanswered_is_an_error() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [["SieveScript/set", {"accountId": "A1"}, "c0"]]})).await;
    assert!(client(&server).activate_sieve("S1").await.is_err());
}

/// Answers `SieveScript/get` with `scripts`, ahead of the catch-all answer
/// `serving` mounts.
async fn reading_back(server: &MockServer, scripts: Value) {
    Mock::given(method("POST"))
        .and(path("/api/"))
        .and(body_string_contains("SieveScript/get"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sessionState": "s1", "methodResponses": [
            ["SieveScript/get", {"accountId": "A1", "state": "v2", "notFound": [], "list": scripts}, "c0"]
        ]})))
        .with_priority(1)
        .mount(server)
        .await;
}

async fn posts(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() == "POST" && r.url.path() == "/api/").count()
}

/// RFC 9661 as published, and Cyrus 3.13: one answer, with the created
/// script `isActive: true`.
#[tokio::test]
async fn a_create_answered_active_needs_no_second_answer() {
    let server = MockServer::start().await;
    with_upload(&server).await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["SieveScript/set", {"accountId": "A1", "created": {"s": {"id": "S1", "isActive": true, "blobId": "SB1"}}, "updated": null}, "c0"]
    ]})).await;
    assert_eq!(client(&server).put_sieve_active("penguin-mail", None, "keep;").await.unwrap(), "S1");
    assert_eq!(posts(&server).await, 1, "no read back");
}

/// Stalwart 0.16: the update answered with the script `isActive: true`,
/// and nothing said of the script it replaced.
#[tokio::test]
async fn an_update_answered_active_needs_no_second_answer() {
    let server = MockServer::start().await;
    with_upload(&server).await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["SieveScript/set", {"accountId": "A1", "updated": {"S1": {"blobId": "SB1", "isActive": true}}}, "c0"]
    ]})).await;
    assert_eq!(client(&server).put_sieve_active("penguin-mail", Some("S1"), "keep;").await.unwrap(), "S1");
}

/// Cyrus 3.13: an activation on its own answers the scripts it switched.
#[tokio::test]
async fn an_activation_answered_in_updated_is_taken() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["SieveScript/set", {"accountId": "A1", "updated": {"S0": {"isActive": false}, "S1": {"isActive": true}}}, "c0"]
    ]})).await;
    client(&server).activate_sieve("S1").await.unwrap();
    assert_eq!(posts(&server).await, 1, "no read back");
}

/// Stalwart 0.16: an activation on its own answers with neither `updated`
/// nor a new state, though the script is then active.
#[tokio::test]
async fn an_activation_answered_with_nothing_is_read_back() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["SieveScript/set", {"accountId": "A1", "oldState": "v1", "newState": "v1"}, "c0"]
    ]})).await;
    reading_back(&server, json!([{"id": "S0", "blobId": "B0", "isActive": false}, {"id": "S1", "blobId": "B1", "isActive": true}])).await;
    assert_eq!(client(&server).activate_sieve("S1").await, Ok(()));
    let read: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains("SieveScript/get"))
        .collect();
    assert_eq!(read.len(), 1, "the scripts were read back once");
    assert_eq!(posts(&server).await, 2);
}

#[tokio::test]
async fn an_activation_the_read_back_does_not_show_is_an_error() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["SieveScript/set", {"accountId": "A1", "oldState": "v1", "newState": "v1"}, "c0"]
    ]})).await;
    reading_back(&server, json!([{"id": "S0", "blobId": "B0", "isActive": true}, {"id": "S1", "blobId": "B1", "isActive": false}])).await;
    assert!(client(&server).activate_sieve("S1").await.is_err());
}

#[tokio::test]
async fn a_create_answered_without_activation_is_read_back() {
    let server = MockServer::start().await;
    with_upload(&server).await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["SieveScript/set", {"accountId": "A1", "created": {"s": {"id": "S1"}}}, "c0"]
    ]})).await;
    reading_back(&server, json!([{"id": "S1", "blobId": "B1", "isActive": true}])).await;
    assert_eq!(client(&server).put_sieve_active("penguin-mail", None, "keep;").await.unwrap(), "S1");
}

#[tokio::test]
async fn card_pages_stay_under_the_servers_get_limit() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["ContactCard/query", {"accountId": "C1", "queryState": "q", "ids": [], "position": 0}, "c0"],
        ["ContactCard/get", {"accountId": "C1", "state": "k", "notFound": [], "list": []}, "c1"]
    ]})).await;
    let client = client(&server);
    client.cards_page(0, 100_000).await.unwrap();
    assert_eq!(sent(&server).await["methodCalls"][0][1]["limit"], 50, "the session says 50");
    client.cards_page(0, 0).await.unwrap();
    assert_eq!(sent(&server).await["methodCalls"][0][1]["limit"], 1, "zero becomes one");
}

#[tokio::test]
async fn card_changes_fetch_the_created_and_the_updated_cards() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["ContactCard/changes", {"accountId": "C1", "oldState": "k1", "newState": "k2", "hasMoreChanges": false, "created": ["K1"], "updated": ["K2"], "destroyed": ["K3"]}, "c0"],
        ["ContactCard/get", {"accountId": "C1", "state": "k2", "notFound": [], "list": [{"id": "K1"}]}, "c1"],
        ["ContactCard/get", {"accountId": "C1", "state": "k2", "notFound": [], "list": [{"id": "K2"}]}, "c2"]
    ]})).await;
    let changed = client(&server).card_changes("k1", 100_000).await.unwrap();
    assert_eq!(changed.changes.new_state, "k2");
    assert_eq!(changed.changes.destroyed, ["K3"]);
    assert_eq!(changed.cards.iter().map(|c| c["id"].as_str().unwrap()).collect::<Vec<_>>(), ["K1", "K2"]);
    let body = sent(&server).await;
    assert_eq!(body["methodCalls"][0][1]["maxChanges"], 50);
    assert_eq!(body["methodCalls"][2][1]["#ids"]["path"], "/updated");
}

#[tokio::test]
async fn a_card_edit_sends_the_patch_and_reads_the_card_back() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["ContactCard/set", {"accountId": "C1", "updated": {"K1": null}}, "c0"],
        ["ContactCard/get", {"accountId": "C1", "state": "k9", "notFound": [], "list": [{"id": "K1", "name": {"full": "Ada L."}}]}, "c1"]
    ]})).await;
    let mut patch = Map::new();
    patch.insert("name/full".into(), json!("Ada L."));
    let card = client(&server).set_card(&CardWrite::Update { id: "K1".into(), patch }).await.unwrap();
    assert_eq!(card["name"]["full"], "Ada L.");
    let body = sent(&server).await;
    assert_eq!(body["methodCalls"][0][1]["update"]["K1"]["name/full"], "Ada L.");
    assert_eq!(body["methodCalls"][1][1]["ids"], json!(["K1"]));
}

#[tokio::test]
async fn a_refused_card_edit_is_an_error() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [
        ["ContactCard/set", {"accountId": "C1", "notUpdated": {"K1": {"type": "notFound"}}}, "c0"],
        ["ContactCard/get", {"accountId": "C1", "state": "k9", "notFound": ["K1"], "list": []}, "c1"]
    ]})).await;
    let write = CardWrite::Update { id: "K1".into(), patch: Map::new() };
    assert_eq!(client(&server).set_card(&write).await, Err(JmapError::NotFound));
}

#[tokio::test]
async fn a_masked_address_is_switched_off_by_a_typed_state() {
    let server = MockServer::start().await;
    serving(&server, json!({"sessionState": "s1", "methodResponses": [["MaskedEmail/set", {"accountId": "A1", "updated": {"ME1": null}}, "c0"]]})).await;
    client(&server).set_masked_state("ME1", MaskedState::Disabled).await.unwrap();
    assert_eq!(sent(&server).await["methodCalls"][0][1]["update"]["ME1"]["state"], "disabled");
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

#[tokio::test]
async fn a_masked_listing_drops_deleted_addresses_and_stops_at_the_cap() {
    let server = MockServer::start().await;
    let mut list: Vec<Value> = (0..mailrs_jmap::MOST_MASKED + 5)
        .map(|i| json!({"id": format!("ME{i}"), "email": format!("m{i}@example.com"), "state": "enabled"}))
        .collect();
    list.insert(0, json!({"id": "GONE", "email": "gone@example.com", "state": "deleted"}));
    serving(&server, json!({"sessionState": "s1", "methodResponses": [["MaskedEmail/get", {"accountId": "A1", "state": "m", "notFound": [], "list": list}, "c0"]]})).await;
    let listed = client(&server).masked_emails().await.unwrap();
    assert_eq!(listed.len(), mailrs_jmap::MOST_MASKED);
    assert!(listed.iter().all(|m| m.state != "deleted"));
}
