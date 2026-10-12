//! Connecting, forgetting and signing in an account through the one
//! interface the app and the CLI share, for every provider.

use std::sync::Arc;

use mailrs_domain::{Account, AccountState, Provider, RemoveSetting, SignInClient};
use mailrs_gmail::OAuthClient;
use mailrs_graph::MicrosoftClient;
use mailrs_store::servers::{self, Pop3Servers, Saved, Security, Servers};
use mailrs_store::{Db, accounts};

use crate::config::Config;
use crate::passwords::{PasswordStore, Secrets};
use crate::services::{AnyIdentities, AnyMail};
use crate::sign_in::own_client::{ID_VAR, OwnClient, OwnClientError, SECRET_VAR};
use crate::sign_in::{Browser, BrowserSignInError, Wanted, google_sign_in, in_browser};
use crate::{Clients, Connected, Connector, Lacks};

async fn store() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("mail.db")).unwrap();
    (db, dir)
}

async fn account(db: &Db, email: &'static str) -> Account {
    db.read(move |c| accounts::account_by_email(c, email))
        .await
        .unwrap()
        .expect("the account is stored")
}

fn both_clients() -> Clients {
    Clients {
        google: Some(OAuthClient::new("cid", "secret")),
        microsoft: Some(MicrosoftClient::new("00000000-0000-0000-0000-000000000000")),
        own_google: None,
    }
}

fn connector(db: &Db, clients: Clients) -> Connector {
    Connector {
        db: db.clone(),
        config: Config::default(),
        clients,
        secrets: Secrets::memory(),
    }
}

fn saved(host: &str, port: u16, user: &str) -> Saved {
    Saved {
        host: host.into(),
        port,
        security: Security::Tls,
        user_name: user.into(),
    }
}

async fn gmail_account(db: &Db) -> Account {
    db.write(|c| accounts::insert_account(c, "me@gmail.com", 0))
        .await
        .unwrap();
    account(db, "me@gmail.com").await
}

async fn microsoft_account(db: &Db) -> Account {
    db.write(|c| accounts::insert_microsoft_account(c, "dana@outlook.com", "Outlook", 0))
        .await
        .unwrap()
        .unwrap();
    account(db, "dana@outlook.com").await
}

async fn imap_account(db: &Db) -> Account {
    db.write(|c| {
        let id = accounts::insert_imap_account(c, "dana@fastmail.com", "Fastmail", 0)?
            .expect("nobody holds the address");
        servers::save(
            c,
            id,
            &Servers {
                imap: saved("imap.fastmail.com", 993, "dana@fastmail.com"),
                smtp: saved("smtp.fastmail.com", 465, "dana@fastmail.com"),
            },
        )
    })
    .await
    .unwrap();
    account(db, "dana@fastmail.com").await
}

async fn pop3_account(db: &Db) -> Account {
    db.write(|c| {
        let id = accounts::insert_pop3_account(c, "dana@example.org", "example.org", RemoveSetting::Never, 0)?
            .expect("nobody holds the address");
        servers::save_pop3(
            c,
            id,
            &Pop3Servers {
                pop3: saved("pop.example.org", 995, "dana@example.org"),
                smtp: saved("smtp.example.org", 465, "dana@example.org"),
            },
        )
    })
    .await
    .unwrap();
    account(db, "dana@example.org").await
}

async fn state(db: &Db, account: &Account) -> AccountState {
    let id = account.id;
    db.read(move |c| accounts::account(c, id)).await.unwrap().unwrap().state
}

#[tokio::test]
async fn a_google_account_with_its_token_connects_to_google() {
    let (db, _dir) = store().await;
    let me = gmail_account(&db).await;
    let reach = connector(&db, both_clients());
    reach.secrets.google.save(&me.email, "rt").unwrap();
    let connected = reach.connect(&me).await.unwrap();
    assert!(matches!(connected, Connected::Ready(services) if matches!(services.mail, AnyMail::Google(_))));
}

#[tokio::test]
async fn a_google_account_without_a_token_needs_to_sign_in() {
    let (db, _dir) = store().await;
    let me = gmail_account(&db).await;
    let connected = connector(&db, both_clients()).connect(&me).await.unwrap();
    assert!(matches!(connected, Connected::NeedsSignIn(Lacks::Secret)));
}

#[tokio::test]
async fn a_build_without_a_google_client_asks_its_google_accounts_to_sign_in() {
    let (db, _dir) = store().await;
    let me = gmail_account(&db).await;
    let reach = connector(&db, Clients::default());
    reach.secrets.google.save(&me.email, "rt").unwrap();
    let connected = reach.connect(&me).await.unwrap();
    assert!(matches!(connected, Connected::NeedsSignIn(Lacks::Client)));
    assert_eq!(state(&db, &me).await, AccountState::NeedsReauth);
}

#[tokio::test]
async fn a_microsoft_account_with_its_token_connects_to_microsoft() {
    let (db, _dir) = store().await;
    let dana = microsoft_account(&db).await;
    let reach = connector(&db, both_clients());
    reach.secrets.microsoft.save(dana.id, "refresh").unwrap();
    let connected = reach.connect(&dana).await.unwrap();
    assert!(matches!(connected, Connected::Ready(services) if matches!(services.identities, AnyIdentities::Microsoft(_))));
}

#[tokio::test]
async fn a_microsoft_account_without_a_token_needs_to_sign_in() {
    let (db, _dir) = store().await;
    let dana = microsoft_account(&db).await;
    let connected = connector(&db, both_clients()).connect(&dana).await.unwrap();
    assert!(matches!(connected, Connected::NeedsSignIn(Lacks::Secret)));
    assert_eq!(state(&db, &dana).await, AccountState::NeedsReauth);
}

#[tokio::test]
async fn a_build_without_a_microsoft_client_asks_its_microsoft_accounts_to_sign_in() {
    let (db, _dir) = store().await;
    let dana = microsoft_account(&db).await;
    let reach = connector(&db, Clients::default());
    reach.secrets.microsoft.save(dana.id, "refresh").unwrap();
    let connected = reach.connect(&dana).await.unwrap();
    assert!(matches!(connected, Connected::NeedsSignIn(Lacks::Client)));
}

#[tokio::test]
async fn an_imap_account_with_its_password_connects_to_its_server() {
    let (db, _dir) = store().await;
    let dana = imap_account(&db).await;
    let reach = connector(&db, Clients::default());
    reach.secrets.passwords.save(dana.id, "secret").unwrap();
    let connected = reach.connect(&dana).await.unwrap();
    assert!(matches!(connected, Connected::Ready(services) if matches!(services.mail, AnyMail::Imap(_))));
}

#[tokio::test]
async fn an_imap_account_without_its_password_needs_to_sign_in() {
    let (db, _dir) = store().await;
    let dana = imap_account(&db).await;
    let connected = connector(&db, both_clients()).connect(&dana).await.unwrap();
    assert!(matches!(connected, Connected::NeedsSignIn(Lacks::Secret)));
}

#[tokio::test]
async fn a_pop3_account_with_its_password_connects_to_its_server() {
    let (db, _dir) = store().await;
    let dana = pop3_account(&db).await;
    let reach = connector(&db, Clients::default());
    reach.secrets.passwords.save(dana.id, "secret").unwrap();
    let connected = reach.connect(&dana).await.unwrap();
    assert!(matches!(connected, Connected::Ready(services) if matches!(services.mail, AnyMail::Pop3(_))));
}

#[tokio::test]
async fn a_pop3_account_without_its_password_needs_to_sign_in() {
    let (db, _dir) = store().await;
    let dana = pop3_account(&db).await;
    let connected = connector(&db, both_clients()).connect(&dana).await.unwrap();
    assert!(matches!(connected, Connected::NeedsSignIn(Lacks::Secret)));
}

/// Every store holds a secret for the account's id and address, so a
/// forget that reached into the wrong store would leave its own behind.
async fn forgetting(db: &Db, account: &Account) -> (Option<String>, Option<String>, Option<String>) {
    let reach = connector(db, both_clients());
    reach.secrets.google.save(&account.email, "token").unwrap();
    reach.secrets.passwords.save(account.id, "password").unwrap();
    reach.secrets.microsoft.save(account.id, "refresh").unwrap();
    reach.forget(account).await.unwrap();
    (
        reach.secrets.google.load(&account.email).unwrap(),
        reach.secrets.passwords.load(account.id).unwrap(),
        reach.secrets.microsoft.load(account.id).unwrap(),
    )
}

#[tokio::test]
async fn forgetting_a_google_account_deletes_only_its_refresh_token() {
    let (db, _dir) = store().await;
    let me = gmail_account(&db).await;
    let left = forgetting(&db, &me).await;
    assert_eq!(left, (None, Some("password".into()), Some("refresh".into())));
}

#[tokio::test]
async fn forgetting_a_microsoft_account_deletes_only_its_refresh_token() {
    let (db, _dir) = store().await;
    let dana = microsoft_account(&db).await;
    let left = forgetting(&db, &dana).await;
    assert_eq!(left, (Some("token".into()), Some("password".into()), None));
}

#[tokio::test]
async fn forgetting_an_imap_account_deletes_only_its_password() {
    let (db, _dir) = store().await;
    let dana = imap_account(&db).await;
    let left = forgetting(&db, &dana).await;
    assert_eq!(left, (Some("token".into()), None, Some("refresh".into())));
}

#[tokio::test]
async fn forgetting_a_pop3_account_deletes_only_its_password() {
    let (db, _dir) = store().await;
    let dana = pop3_account(&db).await;
    let left = forgetting(&db, &dana).await;
    assert_eq!(left, (Some("token".into()), None, Some("refresh".into())));
}

#[tokio::test]
async fn forgetting_a_google_account_deletes_its_own_client_too() {
    let (db, _dir) = store().await;
    let me = gmail_account(&db).await;
    let reach = connector(&db, both_clients());
    reach.secrets.google_clients.save(me.id, &OwnClient::new("own", "secret").encode()).unwrap();
    reach.forget(&me).await.unwrap();
    assert_eq!(reach.secrets.google_clients.load(me.id).unwrap(), None);
}

#[tokio::test]
async fn forgetting_an_imap_account_leaves_the_own_client_kept_under_its_id() {
    let (db, _dir) = store().await;
    let dana = imap_account(&db).await;
    let reach = connector(&db, both_clients());
    reach.secrets.google_clients.save(dana.id, "kept").unwrap();
    reach.forget(&dana).await.unwrap();
    assert_eq!(reach.secrets.google_clients.load(dana.id).unwrap().as_deref(), Some("kept"));
}

#[tokio::test]
async fn a_google_account_with_its_own_client_connects_in_a_build_without_one() {
    let (db, _dir) = store().await;
    let me = gmail_account(&db).await;
    let id = me.id;
    db.write(move |c| accounts::set_sign_in_client(c, id, SignInClient::Own)).await.unwrap();
    let reach = connector(&db, Clients::default());
    reach.secrets.google.save(&me.email, "rt").unwrap();
    reach.secrets.google_clients.save(me.id, &OwnClient::new("own", "secret").encode()).unwrap();
    let connected = reach.connect(&me).await.unwrap();
    assert!(matches!(connected, Connected::Ready(services) if matches!(services.mail, AnyMail::Google(_))));
}

#[tokio::test]
async fn forgetting_an_account_with_no_secret_kept_succeeds() {
    let (db, _dir) = store().await;
    let dana = imap_account(&db).await;
    connector(&db, both_clients()).forget(&dana).await.unwrap();
}

#[test]
fn only_google_and_microsoft_accounts_sign_in_in_the_browser() {
    assert_eq!(Browser::of(Provider::Gmail), Some(Browser::Google));
    assert_eq!(Browser::of(Provider::Microsoft), Some(Browser::Microsoft));
    assert_eq!(Browser::of(Provider::Imap), None);
    assert_eq!(Browser::of(Provider::Pop3), None);
    assert_eq!(Browser::of(Provider::Jmap), None);
}

fn new_jmap(address: &str, token: &str) -> crate::sign_in::NewJmap {
    use mailrs_store::jmap::{JmapAuth, JmapSaved};

    crate::sign_in::NewJmap {
        address: address.into(),
        provider_name: "Fastmail".into(),
        saved: JmapSaved {
            session_url: "https://api.fastmail.com/jmap/session".into(),
            auth: JmapAuth::Bearer,
            user_name: None,
            labels: true,
            capabilities: vec!["urn:ietf:params:jmap:mail".into()],
        },
        secret: token.into(),
        dav_password: None,
    }
}

#[tokio::test]
async fn a_jmap_account_connects_from_its_session_row_and_its_secret() {
    let (db, _dir) = store().await;
    let secrets = Secrets::memory();
    let new = new_jmap("me@fastmail.com", "fmu1-token");
    let account = crate::sign_in::jmap_signed_in(&db, &secrets, new, 0).await.unwrap();
    assert_eq!(account.provider, Provider::Jmap);
    let services = crate::connect_jmap(&db, Arc::clone(&secrets.jmap), Arc::clone(&secrets.passwords), &account, 30).await.unwrap();
    assert!(matches!(services.mail, AnyMail::Jmap(_)));
    assert!(matches!(services.rules, Some(crate::AnyRules::Local(_))), "a session without Sieve keeps its rules here");
    secrets.forget(&account).await.unwrap();
    let missing = crate::connect_jmap(&db, Arc::clone(&secrets.jmap), Arc::clone(&secrets.passwords), &account, 30).await;
    assert!(matches!(missing, Err(crate::SyncError::Backend(crate::BackendError::NeedsReauth))));
    assert_eq!(state(&db, &account).await, AccountState::NeedsReauth);
}

#[tokio::test]
async fn signing_a_jmap_account_in_again_keeps_the_new_token_and_ends_needs_sign_in() {
    let (db, _dir) = store().await;
    let secrets = Secrets::memory();
    let first = crate::sign_in::jmap_signed_in(&db, &secrets, new_jmap("me@fastmail.com", "old"), 0)
        .await
        .unwrap();
    let id = first.id;
    db.write(move |c| accounts::set_state(c, id, AccountState::NeedsReauth)).await.unwrap();
    let again = crate::sign_in::jmap_signed_in(&db, &secrets, new_jmap("me@fastmail.com", "new"), 1)
        .await
        .unwrap();
    assert_eq!(again.id, first.id);
    assert_eq!(again.state, AccountState::Ok);
    assert_eq!(secrets.jmap.load(id).unwrap().as_deref(), Some("new"));
}

#[tokio::test]
async fn a_new_jmap_account_never_inherits_a_password_left_under_its_id() {
    let (db, _dir) = store().await;
    let secrets = Secrets::memory();
    // A removed account whose forget failed left its passwords behind, and
    // ids are reused once the highest row goes.
    secrets.passwords.save(1, "stale").unwrap();
    secrets.passwords.save_outgoing(1, "stale smtp").unwrap();
    let account = crate::sign_in::jmap_signed_in(&db, &secrets, new_jmap("me@fastmail.com", "token"), 0)
        .await
        .unwrap();
    assert_eq!(account.id, 1);
    assert_eq!(secrets.passwords.load(1).unwrap(), None);
    assert_eq!(secrets.passwords.load_outgoing(1).unwrap(), None);
    assert_eq!(secrets.jmap.load(1).unwrap().as_deref(), Some("token"));
}

#[tokio::test]
async fn a_jmap_sign_in_for_an_address_an_imap_account_holds_is_refused() {
    let (db, _dir) = store().await;
    let secrets = Secrets::memory();
    let dana = imap_account(&db).await;
    let refused = crate::sign_in::jmap_signed_in(&db, &secrets, new_jmap(&dana.email, "token"), 0).await;
    assert!(matches!(refused, Err(crate::sign_in::JmapSignInError::Taken { .. })));
    assert_eq!(secrets.jmap.load(dana.id).unwrap(), None, "nothing reaches the keyring");
}

fn session_row(capabilities: &[&str], auth: mailrs_store::jmap::JmapAuth) -> mailrs_store::jmap::JmapSaved {
    mailrs_store::jmap::JmapSaved {
        session_url: "https://jmap.example.org/jmap/session".into(),
        auth,
        user_name: None,
        labels: true,
        capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
    }
}

async fn jmap_signed_in_with(db: &Db, secrets: &Secrets, saved: mailrs_store::jmap::JmapSaved, dav: Option<&str>) -> Account {
    let new = crate::sign_in::NewJmap {
        address: "me@example.org".into(),
        provider_name: "example.org".into(),
        saved,
        secret: "secret".into(),
        dav_password: dav.map(str::to_string),
    };
    crate::sign_in::jmap_signed_in(db, secrets, new, 0).await.unwrap()
}

#[tokio::test]
async fn jmap_contacts_win_over_a_found_carddav_server() {
    use mailrs_jmap::{CONTACTS, MAIL, SIEVE, VACATION};
    use mailrs_store::jmap::JmapAuth;
    use mailrs_store::services::{FoundService, ServiceKind};

    let (db, _dir) = store().await;
    let secrets = Secrets::memory();
    let account = jmap_signed_in_with(&db, &secrets, session_row(&[MAIL, CONTACTS, SIEVE, VACATION], JmapAuth::Basic), None).await;
    let id = account.id;
    db.write(move |c| {
        for (kind, url) in [(ServiceKind::CardDav, "https://dav.example.org/card/"), (ServiceKind::CalDav, "https://dav.example.org/cal/")] {
            mailrs_store::services::save(c, id, &FoundService { kind, url: url.into(), user_name: "me@example.org".into(), confirmed: true, source: "table".into() })?;
        }
        Ok(())
    })
    .await
    .unwrap();
    let services = crate::connect_jmap(&db, Arc::clone(&secrets.jmap), Arc::clone(&secrets.passwords), &account, 30).await.unwrap();
    assert!(matches!(services.contacts, Some(crate::AnyContacts::Jmap(_))));
    assert!(matches!(services.calendar, Some(crate::AnyCalendar::Dav(_))), "the calendar comes from CalDAV, with the account password");
    assert!(matches!(services.rules, Some(crate::AnyRules::JmapSieve(_))));
    assert!(matches!(services.auto_reply, Some(crate::AnyAutoReply::Jmap(_))));
}

#[tokio::test]
async fn a_token_account_reaches_dav_only_with_its_app_password() {
    use mailrs_jmap::MAIL;
    use mailrs_store::jmap::JmapAuth;
    use mailrs_store::services::{FoundService, ServiceKind};

    let (db, _dir) = store().await;
    let secrets = Secrets::memory();
    let account = jmap_signed_in_with(&db, &secrets, session_row(&[MAIL], JmapAuth::Bearer), None).await;
    let id = account.id;
    db.write(move |c| {
        mailrs_store::services::save(c, id, &FoundService { kind: ServiceKind::CalDav, url: "https://caldav.example.org/".into(), user_name: "me@example.org".into(), confirmed: true, source: "table".into() })
    })
    .await
    .unwrap();
    let without = crate::connect_jmap(&db, Arc::clone(&secrets.jmap), Arc::clone(&secrets.passwords), &account, 30).await.unwrap();
    assert!(without.calendar.is_none(), "a token goes nowhere but JMAP");
    assert!(matches!(without.rules, Some(crate::AnyRules::Local(_))));
    let with = jmap_signed_in_with(&db, &secrets, session_row(&[MAIL], JmapAuth::Bearer), Some("app-password")).await;
    let services = crate::connect_jmap(&db, Arc::clone(&secrets.jmap), Arc::clone(&secrets.passwords), &with, 30).await.unwrap();
    assert!(services.calendar.is_some());
}

#[tokio::test]
async fn a_new_jmap_account_keeps_its_calendar_password_beside_its_token() {
    use mailrs_store::jmap::JmapAuth;

    let (db, _dir) = store().await;
    let secrets = Secrets::memory();
    let account = jmap_signed_in_with(&db, &secrets, session_row(&[mailrs_jmap::MAIL], JmapAuth::Bearer), Some("app-password")).await;
    assert_eq!(secrets.passwords.load(account.id).unwrap().as_deref(), Some("app-password"));
    assert_eq!(secrets.jmap.load(account.id).unwrap().as_deref(), Some("secret"));
}

/// The page a browser sign-in hands the opener, with the run dropped once
/// it has, since nobody comes back from that page in a test.
async fn page_opened(browser: Browser, wanted: Wanted) -> String {
    page_opened_with(both_clients(), browser, wanted).await
}

async fn page_opened_with(clients: Clients, browser: Browser, wanted: Wanted) -> String {
    let (db, _dir) = store().await;
    let reach = connector(&db, clients);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let run = in_browser(&reach, browser, wanted, move |url: &str| {
        let _ = tx.send(url.to_string());
    });
    tokio::select! {
        _ = run => panic!("the sign-in ended before anyone came back from the page"),
        url = rx => url.unwrap(),
    }
}

#[tokio::test]
async fn signing_a_google_account_in_again_opens_googles_page() {
    let me = Account {
        id: 1,
        email: "me@gmail.com".into(),
        state: AccountState::NeedsReauth,
        provider: Provider::Gmail,
        provider_name: None,
    };
    let browser = Browser::of(me.provider).unwrap();
    let url = page_opened(browser, Wanted::Only(me.email)).await;
    assert!(url.starts_with("https://accounts.google.com/"), "{url}");
}

#[tokio::test]
async fn signing_a_microsoft_account_in_again_opens_microsofts_page_for_its_address() {
    let dana = Account {
        id: 1,
        email: "dana@outlook.com".into(),
        state: AccountState::NeedsReauth,
        provider: Provider::Microsoft,
        provider_name: Some("Outlook".into()),
    };
    let browser = Browser::of(dana.provider).unwrap();
    let url = page_opened(browser, Wanted::Only(dana.email)).await;
    assert!(url.starts_with("https://login.microsoftonline.com/"), "{url}");
    assert!(url.contains("login_hint=dana%40outlook.com"), "{url}");
}

#[tokio::test]
async fn a_build_without_the_client_opens_no_page() {
    let (db, _dir) = store().await;
    let reach = connector(&db, Clients::default());
    for browser in [Browser::Google, Browser::Microsoft] {
        let refused = in_browser(&reach, browser, Wanted::Anyone, |url: &str| {
            panic!("opened {url}")
        })
        .await;
        assert!(matches!(refused, Err(BrowserSignInError::NoClient(b)) if b == browser));
    }
}

#[tokio::test]
async fn an_own_client_in_the_environment_signs_in_over_the_built_in_one() {
    let clients = Clients {
        own_google: Some(Ok(OwnClient::new("own-cid", "own-secret"))),
        ..both_clients()
    };
    let url = page_opened_with(clients, Browser::Google, Wanted::Anyone).await;
    assert!(url.contains("client_id=own-cid"), "{url}");
}

#[tokio::test]
async fn an_own_client_signs_in_from_a_build_without_the_built_in_one() {
    let clients = Clients {
        own_google: Some(Ok(OwnClient::new("own-cid", "own-secret"))),
        ..Clients::default()
    };
    let url = page_opened_with(clients, Browser::Google, Wanted::Anyone).await;
    assert!(url.contains("client_id=own-cid"), "{url}");
}

#[tokio::test]
async fn half_an_own_client_opens_no_page_and_says_which_value_is_missing() {
    let (db, _dir) = store().await;
    let half = OwnClientError::Half { set: ID_VAR, missing: SECRET_VAR };
    let clients = Clients { own_google: Some(Err(half.clone())), ..both_clients() };
    let refused = in_browser(&connector(&db, clients), Browser::Google, Wanted::Anyone, |url: &str| {
        panic!("opened {url}")
    })
    .await;
    assert!(matches!(refused, Err(BrowserSignInError::OwnClient(err)) if err == half));
}

#[test]
fn the_sign_in_says_which_client_it_uses() {
    let own = Clients { own_google: Some(Ok(OwnClient::new("own-cid", "own-secret"))), ..both_clients() };
    assert_eq!(google_sign_in(&own).unwrap().1, SignInClient::Own);
    assert_eq!(google_sign_in(&both_clients()).unwrap().1, SignInClient::BuiltIn);
    assert!(matches!(google_sign_in(&Clients::default()), Err(BrowserSignInError::NoClient(Browser::Google))));
}

/// A switch to JMAP that crashed after saving its secret leaves it under
/// the IMAP or POP3 account's id.
#[tokio::test]
async fn forgetting_an_imap_or_pop3_account_deletes_a_jmap_secret_too() {
    let (db, _dir) = store().await;
    for account in [imap_account(&db).await, pop3_account(&db).await] {
        let reach = connector(&db, both_clients());
        reach.secrets.jmap.save(account.id, "token").unwrap();
        reach.forget(&account).await.unwrap();
        assert_eq!(reach.secrets.jmap.load(account.id).unwrap(), None, "{:?}", account.provider);
    }
}
