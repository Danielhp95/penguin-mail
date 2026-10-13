//! The sync window's length: 2 years, 5 years or all mail, through each
//! provider's fake, and what changing it does to the mail already stored.

use std::sync::Arc;
use std::time::Duration;

use mailrs_domain::{EpochMillis, MailSet, RemoveSetting, Role};
use mailrs_store::accounts;

use super::{ImapHarness, fake_settings, harness, imap_harness_on};
use crate::fake::{FakeGraph, FakeImap, FakeMail, FakePop3, meta};
use crate::tests::microsoft::outlook;
use crate::tests::pop3::pop3_harness;
use crate::{
    ALL_MAIL, AccountServices, AccountSync, AnyMail, DEFAULT_BODY_CACHE_BYTES, ImapSettings,
    MicrosoftSettings, now_millis,
};

const DAY: EpochMillis = 24 * 60 * 60 * 1000;

fn days_ago(days: i64) -> EpochMillis {
    now_millis() - days * DAY
}

/// Loads the whole window, as the engine's loop does page by page: the
/// first sync of an account, or the rest of a window just made longer.
async fn load(sync: &AccountSync, db: &mailrs_store::Db) {
    let account_id = sync.account_id();
    let started = db
        .read(move |c| Ok(accounts::sync_cursor(c, account_id)?.state.is_some()))
        .await
        .unwrap();
    if !started {
        sync.bootstrap().await.unwrap();
    }
    sync.follow_window().await.unwrap();
    while sync.backfill_step().await.unwrap() {}
}

// ---- Gmail ------------------------------------------------------------------

impl super::Harness {
    /// A sync over the same account and Gmail, keeping `days` of mail.
    pub fn sync_for(&self, days: i64) -> AccountSync {
        self.sync_with(Duration::from_secs(60))
            .with_limits(days, DEFAULT_BODY_CACHE_BYTES)
    }
}

/// A Gmail account with mail in the inbox today, and archived mail 400
/// and 1,500 days old.
async fn gmail() -> super::Harness {
    let h = harness().await;
    h.fake.seed(meta("a", "ta", now_millis(), &["INBOX"]));
    h.fake.seed(meta("b", "tb", days_ago(400), &[]));
    h.fake.seed(meta("c", "tc", days_ago(1500), &[]));
    h
}

#[tokio::test]
async fn all_mail_brings_every_message_whatever_its_age() {
    let h = gmail().await;
    load(&h.sync_for(ALL_MAIL), &h.db).await;
    assert_eq!(h.all_threads().await, ["ta", "tb", "tc"]);
}

#[tokio::test]
async fn two_years_reach_back_two_years_and_no_further() {
    let h = gmail().await;
    load(&h.sync_for(730), &h.db).await;
    assert_eq!(h.all_threads().await, ["ta", "tb"]);
}

#[tokio::test]
async fn a_longer_window_brings_the_older_mail_and_fetches_nothing_already_here() {
    let h = gmail().await;
    load(&h.sync_for(30), &h.db).await;
    assert_eq!(h.all_threads().await, ["ta"]);
    h.fake.reset_usage();

    load(&h.sync_for(1825), &h.db).await;

    assert_eq!(h.all_threads().await, ["ta", "tb", "tc"]);
    let usage = h.fake.usage();
    let fetches = usage.calls_to("users.messages.get") + usage.calls_to("users.threads.get");
    assert_eq!(fetches, 2, "only b and c are new: {usage:?}");
    let cursor = h.cursor().await;
    assert!(cursor.backfill_done && !cursor.backfill_extends);
}

#[tokio::test]
async fn new_mail_arrives_while_older_mail_is_still_loading() {
    let h = gmail().await;
    for n in 0..6 {
        h.fake.seed(meta(&format!("old{n}"), &format!("t-old{n}"), days_ago(500 + n), &[]));
    }
    load(&h.sync_for(30), &h.db).await;
    let longer = h.sync_for(ALL_MAIL);
    longer.follow_window().await.unwrap();
    assert!(longer.backfill_step().await.unwrap(), "pages remain");

    h.fake.deliver(meta("new", "t-new", now_millis(), &["INBOX", "UNREAD"]));
    longer.incremental().await.unwrap();

    assert!(h.threads(MailSet::Role(Role::Inbox)).await.contains(&"t-new".to_string()));
    assert!(!h.cursor().await.backfill_done, "the older mail is still on its way");
    while longer.backfill_step().await.unwrap() {}
    assert!(h.all_threads().await.contains(&"t-old5".to_string()));
}

#[tokio::test]
async fn a_shorter_window_drops_the_older_mail_at_once_and_keeps_the_inbox() {
    let h = gmail().await;
    h.fake.seed(meta("d", "td", days_ago(900), &["INBOX"]));
    load(&h.sync_for(ALL_MAIL), &h.db).await;
    assert_eq!(h.all_threads().await.len(), 4);

    h.sync_for(30).follow_window().await.unwrap();

    assert_eq!(h.all_threads().await, ["ta", "td"]);
}

#[tokio::test]
async fn all_mail_is_never_pruned() {
    let h = gmail().await;
    let sync = h.sync_for(ALL_MAIL);
    load(&sync, &h.db).await;
    sync.prune(now_millis()).await.unwrap();
    assert_eq!(h.all_threads().await, ["ta", "tb", "tc"]);
}

/// An account from before the window was recorded keeps its mail as it is
/// and only notes the window it runs under now.
#[tokio::test]
async fn a_first_run_records_the_window_and_changes_nothing() {
    let h = gmail().await;
    load(&h.sync_for(30), &h.db).await;
    h.db.write(|c| {
        c.execute("UPDATE accounts SET window_days = NULL", [])?;
        Ok(())
    })
    .await
    .unwrap();
    h.fake.reset_usage();

    h.sync_for(730).follow_window().await.unwrap();

    assert!(h.cursor().await.backfill_done);
    assert_eq!(h.fake.usage().calls_to("users.messages.list"), 0);
    let recorded = h.db.read(|c| accounts::window_days(c, 1)).await.unwrap();
    assert_eq!(recorded, Some(730));
}

// ---- IMAP -------------------------------------------------------------------

/// An IMAP account keeping `days` of mail on `imap`, whose feed covers
/// every mailbox at each look.
fn imap_sync(h: &ImapHarness, days: i64) -> AccountSync {
    let settings = ImapSettings {
        window_days: days,
        ..fake_settings()
    };
    let services = AccountServices::imap(Arc::clone(&h.imap), Arc::clone(&h.smtp), settings);
    if let AnyMail::Imap(adapter) = &services.mail {
        adapter.look_at_every_mailbox();
    }
    let (sender, _) = async_channel::unbounded();
    AccountSync::new(h.account_id, services, h.db.clone(), sender)
        .with_limits(days, DEFAULT_BODY_CACHE_BYTES)
}

async fn imap_account() -> ImapHarness {
    let imap = FakeImap::new();
    let raw = |id: &str| format!("Subject: {id}\r\nMessage-ID: <{id}@example.com>\r\n\r\nHi.\r\n").into_bytes();
    imap.deliver("INBOX", raw("a"), now_millis());
    imap.deliver("Archive", raw("b"), days_ago(400));
    imap.deliver("Archive", raw("c"), days_ago(3000));
    imap_harness_on(imap, fake_settings()).await
}

#[tokio::test]
async fn imap_all_mail_lists_every_mailbox_whole() {
    let h = imap_account().await;
    load(&imap_sync(&h, ALL_MAIL), &h.db).await;
    assert_eq!(h.ids().await.len(), 3);
}

#[tokio::test]
async fn imap_two_years_leave_older_mail_on_the_server() {
    let h = imap_account().await;
    load(&imap_sync(&h, 730), &h.db).await;
    assert_eq!(h.ids().await.len(), 2);
}

#[tokio::test]
async fn imap_a_longer_window_brings_older_mail_under_new_ids_alone() {
    let h = imap_account().await;
    load(&imap_sync(&h, 30), &h.db).await;
    let before = h.ids().await;
    assert_eq!(before.len(), 1);

    load(&imap_sync(&h, ALL_MAIL), &h.db).await;

    let after = h.ids().await;
    assert_eq!(after.len(), 3);
    assert!(before.iter().all(|id| after.contains(id)), "the inbox kept its ids");
}

// ---- Microsoft ----------------------------------------------------------------

fn outlook_sync(fake: &Arc<FakeGraph>, db: &mailrs_store::Db, account_id: i64, days: i64) -> AccountSync {
    let settings = MicrosoftSettings {
        address: "me@outlook.com".into(),
        provider_name: "Outlook".into(),
        window_days: days,
    };
    let (sender, _) = async_channel::unbounded();
    AccountSync::new(account_id, AccountServices::microsoft(Arc::clone(fake), settings), db.clone(), sender)
        .with_limits(days, DEFAULT_BODY_CACHE_BYTES)
}

#[tokio::test]
async fn microsoft_all_mail_reaches_mail_of_any_age() {
    let h = outlook().await;
    let archive = h.fake.folder_id("archive");
    let old = h.fake.deliver(&archive, FakeMail { at: days_ago(3000), ..FakeMail::default() });
    let new = h.fake.deliver("inbox", FakeMail { at: now_millis(), ..FakeMail::default() });
    load(&outlook_sync(&h.fake, &h.db, h.account_id, ALL_MAIL), &h.db).await;
    assert!(h.stored(&old).await && h.stored(&new).await);
}

#[tokio::test]
async fn microsoft_a_longer_window_brings_older_mail_and_follows_its_changes() {
    let h = outlook().await;
    let archive = h.fake.folder_id("archive");
    let old = h.fake.deliver(&archive, FakeMail { at: days_ago(400), ..FakeMail::default() });
    load(&outlook_sync(&h.fake, &h.db, h.account_id, 30), &h.db).await;
    assert!(!h.stored(&old).await);

    let longer = outlook_sync(&h.fake, &h.db, h.account_id, 730);
    load(&longer, &h.db).await;
    assert!(h.stored(&old).await);

    // The feed now reads the longer window too, so a change made
    // elsewhere to the older message reaches the store.
    longer.incremental().await.unwrap();
    h.fake.mark(&old, None, Some(true));
    longer.incremental().await.unwrap();
    assert!(h.held(&old).await.keywords.iter().any(|k| k == "$flagged"));
}

/// The look that finds the window changed starts each folder's delta
/// again. Mail that arrives while it reads must reach the store all the
/// same, through this look or the next.
#[tokio::test]
async fn microsoft_mail_arriving_as_the_window_changes_is_not_lost() {
    let h = outlook().await;
    let short = outlook_sync(&h.fake, &h.db, h.account_id, 30);
    load(&short, &h.db).await;
    short.incremental().await.unwrap();

    let longer = outlook_sync(&h.fake, &h.db, h.account_id, 730);
    let inbox = h.fake.folder_id("inbox");
    let gap = FakeMail { at: now_millis(), subject: "In the gap", ..FakeMail::default() };
    h.fake.with(|s| s.deliver_after_delta = Some((inbox, gap)));
    longer.incremental().await.unwrap();
    longer.incremental().await.unwrap();
    let id = h.fake.with(|s| s.messages.iter().find(|(_, m)| m.message.subject.as_deref() == Some("In the gap")).map(|(id, _)| id.clone()));
    assert!(h.stored(&id.expect("the fake delivered it")).await);
}

// ---- POP3 -------------------------------------------------------------------

/// A POP3 account's store is the only copy of its mail, so no window,
/// however short, drops any of it.
#[tokio::test]
async fn a_pop3_account_keeps_old_mail_under_any_window() {
    let h = pop3_harness(FakePop3::default(), RemoveSetting::Never).await;
    let old = h
        .keep("u1", b"Subject: Old\r\n\r\nHi.\r\n", "archive", days_ago(3000))
        .await;
    let short = h.again().with_limits(14, DEFAULT_BODY_CACHE_BYTES);
    let account_id = h.account_id;
    h.db.write(move |c| accounts::set_window_days(c, account_id, ALL_MAIL))
        .await
        .unwrap();

    short.follow_window().await.unwrap();
    short.prune(now_millis()).await.unwrap();

    assert_eq!(h.ids_in(MailSet::Role(Role::Archive)).await, [old]);
}

// ---- What a long backfill holds ---------------------------------------------

/// The most heap the process holds above where it stood while all mail
/// loads from a Gmail of `count` archived messages, each in a thread of
/// its own, after the inbox's first page; and the most one listing call
/// to the fake holds by itself. The fake searches its whole mailbox and
/// copies every id it matches to cut one page from them, work a real Gmail
/// does on its own side, so that part grows with the mailbox and is no
/// cost of the client's.
async fn all_mail_peak(count: usize) -> (usize, usize) {
    use crate::GmailApi;
    use crate::tests::heap::ProcessMark;
    let h = harness().await;
    for n in 0..count {
        let date = days_ago(400 + (n as i64 % 3000));
        h.fake.seed(meta(&format!("m{n}"), &format!("t{n}"), date, &[]));
    }
    h.fake.with(|s| s.page_size = crate::services::LIST_PAGE_SIZE as usize);
    let sync = h.sync_for(ALL_MAIL);
    sync.bootstrap().await.unwrap();
    h.drain();
    let mark = ProcessMark::start();
    // The window takes each change event as it comes, as the app's does.
    while sync.backfill_step().await.unwrap() {
        h.drain();
    }
    let peak = mark.peak();
    let stored: i64 = h
        .db
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?))
        .await
        .unwrap();
    assert_eq!(stored, count as i64);
    let mark = ProcessMark::start();
    let page = h.fake.list_messages("", Some("100"), 100).await.unwrap();
    let listing = mark.peak();
    drop(page);
    (peak, listing)
}

/// Run alone, so other tests add nothing to the process's peak:
/// `cargo test -p mailrs-sync --lib -- --ignored a_long_backfill`
///
/// A backfill holds one page of metadata at a time, so loading four times
/// the mail should hold about the same at its peak, once the fake's own
/// listing is taken off.
#[tokio::test]
#[ignore = "a measurement of the whole process, run alone"]
async fn a_long_backfill_holds_about_one_page_whatever_the_mailbox_size() {
    let (small, small_fake) = all_mail_peak(5_000).await;
    let (large, large_fake) = all_mail_peak(20_000).await;
    eprintln!(
        "all mail: 5,000 messages peaked at {small} bytes ({small_fake} of them the fake's listing), \
         20,000 at {large} bytes ({large_fake} the fake's)"
    );
    let (small, large) = (small.saturating_sub(small_fake), large.saturating_sub(large_fake));
    eprintln!("the client's own: {small} bytes for 5,000, {large} for 20,000");
    assert!(large < small * 3 / 2 + (64 << 10), "20,000 messages held {large} bytes against {small} for 5,000");
    assert!(large < 2 << 20, "a page at a time should stay under 2 MB, held {large}");
}
