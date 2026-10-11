//! The change feed over FakeJmap.

use mailrs_domain::mailbox::keyword::{FLAGGED, SEEN};

use super::{fastmail, raw};
use crate::MailBackend;

#[tokio::test]
async fn new_mail_arrives_with_its_places_and_marks() {
    let h = fastmail().await;
    h.bootstrap_all().await;
    let id = h.fake.deliver("inbox", &raw("n1@x", "New"), chrono::Utc::now().timestamp_millis());
    h.fake.set_keyword(&id, FLAGGED, true);
    h.look().await;
    let held = h.held(&id).await;
    assert_eq!(held.mailboxes, [h.fake.mailbox("inbox")]);
    assert_eq!(held.keywords, [FLAGGED]);
}

#[tokio::test]
async fn a_mark_and_a_move_elsewhere_reach_the_store() {
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &raw("m1@x", "Moved"), chrono::Utc::now().timestamp_millis());
    h.bootstrap_all().await;
    h.fake.set_keyword(&id, SEEN, true);
    h.fake.move_to(&id, &h.fake.mailbox("archive"));
    h.look().await;
    let held = h.held(&id).await;
    assert_eq!(held.mailboxes, [h.fake.mailbox("archive")]);
    assert_eq!(held.keywords, [SEEN]);
}

#[tokio::test]
async fn a_mark_taken_off_elsewhere_comes_off_here() {
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &raw("k1@x", "Flag"), chrono::Utc::now().timestamp_millis());
    h.fake.set_keyword(&id, FLAGGED, true);
    h.bootstrap_all().await;
    h.fake.set_keyword(&id, FLAGGED, false);
    h.look().await;
    assert!(h.held(&id).await.keywords.is_empty());
}

#[tokio::test]
async fn a_message_destroyed_elsewhere_leaves_the_store() {
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &raw("d1@x", "Gone"), chrono::Utc::now().timestamp_millis());
    h.bootstrap_all().await;
    assert!(h.stored(&id).await);
    h.fake.destroy(&id);
    h.look().await;
    assert!(!h.stored(&id).await);
}

#[tokio::test]
async fn a_forgotten_state_relists_without_doubling() {
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &raw("f1@x", "Kept"), chrono::Utc::now().timestamp_millis());
    h.bootstrap_all().await;
    h.fake.forget_changes();
    let later = h.fake.deliver("inbox", &raw("f2@x", "Later"), chrono::Utc::now().timestamp_millis());
    h.look().await;
    h.look().await;
    assert!(h.stored(&id).await && h.stored(&later).await);
    let account = h.account_id;
    let count: i64 = h
        .db
        .read(move |c| Ok(c.query_row("SELECT COUNT(*) FROM messages WHERE account_id = ?1", [account], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn a_new_mailbox_is_listed_after_the_feed_sees_it() {
    let h = fastmail().await;
    h.bootstrap_all().await;
    let trips = h.fake.add_mailbox("Trips", None);
    assert!(h.mail().mailbox_threads(&trips).await.is_err(), "not listed yet");
    h.look().await;
    assert_eq!(h.mail().mailbox_threads(&trips).await.unwrap(), 0);
}

#[tokio::test]
async fn the_state_is_kept_whole_as_json() {
    let h = fastmail().await;
    let changes = h.sync.services().mail.changes(None).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(changes.state.as_str()).unwrap();
    assert!(parsed["email"].is_string() && parsed["mailbox"].is_string());
    assert!(changes.changes.is_empty());
}

#[tokio::test]
async fn a_forgotten_state_answers_state_lost() {
    let h = fastmail().await;
    let first = h.sync.services().mail.changes(None).await.unwrap();
    h.fake.deliver("inbox", &raw("s1@x", "Old"), chrono::Utc::now().timestamp_millis());
    h.fake.forget_changes();
    let err = h.sync.services().mail.changes(Some(&first.state)).await.unwrap_err();
    assert!(matches!(err, crate::BackendError::StateLost), "{err:?}");
}

#[tokio::test]
async fn a_changed_message_comes_restated_with_its_losses() {
    use crate::services::RemoteChange;
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &raw("r1@x", "Whole"), chrono::Utc::now().timestamp_millis());
    let first = h.sync.services().mail.changes(None).await.unwrap();
    h.fake.set_keyword(&id, SEEN, true);
    let next = h.sync.services().mail.changes(Some(&first.state)).await.unwrap();
    let inbox = h.fake.mailbox("inbox");
    let archive = h.fake.mailbox("archive");
    assert!(next.changes.iter().any(|c| matches!(c, RemoteChange::Gained { id: i, memberships, .. }
        if i == &id && memberships.contains(&mailrs_domain::Membership::Keyword(SEEN.into()))
            && memberships.contains(&mailrs_domain::Membership::Mailbox(inbox.clone())))));
    assert!(next.changes.iter().any(|c| matches!(c, RemoteChange::Lost { id: i, memberships }
        if i == &id && memberships.contains(&mailrs_domain::Membership::Mailbox(archive.clone()))
            && memberships.contains(&mailrs_domain::Membership::Keyword(FLAGGED.into())))));
}

#[tokio::test]
async fn a_large_change_set_is_read_in_pages_across_looks() {
    let h = fastmail().await;
    let first = h.sync.services().mail.changes(None).await.unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    for n in 0..1100 {
        h.fake.deliver("inbox", &raw(&format!("p{n}@x"), "Bulk"), now);
    }
    let next = h.sync.services().mail.changes(Some(&first.state)).await.unwrap();
    let gained = next.changes.iter().filter(|c| matches!(c, crate::services::RemoteChange::Gained { .. })).count();
    assert_eq!(gained, 1100);
}
