//! One look for new mail at the mail seam, for a POP3 account: the adapter
//! downloads and stores what is new and hands back what it stored for the
//! caller to announce, never fetches a message twice, and a look cut short
//! keeps what it stored and leaves the rest to the next.

use std::sync::Arc;

use mailrs_domain::{MailSet, RemoveSetting, Role};

use super::pop3::pop3_harness;
use crate::fake::{FakePop3, pop3_mail};
use crate::{BackendError, Look, Looked, MailBackend};

async fn look(mail: &crate::AnyMail) -> Looked {
    match mail.look_for_mail().await {
        Look::Done(looked) => looked,
        Look::Replay => panic!("a POP3 account looks for itself"),
    }
}

#[tokio::test]
async fn a_look_stores_new_mail_and_hands_it_to_the_caller_to_announce() {
    let h = pop3_harness(FakePop3::default().with_message("u1", &pop3_mail(1)), RemoveSetting::Never).await;
    let mail = &h.sync.services().mail;
    // What listing the role mailboxes announced.
    while h.events.try_recv().is_ok() {}
    let first = look(mail).await;
    assert!(first.failed.is_none());
    assert_eq!(first.threads.len(), 1, "the first message's thread");
    assert!(first.new_mail.is_empty(), "a first download announces none");

    h.fake.add("u2", &pop3_mail(2));
    let second = look(mail).await;
    assert_eq!(second.new_mail, ["pop3/u2"]);
    assert_eq!(h.ids_in(MailSet::Role(Role::Inbox)).await, ["pop3/u1", "pop3/u2"]);
    assert!(h.events.try_recv().is_err(), "the adapter announces nothing itself");
}

#[tokio::test]
async fn a_second_look_fetches_nothing_twice() {
    let fake = FakePop3::default().with_message("u1", &pop3_mail(1)).with_message("u2", &pop3_mail(2));
    let h = pop3_harness(fake, RemoveSetting::Never).await;
    look(&h.sync.services().mail).await;
    let again = look(&h.again().services().mail).await;
    assert!(again.threads.is_empty() && again.new_mail.is_empty());
    assert!(again.failed.is_none());
    assert_eq!(h.fake.retr_calls(), [1, 2]);
}

#[tokio::test]
async fn a_look_cut_short_keeps_what_it_stored_and_the_next_carries_on_from_there() {
    let five = || (1..=5).fold(FakePop3::default(), |fake, n| fake.with_message(&format!("u{n}"), &pop3_mail(n)));
    let h = pop3_harness(five().going_away_after_retrs(2), RemoveSetting::Never).await;
    let cut = look(&h.sync.services().mail).await;
    assert!(matches!(cut.failed, Some(BackendError::Offline(_))), "{:?}", cut.failed);
    assert_eq!(cut.threads.len(), 2, "the two messages stored before the drop");
    assert_eq!(h.ids_in(MailSet::Role(Role::Inbox)).await, ["pop3/u1", "pop3/u2"]);

    let rest = Arc::new(five());
    let next = look(&h.with_server(Arc::clone(&rest)).services().mail).await;
    assert!(next.failed.is_none());
    assert_eq!(rest.retr_calls(), [4, 5, 3], "only what was left");
    assert_eq!(h.ids_in(MailSet::Role(Role::Inbox)).await.len(), 5);
}

#[tokio::test]
async fn a_refused_sign_in_comes_back_as_the_looks_failure() {
    let h = pop3_harness(FakePop3::default().refusing_sign_in(), RemoveSetting::Never).await;
    let refused = look(&h.sync.services().mail).await;
    assert!(matches!(refused.failed, Some(BackendError::NeedsReauth)), "{:?}", refused.failed);
    assert!(refused.threads.is_empty());
}
