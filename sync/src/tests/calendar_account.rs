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

mod engine {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use mailrs_domain::{Account, AccountId, AccountState, ChangeEvent, Provider};
    use mailrs_store::{Db, accounts, calendar as store};

    use super::{PERSONAL, calendar_only};
    use crate::calendar_copy::CalendarCopy;
    use crate::fake::meta;
    use crate::mailbox::{Loaded, Mailbox, Mailboxes, Scope, View};
    use crate::tests::{Connected, harness};
    use crate::{AccountSync, Accounts, EngineConfig, SyncEngine, now_millis};

    const NAME: &str = "robin@cloud.example.org";
    const NOW: i64 = 1_790_000_000_000;
    const LUNCH: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:lunch\r\n\
                         DTSTART:20261006T120000Z\r\nDTEND:20261006T130000Z\r\nSUMMARY:Lunch\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    struct Setup {
        engine: Arc<SyncEngine>,
        events: async_channel::Receiver<ChangeEvent>,
        db: Db,
        id: AccountId,
        _dir: tempfile::TempDir,
    }

    async fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("mail.db")).unwrap();
        let id = db.write(|c| accounts::insert_account(c, NAME, 0)).await.unwrap();
        let config = EngineConfig { max_backoff: Duration::from_millis(20), ..EngineConfig::default() };
        let (engine, events) = SyncEngine::new(db.clone(), config);
        Setup { engine: Arc::new(engine), events, db, id, _dir: dir }
    }

    async fn state(s: &Setup) -> AccountState {
        let id = s.id;
        s.db.read(move |c| accounts::account(c, id)).await.unwrap().expect("the account").state
    }

    /// The next state the engine reports for the account.
    async fn reported(s: &Setup) -> AccountState {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let ChangeEvent::AccountStateChanged { state, .. } = s.events.recv().await.expect("the engine's events") {
                    return state;
                }
            }
        })
        .await
        .expect("the engine reported a state")
    }

    #[tokio::test]
    async fn the_engine_keeps_a_calendar_account_and_reports_it_ok() {
        let s = setup().await;
        let (_, services) = calendar_only();
        s.engine.start_account(s.id, services);
        assert_eq!(reported(&s).await, AccountState::Ok, "no mail loop ran to fail on a server with no mail");
        assert_eq!(state(&s).await, AccountState::Ok);
        assert!(s.engine.is_running(s.id));
        let kept = Accounts::services(&*s.engine, s.id).expect("the account's services");
        assert!(!kept.serves_mail() && kept.calendar.is_some());
        s.engine.stop_account(s.id);
        assert!(!s.engine.is_running(s.id));
    }

    #[tokio::test]
    async fn the_copy_reads_a_calendar_account() {
        let s = setup().await;
        let (dav, services) = calendar_only();
        dav.put_resource(&format!("{PERSONAL}lunch.ics"), LUNCH);
        s.engine.start_account(s.id, services);
        let copy = CalendarCopy::new(Arc::clone(&s.engine), s.db.clone());
        let read = copy.refresh_due(&[s.id], NOW, true).await.unwrap();
        assert_eq!(read.events, 1);
        let id = s.id;
        let calendars = s.db.read(move |c| store::calendars(c, id)).await.unwrap();
        assert_eq!(calendars.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Personal"]);
    }

    #[tokio::test]
    async fn a_refused_login_waits_for_the_person_to_sign_in_again() {
        let s = setup().await;
        let (dav, services) = calendar_only();
        dav.put_resource(&format!("{PERSONAL}lunch.ics"), LUNCH);
        dav.with(|f| f.refuse_login = true);
        s.engine.start_account(s.id, services);
        assert_eq!(reported(&s).await, AccountState::Ok);
        let copy = CalendarCopy::new(Arc::clone(&s.engine), s.db.clone());
        copy.refresh_due(&[s.id], NOW, true).await.unwrap();
        assert_eq!(reported(&s).await, AccountState::NeedsReauth, "nothing else would notice the refused password");
        assert_eq!(state(&s).await, AccountState::NeedsReauth);

        // The server takes the password again, but the account that was
        // refused does not go on asking with the one it holds.
        dav.with(|f| f.refuse_login = false);
        let later = NOW + 3_600_000;
        assert_eq!(copy.refresh_due(&[s.id], later, true).await.unwrap().events, 0);

        // Signing in again starts the account on new services, and it reads.
        let api = Arc::new(crate::AnyDav::from(Arc::clone(&dav)));
        let calendar = crate::services::CalDav::new(api, crate::AnyMail::NoMail(crate::NoMail), Vec::new());
        s.engine.start_account(s.id, crate::AccountServices::calendar_only(crate::AnyCalendar::Dav(calendar)));
        assert_eq!(reported(&s).await, AccountState::Ok);
        assert_eq!(copy.refresh_due(&[s.id], later, true).await.unwrap().events, 1);
    }

    #[tokio::test]
    async fn a_mail_account_whose_calendar_refuses_the_login_keeps_its_state() {
        let h = crate::tests::imap_harness().await;
        let dav = Arc::new(mailrs_dav::fake::FakeDav::new());
        dav.with(|f| f.refuse_login = true);
        let api = Arc::new(crate::AnyDav::from(Arc::clone(&dav)));
        let mail = h.sync.services().mail.clone();
        let services = h.sync.services().clone().with_calendar(crate::AnyCalendar::Dav(crate::services::CalDav::new(api, mail, Vec::new())));
        let served = crate::tests::ServedBy::new(h.account_id, Arc::clone(&h.sync), services);
        let copy = CalendarCopy::new(Arc::new(served), h.db.clone());
        copy.refresh_due(&[h.account_id], NOW, true).await.unwrap();
        let id = h.account_id;
        let state = h.db.read(move |c| accounts::account(c, id)).await.unwrap().unwrap().state;
        assert_ne!(state, AccountState::NeedsReauth, "its mail still works, and its loop says how the account is");
    }

    #[tokio::test]
    async fn a_search_leaves_out_an_account_with_no_mail() {
        let h = harness().await;
        h.fake.seed(meta("only-remote", "t5", now_millis(), &["SPAM"]));
        let other = h.db.write(|c| accounts::insert_account(c, NAME, 0)).await.unwrap();
        let (_, services) = calendar_only();
        let (sender, _events) = async_channel::unbounded();
        let calendar_account = Arc::new(AccountSync::new(other, services, h.db.clone(), sender));
        let connected = HashMap::from([(h.account_id, Arc::clone(&h.sync)), (other, calendar_account)]);
        let lists = Mailboxes::new(Arc::new(Connected(connected)), h.db.clone());
        let account = |id, email: &str| Account {
            id,
            email: email.into(),
            state: AccountState::Ok,
            provider: Provider::default(),
            provider_name: None,
        };
        let scope = Scope::over([account(h.account_id, "me@example.com"), account(other, NAME)]);
        let search = Mailbox::Search { query: "in:spam".into(), account_id: None };
        let view = View { now: now_millis(), ..View::default() };
        let found = lists.list(&search, &scope, &view, Loaded::nothing()).await.expect("the search runs");
        assert_eq!(found.rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["t5"]);
        assert!(found.notices.is_empty(), "nothing is said of an account that was never asked: {:?}", found.notices);
    }
}
