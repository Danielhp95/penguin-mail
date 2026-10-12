use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use mailrs_sync::fake::{FakeImap, raw_message};
use mailrs_sync::{Mailboxes, now_millis};

use super::*;
use crate::assistant::run::fake::{Connected, Harness};

/// Answers every mailbox asked with `answer`, and keeps each ask.
#[derive(Default)]
struct Fake {
    asks: RefCell<Vec<Ask>>,
    answer: i64,
}

impl Counter for Rc<Fake> {
    async fn count(&self, ask: Ask) -> Result<Counts, String> {
        let mailboxes = ask.mailboxes.iter().map(|m| (m.clone(), self.answer)).collect();
        self.asks.borrow_mut().push(ask);
        Ok(Counts {
            mailboxes,
            categories: HashMap::new(),
        })
    }
}

/// The real counts over a test's accounts.
impl Counter for Arc<Mailboxes<Connected>> {
    async fn count(&self, ask: Ask) -> Result<Counts, String> {
        read(self, ask).await.map_err(|e| e.to_string())
    }
}

/// A window whose sidebar holds `mailboxes`, keeping the count of each
/// one it was shown, then of account 1's inbox.
struct Window {
    mailboxes: Vec<Mailbox>,
    shown: Rc<RefCell<Vec<Vec<i64>>>>,
}

impl Shows for Window {
    fn wanted(&self) -> Option<Wanted> {
        Some(Wanted {
            mailboxes: self.mailboxes.clone(),
            shown: self.mailboxes[0].clone(),
            view: View::default(),
        })
    }

    fn show(&self, counted: &Counted) {
        let mut seen: Vec<i64> = self.mailboxes.iter().map(|m| counted.of(m)).collect();
        seen.push(counted.inbox(1));
        self.shown.borrow_mut().push(seen);
    }
}

/// Starts the counts on the test's thread.
fn started<C: Counter + 'static>(unread: &Rc<Unread<C>>) {
    tokio::task::spawn_local(unread.start(tokio::time::sleep));
}

/// Lets every task on the thread run, timers included.
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(QUIET * 2).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

fn label(account_id: AccountId) -> Mailbox {
    Mailbox::Label {
        account_id,
        label_id: "Label_kites".into(),
        name: "Kites".into(),
    }
}

#[tokio::test(start_paused = true)]
async fn a_burst_of_changes_is_one_count() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let fake = Rc::new(Fake::default());
            let unread = Unread::new(Rc::clone(&fake));
            started(&unread);
            unread.set_accounts(vec![1, 2]);
            for _ in 0..50 {
                unread.changed();
            }
            unread.answered();
            settle().await;
            assert_eq!(fake.asks.borrow().len(), 1, "the burst shared one count");
            assert!(!fake.asks.borrow()[0].refresh, "no window, so no server count");

            // Sync reports batch after batch. With no window open, those
            // that come while the last count is still recent share one.
            for _ in 0..40 {
                unread.changed();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            settle().await;
            assert_eq!(fake.asks.borrow().len(), 3, "{:?}", fake.asks.borrow().len());
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_change_during_a_count_gets_one_more() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let fake = Rc::new(Fake::default());
            let unread = Unread::new(Rc::clone(&fake));
            let heard = Rc::new(Cell::new(0));
            let (weak, seen) = (Rc::downgrade(&unread), Rc::clone(&heard));
            // Each listener call asks again, as a change landing while the
            // count ran would, until three counts went by.
            unread.listen(Box::new(move |_| {
                seen.set(seen.get() + 1);
                if seen.get() < 3
                    && let Some(unread) = weak.upgrade()
                {
                    unread.changed();
                    unread.changed();
                }
            }));
            started(&unread);
            unread.changed();
            settle().await;
            settle().await;
            assert_eq!(fake.asks.borrow().len(), 3);
            assert_eq!(heard.get(), 3);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_server_answer_with_no_window_reads_only_what_is_kept() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let fake = Rc::new(Fake::default());
            let unread = Unread::new(Rc::clone(&fake));
            started(&unread);
            unread.answered();
            settle().await;
            assert!(!fake.asks.borrow()[0].refresh, "the tray starts no polling");
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_mailbox_the_window_shows_gets_its_count() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let fake = Rc::new(Fake {
                answer: 4,
                ..Fake::default()
            });
            let unread = Unread::new(Rc::clone(&fake));
            let shown = Rc::new(RefCell::new(Vec::new()));
            unread.show_in(Box::new(Window {
                mailboxes: vec![label(1), ALL_INBOXES],
                shown: Rc::clone(&shown),
            }));
            started(&unread);
            unread.set_accounts(vec![1]);
            settle().await;
            let asks = fake.asks.borrow();
            assert_eq!(asks.len(), 1);
            assert_eq!(asks[0].shown, label(1), "the categories follow the window");
            let asked = &asks[0].mailboxes;
            assert_eq!(
                asked.iter().filter(|m| **m == ALL_INBOXES).count(),
                1,
                "a mailbox two subscribers show is counted once: {asked:?}"
            );
            // The label, All Inboxes, and the tray's inbox of account 1.
            assert_eq!(*shown.borrow(), [vec![4, 4, 4]]);
        })
        .await;
}

/// An IMAP account whose server holds unread mail the copy here has not
/// fetched yet: the server counts it, the store does not hold it.
#[tokio::test(start_paused = true)]
async fn the_tray_says_what_the_sidebar_says_for_a_folder_account() {
    let day = 24 * 60 * 60 * 1000;
    let now = now_millis();
    let imap = Arc::new(FakeImap::new());
    imap.deliver_flagged("INBOX", &raw_message("first", "Today", now - day, None), &[], now - day);
    let h = Harness::on_imap(Arc::clone(&imap)).await;
    imap.deliver_flagged("INBOX", &raw_message("second", "Now", now, None), &[], now);
    let lists = Arc::new(h.lists());
    let account_id = h.account_id;
    let local = h
        .db
        .read(move |c| {
            use mailrs_store::threads::{ThreadFilter, unread_threads};
            let inbox = mailrs_domain::MailSet::Role(mailrs_domain::Role::Inbox);
            unread_threads(c, &ThreadFilter::account(account_id, inbox))
        })
        .await
        .unwrap();
    assert_eq!(local, 1, "the tray used to count only this");
    tokio::task::LocalSet::new()
        .run_until(async move {
            let unread = Unread::new(Arc::clone(&lists));
            // The app wakes the counts on each server answer the same way.
            let mut answers = lists.counts_changed();
            let weak = Rc::downgrade(&unread);
            tokio::task::spawn_local(async move {
                while answers.changed().await.is_ok() {
                    let Some(unread) = weak.upgrade() else { break };
                    unread.answered();
                }
            });
            let sidebar = Rc::new(RefCell::new(None));
            let tray = Rc::new(RefCell::new(None));
            let (into_sidebar, into_tray) = (Rc::clone(&sidebar), Rc::clone(&tray));
            unread.listen(Box::new(move |counted| {
                *into_sidebar.borrow_mut() = Some(counted.all_inboxes());
            }));
            unread.listen(Box::new(move |counted| {
                *into_tray.borrow_mut() = Some(counted.inbox(account_id));
            }));
            // The sidebar's own count is the one that asks the server.
            unread.show_in(Box::new(Window {
                mailboxes: vec![ALL_INBOXES],
                shown: Rc::default(),
            }));
            started(&unread);
            unread.set_accounts(vec![account_id]);
            for _ in 0..5 {
                settle().await;
            }
            assert_eq!(*sidebar.borrow(), Some(2), "the server's count");
            assert_eq!(*tray.borrow(), *sidebar.borrow());
        })
        .await;
}

fn threads_changed() -> mailrs_domain::ChangeEvent {
    mailrs_domain::ChangeEvent::ThreadsChanged {
        account_id: 1,
        thread_ids: vec!["t".into()],
    }
}

#[tokio::test(start_paused = true)]
async fn with_no_window_a_burst_of_events_makes_one_count_that_asks_no_server() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let fake = Rc::new(Fake::default());
            let unread = Unread::new(Rc::clone(&fake));
            started(&unread);
            for _ in 0..30 {
                unread.heard(&threads_changed());
            }
            settle().await;
            let asks = fake.asks.borrow();
            assert_eq!(asks.len(), 1);
            assert!(!asks[0].refresh, "the tray starts no server count");
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn with_no_window_only_thread_and_account_events_ask_for_a_count() {
    use mailrs_domain::{AccountState, ChangeEvent};
    tokio::task::LocalSet::new()
        .run_until(async {
            let fake = Rc::new(Fake::default());
            let unread = Unread::new(Rc::clone(&fake));
            started(&unread);
            unread.heard(&ChangeEvent::NewMail { account_id: 1, message_ids: vec!["m".into()] });
            unread.heard(&ChangeEvent::LabelsChanged { account_id: 1 });
            settle().await;
            assert!(fake.asks.borrow().is_empty());
            unread.heard(&ChangeEvent::AccountStateChanged { account_id: 1, state: AccountState::Ok });
            settle().await;
            assert_eq!(fake.asks.borrow().len(), 1);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn with_a_window_every_event_asks_the_servers_as_before() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let fake = Rc::new(Fake::default());
            let unread = Unread::new(Rc::clone(&fake));
            unread.show_in(Box::new(Window {
                mailboxes: vec![ALL_INBOXES],
                shown: Rc::default(),
            }));
            started(&unread);
            settle().await;
            unread.heard(&mailrs_domain::ChangeEvent::LabelsChanged { account_id: 1 });
            settle().await;
            let asks = fake.asks.borrow();
            assert_eq!(asks.len(), 2);
            assert!(asks[1].refresh);
        })
        .await;
}
