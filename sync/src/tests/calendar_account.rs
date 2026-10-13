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
