//! Moves, marks and Delete Forever over FakeJmap.

use std::collections::BTreeSet;

use mailrs_domain::Target;
use mailrs_domain::mailbox::keyword::{FLAGGED, SEEN};

use super::{fastmail, over, raw};
use crate::fake::FakeJmap;
use crate::{BackendError, MailBackend, MailOp, TriageAction};

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[tokio::test]
async fn removing_the_last_mailbox_files_the_message_in_archive() {
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &raw("a1@x", "Archive me"), now());
    h.mail().mailboxes().await.unwrap();
    h.mail().apply(std::slice::from_ref(&id), &[MailOp::RemoveFromMailbox(h.fake.mailbox("inbox"))]).await.unwrap();
    assert_eq!(h.fake.with(|s| s.emails[&id].mailbox_ids.clone()), BTreeSet::from([h.fake.mailbox("archive")]));
}

#[tokio::test]
async fn archive_on_a_label_account_keeps_the_message_s_id() {
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &raw("a2@x", "Labels"), now());
    h.bootstrap_all().await;
    let thread = h.fake.with(|s| s.emails[&id].thread_id.clone());
    h.sync.triage_all(&[Target::thread(h.account_id, thread)], &TriageAction::Archive, None).await.unwrap();
    assert_eq!(h.fake.with(|s| s.emails[&id].mailbox_ids.clone()), BTreeSet::from([h.fake.mailbox("archive")]));
    assert!(h.stored(&id).await);
}

#[tokio::test]
async fn archive_on_a_folder_account_moves_the_message() {
    let h = over(FakeJmap::new().mailboxes_per_email(Some(1))).await;
    let id = h.fake.deliver("inbox", &raw("a3@x", "Folders"), now());
    h.bootstrap_all().await;
    let thread = h.fake.with(|s| s.emails[&id].thread_id.clone());
    h.sync.triage_all(&[Target::thread(h.account_id, thread)], &TriageAction::Archive, None).await.unwrap();
    assert_eq!(h.fake.with(|s| s.emails[&id].mailbox_ids.clone()), BTreeSet::from([h.fake.mailbox("archive")]));
}

#[tokio::test]
async fn a_flag_touches_only_the_flag() {
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &raw("f1@x", "Flag"), now());
    h.fake.set_keyword(&id, SEEN, true);
    h.mail().mailboxes().await.unwrap();
    h.mail().apply(std::slice::from_ref(&id), &[MailOp::SetKeyword { keyword: FLAGGED.into(), on: true }]).await.unwrap();
    let marks = h.fake.with(|s| s.emails[&id].keywords.clone());
    assert_eq!(marks, BTreeSet::from([SEEN.to_string(), FLAGGED.to_string()]));
}

#[tokio::test]
async fn delete_forever_destroys_and_a_gone_message_counts_as_done() {
    let h = fastmail().await;
    let id = h.fake.deliver("trash", &raw("x1@x", "Bin"), now());
    h.mail().mailboxes().await.unwrap();
    h.mail().apply(&[id.clone(), "gone".into()], &[MailOp::Destroy]).await.unwrap();
    assert!(h.fake.with(|s| !s.emails.contains_key(&id)));
}

#[tokio::test]
async fn without_an_archive_the_last_mailbox_stays_and_says_why() {
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &raw("n1@x", "Stay"), now());
    h.fake.with(|s| {
        let archive = s.role_id("archive").unwrap();
        s.mailboxes.remove(&archive);
    });
    h.mail().mailboxes().await.unwrap();
    let refused = h.mail().apply(std::slice::from_ref(&id), &[MailOp::RemoveFromMailbox(h.fake.mailbox("inbox"))]).await.unwrap_err();
    assert_eq!(refused.taken, 0);
    assert!(matches!(refused.error, BackendError::Refused(_)));
    assert_eq!(h.fake.with(|s| s.emails[&id].mailbox_ids.len()), 1);
}

#[tokio::test]
async fn a_category_is_refused_in_words() {
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &raw("c1@x", "Cat"), now());
    let refused = h.mail().apply(&[id], &[MailOp::SetCategory { category: "social".into(), on: true }]).await.unwrap_err();
    assert!(matches!(refused.error, BackendError::Refused(_)));
}
