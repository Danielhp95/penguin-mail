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
    use crate::{AccountSync, Accounts, CalendarService, EngineConfig, SyncEngine, now_millis};

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
    async fn a_refusal_met_outside_the_read_is_reported_once() {
        let s = setup().await;
        let (dav, services) = calendar_only();
        dav.put_resource(&format!("{PERSONAL}lunch.ics"), LUNCH);
        s.engine.start_account(s.id, services);
        assert_eq!(reported(&s).await, AccountState::Ok);
        let copy = CalendarCopy::new(Arc::clone(&s.engine), s.db.clone());
        assert_eq!(copy.refresh_due(&[s.id], NOW, true).await.unwrap().events, 1);

        // The password stops working, and what meets the refusal first is
        // not the periodic read but a call made for the person.
        dav.with(|f| f.refuse_login = true);
        let calendar = Accounts::services(&*s.engine, s.id).and_then(|services| services.calendar).expect("the calendar");
        assert!(matches!(calendar.calendars().await, Err(crate::BackendError::NeedsReauth)));

        // The next tick comes before another read is due. It still says so.
        copy.refresh_due(&[s.id], NOW + 1_000, true).await.unwrap();
        assert_eq!(reported(&s).await, AccountState::NeedsReauth);
        assert_eq!(state(&s).await, AccountState::NeedsReauth);

        // And says it once, not at every tick after.
        copy.refresh_due(&[s.id], NOW + 2_000, true).await.unwrap();
        copy.refresh_due(&[s.id], NOW + 3_000, true).await.unwrap();
        assert!(s.events.try_recv().is_err(), "the state was reported again");
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

mod sign_in {
    use std::sync::{Arc, Mutex};

    use mailrs_dav::Kind;
    use mailrs_domain::{Account, AccountState, Provider};
    use mailrs_sieve::script::Extensions;
    use mailrs_store::services::{self, ServiceKind};
    use mailrs_store::{Db, accounts, calendar as store};

    use crate::passwords::{MemoryPasswords, PasswordError, PasswordStore, Secrets};
    use crate::services::finding::ServiceProbe;
    use crate::sign_in::{CalDavSignInError, NewCalDav, calendar_account_name, caldav_signed_in};
    use crate::{BackendError, Connected, Connector, Lacks, SyncError, connect_caldav};

    const URL: &str = "https://cloud.example.org/remote.php/dav/";
    const NAME: &str = "robin@cloud.example.org";

    /// A server at one address that takes one login. Any other address
    /// answers as no calendar server, and any other login is refused.
    struct Probe {
        url: &'static str,
        tried: Mutex<Vec<String>>,
    }

    fn probe(url: &'static str) -> Probe {
        Probe { url, tried: Mutex::default() }
    }

    impl ServiceProbe for Probe {
        async fn dav(&self, url: &str, user: &str, password: &str, _kind: Kind) -> Result<(), BackendError> {
            self.tried.lock().unwrap().push(url.to_string());
            if url != self.url {
                return Err(BackendError::Unsupported);
            }
            match (user, password) {
                ("robin" | "me@example.org", "app-password") => Ok(()),
                _ => Err(BackendError::NeedsReauth),
            }
        }

        async fn sieve(&self, _host: &str, _port: u16, _user: &str, _password: &str) -> Result<Extensions, BackendError> {
            Err(BackendError::Unsupported)
        }
    }

    fn new(url: &str, user_name: &str, password: &str) -> NewCalDav {
        NewCalDav { url: url.into(), user_name: user_name.into(), password: password.into() }
    }

    async fn db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Db::open(&dir.path().join("mail.db")).unwrap(), dir)
    }

    struct Refusing;

    impl PasswordStore for Refusing {
        fn load(&self, _: mailrs_domain::AccountId) -> Result<Option<String>, PasswordError> {
            Ok(None)
        }
        fn save(&self, _: mailrs_domain::AccountId, _: &str) -> Result<(), PasswordError> {
            Err(PasswordError::Keyring("the collection is locked".into()))
        }
        fn delete(&self, _: mailrs_domain::AccountId) -> Result<(), PasswordError> {
            Ok(())
        }
    }

    #[test]
    fn a_calendar_account_is_named_by_its_user_and_its_host() {
        assert_eq!(calendar_account_name(URL, "robin").as_deref(), Some(NAME));
        assert_eq!(
            calendar_account_name("https://Cloud.Example.org:8443/dav", " robin ").as_deref(),
            Some(NAME),
            "the host alone, in lower case, and the user name without its spaces"
        );
        assert_eq!(
            calendar_account_name(URL, "me@example.org").as_deref(),
            Some("me@example.org@cloud.example.org"),
            "a user name that is an address keeps the host, so it never names a mail account"
        );
        assert_eq!(calendar_account_name("not an address", "robin"), None);
        assert_eq!(calendar_account_name(URL, "  "), None);
    }

    #[tokio::test]
    async fn a_calendar_account_signs_in_and_is_named_user_at_host() {
        let (db, _dir) = db().await;
        let passwords = Arc::new(MemoryPasswords::default());
        let account = caldav_signed_in(&probe(URL), &db, Arc::clone(&passwords), new(URL, "robin", "app-password"), 7)
            .await
            .unwrap();
        assert_eq!((account.email.as_str(), account.provider), (NAME, Provider::CalDav));
        let id = account.id;
        let kept = db.read(move |c| services::load(c, id)).await.unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!((kept[0].kind, kept[0].url.as_str(), kept[0].user_name.as_str()), (ServiceKind::CalDav, URL, "robin"));
        assert!(kept[0].confirmed, "the person typed the address, so the password may go to it");
        assert_eq!(kept[0].source, "typed");
        assert_eq!(passwords.load(id).unwrap().as_deref(), Some("app-password"));
    }

    #[tokio::test]
    async fn a_bare_host_and_spaces_are_normalized() {
        let (db, _dir) = db().await;
        let server = probe("https://cloud.example.org/");
        let account = caldav_signed_in(&server, &db, Arc::new(MemoryPasswords::default()), new("  cloud.example.org ", " robin ", "app-password"), 7)
            .await
            .unwrap();
        assert_eq!(account.email, NAME);
        let id = account.id;
        let kept = db.read(move |c| services::load(c, id)).await.unwrap();
        assert_eq!((kept[0].url.as_str(), kept[0].user_name.as_str()), ("https://cloud.example.org/", "robin"));
    }

    #[tokio::test]
    async fn http_is_refused_and_nothing_is_kept() {
        let (db, _dir) = db().await;
        let server = probe(URL);
        let refused = caldav_signed_in(&server, &db, Arc::new(MemoryPasswords::default()), new("http://cloud.example.org/remote.php/dav/", "robin", "app-password"), 7).await;
        assert!(matches!(refused, Err(CalDavSignInError::Server(SyncError::Backend(BackendError::Refused(_))))), "{:?}", refused.err());
        assert!(server.tried.lock().unwrap().is_empty(), "the password went nowhere");
        assert!(db.read(accounts::list_accounts).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_refused_login_keeps_nothing() {
        let (db, _dir) = db().await;
        let passwords = Arc::new(MemoryPasswords::default());
        let refused = caldav_signed_in(&probe(URL), &db, Arc::clone(&passwords), new(URL, "robin", "wrong"), 7).await;
        assert!(matches!(refused, Err(CalDavSignInError::Server(SyncError::Backend(BackendError::NeedsReauth)))));
        assert!(db.read(accounts::list_accounts).await.unwrap().is_empty());
        assert_eq!(passwords.load(1).unwrap(), None);
    }

    #[tokio::test]
    async fn an_address_that_is_no_calendar_server_keeps_nothing() {
        let (db, _dir) = db().await;
        let refused = caldav_signed_in(&probe(URL), &db, Arc::new(MemoryPasswords::default()), new("https://example.org/", "robin", "app-password"), 7).await;
        assert!(matches!(refused, Err(CalDavSignInError::Server(SyncError::Backend(BackendError::Unsupported)))));
        assert!(db.read(accounts::list_accounts).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_user_name_that_is_an_address_does_not_take_a_mail_accounts_name() {
        let (db, _dir) = db().await;
        db.write(|c| accounts::insert_imap_account(c, "me@example.org", "example.org", 1)).await.unwrap();
        let account = caldav_signed_in(&probe(URL), &db, Arc::new(MemoryPasswords::default()), new(URL, "me@example.org", "app-password"), 7)
            .await
            .unwrap();
        assert_eq!(account.email, "me@example.org@cloud.example.org");
        assert_eq!(db.read(accounts::list_accounts).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_name_another_kind_of_account_holds_is_refused() {
        let (db, _dir) = db().await;
        db.write(|c| accounts::insert_imap_account(c, NAME, "example.org", 1)).await.unwrap();
        let refused = caldav_signed_in(&probe(URL), &db, Arc::new(MemoryPasswords::default()), new(URL, "robin", "app-password"), 7).await;
        let Err(CalDavSignInError::Taken { name, .. }) = refused else {
            panic!("the name is taken: {:?}", refused.map(|a| a.email));
        };
        assert_eq!(name, NAME);
    }

    #[tokio::test]
    async fn signing_in_again_keeps_the_calendars_and_ends_needs_sign_in() {
        let (db, _dir) = db().await;
        let passwords = Arc::new(MemoryPasswords::default());
        let first = caldav_signed_in(&probe(URL), &db, Arc::clone(&passwords), new(URL, "robin", "app-password"), 7)
            .await
            .unwrap();
        let id = first.id;
        db.write(move |c| {
            let personal = mailrs_domain::calendar::Calendar { id: "/cal/personal/".into(), name: "Personal".into(), ..Default::default() };
            store::save_calendars(c, id, &[personal])?;
            accounts::set_state(c, id, AccountState::NeedsReauth)
        })
        .await
        .unwrap();
        let again = caldav_signed_in(&probe(URL), &db, Arc::clone(&passwords), new(URL, "robin", "app-password"), 9)
            .await
            .unwrap();
        assert_eq!(again.id, id, "one account, not two");
        assert_eq!(again.state, AccountState::Ok);
        assert_eq!(db.read(move |c| store::calendars(c, id)).await.unwrap().len(), 1, "what the copy held is still there");
    }

    #[tokio::test]
    async fn a_keyring_that_refuses_leaves_no_account() {
        let (db, _dir) = db().await;
        let refused = caldav_signed_in(&probe(URL), &db, Arc::new(Refusing), new(URL, "robin", "app-password"), 7).await;
        assert!(matches!(refused, Err(CalDavSignInError::Password(_))));
        assert!(db.read(accounts::list_accounts).await.unwrap().is_empty());
    }

    async fn signed_in(db: &Db, passwords: Arc<MemoryPasswords>) -> Account {
        caldav_signed_in(&probe(URL), db, passwords, new(URL, "robin", "app-password"), 7).await.unwrap()
    }

    #[tokio::test]
    async fn connect_builds_services_with_a_calendar_and_no_mail() {
        let (db, _dir) = db().await;
        let passwords = Arc::new(MemoryPasswords::default());
        let account = signed_in(&db, Arc::clone(&passwords)).await;
        let services = connect_caldav(&db, passwords, &account).await.unwrap();
        assert!(!services.serves_mail());
        assert!(services.calendar.is_some());
        assert!(services.contacts.is_none() && services.rules.is_none() && services.auto_reply.is_none());
    }

    #[tokio::test]
    async fn connect_without_a_password_needs_sign_in() {
        let (db, _dir) = db().await;
        let account = signed_in(&db, Arc::new(MemoryPasswords::default())).await;
        let connector = Connector { db: db.clone(), config: crate::config::Config::default(), clients: crate::Clients::default(), secrets: Secrets::memory() };
        assert!(matches!(connector.connect(&account).await, Ok(Connected::NeedsSignIn(Lacks::Secret))));
        let id = account.id;
        let state = db.read(move |c| accounts::account(c, id)).await.unwrap().unwrap().state;
        assert_eq!(state, AccountState::NeedsReauth, "the store says so, since no loop will");
    }

    #[tokio::test]
    async fn forgetting_a_calendar_account_deletes_its_password() {
        let (db, _dir) = db().await;
        let secrets = Secrets::memory();
        let account = caldav_signed_in(&probe(URL), &db, Arc::clone(&secrets.passwords), new(URL, "robin", "app-password"), 7)
            .await
            .unwrap();
        assert!(secrets.passwords.load(account.id).unwrap().is_some());
        secrets.forget(&account).await.unwrap();
        assert_eq!(secrets.passwords.load(account.id).unwrap(), None);
    }
}
