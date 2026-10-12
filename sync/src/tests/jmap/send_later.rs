//! The outbox over a JMAP server that holds mail: the server sends at the
//! hour, the outbox settles, and every way of changing one's mind calls
//! the hold back first. Each `queue(&h)` is a new outbox, as after a
//! restart, so whatever carries over lives in the store.

use std::collections::HashMap;
use std::sync::Arc;

use mailrs_domain::{EpochMillis, Target};
use mailrs_jmap::JmapError;
use mailrs_store::outbox::{self, Queued};

use super::{Harness, over};
use crate::fake::FakeJmap;
use crate::tests::Connected;
use crate::{Outbox, Posted, now_millis};

const HOUR: EpochMillis = 3_600_000;
const MINUTE: EpochMillis = 60_000;

const CHECK_SENT: &str = "Penguin Mail could not confirm whether this message went out. Check Sent.";

fn queue(h: &Harness) -> Outbox<Connected> {
    Outbox::new(Arc::new(Connected(HashMap::from([(h.account_id, Arc::clone(&h.sync))]))), h.db.clone())
}

fn later(h: &Harness, at: EpochMillis) -> Queued {
    Queued {
        account_id: h.account_id,
        subject: "Trip".into(),
        recipients: "ada@example.com".into(),
        send_at: at,
        raw: Some(b"From: me@example.com\r\nTo: ada@example.com\r\nSubject: Trip\r\nMessage-ID: <trip@example.com>\r\n\r\nSee you\r\n".to_vec()),
        composer: "{}".into(),
        ..Queued::default()
    }
}

/// The same letter with a blind copy, which the server's copy leaves out.
fn blind(h: &Harness, at: EpochMillis) -> Queued {
    Queued {
        raw: Some(
            b"From: me@example.com\r\nTo: ada@example.com\r\nBcc: grace@example.com\r\nSubject: Trip\r\nMessage-ID: <trip@example.com>\r\n\r\nSee you\r\n"
                .to_vec(),
        ),
        ..later(h, at)
    }
}

async fn holding() -> Harness {
    over(FakeJmap::new().with_delayed_send(86_400)).await
}

async fn row(h: &Harness, id: i64) -> Option<Queued> {
    h.db.read(move |c| outbox::find(c, id)).await.unwrap()
}

async fn put(h: &Harness, message: Queued) -> i64 {
    match queue(h).schedule(message).await.unwrap() {
        Posted::Waiting(id) => id,
        other => panic!("expected Waiting, got {other:?}"),
    }
}

async fn scheduled(h: &Harness, at: EpochMillis) -> i64 {
    put(h, later(h, at)).await
}

fn pending(h: &Harness) -> usize {
    h.fake.with(|s| s.holds.values().filter(|hold| hold.status == "pending").count())
}

fn sent(h: &Harness) -> usize {
    h.fake.with(|s| s.sent.len())
}

fn asked(h: &Harness) -> usize {
    h.fake.with(|s| s.calls.iter().filter(|c| *c == "EmailSubmission/get").count())
}

#[tokio::test]
async fn send_later_on_a_server_that_holds_mail_leaves_it_with_the_server() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    let kept = row(&h, id).await.unwrap();
    // The server's copy has no Bcc, so the bytes stay here for Send Now
    // and a new hour.
    assert!(kept.held.is_some() && kept.raw.is_some(), "the server holds it, and the bytes stay here");
    assert_eq!(kept.draft_id, kept.message_id);
    assert_eq!(pending(&h), 1);
}

#[tokio::test]
async fn the_hour_passing_files_the_held_message_in_sent() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    let email = row(&h, id).await.unwrap().message_id.unwrap();
    h.fake.release_due(i64::MAX);
    let drained = queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    assert_eq!(drained.sent.len(), 1);
    assert!(row(&h, id).await.is_none());
    assert!(h.fake.with(|s| s.emails[&email].mailbox_ids.contains(&s.role_id("sent").unwrap())));
    assert_eq!(sent(&h), 1, "the server sent it; the outbox sent nothing");
}

#[tokio::test]
async fn before_the_server_sends_the_pass_only_moves_the_next_look() {
    let h = holding().await;
    let at = now_millis() + HOUR;
    let id = scheduled(&h, at).await;
    let now = at + 1_000;
    let outbox = queue(&h);
    let drained = outbox.send_due(now).await.unwrap();
    assert!(drained.sent.is_empty() && drained.stuck.is_empty());
    let kept = row(&h, id).await.unwrap();
    assert!(kept.held.is_some() && kept.send_at == at, "the row keeps the hour the person picked");
    let looks = asked(&h);
    outbox.send_due(now + 30_000).await.unwrap();
    assert_eq!(asked(&h), looks, "no second look within the minute");
    assert_eq!(sent(&h), 0);
}

#[tokio::test]
async fn a_hold_late_on_the_server_says_so_and_keeps_asking() {
    let h = holding().await;
    let at = now_millis() + 2 * MINUTE;
    let id = scheduled(&h, at).await;
    let now = at + 16 * MINUTE;
    // The hour has passed and the server still holds it.
    queue(&h).send_due(now).await.unwrap();
    let kept = row(&h, id).await.unwrap();
    assert!(kept.held.is_some() && kept.problem.is_none(), "still in Send Later, with Send Now and Cancel");
    let when = chrono::DateTime::from_timestamp_millis(now).unwrap().with_timezone(&chrono::Local);
    assert_eq!(crate::mailbox::waiting_line(&kept, when), "The server has not sent this message yet.");
    let early = chrono::DateTime::from_timestamp_millis(at + 14 * MINUTE).unwrap().with_timezone(&chrono::Local);
    assert_ne!(crate::mailbox::waiting_line(&kept, early), "The server has not sent this message yet.");
    let looks = asked(&h);
    queue(&h).send_due(now + 2 * MINUTE).await.unwrap();
    assert_eq!(asked(&h), looks + 1, "it asks again");
    assert_eq!(sent(&h), 0, "and never sends it here");
}

#[tokio::test]
async fn cancel_send_in_time_leaves_a_draft_and_no_hold() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    let kept = row(&h, id).await.unwrap();
    let target = Target::thread(h.account_id, kept.thread_id.clone().unwrap());
    let cancelled = queue(&h).cancel_scheduled(&[target]).await.unwrap();
    assert_eq!((cancelled.in_drafts, cancelled.already_sent), (1, 0));
    assert!(row(&h, id).await.is_none());
    let email = kept.message_id.unwrap();
    assert!(!h.fake.with(|s| s.emails[&email].keywords.contains(mailrs_jmap::HELD)));
}

#[tokio::test]
async fn a_cancel_that_comes_too_late_counts_the_message_as_sent() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    let kept = row(&h, id).await.unwrap();
    h.fake.release_due(i64::MAX);
    let target = Target::thread(h.account_id, kept.thread_id.clone().unwrap());
    let cancelled = queue(&h).cancel_scheduled(&[target]).await.unwrap();
    assert_eq!((cancelled.in_drafts, cancelled.already_sent), (0, 1));
    assert!(row(&h, id).await.is_none());
    assert_eq!(sent(&h), 1, "sent once, by the server");
}

#[tokio::test]
async fn a_hold_cancelled_and_deleted_elsewhere_is_not_called_a_draft() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    let kept = row(&h, id).await.unwrap();
    let email = kept.message_id.clone().unwrap();
    h.fake.with(|s| {
        s.holds.values_mut().for_each(|hold| hold.status = "canceled".into());
        s.emails.remove(&email);
    });
    let target = Target::thread(h.account_id, kept.thread_id.clone().unwrap());
    let cancelled = queue(&h).cancel_scheduled(&[target]).await.unwrap();
    assert_eq!((cancelled.in_drafts, cancelled.cancelled_elsewhere), (0, 1));
    assert!(row(&h, id).await.is_none());
}

#[tokio::test]
async fn a_cancel_the_server_cannot_hear_leaves_the_message_held() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    let kept = row(&h, id).await.unwrap();
    h.fake.with(|s| s.submission_get = false);
    let target = Target::thread(h.account_id, kept.thread_id.clone().unwrap());
    let cancelled = queue(&h).cancel_scheduled(&[target]).await.unwrap();
    assert_eq!((cancelled.in_drafts, cancelled.still_held), (0, 1));
    assert_eq!(row(&h, id).await.unwrap().held, kept.held);
}

#[tokio::test]
async fn send_now_calls_the_hold_back_and_sends_once() {
    let h = holding().await;
    let id = put(&h, blind(&h, now_millis() + HOUR)).await;
    assert!(matches!(queue(&h).send_one(id).await.unwrap(), Posted::Sent(_)));
    assert_eq!(sent(&h), 1);
    assert!(h.fake.with(|s| s.holds.values().all(|hold| hold.status == "canceled")));
    assert!(row(&h, id).await.is_none());
    let to = h.fake.with(|s| s.sent[0].rcpt_to.clone());
    assert!(to.contains(&"grace@example.com".to_string()), "the blind copy goes too: {to:?}");
}

#[tokio::test]
async fn send_now_after_the_server_sent_it_sends_nothing_more() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    h.fake.release_due(i64::MAX);
    assert!(matches!(queue(&h).send_one(id).await.unwrap(), Posted::Sent(_)));
    assert_eq!(sent(&h), 1);
    assert!(row(&h, id).await.is_none());
}

#[tokio::test]
async fn rescheduling_a_held_message_holds_it_again_at_the_new_hour() {
    let h = holding().await;
    let id = put(&h, blind(&h, now_millis() + HOUR)).await;
    let before = row(&h, id).await.unwrap();
    let moved = queue(&h).reschedule(id, now_millis() + 2 * HOUR).await.unwrap().unwrap();
    assert_ne!(moved.held, before.held);
    assert_ne!(moved.message_id, before.message_id);
    let old = before.message_id.unwrap();
    assert!(h.fake.with(|s| !s.emails.contains_key(&old)), "the old copy left Drafts");
    assert_eq!(pending(&h), 1);
    let to = h.fake.with(|s| s.holds.values().find(|hold| hold.status == "pending").unwrap().rcpt_to.clone());
    assert!(to.contains(&"grace@example.com".to_string()), "the blind copy is held again: {to:?}");
}

#[tokio::test]
async fn a_hold_the_server_could_not_take_falls_back_to_the_local_queue() {
    let h = holding().await;
    h.fake.with(|s| s.hold_failure = Some(JmapError::Network("connection reset".into())));
    let id = scheduled(&h, now_millis() + HOUR).await;
    let kept = row(&h, id).await.unwrap();
    assert_eq!(kept.held, None, "the app's own queue sends it at the hour");
    assert!(kept.raw.is_some() || kept.draft_id.is_some(), "the bytes are kept here or in Drafts");
}

#[tokio::test]
async fn a_hold_whose_answer_was_lost_is_found_and_sent_once() {
    let h = holding().await;
    h.fake.with(|s| s.hold_failure = Some(JmapError::HoldUnknown));
    let id = scheduled(&h, now_millis() + HOUR).await;
    let kept = row(&h, id).await.unwrap();
    let handle = h.fake.with(|s| s.holds.keys().next().cloned());
    assert_eq!(kept.held, handle, "the row takes the hold the server kept");
    h.fake.release_due(i64::MAX);
    let drained = queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    assert_eq!(drained.sent.len(), 1);
    assert_eq!(sent(&h), 1, "one send in total");
}

#[tokio::test]
async fn a_lost_answer_nobody_could_check_waits_and_never_sends_here() {
    let h = holding().await;
    h.fake.with(|s| {
        s.hold_failure = Some(JmapError::HoldUnknown);
        s.submission_query_failure = Some(JmapError::Network("connection reset".into()));
    });
    let id = scheduled(&h, now_millis() + HOUR).await;
    let kept = row(&h, id).await.unwrap();
    assert!(kept.held.is_some() && kept.raw.is_some() && kept.problem.is_none(), "{kept:?}");
    // Still out of reach at the hour: nothing goes from here.
    h.fake.with(|s| s.submission_query_failure = Some(JmapError::Network("connection reset".into())));
    let drained = queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    assert!(drained.sent.is_empty() && drained.stuck.is_empty());
    assert_eq!(sent(&h), 0);
    // Back in reach: the outbox finds the server's hold and settles it.
    h.fake.release_due(i64::MAX);
    queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    let drained = queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    assert_eq!(drained.sent.len(), 1);
    assert!(row(&h, id).await.is_none());
    assert_eq!(sent(&h), 1, "one send in total");
}

#[tokio::test]
async fn a_lost_answer_with_no_copy_on_the_server_goes_from_here() {
    let h = holding().await;
    h.fake.with(|s| {
        s.hold_failure = Some(JmapError::HoldUnknown);
        s.submission_query_failure = Some(JmapError::Network("connection reset".into()));
    });
    let id = scheduled(&h, now_millis() + HOUR).await;
    // The server never kept it after all.
    h.fake.with(|s| {
        s.holds.clear();
        let held: Vec<String> =
            s.emails.iter().filter(|(_, e)| e.keywords.contains(mailrs_jmap::HELD)).map(|(id, _)| id.clone()).collect();
        held.iter().for_each(|id| {
            s.emails.remove(id);
        });
    });
    queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    assert_eq!(row(&h, id).await.unwrap().held, None, "the outbox keeps it");
    let drained = queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    assert_eq!(drained.sent.len(), 1);
    assert_eq!(sent(&h), 1);
}

#[tokio::test]
async fn a_hold_the_server_forgot_asks_the_person_to_check_sent() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + 2 * MINUTE).await;
    // The server forgets the hold and leaves its copy held: it may have
    // gone, or not.
    h.fake.with(|s| s.holds.clear());
    let drained = queue(&h).send_due(now_millis() + HOUR).await.unwrap();
    assert_eq!(drained.stuck.len(), 1);
    let kept = row(&h, id).await.unwrap();
    assert_eq!(kept.problem.as_deref(), Some(CHECK_SENT));
    assert!(kept.held.is_some());
    assert!(matches!(queue(&h).send_one(id).await.unwrap(), Posted::Refused(_)), "Send Now sends nothing");
    queue(&h).try_now().await.unwrap();
    queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    assert_eq!(sent(&h), 0, "nothing here sends it");
    queue(&h).drop_one(id).await.unwrap();
    assert!(row(&h, id).await.is_none(), "the person can dismiss it");
}
