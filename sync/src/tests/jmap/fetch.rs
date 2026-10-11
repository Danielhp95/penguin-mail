//! The window, ids and metadata over FakeJmap.

use mailrs_domain::Role;

use super::{fastmail, raw};
use crate::{MailBackend, Want};

fn days_ago(days: i64) -> i64 {
    chrono::Utc::now().timestamp_millis() - days * 86_400_000
}

#[tokio::test]
async fn the_window_holds_recent_mail_and_the_whole_inbox() {
    let h = fastmail().await;
    let recent = h.fake.deliver("archive", &raw("w1@x", "Recent"), days_ago(2));
    let old_inbox = h.fake.deliver("inbox", &raw("w2@x", "Old but in the inbox"), days_ago(400));
    let old_archive = h.fake.deliver("archive", &raw("w3@x", "Old"), days_ago(400));
    h.bootstrap_all().await;
    assert!(h.stored(&recent).await && h.stored(&old_inbox).await);
    assert!(!h.stored(&old_archive).await);
}

#[tokio::test]
async fn backfill_pages_on_until_the_last_page() {
    let h = fastmail().await;
    for n in 0..205 {
        h.fake.deliver("inbox", &raw(&format!("p{n}@x"), "Paged"), days_ago(1));
    }
    let mail = h.mail();
    let first = mail.backfill(30, None).await.unwrap();
    assert_eq!(first.refs.len(), 200);
    let second = mail.backfill(30, first.next.as_deref()).await.unwrap();
    assert_eq!(second.refs.len(), 5);
    assert_eq!(second.next, None);
}

#[tokio::test]
async fn metadata_names_the_sender_subject_date_and_marks() {
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &raw("meta@x", "Hello"), days_ago(0));
    h.mail().mailboxes().await.unwrap();
    let found = h.mail().fetch(vec![Want::message(id.clone()), Want::message("gone")]).await.unwrap();
    let meta = &found.metas[0];
    assert_eq!(meta.subject, "Hello");
    assert_eq!(meta.from.as_ref().unwrap().email, "ada@example.com");
    assert_eq!(meta.rfc822_msgid.as_deref(), Some("meta@x"));
    assert_eq!(meta.roles, [Role::Inbox]);
    assert!(meta.date > 0);
    assert_eq!(found.gone, ["gone"]);
}

#[tokio::test]
async fn a_whole_thread_comes_in_date_order() {
    let h = fastmail().await;
    let first = h.fake.deliver("inbox", &raw("t1@x", "Plans"), days_ago(2));
    let reply = "From: Ada <ada@example.com>\r\nTo: me@example.com\r\nSubject: Re: Plans\r\nMessage-ID: <t2@x>\r\nIn-Reply-To: <t1@x>\r\n\r\nYes\r\n";
    let second = h.fake.deliver("inbox", reply.as_bytes(), days_ago(1));
    let thread = h.fake.with(|s| s.emails[&first].thread_id.clone());
    h.mail().mailboxes().await.unwrap();
    let found = h.mail().fetch_whole(vec![thread, "T-none".into()]).await.unwrap();
    let ids: Vec<&str> = found.whole[0].iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, [first.as_str(), second.as_str()]);
    assert_eq!(found.gone_threads, ["T-none"]);
}

#[tokio::test]
async fn a_sent_message_is_found_by_its_message_id() {
    let h = fastmail().await;
    let id = h.fake.deliver("sent", &raw("sent-1@x", "Out"), days_ago(0));
    h.mail().mailboxes().await.unwrap();
    assert_eq!(h.mail().find_sent("sent-1@x").await.unwrap(), Some(id));
    assert_eq!(h.mail().find_sent("nothing@x").await.unwrap(), None);
}

#[tokio::test]
async fn all_mail_takes_every_message_whatever_its_age() {
    let h = fastmail().await;
    let old = h.fake.deliver("archive", &raw("all@x", "Old"), days_ago(4000));
    h.mail().mailboxes().await.unwrap();
    let page = h.mail().backfill(crate::ALL_MAIL, None).await.unwrap();
    assert!(page.refs.iter().any(|r| r.id == old));
}

#[tokio::test]
async fn window_ids_stay_inside_their_mailbox() {
    let h = fastmail().await;
    h.fake.deliver("inbox", &raw("i1@x", "In"), days_ago(1));
    let archived = h.fake.deliver("archive", &raw("i2@x", "Out"), days_ago(1));
    h.mail().mailboxes().await.unwrap();
    let ids = h.mail().window_ids(30, Some(&h.fake.mailbox("archive"))).await.unwrap();
    assert_eq!(ids.into_iter().map(|r| r.id).collect::<Vec<_>>(), [archived]);
}
