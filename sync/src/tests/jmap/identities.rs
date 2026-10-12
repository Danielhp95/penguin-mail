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
