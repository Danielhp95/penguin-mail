//! The account's mail service in store ids, over the IMAP and Gmail fakes.

use std::sync::Arc;
use std::time::Duration;

use mailrs_domain::Location;
use mailrs_store::remote_refs;

use super::imap::{days_ago, message};
use super::{ImapHarness, fake_settings, harness, imap_harness_on};
use crate::fake::{FakeImap, meta};
use crate::{TriageAction, Want};

const STORE_ID: &str = "INBOX/1001/1";

/// An IMAP account whose one message the app archived: the store knows it
/// as `INBOX/1001/1`, the server as `Archive/1006/1`.
async fn moved() -> ImapHarness {
    let imap = FakeImap::new();
    imap.deliver_flagged("INBOX", &message("a", "Kites", ""), &[], days_ago(1));
    let h = imap_harness_on(imap, fake_settings()).await;
    h.bootstrap().await;
    let thread = h.thread_of(STORE_ID).await.unwrap();
    h.sync.triage_thread(&thread, &TriageAction::Archive).await.unwrap();
    assert_eq!(h.location(STORE_ID).await.as_deref(), Some("Archive/1006/1"));
    h
}

#[tokio::test]
async fn a_store_id_goes_to_the_server_as_the_name_it_has_now() {
    let h = moved().await;
    let raws = h.sync.mail().fetch_raw(&[STORE_ID.to_string()]).await.unwrap();
    assert_eq!(raws[0].id, STORE_ID, "answered under the store's id");
    assert!(String::from_utf8_lossy(&raws[0].bytes).contains("Subject: Kites"));
    let part = h.sync.mail().fetch_structure(STORE_ID).await.unwrap();
    assert_eq!(part.header("Subject"), Some("Kites"));
}

#[tokio::test]
async fn the_server_answers_in_store_ids() {
    let h = moved().await;
    let mail = h.sync.mail();
    let found = mail.fetch(vec![Want::message(STORE_ID)]).await.unwrap();
    let ids: Vec<&str> = found.metas.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, [STORE_ID]);
    let listed = mail.window_ids(30, Some("Archive")).await.unwrap();
    let ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, [STORE_ID]);
}

#[tokio::test]
async fn a_look_waits_for_a_move_in_flight() {
    let h = moved().await;
    let moves = h.sync.mail().hold_moves().await;
    let sync = Arc::clone(&h.sync);
    let look = tokio::spawn(async move { sync.incremental().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!look.is_finished(), "the look waits while mail is moving");
    drop(moves);
    look.await.unwrap().unwrap();
}

/// Gmail's names are the store's ids, so nothing reads a ref: one planted
/// here would send the fetch to a message Gmail does not have.
#[tokio::test]
async fn a_label_server_reads_no_refs_and_never_waits() {
    let h = harness().await;
    h.fake.seed(meta("m1", "t1", crate::now_millis(), &["INBOX"]));
    h.bootstrap_all().await;
    let account_id = h.account_id;
    h.db.write(move |c| {
        let elsewhere = Location { mailbox: "Elsewhere".into(), uidvalidity: 1, uid: 9 };
        remote_refs::locate(c, account_id, "m1", &elsewhere)
    })
    .await
    .unwrap();

    let raws = h.sync.mail().fetch_raw(&["m1".to_string()]).await.unwrap();
    assert_eq!(raws[0].id, "m1");

    let _moves = h.sync.mail().hold_moves().await;
    let again = tokio::time::timeout(Duration::from_millis(10), h.sync.mail().hold_moves()).await;
    assert!(again.is_ok(), "nothing waits on a label server");
}
