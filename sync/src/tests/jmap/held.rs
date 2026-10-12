//! Send Later on a JMAP server that holds mail, through the adapter.

use mailrs_domain::EpochMillis;
use mailrs_jmap::JmapError;

use super::{Harness, over};
use crate::fake::FakeJmap;
use crate::{BackendError, Held, MailBackend, Settled, Unheld, now_millis};

const HOUR: EpochMillis = 3_600_000;

fn letter() -> Vec<u8> {
    b"From: me@example.com\r\nTo: ada@example.com\r\nBcc: secret@example.com\r\nSubject: Trip\r\nMessage-ID: <trip@example.com>\r\n\r\nSee you\r\n".to_vec()
}

async fn holding() -> Harness {
    over(FakeJmap::new().with_delayed_send(86_400)).await
}

/// Calls the hold back as the outbox does, with its row's copy and hour.
async fn back(h: &Harness, held: &Held, at: EpochMillis) -> Result<Unheld, BackendError> {
    h.mail().unschedule(&held.handle, &held.message_id, at).await
}

/// Asks where the hold stands, as the outbox does.
async fn settled(h: &Harness, held: &Held, at: EpochMillis) -> Result<Settled, BackendError> {
    h.mail().settle(&held.handle, &held.message_id, at).await
}

/// The server sends the hold and then forgets its submission.
fn send_and_forget(h: &Harness, held: &Held) {
    h.fake.release_due(i64::MAX);
    h.fake.with(|s| s.holds.remove(&held.handle));
}

/// What the code logs at warning level while `run` runs, on this thread.
async fn warnings<F: std::future::Future>(run: F) -> String {
    #[derive(Clone, Default)]
    struct Log(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let log = Log::default();
    let writer = log.clone();
    let subscriber =
        tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).with_ansi(false).with_writer(move || writer.clone()).finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    run.await;
    String::from_utf8(log.0.lock().unwrap().clone()).unwrap()
}

#[tokio::test]
async fn a_hold_waits_in_drafts_with_its_keyword() {
    let h = holding().await;
    let held = h.mail().schedule(&letter(), now_millis() + HOUR).await.unwrap();
    let (mailboxes, keywords) = h.fake.with(|s| {
        let email = &s.emails[&held.message_id];
        (email.mailbox_ids.clone(), email.keywords.clone())
    });
    assert!(mailboxes.contains(&h.fake.mailbox("drafts")));
    assert!(keywords.contains(mailrs_jmap::HELD) && keywords.contains("$draft"));
    assert_eq!(h.fake.with(|s| s.holds[&held.handle].rcpt_to.clone()), ["ada@example.com", "secret@example.com"]);
    let raw = h.fake.with(|s| s.emails[&held.message_id].raw.clone());
    assert!(!String::from_utf8_lossy(&raw).contains("Bcc:"), "Bcc goes only in the envelope");
}

#[tokio::test]
async fn a_delay_past_the_servers_reach_is_left_to_the_app() {
    let h = over(FakeJmap::new().with_delayed_send(60)).await;
    assert!(matches!(h.mail().schedule(&letter(), now_millis() + HOUR).await, Err(BackendError::Unsupported)));
    assert!(h.fake.with(|s| s.holds.is_empty()));
}

#[tokio::test]
async fn a_server_that_holds_nothing_is_left_to_the_app() {
    let h = over(FakeJmap::new()).await;
    assert!(matches!(h.mail().schedule(&letter(), now_millis() + HOUR).await, Err(BackendError::Unsupported)));
}

#[tokio::test]
async fn a_server_without_submission_get_keeps_the_local_queue() {
    let h = over(FakeJmap::new().with_delayed_send(86_400).without_submission_get()).await;
    assert!(matches!(h.mail().schedule(&letter(), now_millis() + HOUR).await, Err(BackendError::Unsupported)));
    assert!(h.fake.with(|s| s.holds.is_empty()), "James could never call it back, so nothing is held");
}

#[tokio::test]
async fn the_probe_runs_once_a_run() {
    let h = holding().await;
    h.mail().schedule(&letter(), now_millis() + HOUR).await.unwrap();
    h.mail().schedule(&letter(), now_millis() + 2 * HOUR).await.unwrap();
    let probes = h.fake.with(|s| s.calls.iter().filter(|c| *c == "EmailSubmission/get ids=[]").count());
    assert_eq!(probes, 1);
    assert_eq!(h.fake.with(|s| s.holds.len()), 2);
}

#[tokio::test]
async fn a_refused_hold_says_so_and_keeps_nothing() {
    let h = holding().await;
    h.fake.with(|s| s.hold_failure = Some(JmapError::Method { kind: "forbiddenFrom".into(), description: "no ada@example.com".into() }));
    match h.mail().schedule(&letter(), now_millis() + HOUR).await {
        Err(BackendError::Refused(said)) => assert!(!said.contains("ada@example.com"), "no server text: {said}"),
        other => panic!("expected Refused, got {other:?}"),
    }
    assert!(h.fake.with(|s| s.holds.is_empty()));
}

#[tokio::test]
async fn a_lost_answer_reads_as_offline_and_the_message_goes_once() {
    let h = holding().await;
    h.fake.with(|s| s.hold_failure = Some(JmapError::HoldUnknown));
    assert!(matches!(h.mail().schedule(&letter(), now_millis() + HOUR).await, Err(BackendError::Offline(_))));
    assert_eq!(h.fake.with(|s| s.holds.len()), 1, "the server took it, and the adapter tried once");
    h.fake.release_due(i64::MAX);
    assert_eq!(h.fake.with(|s| s.sent.len()), 1);
}

#[tokio::test]
async fn a_hold_goes_at_the_instant_asked_whatever_the_zone() {
    let h = holding().await;
    // An hour and a half second from now as a clock in Kolkata reads it;
    // the server keeps UTC. The half second catches a wait cut short.
    let kolkata = chrono::FixedOffset::east_opt(5 * 3600 + 1800).unwrap();
    let local = chrono::DateTime::from_timestamp_millis(now_millis() + HOUR + 500).unwrap().with_timezone(&kolkata);
    let at = chrono::DateTime::parse_from_rfc3339(&local.to_rfc3339()).unwrap().timestamp_millis();
    let held = h.mail().schedule(&letter(), at).await.unwrap();
    let send_at = h.fake.with(|s| s.holds[&held.handle].send_at);
    assert!(send_at >= at && send_at - at < 2_000, "never early, at most a second late: {}", send_at - at);
    match settled(&h, &held, at).await.unwrap() {
        Settled::Waiting(until) => assert!((until - at).abs() < 2_000, "off by {}", until - at),
        other => panic!("expected Waiting, got {other:?}"),
    }
}

#[tokio::test]
async fn calling_back_in_time_leaves_a_draft() {
    let h = holding().await;
    let at = now_millis() + HOUR;
    let held = h.mail().schedule(&letter(), at).await.unwrap();
    assert_eq!(back(&h, &held, at).await.unwrap(), Unheld::InDrafts);
    let keywords = h.fake.with(|s| s.emails[&held.message_id].keywords.clone());
    assert!(!keywords.contains(mailrs_jmap::HELD) && keywords.contains("$draft"));
    assert_eq!(h.fake.with(|s| s.holds[&held.handle].status.clone()), "canceled");
    assert_eq!(h.fake.release_due(i64::MAX), 0);
    assert!(h.fake.with(|s| s.sent.is_empty()), "a called-back hold never goes");
}

#[tokio::test]
async fn calling_back_too_late_files_it_in_sent() {
    let h = holding().await;
    let at = now_millis() + HOUR;
    let held = h.mail().schedule(&letter(), at).await.unwrap();
    h.fake.release_due(i64::MAX);
    assert_eq!(back(&h, &held, at).await.unwrap(), Unheld::AlreadySent);
    assert!(h.fake.with(|s| s.emails[&held.message_id].mailbox_ids.contains(&s.role_id("sent").unwrap())));
    assert_eq!(h.fake.with(|s| s.sent.len()), 1);
}

#[tokio::test]
async fn a_hold_settles_into_sent_once_the_server_sends_it() {
    let h = holding().await;
    let at = now_millis() + HOUR;
    let held = h.mail().schedule(&letter(), at).await.unwrap();
    match settled(&h, &held, at).await.unwrap() {
        Settled::Waiting(until) => assert!((until - at).abs() < 5_000, "the server's own hour"),
        other => panic!("expected Waiting, got {other:?}"),
    }
    h.fake.release_due(i64::MAX);
    assert_eq!(settled(&h, &held, at).await.unwrap(), Settled::Sent);
    let (in_sent, keywords) = h.fake.with(|s| {
        let email = &s.emails[&held.message_id];
        (email.mailbox_ids.contains(&s.role_id("sent").unwrap()), email.keywords.clone())
    });
    assert!(in_sent && !keywords.contains("$draft") && !keywords.contains(mailrs_jmap::HELD));
}

#[tokio::test]
async fn a_hold_called_back_elsewhere_settles_as_a_draft() {
    let h = holding().await;
    let at = now_millis() + HOUR;
    let held = h.mail().schedule(&letter(), at).await.unwrap();
    h.fake.with(|s| s.holds.get_mut(&held.handle).unwrap().status = "canceled".into());
    assert_eq!(settled(&h, &held, at).await.unwrap(), Settled::Canceled);
    assert!(!h.fake.with(|s| s.emails[&held.message_id].keywords.contains(mailrs_jmap::HELD)));
}

#[tokio::test]
async fn a_hold_sent_on_a_server_without_sent_stops_reading_as_a_draft() {
    let h = holding().await;
    h.fake.with(|s| s.mailboxes.retain(|_, m| m.role.as_deref() != Some("sent") && m.role.as_deref() != Some("archive")));
    let at = now_millis() + HOUR;
    let held = h.mail().schedule(&letter(), at).await.unwrap();
    h.fake.release_due(i64::MAX);
    assert_eq!(settled(&h, &held, at).await.unwrap(), Settled::Sent);
    let (mailboxes, keywords) = h.fake.with(|s| {
        let email = &s.emails[&held.message_id];
        (email.mailbox_ids.clone(), email.keywords.clone())
    });
    assert!(mailboxes.contains(&h.fake.mailbox("drafts")), "it stays where it waited");
    assert!(!keywords.contains("$draft") && !keywords.contains(mailrs_jmap::HELD), "nobody sends it again as a draft");
}

#[tokio::test]
async fn a_hold_sent_and_forgotten_settles_as_sent_and_is_filed() {
    let h = holding().await;
    let at = now_millis() + HOUR;
    let held = h.mail().schedule(&letter(), at).await.unwrap();
    send_and_forget(&h, &held);
    assert_eq!(settled(&h, &held, now_millis() - HOUR).await.unwrap(), Settled::Unknown, "sending leaves the copy as it was");
    h.fake.with(|s| s.emails.get_mut(&held.message_id).unwrap().keywords.remove("$draft"));
    assert_eq!(settled(&h, &held, at).await.unwrap(), Settled::Sent);
    assert!(h.fake.with(|s| s.emails[&held.message_id].mailbox_ids.contains(&s.role_id("sent").unwrap())));
}

#[tokio::test]
async fn a_hold_filed_in_sent_and_forgotten_is_called_back_as_already_sent() {
    let h = holding().await;
    let at = now_millis() + HOUR;
    let held = h.mail().schedule(&letter(), at).await.unwrap();
    h.fake.release_due(i64::MAX);
    assert_eq!(settled(&h, &held, at).await.unwrap(), Settled::Sent);
    h.fake.with(|s| s.holds.remove(&held.handle));
    assert_eq!(back(&h, &held, at).await.unwrap(), Unheld::AlreadySent);
}

#[tokio::test]
async fn a_hold_canceled_elsewhere_and_forgotten_before_its_hour_is_not_sent() {
    let h = holding().await;
    let at = now_millis() + HOUR;
    let held = h.mail().schedule(&letter(), at).await.unwrap();
    h.fake.with(|s| s.holds.remove(&held.handle));
    assert_eq!(settled(&h, &held, at).await.unwrap(), Settled::Canceled);
    assert!(!h.fake.with(|s| s.emails[&held.message_id].keywords.contains(mailrs_jmap::HELD)));
    assert_eq!(back(&h, &held, at).await.unwrap(), Unheld::InDrafts);
}

#[tokio::test]
async fn a_held_copy_past_its_hour_with_the_submission_gone_is_unknown() {
    let h = holding().await;
    let held = h.mail().schedule(&letter(), now_millis() + HOUR).await.unwrap();
    send_and_forget(&h, &held);
    let past = now_millis() - 1_000;
    assert_eq!(settled(&h, &held, past).await.unwrap(), Settled::Unknown);
    assert_eq!(back(&h, &held, past).await.unwrap(), Unheld::Unknown);
    assert!(h.fake.with(|s| s.emails[&held.message_id].keywords.contains(mailrs_jmap::HELD)), "nothing claims it either way");
}

#[tokio::test]
async fn a_held_copy_close_to_its_hour_is_unknown_in_case_the_clocks_differ() {
    let h = holding().await;
    let held = h.mail().schedule(&letter(), now_millis() + HOUR).await.unwrap();
    send_and_forget(&h, &held);
    assert_eq!(settled(&h, &held, now_millis() + 30_000).await.unwrap(), Settled::Unknown);
}

#[tokio::test]
async fn a_copy_gone_with_its_submission_is_unknown() {
    let h = holding().await;
    let at = now_millis() + HOUR;
    let held = h.mail().schedule(&letter(), at).await.unwrap();
    h.fake.with(|s| {
        s.holds.remove(&held.handle);
        s.emails.remove(&held.message_id);
    });
    assert_eq!(settled(&h, &held, at).await.unwrap(), Settled::Unknown);
    assert_eq!(back(&h, &held, at).await.unwrap(), Unheld::Unknown);
}

#[tokio::test]
async fn a_cancel_that_races_the_send_never_reads_as_not_sent() {
    let h = holding().await;
    let held = h.mail().schedule(&letter(), now_millis() + HOUR).await.unwrap();
    // The hour has come: the server sends it between the get and the cancel.
    let at = now_millis();
    h.fake.with(|s| s.cancel_races_send = true);
    assert_eq!(back(&h, &held, at).await.unwrap(), Unheld::Unknown);
    assert_eq!(h.fake.with(|s| s.sent.len()), 1);
}

#[tokio::test]
async fn a_hold_past_its_hour_and_still_pending_asks_again_later() {
    let h = holding().await;
    let held = h.mail().schedule(&letter(), now_millis() + HOUR).await.unwrap();
    h.fake.with(|s| s.holds.get_mut(&held.handle).unwrap().send_at = now_millis() - 1_000);
    let before = now_millis();
    match settled(&h, &held, before - 1_000).await.unwrap() {
        Settled::Waiting(until) => assert!(until >= before + 59_000 && until <= now_millis() + 60_000, "{}", until - before),
        other => panic!("expected Waiting, got {other:?}"),
    }
}

#[tokio::test]
async fn a_copy_deleted_elsewhere_still_counts_as_called_back() {
    let h = holding().await;
    let at = now_millis() + HOUR;
    let held = h.mail().schedule(&letter(), at).await.unwrap();
    h.fake.with(|s| s.emails.remove(&held.message_id));
    assert_eq!(back(&h, &held, at).await.unwrap(), Unheld::InDrafts);
    assert_eq!(settled(&h, &held, at).await.unwrap(), Settled::Canceled);
}

#[tokio::test]
async fn a_submission_state_nobody_knows_is_no_cancel() {
    let h = holding().await;
    let at = now_millis() + HOUR;
    let held = h.mail().schedule(&letter(), at).await.unwrap();
    for status in ["", "undone"] {
        h.fake.with(|s| s.holds.get_mut(&held.handle).unwrap().status = status.into());
        assert!(matches!(settled(&h, &held, at).await, Err(BackendError::Refused(_))), "{status:?}");
    }
    assert!(h.fake.with(|s| s.emails[&held.message_id].keywords.contains(mailrs_jmap::HELD)));
}

#[tokio::test]
async fn the_wait_is_counted_once_the_upload_is_done() {
    let h = holding().await;
    h.fake.with(|s| s.upload_delay = std::time::Duration::from_millis(1_500));
    let at = now_millis() + HOUR + 500;
    let held = h.mail().schedule(&letter(), at).await.unwrap();
    let late = h.fake.with(|s| s.holds[&held.handle].send_at) - at;
    assert!((0..1_200).contains(&late), "late by {late}");
}

#[tokio::test]
async fn a_sent_hold_with_no_drafts_mailbox_to_file_from_is_logged() {
    let h = holding().await;
    h.fake.with(|s| {
        s.mailboxes.retain(|_, m| m.role.as_deref() != Some("drafts"));
        s.holds.insert("S9".into(), crate::fake::FakeHold { email_id: "M9".into(), send_at: 0, status: "final".into(), rcpt_to: Vec::new() });
    });
    let logged = warnings(async {
        assert_eq!(h.mail().settle("S9", "M9", 0).await.unwrap(), Settled::Sent);
    })
    .await;
    assert!(logged.contains("no Drafts mailbox"), "{logged}");
    assert!(!logged.contains("M9") && !logged.contains("S9"), "{logged}");
}

#[tokio::test]
async fn a_hold_whose_answer_was_lost_is_found_by_its_message_id() {
    let h = holding().await;
    h.fake.with(|s| s.hold_failure = Some(JmapError::HoldUnknown));
    let at = now_millis() + HOUR;
    assert!(matches!(h.mail().schedule(&letter(), at).await, Err(BackendError::Offline(_))));
    let found = h.mail().find_hold("trip@example.com").await.unwrap().expect("the server kept it");
    let (handle, email) = h.fake.with(|s| s.holds.iter().map(|(id, hold)| (id.clone(), hold.email_id.clone())).next().unwrap());
    assert_eq!((found.handle, found.message_id), (handle, email));
    assert_eq!(h.mail().find_hold("other@example.com").await.unwrap(), None);
}

#[tokio::test]
async fn a_held_copy_without_a_submission_settles_nothing() {
    let h = holding().await;
    h.mail().schedule(&letter(), now_millis() + HOUR).await.unwrap();
    h.fake.with(|s| s.holds.clear());
    let err = h.mail().find_hold("trip@example.com").await.unwrap_err();
    assert!(!err.is_transient(), "{err:?}");
}

#[tokio::test]
async fn two_held_copies_of_one_message_settle_nothing() {
    let h = holding().await;
    h.mail().schedule(&letter(), now_millis() + HOUR).await.unwrap();
    h.mail().schedule(&letter(), now_millis() + 2 * HOUR).await.unwrap();
    let err = h.mail().find_hold("trip@example.com").await.unwrap_err();
    assert!(!err.is_transient(), "{err:?}");
}
