//! The outbox over a JMAP server that holds mail: the server sends at the
//! hour, the outbox settles, and every way of changing one's mind calls
//! the hold back first. Each `queue(&h)` is a new outbox, as after a
//! restart, so whatever carries over lives in the store.

use std::collections::HashMap;
use std::sync::Arc;

use mailrs_domain::{EpochMillis, MailSet, Role, Target};
use mailrs_jmap::JmapError;
use mailrs_store::outbox::{self, Queued};

use super::{Harness, beside, over};
use crate::fake::FakeJmap;
use crate::tests::Connected;
use crate::{Outbox, Posted, Reopened, now_millis, server_holds};

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
    assert_eq!(pending(&h), 0);
    assert!(!h.fake.with(|s| s.emails.values().any(|e| e.keywords.contains(mailrs_jmap::HELD))), "no held copy is left");
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
    assert!(kept.held.is_some() && kept.raw.is_some(), "no answer: the server may hold it, so nothing goes from here yet");
    // A later pass finds nothing on the server, and the app's own queue
    // sends it at the hour.
    let drained = queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    assert_eq!(drained.sent.len(), 1);
    assert!(row(&h, id).await.is_none());
    assert_eq!(sent(&h), 1);
}

/// Takes the server's hold and its copy away for a while, as a query that
/// lags behind a commit does, and gives back a way to put them back.
fn hide_holds(h: &Harness) -> impl FnOnce() + '_ {
    let hidden = h.fake.with(|s| {
        let holds = std::mem::take(&mut s.holds);
        let copies: Vec<_> = holds.values().filter_map(|hold| s.emails.remove_entry(&hold.email_id)).collect();
        (holds, copies)
    });
    move || {
        h.fake.with(|s| {
            s.holds.extend(hidden.0);
            s.emails.extend(hidden.1);
        });
    }
}

#[tokio::test]
async fn a_hold_that_shows_up_after_an_empty_look_is_sent_once() {
    let h = holding().await;
    h.fake.with(|s| s.hold_failure = Some(JmapError::HoldUnknown));
    let id = scheduled(&h, now_millis() + HOUR).await;
    let restore = hide_holds(&h);
    let drained = queue(&h).send_due(now_millis() + 2 * MINUTE).await.unwrap();
    assert!(drained.sent.is_empty());
    assert!(row(&h, id).await.unwrap().held.is_some(), "an empty look before the hour decides nothing");
    restore();
    h.fake.release_due(i64::MAX);
    queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    assert!(row(&h, id).await.is_none());
    assert_eq!(sent(&h), 1, "one send in total");
}

#[tokio::test]
async fn a_lost_answer_is_not_looked_for_until_a_minute_has_passed() {
    let h = holding().await;
    h.fake.with(|s| s.hold_failure = Some(JmapError::Network("connection reset".into())));
    let id = scheduled(&h, now_millis() + 30_000).await;
    let drained = queue(&h).send_due(now_millis() + 40_000).await.unwrap();
    assert!(drained.sent.is_empty() && row(&h, id).await.unwrap().held.is_some(), "too soon to trust an empty look");
    queue(&h).send_due(now_millis() + 2 * MINUTE).await.unwrap();
    assert_eq!(sent(&h), 1);
}

#[tokio::test]
async fn a_hold_whose_answer_was_lost_is_found_and_sent_once() {
    let h = holding().await;
    h.fake.with(|s| s.hold_failure = Some(JmapError::HoldUnknown));
    let id = scheduled(&h, now_millis() + HOUR).await;
    queue(&h).send_due(now_millis() + 2 * MINUTE).await.unwrap();
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
    h.fake.with(|s| s.hold_failure = Some(JmapError::HoldUnknown));
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
    h.fake.with(|s| s.hold_failure = Some(JmapError::HoldUnknown));
    let id = scheduled(&h, now_millis() + HOUR).await;
    // The server never kept it after all.
    let _gone = hide_holds(&h);
    let drained = queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    assert_eq!(drained.sent.len(), 1, "looked for once more, then sent from here");
    assert!(row(&h, id).await.is_none());
    assert_eq!(sent(&h), 1);
}

#[tokio::test]
async fn send_now_on_a_lost_answer_with_nothing_found_asks_the_person() {
    let h = holding().await;
    h.fake.with(|s| s.hold_failure = Some(JmapError::Network("connection reset".into())));
    let id = scheduled(&h, now_millis() + HOUR).await;
    assert_eq!(queue(&h).send_one(id).await.unwrap(), Posted::Refused(CHECK_SENT.into()));
    assert_eq!(row(&h, id).await.unwrap().problem.as_deref(), Some(CHECK_SENT));
    assert_eq!(sent(&h), 0);
}

#[tokio::test]
async fn deleting_a_held_message_stops_the_servers_send() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    queue(&h).drop_one(id).await.unwrap();
    assert!(row(&h, id).await.is_none());
    assert_eq!(pending(&h), 0);
    h.fake.release_due(i64::MAX);
    assert_eq!(sent(&h), 0);
}

#[tokio::test]
async fn deleting_a_held_message_the_server_cannot_hear_keeps_it() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    h.fake.with(|s| s.submission_get = false);
    let err = queue(&h).drop_one(id).await.unwrap_err();
    assert!(err.to_string().contains("The server still holds this message and could not be asked to let it go."), "{err}");
    assert!(row(&h, id).await.is_some());
}

/// Makes every write that gives a row a server's hold fail, as a full
/// disk or a crash right after the hold would.
async fn refuse_held_writes(h: &Harness) {
    h.db.write(|c| {
        c.execute_batch(
            "CREATE TRIGGER no_holds BEFORE UPDATE ON outbox \
             WHEN NEW.held IS NOT NULL AND NEW.held NOT LIKE 'unconfirmed:%' \
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )?;
        Ok(())
    })
    .await
    .unwrap();
}

async fn allow_held_writes(h: &Harness) {
    h.db.write(|c| Ok(c.execute_batch("DROP TRIGGER no_holds;")?)).await.unwrap();
}

#[tokio::test]
async fn a_hold_whose_row_could_not_be_written_is_still_found() {
    let h = holding().await;
    refuse_held_writes(&h).await;
    assert!(queue(&h).schedule(later(&h, now_millis() + HOUR)).await.is_err());
    allow_held_writes(&h).await;
    let rows = h.db.read(outbox::list).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].held.as_deref().is_some_and(|held| held.starts_with("unconfirmed:")), "{rows:?}");
    h.fake.release_due(i64::MAX);
    queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    assert!(h.db.read(outbox::list).await.unwrap().is_empty());
    assert_eq!(sent(&h), 1, "one send in total");
}

#[tokio::test]
async fn a_new_hour_whose_row_could_not_be_written_keeps_the_new_hour() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    let at = now_millis() + 3 * HOUR;
    refuse_held_writes(&h).await;
    assert!(queue(&h).reschedule(id, at).await.is_err());
    allow_held_writes(&h).await;
    let kept = row(&h, id).await.unwrap();
    assert_eq!(kept.send_at, at);
    assert!(kept.held.as_deref().is_some_and(|held| held.starts_with("unconfirmed:")), "{kept:?}");
    // Before the new hour nothing goes; the server sends it at that hour.
    queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    assert_eq!(sent(&h), 0);
    h.fake.release_due(i64::MAX);
    queue(&h).send_due(at + MINUTE).await.unwrap();
    queue(&h).send_due(at + MINUTE).await.unwrap();
    assert_eq!(sent(&h), 1, "one send in total");
}

#[tokio::test]
async fn cancel_send_puts_the_bcc_back_in_the_draft() {
    let h = holding().await;
    let id = put(&h, blind(&h, now_millis() + HOUR)).await;
    let kept = row(&h, id).await.unwrap();
    let target = Target::thread(h.account_id, kept.thread_id.clone().unwrap());
    assert_eq!(queue(&h).cancel_scheduled(&[target]).await.unwrap().in_drafts, 1);
    let drafts = h.fake.with(|s| {
        let box_id = s.role_id("drafts").unwrap();
        s.emails.values().filter(|e| e.mailbox_ids.contains(&box_id)).map(|e| String::from_utf8_lossy(&e.raw).into_owned()).collect::<Vec<_>>()
    });
    assert_eq!(drafts.len(), 1, "one draft, not the held copy beside it: {drafts:?}");
    assert!(drafts[0].contains("Bcc: grace@example.com"), "{}", drafts[0]);
}

#[tokio::test]
async fn a_hold_canceled_and_deleted_elsewhere_is_told_at_the_hour() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    let email = row(&h, id).await.unwrap().message_id.unwrap();
    h.fake.with(|s| {
        s.holds.values_mut().for_each(|hold| hold.status = "canceled".into());
        s.emails.remove(&email);
    });
    let drained = queue(&h).send_due(now_millis() + 2 * HOUR).await.unwrap();
    assert_eq!(drained.stuck.len(), 1);
    let kept = row(&h, id).await.unwrap();
    assert_eq!(kept.problem.as_deref(), Some("This message was canceled in another app and is no longer in Drafts."));
    assert_eq!(kept.held, None);
    assert_eq!(sent(&h), 0);
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

#[tokio::test]
async fn a_hold_the_server_refuses_to_report_on_stays_held_and_deleting_waits_for_it() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    // The server stops answering for its submissions, as a refusal that
    // is not about the network does.
    h.fake.with(|s| s.submission_get = false);
    let outbox = queue(&h);
    let drained = outbox.send_due(now_millis() + 2 * HOUR).await.unwrap();
    assert!(drained.stuck.is_empty(), "no Outbox row while the hold is alive");
    let kept = row(&h, id).await.unwrap();
    assert!(kept.held.is_some() && kept.problem.is_none(), "{kept:?}");
    let looks = asked(&h);
    outbox.send_due(now_millis() + 2 * HOUR + 2 * MINUTE).await.unwrap();
    assert_eq!(asked(&h), looks + 1, "the hold is still asked about");
    assert!(queue(&h).drop_one(id).await.is_err(), "Delete refuses while the server cannot be asked");
    assert!(row(&h, id).await.is_some());
    h.fake.with(|s| s.submission_get = true);
    queue(&h).drop_one(id).await.unwrap();
    assert!(row(&h, id).await.is_none());
    h.fake.release_due(i64::MAX);
    assert_eq!(sent(&h), 0, "nothing goes after the delete");
}

#[tokio::test]
async fn cancel_after_a_new_hour_whose_hold_was_never_found_keeps_the_bcc() {
    let h = holding().await;
    let id = put(&h, blind(&h, now_millis() + HOUR)).await;
    // The new hold's answer is lost and the server never kept it.
    h.fake.with(|s| s.hold_failure = Some(JmapError::Network("connection reset".into())));
    let moved = queue(&h).reschedule(id, now_millis() + 2 * HOUR).await.unwrap().unwrap();
    assert!(moved.held.as_deref().is_some_and(|held| held.starts_with("unconfirmed:")) && moved.draft_id.is_some());
    // Old enough that an empty look counts.
    h.db.write(move |c| {
        let mut row = outbox::find(c, id)?.unwrap();
        let mark = row.held.take().unwrap();
        let id_part = mark.splitn(3, ':').nth(2).unwrap().to_string();
        row.held = Some(format!("unconfirmed:0:{id_part}"));
        outbox::put(c, &row)?;
        Ok(())
    })
    .await
    .unwrap();
    let target = Target::thread(h.account_id, moved.thread_id.clone().unwrap());
    assert_eq!(queue(&h).cancel_scheduled(&[target]).await.unwrap().in_drafts, 1);
    let drafts = h.fake.with(|s| {
        let box_id = s.role_id("drafts").unwrap();
        s.emails.values().filter(|e| e.mailbox_ids.contains(&box_id)).map(|e| String::from_utf8_lossy(&e.raw).into_owned()).collect::<Vec<_>>()
    });
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    assert!(drafts[0].contains("Bcc: grace@example.com"), "{}", drafts[0]);
}

#[tokio::test]
async fn a_lost_hold_days_away_is_not_looked_for_every_minute() {
    let h = holding().await;
    h.fake.with(|s| s.hold_failure = Some(JmapError::Network("connection reset".into())));
    let at = now_millis() + 20 * HOUR;
    scheduled(&h, at).await;
    let queries = || h.fake.with(|s| s.calls.iter().filter(|c| *c == "Email/query").count());
    let outbox = queue(&h);
    outbox.send_due(now_millis() + 2 * MINUTE).await.unwrap();
    let after_first = queries();
    outbox.send_due(now_millis() + 4 * MINUTE).await.unwrap();
    outbox.send_due(now_millis() + HOUR).await.unwrap();
    assert_eq!(queries(), after_first, "no look until the hour comes near");
    outbox.send_due(at).await.unwrap();
    assert_eq!(sent(&h), 1, "looked for once more at the hour, then sent from here");
}

#[tokio::test]
async fn a_row_asking_to_check_sent_can_be_dismissed_in_any_language() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + 2 * MINUTE).await;
    h.fake.with(|s| s.holds.clear());
    queue(&h).send_due(now_millis() + HOUR).await.unwrap();
    // Written while the app ran in Portuguese.
    h.db.write(move |c| {
        let mut row = outbox::find(c, id)?.unwrap();
        row.problem = Some("O Penguin Mail não conseguiu confirmar se esta mensagem foi enviada. Verifique as Enviadas.".into());
        outbox::put(c, &row)?;
        Ok(())
    })
    .await
    .unwrap();
    h.fake.with(|s| s.submission_get = false);
    queue(&h).drop_one(id).await.unwrap();
    assert!(row(&h, id).await.is_none());
}

/// What a composer opened on the held copy hands the outbox: the copy's id
/// as its draft, and the person's new words.
fn from_copy(h: &Harness, copy: &str, at: EpochMillis) -> Queued {
    Queued {
        draft_id: Some(copy.into()),
        raw: Some(
            b"From: me@example.com\r\nTo: ada@example.com\r\nSubject: Trip\r\nMessage-ID: <edit@example.com>\r\n\r\nChanged\r\n"
                .to_vec(),
        ),
        ..later(h, at)
    }
}

fn changed() -> Vec<u8> {
    b"From: me@example.com\r\nTo: ada@example.com\r\nSubject: Trip\r\nMessage-ID: <edit@example.com>\r\n\r\nChanged\r\n".to_vec()
}

fn sent_text(h: &Harness) -> Vec<String> {
    h.fake.with(|s| s.sent.iter().map(|sent| String::from_utf8_lossy(&sent.raw).into_owned()).collect())
}

async fn copy_of(h: &Harness, id: i64) -> String {
    row(h, id).await.unwrap().draft_id.unwrap()
}

#[tokio::test]
async fn send_from_a_held_copy_sends_once_in_all() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    let copy = copy_of(&h, id).await;
    let posted = queue(&h).post(from_copy(&h, &copy, now_millis())).await.unwrap();
    assert!(matches!(posted, Posted::Sent(_)), "{posted:?}");
    h.fake.release_due(i64::MAX);
    assert_eq!(sent(&h), 1, "the hold was called back before the send: {:?}", sent_text(&h));
    assert!(sent_text(&h)[0].contains("Changed"));
    assert!(row(&h, id).await.is_none());
}

#[tokio::test]
async fn scheduling_a_held_copy_again_sends_once_in_all() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    let copy = copy_of(&h, id).await;
    let at = now_millis() + 2 * HOUR;
    let posted = queue(&h).schedule(from_copy(&h, &copy, at)).await.unwrap();
    assert_eq!(posted, Posted::Waiting(id), "the row that held it takes the new hour");
    assert_eq!(pending(&h), 1, "one hold, at the new hour");
    let kept = row(&h, id).await.unwrap();
    assert_eq!(kept.send_at, at);
    h.fake.release_due(i64::MAX);
    queue(&h).send_due(at + MINUTE).await.unwrap();
    assert_eq!(sent(&h), 1, "{:?}", sent_text(&h));
    assert!(sent_text(&h)[0].contains("Changed"));
    let drafts = h.fake.with(|s| {
        let box_id = s.role_id("drafts").unwrap();
        s.emails.values().filter(|e| e.mailbox_ids.contains(&box_id) && !e.keywords.contains(mailrs_jmap::HELD)).count()
    });
    assert_eq!(drafts, 0, "no stray draft beside the new hold");
}

#[tokio::test]
async fn an_edit_of_a_held_copy_sends_once_in_all() {
    let h = holding().await;
    let at = now_millis() + HOUR;
    let id = scheduled(&h, at).await;
    let outbox = queue(&h);
    let Reopened::Draft(open) = outbox.reopen_held(id).await.unwrap() else {
        panic!("the server let go of it");
    };
    assert_eq!(pending(&h), 0, "opening it called the hold back");
    assert_eq!((open.held.as_deref(), open.send_at), (None, at));
    // The composer saves the edit over the draft it opened.
    let old = open.draft_id.clone();
    let saved = h.sync.save_draft(changed(), open.thread_id.clone(), old.clone()).await.unwrap();
    outbox.draft_saved(h.account_id, old, saved).await.unwrap();
    h.fake.release_due(i64::MAX);
    let drained = queue(&h).send_due(at + MINUTE).await.unwrap();
    assert_eq!(drained.sent.len(), 1);
    assert_eq!(sent(&h), 1, "{:?}", sent_text(&h));
    assert!(sent_text(&h)[0].contains("Changed"));
    assert!(row(&h, id).await.is_none());
}

#[tokio::test]
async fn opening_a_held_copy_puts_the_whole_message_back_in_drafts() {
    let h = holding().await;
    let id = put(&h, blind(&h, now_millis() + HOUR)).await;
    let Reopened::Draft(open) = queue(&h).reopen_held(id).await.unwrap() else {
        panic!("the server let go of it");
    };
    let draft = open.draft_id.clone().unwrap();
    let text = h.fake.with(|s| String::from_utf8_lossy(&s.emails[&draft].raw).into_owned());
    assert!(text.contains("Bcc: grace@example.com"), "{text}");
    assert!(open.raw.is_none(), "the draft holds the bytes now, so an edit is what goes");
}

#[tokio::test]
async fn opening_a_held_copy_the_server_cannot_hear_leaves_it_held() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    h.fake.with(|s| s.submission_get = false);
    match queue(&h).reopen_held(id).await.unwrap() {
        Reopened::Locked { can_cancel, .. } => assert!(can_cancel),
        other => panic!("expected Locked, got {other:?}"),
    }
    assert_eq!(pending(&h), 1);
    assert!(server_holds(&row(&h, id).await.unwrap()));
}

#[tokio::test]
async fn opening_a_held_copy_the_server_sent_says_so() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    h.fake.release_due(i64::MAX);
    assert!(matches!(queue(&h).reopen_held(id).await.unwrap(), Reopened::AlreadySent));
    assert_eq!(sent(&h), 1);
}

#[tokio::test]
async fn saving_a_draft_over_a_held_copy_is_refused() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + HOUR).await;
    let copy = copy_of(&h, id).await;
    assert!(h.sync.save_draft(changed(), None, Some(copy.clone())).await.is_err());
    assert!(h.fake.with(|s| s.emails.contains_key(&copy)), "the held copy stays");
    h.fake.release_due(i64::MAX);
    assert_eq!(sent(&h), 1);
}

/// How many threads the Drafts list of `h` shows.
async fn drafts_listed(h: &Harness) -> i64 {
    let account = h.account_id;
    h.db.read(move |c| {
        mailrs_store::threads::count_threads(c, &mailrs_store::threads::ThreadFilter::account(account, MailSet::Role(Role::Drafts)))
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn a_held_copy_is_no_draft_on_any_install() {
    let h = holding().await;
    put(&h, blind(&h, now_millis() + HOUR)).await;
    let other = beside(&h).await;
    other.bootstrap_all().await;
    assert_eq!(drafts_listed(&other).await, 0, "the copy the server holds is not a draft here");
    h.bootstrap_all().await;
    assert_eq!(drafts_listed(&h).await, 0, "nor on the install that held it");
}

#[tokio::test]
async fn a_held_copy_from_another_install_shows_in_send_later_and_cancel_send_works() {
    let h = holding().await;
    let at = now_millis() + HOUR;
    put(&h, blind(&h, at)).await;
    let other = beside(&h).await;
    other.bootstrap_all().await;
    let mine = queue(&other);
    mine.send_due(now_millis()).await.unwrap();
    let rows = other.db.read(outbox::scheduled).await.unwrap();
    assert_eq!(rows.len(), 1, "the server's hold shows in Send Later here");
    let row = &rows[0];
    assert!(server_holds(row), "as one the server sends");
    assert_eq!(row.send_at / 1000, at / 1000, "at the hour the server keeps");
    assert_eq!((row.subject.as_str(), row.recipients.as_str()), ("Trip", "ada@example.com"));
    let bytes = String::from_utf8_lossy(row.raw.as_deref().unwrap()).into_owned();
    assert!(bytes.contains("Bcc: grace@example.com"), "the Bcc comes back from the envelope: {bytes}");
    assert!(other.fake.with(|s| s.calls.iter().any(|c| c == "EmailSubmission/query")), "found by its copy");

    let target = Target::thread(other.account_id, row.thread_id.clone().unwrap());
    let cancelled = mine.cancel_scheduled(&[target]).await.unwrap();
    assert_eq!(cancelled.in_drafts, 1);
    assert_eq!(pending(&h), 0, "the server will not send it");
    h.fake.release_due(i64::MAX);
    assert_eq!(sent(&h), 0);
    other.look().await;
    assert_eq!(drafts_listed(&other).await, 1, "it is a draft again, Bcc and all");
}

#[tokio::test]
async fn a_held_copy_from_another_install_is_filed_here_once_sent() {
    let h = holding().await;
    let at = now_millis() + HOUR;
    let id = scheduled(&h, at).await;
    let email = row(&h, id).await.unwrap().message_id.unwrap();
    let other = beside(&h).await;
    other.bootstrap_all().await;
    queue(&other).send_due(now_millis()).await.unwrap();
    h.fake.release_due(i64::MAX);
    let drained = queue(&other).send_due(at + MINUTE).await.unwrap();
    assert_eq!(drained.sent.len(), 1);
    assert!(h.fake.with(|s| s.emails[&email].mailbox_ids.contains(&s.role_id("sent").unwrap())), "filed in Sent");
    assert_eq!(sent(&h), 1);
}

#[tokio::test]
async fn the_install_that_held_it_does_not_take_its_own_copy_twice() {
    let h = holding().await;
    scheduled(&h, now_millis() + HOUR).await;
    h.bootstrap_all().await;
    queue(&h).send_due(now_millis()).await.unwrap();
    assert_eq!(h.db.read(outbox::scheduled).await.unwrap().len(), 1);
}

#[tokio::test]
async fn removing_an_account_calls_its_holds_back_to_drafts() {
    let h = holding().await;
    put(&h, blind(&h, now_millis() + HOUR)).await;
    scheduled(&h, now_millis() + 2 * HOUR).await;
    let outbox = queue(&h);
    assert_eq!(outbox.held_count(h.account_id).await.unwrap(), 2, "the dialog can say how many");
    assert_eq!(outbox.call_back_account(h.account_id).await.unwrap(), 0, "every hold came back");
    assert_eq!(pending(&h), 0);
    h.fake.release_due(i64::MAX);
    assert_eq!(sent(&h), 0, "nothing goes out once the account is gone");
    let drafts = h.fake.with(|s| {
        let box_id = s.role_id("drafts").unwrap();
        s.emails.values().filter(|e| e.mailbox_ids.contains(&box_id) && !e.keywords.contains(mailrs_jmap::HELD)).count()
    });
    assert_eq!(drafts, 2, "both messages are drafts on the server again");
}

#[tokio::test]
async fn removing_an_account_whose_server_cannot_hear_counts_what_may_still_go() {
    let h = holding().await;
    scheduled(&h, now_millis() + HOUR).await;
    h.fake.with(|s| s.submission_get = false);
    assert_eq!(queue(&h).call_back_account(h.account_id).await.unwrap(), 1, "the server may still send it");
    assert_eq!(pending(&h), 1);
}

#[tokio::test]
async fn a_hold_canceled_elsewhere_leaves_send_later_before_its_hour() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + 3 * HOUR).await;
    let email = row(&h, id).await.unwrap().message_id.unwrap();
    // Another mail program calls it back, and the copy is a draft again.
    h.fake.with(|s| {
        s.holds.values_mut().for_each(|hold| hold.status = "canceled".into());
        s.emails.get_mut(&email).unwrap().keywords.remove(mailrs_jmap::HELD);
    });
    let drained = queue(&h).send_due(now_millis()).await.unwrap();
    assert!(drained.changed);
    assert!(row(&h, id).await.is_none(), "gone from Send Later hours before its hour");
    assert_eq!(sent(&h), 0);
}

#[tokio::test]
async fn a_hold_hours_away_is_asked_about_at_most_hourly() {
    let h = holding().await;
    scheduled(&h, now_millis() + 5 * HOUR).await;
    let outbox = queue(&h);
    let now = now_millis();
    outbox.send_due(now).await.unwrap();
    let looks = asked(&h);
    outbox.send_due(now + 30 * MINUTE).await.unwrap();
    assert_eq!(asked(&h), looks, "not again within the hour");
    outbox.send_due(now + HOUR + MINUTE).await.unwrap();
    assert_eq!(asked(&h), looks + 1, "once the hour has passed");
}

#[tokio::test]
async fn a_hold_another_install_sent_now_reads_as_sent_here() {
    let h = holding().await;
    let id = scheduled(&h, now_millis() + 3 * HOUR).await;
    let other = beside(&h).await;
    other.bootstrap_all().await;
    queue(&other).send_due(now_millis()).await.unwrap();
    let theirs = other.db.read(outbox::scheduled).await.unwrap().remove(0);
    assert!(matches!(queue(&other).send_one(theirs.id).await.unwrap(), Posted::Sent(_)));
    h.look().await;
    let drained = queue(&h).send_due(now_millis()).await.unwrap();
    assert!(row(&h, id).await.is_none(), "it leaves Send Later here");
    assert!(drained.stuck.is_empty(), "not an Outbox row saying it was canceled: {:?}", drained.stuck);
    assert_eq!(drained.sent.len(), 1, "it reads as sent");
    assert_eq!(sent(&h), 1);
}
