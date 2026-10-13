//! The account's identities over FakeJmap.

use mailrs_jmap::{Identity, SUBMISSION};

use super::{fastmail, over};
use crate::IdentityService;
use crate::fake::FakeJmap;

fn identity(id: &str, email: &str, name: &str, text: &str, html: &str) -> Identity {
    Identity {
        id: id.into(),
        name: name.into(),
        email: email.into(),
        reply_to: None,
        bcc: None,
        text_signature: text.into(),
        html_signature: html.into(),
        may_delete: true,
    }
}

#[tokio::test]
async fn aliases_come_with_their_names_and_signatures() {
    let h = fastmail().await;
    h.fake.with(|s| {
        s.identities = vec![
            identity("I1", "me@example.com", "Me", "-- \nMe", ""),
            identity("I2", "sales@example.com", "Sales", "", "<p>The <b>Sales</b> team</p>"),
            identity("I3", "*@example.org", "", "", ""),
        ]
    });
    let listed = h.sync.services().identities.identities().await.unwrap();
    let emails: Vec<&str> = listed.iter().map(|a| a.email.as_str()).collect();
    assert_eq!(emails, ["me@example.com", "sales@example.com"], "a whole-domain identity is no address");
    assert!(listed[0].default && !listed[1].default);
    assert_eq!(listed[0].name.as_deref(), Some("Me"));
    assert_eq!(listed[1].signature, "The Sales team");
}

#[tokio::test]
async fn without_sending_the_account_sends_as_nobody_but_itself() {
    let h = over(FakeJmap::new().without_capability(SUBMISSION)).await;
    let listed = h.sync.services().identities.identities().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].email, "me@example.com");
}

/// Cyrus with a user id under its default domain (`user1`, not
/// `user1@example.com`) answers its one identity with an empty email.
fn cyrus_like(username: &str) -> FakeJmap {
    let fake = FakeJmap::new();
    fake.with(|s| {
        s.session.username = username.into();
        s.identities = vec![identity(username, "", "", "", "")];
    });
    fake
}

#[tokio::test]
async fn an_empty_identity_takes_the_address_the_server_signs_in_as() {
    let h = over(cyrus_like("me")).await;
    let listed = h.sync.services().identities.identities().await.unwrap();
    let emails: Vec<&str> = listed.iter().map(|a| a.email.as_str()).collect();
    assert_eq!(emails, ["me@example.com"]);
}

#[tokio::test]
async fn an_empty_identity_takes_a_whole_address_user_name() {
    let h = over(cyrus_like("Me@Example.com")).await;
    let listed = h.sync.services().identities.identities().await.unwrap();
    let emails: Vec<&str> = listed.iter().map(|a| a.email.as_str()).collect();
    assert_eq!(emails, ["me@example.com"]);
}

#[tokio::test]
async fn an_empty_identity_takes_no_address_the_server_does_not_name() {
    let h = over(cyrus_like("someone")).await;
    let listed = h.sync.services().identities.identities().await.unwrap();
    assert!(listed.is_empty(), "{listed:?}");
}

#[tokio::test]
async fn an_empty_identity_takes_the_address_the_principal_names() {
    let fake = cyrus_like("u-1234");
    fake.with(|s| {
        let account = s.session.accounts.values_mut().next().unwrap();
        account.account_capabilities.insert(
            "urn:ietf:params:jmap:principals".into(),
            serde_json::json!({ "urn:ietf:params:jmap:calendars": { "sendTo": { "imip": "mailto:ME@example.com" } } }),
        );
    });
    let h = over(fake).await;
    let listed = h.sync.services().identities.identities().await.unwrap();
    let emails: Vec<&str> = listed.iter().map(|a| a.email.as_str()).collect();
    assert_eq!(emails, ["me@example.com"]);
}
