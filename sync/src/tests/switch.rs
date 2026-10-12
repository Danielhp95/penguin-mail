//! Switching an IMAP account to JMAP: matching stored mail to JMAP ids,
//! and the switch itself.

use mailrs_domain::Provider;
use mailrs_jmap::Mailbox;
use mailrs_store::jmap::{JmapAuth, JmapSaved};
use mailrs_store::outbox::Queued;
use mailrs_store::switch::{StoredMailbox, StoredMessage};

use crate::fake::FakeJmap;
use crate::passwords::{PasswordStore, Secrets};
use crate::switch::{SwitchError, match_mailboxes, match_messages, to_jmap_over};
use crate::tests::{ImapHarness, imap_harness};

fn raw(message_id: &str) -> Vec<u8> {
    format!("From: a@example.com\r\nTo: me@example.com\r\nSubject: s\r\nMessage-ID: <{message_id}>\r\n\r\nx\r\n").into_bytes()
}

fn stored(id: &str, message_id: Option<&str>, date: i64) -> StoredMessage {
    StoredMessage { id: id.into(), message_id: message_id.map(str::to_string), date }
}

#[tokio::test]
async fn a_message_matches_by_its_message_id_and_date() {
    let fake = FakeJmap::new();
    let jmap_id = fake.deliver("inbox", &raw("a@x"), 1_000_000);
    let moved = match_messages(&fake, &[stored("INBOX/7/1", Some("a@x"), 1_000_500), stored("INBOX/7/2", None, 0)]).await.unwrap();
    assert_eq!(moved.len(), 1, "no Message-ID, no match");
    assert_eq!(moved[0].from, "INBOX/7/1");
    assert_eq!(moved[0].to, jmap_id);
    assert_eq!(moved[0].thread, fake.with(|s| s.emails[&jmap_id].thread_id.clone()));
}

#[tokio::test]
async fn a_stored_id_with_angle_brackets_still_matches() {
    let fake = FakeJmap::new();
    let jmap_id = fake.deliver("inbox", &raw("a@x"), 1_000_000);
    let moved = match_messages(&fake, &[stored("INBOX/7/1", Some(" <a@x> "), 1_000_000)]).await.unwrap();
    assert_eq!(moved[0].to, jmap_id);
}

#[tokio::test]
async fn two_copies_of_one_message_id_never_share_a_jmap_id() {
    let fake = FakeJmap::new();
    let first = fake.deliver("inbox", &raw("dup@x"), 1_000_000);
    let moved = match_messages(&fake, &[stored("INBOX/7/1", Some("dup@x"), 1_000_000), stored("Copy/3/1", Some("dup@x"), 1_000_000)])
        .await
        .unwrap();
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].to, first);
}

#[tokio::test]
async fn two_copies_on_the_server_go_to_two_stored_copies() {
    let fake = FakeJmap::new();
    let first = fake.deliver("inbox", &raw("dup@x"), 1_000_000);
    let second = fake.deliver("inbox", &raw("dup@x"), 1_000_000);
    let moved = match_messages(&fake, &[stored("INBOX/7/1", Some("dup@x"), 1_000_000), stored("Copy/3/1", Some("dup@x"), 1_000_000)])
        .await
        .unwrap();
    let mut to: Vec<_> = moved.iter().map(|m| m.to.clone()).collect();
    to.sort();
    let mut want = [first, second];
    want.sort();
    assert_eq!(to, want);
}

#[tokio::test]
async fn a_far_off_date_is_another_message() {
    let fake = FakeJmap::new();
    fake.deliver("inbox", &raw("old@x"), 1_000_000);
    let moved = match_messages(&fake, &[stored("INBOX/7/1", Some("old@x"), 1_000_000 + 3 * 86_400_000)]).await.unwrap();
    assert!(moved.is_empty());
}

#[tokio::test]
async fn a_server_that_clamps_its_query_pages_loses_no_match() {
    let fake = FakeJmap::new().clamping_queries(2);
    let ids: Vec<String> = (0..7).map(|n| format!("m{n}@x")).collect();
    let jmap: Vec<String> = ids.iter().map(|id| fake.deliver("inbox", &raw(id), 1_000_000)).collect();
    let kept: Vec<StoredMessage> = ids.iter().enumerate().map(|(n, id)| stored(&format!("INBOX/7/{n}"), Some(id), 1_000_000)).collect();
    let moved = match_messages(&fake, &kept).await.unwrap();
    let mut to: Vec<_> = moved.into_iter().map(|m| m.to).collect();
    to.sort();
    let mut want = jmap;
    want.sort();
    assert_eq!(to, want);
}

#[test]
fn mailboxes_match_by_role_then_by_path() {
    let listed = vec![
        Mailbox { id: "mb-inbox".into(), name: "Inbox".into(), role: Some("inbox".into()), ..Default::default() },
        Mailbox { id: "mb-trips".into(), name: "Trips".into(), ..Default::default() },
        Mailbox { id: "mb-rome".into(), name: "Rome".into(), parent_id: Some("mb-trips".into()), ..Default::default() },
    ];
    let stored = vec![
        StoredMailbox { id: "INBOX".into(), name: "Inbox".into(), role: Some("inbox".into()) },
        StoredMailbox { id: "INBOX.Trips.Rome".into(), name: "Trips.Rome".into(), role: None },
        StoredMailbox { id: "Gone".into(), name: "Gone".into(), role: None },
    ];
    let mut matched = match_mailboxes(&stored, &listed);
    matched.sort();
    assert_eq!(matched, [("INBOX".to_string(), "mb-inbox".to_string()), ("INBOX.Trips.Rome".to_string(), "mb-rome".to_string())]);
}

fn saved() -> JmapSaved {
    JmapSaved {
        session_url: "https://api.fastmail.com/jmap/session".into(),
        auth: JmapAuth::Bearer,
        user_name: None,
        labels: true,
        capabilities: vec![mailrs_jmap::MAIL.into(), mailrs_jmap::SUBMISSION.into()],
    }
}

/// An IMAP account bootstrapped from FakeImap, whose server's JMAP side
/// holds the same two messages.
async fn imap_with_jmap_twin() -> (ImapHarness, FakeJmap, Secrets) {
    imap_with_jmap_twin_and(|_| {}).await
}

/// The same, with `more` putting more mail on the IMAP side first.
async fn imap_with_jmap_twin_and(more: impl FnOnce(&ImapHarness)) -> (ImapHarness, FakeJmap, Secrets) {
    let h = imap_harness().await;
    let now = crate::now_millis();
    h.imap.deliver("INBOX", raw("one@x"), now);
    h.imap.deliver("INBOX", raw("two@x"), now);
    more(&h);
    h.bootstrap().await;
    let id = h.account_id;
    h.db.write(move |c| Ok(c.execute("UPDATE accounts SET provider = 'imap', provider_name = 'Fastmail' WHERE id = ?1", [id])?))
        .await
        .unwrap();
    let fake = FakeJmap::new();
    fake.deliver("inbox", &raw("one@x"), now);
    fake.deliver("inbox", &raw("two@x"), now);
    let secrets = Secrets::memory();
    secrets.passwords.save(id, "imap-app-password").unwrap();
    (h, fake, secrets)
}

async fn account(h: &ImapHarness) -> mailrs_domain::Account {
    let id = h.account_id;
    h.db.read(move |c| mailrs_store::accounts::account(c, id)).await.unwrap().unwrap()
}

/// The stored id of the message whose Message-ID holds `message_id`.
async fn stored_id(h: &ImapHarness, message_id: &str) -> Option<String> {
    let (id, wanted) = (h.account_id, format!("%{message_id}%"));
    h.db.read(move |c| {
        let mut stmt = c.prepare("SELECT id FROM messages WHERE account_id = ?1 AND rfc822_msgid LIKE ?2")?;
        let found = stmt.query_map(rusqlite::params![id, wanted], |row| row.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(found.into_iter().next())
    })
    .await
    .unwrap()
}

async fn outbox_rows(h: &ImapHarness) -> Vec<Queued> {
    h.db.read(mailrs_store::outbox::list).await.unwrap()
}

/// A draft in the IMAP account's Drafts, written an hour ago.
fn draft_in_imap(h: &ImapHarness, message_id: &str) {
    h.imap.deliver_flagged("Drafts", &raw(message_id), &["\\Draft", "\\Seen"], crate::now_millis() - 3_600_000);
}

#[tokio::test]
async fn a_switch_keeps_the_account_and_its_mail_under_jmap_ids() {
    let (h, fake, secrets) = imap_with_jmap_twin().await;
    let account = account(&h).await;
    let switched = to_jmap_over(&h.db, &secrets, &account, &fake, saved(), "fmu1-token".into()).await.unwrap();
    assert_eq!(switched.account.provider, Provider::Jmap);
    assert_eq!(switched.account.id, h.account_id);
    assert_eq!(switched.kept, 2);
    let ids = h.ids().await;
    let jmap_ids: Vec<String> = fake.with(|s| s.emails.keys().cloned().collect());
    assert!(ids.iter().all(|id| jmap_ids.contains(id)), "{ids:?}");
    assert_eq!(secrets.jmap.load(h.account_id).unwrap().as_deref(), Some("fmu1-token"));
    assert_eq!(secrets.passwords.load(h.account_id).unwrap(), None, "no DAV server kept it");
}

#[tokio::test]
async fn the_imap_password_stays_when_a_calendar_server_needs_it() {
    let (h, fake, secrets) = imap_with_jmap_twin().await;
    let id = h.account_id;
    h.db.write(move |c| {
        mailrs_store::services::save(c, id, &mailrs_store::services::FoundService {
            kind: mailrs_store::services::ServiceKind::CalDav,
            url: "https://caldav.fastmail.com/dav/calendars".into(),
            user_name: "me@example.com".into(),
            confirmed: true,
            source: "table".into(),
        })
    })
    .await
    .unwrap();
    let account = account(&h).await;
    to_jmap_over(&h.db, &secrets, &account, &fake, saved(), "fmu1-token".into()).await.unwrap();
    assert_eq!(secrets.passwords.load(id).unwrap().as_deref(), Some("imap-app-password"));
}

#[tokio::test]
async fn a_refused_sign_in_leaves_the_imap_account_as_it_was() {
    let (h, fake, secrets) = imap_with_jmap_twin().await;
    let before = h.ids().await;
    fake.with(|s| s.session.capabilities.remove(mailrs_jmap::MAIL));
    let id = h.account_id;
    let account = account(&h).await;
    let refused = to_jmap_over(&h.db, &secrets, &account, &fake, saved(), "fmu1-token".into()).await;
    assert!(matches!(refused, Err(SwitchError::Sync(_)) | Err(SwitchError::SignIn(_))), "{refused:?}");
    assert_eq!(h.ids().await, before);
    let row = h.db.read(move |c| mailrs_store::accounts::account(c, id)).await.unwrap().unwrap();
    assert_eq!(row.provider, Provider::Imap);
    assert_eq!(secrets.jmap.load(id).unwrap(), None, "no token is left behind");
    assert_eq!(secrets.passwords.load(id).unwrap().as_deref(), Some("imap-app-password"));
}

#[tokio::test]
async fn only_an_imap_account_switches() {
    let (h, fake, secrets) = imap_with_jmap_twin().await;
    let id = h.account_id;
    h.db.write(move |c| Ok(c.execute("UPDATE accounts SET provider = 'pop3' WHERE id = ?1", [id])?)).await.unwrap();
    let account = account(&h).await;
    assert!(matches!(to_jmap_over(&h.db, &secrets, &account, &fake, saved(), "t".into()).await, Err(SwitchError::NotImap)));
}

#[tokio::test]
async fn a_swap_that_fails_takes_the_token_back_out() {
    let (h, fake, secrets) = imap_with_jmap_twin().await;
    let (id, before) = (h.account_id, h.ids().await);
    h.db.write(|c| {
        Ok(c.execute_batch(
            "CREATE TEMP TRIGGER no_switch BEFORE UPDATE OF provider ON accounts BEGIN SELECT RAISE(ABORT, 'no'); END;",
        )?)
    })
    .await
    .unwrap();
    let account = account(&h).await;
    let failed = to_jmap_over(&h.db, &secrets, &account, &fake, saved(), "fmu1-token".into()).await;
    assert!(matches!(failed, Err(SwitchError::Sync(_))), "{failed:?}");
    assert_eq!(h.ids().await, before);
    assert_eq!(secrets.jmap.load(id).unwrap(), None, "no token is left behind");
    assert_eq!(secrets.passwords.load(id).unwrap().as_deref(), Some("imap-app-password"));
}

#[tokio::test]
async fn an_unsent_draft_kept_only_here_crosses_to_jmap_drafts() {
    let (h, fake, secrets) = imap_with_jmap_twin_and(|h| draft_in_imap(h, "draft@x")).await;
    let old = stored_id(&h, "draft@x").await.expect("the draft is stored");
    let id = h.account_id;
    let old_id = old.clone();
    h.db.write(move |c| mailrs_store::local_messages::put(c, id, &old_id, &raw("draft@x"))).await.unwrap();
    let account = account(&h).await;
    to_jmap_over(&h.db, &secrets, &account, &fake, saved(), "fmu1-token".into()).await.unwrap();
    let new = stored_id(&h, "draft@x").await.expect("the draft is still stored");
    assert_ne!(new, old);
    let drafts = fake.mailbox("drafts");
    let (keywords, mailboxes) = fake.with(|s| (s.emails[&new].keywords.clone(), s.emails[&new].mailbox_ids.clone()));
    assert!(keywords.contains("$draft"), "{keywords:?}");
    assert!(mailboxes.contains(&drafts), "{mailboxes:?}");
}

#[tokio::test]
async fn a_send_later_draft_crosses_with_the_bytes_its_outbox_row_holds() {
    let (h, fake, secrets) = imap_with_jmap_twin_and(|h| draft_in_imap(h, "later@x")).await;
    let old = stored_id(&h, "later@x").await.expect("the draft is stored");
    let id = h.account_id;
    let queued = Queued {
        account_id: id,
        draft_id: Some(old.clone()),
        message_id: Some(old.clone()),
        subject: "s".into(),
        recipients: "me@example.com".into(),
        send_at: crate::now_millis() + 86_400_000,
        raw: Some(raw("later@x")),
        ..Queued::default()
    };
    h.db.write(move |c| mailrs_store::outbox::put(c, &queued)).await.unwrap();
    let account = account(&h).await;
    to_jmap_over(&h.db, &secrets, &account, &fake, saved(), "fmu1-token".into()).await.unwrap();
    let new = stored_id(&h, "later@x").await.expect("the draft is still stored");
    assert!(fake.with(|s| s.emails.contains_key(&new)));
    let row = outbox_rows(&h).await.remove(0);
    assert_eq!(row.draft_id.as_deref(), Some(new.as_str()));
    assert_eq!(row.message_id.as_deref(), Some(new.as_str()));
}

#[tokio::test]
async fn a_draft_the_server_already_holds_is_not_uploaded_again() {
    let (h, fake, secrets) = imap_with_jmap_twin_and(|h| draft_in_imap(h, "again@x")).await;
    // An earlier switch that failed later on left this copy, received
    // when it was imported rather than when the draft was written.
    let earlier = fake.deliver("drafts", &raw("again@x"), crate::now_millis() + 3 * 86_400_000);
    let (id, old) = (h.account_id, stored_id(&h, "again@x").await.unwrap());
    h.db.write(move |c| mailrs_store::local_messages::put(c, id, &old, &raw("again@x"))).await.unwrap();
    let account = account(&h).await;
    to_jmap_over(&h.db, &secrets, &account, &fake, saved(), "fmu1-token".into()).await.unwrap();
    assert_eq!(stored_id(&h, "again@x").await.as_deref(), Some(earlier.as_str()));
    assert!(!fake.with(|s| s.calls.iter().any(|c| c == "Email/import")));
}

#[tokio::test]
async fn an_outbox_row_lets_go_of_a_message_the_switch_dropped() {
    let (h, fake, secrets) = imap_with_jmap_twin_and(|h| {
        h.imap.deliver("INBOX", raw("gone@x"), crate::now_millis());
    })
    .await;
    let gone = stored_id(&h, "gone@x").await.expect("stored before the switch");
    let id = h.account_id;
    let thread: String = {
        let gone = gone.clone();
        h.db.read(move |c| Ok(c.query_row("SELECT thread_id FROM messages WHERE account_id = ?1 AND id = ?2", rusqlite::params![id, gone], |r| r.get(0))?))
            .await
            .unwrap()
    };
    let queued = Queued {
        account_id: id,
        message_id: Some(gone),
        thread_id: Some(thread),
        subject: "Re: s".into(),
        recipients: "a@example.com".into(),
        send_at: crate::now_millis(),
        raw: Some(raw("reply@x")),
        ..Queued::default()
    };
    h.db.write(move |c| mailrs_store::outbox::put(c, &queued)).await.unwrap();
    let account = account(&h).await;
    let switched = to_jmap_over(&h.db, &secrets, &account, &fake, saved(), "fmu1-token".into()).await.unwrap();
    assert_eq!(switched.dropped, 1);
    let row = outbox_rows(&h).await.remove(0);
    assert_eq!((row.message_id, row.thread_id), (None, None));
    assert_eq!(row.raw, Some(raw("reply@x")), "the row still sends");
}
