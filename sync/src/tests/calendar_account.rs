//! An account with calendars and no mail: what its services answer and
//! offer.

use std::sync::Arc;

use mailrs_dav::Kind;
use mailrs_dav::fake::FakeDav;

use crate::services::{CalDav, NoMail};
use crate::{
    AccountServices, AnyCalendar, AnyDav, AnyMail, BackendError, IdentityService, MailBackend, SearchQuery, Withheld,
};

pub(crate) const PERSONAL: &str = "/cal/personal/";

/// A server with one calendar, and the services of a calendar account on
/// it.
pub(crate) fn calendar_only() -> (Arc<FakeDav>, AccountServices) {
    let dav = Arc::new(FakeDav::new());
    dav.add_collection(PERSONAL, Kind::Calendar, "Personal", Some("#0082c9"));
    let api = Arc::new(AnyDav::from(Arc::clone(&dav)));
    let calendar = CalDav::new(api, AnyMail::NoMail(NoMail), Vec::new());
    (dav, AccountServices::calendar_only(AnyCalendar::Dav(calendar)))
}

#[tokio::test]
async fn no_mail_answers_unsupported_and_offers_nothing() {
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
}

#[tokio::test]
async fn calendar_only_services_offer_a_calendar_and_no_mail() {
    let (_, services) = calendar_only();
    assert!(!services.serves_mail());
    let offers = services.offers();
    assert!(offers.calendar);
    assert!(!offers.mail);
    assert!(!offers.contacts && !offers.rules && !offers.auto_reply && !offers.search);
    assert!(offers.missing().is_empty(), "nothing is said to be missing of an account that was never a mail account");
    assert_eq!(services.withheld(), Withheld::NONE);
    assert!(services.identities.identities().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_mail_account_still_serves_mail() {
    let services = AccountServices::google(Arc::new(crate::fake::FakeGmail::new()));
    assert!(services.serves_mail());
    assert!(services.offers().mail);
    assert!(services.offers().tells_organizer);
}
