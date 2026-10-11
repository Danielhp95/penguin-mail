//! Drafts and sending over FakeJmap.

use mailrs_jmap::{Identity, SUBMISSION};

use super::{fastmail, over};
use crate::fake::FakeJmap;
use crate::{BackendError, MailBackend};

fn letter(from: &str, extra: &str) -> Vec<u8> {
    format!(
        "From: Me <{from}>\r\nTo: ada@example.com\r\n{extra}Subject: Hi\r\nMessage-ID: <out-1@example.com>\r\n\r\nHello\r\n"
    )
    .into_bytes()
}

fn alias(id: &str, email: &str) -> Identity {
    Identity {
        id: id.into(),
        name: String::new(),
        email: email.into(),
        reply_to: None,
        bcc: None,
        text_signature: String::new(),
        html_signature: String::new(),
        may_delete: true,
    }
}

#[tokio::test]
async fn bcc_goes_in_the_envelope_and_not_in_the_copy() {
    let h = fastmail().await;
    h.mail().send(&letter("me@example.com", "Bcc: secret@example.com\r\n"), None).await.unwrap();
    let sent = h.fake.with(|s| s.sent[0].clone());
    assert!(sent.rcpt_to.contains(&"secret@example.com".to_string()));
    assert!(sent.rcpt_to.contains(&"ada@example.com".to_string()));
    assert!(!String::from_utf8_lossy(&sent.raw).to_ascii_lowercase().contains("bcc:"));
}

#[tokio::test]
async fn the_copy_lands_in_sent_without_draft() {
    let h = fastmail().await;
    let id = h.mail().send(&letter("me@example.com", ""), None).await.unwrap();
    let (mailboxes, keywords) = h.fake.with(|s| (s.emails[&id].mailbox_ids.clone(), s.emails[&id].keywords.clone()));
    assert_eq!(mailboxes.into_iter().collect::<Vec<_>>(), [h.fake.mailbox("sent")]);
    assert!(!keywords.contains("$draft"));
}

#[tokio::test]
async fn an_alias_sends_as_its_own_identity() {
    let h = fastmail().await;
    h.fake.with(|s| s.identities.push(alias("I2", "Alias@Example.com")));
    h.mail().send(&letter("alias@example.com", ""), None).await.unwrap();
    assert_eq!(h.fake.with(|s| s.sent[0].identity_id.clone()), "I2");
}

#[tokio::test]
async fn a_whole_domain_identity_covers_any_address_there() {
    let h = fastmail().await;
    h.fake.with(|s| s.identities.push(alias("I3", "*@example.org")));
    h.mail().send(&letter("shop@example.org", ""), None).await.unwrap();
    assert_eq!(h.fake.with(|s| s.sent[0].identity_id.clone()), "I3");
}

#[tokio::test]
async fn a_draft_saved_twice_keeps_one_copy_under_a_new_id() {
    let h = fastmail().await;
    let first = h.mail().save_draft(None, &letter("me@example.com", ""), None).await.unwrap();
    let second = h.mail().save_draft(Some(&first.draft_id), &letter("me@example.com", ""), None).await.unwrap();
    assert_ne!(first.draft_id, second.draft_id);
    assert!(h.fake.with(|s| !s.emails.contains_key(&first.draft_id)));
    let listed: Vec<String> = h.mail().list_drafts().await.unwrap().into_iter().map(|d| d.draft_id).collect();
    assert_eq!(listed, std::slice::from_ref(&second.draft_id));
    let sent = h.mail().send_draft(&second.draft_id).await.unwrap();
    assert!(h.fake.with(|s| !s.emails.contains_key(&second.draft_id) && s.emails.contains_key(&sent)));
}

#[tokio::test]
async fn a_draft_whose_old_copy_stays_still_saves_and_tries_the_old_one_again() {
    let h = fastmail().await;
    let first = h.mail().save_draft(None, &letter("me@example.com", ""), None).await.unwrap();
    h.fake.with(|s| s.refused.insert(first.draft_id.clone()));
    let second = h.mail().save_draft(Some(&first.draft_id), &letter("me@example.com", ""), None).await.unwrap();
    assert!(h.fake.with(|s| s.emails.contains_key(&second.draft_id)));
    assert_eq!(h.fake.with(|s| s.calls.iter().filter(|c| *c == "Email/set").count()), 1);
}

#[tokio::test]
async fn a_draft_whose_old_copy_is_gone_saves_without_trying_again() {
    let h = fastmail().await;
    let first = h.mail().save_draft(None, &letter("me@example.com", ""), None).await.unwrap();
    h.fake.destroy(&first.draft_id);
    h.mail().save_draft(Some(&first.draft_id), &letter("me@example.com", ""), None).await.unwrap();
    assert_eq!(h.fake.with(|s| s.calls.iter().filter(|c| *c == "Email/set").count()), 0);
}

#[tokio::test]
async fn a_read_only_account_refuses_to_send_in_words() {
    let h = over(FakeJmap::new().without_capability(SUBMISSION)).await;
    let refused = h.mail().send(&letter("me@example.com", ""), None).await;
    assert!(matches!(refused, Err(BackendError::Refused(_))), "{refused:?}");
    assert!(h.fake.with(|s| s.sent.is_empty()));
}

#[tokio::test]
async fn a_read_only_account_refuses_to_save_or_send_a_draft() {
    let h = over(FakeJmap::new().without_capability(SUBMISSION)).await;
    let saved = h.mail().save_draft(None, &letter("me@example.com", ""), None).await;
    assert!(matches!(saved, Err(BackendError::Refused(_))), "{saved:?}");
    let draft = h.fake.deliver("drafts", &letter("me@example.com", ""), 0);
    let sent = h.mail().send_draft(&draft).await;
    assert!(matches!(sent, Err(BackendError::Refused(_))), "{sent:?}");
    assert!(h.fake.with(|s| s.sent.is_empty() && s.emails.len() == 1));
}

#[tokio::test]
async fn an_account_without_identities_says_so() {
    let h = fastmail().await;
    h.fake.with(|s| s.identities.clear());
    let refused = h.mail().send(&letter("me@example.com", ""), None).await;
    assert!(matches!(refused, Err(BackendError::Refused(_))), "{refused:?}");
}

#[tokio::test]
async fn a_refused_submission_says_why_and_leaves_no_copy() {
    let h = over(FakeJmap::new().refusing_submissions("forbiddenToSend")).await;
    let refused = h.mail().send(&letter("me@example.com", ""), None).await;
    let Err(BackendError::Refused(why)) = refused else { panic!("{refused:?}") };
    assert_eq!(why, "The server does not let this account send mail now.");
    assert!(h.fake.with(|s| s.sent.is_empty() && s.emails.is_empty()));
}

#[tokio::test]
async fn an_account_with_neither_drafts_nor_sent_refuses_to_send_in_words() {
    let h = fastmail().await;
    h.fake.with(|s| s.mailboxes.retain(|_, m| !matches!(m.role.as_deref(), Some("drafts" | "sent"))));
    let refused = h.mail().send(&letter("me@example.com", ""), None).await;
    let Err(BackendError::Refused(why)) = refused else { panic!("{refused:?}") };
    assert_eq!(why, "The server has neither a Drafts nor a Sent mailbox to keep the message in.");
    assert!(h.fake.with(|s| s.sent.is_empty()));
}
