//! An account with calendars and no mail: what its services answer and
//! offer. Signing one in is tested in `connect`, connecting and
//! forgetting one in `connector`, and keeping one in `engine`.

use std::sync::Arc;

use mailrs_dav::Kind;
use mailrs_dav::fake::FakeDav;

use crate::services::NoMail;
use crate::{AccountServices, BackendError, IdentityService, MailBackend, SearchQuery, Withheld};

pub(super) const PERSONAL: &str = "/cal/personal/";

/// A server with one calendar, and the services of a calendar account on
/// it.
pub(super) fn calendar_only() -> (Arc<FakeDav>, AccountServices) {
    let dav = Arc::new(FakeDav::new());
    dav.add_collection(PERSONAL, Kind::Calendar, "Personal", Some("#0082c9"));
    let services = AccountServices::calendar_only(Arc::clone(&dav));
    (dav, services)
}

#[tokio::test]
async fn no_mail_answers_unsupported_or_nothing_and_offers_nothing() {
    let mail = NoMail;
    assert!(matches!(mail.mailboxes().await, Err(BackendError::Unsupported)));
    assert!(matches!(mail.send(b"raw", None).await, Err(BackendError::Unsupported)));
    assert!(matches!(mail.search(&SearchQuery::Native("x".into()), 10).await, Err(BackendError::Unsupported)));
    let refused = mail.apply(&["m1".to_string()], &[]).await.unwrap_err();
    assert_eq!(refused.taken, 0);
    assert!(matches!(refused.error, BackendError::Unsupported));
    assert!(mail.identities().await.unwrap().is_empty());
    let caps = mail.capabilities();
    assert!(!caps.labels && !caps.delete_forever && !caps.local_mailboxes && !caps.native_search);
    assert_eq!(mail.poll_interval(), None);
    // What asks every account hears nothing, and no failure to log.
    assert_eq!(mail.storage().await.unwrap(), None);
    assert!(mail.unread_counts().await.unwrap().is_empty());
}

#[tokio::test]
async fn calendar_only_services_offer_a_calendar_and_no_mail() {
    let (_, services) = calendar_only();
    assert!(!services.serves_mail());
    let offers = services.offers();
    assert!(offers.calendar);
    assert!(!offers.contacts && !offers.rules && !offers.auto_reply && !offers.search);
    assert_eq!(services.withheld(), Withheld::NONE);
    assert!(services.identities.identities().await.unwrap().is_empty());
}

/// A calendar account removed and then added again, to the same server or
/// another, under the id SQLite gives back: the copy reads the new one at
/// once rather than waiting out the times it kept for the old one.
#[tokio::test]
async fn an_account_added_under_a_removed_ones_id_lists_its_calendars_at_once() {
    use mailrs_store::{Db, accounts};

    use crate::calendar_copy::CalendarCopy;
    use crate::{EngineConfig, SyncEngine};

    const NOW: i64 = 1_790_000_000_000;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("mail.db")).unwrap();
    let (engine, _events) = SyncEngine::new(db.clone(), EngineConfig::default());
    let engine = Arc::new(engine);
    let copy = CalendarCopy::new(Arc::clone(&engine), db.clone());
    let names = |id| {
        let db = db.clone();
        async move {
            let calendars = db.read(move |c| mailrs_store::calendar::calendars(c, id)).await.unwrap();
            calendars.into_iter().map(|calendar| calendar.name).collect::<Vec<_>>()
        }
    };

    let first = db.write(|c| accounts::insert_caldav_account(c, "robin@cloud.example.org", 0)).await.unwrap().unwrap();
    engine.start_account(first, calendar_only().1);
    copy.refresh_due(&[first], NOW, true).await.unwrap();
    assert_eq!(names(first).await, ["Personal"]);

    // Removing it, as the app does: the engine stops it, the copy forgets
    // it, and its row goes with its calendars.
    engine.stop_account(first);
    copy.forget(first);
    db.write(move |c| accounts::delete_account(c, first)).await.unwrap();

    let second = db.write(|c| accounts::insert_caldav_account(c, "dana@other.example.net", 0)).await.unwrap().unwrap();
    assert_eq!(second, first, "SQLite gives the id back");
    let dav = Arc::new(FakeDav::new());
    dav.add_collection("/cal/work/", Kind::Calendar, "Work", None);
    engine.start_account(second, AccountServices::calendar_only(dav));
    copy.refresh_due(&[second], NOW + 2 * 60_000, true).await.unwrap();
    assert_eq!(names(second).await, ["Work"]);
}
